//! The colour changes tmux makes to a pane's cells as it draws them
//! (tty.c `tty_attributes`, `tty_check_fg`, `tty_check_bg`,
//! `tty_check_us`): the pane's palette, `pane-colours` under the entries the
//! program set with OSC 4 (colour.c `colour_palette_get`), and `dim=` of
//! the window style that applies (`colour_dim`, `tty_dim_default_colour`).

use std::rc::Rc;

use vtmux_core::colour;
use vtmux_vt::{Cell, Color};

/// A pane's palette (`struct colour_palette`): entry `n`, when set,
/// replaces palette colour `n` as the cell is drawn.
pub type Palette = Rc<[Option<Color>; 256]>;

/// `dim=N` of the pane's window style (`style_ctx->dim`), with what a
/// default colour dims from for the drawing client
/// (`tty_dim_default_colour`): its terminal's OSC 10 and 11 replies, else
/// white on black for a dark theme and black on white for a light one,
/// else the default, which does not dim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dim {
    pub pct: u8,
    pub fg: Color,
    pub bg: Color,
}

impl Dim {
    #[must_use]
    pub fn new(pct: u8) -> Option<Self> {
        (pct != 0).then_some(Self {
            pct,
            fg: Color::Default,
            bg: Color::Default,
        })
    }
}

/// `colour_palette_get`: the entry for a palette colour, in either form.
fn get(p: &[Option<Color>; 256], c: Color) -> Option<Color> {
    p[usize::from(c.palette_index()?)]
}

/// `colour_dim`: `c` moved `pct` percent toward black, as RGB; a default
/// colour stays.
fn dim(colour: Color, pct: u8) -> Color {
    let rgb = match colour {
        Color::Default => return colour,
        _ if pct >= 100 => return Color::Rgb(0, 0, 0),
        Color::Palette(n) | Color::Palette256(n) => colour::palette_rgb(n),
        Color::Rgb(red, green, blue) => (red, green, blue),
    };
    let keep = u16::from(100 - pct);
    let scale = |v: u8| u8::try_from(u16::from(v) * keep / 100).unwrap_or(u8::MAX);
    Color::Rgb(scale(rgb.0), scale(rgb.1), scale(rgb.2))
}

/// Change a pane's drawn cells (their default colours already put in,
/// `style_ctx->defaults`) as `tty_attributes` does: the palette, then the
/// dim, then the palette again with the bold-bright entry for colours 0-7
/// (`tty_check_fg`; vtmux knows no terminal with `Nobr`). Cells marked in
/// `no_palette` (`GRID_FLAG_NOPALETTE`) skip the palette, not the dim.
/// The dim leaves an erased cell's (no text) foreground alone: it shows
/// only its background.
pub fn apply(
    cells: &mut [Cell],
    palette: Option<&[Option<Color>; 256]>,
    no_palette: &[bool],
    d: Option<Dim>,
) {
    for (i, c) in cells.iter_mut().enumerate() {
        let text = !c.text.is_empty();
        let p = palette.filter(|_| !no_palette.get(i).copied().unwrap_or(false));
        let s = &mut c.style;
        if let Some(p) = p {
            s.fg = get(p, s.fg).unwrap_or(s.fg);
            s.bg = get(p, s.bg).unwrap_or(s.bg);
        }
        if let Some(d) = d {
            if text {
                if s.fg == Color::Default {
                    s.fg = d.fg;
                }
                s.fg = dim(s.fg, d.pct);
            }
            if s.bg == Color::Default {
                s.bg = d.bg;
            }
            s.bg = dim(s.bg, d.pct);
        }
        if let Some(p) = p {
            let fg = match s.fg {
                Color::Palette(n) if n < 8 && s.bold => Color::Palette(n + 8),
                fg => fg,
            };
            s.fg = get(p, fg).unwrap_or(s.fg);
            s.bg = get(p, s.bg).unwrap_or(s.bg);
            s.underline_color = get(p, s.underline_color).unwrap_or(s.underline_color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, fg: Color, bg: Color, bold: bool) -> Cell {
        let mut c = Cell {
            text: text.into(),
            width: 1,
            ..Cell::default()
        };
        c.style.fg = fg;
        c.style.bg = bg;
        c.style.bold = bold;
        c
    }

    fn palette(entries: &[(u8, Color)]) -> [Option<Color>; 256] {
        let mut p = [None; 256];
        for &(n, c) in entries {
            p[usize::from(n)] = Some(c);
        }
        p
    }

    /// colour.c `colour_palette_get` through tty.c `tty_attributes` and
    /// `tty_check_fg`: every form of a palette colour is looked up, an
    /// entry that is itself a palette colour is looked up again, and bold
    /// 0-7 takes the bright entry when the plain one is not set.
    #[test]
    fn palette_lookups() {
        let p = palette(&[
            (1, Color::Rgb(255, 0, 255)),
            (2, Color::Palette(4)),
            (4, Color::Palette(5)),
            (9, Color::Palette(12)),
            (100, Color::Rgb(1, 2, 3)),
        ]);
        let mut cells = [
            cell("a", Color::Palette(1), Color::Palette(1), false),
            cell("b", Color::Palette256(1), Color::Default, true),
            cell("c", Color::Palette(2), Color::Default, false),
            cell("d", Color::Palette(1), Color::Default, true),
            cell("e", Color::Palette(3), Color::Default, true),
            cell("f", Color::Palette(9), Color::Palette(100), false),
            cell("g", Color::Rgb(9, 9, 9), Color::Default, false),
            cell("", Color::Palette(1), Color::Palette(2), false),
        ];
        apply(&mut cells, Some(&p), &[], None);
        let got: Vec<(Color, Color)> = cells.iter().map(|c| (c.style.fg, c.style.bg)).collect();
        assert_eq!(
            got,
            [
                (Color::Rgb(255, 0, 255), Color::Rgb(255, 0, 255)),
                (Color::Rgb(255, 0, 255), Color::Default),
                // 2 -> 4, then 4 -> 5.
                (Color::Palette(5), Color::Default),
                // Bold 1: entry 1 first, an RGB colour is not looked up again.
                (Color::Rgb(255, 0, 255), Color::Default),
                // Bold 3: no entry 3, so entry 11, which is not set either.
                (Color::Palette(3), Color::Default),
                (Color::Palette(12), Color::Rgb(1, 2, 3)),
                (Color::Rgb(9, 9, 9), Color::Default),
                // No text: a space tmux would write in these colours.
                (Color::Rgb(255, 0, 255), Color::Palette(5)),
            ]
        );
        let mut marked = [cell("a", Color::Palette(1), Color::Default, false)];
        apply(&mut marked, Some(&p), &[true], None);
        assert_eq!(marked[0].style.fg, Color::Palette(1));
    }

    /// colour.c `colour_dim`: toward black by the percentage, in integer
    /// arithmetic on xterm's RGB for palette colours; the default stays
    /// unless the client's colours stand in (`tty_dim_default_colour`),
    /// and 100 is black.
    #[test]
    fn dim_colours() {
        let half = Dim::new(50);
        let mut cells = [
            cell("a", Color::Palette(1), Color::Palette(12), false),
            cell("b", Color::Rgb(200, 100, 51), Color::Default, false),
            cell("c", Color::Palette256(100), Color::Default, false),
            cell("", Color::Default, Color::Default, false),
        ];
        apply(&mut cells, None, &[], half);
        let got: Vec<(Color, Color)> = cells.iter().map(|c| (c.style.fg, c.style.bg)).collect();
        assert_eq!(
            got,
            [
                (Color::Rgb(0x40, 0, 0), Color::Rgb(0, 0, 0x7f)),
                (Color::Rgb(100, 50, 25), Color::Default),
                (Color::Rgb(0x43, 0x43, 0), Color::Default),
                (Color::Default, Color::Default),
            ]
        );
        let known = Dim {
            pct: 20,
            fg: Color::Rgb(255, 255, 255),
            bg: Color::Palette(0),
        };
        let mut cells = [
            cell("a", Color::Default, Color::Default, false),
            cell("", Color::Default, Color::Default, false),
        ];
        apply(&mut cells, None, &[], Some(known));
        assert_eq!(cells[0].style.fg, Color::Rgb(204, 204, 204));
        assert_eq!(cells[0].style.bg, Color::Rgb(0, 0, 0));
        assert_eq!(cells[1].style.fg, Color::Default);
        let mut black = [cell("a", Color::Default, Color::Palette(3), false)];
        apply(&mut black, None, &[], Dim::new(100));
        assert_eq!(
            (black[0].style.fg, black[0].style.bg),
            (Color::Default, Color::Rgb(0, 0, 0))
        );
    }
}
