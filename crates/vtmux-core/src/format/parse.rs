//! Template parser. Ports `format_expand1`'s scanning (format.c:6708-6893),
//! `format_skip1` (4829), `format_build_modifiers` (4936) and the dispatch in
//! `format_replace` (6019-6600) into a typed AST.

use super::{CondArm, Format, LOOP_LIMIT, Node, Value};

/// `format_upper` / `format_lower` (format.c:222-281): the `#S` style aliases.
fn alias(ch: u8) -> Option<&'static str> {
    Some(match ch {
        b'D' => "pane_id",
        b'F' => "window_flags",
        b'H' => "host",
        b'I' => "window_index",
        b'P' => "pane_index",
        b'S' => "session_name",
        b'T' => "pane_title",
        b'W' => "window_name",
        b'h' => "host_short",
        _ => return None,
    })
}

#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    text: String,
}

impl Builder {
    fn node(&mut self, n: Node) {
        self.flush();
        self.nodes.push(n);
    }

    fn flush(&mut self) {
        if !self.text.is_empty() {
            self.nodes.push(Node::Text(std::mem::take(&mut self.text)));
        }
    }

    fn finish(mut self) -> Format {
        self.flush();
        Format { nodes: self.nodes }
    }
}

/// `format_skip1`: index of the first byte of `end` at or after `start` that is
/// outside any `#{...}` and not escaped (`#,` `##` `#{` `#}` `#:`), or `None`.
pub(super) fn skip(s: &str, start: usize, end: &[u8]) -> Option<usize> {
    let b = s.as_bytes();
    let mut brackets = 0i32;
    let mut i = start;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == b'#' && next == Some(b'{') {
            brackets += 1;
        }
        if c == b'#' && next.is_some_and(|n| b",#{}:".contains(&n)) {
            i += 2;
            continue;
        }
        if c == b'}' {
            brackets -= 1;
        }
        if end.contains(&c) && brackets == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a template. `depth` is the expansion depth the result will be
/// evaluated at: anything at or past [`LOOP_LIMIT`] is always empty, which also
/// bounds the recursion here.
#[allow(clippy::too_many_lines)]
pub(super) fn template(s: &str, depth: usize, deferred: &mut Vec<String>) -> Format {
    if depth >= LOOP_LIMIT {
        return Format::default();
    }
    // C strings: a NUL ends the template.
    let s = s.split('\0').next().unwrap_or_default();
    let b = s.as_bytes();
    let mut out = Builder::default();
    let mut i = 0;
    // Position of the `]` closing the last `#[` (format.c:6838): aliases are
    // not expanded before it.
    let mut style_end: Option<usize> = None;

    while i < b.len() {
        if b[i] != b'#' {
            let j = b[i..]
                .iter()
                .position(|&c| c == b'#')
                .map_or(b.len(), |p| i + p);
            out.text.push_str(&s[i..j]);
            i = j;
            continue;
        }
        i += 1;
        if i >= b.len() {
            break; // a trailing `#` is dropped (format.c:6760)
        }
        let ch = b[i];
        i += 1;
        match ch {
            b'(' => {
                // `#(cmd)`: jobs belong to the server (D9); expands to nothing.
                let mut brackets = 1;
                let mut close = None;
                for (at, &c) in b.iter().enumerate().skip(i) {
                    if c == b'(' {
                        brackets += 1;
                    }
                    if c == b')' {
                        brackets -= 1;
                        if brackets == 0 {
                            close = Some(at);
                            break;
                        }
                    }
                }
                let Some(close) = close else {
                    out.node(Node::Abort);
                    break;
                };
                deferred.push("#()".to_owned());
                i = close + 1;
            }
            b'{' => {
                let Some(close) = skip(s, i - 2, b"}") else {
                    out.node(Node::Abort);
                    break;
                };
                if let Some(node) = replace(&s[i..close], depth, deferred) {
                    out.node(node);
                    i = close + 1;
                } else {
                    out.node(Node::Abort);
                    break;
                }
            }
            b'[' | b'#' => {
                // format.c:6788-6826: `#[` and `##[` (any number of #s before
                // the bracket) are styles, left for the drawing code.
                let mut run = if ch == b'[' { i - 1 } else { i };
                let mut hashes = if ch == b'[' { 1 } else { 2 };
                while b.get(run) == Some(&b'#') {
                    run += 1;
                    hashes += 1;
                }
                if b.get(run) == Some(&b'[') {
                    style_end = skip(s, i - 2, b"]");
                    out.text.push_str(&s[i - 2..=run]);
                    i = run + 1;
                } else {
                    // A run of n #s halves; an odd one stays to start an escape.
                    out.text.extend(std::iter::repeat_n('#', hashes / 2));
                    i = if hashes % 2 == 1 { run - 1 } else { run };
                }
            }
            b'}' | b',' => out.text.push(char::from(ch)),
            _ => {
                if let Some(name) = alias(ch).filter(|_| style_end.is_none_or(|e| i > e)) {
                    out.node(Node::Replace {
                        value: Value::Var(name.to_owned()),
                        reexpand: false,
                    });
                } else {
                    // Unknown: keep the `#`; the next char is copied as is.
                    out.text.push('#');
                    i -= 1;
                }
            }
        }
    }
    out.finish()
}

/// One `#{key}`. `None` is tmux's "failed" return, which ends the enclosing
/// expansion.
fn replace(key: &str, depth: usize, deferred: &mut Vec<String>) -> Option<Node> {
    let (mods, body) = match modifiers(key) {
        Some((mods, off)) => (mods, &key[off..]),
        None => (Vec::new(), key),
    };

    // `t` is supported only bare (`#{t:x}`); `t/p`, `t/f/...` and the other
    // argument forms are deferred.
    let unsupported: Vec<&String> = mods
        .iter()
        .filter(|m| !matches!(m.name.as_str(), "E" | "T" | "||" | "&&" | "==" | "!="))
        .filter(|m| m.name != "t" || m.has_args)
        .map(|m| &m.name)
        .collect();
    if let Some(first) = unsupported.first() {
        let node = Node::Deferred((*first).clone());
        deferred.extend(unsupported.into_iter().cloned());
        return Some(node);
    }
    let mods: Vec<String> = mods.into_iter().map(|m| m.name).collect();

    // `T` would run strftime first; deferred, so it is `E` for now.
    let reexpand = mods.iter().any(|m| m == "E" || m == "T");
    let inner = depth + 1;
    let mut sub = |s: &str| template(s, inner, deferred);

    // The same precedence as format.c:6560-6600 (the `m < >` comparisons, `e`
    // and the loops are among the deferred modifiers).
    let value = if let Some(op) = mods.iter().rev().find(|m| *m == "||" || *m == "&&") {
        let mut parts = Vec::new();
        let mut start = 0;
        loop {
            // format_bool_op_n (format.c:5176): operands split at top-level commas.
            let Some(c) = skip(body, start, b",") else {
                parts.push(sub(&body[start..]));
                break;
            };
            parts.push(sub(&body[start..c]));
            start = c + 1;
        }
        if op == "||" {
            Value::Or(parts)
        } else {
            Value::And(parts)
        }
    } else if let Some(op) = mods.iter().rev().find(|m| *m == "==" || *m == "!=") {
        // format_choose (format.c:4867): no comma is a syntax error.
        let c = skip(body, 0, b",")?;
        Value::Cmp {
            ne: op == "!=",
            left: sub(&body[..c]),
            right: sub(&body[c + 1..]),
        }
    } else if let Some(conds) = body.strip_prefix('?') {
        // format.c:6502-6580.
        let mut arms = Vec::new();
        let mut default = None;
        let mut rest = conds;
        loop {
            let Some(c) = skip(rest, 0, b",") else {
                default = Some(sub(rest));
                break;
            };
            let raw = &rest[..c];
            rest = &rest[c + 1..];
            let end = skip(rest, 0, b",");
            arms.push(CondArm {
                raw: raw.to_owned(),
                test: sub(raw),
                then: sub(&rest[..end.unwrap_or(rest.len())]),
            });
            let Some(end) = end else { break };
            rest = &rest[end + 1..];
        }
        Value::Cond { arms, default }
    } else if body.contains("#{") {
        Value::Inner(sub(body))
    } else if mods.iter().any(|m| m == "t") {
        // FORMAT_TIMESTRING only affects the plain lookup (format_find,
        // format.c:4681-4709).
        Value::Time(body.to_owned())
    } else {
        Value::Var(body.to_owned())
    };
    Some(Node::Replace { value, reexpand })
}

/// One entry of a modifier list.
struct Modifier {
    name: String,
    /// Given arguments (`t/p`, `=3`, `s/a/b/`).
    has_args: bool,
}

/// `format_is_end`.
fn is_end(c: u8) -> bool {
    c == b';' || c == b':'
}

/// `format_build_modifiers` (format.c:4936-5033): the modifier names before the
/// first top-level `:`, and the offset of the body after it. `None` when the
/// text is not a modifier list (then the whole key is the body). Arguments are
/// scanned past, not kept: every modifier given one is deferred.
fn modifiers(key: &str) -> Option<(Vec<Modifier>, usize)> {
    const PLAIN: &[u8] = b"labdnwETSWPOVL!<>A";
    const TWO: [&[u8]; 7] = [b"||", b"&&", b"!!", b"!=", b"==", b"<=", b">="];
    const WITH_ARGS: &[u8] = b"ImCLNPSOVst=pReqWcA";

    let b = key.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut mods: Vec<Modifier> = Vec::new();
    let mut cp = 0;
    let plain = |name: String| Modifier {
        name,
        has_args: false,
    };
    let with_args = |name: String| Modifier {
        name,
        has_args: true,
    };

    while cp < b.len() && b[cp] != b':' {
        if b[cp] == b';' {
            cp += 1;
        }
        if cp >= b.len() {
            break;
        }
        let c = b[cp];
        if PLAIN.contains(&c) && is_end(at(cp + 1)) {
            mods.push(plain(char::from(c).to_string()));
            cp += 1;
            continue;
        }
        if TWO.iter().any(|t| b[cp..].starts_with(t)) && is_end(at(cp + 2)) {
            mods.push(plain(key[cp..cp + 2].to_owned()));
            cp += 2;
            continue;
        }
        if !WITH_ARGS.contains(&c) {
            break;
        }
        let name = char::from(c).to_string();
        if is_end(at(cp + 1)) {
            mods.push(plain(name));
            cp += 1;
            continue;
        }
        let first = at(cp + 1);
        if !first.is_ascii_punctuation() || first == b'-' {
            // One argument, no wrapper character.
            let end = skip(key, cp + 1, b":;")?;
            mods.push(with_args(name));
            cp = end;
            continue;
        }
        // Arguments separated by the wrapper character, e.g. `s/a/b/`.
        let set = [first, b';', b':'];
        cp += 1;
        loop {
            if at(cp) == first && is_end(at(cp + 1)) {
                cp += 1;
                break;
            }
            let Some(end) = skip(key, cp + 1, &set) else {
                break;
            };
            cp = end;
            if is_end(at(cp)) {
                break;
            }
        }
        mods.push(with_args(name));
    }
    if at(cp) != b':' {
        return None;
    }
    Some((mods, cp + 1))
}
