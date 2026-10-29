#![allow(clippy::unreadable_literal)]
//! Colours for the `c` modifier: tmux 3.8 `colour_fromstring`, `colour_force_rgb` and
//! `colour_toescape` (colour.c) for a format with no client attached.
//!
//! TODO: the styles work (`colour.rs` at the crate root) owns the typed colour; replace
//! this private copy with it once it lands on m1.
//!
//! With a client, `colour_toescape` also downgrades to what the terminal supports and
//! resolves theme colours from the client; that is the server's business and is not
//! modelled here (no client: 256 colours and RGB are both available).

use super::colour_names::X11;
use super::num::strtonum;

/// A tmux colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Colour {
    /// `default` (SGR 39/49).
    Default,
    /// `terminal` (also SGR 39/49).
    Terminal,
    /// `black`..`white`, 0-7.
    Ansi(u8),
    /// `brightblack`..`brightwhite`, 90-97 as 0-7.
    Bright(u8),
    /// `colour0`..`colour255`.
    Palette(u8),
    Rgb(u32),
    /// A `themeX` slot, 0-9.
    Theme(u8),
}

const THEMES: [&str; 10] = [
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

/// The ANSI colour a theme slot falls back to without a client (`terminal_colour`).
const THEME_TERMINAL: [u8; 10] = [0, 7, 7, 0, 2, 3, 1, 4, 6, 5];

const NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

/// `colour_fromstring` (colour.c:387); `None` is -1.
pub(super) fn from_string(s: &str) -> Option<Colour> {
    let b = s.as_bytes();
    if b.first() == Some(&b'#') && b.len() == 7 {
        if !b[1..].iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        return u32::from_str_radix(&s[1..], 16).ok().map(Colour::Rgb);
    }
    for prefix in ["colour", "color"] {
        if b.len() >= prefix.len() && b[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
            let n = strtonum(&s[prefix.len()..], 0, 255)?;
            return u8::try_from(n).ok().map(Colour::Palette);
        }
    }
    if s.eq_ignore_ascii_case("default") {
        return Some(Colour::Default);
    }
    if s.eq_ignore_ascii_case("terminal") {
        return Some(Colour::Terminal);
    }
    if let Some(i) = THEMES.iter().position(|t| s.eq_ignore_ascii_case(t)) {
        return u8::try_from(i).ok().map(Colour::Theme);
    }
    for (i, name) in (0u8..).zip(NAMES) {
        if s.eq_ignore_ascii_case(name) || s == i.to_string() {
            return Some(Colour::Ansi(i));
        }
        let bright = format!("bright{name}");
        if s.eq_ignore_ascii_case(&bright) || s == (90 + i).to_string() {
            return Some(Colour::Bright(i));
        }
    }
    by_name(s)
}

/// `colour_byname` (colour.c:566): grey/gray with a percentage, then the X11 names.
fn by_name(name: &str) -> Option<Colour> {
    let b = name.as_bytes();
    if b.len() >= 4
        && (b[..4].eq_ignore_ascii_case(b"grey") || b[..4].eq_ignore_ascii_case(b"gray"))
    {
        if b.len() == 4 {
            return Some(Colour::Rgb(0x00be_bebe));
        }
        let pct = strtonum(&name[4..], 0, 100)?;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let c = (2.55 * pct as f64).round() as u32;
        return (c <= 255).then_some(Colour::Rgb(c << 16 | c << 8 | c));
    }
    X11.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, v)| Colour::Rgb(v))
}

/// `colour_256toRGB` (colour.c:466): the xterm palette.
fn palette_rgb(n: u8) -> u32 {
    const LEVELS: [u32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    const BASE: [u32; 16] = [
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0x808080,
        0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let n = usize::from(n);
    match n {
        0..16 => BASE[n],
        16..232 => {
            let i = n - 16;
            LEVELS[i / 36] << 16 | LEVELS[i / 6 % 6] << 8 | LEVELS[i % 6]
        }
        _ => {
            // The 24 greys: 8, 18, ... 238.
            let g = u32::try_from(8 + 10 * (n - 232)).unwrap_or(0);
            g << 16 | g << 8 | g
        }
    }
}

/// `colour_force_rgb` (colour.c:189): the colour as 24-bit RGB, when it has one.
pub(super) fn to_rgb(c: Colour) -> Option<u32> {
    match c {
        Colour::Rgb(v) => Some(v),
        Colour::Palette(n) | Colour::Ansi(n) => Some(palette_rgb(n)),
        Colour::Bright(n) => Some(palette_rgb(8 + n)),
        Colour::Default | Colour::Terminal | Colour::Theme(_) => None,
    }
}

/// `colour_toescape` (colour.c:295) with no client: the SGR sequence, or `None` when
/// the colour has none.
pub(super) fn to_escape(c: Colour, bg: bool) -> Option<String> {
    let o: u32 = if bg { 40 } else { 30 };
    let c = match c {
        Colour::Theme(n) => Colour::Ansi(*THEME_TERMINAL.get(usize::from(n)).unwrap_or(&8)),
        c => c,
    };
    Some(match c {
        Colour::Default | Colour::Terminal => format!("\x1b[{}m", o + 9),
        Colour::Rgb(v) => format!(
            "\x1b[{};2;{};{};{}m",
            o + 8,
            v >> 16 & 0xff,
            v >> 8 & 0xff,
            v & 0xff
        ),
        Colour::Palette(n) => format!("\x1b[{};5;{n}m", o + 8),
        Colour::Ansi(n) => format!("\x1b[{}m", o + u32::from(n)),
        Colour::Bright(n) => format!("\x1b[{}m", o + 60 + u32::from(n)),
        Colour::Theme(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_the_xterm_table() {
        assert_eq!(palette_rgb(16), 0x000000);
        assert_eq!(palette_rgb(196), 0xff0000);
        assert_eq!(palette_rgb(231), 0xffffff);
        assert_eq!(palette_rgb(232), 0x080808);
        assert_eq!(palette_rgb(255), 0xeeeeee);
    }
}
