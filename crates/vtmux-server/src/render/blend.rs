//! The pane blend (D26, `docs/specs/2026-11-04-pane-blend.md`): every cell
//! of a pane, explicit colours included, moved toward the colours of
//! `vtmux-pane-active-blend` or `vtmux-pane-inactive-blend` after the
//! window style. vtmux-only: tmux has no such layer.
//!
//! The arithmetic needs the outer terminal's colours ([`TermColours`], from
//! its OSC 10/11/4 replies); a client that has not answered OSC 10 and 11
//! gets no blend at all.

use vtmux_core::blend::{self, Blend, Part, Toward};
use vtmux_core::colour;
use vtmux_vt::{Cell, Color};

use super::Caps;
use super::compose::Theme;

type Rgb = (u8, u8, u8);

/// What a client's terminal said its colours are: OSC 10 (`fg`), OSC 11
/// (`bg`) and OSC 4 for palette entries 0-15, as the blend sees them: a
/// copy of the client's [`crate::term_queries::Reported`]
/// ([`Reported::blend_colours`](crate::term_queries::Reported::blend_colours)),
/// kept in [`Mix`] so a change recomposes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermColours {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub palette: [Option<Rgb>; 16],
}

impl TermColours {
    /// Palette entry `n` as the terminal shows it: its OSC 4 reply for
    /// 0-15 when there was one, else xterm's.
    fn palette_rgb(&self, n: u8) -> Rgb {
        self.palette
            .get(usize::from(n))
            .copied()
            .flatten()
            .unwrap_or_else(|| colour::palette_rgb(n))
    }
}

/// How one pane's cells are changed; part of the compose cache key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneBlend {
    /// Colours mixed in sRGB.
    Mix(Mix),
    /// A terminal of 16 colours or fewer: SGR 2 on an inactive pane.
    Faint,
}

/// A [`PaneBlend::Mix`]: targets resolved to RGB, with the terminal's
/// colours the cells' own come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mix {
    fg: Option<(Rgb, u8)>,
    bg: Option<(Rgb, u8)>,
    default_fg: Rgb,
    default_bg: Rgb,
    colours: TermColours,
    /// The terminal takes RGB; else the nearest 256-palette entry is sent.
    rgb: bool,
}

/// The blend for a pane from its option's value: `None` when the option is
/// empty or invalid, or the terminal's default colours are unknown (no
/// reply to OSC 10 and 11: a light terminal must never be blended toward
/// a guessed dark colour). `inactive` picks the 16-colour fallback, which
/// marks only inactive panes.
#[must_use]
pub fn resolve(
    value: &str,
    inactive: bool,
    colours: &TermColours,
    caps: Caps,
    theme: &Theme,
) -> Option<PaneBlend> {
    let Blend { fg, bg } = blend::parse(value).ok().flatten()?;
    let (ink, paper) = (colours.fg?, colours.bg?);
    if !caps.rgb && caps.palette_colours < 256 {
        return inactive.then_some(PaneBlend::Faint);
    }
    let target = |part: Option<Part>, default: Rgb| {
        part.map(|p| {
            let rgb = match p.toward {
                Toward::TerminalFg => ink,
                Toward::TerminalBg => paper,
                Toward::Colour(c) => colour_rgb(theme.colour(c), default, colours),
            };
            (rgb, p.pct)
        })
    };
    Some(PaneBlend::Mix(Mix {
        fg: target(fg, ink),
        bg: target(bg, paper),
        default_fg: ink,
        default_bg: paper,
        colours: *colours,
        rgb: caps.rgb,
    }))
}

/// What colour `c` looks like, `default` for the terminal's own.
fn colour_rgb(c: Color, default: Rgb, colours: &TermColours) -> Rgb {
    match c {
        Color::Default => default,
        Color::Palette(n) => colours.palette_rgb(n),
        Color::Rgb(r, g, b) => (r, g, b),
    }
}

impl PaneBlend {
    /// Blend a pane's cells as drawn (after the window style and
    /// `tty_draw_line`'s erasing). Erased cells (no text) show only their
    /// background, so only that changes.
    pub fn apply(&self, cells: &mut [Cell]) {
        match self {
            Self::Faint => {
                for c in cells.iter_mut().filter(|c| !c.text.is_empty()) {
                    c.style.faint = true;
                }
            }
            Self::Mix(m) => {
                for c in cells.iter_mut() {
                    m.cell(c);
                }
            }
        }
    }
}

impl Mix {
    /// Reverse video is blended as shown: the cell's background is the
    /// glyph's colour then, so it goes toward the `fg=` target, and the
    /// other way round; the attribute stays.
    fn cell(&self, c: &mut Cell) {
        let s = &mut c.style;
        let (fg_part, bg_part) = if s.inverse && !c.text.is_empty() {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        if !c.text.is_empty() {
            s.fg = self.mix(s.fg, self.default_fg, fg_part);
            // A default underline colour follows the glyph's.
            if s.underline_color != Color::Default {
                s.underline_color = self.mix(s.underline_color, self.default_fg, self.fg);
            }
        }
        s.bg = self.mix(s.bg, self.default_bg, bg_part);
    }

    fn mix(&self, c: Color, default: Rgb, part: Option<(Rgb, u8)>) -> Color {
        let Some((to, pct)) = part else {
            return c;
        };
        let from = colour_rgb(c, default, &self.colours);
        let (r, g, b) = blend::mix(from, to, pct);
        // Unchanged stays as it was: a default colour remains the
        // terminal's own (fewer bytes, erasable runs, and no palette
        // approximation of it on a 256-colour terminal).
        if (r, g, b) == from {
            return c;
        }
        if self.rgb {
            Color::Rgb(r, g, b)
        } else {
            Color::Palette(self.nearest((r, g, b)))
        }
    }

    /// The palette entry closest to `rgb`: xterm's cube and grey ramp
    /// (`colour_find_rgb`), or one of 0-15 the terminal told us.
    fn nearest(&self, rgb: Rgb) -> u8 {
        let dist = |(r, g, b): Rgb| {
            let d = |x: u8, y: u8| (i32::from(x) - i32::from(y)).pow(2);
            d(r, rgb.0) + d(g, rgb.1) + d(b, rgb.2)
        };
        let cube = colour::tty::find_rgb(rgb.0, rgb.1, rgb.2);
        let mut best = (dist(colour::palette_rgb(cube)), cube);
        for (n, known) in (0u8..).zip(self.colours.palette) {
            if let Some(p) = known
                && dist(p) < best.0
            {
                best = (dist(p), n);
            }
        }
        best.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtmux_vt::Style;

    const MOCHA_BG: Rgb = (0x1e, 0x1e, 0x2e);
    const MOCHA_FG: Rgb = (0xcd, 0xd6, 0xf4);

    fn caps(rgb: bool, palette_colours: u32) -> Caps {
        Caps {
            rgb,
            palette_colours,
            theme_palette: palette_colours >= 256,
            sync: false,
            bce: true,
            xterm: true,
            colours: palette_colours,
            overlay: false,
            no_am: false,
            light_theme: false,
        }
    }

    fn known() -> TermColours {
        TermColours {
            fg: Some(MOCHA_FG),
            bg: Some(MOCHA_BG),
            ..TermColours::default()
        }
    }

    fn cell(text: &str, style: Style) -> Cell {
        Cell {
            text: text.into(),
            width: 1,
            style,
            ..Cell::default()
        }
    }

    fn blended(value: &str, c: &Cell) -> Cell {
        let b = resolve(value, true, &known(), caps(true, 256), &Theme::default()).expect("on");
        let mut cells = [c.clone()];
        b.apply(&mut cells);
        cells[0].clone()
    }

    #[test]
    fn percent_0_25_100() {
        let red = cell(
            "x",
            Style {
                fg: Color::Rgb(255, 0, 0),
                bg: Color::Rgb(0, 0, 255),
                ..Style::default()
            },
        );
        let b = blended("fg=#000000/0,bg=#000000/100", &red);
        assert_eq!(
            (b.style.fg, b.style.bg),
            (Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 0))
        );
        let b = blended("fg=#000000/25,bg=#ffffff/25", &red);
        assert_eq!(
            (b.style.fg, b.style.bg),
            (Color::Rgb(191, 0, 0), Color::Rgb(64, 64, 255))
        );
    }

    #[test]
    fn default_colours_come_from_the_terminal() {
        let b = blended("fg=bg/25,bg=fg/50", &cell("x", Style::default()));
        assert_eq!(b.style.fg, Color::Rgb(161, 168, 195));
        assert_eq!(b.style.bg, Color::Rgb(118, 122, 145));
        // A part that is not set leaves its colour alone.
        let b = blended("bg=#000000/50", &cell("x", Style::default()));
        assert_eq!(
            (b.style.fg, b.style.bg),
            (Color::Default, Color::Rgb(15, 15, 23))
        );
    }

    #[test]
    fn palette_cells_use_the_terminals_palette_when_known() {
        let c = cell(
            "x",
            Style {
                fg: Color::Palette(1),
                ..Style::default()
            },
        );
        // xterm's red (128,0,0) half way to black.
        assert_eq!(blended("fg=#000000/50", &c).style.fg, Color::Rgb(64, 0, 0));
        let mut colours = known();
        colours.palette[1] = Some((0xf3, 0x8b, 0xa8));
        let b = resolve(
            "fg=#000000/50",
            true,
            &colours,
            caps(true, 256),
            &Theme::default(),
        )
        .unwrap();
        let mut cells = [c];
        b.apply(&mut cells);
        assert_eq!(cells[0].style.fg, Color::Rgb(122, 70, 84));
    }

    #[test]
    fn reverse_video_is_blended_as_shown() {
        let c = cell(
            "x",
            Style {
                fg: Color::Rgb(200, 200, 200),
                bg: Color::Rgb(0, 0, 0),
                inverse: true,
                ..Style::default()
            },
        );
        // Shown: glyph black (the bg slot), background grey (the fg slot).
        let b = blended("fg=#ffffff/100", &c);
        assert!(b.style.inverse);
        assert_eq!(
            (b.style.fg, b.style.bg),
            (Color::Rgb(200, 200, 200), Color::Rgb(255, 255, 255))
        );
        let b = blended("bg=#000000/50", &c);
        assert_eq!(
            (b.style.fg, b.style.bg),
            (Color::Rgb(100, 100, 100), Color::Rgb(0, 0, 0))
        );
    }

    #[test]
    fn underline_colour_follows_fg() {
        let c = cell(
            "x",
            Style {
                underline_color: Color::Rgb(0, 200, 0),
                ..Style::default()
            },
        );
        let b = blended("fg=#000000/50,bg=#ffffff/50", &c);
        assert_eq!(b.style.underline_color, Color::Rgb(0, 100, 0));
        let b = blended("fg=#000000/50", &cell("x", Style::default()));
        assert_eq!(b.style.underline_color, Color::Default, "follows the glyph");
    }

    #[test]
    fn erased_cells_change_only_their_background() {
        let erased = Cell {
            width: 1,
            ..Cell::default()
        };
        let b = blended("fg=#ff0000,bg=#000000/50", &erased);
        assert_eq!(b.style.fg, Color::Default);
        assert_eq!(b.style.bg, Color::Rgb(15, 15, 23));
        assert_eq!(b.text, "");
    }

    /// `bg=bg/25` on a default background moves it nowhere: it stays
    /// default, as does a colour blended 0%.
    #[test]
    fn an_unchanged_colour_is_kept() {
        let b = blended("fg=bg/25,bg=bg/25", &cell("x", Style::default()));
        assert_eq!(b.style.bg, Color::Default);
        assert_ne!(b.style.fg, Color::Default);
        let c = cell(
            "x",
            Style {
                fg: Color::Palette(1),
                ..Style::default()
            },
        );
        assert_eq!(blended("fg=#000000/0", &c).style.fg, Color::Palette(1));
    }

    #[test]
    fn no_reply_or_no_value_is_off() {
        let t = Theme::default();
        let c = caps(true, 256);
        assert_eq!(
            resolve("fg=bg/25", true, &TermColours::default(), c, &t),
            None
        );
        let only_bg = TermColours {
            bg: Some(MOCHA_BG),
            ..TermColours::default()
        };
        assert_eq!(resolve("fg=bg/25", true, &only_bg, c, &t), None);
        assert_eq!(resolve("", true, &known(), c, &t), None);
        assert_eq!(resolve("fg=nope", true, &known(), c, &t), None);
    }

    #[test]
    fn colour_depth_fallbacks() {
        let t = Theme::default();
        let c = cell(
            "x",
            Style {
                fg: Color::Rgb(255, 0, 0),
                ..Style::default()
            },
        );
        // 256 colours: the nearest palette entry.
        let b = resolve("fg=#000000/50", true, &known(), caps(false, 256), &t).unwrap();
        let mut cells = [c.clone(), Cell::default()];
        b.apply(&mut cells);
        assert_eq!(
            cells[0].style.fg,
            Color::Palette(88),
            "(128,0,0) is cube entry 88"
        );
        // 16 colours: SGR 2 on inactive panes only, no colour change.
        for colours in [8, 16] {
            let b = resolve(
                "fg=bg/25,bg=bg/25",
                true,
                &known(),
                caps(false, colours),
                &t,
            );
            assert_eq!(b, Some(PaneBlend::Faint));
            let mut cells = [c.clone(), Cell::default()];
            b.unwrap().apply(&mut cells);
            assert!(cells[0].style.faint);
            assert_eq!(cells[0].style.fg, Color::Rgb(255, 0, 0));
            assert_eq!(cells[1], Cell::default(), "an erased cell is left alone");
            assert_eq!(
                resolve("bg=bg/25", false, &known(), caps(false, colours), &t),
                None
            );
        }
    }

    #[test]
    fn nearest_prefers_a_known_base_colour() {
        let mut colours = known();
        colours.palette[1] = Some((0xf3, 0x8b, 0xa8));
        let b = resolve(
            "fg=#f38ba8",
            true,
            &colours,
            caps(false, 256),
            &Theme::default(),
        )
        .unwrap();
        let mut cells = [cell("x", Style::default())];
        b.apply(&mut cells);
        assert_eq!(cells[0].style.fg, Color::Palette(1));
    }
}
