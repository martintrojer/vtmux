//! Template parser. Ports `format_expand1`'s scanning (format.c:6708-6893),
//! `format_skip1` (4829), `format_build_modifiers` (4936) and the dispatch in
//! `format_replace` (6019-6600) into a typed AST.

use super::mods;
use super::{Format, LOOP_LIMIT, Node};

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

    fn finish(mut self, src: &str) -> Format {
        self.flush();
        Format {
            nodes: self.nodes,
            src: src.contains('%').then(|| src.into()),
        }
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
                        value: super::Value::Var {
                            name: name.to_owned(),
                            vm: super::VarMods::default(),
                        },
                        post: Box::default(),
                    });
                } else {
                    // Unknown: keep the `#`; the next char is copied as is.
                    out.text.push('#');
                    i -= 1;
                }
            }
        }
    }
    out.finish(s)
}

/// One `#{key}`. `None` is tmux's "failed" return, which ends the enclosing
/// expansion.
fn replace(key: &str, depth: usize, deferred: &mut Vec<String>) -> Option<Node> {
    let (list, body) = match modifiers(key) {
        Some((list, off)) => (list, &key[off..]),
        None => (Vec::new(), key),
    };

    let unsupported: Vec<&str> = list
        .iter()
        .map(|m| m.name.as_str())
        .filter(|m| !mods::SUPPORTED.contains(m))
        .collect();
    if let Some(first) = unsupported.first() {
        let node = Node::Deferred((*first).to_owned());
        deferred.extend(unsupported.into_iter().map(str::to_owned));
        return Some(node);
    }
    let (value, post) = mods::build(&list, body, depth + 1, deferred)?;
    Some(Node::Replace {
        value,
        post: Box::new(post),
    })
}

/// `format_is_end`.
fn is_end(c: u8) -> bool {
    c == b';' || c == b':'
}

/// A modifier as written: its name and its arguments (unescaped and expanded later).
#[derive(Debug, Clone)]
pub(super) struct RawMod {
    pub name: String,
    pub args: Vec<String>,
}

/// `format_unescape` (format.c:4768): drop the escaping `#` of `#,` `##` `#{` `#}` `#:`
/// outside any `#{...}`.
pub(super) fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut brackets = 0i32;
    let mut i = 0;
    while i < b.len() {
        let next = b.get(i + 1).copied();
        if b[i] == b'#' && next == Some(b'{') {
            brackets += 1;
        }
        if brackets == 0 && b[i] == b'#' && next.is_some_and(|n| b",#{}:".contains(&n)) {
            out.push(b[i + 1]);
            i += 2;
            continue;
        }
        if b[i] == b'}' {
            brackets -= 1;
        }
        out.push(b[i]);
        i += 1;
    }
    // Only ASCII `#`s were removed, so this is still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// `format_build_modifiers` (format.c:4936-5033): the modifiers before the first
/// top-level `:`, and the offset of the body after it. `None` when the text is not a
/// modifier list (then the whole key is the body).
fn modifiers(key: &str) -> Option<(Vec<RawMod>, usize)> {
    const PLAIN: &[u8] = b"labdnwETSWPOVL!<>A";
    const TWO: [&[u8]; 7] = [b"||", b"&&", b"!!", b"!=", b"==", b"<=", b">="];
    const WITH_ARGS: &[u8] = b"ImCLNPSOVst=pReqWcA";

    let b = key.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut mods: Vec<RawMod> = Vec::new();
    let mut cp = 0;
    let plain = |name: &str| RawMod {
        name: name.to_owned(),
        args: Vec::new(),
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
            mods.push(plain(&key[cp..=cp]));
            cp += 1;
            continue;
        }
        if TWO.iter().any(|t| b[cp..].starts_with(t)) && is_end(at(cp + 2)) {
            mods.push(plain(&key[cp..cp + 2]));
            cp += 2;
            continue;
        }
        if !WITH_ARGS.contains(&c) {
            break;
        }
        let name = char::from(c).to_string();
        if is_end(at(cp + 1)) {
            mods.push(plain(&name));
            cp += 1;
            continue;
        }
        let first = at(cp + 1);
        if !first.is_ascii_punctuation() || first == b'-' {
            // One argument, no wrapper character.
            let end = skip(key, cp + 1, b":;")?;
            mods.push(RawMod {
                name,
                args: vec![key[cp + 1..end].to_owned()],
            });
            cp = end;
            continue;
        }
        // Arguments separated by the wrapper character, e.g. `s/a/b/`.
        let set = [first, b';', b':'];
        let mut args = Vec::new();
        cp += 1;
        loop {
            if at(cp) == first && is_end(at(cp + 1)) {
                cp += 1;
                break;
            }
            let Some(end) = skip(key, cp + 1, &set) else {
                break;
            };
            args.push(key[cp + 1..end].to_owned());
            cp = end;
            if is_end(at(cp)) {
                break;
            }
        }
        mods.push(RawMod { name, args });
    }
    if at(cp) != b':' {
        return None;
    }
    Some((mods, cp + 1))
}
