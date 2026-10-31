//! Name rules tmux applies at the edge: session and window name checks
//! (tmux.c `check_name`, `clean_name`), window names from commands
//! (names.c `parse_window_name`), argv quoting (arguments.c `args_escape`)
//! and the non-UTF-8 client sanitiser (utf8.c `utf8_sanitize`).

use std::fmt::Write as _;
use std::path::Path;

use unicode_width::UnicodeWidthChar;

/// `check_name` -> `utf8_isvalid` (utf8.c:827-847): ASCII must be printable
/// (0x20-0x7e); any other valid UTF-8 character is fine.
#[must_use]
pub fn check_name(name: &str) -> bool {
    name.chars()
        .all(|c| !vtmux_core::rawstr::is_raw(c) && (!c.is_ascii() || (' '..='~').contains(&c)))
}

/// `vis(3)` with `VIS_OCTAL|VIS_CSTYLE|VIS_TAB|VIS_NL` over a string that
/// passed [`check_name`] (so only the backslash needs escaping) or any
/// string (control characters become C escapes or octal).
#[must_use]
pub fn vis(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
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
            c if vtmux_core::rawstr::is_raw(c) => {
                let _ = write!(
                    out,
                    "\\{:03o}",
                    vtmux_core::rawstr::to_bytes(&c.to_string())[0]
                );
            }
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
/// with a path reduced to its basename.
#[must_use]
pub fn parse_window_name(input: &str) -> String {
    let mut name = input;
    if let Some(rest) = name.strip_prefix('"') {
        name = rest;
    }
    if let Some(q) = name.find('"') {
        name = &name[..q];
    }
    if let Some(rest) = name.strip_prefix("exec ") {
        name = rest;
    }
    name = name.trim_start_matches([' ', '-']);
    if let Some(sp) = name.find(' ') {
        name = &name[..sp];
    }
    // Trailing characters that are neither alphanumeric nor punctuation.
    let mut end = name.len();
    while end > 1 {
        let c = name.as_bytes()[end - 1];
        if c.is_ascii_alphanumeric() || c.is_ascii_punctuation() || !c.is_ascii() {
            break;
        }
        end -= 1;
    }
    let name = &name[..end];
    let name = if name.starts_with('/') {
        Path::new(name)
            .file_name()
            .map_or_else(|| "/".to_owned(), |b| b.to_string_lossy().into_owned())
    } else {
        name.to_owned()
    };
    vis(&name)
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
    for c in s.chars() {
        if vtmux_core::rawstr::is_raw(c) {
            out.push('_');
        } else if c.is_ascii() {
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
