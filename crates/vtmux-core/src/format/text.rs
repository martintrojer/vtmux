//! Text helpers behind the `=` `p` `n` `w` `b` `d` `q` modifiers: width, trimming and
//! padding as format-draw.c and utf8.c do them, POSIX `basename`/`dirname` as glibc does
//! them (the reference tmux is built against glibc's libgen), and the quoting functions.

use super::parse::skip;
use super::wcwidth::char_width;
use crate::rawstr::{Unit, unit_at};
use std::fmt::Write as _;

/// `format_leading_hashes` (format-draw.c:648): the run of `#`s at `i`, its drawn
/// width, and where scanning resumes. A run before `[` that is odd-sized resumes at
/// the last `#`, which starts a style.
fn leading_hashes(b: &[u8], i: usize) -> (usize, usize, usize) {
    let n = b[i..].iter().take_while(|&&c| c == b'#').count();
    if b.get(i + n) != Some(&b'[') {
        return (n, n.div_ceil(2), i + n);
    }
    if n % 2 == 0 {
        (n, n / 2, i + n)
    } else {
        (n, n / 2, i + n - 1)
    }
}

/// The unit at byte `i` of `s` (`i` is on a boundary: only ASCII bytes are skipped
/// one at a time) and its length in `s`.
fn unit(s: &str, i: usize) -> (Unit, usize) {
    unit_at(s, i).unwrap_or((Unit::Char('\0'), 1))
}

/// What one non-`#` unit adds to a width: `Some(w)` for a drawn character, `None`
/// for an ASCII control byte that format-draw.c skips, or a byte that is not
/// UTF-8 (not copied, no width).
fn drawn_width(u: Unit) -> Option<usize> {
    match u {
        Unit::Byte(_) => None,
        Unit::Char(c) if c.is_ascii() => (' '..='~').contains(&c).then_some(1),
        Unit::Char(c) => Some(char_width(c)),
    }
}

/// `format_width` (format-draw.c:1100): the display width of `s`, taking `#[...]`
/// styles and `##` escapes into account.
#[must_use]
pub fn width(s: &str) -> usize {
    let b = s.as_bytes();
    let mut total = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' {
            let (_, lw, end) = leading_hashes(b, i);
            total += lw;
            i = end;
            if b.get(i) == Some(&b'#') {
                let Some(e) = skip(s, i + 2, b"]") else {
                    return 0;
                };
                i = e + 1;
            }
        } else {
            match unit(s, i) {
                // `format_width` has no re-scan: a lead byte takes the whole
                // sequence it announces (utf8_open size), valid or not.
                (Unit::Byte(lead), _) => i += skip_sequence(s, i, lead),
                (u, len) => {
                    total += drawn_width(u).unwrap_or(0);
                    i += len;
                }
            }
        }
    }
    total
}

/// How many bytes of `s` from `i` (on the byte `lead`, not UTF-8)
/// `format_width` consumes: `utf8_open`'s size for a lead byte, else the one
/// byte.
fn skip_sequence(s: &str, i: usize, lead: u8) -> usize {
    let size = match lead {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    // `size` bytes of the text: whole characters, a split one ends with it.
    let mut bytes = 0;
    let mut end = i;
    while bytes < size
        && let Some((u, len)) = unit_at(s, end)
    {
        bytes += u.byte_len();
        end += len;
    }
    end - i
}

/// `format_trim_left` (format-draw.c:1139): the first `limit` columns of `s`.
#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn trim_left(s: &str, limit: usize) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut total = 0;
    let mut i = 0;
    while i < b.len() && total < limit {
        if b[i] == b'#' {
            let (n, lw, end) = leading_hashes(b, i);
            let lw = lw.min(limit - total);
            if lw != 0 {
                out.extend(std::iter::repeat_n('#', if n == 1 { 1 } else { 2 * lw }));
                total += lw;
            }
            i = end;
            if b.get(i) == Some(&b'#') {
                let Some(e) = skip(s, i + 2, b"]") else { break };
                out.push_str(&s[i..=e]);
                i = e + 1;
            }
        } else {
            let (u, len) = unit(s, i);
            if let Some(w) = drawn_width(u) {
                if total + w <= limit {
                    out.push_str(&s[i..i + len]);
                }
                total += w;
            }
            i += len;
        }
    }
    out
}

/// `format_trim_right` (format-draw.c:1200): the last `limit` columns of `s`.
#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn trim_right(s: &str, limit: usize) -> String {
    let total_width = width(s);
    if total_width <= limit {
        return s.to_owned();
    }
    let skip_cols = total_width - limit;
    let b = s.as_bytes();
    let mut out = String::new();
    let mut total = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' {
            let (n, lw, end) = leading_hashes(b, i);
            let mut copy = lw;
            if total <= skip_cols {
                copy = copy.saturating_sub(skip_cols - total);
            }
            if copy != 0 {
                out.extend(std::iter::repeat_n('#', if n == 1 { 1 } else { 2 * copy }));
            }
            total += lw;
            i = end;
            if b.get(i) == Some(&b'#') {
                let Some(e) = skip(s, i + 2, b"]") else { break };
                out.push_str(&s[i..=e]);
                i = e + 1;
            }
        } else {
            let (u, len) = unit(s, i);
            if let Some(w) = drawn_width(u) {
                if total >= skip_cols {
                    out.push_str(&s[i..i + len]);
                }
                total += w;
            }
            i += len;
        }
    }
    out
}

/// `utf8_cstrwidth` (utf8.c:964): width for padding, which does not know about styles.
fn cstr_width(s: &str) -> usize {
    crate::rawstr::units(s)
        .map(|u| match u {
            // `utf8_cstrwidth` tests `*s > 0x1f` on a signed char: a byte
            // that is not UTF-8 is negative, so it has no width.
            Unit::Byte(_) => 0,
            Unit::Char(c) if c.is_ascii() => usize::from(c > '\x1f' && c != '\x7f'),
            Unit::Char(c) => char_width(c),
        })
        .sum()
}

/// `utf8_padcstr` (utf8.c:990): pad on the right with spaces up to `width`.
pub(super) fn pad_right(s: &str, width: usize) -> String {
    let n = cstr_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

/// `utf8_rpadcstr` (utf8.c:1011): pad on the left.
pub(super) fn pad_left(s: &str, width: usize) -> String {
    let n = cstr_width(s);
    format!("{}{s}", " ".repeat(width.saturating_sub(n)))
}

/// POSIX `basename` as glibc's `__xpg_basename` returns it.
pub(super) fn basename(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    trimmed.rsplit('/').next().unwrap_or_default().to_owned()
}

/// POSIX `dirname` as glibc's `dirname` returns it, including its `//` special case.
pub(super) fn dirname(path: &str) -> String {
    let b = path.as_bytes();
    let Some(mut last) = path.rfind('/') else {
        return ".".to_owned();
    };
    // Trailing slashes: look for the slash before the last component.
    if last != 0 && last + 1 == b.len() {
        let mut run = last;
        while run != 0 && b[run - 1] == b'/' {
            run -= 1;
        }
        if run != 0 {
            match path[..run].rfind('/') {
                Some(p) => last = p,
                None => return ".".to_owned(),
            }
        }
    }
    let mut run = last;
    while run != 0 && b[run - 1] == b'/' {
        run -= 1;
    }
    let end = if run == 0 {
        // The last slash opens the path: "/", but exactly two leading slashes stay "//".
        if last == 1 { 2 } else { 1 }
    } else {
        run
    };
    path[..end].to_owned()
}

/// `format_quote_shell` (format.c:4458): backslash the shell's special characters.
pub(super) fn quote_shell(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "|&;<>(){}$`\\\"'*?[# =%\n\t".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `format_quote_shell_single` (format.c:4475): POSIX single quotes.
pub(super) fn quote_shell_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `format_quote_style` (format.c:4498): double every `#`.
pub(super) fn quote_style(s: &str) -> String {
    s.replace('#', "##")
}

/// `args_escape` (arguments.c:606): how tmux prints one argument so that its own
/// parser reads it back, with `vis(3)` flags OCTAL|CSTYLE|TAB|NL (+DQ when
/// double-quoted).
pub(super) fn args_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    let quotes = if s.contains([' ', '#', '\'', ';', '$', '{', '}', '%']) {
        Some('"')
    } else if s.contains([' ', '"']) {
        Some('\'')
    } else {
        None
    };
    let mut chars = s.chars();
    if let (Some(first), None) = (chars.next(), chars.next())
        && first != ' '
        && (quotes.is_some() || first == '~')
    {
        return format!("\\{first}");
    }
    let dq = quotes == Some('"');
    let escaped = vis(s, dq);
    match quotes {
        Some('\'') => format!("'{escaped}'"),
        Some(_) if escaped.starts_with('~') => format!("\"\\{escaped}\""),
        Some(_) => format!("\"{escaped}\""),
        None if escaped.starts_with('~') => format!("\\{escaped}"),
        None => escaped,
    }
}

/// `utf8_strvis` with the octal, C-style, tab and newline flags (and the double-quote
/// flag when `dq`).
fn vis(s: &str, dq: bool) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    let mut it = crate::rawstr::units(s).peekable();
    while let Some(u) = it.next() {
        let next = it.peek().copied().and_then(Unit::char);
        let c = match u {
            Unit::Byte(b) => {
                let _ = write!(out, "\\{b:03o}");
                continue;
            }
            Unit::Char(c) => c,
        };
        match c {
            '$' if dq && next.is_some() => {
                if next.is_some_and(|n| n.is_ascii_alphabetic() || n == '_' || n == '{') {
                    out.push('\\');
                }
                out.push('$');
            }
            '"' if dq => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\x08' => out.push_str("\\b"),
            '\x07' => out.push_str("\\a"),
            '\x0b' => out.push_str("\\v"),
            '\t' => out.push_str("\\t"),
            '\x0c' => out.push_str("\\f"),
            c if c.is_ascii() && !(' '..='~').contains(&c) => {
                let _ = write!(out, "\\{:03o}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirname_and_basename_follow_glibc() {
        // Reference values: set -g @v X ; display #{d:@v} / #{b:@v} (see format_cases.sh).
        for (p, d, b) in [
            ("", ".", "."),
            ("/", "/", "/"),
            ("//", "//", "/"),
            ("///", "/", "/"),
            ("a", ".", "a"),
            ("a/", ".", "a"),
            ("/a", "/", "a"),
            ("//a", "//", "a"),
            ("a/b/", "a", "b"),
            ("/a/b//", "/a", "b"),
            ("a///", ".", "a"),
            ("///a//", "/", "a"),
            ("a//b//c", "a//b", "c"),
        ] {
            assert_eq!(dirname(p), d, "dirname {p:?}");
            assert_eq!(basename(p), b, "basename {p:?}");
        }
    }

    #[test]
    fn trimming_counts_styles_and_escapes() {
        assert_eq!(width("a##b"), 3);
        assert_eq!(width("#[fg=red]ab"), 2);
        assert_eq!(trim_left("#[fg=red]abc", 2), "#[fg=red]ab");
        assert_eq!(trim_right("abcd", 2), "cd");
        assert_eq!(width("世a"), 3);
        assert_eq!(width("a#b"), 3);
        assert_eq!(width("a\x01b"), 2);
        assert_eq!(width("ab#[fg=red"), 0);
        assert_eq!(trim_left("hello", 10), "hello");
        assert_eq!(trim_left("a世b", 2), "a");
        assert_eq!(trim_right("a世b", 2), "b");
        assert_eq!(trim_left("a\x01bc", 2), "ab");
    }
}
