//! Pure POSIX ERE matching for tmux's `regcomp(REG_EXTENDED)` / `regexec` edge.
//! Searches choose the earliest start and longest whole match, not Perl alternation
//! order. Captures keep greedy traversal order when whole-match lengths tie.
//! `REG_NEWLINE` is off: dot and negated brackets include newlines.
//! Character properties are glibc's reference UTF-8 locale, not the host locale.
//! Patterns without backreferences run on a linear-time NFA (`nfa.rs`) with no
//! work cap. Backreference patterns backtrack, as glibc does, with a work budget
//! per start position.
//! Resource limits return `Error::Limit`: 64 nested groups or chained quantifiers,
//! 128 total groups/quantifiers, about a million compiled NFA nodes after `{m,n}`
//! expansion, and, for backreference patterns only, one million capture-weighted
//! steps per start position and sixteen million per search
//! (compat/DIVERGENCES.md D-001).
mod ctype;
mod ctype_tables;
mod nfa;
mod parse;
#[cfg(test)]
mod tests;

pub(crate) use ctype::{Class, equal, lower, upper};
use std::ops::Range;

/// Capture-weighted backtracking steps per start position (backreferences only).
const BACKREF_START_BUDGET: usize = 1_000_000;
/// Steps for one whole backreference search, about a second of work.
const BACKREF_SEARCH_BUDGET: usize = 16_000_000;

/// Malformed ERE, excessive nesting, or an exhausted matching work budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Syntax,
    Limit,
}

#[derive(Debug, Clone)]
enum Expr {
    Empty,
    Literal(char),
    Any,
    Bracket(Bracket),
    Anchor(Anchor),
    Backref(usize),
    Seq(Vec<Self>),
    Alt(Vec<Self>),
    Group(usize, Box<Self>),
    Repeat {
        expr: Box<Self>,
        min: usize,
        max: Option<usize>,
    },
}

#[derive(Debug, Clone, Copy)]
enum Anchor {
    Start,
    End,
    Boundary,
    NotBoundary,
    WordStart,
    WordEnd,
}

#[derive(Debug, Clone)]
pub(crate) struct Bracket {
    pub(crate) negated: bool,
    pub(crate) items: Vec<Item>,
}
#[derive(Debug, Clone)]
pub(crate) enum Item {
    Char(char),
    Range(char, char),
    Class(Class),
}
impl Bracket {
    fn matches(&self, c: char, ignore_case: bool) -> bool {
        let yes = self.items.iter().any(|item| match *item {
            Item::Char(a) => equal(a, c, ignore_case),
            Item::Range(a, b) => {
                let c = if ignore_case { upper(c) } else { c };
                a <= c && c <= b
            }
            Item::Class(class) => {
                class.contains(c)
                    || (ignore_case
                        && matches!(class, Class::Upper | Class::Lower)
                        && (Class::Upper.contains(c) || Class::Lower.contains(c)))
            }
        });
        yes != self.negated
    }
}

/// A compiled ERE. Limits are explicit errors, never reported as a successful miss.
#[derive(Debug, Clone)]
pub struct Regex {
    expr: Expr,
    groups: usize,
    ignore_case: bool,
    /// `None` for backreference patterns, which use the backtracker.
    nfa: Option<nfa::Nfa>,
}
/// Byte ranges into the original input; capture 0 is the whole match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub captures: Vec<Option<Range<usize>>>,
}
#[derive(Clone, PartialEq, Eq, Hash)]
struct State {
    pos: usize,
    captures: Vec<Option<Range<usize>>>,
}

impl Regex {
    pub fn new(pattern: &str, ignore_case: bool) -> Result<Self, Error> {
        let (expr, groups) =
            parse::parse(pattern.split('\0').next().unwrap_or_default(), ignore_case)?;
        let nfa = if has_backref(&expr) {
            None
        } else {
            Some(nfa::Nfa::compile(&expr, groups, ignore_case)?)
        };
        Ok(Self {
            expr,
            groups,
            ignore_case,
            nfa,
        })
    }

    /// `regexec` with `REG_NOSUB`: whether any match exists.
    pub fn is_match(&self, text: &str) -> Result<bool, Error> {
        let text = text.split('\0').next().unwrap_or_default();
        match &self.nfa {
            Some(nfa) => Ok(nfa.span(text).is_some()),
            None => self.find(text).map(|m| m.is_some()),
        }
    }

    pub fn find(&self, text: &str) -> Result<Option<Match>, Error> {
        let text = text.split('\0').next().unwrap_or_default();
        // Flat literal patterns need no state exploration, even for multi-megabyte text.
        if !self.ignore_case
            && let Expr::Seq(parts) = &self.expr
        {
            let literal: Option<String> = parts
                .iter()
                .map(|p| match p {
                    Expr::Literal(c) => Some(*c),
                    _ => None,
                })
                .collect();
            if let Some(literal) = literal {
                return Ok(text.find(&literal).map(|start| Match {
                    captures: vec![Some(start..start + literal.len())],
                }));
            }
        }
        if let Some(nfa) = &self.nfa {
            return Ok(nfa.span(text).and_then(|(start, end)| {
                nfa.captures(text, start, end)
                    .map(|captures| Match { captures })
            }));
        }
        // Backreference patterns backtrack, as glibc does: each start position
        // gets its own budget, and the whole search a ceiling on wall time.
        let mut total = BACKREF_SEARCH_BUDGET;
        for start in text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
        {
            let mut budget = BACKREF_START_BUDGET.min(total);
            let state = State {
                pos: start,
                captures: vec![None; self.groups + 1],
            };
            let before = budget;
            let states = self.run(&self.expr, state, text, &mut budget)?;
            total -= before - budget;
            // max_by_key would choose the last tied path; preserve the first greedy path.
            let mut best: Option<State> = None;
            for s in states {
                if best.as_ref().is_none_or(|b| {
                    s.pos > b.pos
                        || (s.pos == b.pos
                            && s.captures
                                .iter()
                                .zip(&b.captures)
                                .find_map(|(a, b)| match (a, b) {
                                    (Some(a), Some(b)) if a.is_empty() && !b.is_empty() => {
                                        Some(false)
                                    }
                                    (Some(a), Some(b)) if !a.is_empty() && b.is_empty() => {
                                        Some(true)
                                    }
                                    (Some(a), None) if a.is_empty() => Some(true),
                                    (None, Some(b)) if b.is_empty() => Some(false),
                                    (a, b) if a != b => Some(false),
                                    _ => None,
                                })
                                .unwrap_or(false))
                }) {
                    best = Some(s);
                }
            }
            if let Some(mut s) = best {
                s.captures[0] = Some(start..s.pos);
                return Ok(Some(Match {
                    captures: s.captures,
                }));
            }
        }
        Ok(None)
    }

    #[allow(clippy::too_many_lines)]
    fn run(
        &self,
        expr: &Expr,
        mut s: State,
        text: &str,
        budget: &mut usize,
    ) -> Result<Vec<State>, Error> {
        *budget = budget.checked_sub(self.groups + 1).ok_or(Error::Limit)?;
        let next = text[s.pos..].chars().next();
        match expr {
            Expr::Empty => Ok(vec![s]),
            Expr::Literal(c) => Ok(if next.is_some_and(|n| equal(*c, n, self.ignore_case)) {
                s.pos += next.unwrap().len_utf8();
                vec![s]
            } else {
                vec![]
            }),
            Expr::Any | Expr::Bracket(_) => {
                if let Some(c) = next
                    && (matches!(expr, Expr::Any)
                        || matches!(expr, Expr::Bracket(b) if b.matches(c, self.ignore_case)))
                {
                    s.pos += c.len_utf8();
                    Ok(vec![s])
                } else {
                    Ok(vec![])
                }
            }
            Expr::Anchor(a) => {
                let word = |c: char| c == '_' || Class::Alnum.contains(c);
                let before = text[..s.pos].chars().next_back().is_some_and(word);
                let after = next.is_some_and(word);
                let yes = match a {
                    Anchor::Start => s.pos == 0,
                    Anchor::End => s.pos == text.len(),
                    Anchor::Boundary => before != after,
                    Anchor::NotBoundary => before == after,
                    Anchor::WordStart => !before && after,
                    Anchor::WordEnd => before && !after,
                };
                Ok(if yes { vec![s] } else { vec![] })
            }
            Expr::Backref(n) => {
                let Some(r) = &s.captures[*n] else {
                    return Ok(vec![]);
                };
                // The comparison is work too: charge its length.
                *budget = budget.checked_sub(r.len()).ok_or(Error::Limit)?;
                let mut at = s.pos;
                for c in text[r.clone()].chars() {
                    let Some(n) = text[at..].chars().next() else {
                        return Ok(vec![]);
                    };
                    if !equal(c, n, self.ignore_case) {
                        return Ok(vec![]);
                    }
                    at += n.len_utf8();
                }
                s.pos = at;
                Ok(vec![s])
            }
            Expr::Group(n, expr) => {
                let start = s.pos;
                let mut out = self.run(expr, s, text, budget)?;
                for state in &mut out {
                    state.captures[*n] = Some(start..state.pos);
                }
                Ok(out)
            }
            Expr::Seq(parts) => {
                let mut states = vec![s];
                for part in parts {
                    let mut out = Vec::new();
                    for state in states {
                        out.extend(self.run(part, state, text, budget)?);
                    }
                    states = unique(out);
                    if states.is_empty() {
                        break;
                    }
                }
                Ok(states)
            }
            Expr::Alt(parts) => {
                let mut out = Vec::new();
                for part in parts {
                    out.extend(self.run(part, s.clone(), text, budget)?);
                }
                Ok(unique(out))
            }
            Expr::Repeat { expr, min, max } => {
                // Ordered DFS, not breadth-first levels: a greedy inner repetition
                // must get its whole span before a later iteration gets a turn.
                // Use an explicit stack so text length cannot exhaust the call stack.
                let mut stack = vec![(s, 0usize, false)];
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                while let Some((state, count, emit)) = stack.pop() {
                    *budget = budget.checked_sub(self.groups + 1).ok_or(Error::Limit)?;
                    if emit {
                        if count >= *min {
                            out.push(state);
                        }
                        continue;
                    }
                    if !seen.insert((state.clone(), count)) {
                        continue;
                    }
                    stack.push((state.clone(), count, true));
                    if max.is_some_and(|m| count >= m) {
                        continue;
                    }
                    let children = self.run(expr, state.clone(), text, budget)?;
                    for next in children.into_iter().rev() {
                        if next.pos == state.pos {
                            if next != state || count < *min {
                                stack.push((next, count.max(*min), false));
                            }
                        } else {
                            stack.push((next, count + 1, false));
                        }
                    }
                }
                Ok(unique(out))
            }
        }
    }
}

fn unique(states: Vec<State>) -> Vec<State> {
    let mut seen = std::collections::HashSet::new();
    states
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

fn has_backref(expr: &Expr) -> bool {
    match expr {
        Expr::Backref(_) => true,
        Expr::Seq(parts) | Expr::Alt(parts) => parts.iter().any(has_backref),
        Expr::Group(_, e) | Expr::Repeat { expr: e, .. } => has_backref(e),
        _ => false,
    }
}
