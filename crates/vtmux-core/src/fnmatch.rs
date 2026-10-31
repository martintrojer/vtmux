//! glibc fnmatch with the flags tmux uses: no pathname/period restrictions,
//! backslash escapes on, optionally `FNM_CASEFOLD`. Match the entire string.
//! glibc tries wide characters first, then bytes even when the wide match merely
//! fails (posix/fnmatch.c:279); `??` therefore matches a two-byte `é`.
use crate::regex::{Class, lower};

#[derive(Clone)]
enum Token {
    Literal(char),
    Any,
    Star,
    Bracket { negated: bool, items: Vec<Item> },
    Fail,
}
#[derive(Clone)]
enum Item {
    Char(char),
    Range(char, char),
    Class(Class),
}

/// Match an entire string, with glibc's UTF-8 then byte fallback semantics.
#[must_use]
pub fn matches(pattern: &str, text: &str, ignore_case: bool) -> bool {
    use crate::rawstr::{c_str, is_utf8, to_bytes};
    let (pattern, text) = (c_str(pattern), c_str(text));
    // A string that is not UTF-8 has no wide form (`mbsrtowcs` fails): only
    // the byte pass runs.
    (is_utf8(pattern)
        && is_utf8(text)
        && run(
            &pattern.chars().collect::<Vec<_>>(),
            &text.chars().collect::<Vec<_>>(),
            ignore_case,
            true,
        ))
        || run(
            &to_bytes(pattern)
                .iter()
                .copied()
                .map(char::from)
                .collect::<Vec<_>>(),
            &to_bytes(text)
                .iter()
                .copied()
                .map(char::from)
                .collect::<Vec<_>>(),
            ignore_case,
            false,
        )
}

fn fold(c: char, icase: bool, wide: bool) -> char {
    if !icase {
        c
    } else if wide {
        lower(c)
    } else {
        c.to_ascii_lowercase()
    }
}

fn run(pattern: &[char], text: &[char], icase: bool, wide: bool) -> bool {
    // Rolling dynamic programming bounds stack and memory independently of pattern.
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    for token in tokenize(pattern) {
        let mut next = vec![false; text.len() + 1];
        if matches!(token, Token::Star) {
            next[0] = previous[0];
            for i in 1..=text.len() {
                next[i] = previous[i] || next[i - 1];
            }
        } else {
            for (i, &c) in text.iter().enumerate() {
                next[i + 1] = previous[i]
                    && match &token {
                        Token::Literal(a) => fold(*a, icase, wide) == fold(c, icase, wide),
                        Token::Any => true,
                        Token::Bracket { negated, items } => {
                            let found = items.iter().any(|item| match item {
                                Item::Char(a) => fold(*a, icase, wide) == fold(c, icase, wide),
                                Item::Class(class) => {
                                    if wide || c.is_ascii() {
                                        class.contains(c)
                                    } else {
                                        false
                                    }
                                }
                                Item::Range(a, b) => {
                                    let (a, b, c) = (
                                        fold(*a, icase, wide),
                                        fold(*b, icase, wide),
                                        fold(c, icase, wide),
                                    );
                                    // Builtin C LC_COLLATE has wide sequence values only
                                    // through Latin-1. An unknown high endpoint retains
                                    // the ASCII interval, or the exact known low endpoint.
                                    if wide && b > '\u{ff}' {
                                        c <= '\u{ff}' && a <= c && (c <= '\u{7f}' || c == a)
                                    } else {
                                        (!wide || c <= '\u{ff}') && a <= c && c <= b
                                    }
                                }
                            });
                            found != *negated
                        }
                        Token::Fail | Token::Star => false,
                    };
            }
        }
        previous = next;
    }
    previous[text.len()]
}

fn tokenize(pattern: &[char]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(&c) = pattern.get(at) {
        at += 1;
        tokens.push(match c {
            '*' => Token::Star,
            '?' => Token::Any,
            '\\' => match pattern.get(at) {
                Some(&c) => {
                    at += 1;
                    Token::Literal(c)
                }
                None => Token::Fail,
            },
            '[' => match bracket(pattern, at) {
                Some((token, end)) => {
                    at = end;
                    token
                }
                None => Token::Literal('['),
            },
            c => Token::Literal(c),
        });
    }
    tokens
}

fn bracket(pattern: &[char], mut at: usize) -> Option<(Token, usize)> {
    let negated = pattern.get(at).is_some_and(|c| matches!(c, '!' | '^'));
    if negated {
        at += 1;
    }
    let mut items = Vec::new();
    while let Some(&c) = pattern.get(at) {
        if c == ']' && !items.is_empty() {
            return Some((Token::Bracket { negated, items }, at + 1));
        }
        let (item, end) = bracket_item(pattern, at)?;
        at = end;
        if let Item::Char(a) = item
            && pattern.get(at) == Some(&'-')
            && pattern.get(at + 1) != Some(&']')
        {
            let (b, end) = bracket_item(pattern, at + 1)?;
            if let Item::Char(b) = b {
                items.push(Item::Range(a, b));
            } else {
                items.push(Item::Char(a));
                items.push(Item::Char('-'));
                items.push(b);
            }
            at = end;
        } else {
            items.push(item);
        }
    }
    None
}

fn bracket_item(pattern: &[char], at: usize) -> Option<(Item, usize)> {
    let c = *pattern.get(at)?;
    if c == '\\' {
        return Some((Item::Char(*pattern.get(at + 1)?), at + 2));
    }
    if c == '['
        && pattern
            .get(at + 1)
            .is_some_and(|c| matches!(c, ':' | '.' | '='))
    {
        let delimiter = pattern[at + 1];
        let end = (at + 2..pattern.len().saturating_sub(1))
            .find(|&i| pattern[i] == delimiter && pattern[i + 1] == ']')?;
        let name: String = pattern[at + 2..end].iter().collect();
        let item = if delimiter == ':' {
            Class::parse(&name).map(Item::Class)?
        } else {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Item::Char(c)
        };
        return Some((item, end + 2));
    }
    Some((Item::Char(c), at + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("regex/fnmatch_golden.in");
    #[test]
    fn fnmatch_glibc_goldens() {
        for (pattern, text, icase, expected) in rows() {
            assert_eq!(
                matches(pattern, text, icase),
                expected,
                "{pattern:?}, {text:?}, i={icase}"
            );
        }
    }
}
