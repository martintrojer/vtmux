//! Typed styles: the `#[...]` / `status-style` grammar of tmux 3.8 (`style.c`).
//!
//! [`Style::parse`] is `style_parse` (style.c:96-283), token by token and in
//! tmux's order of tests, so a token that several branches could claim goes to
//! the same one. Its quirks are kept and pinned by `tests/style_cases.sh`
//! (`set -g status-style '<s>'` accepts or rejects):
//!
//! - an unknown `range=` type is silently accepted (no branch errors, style.c:157-217);
//! - `fg=` alone is not a colour setting, it falls through to the attribute
//!   parser and fails; `?g=` accepts any first letter but only `f` and `b` pass;
//! - a failed parse leaves the style untouched (`style_copy(sy, &saved)`).
//!
//! Style options are stored as typed by the user and only checked at `set`
//! time, and not at all when the value contains `#{` (options.c:1199-1203);
//! [`validate_style_option`] and [`validate_colour_string_option`] are those
//! checks. `style_tostring` is not a public format: [`Style`]'s `Display` is for
//! logs and tests only. tmux's version also has no case for `range=control`, so
//! it prints the previous label there; this one prints `control|N`.

use std::fmt;

use crate::colour::{Colour, strtonum};

/// Cell attributes (`GRID_ATTR_*` in tmux). A set; the bit values are private.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs(u16);

impl Attrs {
    pub const NONE: Self = Self(0);
    /// `acs`: the alternate character set.
    pub const ACS: Self = Self(1 << 0);
    /// `bright` / `bold`.
    pub const BRIGHT: Self = Self(1 << 1);
    pub const DIM: Self = Self(1 << 2);
    pub const UNDERSCORE: Self = Self(1 << 3);
    pub const BLINK: Self = Self(1 << 4);
    pub const REVERSE: Self = Self(1 << 5);
    pub const HIDDEN: Self = Self(1 << 6);
    pub const ITALICS: Self = Self(1 << 7);
    pub const STRIKETHROUGH: Self = Self(1 << 8);
    pub const DOUBLE_UNDERSCORE: Self = Self(1 << 9);
    pub const CURLY_UNDERSCORE: Self = Self(1 << 10);
    pub const DOTTED_UNDERSCORE: Self = Self(1 << 11);
    pub const DASHED_UNDERSCORE: Self = Self(1 << 12);
    pub const OVERLINE: Self = Self(1 << 13);
    /// `noattr`: only meaningful to selections; drawing ignores it.
    pub const NOATTR: Self = Self(1 << 14);

    /// The names `attributes_tostring` prints, in its order (attributes.c:33-48).
    const PRINT_ORDER: [(Self, &'static str); 15] = [
        (Self::ACS, "acs"),
        (Self::BRIGHT, "bright"),
        (Self::DIM, "dim"),
        (Self::UNDERSCORE, "underscore"),
        (Self::BLINK, "blink"),
        (Self::REVERSE, "reverse"),
        (Self::HIDDEN, "hidden"),
        (Self::ITALICS, "italics"),
        (Self::STRIKETHROUGH, "strikethrough"),
        (Self::DOUBLE_UNDERSCORE, "double-underscore"),
        (Self::CURLY_UNDERSCORE, "curly-underscore"),
        (Self::DOTTED_UNDERSCORE, "dotted-underscore"),
        (Self::DASHED_UNDERSCORE, "dashed-underscore"),
        (Self::OVERLINE, "overline"),
        (Self::NOATTR, "noattr"),
    ];

    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// `attributes_fromstring` (attributes.c:54-104): names separated by
    /// ` `, `,` or `|`, case-insensitive. `default` and `none` alone are the
    /// empty set. `None` is tmux's `-1`: an unknown name, an empty string, or a
    /// separator at either end.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        const SEPARATORS: [char; 3] = [' ', ',', '|'];
        if s.is_empty() || s.starts_with(SEPARATORS) || s.ends_with(SEPARATORS) {
            return None;
        }
        if s.eq_ignore_ascii_case("default") || s.eq_ignore_ascii_case("none") {
            return Some(Self::NONE);
        }
        let mut attrs = Self::NONE;
        for name in s.split(SEPARATORS).filter(|n| !n.is_empty()) {
            attrs = attrs.union(match name.to_ascii_lowercase().as_str() {
                "acs" => Self::ACS,
                "bright" | "bold" => Self::BRIGHT,
                "dim" => Self::DIM,
                "underscore" => Self::UNDERSCORE,
                "blink" => Self::BLINK,
                "reverse" => Self::REVERSE,
                "hidden" => Self::HIDDEN,
                "italics" => Self::ITALICS,
                "strikethrough" => Self::STRIKETHROUGH,
                "double-underscore" => Self::DOUBLE_UNDERSCORE,
                "curly-underscore" => Self::CURLY_UNDERSCORE,
                "dotted-underscore" => Self::DOTTED_UNDERSCORE,
                "dashed-underscore" => Self::DASHED_UNDERSCORE,
                "overline" => Self::OVERLINE,
                _ => return None,
            });
        }
        Some(attrs)
    }
}

impl fmt::Display for Attrs {
    /// `attributes_tostring`: `none` or the comma-joined names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let mut sep = "";
        for (attr, name) in Self::PRINT_ORDER {
            if self.contains(attr) {
                write!(f, "{sep}{name}")?;
                sep = ",";
            }
        }
        Ok(())
    }
}

/// What a drawn cell looks like: the part of tmux's `grid_cell` a style sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Colour,
    pub bg: Colour,
    /// Underline colour.
    pub us: Colour,
    pub attrs: Attrs,
    /// `link=URI`: the hyperlink the cell carries.
    pub link: Option<String>,
}

/// `align=` (style.c:196-209).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// Not set: the status line's left part.
    #[default]
    Default,
    Left,
    Centre,
    Right,
    AbsoluteCentre,
}

/// `list=` (style.c:130-145).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum List {
    #[default]
    Off,
    On,
    Focus,
    LeftMarker,
    RightMarker,
}

/// `range=`: what a clickable stretch of the line stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeKind {
    Left,
    Right,
    Pane(u32),
    Window(u32),
    Session(u32),
    /// `range=user|NAME`; tmux keeps 15 bytes of the name.
    User(String),
    /// `range=control|N`, N 0-9.
    Control(u8),
}

/// `width=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    Cells(u32),
    /// 0-100.
    Percent(u8),
}

/// `push-default`, `pop-default`, `set-default`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DefaultAction {
    #[default]
    None,
    Push,
    Pop,
    Set,
}

/// A parsed style: the cell look plus layout directives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub cell: CellStyle,
    /// `ignore`: the rest of the text is literal, `#[` included.
    pub ignore: bool,
    /// `dim=N`, 0-100.
    pub dim: u8,
    /// `fill=`; [`Colour::Default`] is unset.
    pub fill: Colour,
    pub align: Align,
    pub list: List,
    pub range: Option<RangeKind>,
    /// `None` when unset. tmux stores an `int`, so a value above `i32::MAX`
    /// reads back as unset.
    pub width: Option<Width>,
    /// As `width`.
    pub pad: Option<u32>,
    pub default_action: DefaultAction,
}

/// `style_parse` found a token it does not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleSyntax;

/// `invalid style: X` (options.c:1202).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidStyle(pub String);

impl fmt::Display for InvalidStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid style: {}", self.0)
    }
}

impl std::error::Error for InvalidStyle {}

/// `invalid colour: X` (options.c:1207).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidColour(pub String);

impl fmt::Display for InvalidColour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid colour: {}", self.0)
    }
}

impl std::error::Error for InvalidColour {}

const DELIMITERS: [char; 3] = [' ', ',', '\n'];
/// `char tmp[256]` in `style_parse`: a token may hold 255 bytes.
const MAX_TOKEN: usize = 255;
/// `char range_string[16]`.
const RANGE_STRING_MAX: usize = 15;

/// `strncasecmp(tok, prefix, len) == 0`, returning what follows the prefix.
fn strip_prefix_ci<'a>(tok: &'a str, prefix: &str) -> Option<&'a str> {
    let head = tok.as_bytes().get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix.as_bytes())
        .then(|| &tok[prefix.len()..])
}

fn u32_in(s: &str, max: u32) -> Option<u32> {
    u32::try_from(strtonum(s, 0, i64::from(max))?).ok()
}

/// A `strtonum(.., 0, UINT_MAX)` stored in an `int`.
fn as_c_int(n: u32) -> Option<u32> {
    i32::try_from(n).ok().and_then(|v| u32::try_from(v).ok())
}

impl Style {
    /// `style_set`: the default style over a base cell.
    #[must_use]
    pub fn new(base: &CellStyle) -> Self {
        Self {
            cell: base.clone(),
            ..Self::default()
        }
    }

    /// Parse `input` onto this style (`style_parse`). `base` is what `default`
    /// and an explicit `fg=default` fall back to. On error the style is left
    /// as it was.
    pub fn parse(&mut self, base: &CellStyle, input: &str) -> Result<(), StyleSyntax> {
        let mut next = self.clone();
        for tok in input.split(DELIMITERS).filter(|t| !t.is_empty()) {
            if tok.len() > MAX_TOKEN {
                return Err(StyleSyntax);
            }
            next.apply(base, tok)?;
        }
        *self = next;
        Ok(())
    }

    /// One token, in the order `style_parse` tests them.
    fn apply(&mut self, base: &CellStyle, tok: &str) -> Result<(), StyleSyntax> {
        let end = tok.len();
        let bytes = tok.as_bytes();
        let is = |name: &str| tok.eq_ignore_ascii_case(name);

        if is("default") {
            self.cell.fg = base.fg;
            self.cell.bg = base.bg;
            self.cell.us = base.us;
            self.cell.attrs = base.attrs;
            self.cell.link = None;
        } else if is("ignore") {
            self.ignore = true;
        } else if is("noignore") {
            self.ignore = false;
        } else if is("push-default") {
            self.default_action = DefaultAction::Push;
        } else if is("pop-default") {
            self.default_action = DefaultAction::Pop;
        } else if is("set-default") {
            self.default_action = DefaultAction::Set;
        } else if is("nolist") {
            self.list = List::Off;
        } else if let Some(v) = strip_prefix_ci(tok, "list=") {
            self.list = match v.to_ascii_lowercase().as_str() {
                "on" => List::On,
                "focus" => List::Focus,
                "left-marker" => List::LeftMarker,
                "right-marker" => List::RightMarker,
                _ => return Err(StyleSyntax),
            };
        } else if is("norange") {
            self.range = None;
        } else if end > 6 && strip_prefix_ci(tok, "range=").is_some() {
            self.apply_range(&tok[6..])?;
        } else if is("noalign") {
            self.align = Align::Default;
        } else if end > 6 && strip_prefix_ci(tok, "align=").is_some() {
            self.align = match tok[6..].to_ascii_lowercase().as_str() {
                "left" => Align::Left,
                "centre" => Align::Centre,
                "right" => Align::Right,
                "absolute-centre" => Align::AbsoluteCentre,
                _ => return Err(StyleSyntax),
            };
        } else if end > 5 && strip_prefix_ci(tok, "fill=").is_some() {
            self.fill = Colour::parse(&tok[5..]).ok_or(StyleSyntax)?;
        } else if end > 4 && strip_prefix_ci(tok, "dim=").is_some() {
            let v = tok[4..].strip_suffix('%').unwrap_or(&tok[4..]);
            self.dim = u8::try_from(u32_in(v, 100).ok_or(StyleSyntax)?).map_err(|_| StyleSyntax)?;
        } else if end > 3 && bytes[1..3].eq_ignore_ascii_case(b"g=") {
            // Any first letter gets here; the colour is checked before it.
            let colour = Colour::parse(&tok[3..]).ok_or(StyleSyntax)?;
            let slot = match bytes[0] {
                b'f' | b'F' => &mut self.cell.fg,
                b'b' | b'B' => &mut self.cell.bg,
                _ => return Err(StyleSyntax),
            };
            *slot = if colour == Colour::Default {
                if bytes[0].eq_ignore_ascii_case(&b'f') {
                    base.fg
                } else {
                    base.bg
                }
            } else {
                colour
            };
        } else if end > 3 && strip_prefix_ci(tok, "us=").is_some() {
            let colour = Colour::parse(&tok[3..]).ok_or(StyleSyntax)?;
            self.cell.us = if colour == Colour::Default {
                base.us
            } else {
                colour
            };
        } else if is("none") {
            self.cell.attrs = Attrs::NONE;
        } else if end > 2 && strip_prefix_ci(tok, "no").is_some() {
            // `nolink` and `noattr` are compared case-sensitively (strcmp).
            match &tok[2..] {
                "link" => self.cell.link = None,
                "attr" => self.cell.attrs = self.cell.attrs.union(Attrs::NOATTR),
                rest => {
                    let attrs = Attrs::parse(rest).ok_or(StyleSyntax)?;
                    self.cell.attrs = self.cell.attrs.without(attrs);
                }
            }
        } else if end > 6 && strip_prefix_ci(tok, "width=").is_some() {
            let v = &tok[6..];
            self.width = if end > 7 && v.ends_with('%') {
                let pct = u32_in(&v[..v.len() - 1], 100).ok_or(StyleSyntax)?;
                Some(Width::Percent(u8::try_from(pct).map_err(|_| StyleSyntax)?))
            } else {
                as_c_int(u32_in(v, u32::MAX).ok_or(StyleSyntax)?).map(Width::Cells)
            };
        } else if end > 4 && strip_prefix_ci(tok, "pad=").is_some() {
            self.pad = as_c_int(u32_in(&tok[4..], u32::MAX).ok_or(StyleSyntax)?);
        } else if let Some(uri) = strip_prefix_ci(tok, "link=") {
            self.cell.link = (!uri.is_empty()).then(|| uri.to_owned());
        } else {
            let attrs = Attrs::parse(tok).ok_or(StyleSyntax)?;
            self.cell.attrs = self.cell.attrs.union(attrs);
        }
        Ok(())
    }

    /// `range=TYPE[|ARG]` (style.c:157-217); `spec` is what follows `range=`.
    fn apply_range(&mut self, spec: &str) -> Result<(), StyleSyntax> {
        let (kind, arg) = match spec.split_once('|') {
            Some((_, "")) => return Err(StyleSyntax),
            Some((k, a)) => (k, Some(a)),
            None => (spec, None),
        };
        let kind = kind.to_ascii_lowercase();
        let range = match (kind.as_str(), arg) {
            ("left", None) => Some(RangeKind::Left),
            ("right", None) => Some(RangeKind::Right),
            ("control", Some(a)) => {
                let n = u32_in(a, 9).ok_or(StyleSyntax)?;
                Some(RangeKind::Control(
                    u8::try_from(n).map_err(|_| StyleSyntax)?,
                ))
            }
            ("pane", Some(a)) => {
                let digits = a
                    .strip_prefix('%')
                    .filter(|d| !d.is_empty())
                    .ok_or(StyleSyntax)?;
                Some(RangeKind::Pane(
                    u32_in(digits, u32::MAX).ok_or(StyleSyntax)?,
                ))
            }
            ("window", Some(a)) => Some(RangeKind::Window(u32_in(a, u32::MAX).ok_or(StyleSyntax)?)),
            ("session", Some(a)) => {
                let digits = a
                    .strip_prefix('$')
                    .filter(|d| !d.is_empty())
                    .ok_or(StyleSyntax)?;
                Some(RangeKind::Session(
                    u32_in(digits, u32::MAX).ok_or(StyleSyntax)?,
                ))
            }
            // A label on `left`/`right`, or none on the types that need one.
            ("left" | "right", Some(_))
            | ("control" | "pane" | "window" | "session" | "user", None) => {
                return Err(StyleSyntax);
            }
            ("user", Some(a)) => Some(RangeKind::User(truncate_bytes(a, RANGE_STRING_MAX))),
            // An unknown type matches no branch and is not an error.
            _ => return Ok(()),
        };
        self.range = range;
        Ok(())
    }
}

/// `strlcpy` into a buffer of `max + 1` bytes, without cutting a character.
fn truncate_bytes(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

impl fmt::Display for Style {
    /// For logs and tests: `style_tostring`'s shape (style.c:319-415).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        match self.list {
            List::Off => {}
            List::On => parts.push("list=on".into()),
            List::Focus => parts.push("list=focus".into()),
            List::LeftMarker => parts.push("list=left-marker".into()),
            List::RightMarker => parts.push("list=right-marker".into()),
        }
        if let Some(r) = &self.range {
            parts.push(match r {
                RangeKind::Left => "range=left".into(),
                RangeKind::Right => "range=right".into(),
                RangeKind::Pane(n) => format!("range=pane|%{n}"),
                RangeKind::Window(n) => format!("range=window|{n}"),
                RangeKind::Session(n) => format!("range=session|${n}"),
                RangeKind::User(s) => format!("range=user|{s}"),
                RangeKind::Control(n) => format!("range=control|{n}"),
            });
        }
        match self.align {
            Align::Default => {}
            Align::Left => parts.push("align=left".into()),
            Align::Centre => parts.push("align=centre".into()),
            Align::Right => parts.push("align=right".into()),
            Align::AbsoluteCentre => parts.push("align=absolute-centre".into()),
        }
        match self.default_action {
            DefaultAction::None => {}
            DefaultAction::Push => parts.push("push-default".into()),
            DefaultAction::Pop => parts.push("pop-default".into()),
            DefaultAction::Set => parts.push("set-default".into()),
        }
        if self.fill != Colour::Default {
            parts.push(format!("fill={}", self.fill));
        }
        if self.dim != 0 {
            parts.push(format!("dim={}%", self.dim));
        }
        if self.cell.fg != Colour::Default {
            parts.push(format!("fg={}", self.cell.fg));
        }
        if self.cell.bg != Colour::Default {
            parts.push(format!("bg={}", self.cell.bg));
        }
        if self.cell.us != Colour::Default {
            parts.push(format!("us={}", self.cell.us));
        }
        if !self.cell.attrs.is_empty() {
            parts.push(self.cell.attrs.to_string());
        }
        match self.width {
            Some(Width::Cells(n)) => parts.push(format!("width={n}")),
            Some(Width::Percent(n)) => parts.push(format!("width={n}%")),
            None => {}
        }
        if let Some(n) = self.pad {
            parts.push(format!("pad={n}"));
        }
        if let Some(uri) = &self.cell.link {
            parts.push(format!("link={uri}"));
        }
        if parts.is_empty() {
            f.write_str("default")
        } else {
            f.write_str(&parts.join(","))
        }
    }
}

/// The check `set` applies to a style option (options.c:1199-1203): `value`
/// must parse over the default cell, unless it contains `#{`, which is only
/// expanded (and checked) when used.
pub fn validate_style_option(value: &str) -> Result<(), InvalidStyle> {
    if value.contains("#{") {
        return Ok(());
    }
    Style::default()
        .parse(&CellStyle::default(), value)
        .map_err(|StyleSyntax| InvalidStyle(value.to_owned()))
}

/// The check `set` applies to a colour kept as a string, such as
/// `display-panes-colour` (options.c:1204-1209, `style_parse_colour`
/// style.c:477-497): empty is fine, otherwise a colour name, unless the value
/// contains `#{`.
pub fn validate_colour_string_option(value: &str) -> Result<(), InvalidColour> {
    if value.contains("#{") || value.is_empty() || Colour::parse(value).is_some() {
        return Ok(());
    }
    Err(InvalidColour(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // BEGIN GENERATED style_cases
    // set -g status-style 'S': accepted?, then for accepted S the style_tostring of what
    // style_parse built, from a -vv server drawing '#[S]x' (None: not compared)
    const STYLE_CASES: &[(&str, bool, Option<&str>)] = &[
        ("", true, None),
        ("default", true, Some("default")),
        ("DEFAULT", true, Some("default")),
        ("none", true, Some("default")),
        ("NONE", true, Some("default")),
        (" , ,\u{a}", true, None),
        ("fg=red", true, Some("fg=red")),
        ("FG=RED", true, Some("fg=red")),
        ("Bg=Blue", true, Some("bg=blue")),
        ("bg=#102030", true, Some("bg=#102030")),
        (
            "fg=colour196,bg=colour17",
            true,
            Some("fg=colour196,bg=colour17"),
        ),
        ("fg=default", true, Some("default")),
        ("bg=default", true, Some("default")),
        ("fg=terminal", true, Some("fg=terminal")),
        ("fg=themegreen", true, Some("fg=themegreen")),
        ("fg=nope", false, None),
        ("bg=", false, None),
        ("fg=", false, None),
        ("xg=red", false, None),
        ("fg=#12345", false, None),
        ("fg=red,bg=blue,bold", true, Some("fg=red,bg=blue,bright")),
        (
            "fg=red bg=blue italics",
            true,
            Some("fg=red,bg=blue,italics"),
        ),
        ("fg=red\u{a}bg=blue", true, None),
        ("fg=red,,bg=blue", true, Some("fg=red,bg=blue")),
        ("  fg=red  ", true, Some("fg=red")),
        ("us=red", true, Some("us=red")),
        ("us=#00ff00", true, Some("us=#00ff00")),
        ("us=default", true, Some("default")),
        ("us=", false, None),
        ("us=nope", false, None),
        ("bold", true, Some("bright")),
        ("BOLD", true, Some("bright")),
        ("bright", true, Some("bright")),
        ("dim", true, Some("dim")),
        ("underscore", true, Some("underscore")),
        ("blink", true, Some("blink")),
        ("reverse", true, Some("reverse")),
        ("hidden", true, Some("hidden")),
        ("italics", true, Some("italics")),
        ("strikethrough", true, Some("strikethrough")),
        ("double-underscore", true, Some("double-underscore")),
        ("curly-underscore", true, Some("curly-underscore")),
        ("dotted-underscore", true, Some("dotted-underscore")),
        ("dashed-underscore", true, Some("dashed-underscore")),
        ("overline", true, Some("overline")),
        ("acs", true, Some("acs")),
        ("bold,italics", true, Some("bright,italics")),
        ("bold|italics", true, Some("bright,italics")),
        ("bold italics reverse", true, Some("bright,reverse,italics")),
        ("bold,", true, Some("bright")),
        ("underscore|reverse", true, Some("underscore,reverse")),
        ("bold,nope", false, None),
        ("nope", false, None),
        ("noattr", true, Some("noattr")),
        ("NOATTR", false, None),
        ("nobold", true, Some("default")),
        ("nobright", true, Some("default")),
        ("nodim", true, Some("default")),
        ("noreverse", true, Some("default")),
        ("NOBOLD", true, Some("default")),
        ("nodouble-underscore", true, Some("default")),
        ("nonope", false, None),
        ("no", false, None),
        ("nonone", true, Some("default")),
        ("nodefault", true, Some("default")),
        ("bold,nobold", true, Some("default")),
        ("nobold,bold", true, Some("bright")),
        ("nolink", true, Some("default")),
        (
            "link=http://example.com",
            true,
            Some("link=http://example.com"),
        ),
        ("link=", true, Some("default")),
        ("link=a b", false, None),
        ("NOLINK", false, None),
        ("LINK=x", true, Some("link=x")),
        (
            "link=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            true,
            Some(
                "link=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        ),
        (
            "link=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            false,
            None,
        ),
        ("list=on", true, Some("list=on")),
        ("list=focus", true, Some("list=focus")),
        ("list=left-marker", true, Some("list=left-marker")),
        ("list=right-marker", true, Some("list=right-marker")),
        ("LIST=ON", true, Some("list=on")),
        ("list=", false, None),
        ("list=off", false, None),
        ("list=nope", false, None),
        ("nolist", true, Some("default")),
        ("NOLIST", true, Some("default")),
        ("align=left", true, Some("align=left")),
        ("align=centre", true, Some("align=centre")),
        ("align=right", true, Some("align=right")),
        ("align=absolute-centre", true, Some("align=absolute-centre")),
        ("ALIGN=CENTRE", true, Some("align=centre")),
        ("align=center", false, None),
        ("align=", false, None),
        ("align=nope", false, None),
        ("noalign", true, Some("default")),
        ("range=left", true, Some("range=left")),
        ("range=right", true, Some("range=right")),
        ("RANGE=LEFT", true, Some("range=left")),
        ("range=left|1", false, None),
        ("range=right|x", false, None),
        ("range=pane|%3", true, Some("range=pane|%3")),
        ("range=pane|%0", true, Some("range=pane|%0")),
        ("range=pane|3", false, None),
        ("range=pane|%", false, None),
        ("range=pane|%x", false, None),
        (
            "range=pane|%4294967295",
            true,
            Some("range=pane|%4294967295"),
        ),
        ("range=pane|%4294967296", false, None),
        ("range=pane", false, None),
        ("range=pane|", false, None),
        ("range=window|3", true, Some("range=window|3")),
        ("range=window|0", true, Some("range=window|0")),
        ("range=window|x", false, None),
        ("range=window|-1", false, None),
        (
            "range=window|4294967295",
            true,
            Some("range=window|4294967295"),
        ),
        ("range=window|4294967296", false, None),
        ("range=window", false, None),
        ("range=session|$2", true, Some("range=session|$2")),
        ("range=session|2", false, None),
        ("range=session|$", false, None),
        ("range=session|$x", false, None),
        ("range=session", false, None),
        ("range=control|0", true, None),
        ("range=control|9", true, None),
        ("range=control|10", false, None),
        ("range=control|x", false, None),
        ("range=control", false, None),
        ("range=user|abc", true, Some("range=user|abc")),
        ("range=user|a|b", true, Some("range=user|a|b")),
        (
            "range=user|0123456789abcdef",
            true,
            Some("range=user|0123456789abcde"),
        ),
        (
            "range=user|0123456789abcde",
            true,
            Some("range=user|0123456789abcde"),
        ),
        ("range=user", false, None),
        ("range=user|", false, None),
        ("range=foo", true, Some("default")),
        ("range=foo|1", true, Some("default")),
        ("range=", false, None),
        ("range=|", false, None),
        ("norange", true, Some("default")),
        ("NORANGE", true, Some("default")),
        ("fill=red", true, Some("fill=red")),
        ("fill=#abcdef", true, Some("fill=#abcdef")),
        ("fill=default", true, Some("default")),
        ("fill=nope", false, None),
        ("fill=", false, None),
        ("dim=0", true, Some("default")),
        ("dim=50", true, Some("dim=50%")),
        ("dim=50%", true, Some("dim=50%")),
        ("dim=100", true, Some("dim=100%")),
        ("dim=101", false, None),
        ("dim=-1", false, None),
        ("dim=x", false, None),
        ("dim=", false, None),
        ("dim=%", false, None),
        ("DIM=10", true, Some("dim=10%")),
        ("width=10", true, Some("width=10")),
        ("width=0", true, Some("width=0")),
        ("width=10%", true, Some("width=10%")),
        ("width=100%", true, Some("width=100%")),
        ("width=101%", false, None),
        ("width=%", false, None),
        ("width=x", false, None),
        ("width=", false, None),
        ("width=2147483647", true, Some("width=2147483647")),
        ("width=2147483648", true, Some("default")),
        ("width=4294967295", true, Some("default")),
        ("width=4294967296", false, None),
        ("width=-1", false, None),
        ("pad=2", true, Some("pad=2")),
        ("pad=0", true, Some("pad=0")),
        ("pad=x", false, None),
        ("pad=", false, None),
        ("pad=2147483648", true, Some("default")),
        ("pad=-1", false, None),
        ("push-default", true, Some("push-default")),
        ("pop-default", true, Some("pop-default")),
        ("set-default", true, Some("set-default")),
        ("PUSH-DEFAULT", true, Some("push-default")),
        ("ignore", true, Some("default")),
        ("noignore", true, Some("default")),
        ("IGNORE", true, Some("default")),
        (
            "fg=red,align=right,range=window|2,list=on,bold,width=5,pad=1,dim=20,us=blue,fill=black,link=u",
            true,
            Some(
                "list=on,range=window|2,align=right,fill=black,dim=20%,fg=red,us=blue,bright,width=5,pad=1,link=u",
            ),
        ),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            false,
            None,
        ),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            false,
            None,
        ),
        (
            "fg=rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr",
            false,
            None,
        ),
        (
            "fg=rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr",
            false,
            None,
        ),
    ];
    // set -g display-panes-colour X: None is accepted, Some is the error text
    const COLOUR_STRING_CASES: &[(&str, Option<&str>)] = &[
        ("default", None),
        ("Default", None),
        ("terminal", None),
        ("TERMINAL", None),
        ("black", None),
        ("Red", None),
        ("GREEN", None),
        ("yellow", None),
        ("Blue", None),
        ("magenta", None),
        ("cyan", None),
        ("white", None),
        ("brightblack", None),
        ("brightRED", None),
        ("brightgreen", None),
        ("brightyellow", None),
        ("brightblue", None),
        ("brightmagenta", None),
        ("brightcyan", None),
        ("brightwhite", None),
        ("90", None),
        ("91", None),
        ("92", None),
        ("93", None),
        ("94", None),
        ("95", None),
        ("96", None),
        ("97", None),
        ("0", None),
        ("1", None),
        ("2", None),
        ("3", None),
        ("4", None),
        ("5", None),
        ("6", None),
        ("7", None),
        ("8", Some("invalid colour: 8")),
        ("9", Some("invalid colour: 9")),
        ("10", Some("invalid colour: 10")),
        ("15", Some("invalid colour: 15")),
        ("16", Some("invalid colour: 16")),
        ("89", Some("invalid colour: 89")),
        ("98", Some("invalid colour: 98")),
        ("255", Some("invalid colour: 255")),
        ("256", Some("invalid colour: 256")),
        ("-1", Some("invalid colour: -1")),
        ("00", Some("invalid colour: 00")),
        ("01", Some("invalid colour: 01")),
        ("091", Some("invalid colour: 091")),
        ("0x1", Some("invalid colour: 0x1")),
        (" 1", Some("invalid colour:  1")),
        ("1 ", Some("invalid colour: 1 ")),
        ("colour0", None),
        ("colour5", None),
        ("COLOUR5", None),
        ("Colour255", None),
        ("color17", None),
        ("COLOR5", None),
        ("colour256", Some("invalid colour: colour256")),
        ("colour", Some("invalid colour: colour")),
        ("color", Some("invalid colour: color")),
        ("colour-0", None),
        ("colour+5", None),
        ("colour 5", None),
        ("colour5x", Some("invalid colour: colour5x")),
        ("colour05", None),
        ("colour1000", Some("invalid colour: colour1000")),
        ("#FFaa00", None),
        ("#ffaa00", None),
        ("#000000", None),
        ("#fff", Some("invalid colour: #fff")),
        ("#12345", Some("invalid colour: #12345")),
        ("#1234567", Some("invalid colour: #1234567")),
        ("#ggg000", Some("invalid colour: #ggg000")),
        ("#12345g", Some("invalid colour: #12345g")),
        ("# 12345", Some("invalid colour: # 12345")),
        ("grey", None),
        ("gray", None),
        ("GREY", None),
        ("grey0", None),
        ("grey50", None),
        ("gray100", None),
        ("grey33", None),
        ("grey 5", None),
        ("grey-1", Some("invalid colour: grey-1")),
        ("grey101", Some("invalid colour: grey101")),
        ("greyx", Some("invalid colour: greyx")),
        ("grey1.5", Some("invalid colour: grey1.5")),
        ("grey05", None),
        ("themeblack", None),
        ("themewhite", None),
        ("themelightgrey", None),
        ("themedarkgrey", None),
        ("themegreen", None),
        ("themeyellow", None),
        ("themered", None),
        ("themeblue", None),
        ("themecyan", None),
        ("thememagenta", None),
        ("THEMEGREEN", None),
        ("theme", Some("invalid colour: theme")),
        ("themepink", Some("invalid colour: themepink")),
        ("alice blue", None),
        ("AliceBlue", None),
        ("aliceblue", None),
        ("ALICEBLUE", None),
        ("antique white", None),
        ("DarkOliveGreen2", None),
        ("dark olive green", None),
        ("medium spring green", None),
        ("YellowGreen", None),
        ("light goldenrod yellow", None),
        ("navy blue", None),
        ("NavyBlue", None),
        ("violet red", None),
        ("WhiteSmoke", None),
        ("white smoke", None),
        ("Snow4", None),
        ("snow 4", Some("invalid colour: snow 4")),
        ("", None),
        (" ", Some("invalid colour:  ")),
        ("red ", Some("invalid colour: red ")),
        (" red", Some("invalid colour:  red")),
        ("nope", Some("invalid colour: nope")),
        ("red,blue", Some("invalid colour: red,blue")),
        ("none", Some("invalid colour: none")),
        ("-", Some("invalid colour: -")),
        ("colour red", Some("invalid colour: colour red")),
        ("#{x}", None),
        ("red#{x}", None),
    ];
    // END GENERATED style_cases

    #[test]
    fn style_options_accept_and_reject_as_the_reference() {
        for &(s, ok, _) in STYLE_CASES {
            let got = validate_style_option(s);
            assert_eq!(got.is_ok(), ok, "style {s:?}");
            if let Err(e) = got {
                assert_eq!(e.to_string(), format!("invalid style: {s}"));
            }
        }
    }

    #[test]
    fn parsed_style_prints_as_the_reference_built_it() {
        // The generator ran its -vv server with `status-style default`, so the
        // logged style holds only what the string itself set.
        let mut checked = 0;
        for &(s, ok, shown) in STYLE_CASES {
            let Some(want) = shown else { continue };
            assert!(ok);
            let mut st = Style::default();
            st.parse(&CellStyle::default(), s).unwrap();
            assert_eq!(st.to_string(), want, "style {s:?}");
            checked += 1;
        }
        assert!(checked > 100, "{checked}");
    }

    #[test]
    fn colour_string_options_match_reference() {
        for &(c, err) in COLOUR_STRING_CASES {
            match (validate_colour_string_option(c), err) {
                (Ok(()), None) => {}
                (Err(e), Some(text)) => assert_eq!(e.to_string(), text, "colour {c:?}"),
                (got, want) => panic!("colour {c:?}: got {got:?}, reference {want:?}"),
            }
        }
    }

    #[test]
    fn options_with_formats_are_not_checked() {
        assert!(validate_style_option("fg=nope#{x}").is_ok());
        assert!(validate_colour_string_option("nope#{x}").is_ok());
    }

    #[test]
    fn failed_parse_leaves_the_style_alone() {
        let mut st = Style::default();
        st.parse(&CellStyle::default(), "fg=red,bold").unwrap();
        let before = st.clone();
        assert!(st.parse(&CellStyle::default(), "bg=blue,bogus").is_err());
        assert_eq!(st, before);
    }

    #[test]
    fn default_and_explicit_default_return_to_the_base() {
        let base = CellStyle {
            fg: Colour::Ansi(2),
            bg: Colour::Palette(4),
            us: Colour::Rgb(1, 2, 3),
            attrs: Attrs::UNDERSCORE,
            link: None,
        };
        let mut st = Style::new(&base);
        st.parse(&base, "fg=red,bg=blue,us=red,bold,nounderscore")
            .unwrap();
        assert_eq!(st.cell.fg, Colour::Ansi(1));
        st.parse(&base, "fg=default,bg=default,us=default").unwrap();
        assert_eq!(
            (st.cell.fg, st.cell.bg, st.cell.us),
            (base.fg, base.bg, base.us)
        );
        st.parse(&base, "default").unwrap();
        assert_eq!(st.cell, base);
    }

    #[test]
    fn attrs_names_and_separators() {
        assert_eq!(
            Attrs::parse("bold|italics").unwrap().to_string(),
            "bright,italics"
        );
        assert_eq!(Attrs::parse("none"), Some(Attrs::NONE));
        assert_eq!(Attrs::NONE.to_string(), "none");
        assert!(Attrs::parse("bold,").is_none());
        assert!(Attrs::parse(",bold").is_none());
        assert!(Attrs::parse("").is_none());
    }

    #[test]
    fn range_user_label_keeps_fifteen_bytes() {
        let mut st = Style::default();
        st.parse(&CellStyle::default(), "range=user|0123456789abcdef")
            .unwrap();
        assert_eq!(
            st.range,
            Some(RangeKind::User("0123456789abcde".to_owned()))
        );
    }

    #[test]
    fn strtonum_is_the_c_one() {
        assert_eq!(strtonum("42", 0, 100), Some(42));
        assert_eq!(strtonum(" 42", 0, 100), Some(42));
        assert_eq!(strtonum("+7", 0, 100), Some(7));
        assert_eq!(strtonum("-0", 0, 100), Some(0));
        assert_eq!(strtonum("101", 0, 100), None);
        assert_eq!(strtonum("4x", 0, 100), None);
        assert_eq!(strtonum("", 0, 100), None);
        assert_eq!(strtonum("99999999999999999999999", 0, 100), None);
    }
}
