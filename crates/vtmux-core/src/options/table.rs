//! The options table (options-table.c `options_table[]`): every tmux 3.8
//! option, its scope, value type and default. `table.in` is generated from
//! the C source by `gen_table.sh`; this file holds the types it is written in
//! and the name lookups (`options_match`, `options_parse`).

use crate::colour::Colour;
use crate::keys::Key;

/// Which options trees an option lives in (`OPTIONS_TABLE_SERVER` ...).
/// tmux's 3.8 table uses exactly these four combinations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Server,
    Session,
    Window,
    /// A window option that a pane may override (`-p`).
    WindowPane,
}

impl Scope {
    /// Whether an option of this scope is listed by `show-options` at
    /// `level` (`~oe->scope & scope`, cmd-show-options.c:375).
    #[must_use]
    pub fn includes(self, level: Level) -> bool {
        matches!(
            (self, level),
            (Self::Server, Level::Server)
                | (Self::Session, Level::Session)
                | (Self::Window | Self::WindowPane, Level::Window)
                | (Self::WindowPane, Level::Pane)
        )
    }
}

/// One options tree's place in the hierarchy: what `-s`, `-g`, `-w` and `-p`
/// pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Server,
    Session,
    Window,
    Pane,
}

/// What `set` checks a string option against before accepting it
/// (`options_from_string_check`, options.c:1185-1211).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringCheck {
    None,
    /// `OPTIONS_TABLE_IS_STYLE`: a style, unless it contains `#{`.
    Style,
    /// `OPTIONS_TABLE_IS_COLOUR`: empty or a colour, unless it contains `#{`.
    Colour,
    /// `default-size`: `.pattern = "[0-9]*x[0-9]*"`.
    Size,
    /// `default-shell`: `checkshell` (tmux.c:98).
    Shell,
}

/// The value type of an option (`enum options_table_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    String(StringCheck),
    Number { min: u32, max: u32 },
    Key,
    Colour,
    Flag,
    Choice(&'static [&'static str]),
    Command,
}

/// A table default (`default_str`, `default_num` or `default_arr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultValue {
    /// A string, a command, or an array assigned from one string.
    Str(&'static str),
    /// An array's items, indexed from 0 (`status-format`).
    List(&'static [&'static str]),
    Number(i64),
    Key(Key),
    Colour(Colour),
    Flag(bool),
    Choice(usize),
}

/// One row of the options table.
#[derive(Debug, PartialEq, Eq)]
pub struct OptionDef {
    pub name: &'static str,
    pub scope: Scope,
    pub kind: Kind,
    /// `OPTIONS_TABLE_IS_ARRAY`.
    pub array: bool,
    /// `OPTIONS_TABLE_IS_HOOK`: hidden from `show-options` without `-H`.
    pub hook: bool,
    /// What splits an array value into items, or joins an appended string;
    /// `None` is tmux's NULL (`" ,"` for arrays, nothing for strings).
    pub separator: Option<&'static str>,
    pub default: DefaultValue,
}

impl OptionDef {
    /// `options_is_string`: shown quoted by `show-options`.
    #[must_use]
    pub fn is_string(&self) -> bool {
        matches!(self.kind, Kind::String(_))
    }
}

#[allow(clippy::unreadable_literal)]
mod generated {
    use super::{Colour, DefaultValue, Key, Kind, OptionDef, Scope, StringCheck};
    include!("table.in");
}
use generated::{ALIASES, TABLE};

/// Every table option, in table order (the order `show-options` lists them).
#[must_use]
pub fn table() -> &'static [OptionDef] {
    &TABLE
}

/// `options_search`: the table entry named exactly `name`.
#[must_use]
pub fn find(name: &str) -> Option<&'static OptionDef> {
    TABLE.iter().find(|d| d.name == name)
}

/// `options_map_name`: the 3.8 spelling of an old option name
/// (`display-panes-color` -> `display-panes-colour`).
pub(super) fn map_name(name: &str) -> &str {
    ALIASES
        .iter()
        .find(|(from, _)| *from == name)
        .map_or(name, |(_, to)| to)
}

/// An array item's key. tmux keeps keys as strings but orders and
/// normalises the all-digit ones as numbers (`options_array_cmp`,
/// `options_array_correct_key`): `007` is item 7, and every numbered item
/// sorts before every named one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArrayKey {
    Index(u32),
    Name(String),
}

impl ArrayKey {
    /// `options_array_correct_key`: `None` for an empty key or a number
    /// above `UINT_MAX`.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        if key.is_empty() {
            return None;
        }
        if key.bytes().all(|b| b.is_ascii_digit()) {
            // Leading zeros are fine; the value must fit in a u_int.
            return key.parse::<u32>().ok().map(Self::Index);
        }
        Some(Self::Name(key.to_owned()))
    }
}

impl std::fmt::Display for ArrayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Index(n) => write!(f, "{n}"),
            Self::Name(s) => f.write_str(s),
        }
    }
}

/// `options_parse`: split `name[key]`. `None` for an empty name, a `[`
/// without a final `]`, an empty key or a bad key.
pub(super) fn parse_name(s: &str) -> Option<(&str, Option<ArrayKey>)> {
    if s.is_empty() {
        return None;
    }
    let Some(open) = s.find('[') else {
        return Some((s, None));
    };
    let rest = &s[open + 1..];
    // `strchr(cp + 1, ']')`: the first `]`, which must end the string.
    let close = rest.find(']')?;
    if close + 1 != rest.len() || close == 0 {
        return None;
    }
    Some((&s[..open], Some(ArrayKey::parse(&rest[..close])?)))
}

/// What a `set-option`/`show-options` name argument names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Matched {
    Table(&'static OptionDef, Option<ArrayKey>),
    User(String, Option<ArrayKey>),
}

/// Why `options_match` found nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MatchError {
    /// No table option has this name or prefix.
    Unknown,
    Ambiguous,
    /// `options_parse` rejected the text. tmux leaves its `ambiguous`
    /// result unset on this path and its callers read it anyway: in the
    /// local reference build `set` says `ambiguous option` and `show` says
    /// `invalid option` (pinned by the `names-malformed` golden case).
    /// Other builds differ (CI's Ubuntu `show` says `ambiguous`), so the
    /// CLI cross-check treats the word as unspecified.
    Unparsable,
}

/// `options_match` (options.c:784): a user option, an exact table name, an
/// old spelling, or an unambiguous prefix of a table name. A vtmux option
/// matches by its exact name only, so tmux's prefixes and errors stay as
/// they are.
pub(super) fn match_name(s: &str) -> Result<Matched, MatchError> {
    let (name, key) = parse_name(s).ok_or(MatchError::Unparsable)?;
    if name.starts_with('@') {
        return Ok(Matched::User(name.to_owned(), key));
    }
    if let Some(def) = super::vtmux::find(name) {
        return Ok(Matched::Table(def, key));
    }
    let name = map_name(name);
    let mut found = None;
    for def in &TABLE {
        if def.name == name {
            return Ok(Matched::Table(def, key));
        }
        if def.name.starts_with(name) {
            if found.is_some() {
                return Err(MatchError::Ambiguous);
            }
            found = Some(def);
        }
    }
    found
        .map(|def| Matched::Table(def, key))
        .ok_or(MatchError::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_every_option() {
        // options-table.c: 183 entries plus 89 from the hook macros.
        assert_eq!(TABLE.len(), 272);
        assert_eq!(TABLE.iter().filter(|d| d.hook).count(), 89);
        assert!(TABLE.iter().filter(|d| d.hook).all(|d| d.array));
    }

    #[test]
    fn array_keys_order_numbers_first() {
        let mut keys = ["b", "10", "a", "2", "007"]
            .iter()
            .map(|k| ArrayKey::parse(k).unwrap())
            .collect::<Vec<_>>();
        keys.sort();
        let shown: Vec<String> = keys.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["2", "7", "10", "a", "b"]);
        assert_eq!(ArrayKey::parse(""), None);
        assert_eq!(
            ArrayKey::parse("4294967295"),
            Some(ArrayKey::Index(u32::MAX))
        );
        assert_eq!(ArrayKey::parse("4294967296"), None);
        assert_eq!(
            ArrayKey::parse("0004294967295"),
            Some(ArrayKey::Index(u32::MAX))
        );
    }

    #[test]
    fn names_match_by_prefix_and_alias() {
        assert!(
            matches!(match_name("status-left-l"), Ok(Matched::Table(d, None)) if d.name == "status-left-length")
        );
        assert!(
            matches!(match_name("pane-colors[3]"), Ok(Matched::Table(d, Some(ArrayKey::Index(3)))) if d.name == "pane-colours")
        );
        assert_eq!(match_name("s"), Err(MatchError::Ambiguous));
        assert_eq!(match_name("nope"), Err(MatchError::Unknown));
        assert_eq!(match_name("x["), Err(MatchError::Unparsable));
        assert_eq!(match_name("a[]"), Err(MatchError::Unparsable));
        assert_eq!(match_name("a[1]b"), Err(MatchError::Unparsable));
        assert_eq!(match_name(""), Err(MatchError::Unparsable));
    }

    /// vtmux options are exact names, outside tmux's prefix space.
    #[test]
    fn vtmux_options_match_exactly() {
        assert!(
            matches!(match_name("vtmux-focus-ring"), Ok(Matched::Table(d, None)) if d.name == "vtmux-focus-ring")
        );
        for prefix in ["vt", "vtm", "vtmux", "vtmux-focus"] {
            assert_eq!(match_name(prefix), Err(MatchError::Unknown), "{prefix}");
        }
        assert!(find("vtmux-focus-ring").is_none(), "not in tmux's table");
    }
}
