//! The canonical command printer: parsed commands back to text, the way
//! `list-keys` shows a binding.
//!
//! Three tmux functions, one module:
//!
//! - [`args_escape`]: arguments.c `args_escape` (606-650), one argument.
//! - `args_print` (arguments.c:548-603), the flags and arguments of one
//!   command, reached through [`CommandCall::print`] (`cmd_print`, cmd.c:580).
//! - [`CommandList::print`]: `cmd_list_print` (cmd.c:685-720), the
//!   separators.
//!
//! What this normalises, all visible in `list-keys`: flags regroup
//! (`choose-tree -sZ -O name` prints `choose-tree -Zs -O name`: bare flags
//! first, sorted by flag byte, then flags with values, sorted), aliases are
//! gone (the table entry's name prints), `$HOME` was expanded when it was
//! parsed, and quoting is `args_escape`'s, not the source's.

use std::fmt::Write as _;

use super::args::{ArgValue, Args};
use super::{CommandCall, CommandList};

/// `CMD_LIST_PRINT_*` (tmux.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrintFlags {
    /// `CMD_LIST_PRINT_ESCAPED`: separators are ` \; ` and ` \;\; `, so the
    /// text can be fed back to a command parser.
    pub escaped: bool,
    /// `CMD_LIST_PRINT_NO_GROUPS`: every separator is the single one.
    pub no_groups: bool,
}

impl PrintFlags {
    /// What `list-keys` passes (cmd-list-keys.c:154).
    pub const LIST_KEYS: Self = Self {
        escaped: true,
        no_groups: true,
    };
}

impl CommandList {
    /// `cmd_list_print`: the commands joined by ` ; `, or ` ;; ` where the
    /// next command is in another group (unless `no_groups`). `escaped`
    /// makes the separators ` \; ` and ` \;\; `.
    #[must_use]
    pub fn print(&self, flags: PrintFlags) -> String {
        let (single, double) = if flags.escaped {
            (" \\; ", " \\;\\; ")
        } else {
            (" ; ", " ;; ")
        };
        let mut out = String::new();
        for (i, call) in self.calls.iter().enumerate() {
            out.push_str(&call.print());
            if let Some(next) = self.calls.get(i + 1) {
                let separator = if !flags.no_groups && call.group != next.group {
                    double
                } else {
                    single
                };
                out.push_str(separator);
            }
        }
        out
    }
}

impl CommandCall {
    /// `cmd_print`: the command name, then `args_print`.
    #[must_use]
    pub fn print(&self) -> String {
        let args = args_print(self);
        if args.is_empty() {
            self.entry.name.to_owned()
        } else {
            format!("{} {args}", self.entry.name)
        }
    }
}

/// `args_print`. The flags are a tree keyed by flag byte, so both passes
/// walk them in byte order.
fn args_print(call: &CommandCall) -> String {
    let template = call.entry.template;
    // `x::` flags: the entry carries ARGS_ENTRY_OPTIONAL_VALUE when its
    // FIRST occurrence had no value (`args_set` sets entry flags only when
    // it creates the entry).
    let optional = |flag: char, args: &Args| {
        let Some(at) = template.find(flag) else {
            return false;
        };
        template[at + 1..].starts_with("::") && args.flags[&flag].first() == Some(&None)
    };
    let args: &Args = &call.args;
    let mut buf = String::new();

    // Flags without values.
    for (flag, occurrences) in &args.flags {
        if optional(*flag, args) || occurrences.iter().any(Option::is_some) {
            continue;
        }
        if buf.is_empty() {
            buf.push('-');
        }
        for _ in occurrences {
            buf.push(*flag);
        }
    }

    // Then the flags with values; an optional-value flag prints bare, once,
    // whatever it was given later.
    let mut last_optional = false;
    for (flag, occurrences) in &args.flags {
        if optional(*flag, args) {
            add_flag(&mut buf, *flag);
            last_optional = true;
            continue;
        }
        let values: Vec<&str> = occurrences.iter().filter_map(|v| v.as_deref()).collect();
        if values.is_empty() {
            continue;
        }
        for value in values {
            add_flag(&mut buf, *flag);
            add_value(&mut buf, &ArgValue::Str(value.to_owned()));
        }
        last_optional = false;
    }
    if last_optional {
        buf.push_str(" --");
    }

    // And finally the argument vector.
    for value in &args.positionals {
        add_value(&mut buf, value);
    }
    buf
}

fn add_flag(buf: &mut String, flag: char) {
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push('-');
    buf.push(flag);
}

/// `args_print_add_value`: a separating space (when `buf` is not empty),
/// then a block as `{ commands }` or a string through [`args_escape`].
fn add_value(buf: &mut String, value: &ArgValue) {
    if !buf.is_empty() {
        buf.push(' ');
    }
    match value {
        ArgValue::Commands(list) => {
            let _ = write!(buf, "{{ {} }}", list.print(PrintFlags::default()));
        }
        ArgValue::Str(s) | ArgValue::Bytes { text: s, .. } => buf.push_str(&args_escape(s)),
    }
}

/// `args_escape`: quote one argument so tmux's parser reads it back.
///
/// Characters from `" #';${}%"` get double quotes, a space or `"` alone gets
/// single quotes, a lone character that needed quotes (or `~`) is a
/// backslash escape instead, and an empty string is `''`. Inside, bytes are
/// written as `vis(3)` does with `VIS_OCTAL|VIS_CSTYLE|VIS_TAB|VIS_NL`
/// (compat/vis.c, through `utf8_stravis`): valid UTF-8 passes through,
/// `\n \r \t` and friends are C escapes, other controls are `\ooo`, `\` is
/// doubled and, in double quotes, `"` is escaped and so is `$` before a
/// letter, `_` or `{` (utf8.c:761-795).
#[must_use]
pub fn args_escape(s: &str) -> String {
    const DQUOTED: &str = " #';${}%";
    const SQUOTED: &str = " \"";
    if s.is_empty() {
        return "''".to_owned();
    }
    let quotes = if s.contains(|c| DQUOTED.contains(c)) {
        Some('"')
    } else if s.contains(|c| SQUOTED.contains(c)) {
        Some('\'')
    } else {
        None
    };
    let mut chars = s.chars();
    let first = chars.next().expect("not empty");
    if first != ' '
        && chars.next().is_none()
        && first.is_ascii()
        && (quotes.is_some() || first == '~')
    {
        return format!("\\{first}");
    }
    let escaped = vis(s, quotes == Some('"'));
    match quotes {
        Some('\'') => format!("'{escaped}'"),
        Some(_) if escaped.starts_with('~') => format!("\"\\{escaped}\""),
        Some(_) => format!("\"{escaped}\""),
        None if escaped.starts_with('~') => format!("\\{escaped}"),
        None => escaped,
    }
}

/// `utf8_strvis` with the flags `args_escape` uses.
fn vis(s: &str, double_quoted: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if !c.is_ascii() {
            out.push(c);
            continue;
        }
        let next = chars.peek().copied();
        match c {
            '$' if double_quoted => {
                // Only when another byte follows (`src < end - 1`).
                if next.is_some_and(|n| n.is_ascii_alphabetic() || n == '_' || n == '{') {
                    out.push('\\');
                }
                out.push('$');
            }
            '"' if double_quoted => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // isgraph, plus the space (VIS_SP is not set).
            ' '..='~' => out.push(c),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{7}' => out.push_str("\\a"),
            '\u{b}' => out.push_str("\\v"),
            '\t' => out.push_str("\\t"),
            '\u{c}' => out.push_str("\\f"),
            '\0' => {
                // `\0`, and `\000` when an octal digit follows, so it does
                // not run into it.
                out.push_str(if next.is_some_and(|n| ('0'..='7').contains(&n)) {
                    "\\000"
                } else {
                    "\\0"
                });
            }
            _ => {
                let _ = write!(out, "\\{:03o}", u32::from(c));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_plain_and_empty() {
        assert_eq!(args_escape("abc"), "abc");
        assert_eq!(args_escape(""), "''");
        assert_eq!(args_escape("é"), "é");
    }

    #[test]
    fn escape_single_characters() {
        // A lone character that needs quotes is a backslash escape.
        assert_eq!(args_escape(";"), "\\;");
        assert_eq!(args_escape("#"), "\\#");
        assert_eq!(args_escape("\""), "\\\"");
        assert_eq!(args_escape("~"), "\\~");
        assert_eq!(args_escape(" "), "\" \"");
        assert_eq!(args_escape("\\"), "\\\\");
    }

    #[test]
    fn escape_quotes() {
        assert_eq!(args_escape("a b"), "\"a b\"");
        assert_eq!(args_escape("a\"b"), "'a\"b'");
        assert_eq!(args_escape("a\"b c"), "\"a\\\"b c\"");
        assert_eq!(args_escape("~a"), "\\~a");
        assert_eq!(args_escape("~a b"), "\"\\~a b\"");
        assert_eq!(args_escape("$HOME"), "\"\\$HOME\"");
        assert_eq!(args_escape("x $((1)) $"), "\"x $((1)) $\"");
    }

    #[test]
    fn escape_controls() {
        assert_eq!(args_escape("a\tb"), "a\\tb");
        assert_eq!(args_escape("a\nb"), "a\\nb");
        assert_eq!(args_escape("\u{1b}"), "\\033");
        assert_eq!(args_escape("\u{7f}"), "\\177");
        assert_eq!(args_escape("a\0b"), "a\\0b");
        assert_eq!(args_escape("a\x001"), "a\\0001");
    }
}
