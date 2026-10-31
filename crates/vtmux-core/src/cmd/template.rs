//! Prompt templates: `%1`…`%9`, `%%` and `%%%` replaced with what was typed
//! (cmd.c `cmd_template_replace`), in a command string or every string
//! argument of a parsed block (arguments.c `args_copy_copy_value`,
//! cmd.c `cmd_list_copy`).

use super::{ArgValue, CommandList};

/// `cmd_template_replace(template, s, idx)`.
///
/// - `%idx` is `s` as it is, every time it appears; `%idx%` is `s` with
///   `"\$;~` backslash-escaped.
/// - The first `%%` that is not `%idx` is `s` with each `'` written `'\''`
///   (for a template already in single quotes); `%%%` is the double-quote
///   escaping instead. Later `%%` stay as they are.
#[must_use]
pub fn replace(template: &str, s: &str, idx: u8) -> String {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    if !template.contains('%') {
        return template.to_owned();
    }
    let digit = char::from(b'0' + idx);
    let mut out = String::with_capacity(template.len());
    let mut replaced = false;
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        if c != '%' {
            out.push(c);
            continue;
        }
        let quote = if (1..=9).contains(&idx) && rest.starts_with(digit) {
            rest = &rest[1..];
            if let Some(r) = rest.strip_prefix('%') {
                rest = r;
                Quote::Double
            } else {
                Quote::None
            }
        } else if rest.starts_with('%') && !replaced {
            replaced = true;
            rest = &rest[1..];
            if let Some(r) = rest.strip_prefix('%') {
                rest = r;
                Quote::Double
            } else {
                Quote::Single
            }
        } else {
            out.push('%');
            continue;
        };
        for ch in s.chars() {
            if quote == Quote::Single && ch == '\'' {
                out.push_str("'\\''");
                continue;
            }
            if quote == Quote::Double && "\"\\$;~".contains(ch) {
                out.push('\\');
            }
            out.push(ch);
        }
    }
    out
}

/// `cmd_list_copy(list, argc, argv)`: the block with every string
/// argument, flag values included, run through [`replace`] for each of
/// `argv` in turn (`%1` the first).
#[must_use]
pub fn replace_list(list: &CommandList, argv: &[String]) -> CommandList {
    let fill = |text: &str| {
        argv.iter().enumerate().fold(text.to_owned(), |t, (i, a)| {
            replace(&t, a, u8::try_from(i + 1).unwrap_or(u8::MAX))
        })
    };
    let mut out = list.clone();
    for call in &mut out.calls {
        for values in call.args.flags.values_mut() {
            for v in values.iter_mut().flatten() {
                *v = fill(v);
            }
        }
        for value in &mut call.args.positionals {
            *value = match value {
                ArgValue::Str(s) => ArgValue::Str(fill(s)),
                ArgValue::Bytes { text, .. } => ArgValue::Str(fill(text)),
                ArgValue::Commands(inner) => ArgValue::Commands(replace_list(inner, argv)),
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values from tmux 3.8 regress/cmd-template-replace.sh and
    /// cmd.c `cmd_template_replace` read as the spec.
    #[test]
    fn replacements() {
        assert_eq!(replace("set @r '%%'", "can't", 1), "set @r 'can'\\''t'");
        assert_eq!(replace("a '%%' ; b '%%'", "x", 1), "a 'x' ; b '%%'");
        assert_eq!(
            replace("r \"%%%\"", "a\"$;~\\z", 1),
            "r \"a\\\"\\$\\;\\~\\\\z\""
        );
        assert_eq!(replace("r %1 ; s %1", "v", 1), "r v ; s v");
        assert_eq!(replace("one %1 two %2", "B", 2), "one %1 two B");
        assert_eq!(replace("d \"%2%\"", "b\"y", 2), "d \"b\\\"y\"");
        assert_eq!(replace("no markers", "x", 1), "no markers");
        assert_eq!(replace("100% %x", "x", 1), "100% %x");
        // %% after %1 replaced is still the first %%.
        assert_eq!(replace("%1 %%", "q", 1), "q q");
    }
}
