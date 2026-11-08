//! Name rules tmux applies at the edge: session and window name checks
//! (tmux.c `check_name`, `clean_name`), window names from commands
//! (names.c `parse_window_name`), argv quoting (arguments.c `args_escape`)
//! and the non-UTF-8 client sanitiser (utf8.c `utf8_sanitize`).

use std::fmt::Write as _;

use unicode_width::UnicodeWidthChar;
use vtmux_core::rawstr::Unit;

/// `check_name` -> `utf8_isvalid` (utf8.c:827-847): ASCII must be printable
/// (0x20-0x7e); any other valid UTF-8 character is fine.
#[must_use]
pub fn check_name(name: &str) -> bool {
    vtmux_core::rawstr::units(name).all(|u| match u {
        Unit::Char(c) => !c.is_ascii() || (' '..='~').contains(&c),
        Unit::Byte(_) => false,
    })
}

/// `vis(3)` with `VIS_OCTAL|VIS_CSTYLE|VIS_TAB|VIS_NL` over a string that
/// passed [`check_name`] (so only the backslash needs escaping) or any
/// string (control characters become C escapes or octal).
#[must_use]
pub fn vis(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for u in vtmux_core::rawstr::units(s) {
        let c = match u {
            Unit::Byte(b) => {
                let _ = write!(out, "\\{b:03o}");
                continue;
            }
            Unit::Char(c) => c,
        };
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\0' => out.push_str("\\000"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\{:03o}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// `clean_name(name, 0)` (tmux.c:303-317) for a name that passed
/// [`check_name`].
#[must_use]
pub fn clean_name(name: &str) -> String {
    vis(name)
}

/// `clean_name(name, 1)` (tmux.c:303-317) for a name from a program
/// (automatic rename): `#(` loses its `#` so the name cannot run a
/// command when a format shows it, then [`vis`].
#[must_use]
pub fn clean_untrusted(name: &str) -> String {
    vis(&name.replace("#(", "_("))
}

/// `parse_window_name` (names.c:140-176): the first word of a command,
/// without quotes, `exec `, leading spaces or dashes, trailing junk, and
/// with a path reduced to its basename; empty when what is left is not a
/// name `clean_name` takes (`utf8_isvalid`). Read as bytes, as tmux does:
/// trailing junk is any byte glibc's `isalnum` and `ispunct` both refuse,
/// which in a UTF-8 locale is every byte past 0x7f (D-019), so a name
/// loses the non-ASCII characters it ends with.
#[must_use]
pub fn parse_window_name(input: &str) -> String {
    let bytes = vtmux_core::rawstr::to_bytes(input);
    let mut name: &[u8] = &bytes;
    if let Some(rest) = name.strip_prefix(b"\"") {
        name = rest;
    }
    if let Some(q) = name.iter().position(|&b| b == b'"') {
        name = &name[..q];
    }
    if let Some(rest) = name.strip_prefix(b"exec ") {
        name = rest;
    }
    while let [b' ' | b'-', rest @ ..] = name {
        name = rest;
    }
    if let Some(sp) = name.iter().position(|&b| b == b' ') {
        name = &name[..sp];
    }
    // `ptr > name`: the first byte stays.
    while let [_, .., last] = name
        && !(last.is_ascii_alphanumeric() || last.is_ascii_punctuation())
    {
        name = &name[..name.len() - 1];
    }
    if name.first() == Some(&b'/') {
        name = basename(name);
    }
    let name = vtmux_core::rawstr::from_bytes(name);
    if check_name(&name) {
        vis(&name)
    } else {
        String::new()
    }
}

/// POSIX `basename(3)` of a path that starts with `/`: the part after the
/// last `/` once trailing ones are gone, `/` for nothing but slashes.
fn basename(path: &[u8]) -> &[u8] {
    let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
    if end == 0 {
        return b"/";
    }
    let start = path[..end]
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |i| i + 1);
    &path[start..end]
}

/// `args_escape` (arguments.c:606-649): one argument quoted the way
/// `cmd_stringify_argv` prints it.
#[must_use]
pub fn args_escape(s: &str) -> String {
    const DQUOTED: &str = " #';${}%";
    const SQUOTED: &str = " \"";
    if s.is_empty() {
        return "''".to_owned();
    }
    let quote = if s.contains(|c| DQUOTED.contains(c)) {
        Some('"')
    } else if s.contains(|c| SQUOTED.contains(c)) {
        Some('\'')
    } else {
        None
    };
    let mut chars = s.chars();
    if let (Some(first), None) = (chars.next(), chars.next())
        && first != ' '
        && (quote.is_some() || first == '~')
    {
        return format!("\\{first}");
    }
    let mut escaped = vis(s);
    if quote == Some('"') {
        // VIS_DQ: `"` gets a backslash (compat/vis.c:60), and so does a `$`
        // that starts a variable (`$x`, `$_`, `${`; utf8.c:781-786).
        escaped = escaped.replace('"', "\\\"");
        let chars: Vec<char> = escaped.chars().collect();
        let mut out = String::with_capacity(escaped.len());
        for (i, c) in chars.iter().enumerate() {
            if *c == '$'
                && chars
                    .get(i + 1)
                    .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_' || *n == '{')
            {
                out.push('\\');
            }
            out.push(*c);
        }
        escaped = out;
    }
    let tilde = if escaped.starts_with('~') { "\\" } else { "" };
    match quote {
        Some('\'') => format!("'{escaped}'"),
        Some(_) => format!("\"{tilde}{escaped}\""),
        None => format!("{tilde}{escaped}"),
    }
}

/// `cmd_stringify_argv` (cmd.c:364-389).
#[must_use]
pub fn stringify_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| args_escape(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `utf8_sanitize` (utf8.c:855-886): for a client without UTF-8, each
/// non-ASCII character becomes one `_` per display column and every
/// non-printable byte becomes `_`.
#[must_use]
pub fn utf8_sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for u in vtmux_core::rawstr::units(s) {
        let Unit::Char(c) = u else {
            out.push('_');
            continue;
        };
        if c.is_ascii() {
            out.push(if (' '..='~').contains(&c) { c } else { '_' });
        } else {
            for _ in 0..c.width().unwrap_or(0) {
                out.push('_');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: `new -d -s 'a\b'` lists as `a\\b`; a name with a tab is
    /// `invalid session name`.
    #[test]
    fn session_names() {
        assert!(check_name("a.b:c"));
        assert!(check_name("é"));
        assert!(!check_name("tab\tx"));
        assert_eq!(clean_name("a\\b"), "a\\\\b");
    }

    #[test]
    fn window_names_from_commands() {
        assert_eq!(parse_window_name("/bin/sh"), "sh");
        assert_eq!(parse_window_name("sleep 100"), "sleep");
        assert_eq!(parse_window_name("exec  -foo bar"), "foo");
        assert_eq!(parse_window_name("\"/usr/bin/vim\" x"), "vim");
        assert_eq!(parse_window_name("-zsh"), "zsh");
        assert_eq!(parse_window_name("/usr/bin/foo/ x"), "foo");
        assert_eq!(parse_window_name("///"), "/");
        assert_eq!(parse_window_name("/.."), "..");
        assert_eq!(parse_window_name("ab\u{1}"), "ab");
    }

    /// Reference (tmux 3.8 on glibc), `#{pane_current_command}` of
    /// `exec -a NAME sleep 100`: trailing non-ASCII goes byte by byte, the
    /// first byte stays, and a name `utf8_isvalid` refuses is empty.
    #[test]
    fn window_names_as_tmux_reads_the_bytes() {
        assert_eq!(parse_window_name("abé"), "ab");
        assert_eq!(parse_window_name("ab漢"), "ab");
        assert_eq!(parse_window_name("aéb"), "aéb");
        assert_eq!(parse_window_name("é"), "");
        assert_eq!(parse_window_name("a\tb"), "");
        let invalid = vtmux_core::rawstr::from_bytes(b"a\xffb");
        assert_eq!(parse_window_name(&invalid), "");
        assert_eq!(parse_window_name("a\\b"), "a\\\\b");
        assert_eq!(parse_window_name("x#(echo hi)y"), "x#(echo");
    }

    #[test]
    fn argv_quoting() {
        assert_eq!(stringify_argv(&["sleep".into(), "100".into()]), "sleep 100");
        assert_eq!(args_escape(""), "''");
        assert_eq!(args_escape("a b"), "\"a b\"");
        assert_eq!(args_escape("a\"b"), "'a\"b'");
        assert_eq!(args_escape(";"), "\\;");
        assert_eq!(args_escape("~x"), "\\~x");
        // Reference: `#{pane_start_command}` of `exec cat # a$b $1 ${c}`.
        assert_eq!(args_escape("a$b $1 ${c} $_"), "\"a\\$b $1 \\${c} \\$_\"");
    }

    /// Reference: `LANG=C tmux display -p 'é漢x'` prints `___x`.
    #[test]
    fn sanitise_for_non_utf8_clients() {
        assert_eq!(utf8_sanitize("é漢x"), "___x");
        assert_eq!(utf8_sanitize("a\u{1}b\tc"), "a_b_c");
    }
}
