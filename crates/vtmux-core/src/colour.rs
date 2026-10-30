//! Typed colours, parsed and printed exactly as tmux 3.8 does (`colour.c`).
//!
//! [`Colour::parse`] is `colour_fromstring` (colour.c:387): `#rrggbb`,
//! `colourN`/`colorN`, `default`, `terminal`, the `theme*` slots, the eight
//! ANSI names or `0-7`, the bright forms (`brightred` or `91`, tmux's `90-97`),
//! `greyN`/`grayN` and the X11 names. Display is `colour_tostring`
//! (colour.c:226). Results are pinned by `tests/style_cases.sh`, which feeds
//! typed colour options (`set -g status-fg X; show -gv status-fg`) to the
//! reference tmux.
//!
//! tmux packs a colour into one `int` with flag bits; the core does not: the
//! variants below are what the flags mean.

use std::fmt;

/// A colour a style can name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Colour {
    /// tmux's `8`: inherit the base style's colour.
    #[default]
    Default,
    /// tmux's `9`: the terminal's own default colour.
    Terminal,
    /// The eight ANSI colours, 0-7 (`black` .. `white`).
    Ansi(u8),
    /// The bright ANSI colours, 0-7 (`brightblack` .. `brightwhite`).
    Bright(u8),
    /// A palette index written as `colourN`/`colorN`.
    Palette(u8),
    Rgb(u8, u8, u8),
    /// One of the ten `theme*` slots, an index into [`THEME_NAMES`]. The slot
    /// is resolved per client when the colour is drawn.
    Theme(u8),
}

/// The `theme*` colour names in slot order (colour.c:30-87).
pub const THEME_NAMES: [&str; 10] = [
    "themeblack",
    "themewhite",
    "themelightgrey",
    "themedarkgrey",
    "themegreen",
    "themeyellow",
    "themered",
    "themeblue",
    "themecyan",
    "thememagenta",
];

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];
const BRIGHT_NAMES: [&str; 8] = [
    "brightblack",
    "brightred",
    "brightgreen",
    "brightyellow",
    "brightblue",
    "brightmagenta",
    "brightcyan",
    "brightwhite",
];

/// `bad colour: X`: a typed colour option (`status-fg`, `pane-colours[]`) was
/// given something `colour_fromstring` rejects (options.c:616, 1333).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadColour(pub String);

impl fmt::Display for BadColour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad colour: {}", self.0)
    }
}

impl std::error::Error for BadColour {}

/// C's `strtonum(3)` as tmux's compat copy has it: `strtoll` in base 10
/// (leading white space and one sign allowed, nothing after the digits), then a
/// range check. `None` is any of invalid, too small, too large.
pub(crate) fn strtonum(s: &str, min: i64, max: i64) -> Option<i64> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, digits) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let magnitude: i128 = digits.parse().ok()?;
    let n = i64::try_from(if neg { -magnitude } else { magnitude }).ok()?;
    (min..=max).contains(&n).then_some(n)
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

impl Colour {
    /// `colour_fromstring` (colour.c:387). `None` is tmux's `-1`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s.starts_with('#') && s.len() == 7 {
            let hex = &s.as_bytes()[1..];
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            let byte = |i: usize| u8::from_str_radix(&s[1 + 2 * i..3 + 2 * i], 16).ok();
            return Some(Self::Rgb(byte(0)?, byte(1)?, byte(2)?));
        }
        for prefix in ["colour", "color"] {
            if starts_with_ci(s, prefix) {
                let n = strtonum(&s[prefix.len()..], 0, 255)?;
                return u8::try_from(n).ok().map(Self::Palette);
            }
        }
        if s.eq_ignore_ascii_case("default") {
            return Some(Self::Default);
        }
        if s.eq_ignore_ascii_case("terminal") {
            return Some(Self::Terminal);
        }
        if let Some(i) = THEME_NAMES.iter().position(|n| s.eq_ignore_ascii_case(n)) {
            return Some(Self::Theme(u8::try_from(i).ok()?));
        }
        for (i, name) in (0u8..).zip(ANSI_NAMES) {
            if s.eq_ignore_ascii_case(name) || s == i.to_string() {
                return Some(Self::Ansi(i));
            }
        }
        for (i, name) in (0u8..).zip(BRIGHT_NAMES) {
            if s.eq_ignore_ascii_case(name) || s == (90 + i).to_string() {
                return Some(Self::Bright(i));
            }
        }
        by_name(s)
    }

    /// The value of a typed colour option: [`Colour::parse`] with tmux's error.
    pub fn parse_option(s: &str) -> Result<Self, BadColour> {
        Self::parse(s).ok_or_else(|| BadColour(s.to_owned()))
    }

    /// `colour_force_rgb` (colour.c:189): the colour as RGB where it has a
    /// fixed one. `None` for `default`, `terminal` and the theme slots, which
    /// depend on the client.
    #[must_use]
    pub fn rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Self::Rgb(r, g, b) => Some((r, g, b)),
            Self::Palette(n) | Self::Ansi(n) => Some(palette_rgb(n)),
            Self::Bright(n) => Some(palette_rgb(8 + n)),
            Self::Default | Self::Terminal | Self::Theme(_) => None,
        }
    }
}

/// `colour_256toRGB` (colour.c:466): xterm's 256-colour palette.
#[must_use]
pub fn palette_rgb(n: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0x80, 0x00, 0x00),
        (0x00, 0x80, 0x00),
        (0x80, 0x80, 0x00),
        (0x00, 0x00, 0x80),
        (0x80, 0x00, 0x80),
        (0x00, 0x80, 0x80),
        (0xc0, 0xc0, 0xc0),
        (0x80, 0x80, 0x80),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x00, 0x00, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    const LEVELS: [u8; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    match n {
        0..=15 => BASE[usize::from(n)],
        16..=231 => {
            let i = usize::from(n - 16);
            (LEVELS[i / 36], LEVELS[i / 6 % 6], LEVELS[i % 6])
        }
        232..=255 => {
            let v = 8 + 10 * (n - 232);
            (v, v, v)
        }
    }
}

/// `colour_byname` (colour.c:566): `grey`/`gray` with an optional 0-100 level,
/// else an X11 name, case-insensitively and with spaces as in the table.
fn by_name(name: &str) -> Option<Colour> {
    if starts_with_ci(name, "grey") || starts_with_ci(name, "gray") {
        if name.len() == 4 {
            return Some(Colour::Rgb(0xbe, 0xbe, 0xbe));
        }
        let level = strtonum(&name[4..], 0, 100)?;
        // `round(2.55 * c)` for c in 0..=100, which is 0..=255.
        let level = u8::try_from(level).ok()?;
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v = (2.55 * f64::from(level)).round() as u8;
        return Some(Colour::Rgb(v, v, v));
    }
    X11.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, (r, g, b))| Colour::Rgb(r, g, b))
}

impl fmt::Display for Colour {
    /// `colour_tostring` (colour.c:226).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Default => f.write_str("default"),
            Self::Terminal => f.write_str("terminal"),
            Self::Ansi(n) => f.write_str(ANSI_NAMES.get(usize::from(n)).unwrap_or(&"invalid")),
            Self::Bright(n) => f.write_str(BRIGHT_NAMES.get(usize::from(n)).unwrap_or(&"invalid")),
            Self::Palette(n) => write!(f, "colour{n}"),
            Self::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Self::Theme(n) => f.write_str(THEME_NAMES.get(usize::from(n)).unwrap_or(&"invalid")),
        }
    }
}

/// The X11 colour names `colour_byname` knows (colour.c:571-1150), as written
/// there: CamelCase names and their spaced forms.
static X11: &[(&str, (u8, u8, u8))] = &[
    // BEGIN GENERATED x11
    ("AliceBlue", (0xf0, 0xf8, 0xff)),
    ("AntiqueWhite", (0xfa, 0xeb, 0xd7)),
    ("AntiqueWhite1", (0xff, 0xef, 0xdb)),
    ("AntiqueWhite2", (0xee, 0xdf, 0xcc)),
    ("AntiqueWhite3", (0xcd, 0xc0, 0xb0)),
    ("AntiqueWhite4", (0x8b, 0x83, 0x78)),
    ("BlanchedAlmond", (0xff, 0xeb, 0xcd)),
    ("BlueViolet", (0x8a, 0x2b, 0xe2)),
    ("CadetBlue", (0x5f, 0x9e, 0xa0)),
    ("CadetBlue1", (0x98, 0xf5, 0xff)),
    ("CadetBlue2", (0x8e, 0xe5, 0xee)),
    ("CadetBlue3", (0x7a, 0xc5, 0xcd)),
    ("CadetBlue4", (0x53, 0x86, 0x8b)),
    ("CornflowerBlue", (0x64, 0x95, 0xed)),
    ("DarkBlue", (0x00, 0x00, 0x8b)),
    ("DarkCyan", (0x00, 0x8b, 0x8b)),
    ("DarkGoldenrod", (0xb8, 0x86, 0x0b)),
    ("DarkGoldenrod1", (0xff, 0xb9, 0x0f)),
    ("DarkGoldenrod2", (0xee, 0xad, 0x0e)),
    ("DarkGoldenrod3", (0xcd, 0x95, 0x0c)),
    ("DarkGoldenrod4", (0x8b, 0x65, 0x08)),
    ("DarkGray", (0xa9, 0xa9, 0xa9)),
    ("DarkGreen", (0x00, 0x64, 0x00)),
    ("DarkGrey", (0xa9, 0xa9, 0xa9)),
    ("DarkKhaki", (0xbd, 0xb7, 0x6b)),
    ("DarkMagenta", (0x8b, 0x00, 0x8b)),
    ("DarkOliveGreen", (0x55, 0x6b, 0x2f)),
    ("DarkOliveGreen1", (0xca, 0xff, 0x70)),
    ("DarkOliveGreen2", (0xbc, 0xee, 0x68)),
    ("DarkOliveGreen3", (0xa2, 0xcd, 0x5a)),
    ("DarkOliveGreen4", (0x6e, 0x8b, 0x3d)),
    ("DarkOrange", (0xff, 0x8c, 0x00)),
    ("DarkOrange1", (0xff, 0x7f, 0x00)),
    ("DarkOrange2", (0xee, 0x76, 0x00)),
    ("DarkOrange3", (0xcd, 0x66, 0x00)),
    ("DarkOrange4", (0x8b, 0x45, 0x00)),
    ("DarkOrchid", (0x99, 0x32, 0xcc)),
    ("DarkOrchid1", (0xbf, 0x3e, 0xff)),
    ("DarkOrchid2", (0xb2, 0x3a, 0xee)),
    ("DarkOrchid3", (0x9a, 0x32, 0xcd)),
    ("DarkOrchid4", (0x68, 0x22, 0x8b)),
    ("DarkRed", (0x8b, 0x00, 0x00)),
    ("DarkSalmon", (0xe9, 0x96, 0x7a)),
    ("DarkSeaGreen", (0x8f, 0xbc, 0x8f)),
    ("DarkSeaGreen1", (0xc1, 0xff, 0xc1)),
    ("DarkSeaGreen2", (0xb4, 0xee, 0xb4)),
    ("DarkSeaGreen3", (0x9b, 0xcd, 0x9b)),
    ("DarkSeaGreen4", (0x69, 0x8b, 0x69)),
    ("DarkSlateBlue", (0x48, 0x3d, 0x8b)),
    ("DarkSlateGray", (0x2f, 0x4f, 0x4f)),
    ("DarkSlateGray1", (0x97, 0xff, 0xff)),
    ("DarkSlateGray2", (0x8d, 0xee, 0xee)),
    ("DarkSlateGray3", (0x79, 0xcd, 0xcd)),
    ("DarkSlateGray4", (0x52, 0x8b, 0x8b)),
    ("DarkSlateGrey", (0x2f, 0x4f, 0x4f)),
    ("DarkTurquoise", (0x00, 0xce, 0xd1)),
    ("DarkViolet", (0x94, 0x00, 0xd3)),
    ("DeepPink", (0xff, 0x14, 0x93)),
    ("DeepPink1", (0xff, 0x14, 0x93)),
    ("DeepPink2", (0xee, 0x12, 0x89)),
    ("DeepPink3", (0xcd, 0x10, 0x76)),
    ("DeepPink4", (0x8b, 0x0a, 0x50)),
    ("DeepSkyBlue", (0x00, 0xbf, 0xff)),
    ("DeepSkyBlue1", (0x00, 0xbf, 0xff)),
    ("DeepSkyBlue2", (0x00, 0xb2, 0xee)),
    ("DeepSkyBlue3", (0x00, 0x9a, 0xcd)),
    ("DeepSkyBlue4", (0x00, 0x68, 0x8b)),
    ("DimGray", (0x69, 0x69, 0x69)),
    ("DimGrey", (0x69, 0x69, 0x69)),
    ("DodgerBlue", (0x1e, 0x90, 0xff)),
    ("DodgerBlue1", (0x1e, 0x90, 0xff)),
    ("DodgerBlue2", (0x1c, 0x86, 0xee)),
    ("DodgerBlue3", (0x18, 0x74, 0xcd)),
    ("DodgerBlue4", (0x10, 0x4e, 0x8b)),
    ("FloralWhite", (0xff, 0xfa, 0xf0)),
    ("ForestGreen", (0x22, 0x8b, 0x22)),
    ("GhostWhite", (0xf8, 0xf8, 0xff)),
    ("GreenYellow", (0xad, 0xff, 0x2f)),
    ("HotPink", (0xff, 0x69, 0xb4)),
    ("HotPink1", (0xff, 0x6e, 0xb4)),
    ("HotPink2", (0xee, 0x6a, 0xa7)),
    ("HotPink3", (0xcd, 0x60, 0x90)),
    ("HotPink4", (0x8b, 0x3a, 0x62)),
    ("IndianRed", (0xcd, 0x5c, 0x5c)),
    ("IndianRed1", (0xff, 0x6a, 0x6a)),
    ("IndianRed2", (0xee, 0x63, 0x63)),
    ("IndianRed3", (0xcd, 0x55, 0x55)),
    ("IndianRed4", (0x8b, 0x3a, 0x3a)),
    ("LavenderBlush", (0xff, 0xf0, 0xf5)),
    ("LavenderBlush1", (0xff, 0xf0, 0xf5)),
    ("LavenderBlush2", (0xee, 0xe0, 0xe5)),
    ("LavenderBlush3", (0xcd, 0xc1, 0xc5)),
    ("LavenderBlush4", (0x8b, 0x83, 0x86)),
    ("LawnGreen", (0x7c, 0xfc, 0x00)),
    ("LemonChiffon", (0xff, 0xfa, 0xcd)),
    ("LemonChiffon1", (0xff, 0xfa, 0xcd)),
    ("LemonChiffon2", (0xee, 0xe9, 0xbf)),
    ("LemonChiffon3", (0xcd, 0xc9, 0xa5)),
    ("LemonChiffon4", (0x8b, 0x89, 0x70)),
    ("LightBlue", (0xad, 0xd8, 0xe6)),
    ("LightBlue1", (0xbf, 0xef, 0xff)),
    ("LightBlue2", (0xb2, 0xdf, 0xee)),
    ("LightBlue3", (0x9a, 0xc0, 0xcd)),
    ("LightBlue4", (0x68, 0x83, 0x8b)),
    ("LightCoral", (0xf0, 0x80, 0x80)),
    ("LightCyan", (0xe0, 0xff, 0xff)),
    ("LightCyan1", (0xe0, 0xff, 0xff)),
    ("LightCyan2", (0xd1, 0xee, 0xee)),
    ("LightCyan3", (0xb4, 0xcd, 0xcd)),
    ("LightCyan4", (0x7a, 0x8b, 0x8b)),
    ("LightGoldenrod", (0xee, 0xdd, 0x82)),
    ("LightGoldenrod1", (0xff, 0xec, 0x8b)),
    ("LightGoldenrod2", (0xee, 0xdc, 0x82)),
    ("LightGoldenrod3", (0xcd, 0xbe, 0x70)),
    ("LightGoldenrod4", (0x8b, 0x81, 0x4c)),
    ("LightGoldenrodYellow", (0xfa, 0xfa, 0xd2)),
    ("LightGray", (0xd3, 0xd3, 0xd3)),
    ("LightGreen", (0x90, 0xee, 0x90)),
    ("LightGrey", (0xd3, 0xd3, 0xd3)),
    ("LightPink", (0xff, 0xb6, 0xc1)),
    ("LightPink1", (0xff, 0xae, 0xb9)),
    ("LightPink2", (0xee, 0xa2, 0xad)),
    ("LightPink3", (0xcd, 0x8c, 0x95)),
    ("LightPink4", (0x8b, 0x5f, 0x65)),
    ("LightSalmon", (0xff, 0xa0, 0x7a)),
    ("LightSalmon1", (0xff, 0xa0, 0x7a)),
    ("LightSalmon2", (0xee, 0x95, 0x72)),
    ("LightSalmon3", (0xcd, 0x81, 0x62)),
    ("LightSalmon4", (0x8b, 0x57, 0x42)),
    ("LightSeaGreen", (0x20, 0xb2, 0xaa)),
    ("LightSkyBlue", (0x87, 0xce, 0xfa)),
    ("LightSkyBlue1", (0xb0, 0xe2, 0xff)),
    ("LightSkyBlue2", (0xa4, 0xd3, 0xee)),
    ("LightSkyBlue3", (0x8d, 0xb6, 0xcd)),
    ("LightSkyBlue4", (0x60, 0x7b, 0x8b)),
    ("LightSlateBlue", (0x84, 0x70, 0xff)),
    ("LightSlateGray", (0x77, 0x88, 0x99)),
    ("LightSlateGrey", (0x77, 0x88, 0x99)),
    ("LightSteelBlue", (0xb0, 0xc4, 0xde)),
    ("LightSteelBlue1", (0xca, 0xe1, 0xff)),
    ("LightSteelBlue2", (0xbc, 0xd2, 0xee)),
    ("LightSteelBlue3", (0xa2, 0xb5, 0xcd)),
    ("LightSteelBlue4", (0x6e, 0x7b, 0x8b)),
    ("LightYellow", (0xff, 0xff, 0xe0)),
    ("LightYellow1", (0xff, 0xff, 0xe0)),
    ("LightYellow2", (0xee, 0xee, 0xd1)),
    ("LightYellow3", (0xcd, 0xcd, 0xb4)),
    ("LightYellow4", (0x8b, 0x8b, 0x7a)),
    ("LimeGreen", (0x32, 0xcd, 0x32)),
    ("MediumAquamarine", (0x66, 0xcd, 0xaa)),
    ("MediumBlue", (0x00, 0x00, 0xcd)),
    ("MediumOrchid", (0xba, 0x55, 0xd3)),
    ("MediumOrchid1", (0xe0, 0x66, 0xff)),
    ("MediumOrchid2", (0xd1, 0x5f, 0xee)),
    ("MediumOrchid3", (0xb4, 0x52, 0xcd)),
    ("MediumOrchid4", (0x7a, 0x37, 0x8b)),
    ("MediumPurple", (0x93, 0x70, 0xdb)),
    ("MediumPurple1", (0xab, 0x82, 0xff)),
    ("MediumPurple2", (0x9f, 0x79, 0xee)),
    ("MediumPurple3", (0x89, 0x68, 0xcd)),
    ("MediumPurple4", (0x5d, 0x47, 0x8b)),
    ("MediumSeaGreen", (0x3c, 0xb3, 0x71)),
    ("MediumSlateBlue", (0x7b, 0x68, 0xee)),
    ("MediumSpringGreen", (0x00, 0xfa, 0x9a)),
    ("MediumTurquoise", (0x48, 0xd1, 0xcc)),
    ("MediumVioletRed", (0xc7, 0x15, 0x85)),
    ("MidnightBlue", (0x19, 0x19, 0x70)),
    ("MintCream", (0xf5, 0xff, 0xfa)),
    ("MistyRose", (0xff, 0xe4, 0xe1)),
    ("MistyRose1", (0xff, 0xe4, 0xe1)),
    ("MistyRose2", (0xee, 0xd5, 0xd2)),
    ("MistyRose3", (0xcd, 0xb7, 0xb5)),
    ("MistyRose4", (0x8b, 0x7d, 0x7b)),
    ("NavajoWhite", (0xff, 0xde, 0xad)),
    ("NavajoWhite1", (0xff, 0xde, 0xad)),
    ("NavajoWhite2", (0xee, 0xcf, 0xa1)),
    ("NavajoWhite3", (0xcd, 0xb3, 0x8b)),
    ("NavajoWhite4", (0x8b, 0x79, 0x5e)),
    ("NavyBlue", (0x00, 0x00, 0x80)),
    ("OldLace", (0xfd, 0xf5, 0xe6)),
    ("OliveDrab", (0x6b, 0x8e, 0x23)),
    ("OliveDrab1", (0xc0, 0xff, 0x3e)),
    ("OliveDrab2", (0xb3, 0xee, 0x3a)),
    ("OliveDrab3", (0x9a, 0xcd, 0x32)),
    ("OliveDrab4", (0x69, 0x8b, 0x22)),
    ("OrangeRed", (0xff, 0x45, 0x00)),
    ("OrangeRed1", (0xff, 0x45, 0x00)),
    ("OrangeRed2", (0xee, 0x40, 0x00)),
    ("OrangeRed3", (0xcd, 0x37, 0x00)),
    ("OrangeRed4", (0x8b, 0x25, 0x00)),
    ("PaleGoldenrod", (0xee, 0xe8, 0xaa)),
    ("PaleGreen", (0x98, 0xfb, 0x98)),
    ("PaleGreen1", (0x9a, 0xff, 0x9a)),
    ("PaleGreen2", (0x90, 0xee, 0x90)),
    ("PaleGreen3", (0x7c, 0xcd, 0x7c)),
    ("PaleGreen4", (0x54, 0x8b, 0x54)),
    ("PaleTurquoise", (0xaf, 0xee, 0xee)),
    ("PaleTurquoise1", (0xbb, 0xff, 0xff)),
    ("PaleTurquoise2", (0xae, 0xee, 0xee)),
    ("PaleTurquoise3", (0x96, 0xcd, 0xcd)),
    ("PaleTurquoise4", (0x66, 0x8b, 0x8b)),
    ("PaleVioletRed", (0xdb, 0x70, 0x93)),
    ("PaleVioletRed1", (0xff, 0x82, 0xab)),
    ("PaleVioletRed2", (0xee, 0x79, 0x9f)),
    ("PaleVioletRed3", (0xcd, 0x68, 0x89)),
    ("PaleVioletRed4", (0x8b, 0x47, 0x5d)),
    ("PapayaWhip", (0xff, 0xef, 0xd5)),
    ("PeachPuff", (0xff, 0xda, 0xb9)),
    ("PeachPuff1", (0xff, 0xda, 0xb9)),
    ("PeachPuff2", (0xee, 0xcb, 0xad)),
    ("PeachPuff3", (0xcd, 0xaf, 0x95)),
    ("PeachPuff4", (0x8b, 0x77, 0x65)),
    ("PowderBlue", (0xb0, 0xe0, 0xe6)),
    ("RebeccaPurple", (0x66, 0x33, 0x99)),
    ("RosyBrown", (0xbc, 0x8f, 0x8f)),
    ("RosyBrown1", (0xff, 0xc1, 0xc1)),
    ("RosyBrown2", (0xee, 0xb4, 0xb4)),
    ("RosyBrown3", (0xcd, 0x9b, 0x9b)),
    ("RosyBrown4", (0x8b, 0x69, 0x69)),
    ("RoyalBlue", (0x41, 0x69, 0xe1)),
    ("RoyalBlue1", (0x48, 0x76, 0xff)),
    ("RoyalBlue2", (0x43, 0x6e, 0xee)),
    ("RoyalBlue3", (0x3a, 0x5f, 0xcd)),
    ("RoyalBlue4", (0x27, 0x40, 0x8b)),
    ("SaddleBrown", (0x8b, 0x45, 0x13)),
    ("SandyBrown", (0xf4, 0xa4, 0x60)),
    ("SeaGreen", (0x2e, 0x8b, 0x57)),
    ("SeaGreen1", (0x54, 0xff, 0x9f)),
    ("SeaGreen2", (0x4e, 0xee, 0x94)),
    ("SeaGreen3", (0x43, 0xcd, 0x80)),
    ("SeaGreen4", (0x2e, 0x8b, 0x57)),
    ("SkyBlue", (0x87, 0xce, 0xeb)),
    ("SkyBlue1", (0x87, 0xce, 0xff)),
    ("SkyBlue2", (0x7e, 0xc0, 0xee)),
    ("SkyBlue3", (0x6c, 0xa6, 0xcd)),
    ("SkyBlue4", (0x4a, 0x70, 0x8b)),
    ("SlateBlue", (0x6a, 0x5a, 0xcd)),
    ("SlateBlue1", (0x83, 0x6f, 0xff)),
    ("SlateBlue2", (0x7a, 0x67, 0xee)),
    ("SlateBlue3", (0x69, 0x59, 0xcd)),
    ("SlateBlue4", (0x47, 0x3c, 0x8b)),
    ("SlateGray", (0x70, 0x80, 0x90)),
    ("SlateGray1", (0xc6, 0xe2, 0xff)),
    ("SlateGray2", (0xb9, 0xd3, 0xee)),
    ("SlateGray3", (0x9f, 0xb6, 0xcd)),
    ("SlateGray4", (0x6c, 0x7b, 0x8b)),
    ("SlateGrey", (0x70, 0x80, 0x90)),
    ("SpringGreen", (0x00, 0xff, 0x7f)),
    ("SpringGreen1", (0x00, 0xff, 0x7f)),
    ("SpringGreen2", (0x00, 0xee, 0x76)),
    ("SpringGreen3", (0x00, 0xcd, 0x66)),
    ("SpringGreen4", (0x00, 0x8b, 0x45)),
    ("SteelBlue", (0x46, 0x82, 0xb4)),
    ("SteelBlue1", (0x63, 0xb8, 0xff)),
    ("SteelBlue2", (0x5c, 0xac, 0xee)),
    ("SteelBlue3", (0x4f, 0x94, 0xcd)),
    ("SteelBlue4", (0x36, 0x64, 0x8b)),
    ("VioletRed", (0xd0, 0x20, 0x90)),
    ("VioletRed1", (0xff, 0x3e, 0x96)),
    ("VioletRed2", (0xee, 0x3a, 0x8c)),
    ("VioletRed3", (0xcd, 0x32, 0x78)),
    ("VioletRed4", (0x8b, 0x22, 0x52)),
    ("WebGray", (0x80, 0x80, 0x80)),
    ("WebGreen", (0x00, 0x80, 0x00)),
    ("WebGrey", (0x80, 0x80, 0x80)),
    ("WebMaroon", (0x80, 0x00, 0x00)),
    ("WebPurple", (0x80, 0x00, 0x80)),
    ("WhiteSmoke", (0xf5, 0xf5, 0xf5)),
    ("X11Gray", (0xbe, 0xbe, 0xbe)),
    ("X11Green", (0x00, 0xff, 0x00)),
    ("X11Grey", (0xbe, 0xbe, 0xbe)),
    ("X11Maroon", (0xb0, 0x30, 0x60)),
    ("X11Purple", (0xa0, 0x20, 0xf0)),
    ("YellowGreen", (0x9a, 0xcd, 0x32)),
    ("alice blue", (0xf0, 0xf8, 0xff)),
    ("antique white", (0xfa, 0xeb, 0xd7)),
    ("aqua", (0x00, 0xff, 0xff)),
    ("aquamarine", (0x7f, 0xff, 0xd4)),
    ("aquamarine1", (0x7f, 0xff, 0xd4)),
    ("aquamarine2", (0x76, 0xee, 0xc6)),
    ("aquamarine3", (0x66, 0xcd, 0xaa)),
    ("aquamarine4", (0x45, 0x8b, 0x74)),
    ("azure", (0xf0, 0xff, 0xff)),
    ("azure1", (0xf0, 0xff, 0xff)),
    ("azure2", (0xe0, 0xee, 0xee)),
    ("azure3", (0xc1, 0xcd, 0xcd)),
    ("azure4", (0x83, 0x8b, 0x8b)),
    ("beige", (0xf5, 0xf5, 0xdc)),
    ("bisque", (0xff, 0xe4, 0xc4)),
    ("bisque1", (0xff, 0xe4, 0xc4)),
    ("bisque2", (0xee, 0xd5, 0xb7)),
    ("bisque3", (0xcd, 0xb7, 0x9e)),
    ("bisque4", (0x8b, 0x7d, 0x6b)),
    ("black", (0x00, 0x00, 0x00)),
    ("blanched almond", (0xff, 0xeb, 0xcd)),
    ("blue violet", (0x8a, 0x2b, 0xe2)),
    ("blue", (0x00, 0x00, 0xff)),
    ("blue1", (0x00, 0x00, 0xff)),
    ("blue2", (0x00, 0x00, 0xee)),
    ("blue3", (0x00, 0x00, 0xcd)),
    ("blue4", (0x00, 0x00, 0x8b)),
    ("brown", (0xa5, 0x2a, 0x2a)),
    ("brown1", (0xff, 0x40, 0x40)),
    ("brown2", (0xee, 0x3b, 0x3b)),
    ("brown3", (0xcd, 0x33, 0x33)),
    ("brown4", (0x8b, 0x23, 0x23)),
    ("burlywood", (0xde, 0xb8, 0x87)),
    ("burlywood1", (0xff, 0xd3, 0x9b)),
    ("burlywood2", (0xee, 0xc5, 0x91)),
    ("burlywood3", (0xcd, 0xaa, 0x7d)),
    ("burlywood4", (0x8b, 0x73, 0x55)),
    ("cadet blue", (0x5f, 0x9e, 0xa0)),
    ("chartreuse", (0x7f, 0xff, 0x00)),
    ("chartreuse1", (0x7f, 0xff, 0x00)),
    ("chartreuse2", (0x76, 0xee, 0x00)),
    ("chartreuse3", (0x66, 0xcd, 0x00)),
    ("chartreuse4", (0x45, 0x8b, 0x00)),
    ("chocolate", (0xd2, 0x69, 0x1e)),
    ("chocolate1", (0xff, 0x7f, 0x24)),
    ("chocolate2", (0xee, 0x76, 0x21)),
    ("chocolate3", (0xcd, 0x66, 0x1d)),
    ("chocolate4", (0x8b, 0x45, 0x13)),
    ("coral", (0xff, 0x7f, 0x50)),
    ("coral1", (0xff, 0x72, 0x56)),
    ("coral2", (0xee, 0x6a, 0x50)),
    ("coral3", (0xcd, 0x5b, 0x45)),
    ("coral4", (0x8b, 0x3e, 0x2f)),
    ("cornflower blue", (0x64, 0x95, 0xed)),
    ("cornsilk", (0xff, 0xf8, 0xdc)),
    ("cornsilk1", (0xff, 0xf8, 0xdc)),
    ("cornsilk2", (0xee, 0xe8, 0xcd)),
    ("cornsilk3", (0xcd, 0xc8, 0xb1)),
    ("cornsilk4", (0x8b, 0x88, 0x78)),
    ("crimson", (0xdc, 0x14, 0x3c)),
    ("cyan", (0x00, 0xff, 0xff)),
    ("cyan1", (0x00, 0xff, 0xff)),
    ("cyan2", (0x00, 0xee, 0xee)),
    ("cyan3", (0x00, 0xcd, 0xcd)),
    ("cyan4", (0x00, 0x8b, 0x8b)),
    ("dark blue", (0x00, 0x00, 0x8b)),
    ("dark cyan", (0x00, 0x8b, 0x8b)),
    ("dark goldenrod", (0xb8, 0x86, 0x0b)),
    ("dark gray", (0xa9, 0xa9, 0xa9)),
    ("dark green", (0x00, 0x64, 0x00)),
    ("dark grey", (0xa9, 0xa9, 0xa9)),
    ("dark khaki", (0xbd, 0xb7, 0x6b)),
    ("dark magenta", (0x8b, 0x00, 0x8b)),
    ("dark olive green", (0x55, 0x6b, 0x2f)),
    ("dark orange", (0xff, 0x8c, 0x00)),
    ("dark orchid", (0x99, 0x32, 0xcc)),
    ("dark red", (0x8b, 0x00, 0x00)),
    ("dark salmon", (0xe9, 0x96, 0x7a)),
    ("dark sea green", (0x8f, 0xbc, 0x8f)),
    ("dark slate blue", (0x48, 0x3d, 0x8b)),
    ("dark slate gray", (0x2f, 0x4f, 0x4f)),
    ("dark slate grey", (0x2f, 0x4f, 0x4f)),
    ("dark turquoise", (0x00, 0xce, 0xd1)),
    ("dark violet", (0x94, 0x00, 0xd3)),
    ("deep pink", (0xff, 0x14, 0x93)),
    ("deep sky blue", (0x00, 0xbf, 0xff)),
    ("dim gray", (0x69, 0x69, 0x69)),
    ("dim grey", (0x69, 0x69, 0x69)),
    ("dodger blue", (0x1e, 0x90, 0xff)),
    ("firebrick", (0xb2, 0x22, 0x22)),
    ("firebrick1", (0xff, 0x30, 0x30)),
    ("firebrick2", (0xee, 0x2c, 0x2c)),
    ("firebrick3", (0xcd, 0x26, 0x26)),
    ("firebrick4", (0x8b, 0x1a, 0x1a)),
    ("floral white", (0xff, 0xfa, 0xf0)),
    ("forest green", (0x22, 0x8b, 0x22)),
    ("fuchsia", (0xff, 0x00, 0xff)),
    ("gainsboro", (0xdc, 0xdc, 0xdc)),
    ("ghost white", (0xf8, 0xf8, 0xff)),
    ("gold", (0xff, 0xd7, 0x00)),
    ("gold1", (0xff, 0xd7, 0x00)),
    ("gold2", (0xee, 0xc9, 0x00)),
    ("gold3", (0xcd, 0xad, 0x00)),
    ("gold4", (0x8b, 0x75, 0x00)),
    ("goldenrod", (0xda, 0xa5, 0x20)),
    ("goldenrod1", (0xff, 0xc1, 0x25)),
    ("goldenrod2", (0xee, 0xb4, 0x22)),
    ("goldenrod3", (0xcd, 0x9b, 0x1d)),
    ("goldenrod4", (0x8b, 0x69, 0x14)),
    ("green yellow", (0xad, 0xff, 0x2f)),
    ("green", (0x00, 0xff, 0x00)),
    ("green1", (0x00, 0xff, 0x00)),
    ("green2", (0x00, 0xee, 0x00)),
    ("green3", (0x00, 0xcd, 0x00)),
    ("green4", (0x00, 0x8b, 0x00)),
    ("honeydew", (0xf0, 0xff, 0xf0)),
    ("honeydew1", (0xf0, 0xff, 0xf0)),
    ("honeydew2", (0xe0, 0xee, 0xe0)),
    ("honeydew3", (0xc1, 0xcd, 0xc1)),
    ("honeydew4", (0x83, 0x8b, 0x83)),
    ("hot pink", (0xff, 0x69, 0xb4)),
    ("indian red", (0xcd, 0x5c, 0x5c)),
    ("indigo", (0x4b, 0x00, 0x82)),
    ("ivory", (0xff, 0xff, 0xf0)),
    ("ivory1", (0xff, 0xff, 0xf0)),
    ("ivory2", (0xee, 0xee, 0xe0)),
    ("ivory3", (0xcd, 0xcd, 0xc1)),
    ("ivory4", (0x8b, 0x8b, 0x83)),
    ("khaki", (0xf0, 0xe6, 0x8c)),
    ("khaki1", (0xff, 0xf6, 0x8f)),
    ("khaki2", (0xee, 0xe6, 0x85)),
    ("khaki3", (0xcd, 0xc6, 0x73)),
    ("khaki4", (0x8b, 0x86, 0x4e)),
    ("lavender blush", (0xff, 0xf0, 0xf5)),
    ("lavender", (0xe6, 0xe6, 0xfa)),
    ("lawn green", (0x7c, 0xfc, 0x00)),
    ("lemon chiffon", (0xff, 0xfa, 0xcd)),
    ("light blue", (0xad, 0xd8, 0xe6)),
    ("light coral", (0xf0, 0x80, 0x80)),
    ("light cyan", (0xe0, 0xff, 0xff)),
    ("light goldenrod yellow", (0xfa, 0xfa, 0xd2)),
    ("light goldenrod", (0xee, 0xdd, 0x82)),
    ("light gray", (0xd3, 0xd3, 0xd3)),
    ("light green", (0x90, 0xee, 0x90)),
    ("light grey", (0xd3, 0xd3, 0xd3)),
    ("light pink", (0xff, 0xb6, 0xc1)),
    ("light salmon", (0xff, 0xa0, 0x7a)),
    ("light sea green", (0x20, 0xb2, 0xaa)),
    ("light sky blue", (0x87, 0xce, 0xfa)),
    ("light slate blue", (0x84, 0x70, 0xff)),
    ("light slate gray", (0x77, 0x88, 0x99)),
    ("light slate grey", (0x77, 0x88, 0x99)),
    ("light steel blue", (0xb0, 0xc4, 0xde)),
    ("light yellow", (0xff, 0xff, 0xe0)),
    ("lime green", (0x32, 0xcd, 0x32)),
    ("lime", (0x00, 0xff, 0x00)),
    ("linen", (0xfa, 0xf0, 0xe6)),
    ("magenta", (0xff, 0x00, 0xff)),
    ("magenta1", (0xff, 0x00, 0xff)),
    ("magenta2", (0xee, 0x00, 0xee)),
    ("magenta3", (0xcd, 0x00, 0xcd)),
    ("magenta4", (0x8b, 0x00, 0x8b)),
    ("maroon", (0xb0, 0x30, 0x60)),
    ("maroon1", (0xff, 0x34, 0xb3)),
    ("maroon2", (0xee, 0x30, 0xa7)),
    ("maroon3", (0xcd, 0x29, 0x90)),
    ("maroon4", (0x8b, 0x1c, 0x62)),
    ("medium aquamarine", (0x66, 0xcd, 0xaa)),
    ("medium blue", (0x00, 0x00, 0xcd)),
    ("medium orchid", (0xba, 0x55, 0xd3)),
    ("medium purple", (0x93, 0x70, 0xdb)),
    ("medium sea green", (0x3c, 0xb3, 0x71)),
    ("medium slate blue", (0x7b, 0x68, 0xee)),
    ("medium spring green", (0x00, 0xfa, 0x9a)),
    ("medium turquoise", (0x48, 0xd1, 0xcc)),
    ("medium violet red", (0xc7, 0x15, 0x85)),
    ("midnight blue", (0x19, 0x19, 0x70)),
    ("mint cream", (0xf5, 0xff, 0xfa)),
    ("misty rose", (0xff, 0xe4, 0xe1)),
    ("moccasin", (0xff, 0xe4, 0xb5)),
    ("navajo white", (0xff, 0xde, 0xad)),
    ("navy blue", (0x00, 0x00, 0x80)),
    ("navy", (0x00, 0x00, 0x80)),
    ("old lace", (0xfd, 0xf5, 0xe6)),
    ("olive drab", (0x6b, 0x8e, 0x23)),
    ("olive", (0x80, 0x80, 0x00)),
    ("orange red", (0xff, 0x45, 0x00)),
    ("orange", (0xff, 0xa5, 0x00)),
    ("orange1", (0xff, 0xa5, 0x00)),
    ("orange2", (0xee, 0x9a, 0x00)),
    ("orange3", (0xcd, 0x85, 0x00)),
    ("orange4", (0x8b, 0x5a, 0x00)),
    ("orchid", (0xda, 0x70, 0xd6)),
    ("orchid1", (0xff, 0x83, 0xfa)),
    ("orchid2", (0xee, 0x7a, 0xe9)),
    ("orchid3", (0xcd, 0x69, 0xc9)),
    ("orchid4", (0x8b, 0x47, 0x89)),
    ("pale goldenrod", (0xee, 0xe8, 0xaa)),
    ("pale green", (0x98, 0xfb, 0x98)),
    ("pale turquoise", (0xaf, 0xee, 0xee)),
    ("pale violet red", (0xdb, 0x70, 0x93)),
    ("papaya whip", (0xff, 0xef, 0xd5)),
    ("peach puff", (0xff, 0xda, 0xb9)),
    ("peru", (0xcd, 0x85, 0x3f)),
    ("pink", (0xff, 0xc0, 0xcb)),
    ("pink1", (0xff, 0xb5, 0xc5)),
    ("pink2", (0xee, 0xa9, 0xb8)),
    ("pink3", (0xcd, 0x91, 0x9e)),
    ("pink4", (0x8b, 0x63, 0x6c)),
    ("plum", (0xdd, 0xa0, 0xdd)),
    ("plum1", (0xff, 0xbb, 0xff)),
    ("plum2", (0xee, 0xae, 0xee)),
    ("plum3", (0xcd, 0x96, 0xcd)),
    ("plum4", (0x8b, 0x66, 0x8b)),
    ("powder blue", (0xb0, 0xe0, 0xe6)),
    ("purple", (0xa0, 0x20, 0xf0)),
    ("purple1", (0x9b, 0x30, 0xff)),
    ("purple2", (0x91, 0x2c, 0xee)),
    ("purple3", (0x7d, 0x26, 0xcd)),
    ("purple4", (0x55, 0x1a, 0x8b)),
    ("rebecca purple", (0x66, 0x33, 0x99)),
    ("red", (0xff, 0x00, 0x00)),
    ("red1", (0xff, 0x00, 0x00)),
    ("red2", (0xee, 0x00, 0x00)),
    ("red3", (0xcd, 0x00, 0x00)),
    ("red4", (0x8b, 0x00, 0x00)),
    ("rosy brown", (0xbc, 0x8f, 0x8f)),
    ("royal blue", (0x41, 0x69, 0xe1)),
    ("saddle brown", (0x8b, 0x45, 0x13)),
    ("salmon", (0xfa, 0x80, 0x72)),
    ("salmon1", (0xff, 0x8c, 0x69)),
    ("salmon2", (0xee, 0x82, 0x62)),
    ("salmon3", (0xcd, 0x70, 0x54)),
    ("salmon4", (0x8b, 0x4c, 0x39)),
    ("sandy brown", (0xf4, 0xa4, 0x60)),
    ("sea green", (0x2e, 0x8b, 0x57)),
    ("seashell", (0xff, 0xf5, 0xee)),
    ("seashell1", (0xff, 0xf5, 0xee)),
    ("seashell2", (0xee, 0xe5, 0xde)),
    ("seashell3", (0xcd, 0xc5, 0xbf)),
    ("seashell4", (0x8b, 0x86, 0x82)),
    ("sienna", (0xa0, 0x52, 0x2d)),
    ("sienna1", (0xff, 0x82, 0x47)),
    ("sienna2", (0xee, 0x79, 0x42)),
    ("sienna3", (0xcd, 0x68, 0x39)),
    ("sienna4", (0x8b, 0x47, 0x26)),
    ("silver", (0xc0, 0xc0, 0xc0)),
    ("sky blue", (0x87, 0xce, 0xeb)),
    ("slate blue", (0x6a, 0x5a, 0xcd)),
    ("slate gray", (0x70, 0x80, 0x90)),
    ("slate grey", (0x70, 0x80, 0x90)),
    ("snow", (0xff, 0xfa, 0xfa)),
    ("snow1", (0xff, 0xfa, 0xfa)),
    ("snow2", (0xee, 0xe9, 0xe9)),
    ("snow3", (0xcd, 0xc9, 0xc9)),
    ("snow4", (0x8b, 0x89, 0x89)),
    ("spring green", (0x00, 0xff, 0x7f)),
    ("steel blue", (0x46, 0x82, 0xb4)),
    ("tan", (0xd2, 0xb4, 0x8c)),
    ("tan1", (0xff, 0xa5, 0x4f)),
    ("tan2", (0xee, 0x9a, 0x49)),
    ("tan3", (0xcd, 0x85, 0x3f)),
    ("tan4", (0x8b, 0x5a, 0x2b)),
    ("teal", (0x00, 0x80, 0x80)),
    ("thistle", (0xd8, 0xbf, 0xd8)),
    ("thistle1", (0xff, 0xe1, 0xff)),
    ("thistle2", (0xee, 0xd2, 0xee)),
    ("thistle3", (0xcd, 0xb5, 0xcd)),
    ("thistle4", (0x8b, 0x7b, 0x8b)),
    ("tomato", (0xff, 0x63, 0x47)),
    ("tomato1", (0xff, 0x63, 0x47)),
    ("tomato2", (0xee, 0x5c, 0x42)),
    ("tomato3", (0xcd, 0x4f, 0x39)),
    ("tomato4", (0x8b, 0x36, 0x26)),
    ("turquoise", (0x40, 0xe0, 0xd0)),
    ("turquoise1", (0x00, 0xf5, 0xff)),
    ("turquoise2", (0x00, 0xe5, 0xee)),
    ("turquoise3", (0x00, 0xc5, 0xcd)),
    ("turquoise4", (0x00, 0x86, 0x8b)),
    ("violet red", (0xd0, 0x20, 0x90)),
    ("violet", (0xee, 0x82, 0xee)),
    ("web gray", (0x80, 0x80, 0x80)),
    ("web green", (0x00, 0x80, 0x00)),
    ("web grey", (0x80, 0x80, 0x80)),
    ("web maroon", (0x80, 0x00, 0x00)),
    ("web purple", (0x80, 0x00, 0x80)),
    ("wheat", (0xf5, 0xde, 0xb3)),
    ("wheat1", (0xff, 0xe7, 0xba)),
    ("wheat2", (0xee, 0xd8, 0xae)),
    ("wheat3", (0xcd, 0xba, 0x96)),
    ("wheat4", (0x8b, 0x7e, 0x66)),
    ("white smoke", (0xf5, 0xf5, 0xf5)),
    ("white", (0xff, 0xff, 0xff)),
    ("x11 gray", (0xbe, 0xbe, 0xbe)),
    ("x11 green", (0x00, 0xff, 0x00)),
    ("x11 grey", (0xbe, 0xbe, 0xbe)),
    ("x11 maroon", (0xb0, 0x30, 0x60)),
    ("x11 purple", (0xa0, 0x20, 0xf0)),
    ("yellow green", (0x9a, 0xcd, 0x32)),
    ("yellow", (0xff, 0xff, 0x00)),
    ("yellow1", (0xff, 0xff, 0x00)),
    ("yellow2", (0xee, 0xee, 0x00)),
    ("yellow3", (0xcd, 0xcd, 0x00)),
    ("yellow4", (0x8b, 0x8b, 0x00)),
    // END GENERATED x11
];

#[cfg(test)]
mod tests {
    use super::*;

    // BEGIN GENERATED colour_forms
    // set -g status-fg X; show -gv status-fg   (every 7th also set -g pane-colours[0] X)
    const FORMS: &[(&str, Result<&str, &str>)] = &[
        ("default", Ok("default")),
        ("Default", Ok("default")),
        ("terminal", Ok("terminal")),
        ("TERMINAL", Ok("terminal")),
        ("black", Ok("black")),
        ("Red", Ok("red")),
        ("GREEN", Ok("green")),
        ("yellow", Ok("yellow")),
        ("Blue", Ok("blue")),
        ("magenta", Ok("magenta")),
        ("cyan", Ok("cyan")),
        ("white", Ok("white")),
        ("brightblack", Ok("brightblack")),
        ("brightRED", Ok("brightred")),
        ("brightgreen", Ok("brightgreen")),
        ("brightyellow", Ok("brightyellow")),
        ("brightblue", Ok("brightblue")),
        ("brightmagenta", Ok("brightmagenta")),
        ("brightcyan", Ok("brightcyan")),
        ("brightwhite", Ok("brightwhite")),
        ("90", Ok("brightblack")),
        ("91", Ok("brightred")),
        ("92", Ok("brightgreen")),
        ("93", Ok("brightyellow")),
        ("94", Ok("brightblue")),
        ("95", Ok("brightmagenta")),
        ("96", Ok("brightcyan")),
        ("97", Ok("brightwhite")),
        ("0", Ok("black")),
        ("1", Ok("red")),
        ("2", Ok("green")),
        ("3", Ok("yellow")),
        ("4", Ok("blue")),
        ("5", Ok("magenta")),
        ("6", Ok("cyan")),
        ("7", Ok("white")),
        ("8", Err("bad colour: 8")),
        ("9", Err("bad colour: 9")),
        ("10", Err("bad colour: 10")),
        ("15", Err("bad colour: 15")),
        ("16", Err("bad colour: 16")),
        ("89", Err("bad colour: 89")),
        ("98", Err("bad colour: 98")),
        ("255", Err("bad colour: 255")),
        ("256", Err("bad colour: 256")),
        ("-1", Err("bad colour: -1")),
        ("00", Err("bad colour: 00")),
        ("01", Err("bad colour: 01")),
        ("091", Err("bad colour: 091")),
        ("0x1", Err("bad colour: 0x1")),
        (" 1", Err("bad colour:  1")),
        ("1 ", Err("bad colour: 1 ")),
        ("colour0", Ok("colour0")),
        ("colour5", Ok("colour5")),
        ("COLOUR5", Ok("colour5")),
        ("Colour255", Ok("colour255")),
        ("color17", Ok("colour17")),
        ("COLOR5", Ok("colour5")),
        ("colour256", Err("bad colour: colour256")),
        ("colour", Err("bad colour: colour")),
        ("color", Err("bad colour: color")),
        ("colour-0", Ok("colour0")),
        ("colour+5", Ok("colour5")),
        ("colour 5", Ok("colour5")),
        ("colour5x", Err("bad colour: colour5x")),
        ("colour05", Ok("colour5")),
        ("colour1000", Err("bad colour: colour1000")),
        ("#FFaa00", Ok("#ffaa00")),
        ("#ffaa00", Ok("#ffaa00")),
        ("#000000", Ok("#000000")),
        ("#fff", Err("bad colour: #fff")),
        ("#12345", Err("bad colour: #12345")),
        ("#1234567", Err("bad colour: #1234567")),
        ("#ggg000", Err("bad colour: #ggg000")),
        ("#12345g", Err("bad colour: #12345g")),
        ("# 12345", Err("bad colour: # 12345")),
        ("grey", Ok("#bebebe")),
        ("gray", Ok("#bebebe")),
        ("GREY", Ok("#bebebe")),
        ("grey0", Ok("#000000")),
        ("grey50", Ok("#7f7f7f")),
        ("gray100", Ok("#ffffff")),
        ("grey33", Ok("#545454")),
        ("grey 5", Ok("#0d0d0d")),
        ("grey-1", Err("bad colour: grey-1")),
        ("grey101", Err("bad colour: grey101")),
        ("greyx", Err("bad colour: greyx")),
        ("grey1.5", Err("bad colour: grey1.5")),
        ("grey05", Ok("#0d0d0d")),
        ("themeblack", Ok("themeblack")),
        ("themewhite", Ok("themewhite")),
        ("themelightgrey", Ok("themelightgrey")),
        ("themedarkgrey", Ok("themedarkgrey")),
        ("themegreen", Ok("themegreen")),
        ("themeyellow", Ok("themeyellow")),
        ("themered", Ok("themered")),
        ("themeblue", Ok("themeblue")),
        ("themecyan", Ok("themecyan")),
        ("thememagenta", Ok("thememagenta")),
        ("THEMEGREEN", Ok("themegreen")),
        ("theme", Err("bad colour: theme")),
        ("themepink", Err("bad colour: themepink")),
        ("alice blue", Ok("#f0f8ff")),
        ("AliceBlue", Ok("#f0f8ff")),
        ("aliceblue", Ok("#f0f8ff")),
        ("ALICEBLUE", Ok("#f0f8ff")),
        ("antique white", Ok("#faebd7")),
        ("DarkOliveGreen2", Ok("#bcee68")),
        ("dark olive green", Ok("#556b2f")),
        ("medium spring green", Ok("#00fa9a")),
        ("YellowGreen", Ok("#9acd32")),
        ("light goldenrod yellow", Ok("#fafad2")),
        ("navy blue", Ok("#000080")),
        ("NavyBlue", Ok("#000080")),
        ("violet red", Ok("#d02090")),
        ("WhiteSmoke", Ok("#f5f5f5")),
        ("white smoke", Ok("#f5f5f5")),
        ("Snow4", Ok("#8b8989")),
        ("snow 4", Err("bad colour: snow 4")),
        ("", Err("bad colour: ")),
        (" ", Err("bad colour:  ")),
        ("red ", Err("bad colour: red ")),
        (" red", Err("bad colour:  red")),
        ("nope", Err("bad colour: nope")),
        ("red,blue", Err("bad colour: red,blue")),
        ("none", Err("bad colour: none")),
        ("-", Err("bad colour: -")),
        ("colour red", Err("bad colour: colour red")),
    ];
    // END GENERATED colour_forms

    // BEGIN GENERATED colour_rgb
    // set -gF @out '#{c:X}'; show -gv @out   (colour_force_rgb: hex, or empty)
    const RGB_ROWS: &[(&str, &str)] = &[
        ("default", ""),
        ("Default", ""),
        ("terminal", ""),
        ("TERMINAL", ""),
        ("black", "000000"),
        ("Red", "800000"),
        ("GREEN", "008000"),
        ("yellow", "808000"),
        ("Blue", "000080"),
        ("magenta", "800080"),
        ("cyan", "008080"),
        ("white", "c0c0c0"),
        ("brightblack", "808080"),
        ("brightRED", "ff0000"),
        ("brightgreen", "00ff00"),
        ("brightyellow", "ffff00"),
        ("brightblue", "0000ff"),
        ("brightmagenta", "ff00ff"),
        ("brightcyan", "00ffff"),
        ("brightwhite", "ffffff"),
        ("90", "808080"),
        ("91", "ff0000"),
        ("92", "00ff00"),
        ("93", "ffff00"),
        ("94", "0000ff"),
        ("95", "ff00ff"),
        ("96", "00ffff"),
        ("97", "ffffff"),
        ("0", "000000"),
        ("1", "800000"),
        ("2", "008000"),
        ("3", "808000"),
        ("4", "000080"),
        ("5", "800080"),
        ("6", "008080"),
        ("7", "c0c0c0"),
        ("colour0", "000000"),
        ("colour5", "800080"),
        ("COLOUR5", "800080"),
        ("Colour255", "eeeeee"),
        ("color17", "00005f"),
        ("COLOR5", "800080"),
        ("colour-0", "000000"),
        ("colour+5", "800080"),
        ("colour 5", "800080"),
        ("colour05", "800080"),
        ("grey", "bebebe"),
        ("gray", "bebebe"),
        ("GREY", "bebebe"),
        ("grey0", "000000"),
        ("grey50", "7f7f7f"),
        ("gray100", "ffffff"),
        ("grey33", "545454"),
        ("grey 5", "0d0d0d"),
        ("grey05", "0d0d0d"),
        ("themeblack", ""),
        ("themewhite", ""),
        ("themelightgrey", ""),
        ("themedarkgrey", ""),
        ("themegreen", ""),
        ("themeyellow", ""),
        ("themered", ""),
        ("themeblue", ""),
        ("themecyan", ""),
        ("thememagenta", ""),
        ("THEMEGREEN", ""),
        ("alice blue", "f0f8ff"),
        ("AliceBlue", "f0f8ff"),
        ("aliceblue", "f0f8ff"),
        ("ALICEBLUE", "f0f8ff"),
        ("antique white", "faebd7"),
        ("DarkOliveGreen2", "bcee68"),
        ("dark olive green", "556b2f"),
        ("medium spring green", "00fa9a"),
        ("YellowGreen", "9acd32"),
        ("light goldenrod yellow", "fafad2"),
        ("navy blue", "000080"),
        ("NavyBlue", "000080"),
        ("violet red", "d02090"),
        ("WhiteSmoke", "f5f5f5"),
        ("white smoke", "f5f5f5"),
        ("Snow4", "8b8989"),
        ("#ff8000", "ff8000"),
        ("#010203", "010203"),
        ("nope", ""),
        ("none", ""),
        ("colour9", "ff0000"),
        ("colour196", "ff0000"),
        ("colour232", "080808"),
    ];
    // #{c:colourN} for N in 0..=255
    const PALETTE: &[&str] = &[
        "000000", "800000", "008000", "808000", "000080", "800080", "008080", "c0c0c0", "808080",
        "ff0000", "00ff00", "ffff00", "0000ff", "ff00ff", "00ffff", "ffffff", "000000", "00005f",
        "000087", "0000af", "0000d7", "0000ff", "005f00", "005f5f", "005f87", "005faf", "005fd7",
        "005fff", "008700", "00875f", "008787", "0087af", "0087d7", "0087ff", "00af00", "00af5f",
        "00af87", "00afaf", "00afd7", "00afff", "00d700", "00d75f", "00d787", "00d7af", "00d7d7",
        "00d7ff", "00ff00", "00ff5f", "00ff87", "00ffaf", "00ffd7", "00ffff", "5f0000", "5f005f",
        "5f0087", "5f00af", "5f00d7", "5f00ff", "5f5f00", "5f5f5f", "5f5f87", "5f5faf", "5f5fd7",
        "5f5fff", "5f8700", "5f875f", "5f8787", "5f87af", "5f87d7", "5f87ff", "5faf00", "5faf5f",
        "5faf87", "5fafaf", "5fafd7", "5fafff", "5fd700", "5fd75f", "5fd787", "5fd7af", "5fd7d7",
        "5fd7ff", "5fff00", "5fff5f", "5fff87", "5fffaf", "5fffd7", "5fffff", "870000", "87005f",
        "870087", "8700af", "8700d7", "8700ff", "875f00", "875f5f", "875f87", "875faf", "875fd7",
        "875fff", "878700", "87875f", "878787", "8787af", "8787d7", "8787ff", "87af00", "87af5f",
        "87af87", "87afaf", "87afd7", "87afff", "87d700", "87d75f", "87d787", "87d7af", "87d7d7",
        "87d7ff", "87ff00", "87ff5f", "87ff87", "87ffaf", "87ffd7", "87ffff", "af0000", "af005f",
        "af0087", "af00af", "af00d7", "af00ff", "af5f00", "af5f5f", "af5f87", "af5faf", "af5fd7",
        "af5fff", "af8700", "af875f", "af8787", "af87af", "af87d7", "af87ff", "afaf00", "afaf5f",
        "afaf87", "afafaf", "afafd7", "afafff", "afd700", "afd75f", "afd787", "afd7af", "afd7d7",
        "afd7ff", "afff00", "afff5f", "afff87", "afffaf", "afffd7", "afffff", "d70000", "d7005f",
        "d70087", "d700af", "d700d7", "d700ff", "d75f00", "d75f5f", "d75f87", "d75faf", "d75fd7",
        "d75fff", "d78700", "d7875f", "d78787", "d787af", "d787d7", "d787ff", "d7af00", "d7af5f",
        "d7af87", "d7afaf", "d7afd7", "d7afff", "d7d700", "d7d75f", "d7d787", "d7d7af", "d7d7d7",
        "d7d7ff", "d7ff00", "d7ff5f", "d7ff87", "d7ffaf", "d7ffd7", "d7ffff", "ff0000", "ff005f",
        "ff0087", "ff00af", "ff00d7", "ff00ff", "ff5f00", "ff5f5f", "ff5f87", "ff5faf", "ff5fd7",
        "ff5fff", "ff8700", "ff875f", "ff8787", "ff87af", "ff87d7", "ff87ff", "ffaf00", "ffaf5f",
        "ffaf87", "ffafaf", "ffafd7", "ffafff", "ffd700", "ffd75f", "ffd787", "ffd7af", "ffd7d7",
        "ffd7ff", "ffff00", "ffff5f", "ffff87", "ffffaf", "ffffd7", "ffffff", "080808", "121212",
        "1c1c1c", "262626", "303030", "3a3a3a", "444444", "4e4e4e", "585858", "626262", "6c6c6c",
        "767676", "808080", "8a8a8a", "949494", "9e9e9e", "a8a8a8", "b2b2b2", "bcbcbc", "c6c6c6",
        "d0d0d0", "dadada", "e4e4e4", "eeeeee",
    ];
    // #{c:greyN} for N in 0..=100
    const GREYS: &[&str] = &[
        "000000", "030303", "050505", "080808", "0a0a0a", "0d0d0d", "0f0f0f", "121212", "141414",
        "171717", "1a1a1a", "1c1c1c", "1f1f1f", "212121", "242424", "262626", "292929", "2b2b2b",
        "2e2e2e", "303030", "333333", "363636", "383838", "3b3b3b", "3d3d3d", "404040", "424242",
        "454545", "474747", "4a4a4a", "4d4d4d", "4f4f4f", "525252", "545454", "575757", "595959",
        "5c5c5c", "5e5e5e", "616161", "636363", "666666", "696969", "6b6b6b", "6e6e6e", "707070",
        "737373", "757575", "787878", "7a7a7a", "7d7d7d", "7f7f7f", "828282", "858585", "878787",
        "8a8a8a", "8c8c8c", "8f8f8f", "919191", "949494", "969696", "999999", "9c9c9c", "9e9e9e",
        "a1a1a1", "a3a3a3", "a6a6a6", "a8a8a8", "ababab", "adadad", "b0b0b0", "b3b3b3", "b5b5b5",
        "b8b8b8", "bababa", "bdbdbd", "bfbfbf", "c2c2c2", "c4c4c4", "c7c7c7", "c9c9c9", "cccccc",
        "cfcfcf", "d1d1d1", "d4d4d4", "d6d6d6", "d9d9d9", "dbdbdb", "dedede", "e0e0e0", "e3e3e3",
        "e5e5e5", "e8e8e8", "ebebeb", "ededed", "f0f0f0", "f2f2f2", "f5f5f5", "f7f7f7", "fafafa",
        "fcfcfc", "ffffff",
    ];
    // END GENERATED colour_rgb

    #[test]
    fn typed_colour_options_match_reference() {
        for &(input, want) in FORMS {
            let got = Colour::parse_option(input)
                .map(|c| c.to_string())
                .map_err(|e| e.to_string());
            let want = want.map(str::to_owned).map_err(str::to_owned);
            assert_eq!(got, want, "input {input:?}");
        }
    }

    fn hex(c: Option<(u8, u8, u8)>) -> String {
        c.map(|(r, g, b)| format!("{r:02x}{g:02x}{b:02x}"))
            .unwrap_or_default()
    }

    #[test]
    fn force_rgb_matches_reference() {
        for &(input, want) in RGB_ROWS {
            let got = hex(Colour::parse(input).and_then(Colour::rgb));
            assert_eq!(got, want, "input {input:?}");
        }
    }

    #[test]
    fn palette_matches_reference() {
        for (n, &want) in (0u8..=255).zip(PALETTE.iter()) {
            let (r, g, b) = palette_rgb(n);
            assert_eq!(hex(Some((r, g, b))), want, "colour{n}");
            assert_eq!(hex(Colour::Palette(n).rgb()), want, "colour{n}");
        }
    }

    #[test]
    fn grey_levels_match_reference() {
        for (level, &want) in GREYS.iter().enumerate() {
            for word in ["grey", "gray", "GREY"] {
                let got = hex(Colour::parse(&format!("{word}{level}")).and_then(Colour::rgb));
                assert_eq!(got, want, "{word}{level}");
            }
        }
    }

    #[test]
    fn every_x11_name_resolves_case_insensitively() {
        // `colour_fromstring` reads these before the table (colour.c:411-421).
        const ANSI: [&str; 8] = [
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ];
        for &(name, (r, g, b)) in X11 {
            if ANSI.contains(&name.to_lowercase().as_str()) {
                continue;
            }
            let want = Colour::Rgb(r, g, b);
            assert_eq!(Colour::parse(name), Some(want), "{name}");
            assert_eq!(Colour::parse(&name.to_uppercase()), Some(want), "{name}");
            assert_eq!(Colour::parse(&name.to_lowercase()), Some(want), "{name}");
        }
    }

    #[test]
    fn display_round_trips_through_parse() {
        for c in [
            Colour::Default,
            Colour::Terminal,
            Colour::Ansi(3),
            Colour::Bright(5),
            Colour::Palette(200),
            Colour::Rgb(1, 2, 3),
            Colour::Theme(9),
        ] {
            assert_eq!(Colour::parse(&c.to_string()), Some(c));
        }
    }
}
