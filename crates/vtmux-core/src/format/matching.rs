//! The `m` and `s` edge (`format.c:format_match/format_sub`, `regsub.c:regsub`).
use super::parse::RawMod;
use super::{Eval, Format, Post, Value, parse};
use crate::regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PatternMatch {
    flags: Option<Format>,
    left: Format,
    right: Format,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Substitute {
    args: Vec<Format>,
}

pub(super) fn build_match(
    m: &RawMod,
    body: &str,
    depth: usize,
    deferred: &mut Vec<String>,
) -> Option<Value> {
    let comma = parse::skip(body, 0, b",")?;
    Some(Value::Match(PatternMatch {
        flags: m
            .args
            .first()
            .map(|s| parse::template(&parse::unescape(s), depth, deferred)),
        left: parse::template(&body[..comma], depth, deferred),
        right: parse::template(&body[comma + 1..], depth, deferred),
    }))
}

pub(super) fn add_substitutions(
    post: &mut Post,
    list: &[RawMod],
    depth: usize,
    deferred: &mut Vec<String>,
) {
    post.substitutions = list
        .iter()
        .filter(|m| m.name == "s" && m.args.len() >= 2)
        .map(|m| Substitute {
            args: m
                .args
                .iter()
                .map(|s| parse::template(&parse::unescape(s), depth, deferred))
                .collect(),
        })
        .collect();
}

impl Eval<'_> {
    pub(super) fn pattern_match(&mut self, m: &PatternMatch, depth: usize) -> String {
        let flags = m
            .flags
            .as_ref()
            .map(|f| self.expand(f, depth))
            .unwrap_or_default();
        let left = self.expand(&m.left, depth);
        let right = self.expand(&m.right, depth);
        if flags.contains(['p', 'z']) {
            return super::fuzzy::matches(&left, &right, flags.contains('p'));
        }
        let yes = if flags.contains('r') {
            // format.c:format_match returns "0" when regcomp or regexec fails
            // for any reason, including resource exhaustion; so does a limit here.
            Regex::new(&left, flags.contains('i'))
                .and_then(|r| r.is_match(&right))
                .unwrap_or(false)
        } else {
            crate::fnmatch::matches(&left, &right, flags.contains('i'))
        };
        if yes { "1" } else { "0" }.to_owned()
    }

    pub(super) fn substitute(&mut self, s: &Substitute, text: &str, depth: usize) -> String {
        // format_build_modifiers expands once; format_replace expands pattern
        // and replacement a second time at the done label (flags only once).
        let args: Vec<String> = s.args.iter().map(|f| self.expand(f, depth)).collect();
        let pattern = self.expand(&parse::template(&args[0], depth, &mut Vec::new()), depth);
        let with = self.expand(&parse::template(&args[1], depth, &mut Vec::new()), depth);
        regsub(
            &pattern,
            &with,
            text,
            args.get(2).is_some_and(|s| s.contains('i')),
        )
    }
}

/// `regsub` (regsub.c) over the bytes `text` stands for: an empty match
/// moves on one byte, which can be inside a character, and the search from
/// there sees the rest of that character as bytes that are not UTF-8.
fn regsub(pattern: &str, with: &str, text: &str, icase: bool) -> String {
    use crate::rawstr::{byte_len, from_bytes, from_vec, to_bytes};
    if text.is_empty() || pattern.is_empty() {
        return text.to_owned();
    }
    // A regcomp failure makes regsub return NULL; format_sub then keeps the text.
    let Ok(regex) = Regex::new(pattern, icase) else {
        return text.to_owned();
    };
    let bytes = to_bytes(text);
    let end = bytes.len();
    let mut out = Vec::new();
    let (mut start, mut last, mut empty) = (0usize, 0usize, false);
    while start <= end {
        // regexec(text + start): offsets in `rest` are mapped back to bytes.
        let rest = from_bytes(&bytes[start..]);
        let at = |i: usize| start + byte_len(&rest[..i]);
        // regsub.c: any regexec failure (no match, or a resource limit) copies
        // the rest of the text and stops; earlier replacements stand.
        let Ok(Some(m)) = regex.find(&rest) else {
            out.extend_from_slice(&bytes[start..]);
            break;
        };
        let captures: Vec<Option<std::ops::Range<usize>>> = m
            .captures
            .iter()
            .map(|c| c.as_ref().map(|r| at(r.start)..at(r.end)))
            .collect();
        let whole = captures[0].clone().unwrap();
        let (so, eo) = (whole.start, whole.end);
        out.extend_from_slice(&bytes[last..so]);
        if pattern.starts_with('^') {
            replacement(&mut out, with, &bytes, &captures);
            out.extend_from_slice(&bytes[eo..]);
            break;
        }
        if empty || so != last || so != eo {
            replacement(&mut out, with, &bytes, &captures);
            last = eo;
            start = eo;
            empty = false;
        } else {
            last = eo;
            start = eo + 1;
            empty = true;
        }
    }
    from_vec(out)
}

/// `regsub_expand`: `with`, `\\N` replaced by capture N's bytes.
fn replacement(
    out: &mut Vec<u8>,
    with: &str,
    bytes: &[u8],
    captures: &[Option<std::ops::Range<usize>>],
) {
    use crate::rawstr::Unit;
    let mut units = crate::rawstr::units(with);
    while let Some(mut u) = units.next() {
        if u == Unit::Char('\\')
            && let Some(next) = units.next()
        {
            u = next;
            if let Some(n) = u.char().and_then(|c| c.to_digit(10))
                && let Some(Some(range)) = captures.get(usize::try_from(n).unwrap())
                && !range.is_empty()
            {
                out.extend_from_slice(&bytes[range.clone()]);
                continue;
            }
        }
        match u {
            Unit::Char(c) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            Unit::Byte(b) => out.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FormatContext, expand_str};
    struct Context;
    impl FormatContext for Context {
        fn get(&self, name: &str) -> Option<String> {
            match name {
                "@v" => Some("ababa".to_owned()),
                "@inner" => Some("#{@v}".to_owned()),
                _ => None,
            }
        }
    }
    include!("matching_golden.in");

    /// A backreference search that trips the work cap behaves like a failed
    /// `regexec` in tmux (format.c:`format_match`, regsub.c:`regsub`).
    #[test]
    fn format_matching_limit_is_regexec_failure() {
        let tail = "a".repeat(3000);
        assert_eq!(
            crate::regex::Regex::new("(a*)\\1b", false)
                .unwrap()
                .find(&tail),
            Err(crate::regex::Error::Limit)
        );
        assert_eq!(
            expand_str(&format!("#{{m/r:(a*)\\1b,{tail}}}"), &Context),
            "0"
        );
        let text = format!("aab {tail}");
        assert_eq!(
            super::regsub("(a*)\\1b", "X", &text, false),
            format!("X {tail}")
        );
    }
    #[test]
    fn format_matching_reference_goldens() {
        for (fmt, expected) in rows() {
            assert_eq!(expand_str(fmt, &Context), expected, "{fmt}");
        }
    }
}
