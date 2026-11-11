//! The `c/f` and `c/b` forms of the `c` modifier: tmux 3.8 `colour_toescape`
//! (colour.c:295) for a format with no client attached. The colour itself is
//! [`crate::colour::Colour`].
//!
//! With a client, `colour_toescape` resolves theme colours from the client's theme
//! (D-074); that is not modelled here. Its downgrade of RGB and 256 colours for the
//! client's terminal never runs (colour.c:318-323 masks disjoint bits), so none is
//! modelled either.

use crate::colour::Colour;

/// The ANSI colour a theme slot falls back to without a client (`terminal_colour`).
const THEME_TERMINAL: [u8; 10] = [0, 7, 7, 0, 2, 3, 1, 4, 6, 5];

/// `colour_toescape` (colour.c:295) with no client: the SGR sequence, or `None` when
/// the colour has none.
pub(super) fn to_escape(colour: Colour, bg: bool) -> Option<String> {
    let base: u32 = if bg { 40 } else { 30 };
    let colour = match colour {
        Colour::Theme(n) => Colour::Ansi(*THEME_TERMINAL.get(usize::from(n)).unwrap_or(&8)),
        c => c,
    };
    Some(match colour {
        Colour::Default | Colour::Terminal => format!("\x1b[{}m", base + 9),
        Colour::Rgb(red, green, blue) => format!("\x1b[{};2;{red};{green};{blue}m", base + 8),
        Colour::Palette(n) => format!("\x1b[{};5;{n}m", base + 8),
        Colour::Ansi(n) => format!("\x1b[{}m", base + u32::from(n)),
        Colour::Bright(n) => format!("\x1b[{}m", base + 60 + u32::from(n)),
        Colour::Theme(_) => return None,
    })
}
