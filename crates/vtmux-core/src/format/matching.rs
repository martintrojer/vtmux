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

/// Byte-oriented empty-match progression is intentional (regsub.c:regsub).
/// The public format API uses UTF-8 strings: tmux can split a character here,
/// so retain all original bytes and convert only at the final boundary.
fn regsub(pattern: &str, with: &str, text: &str, icase: bool) -> String {
    if text.is_empty() || pattern.is_empty() {
        return text.to_owned();
    }
    // A regcomp failure makes regsub return NULL; format_sub then keeps the text.
    let Ok(regex) = Regex::new(pattern, icase) else {
        return text.to_owned();
    };
    let mut out = Vec::new();
    let (mut start, mut last, mut empty) = (0usize, 0usize, false);
    while start <= text.len() {
        // regexec can search after an invalid UTF-8 continuation byte. For empty
        // matches this offset itself is meaningful; other patterns skip to the
        // next boundary. Preserve the byte progress from regsub.
        let mut boundary = start;
        while boundary < text.len() && !text.is_char_boundary(boundary) {
            boundary += 1;
        }
        // regsub.c: any regexec failure (no match, or a resource limit) copies
        // the rest of the text and stops; earlier replacements stand.
        let result = regex.find(&text[boundary..]);
        let Ok(Some(m)) = result else {
            out.extend_from_slice(&text.as_bytes()[start..]);
            break;
        };
        let range = m.captures[0].clone().unwrap();
        let so = boundary + range.start;
        let eo = boundary + range.end;
        out.extend_from_slice(&text.as_bytes()[last..so]);
        if pattern.starts_with('^') {
            replacement(&mut out, with, &text[boundary..], &m.captures);
            out.extend_from_slice(&text.as_bytes()[eo..]);
            break;
        }
        if empty || so != last || so != eo {
            replacement(&mut out, with, &text[boundary..], &m.captures);
            last = eo;
            start = eo;
            empty = false;
        } else {
            last = eo;
            start = eo + 1;
            empty = true;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn replacement(
    out: &mut Vec<u8>,
    with: &str,
    text: &str,
    captures: &[Option<std::ops::Range<usize>>],
) {
    let mut chars = with.chars();
    while let Some(mut c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            c = next;
            if let Some(n) = c.to_digit(10)
                && let Some(Some(range)) = captures.get(usize::try_from(n).unwrap())
                && !range.is_empty()
            {
                out.extend_from_slice(&text.as_bytes()[range.clone()]);
                continue;
            }
        }
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
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
