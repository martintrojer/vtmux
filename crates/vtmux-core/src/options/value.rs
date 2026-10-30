//! Option values: what `set-option` turns text into (`options_from_string`,
//! `options_array_set`, options.c) and how tmux prints them back
//! (`options_value_to_string`).

use std::collections::BTreeMap;

use super::OptionError;
use super::table::{ArrayKey, Kind, OptionDef, StringCheck};
use crate::cmd::{CommandList, ConfigEnv, parse_string, strtonum};
use crate::colour::Colour;
use crate::keys::Key;
use crate::style::{validate_colour_string_option, validate_style_option};

/// One value: a whole option, or one item of an array option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    String(String),
    Number(i64),
    Key(Key),
    Colour(Colour),
    Flag(bool),
    /// An index into the option's [`Kind::Choice`] names.
    Choice(usize),
    Command(CommandList),
}

impl Value {
    /// `options_value_to_string`. `numeric` is what format expansion asks
    /// for: flags as `1`/`0` instead of `on`/`off`.
    #[must_use]
    pub fn to_text(&self, def: Option<&OptionDef>, numeric: bool) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Number(n) => n.to_string(),
            Self::Key(k) => k.name(false),
            Self::Colour(c) => c.to_string(),
            Self::Flag(on) => match (numeric, on) {
                (true, true) => "1".to_owned(),
                (true, false) => "0".to_owned(),
                (false, true) => "on".to_owned(),
                (false, false) => "off".to_owned(),
            },
            Self::Choice(n) => match def.map(|d| d.kind) {
                Some(Kind::Choice(names)) => names.get(*n).copied().unwrap_or_default().to_owned(),
                _ => n.to_string(),
            },
            Self::Command(list) => list.print(crate::cmd::PrintFlags::default()),
        }
    }
}

/// An option's value: one [`Value`], or an array's items by key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionValue {
    Single(Value),
    Array(BTreeMap<ArrayKey, Value>),
}

impl OptionValue {
    /// `options_to_string`: the whole value, or one item (`""` when the key
    /// has no item). A whole array is its items joined by spaces.
    #[must_use]
    pub fn to_text(
        &self,
        def: Option<&OptionDef>,
        key: Option<&ArrayKey>,
        numeric: bool,
    ) -> String {
        match (self, key) {
            (Self::Single(v), _) => v.to_text(def, numeric),
            (Self::Array(items), None) => items
                .values()
                .map(|v| v.to_text(def, numeric))
                .collect::<Vec<_>>()
                .join(" "),
            (Self::Array(items), Some(k)) => items
                .get(k)
                .map(|v| v.to_text(def, numeric))
                .unwrap_or_default(),
        }
    }
}

/// `checkshell` (tmux.c:98) needs the file system; the server answers it.
pub trait ShellCheck {
    /// Whether `path` is absolute, executable and not tmux itself.
    fn usable_shell(&self, path: &str) -> bool;
}

/// Parse a command option's text (`cmd_parse_from_string`).
pub(super) fn parse_command(
    text: &str,
    env: &mut dyn ConfigEnv,
) -> Result<CommandList, OptionError> {
    parse_string(text, env).map_err(|e| OptionError::Command(e.to_string()))
}

/// The checks `options_from_string_check` (options.c:1185) applies to a
/// string option's new value.
pub(super) fn check_string(
    check: StringCheck,
    value: &str,
    shell: &dyn ShellCheck,
) -> Result<(), OptionError> {
    match check {
        StringCheck::Shell if !shell.usable_shell(value) => {
            Err(OptionError::NotSuitableShell(value.to_owned()))
        }
        StringCheck::Size if !matches_size(value) => Err(OptionError::ValueIs {
            why: "invalid",
            value: value.to_owned(),
        }),
        StringCheck::None | StringCheck::Shell | StringCheck::Size => Ok(()),
        StringCheck::Style => validate_style_option(value).map_err(OptionError::InvalidStyle),
        StringCheck::Colour => {
            validate_colour_string_option(value).map_err(OptionError::InvalidColour)
        }
    }
}

/// `fnmatch("[0-9]*x[0-9]*", value, 0)`: a digit, anything, `x`, a digit,
/// anything. (`*` matches any characters, `x` included.)
fn matches_size(value: &str) -> bool {
    let b = value.as_bytes();
    if !b.first().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    // Some `x` after the first digit followed by a digit.
    b.windows(2)
        .skip(1)
        .any(|w| w[0] == b'x' && w[1].is_ascii_digit())
}

/// `options_from_string` (options.c:1286) for an option that is not an
/// array, given its current (possibly inherited) value for the toggles.
/// Strings come back unchecked: appending and the checks need the tree.
pub(super) fn parse_scalar(
    def: &OptionDef,
    value: Option<&str>,
    current: Option<&Value>,
    env: &mut dyn ConfigEnv,
) -> Result<Value, OptionError> {
    // options.c:1290: only flags and choices may toggle.
    let need = |v: Option<&str>| v.map(str::to_owned).ok_or(OptionError::EmptyValue);
    match def.kind {
        Kind::String(_) => Ok(Value::String(need(value)?)),
        Kind::Number { min, max } => {
            let text = need(value)?;
            strtonum(&text, i64::from(min), i64::from(max))
                .map(Value::Number)
                .map_err(|why| OptionError::ValueIs {
                    why: match why {
                        crate::cmd::NumError::TooSmall => "too small",
                        crate::cmd::NumError::TooLarge => "too large",
                        crate::cmd::NumError::Missing | crate::cmd::NumError::Invalid => "invalid",
                    },
                    value: text,
                })
        }
        Kind::Key => {
            let text = need(value)?;
            Key::from_name(&text)
                .map(Value::Key)
                .ok_or(OptionError::BadKey(text))
        }
        Kind::Colour => {
            let text = need(value)?;
            Colour::parse_option(&text)
                .map(Value::Colour)
                .map_err(OptionError::BadColour)
        }
        Kind::Flag => {
            // options_from_string_flag (options.c:1214).
            let on = match value {
                None | Some("") => !matches!(current, Some(Value::Flag(true))),
                Some(v)
                    if v == "1"
                        || v.eq_ignore_ascii_case("on")
                        || v.eq_ignore_ascii_case("yes") =>
                {
                    true
                }
                Some(v)
                    if v == "0"
                        || v.eq_ignore_ascii_case("off")
                        || v.eq_ignore_ascii_case("no") =>
                {
                    false
                }
                Some(v) => return Err(OptionError::BadValue(v.to_owned())),
            };
            Ok(Value::Flag(on))
        }
        Kind::Choice(names) => {
            // options_from_string_choice (options.c:1256): with no value, 0
            // and 1 swap and anything else stays.
            let Some(v) = value else {
                let n = match current {
                    Some(Value::Choice(n)) => *n,
                    _ => 0,
                };
                return Ok(Value::Choice(match n {
                    0 => 1,
                    1 => 0,
                    n => n,
                }));
            };
            // options_find_choice: the last exact match wins (they are
            // unique anyway).
            names
                .iter()
                .rposition(|n| *n == v)
                .map(Value::Choice)
                .ok_or_else(|| OptionError::UnknownValue(v.to_owned()))
        }
        Kind::Command => parse_command(&need(value)?, env).map(Value::Command),
    }
}

/// `options_array_set` for one item's new text. `old` is the item already
/// there, for `-a` on a string array.
pub(super) fn parse_item(
    def: &OptionDef,
    value: &str,
    old: Option<&Value>,
    append: bool,
    env: &mut dyn ConfigEnv,
) -> Result<Value, OptionError> {
    match def.kind {
        Kind::Command => parse_command(value, env).map(Value::Command),
        Kind::String(_) => Ok(Value::String(match (old, append) {
            (Some(Value::String(s)), true) => format!("{s}{value}"),
            _ => value.to_owned(),
        })),
        Kind::Colour => Colour::parse_option(value)
            .map(Value::Colour)
            .map_err(OptionError::BadColour),
        _ => Err(OptionError::Other("wrong array type")),
    }
}

/// `options_array_assign` (options.c:615): split `text` at the option's
/// separator characters (`" ,"` when it has none) and put each non-empty
/// piece at the first free index. An empty separator makes the whole text
/// one item. On an error the items before it stay.
pub(super) fn assign_array(
    def: &OptionDef,
    items: &mut BTreeMap<ArrayKey, Value>,
    text: &str,
    env: &mut dyn ConfigEnv,
) -> Result<(), OptionError> {
    let separator = def.separator.unwrap_or(" ,");
    if text.is_empty() {
        return Ok(());
    }
    let pieces: Vec<&str> = if separator.is_empty() {
        vec![text]
    } else {
        text.split(|c| separator.contains(c))
            .filter(|p| !p.is_empty())
            .collect()
    };
    for piece in pieces {
        let free = (0..=u32::MAX)
            .find(|i| !items.contains_key(&ArrayKey::Index(*i)))
            .unwrap_or(u32::MAX);
        let value = parse_item(def, piece, None, false, env)?;
        items.insert(ArrayKey::Index(free), value);
    }
    Ok(())
}

/// The value an option starts with (`options_default`, options.c:328).
/// `term` stands in for the build's `TMUX_TERM`.
pub(super) fn default_value(def: &OptionDef, term: &str) -> OptionValue {
    use super::table::DefaultValue as D;
    let mut env = crate::cmd::NoEnv;
    if def.array {
        let mut items = BTreeMap::new();
        match def.default {
            D::List(list) => {
                for (i, s) in (0u32..).zip(list) {
                    let v = parse_item(def, s, None, false, &mut env)
                        .expect("table defaults are valid");
                    items.insert(ArrayKey::Index(i), v);
                }
            }
            D::Str(s) => {
                assign_array(def, &mut items, s, &mut env).expect("table defaults are valid");
            }
            _ => {}
        }
        return OptionValue::Array(items);
    }
    OptionValue::Single(match def.default {
        D::Str(_) if def.name == "default-terminal" => Value::String(term.to_owned()),
        D::Str(s) if def.kind == Kind::Command => {
            Value::Command(parse_command(s, &mut env).expect("table defaults are valid"))
        }
        D::Str(s) => Value::String(s.to_owned()),
        D::List(_) => unreachable!("only arrays have list defaults"),
        D::Number(n) => Value::Number(n),
        D::Key(k) => Value::Key(k),
        D::Colour(c) => Value::Colour(c),
        D::Flag(b) => Value::Flag(b),
        D::Choice(n) => Value::Choice(n),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_pattern() {
        for ok in ["80x24", "1x1", "1xx2", "12ax3b", "1x2x"] {
            assert!(matches_size(ok), "{ok}");
        }
        for bad in ["x", "10x", "x10", "abc", "", "a1x1"] {
            assert!(!matches_size(bad), "{bad}");
        }
    }
}
