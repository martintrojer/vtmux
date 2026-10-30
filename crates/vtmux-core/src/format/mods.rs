//! Building the typed value of one `#{modifiers:body}` (parse side). This follows the
//! precedence of `format_replace` (format.c:6019-6704): the first rule in the chain that
//! applies decides what the body means. Flags that tmux reads out of expanded modifier
//! arguments (`q/s`, `c/f`, `N/w` ...) are kept as argument templates and decided when
//! the template is expanded, because tmux expands every argument first.

use super::loops::{LoopKind, SortMod};
use super::parse::{RawMod, skip, template, unescape};
use super::{CmpOp, CondArm, Format, Post, Reexpand, Value, VarMods};

/// The modifiers this module (and the evaluator) implements; any other modifier makes
/// the whole `#{...}` a [`super::Node::Deferred`].
pub(super) const SUPPORTED: &[&str] = &[
    "E", "T", "||", "&&", "==", "!=", "<", ">", "<=", ">=", "!", "!!", "e", "l", "a", "b", "d",
    "n", "w", "=", "p", "c", "q", "R", "V", "N", "C", "t", "m", "s", "S", "W", "P", "L", "O",
];

/// An argument as a template (`format_unescape` then `format_expand1`, at expansion).
fn arg(s: &str, depth: usize, deferred: &mut Vec<String>) -> Format {
    template(&unescape(s), depth, deferred)
}

fn first_arg(m: &RawMod, depth: usize, deferred: &mut Vec<String>) -> Option<Format> {
    m.args.first().map(|a| arg(a, depth, deferred))
}

/// `format_choose` without expansion: split at the first top-level comma.
fn choose(body: &str) -> Option<(&str, &str)> {
    let c = skip(body, 0, b",")?;
    Some((&body[..c], &body[c + 1..]))
}

/// Build the value and the post-processing of a modifier list over `body`. `inner` is
/// the depth at which sub-templates are expanded. `None` is tmux's "failed", which ends
/// the enclosing expansion.
pub(super) fn build(
    list: &[RawMod],
    body: &str,
    inner: usize,
    deferred: &mut Vec<String>,
) -> Option<(Value, Post)> {
    let has = |n: &str| list.iter().any(|m| m.name == n);
    let post = post(list, inner, deferred);
    let vm = var_mods(list, inner, deferred);
    let value = if has("l") {
        // format.c:6334: the raw body, unescaped.
        Value::Literal(unescape(body))
    } else if has("a") {
        Value::Char(template(body, inner, deferred))
    } else if has("c") {
        Value::Colour {
            flags: list
                .iter()
                .filter(|m| m.name == "c")
                .filter_map(|m| first_arg(m, inner, deferred))
                .collect(),
            body: template(body, inner, deferred),
        }
    } else {
        chain(list, &vm, body, inner, deferred, true)?
    };
    Some((value, post))
}

/// The rules from loops onward. `with_names` is false when rebuilding the fallback of
/// `N` whose flags matched neither window nor session.
#[allow(clippy::too_many_lines, clippy::similar_names)]
fn chain(
    list: &[RawMod],
    vm: &VarMods,
    body: &str,
    inner: usize,
    deferred: &mut Vec<String>,
    with_names: bool,
) -> Option<Value> {
    let has = |n: &str| list.iter().any(|m| m.name == n);
    let last = |names: &[&str]| list.iter().rfind(|m| names.contains(&m.name.as_str()));
    macro_rules! sub {
        ($s:expr) => {
            template($s, inner, deferred)
        };
    }

    // format.c:6383-6407: the loops, sessions first.
    let kind = ["S", "W", "P", "L"]
        .iter()
        .find(|n| has(n))
        .and_then(|n| LoopKind::from_modifier(n));
    if let Some(kind) = kind {
        // The last of S W P L sets the sort, whichever loop runs.
        let s = last(&["S", "W", "P", "L"])?;
        let sort = SortMod {
            kind: LoopKind::from_modifier(&s.name)?,
            arg: first_arg(s, inner, deferred),
        };
        // format_choose without expansion; clients have no active form.
        let split = if kind == LoopKind::Clients {
            None
        } else {
            choose(body)
        };
        let (all, active) = match split {
            Some((a, b)) => (sub!(a), Some(sub!(b))),
            None => (sub!(body), None),
        };
        return Some(Value::Loop {
            kind,
            sort,
            all,
            active,
        });
    }
    if has("O") {
        // `loop_flags` is shared with V: the last O or V with exactly one argument.
        let flags = list
            .iter()
            .rfind(|m| (m.name == "O" || m.name == "V") && m.args.len() == 1)
            .map(|m| arg(&m.args[0], inner, deferred));
        return Some(Value::Options {
            flags,
            body: sub!(body),
        });
    }
    if let Some(v) = list.iter().rfind(|m| m.name == "V") {
        // `argc == 1` only; with more arguments the flags stay empty.
        let flags = (v.args.len() == 1).then(|| arg(&v.args[0], inner, deferred));
        return Some(Value::Environ {
            flags,
            body: template(body, inner, deferred),
        });
    }
    if with_names && has("N") {
        let flags = list
            .iter()
            .filter(|m| m.name == "N")
            .map(|m| first_arg(m, inner, deferred))
            .collect();
        let fallback = chain(list, vm, body, inner, &mut Vec::new(), false).map(Box::new);
        return Some(Value::Name {
            flags,
            body: template(body, inner, deferred),
            fallback,
        });
    }
    if let Some(c) = last(&["C"]) {
        return Some(Value::Search {
            flags: first_arg(c, inner, deferred),
            body: template(body, inner, deferred),
        });
    }
    if has("R") {
        let (left, right) = choose(body)?;
        return Some(Value::Repeat {
            left: sub!(left),
            right: sub!(right),
        });
    }
    if has("!") {
        return Some(Value::Not {
            negate: true,
            body: sub!(body),
        });
    }
    if has("!!") {
        return Some(Value::Not {
            negate: false,
            body: sub!(body),
        });
    }
    if let Some(op) = last(&["||", "&&"]) {
        let mut parts = Vec::new();
        let mut start = 0;
        loop {
            // format_bool_op_n (format.c:5176): operands split at top-level commas.
            let Some(c) = skip(body, start, b",") else {
                parts.push(sub!(&body[start..]));
                break;
            };
            parts.push(sub!(&body[start..c]));
            start = c + 1;
        }
        return Some(if op.name == "||" {
            Value::Or(parts)
        } else {
            Value::And(parts)
        });
    }
    if let Some(op) = last(&["==", "!=", "<", ">", "<=", ">=", "m"]) {
        if op.name == "m" {
            return super::matching::build_match(op, body, inner, deferred);
        }
        // format_choose: no comma is a syntax error.
        let (left, right) = choose(body)?;
        let op = match op.name.as_str() {
            "==" => CmpOp::Eq,
            "!=" => CmpOp::Ne,
            "<" => CmpOp::Lt,
            ">" => CmpOp::Gt,
            "<=" => CmpOp::Le,
            _ => CmpOp::Ge,
        };
        return Some(Value::Cmp {
            op,
            left: sub!(left),
            right: sub!(right),
        });
    }
    if let Some(conds) = body.strip_prefix('?') {
        // format.c:6502-6580.
        let mut arms = Vec::new();
        let mut default = None;
        let mut rest = conds;
        loop {
            let Some(c) = skip(rest, 0, b",") else {
                default = Some(sub!(rest));
                break;
            };
            let raw = &rest[..c];
            rest = &rest[c + 1..];
            let end = skip(rest, 0, b",");
            arms.push(CondArm {
                raw: raw.to_owned(),
                test: sub!(raw),
                then: sub!(&rest[..end.unwrap_or(rest.len())]),
            });
            let Some(end) = end else { break };
            rest = &rest[end + 1..];
        }
        return Some(Value::Cond {
            arms,
            default,
            vm: vm.clone(),
        });
    }
    // `e` with 1 to 3 arguments (format.c:6117); otherwise it is ignored.
    if let Some(e) = list
        .iter()
        .rfind(|m| m.name == "e" && (1..=3).contains(&m.args.len()))
    {
        let args = e.args.iter().map(|a| arg(a, inner, deferred)).collect();
        let operands = choose(body).map(|(l, r)| (sub!(l), sub!(r)));
        return Some(Value::Expr { args, operands });
    }
    Some(if body.contains("#{") {
        Value::Inner(sub!(body))
    } else {
        Value::Var {
            name: body.to_owned(),
            vm: vm.clone(),
        }
    })
}

/// The modifiers `format_find` applies to a found variable: `t`, `b`, `d` and the `q` family.
fn var_mods(list: &[RawMod], inner: usize, deferred: &mut Vec<String>) -> VarMods {
    VarMods {
        time: list
            .iter()
            .filter(|m| m.name == "t")
            .map(|m| super::time::TimeMod {
                flags: first_arg(m, inner, deferred),
                fmt: m.args.get(1).map(|a| arg(a, inner, deferred)),
            })
            .collect(),
        basename: list.iter().any(|m| m.name == "b"),
        dirname: list.iter().any(|m| m.name == "d"),
        quote: list
            .iter()
            .filter(|m| m.name == "q")
            .map(|m| first_arg(m, inner, deferred))
            .collect(),
    }
}

/// What happens to the value after it is found (the `done:` label, format.c:6602-6682).
fn post(list: &[RawMod], inner: usize, deferred: &mut Vec<String>) -> Post {
    let mut p = Post::default();
    for m in list {
        match m.name.as_str() {
            // `E` wins over `T` (format.c:6603-6607).
            "E" => p.reexpand = Reexpand::E,
            "T" if p.reexpand == Reexpand::No => p.reexpand = Reexpand::T,
            "=" if !m.args.is_empty() => {
                p.limit = first_arg(m, inner, deferred);
                if let Some(marker) = m.args.get(1) {
                    p.marker = Some(arg(marker, inner, deferred));
                }
            }
            "p" if !m.args.is_empty() => p.pad = first_arg(m, inner, deferred),
            "n" => p.length = true,
            "w" => p.width = true,
            _ => {}
        }
    }
    super::matching::add_substitutions(&mut p, list, inner, deferred);
    p
}
