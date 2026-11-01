//! Compose the final picture before diffing it (D14). Never stream pane
//! output directly: overlays and status are in the same snapshot.
//!
//! A window is drawn in two steps, as tmux's screen-redraw.c does: a
//! [`scene::Scene`] says what every cell is (pane, border, pane status line,
//! fill), then [`compose::compose`] draws it with the styles the model
//! gives ([`window::Source`]).
use std::fmt::Write as _;

pub mod compose;
pub mod popup;
pub mod scene;
pub mod status;
pub mod window;
use vtmux_core::colour::{self, Colour, tty::Depth};
use vtmux_core::protocol::Hello;
use vtmux_core::term_features::{Cap, Term};
use vtmux_vt::{Color, Snapshot, Style, Underline};

/// What the compositor needs of a client's terminal, from its
/// [`Term`] (terminfo-free, D8): `terminal-features`, `terminal-overrides`,
/// `-T` and `COLORTERM` have already been applied to it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent terminal capabilities"
)]
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub rgb: bool,
    /// `Term::palette_size`: what `tty_check_fg` believes the terminal has.
    pub palette_colours: u32,
    pub theme_palette: bool,
    pub sync: bool,
    pub bce: bool,
    pub xterm: bool,
    /// `#{client_colours}`.
    pub colours: u32,
    /// An overlay (a popup) is up: `tty_draw_line_clear` writes spaces,
    /// never erases (`overlay_check != NULL`, tty-draw.c:61).
    pub overlay: bool,
}
impl Caps {
    /// How [`compose::draw_line_by`] may clear a run of empty cells.
    #[must_use]
    pub fn clear(self) -> compose::Clear {
        if self.overlay {
            compose::Clear::Spaces
        } else if self.bce {
            compose::Clear::Bce
        } else {
            compose::Clear::FakeBce
        }
    }

    /// `tty_term_create` for a client's hello: `None` for a terminal vtmux
    /// has no table for.
    #[must_use]
    pub fn open_term(
        hello: &Hello,
        features: &[String],
        overrides: &[String],
    ) -> Option<(Term, Self)> {
        let tty = hello.tty.as_ref()?;
        let colorterm = hello
            .env
            .iter()
            .find(|(k, _)| k == "COLORTERM")
            .map(|(_, v)| v.as_str());
        let term = Term::open(&tty.term, &tty.features, colorterm, features, overrides)?;
        let caps = Self::from_term(&term);
        Some((term, caps))
    }
    #[must_use]
    pub fn from_term(term: &Term) -> Self {
        Self {
            rgb: term.rgb(),
            palette_colours: term.palette_size(),
            theme_palette: term.colours256(),
            sync: term.has(Cap::Sync),
            bce: term.is_xterm(),
            xterm: term.is_xterm(),
            colours: term.colours(),
            overlay: false,
        }
    }
    #[must_use]
    pub fn startup(self) -> Vec<u8> {
        let title = if self.xterm { "\x1b[22;0;0t" } else { "" };
        format!("\x1b[?1049h{title}\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?2026$p").into_bytes()
    }
    #[must_use]
    pub fn teardown(self) -> Vec<u8> {
        let title = if self.xterm { "\x1b[23;0;0t" } else { "" };
        format!("\x1b[r\x1b[0m\x1b[?1l\x1b>\x1b[H\x1b[2J\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004l\x1b[?1004l\x1b[?1049l{title}\x1b[?2031l").into_bytes()
    }
}

#[derive(Default)]
pub struct Compositor {
    last: Option<Snapshot>,
    /// The client is not keeping up: compose nothing until it drains.
    blocked: bool,
}
impl Compositor {
    pub fn invalidate(&mut self) {
        self.last = None;
    }
    /// Rows `y..y + n` of the last frame sent, if it had this size: what
    /// the terminal shows there.
    #[must_use]
    pub fn shown_rows(&self, cols: u16, rows: u16, y: u16, n: u16) -> Option<&[vtmux_vt::Cell]> {
        let last = self
            .last
            .as_ref()
            .filter(|s| (s.cols, s.rows) == (cols, rows))?;
        let at = usize::from(y) * usize::from(cols);
        last.cells.get(at..at + usize::from(n) * usize::from(cols))
    }
    /// Backpressure (D14), tty.c `tty_block_maybe` and `TTY_BLOCK_START` /
    /// `TTY_BLOCK_STOP`: once `queued` bytes for the client reach
    /// `1 + cols*rows*8`, stop composing; when they fall under
    /// `1 + cols*rows/8`, resume with a full redraw (tmux sets
    /// `CLIENT_ALLREDRAWFLAGS` and `tty_invalidate`). tmux drops its queued
    /// output instead; vtmux's queue is whole protocol messages, so it keeps
    /// them and composes nothing more, which bounds the queue the same way.
    pub fn ready(&mut self, queued: usize, cols: u16, rows: u16) -> bool {
        let cells = usize::from(cols) * usize::from(rows);
        if self.blocked {
            if queued > cells / 8 {
                return false;
            }
            self.blocked = false;
            self.invalidate();
            return true;
        }
        if queued > cells * 8 {
            self.blocked = true;
            return false;
        }
        true
    }
    /// One frame per tick. The caller supplies a fully composed screen;
    /// therefore changes behind opaque overlays cannot escape into a frame.
    pub fn frame(&mut self, screen: Snapshot, caps: Caps) -> Vec<u8> {
        if self.last.as_ref() == Some(&screen) {
            return Vec::new();
        }
        let mut out = String::new();
        if caps.sync {
            out.push_str("\x1b[?2026h");
        }
        out.push_str("\x1b[?25l");
        let same_size = self
            .last
            .as_ref()
            .is_some_and(|s| (s.cols, s.rows) == (screen.cols, screen.rows));
        let mut style = Style::default();
        out.push_str("\x1b[0m");
        for y in 0..screen.rows {
            let mut x = 0;
            while x < screen.cols {
                let cell = screen.cell(x, y).expect("screen cell");
                let changed =
                    !same_size || self.last.as_ref().and_then(|s| s.cell(x, y)) != Some(cell);
                if !changed || cell.width == 0 {
                    x += 1;
                    continue;
                }
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                // tty-draw.c tty_draw_line_clear: a run of ten or more empty
                // cells is erased (EL to the end of the line, else ECH), not
                // written: erased cells keep only the background and hold no
                // text, which a terminal (and capture -e) can tell apart.
                // compose::draw_line has already decided which pane and pane
                // status cells tmux erases (empty text, background only);
                // borders and fill keep their text, so they never join a run.
                let run = (x..screen.cols)
                    .take_while(|&cx| {
                        screen.cell(cx, y).is_some_and(|c| {
                            c.text.is_empty()
                                && c.width == 1
                                && c.style == cell.style
                                && (!same_size
                                    || self.last.as_ref().and_then(|s| s.cell(cx, y)) != Some(c))
                        })
                    })
                    .count();
                let erasable = cell.style.bg == Color::Default || caps.bce;
                if run >= 10 && erasable {
                    set_style(&mut out, &mut style, cell.style, caps);
                    let run = u16::try_from(run).unwrap_or(u16::MAX);
                    if x + run == screen.cols {
                        out.push_str("\x1b[K");
                        break;
                    }
                    let _ = write!(out, "\x1b[{run}X");
                    x += run;
                    continue;
                }
                // tmux tty_draw_line clears long empty runs with BCE: the
                // erased cells retain background, not foreground. Keep this
                // observable distinction for the status gap (oracle trace).
                let blank = cell.text.is_empty() || cell.text == " ";
                if blank
                    && cell.style == Style::default()
                    && (x..screen.cols).all(|cx| {
                        screen.cell(cx, y).is_some_and(|c| {
                            (c.text.is_empty() || c.text == " ") && c.style == cell.style
                        })
                    })
                    && caps.bce
                    && !caps.overlay
                {
                    set_style(&mut out, &mut style, cell.style, caps);
                    out.push_str("\x1b[K");
                    break;
                }
                set_style(&mut out, &mut style, cell.style, caps);
                out.push_str(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                });
                x += u16::from(cell.width.max(1));
            }
        }
        place_cursor(&mut out, &mut style, &screen, caps);
        if screen.cursor_visible {
            out.push_str("\x1b[?25h");
        }
        if caps.sync {
            out.push_str("\x1b[?2026l");
        }
        self.last = Some(screen);
        out.into_bytes()
    }
}
/// End the frame's drawing (`\x1b[0m`) and move the cursor. A column
/// past the last one is the terminal's pending wrap after the row's last
/// cell, which `tty_cursor` leaves alone (tty.c:2468); writing that cell
/// again puts the terminal there.
fn place_cursor(out: &mut String, style: &mut Style, screen: &Snapshot, caps: Caps) {
    match screen.cursor {
        Some((x, y)) if x >= screen.cols => {
            let last = (0..screen.cols)
                .rev()
                .find_map(|cx| screen.cell(cx, y).filter(|c| c.width != 0).map(|c| (cx, c)));
            if let Some((cx, cell)) = last {
                let _ = write!(out, "\x1b[{};{}H", y + 1, cx + 1);
                set_style(out, style, cell.style, caps);
                out.push_str(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                });
            }
            out.push_str("\x1b[0m");
        }
        Some((x, y)) => {
            out.push_str("\x1b[0m");
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
        }
        None => out.push_str("\x1b[0m"),
    }
}
fn set_style(out: &mut String, current: &mut Style, wanted: Style, caps: Caps) {
    if *current == wanted {
        return;
    }
    out.push_str("\x1b[0m");
    // tty_check_fg/bg: colours the terminal cannot show are downgraded; an
    // aixterm foreground on a terminal without them is bold instead.
    let depth = Depth {
        rgb: caps.rgb,
        colours: caps.palette_colours,
    };
    let (fg, bright) = colour::tty::check_fg(core_colour(wanted.fg), core_colour(wanted.bg), depth);
    let bg = colour::tty::check_bg(core_colour(wanted.bg), depth);
    for (on, code) in [
        (wanted.bold || bright, 1),
        (wanted.faint, 2),
        (wanted.italic, 3),
        (wanted.underline != Underline::None, 4),
        (wanted.blink, 5),
        (wanted.inverse, 7),
        (wanted.invisible, 8),
        (wanted.strikethrough, 9),
    ] {
        if on {
            let _ = write!(out, "\x1b[{code}m");
        }
    }
    color(out, fg, false);
    color(out, bg, true);
    *current = wanted;
}
/// A cell colour as `tty_check_fg`/`tty_check_bg` take it: palette 0-7 and
/// 8-15 are the plain and aixterm colours.
fn core_colour(c: Color) -> Colour {
    match c {
        Color::Default => Colour::Default,
        Color::Palette(n @ 0..=7) => Colour::Ansi(n),
        Color::Palette(n @ 8..=15) => Colour::Bright(n - 8),
        Color::Palette(n) => Colour::Palette(n),
        Color::Rgb(r, g, b) => Colour::Rgb(r, g, b),
    }
}
/// The SGR parameters of a colour already fitted to the terminal.
fn color(out: &mut String, c: Colour, bg: bool) {
    let base = if bg { 40 } else { 30 };
    match c {
        Colour::Ansi(n) => {
            let _ = write!(out, "\x1b[{}m", base + u16::from(n));
        }
        Colour::Bright(n) => {
            let _ = write!(out, "\x1b[{}m", base + 60 + u16::from(n));
        }
        Colour::Palette(n) => {
            let _ = write!(out, "\x1b[{};5;{n}m", base + 8);
        }
        Colour::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[{};2;{r};{g};{b}m", base + 8);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_and_sync_are_one_final_picture() {
        let caps = Caps {
            rgb: false,
            palette_colours: 256,
            theme_palette: false,
            sync: true,
            bce: true,
            xterm: true,
            colours: 256,
            overlay: false,
        };
        let mut pane = vtmux_vt::PaneTerm::new(4, 2, 0);
        pane.feed(b"abc");
        let mut screen = vtmux_vt::snapshot(&pane);
        screen.cells[0].text = "O".into(); // opaque overlay
        let mut compositor = Compositor::default();
        let frame = compositor.frame(screen.clone(), caps);
        assert!(frame.starts_with(b"\x1b[?2026h\x1b[?25l"));
        assert!(frame.ends_with(b"\x1b[?2026l"));
        assert!(!frame.windows(3).any(|x| x == b"abc"));
        assert_eq!(compositor.frame(screen.clone(), caps), Vec::new());
        screen.cells[1].text = "X".into();
        let frame = compositor.frame(screen, caps);
        assert!(frame.contains(&b'X'));
        assert!(!frame.contains(&b'O'));
    }

    /// tty.c:82-83 thresholds for an 80x24 client: block at 15361 queued
    /// bytes, unblock under 241, then one full frame, not a diff.
    #[test]
    fn blocks_at_start_and_redraws_fully_under_stop() {
        let caps = Caps {
            rgb: false,
            palette_colours: 256,
            theme_palette: false,
            sync: false,
            bce: true,
            xterm: true,
            colours: 256,
            overlay: false,
        };
        let mut pane = vtmux_vt::PaneTerm::new(80, 24, 0);
        pane.feed(b"one");
        let mut c = Compositor::default();
        assert!(c.ready(15_360, 80, 24));
        let first = c.frame(vtmux_vt::snapshot(&pane), caps);
        assert!(!c.ready(15_361, 80, 24));
        assert!(!c.ready(241, 80, 24), "still blocked until under STOP");
        pane.feed(b"\r\ntwo");
        let screen = vtmux_vt::snapshot(&pane);
        assert!(c.ready(240, 80, 24));
        assert!(c.ready(15_360, 80, 24), "unblocked again");
        let redraw = c.frame(screen.clone(), caps);
        let fresh = Compositor::default().frame(screen, caps);
        assert_eq!(redraw, fresh, "unblocking redraws every cell");
        assert_ne!(redraw, first);
    }
}
