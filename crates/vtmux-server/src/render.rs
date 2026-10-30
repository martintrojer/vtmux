//! Compose the final picture before diffing it (D14). Never stream pane
//! output directly: overlays and status are in the same snapshot.
//!
//! A window is drawn in two steps, as tmux's screen-redraw.c does: a
//! [`scene::Scene`] says what every cell is (pane, border, pane status line,
//! fill), then [`compose::compose`] draws it with the styles the model
//! gives ([`window::Source`]).
use std::fmt::Write as _;

pub mod compose;
pub mod scene;
pub mod window;
use vtmux_core::protocol::Hello;
use vtmux_vt::{Cell, Color, Snapshot, Style, Underline};

/// M0 terminfo-free table for the three supported terminal families. Full
/// overrides, theme detection and extended styles are M1 (D8).
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent terminal capabilities"
)]
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub rgb: bool,
    pub palette: bool,
    pub theme_palette: bool,
    pub sync: bool,
    pub bce: bool,
    pub xterm: bool,
}
impl Caps {
    #[must_use]
    pub fn from_hello(hello: &Hello) -> Option<Self> {
        let tty = hello.tty.as_ref()?;
        let xterm = tty.term.starts_with("xterm");
        if !xterm
            && !matches!(
                tty.term.as_str(),
                "tmux-256color" | "screen" | "screen-256color"
            )
        {
            return None;
        }
        let color = hello
            .env
            .iter()
            .find(|(k, _)| k == "COLORTERM")
            .map_or("", |(_, v)| v.as_str());
        let feature = |s: &str| {
            tty.features
                .iter()
                .any(|f| f.split([',', ':']).any(|x| x == s))
        };
        let rgb = color.eq_ignore_ascii_case("truecolor") || color == "24bit" || feature("RGB");
        let theme_palette = rgb || color.contains("256") || feature("256");
        Some(Self {
            rgb,
            palette: tty.term.contains("256") || theme_palette,
            theme_palette,
            sync: feature("sync"),
            bce: xterm,
            xterm,
        })
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
    /// `server_client_update_theme_colours` + `format_cb_client_colours`: a
    /// plain xterm-256color reports 16 unless the 256/RGB feature is set.
    #[must_use]
    pub fn status_style(self) -> Style {
        Style {
            fg: if self.theme_palette {
                Color::Rgb(13, 13, 13)
            } else {
                Color::Palette(0)
            },
            bg: if self.theme_palette {
                Color::Rgb(154, 205, 50)
            } else {
                Color::Palette(2)
            },
            ..Style::default()
        }
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
        out.push_str("\x1b[0m");
        if let Some((x, y)) = screen.cursor {
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
        }
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
fn set_style(out: &mut String, current: &mut Style, wanted: Style, caps: Caps) {
    if *current == wanted {
        return;
    }
    out.push_str("\x1b[0m");
    for (on, code) in [
        (wanted.bold, 1),
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
    color(out, wanted.fg, false, caps);
    color(out, wanted.bg, true, caps);
    *current = wanted;
}
fn color(out: &mut String, c: Color, bg: bool, caps: Caps) {
    let base = if bg { 40 } else { 30 };
    match c {
        Color::Default => {}
        Color::Palette(n) if n < 8 => {
            let _ = write!(out, "\x1b[{}m", base + u16::from(n));
        }
        Color::Palette(n) if n < 16 => {
            let _ = write!(out, "\x1b[{}m", base + 60 + u16::from(n) - 8);
        }
        Color::Palette(n) => {
            let n = if caps.palette { n } else { n % 8 };
            let _ = write!(out, "\x1b[{};5;{n}m", base + 8);
        }
        Color::Rgb(r, g, b) if caps.rgb => {
            let _ = write!(out, "\x1b[{};2;{r};{g};{b}m", base + 8);
        }
        Color::Rgb(r, g, b) => color(out, Color::Palette(rgb_palette(r, g, b)), bg, caps),
    }
}
#[allow(clippy::many_single_char_names)] // RGB components and cube coordinates
fn rgb_palette(r: u8, g: u8, b: u8) -> u8 {
    // colour.c colour_find_rgb: closest cube or grey.
    let cube = |v: u8| {
        if v < 48 {
            0
        } else if v < 114 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let q = [cube(r), cube(g), cube(b)];
    let levels = [0i32, 95, 135, 175, 215, 255];
    let dist = |red: i32, green: i32, blue: i32| {
        (red - i32::from(r)).pow(2) + (green - i32::from(g)).pow(2) + (blue - i32::from(b)).pow(2)
    };
    let avg = (i32::from(r) + i32::from(g) + i32::from(b)) / 3;
    let grey = ((avg - 3) / 10).clamp(0, 23);
    let v = 8 + 10 * grey;
    if dist(v, v, v)
        < dist(
            levels[q[0] as usize],
            levels[q[1] as usize],
            levels[q[2] as usize],
        )
    {
        232 + u8::try_from(grey).unwrap()
    } else {
        16 + 36 * q[0] + 6 * q[1] + q[2]
    }
}

/// Put the default status directly on the bottom row. Full status-format is
/// M1. Limits/order: options-table.c status-left/right/window-status
/// defaults; layout: format-draw.c `format_draw_left` (`status-justify left`).
pub fn status(
    screen: &mut Snapshot,
    left: &str,
    windows: &[(String, bool)],
    right: &str,
    caps: Caps,
) {
    let Some(y) = screen.rows.checked_sub(1) else {
        return;
    };
    let cols = usize::from(screen.cols);
    let base = caps.status_style();
    let row = &mut screen.cells[usize::from(y) * cols..][..cols];
    row.fill(Cell {
        text: String::new(),
        width: 1,
        style: Style {
            fg: Color::Default,
            ..base
        },
        ..Cell::default()
    });
    // `#{T;=/#{status-left-length}:status-left}`, status-left-length 10.
    let left = cells(&vtmux_core::format::trim::trim_left(left, 10), base);
    let right = cells(right, base);
    // The window list, `window-status-separator` " " between entries; the
    // current entry is the list focus (`list=focus`).
    let mut list = Vec::new();
    let mut focus = (0, 0);
    for (i, (text, current)) in windows.iter().enumerate() {
        if i > 0 {
            list.extend(cells(" ", base));
        }
        let style = Style {
            underline: if *current {
                Underline::Single
            } else {
                Underline::None
            },
            ..base
        };
        let start = list.len();
        list.extend(cells(text, style));
        if *current {
            focus = (start, list.len());
        }
    }
    // Trim first the list, then the right, then the left (no centre or
    // after-list section in the default format).
    let (mut width_left, mut width_list, mut width_right) = (left.len(), list.len(), right.len());
    while width_left + width_list + width_right > cols {
        if width_list > 0 {
            width_list -= 1;
        } else if width_right > 0 {
            width_right -= 1;
        } else {
            width_left -= 1;
        }
    }
    put(row, 0, &left, 0, width_left);
    put(
        row,
        cols - width_right,
        &right,
        right.len() - width_right,
        width_right,
    );
    if width_list > 0 {
        put_list(row, width_left, width_list, &list, focus, base);
    }
}

/// One status section as cells: a wide character is followed by a
/// zero-width continuation, combining marks join the previous character.
fn cells(text: &str, style: Style) -> Vec<Cell> {
    use unicode_width::UnicodeWidthChar;
    let mut out: Vec<Cell> = Vec::new();
    for c in text.chars() {
        match c.width().unwrap_or(0) {
            0 => {
                if let Some(base) = out.iter_mut().rev().find(|cell| cell.width != 0) {
                    base.text.push(c);
                }
            }
            w => {
                out.push(Cell {
                    text: c.to_string(),
                    width: u8::try_from(w).unwrap_or(1),
                    style,
                    ..Cell::default()
                });
                if w == 2 {
                    out.push(Cell {
                        text: String::new(),
                        width: 0,
                        style,
                        ..Cell::default()
                    });
                }
            }
        }
    }
    out
}

/// `format_draw_put`: copy `width` cells of `src` from `start` to `offset`.
/// A wide character cut in half becomes a space.
fn put(row: &mut [Cell], offset: usize, src: &[Cell], start: usize, width: usize) {
    for i in 0..width {
        let mut cell = src[start + i].clone();
        let cut = (i == 0 && cell.width == 0) || (i + 1 == width && cell.width == 2);
        if cut {
            cell.text = " ".into();
            cell.width = 1;
        }
        row[offset + i] = cell;
    }
}

/// `format_draw_put_list`: a trimmed list keeps the focus in view and marks
/// the hidden sides with `<` and `>` (`list=left-marker`/`right-marker`).
fn put_list(
    row: &mut [Cell],
    mut offset: usize,
    mut width: usize,
    list: &[Cell],
    focus: (usize, usize),
    style: Style,
) {
    if width >= list.len() {
        put(row, offset, list, 0, list.len());
        return;
    }
    let centre = focus.0 + (focus.1 - focus.0) / 2;
    let mut start = centre.saturating_sub(width / 2).min(list.len() - width);
    let marker = |text: &str| Cell {
        text: text.into(),
        width: 1,
        style,
        ..Cell::default()
    };
    if start != 0 && width > 1 {
        row[offset] = marker("<");
        offset += 1;
        start += 1;
        width -= 1;
    }
    if start + width < list.len() && width > 1 {
        row[offset + width - 1] = marker(">");
        width -= 1;
    }
    put(row, offset, list, start, width);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_and_sync_are_one_final_picture() {
        let caps = Caps {
            rgb: false,
            palette: true,
            theme_palette: false,
            sync: true,
            bce: true,
            xterm: true,
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
            palette: true,
            theme_palette: false,
            sync: false,
            bce: true,
            xterm: true,
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

    fn status_row(cols: u16, windows: &[(&str, bool)], right: &str) -> (String, Snapshot) {
        let caps = Caps {
            rgb: false,
            palette: true,
            theme_palette: false,
            sync: false,
            bce: true,
            xterm: true,
        };
        let mut screen = vtmux_vt::snapshot(&vtmux_vt::PaneTerm::new(cols, 2, 0));
        let windows: Vec<_> = windows.iter().map(|(t, c)| ((*t).to_owned(), *c)).collect();
        status(&mut screen, "[a] ", &windows, right, caps);
        let text = (0..cols)
            .map(|x| screen.cell(x, 1).unwrap())
            .filter(|c| c.width != 0)
            .map(|c| if c.text.is_empty() { " " } else { &c.text })
            .collect();
        (text, screen)
    }

    /// Reference tmux 3.8 status rows (outer tmux observer, /tmp/rv26/obs3.sh):
    /// the list is trimmed first, then the right, then the left
    /// (format-draw.c `format_draw_left`).
    #[test]
    fn narrow_status_trims_list_then_right_then_left() {
        let right = "\"t\" 07:50 30-Oct-26";
        let one = [("0:w*", true)];
        assert_eq!(status_row(20, &one, right).0, "[a]  07:50 30-Oct-26");
        assert_eq!(status_row(12, &one, right).0, "[a] 0-Oct-26");
        assert_eq!(status_row(24, &one, right).0, "[a] w\"t\" 07:50 30-Oct-26");
        let long = "\"aaaaaaaaaaaaaaaaaaaaa\" 07:50 30-Oct-26";
        assert_eq!(
            status_row(40, &one, long).0,
            "[a] aaaaaaaaaaaaaaaaaaa\" 07:50 30-Oct-26"
        );
        assert_eq!(status_row(3, &one, right).0, "[a]");
        assert_eq!(status_row(1, &one, right).0, "[");
    }

    /// `format_draw_put_list`: the focus stays in view, `<`/`>` mark the cut.
    #[test]
    fn trimmed_window_list_has_markers() {
        let right = "\"t\" 07:50 30-Oct-26";
        let two = [("0:w*", true), ("1:second", false)];
        // Reference at 30x5: `[a] 0:w* 1>"t" ...`.
        let (text, screen) = status_row(30, &two, right);
        assert_eq!(text, "[a] 0:w* 1>\"t\" 07:50 30-Oct-26");
        assert_eq!(
            screen.cell(4, 1).unwrap().style.underline,
            Underline::Single
        );
        assert_eq!(screen.cell(10, 1).unwrap().style.underline, Underline::None);
        // Reference rows (3.8, outer-tmux observer, all titles `t`).
        for (cols, windows, row) in [
            (
                30,
                &[("0:w", false), ("1:second*", true)][..],
                "[a] <secon>",
            ),
            (
                31,
                &[("0:a", false), ("1:bbbbbbb*", true), ("2:c", false)],
                "[a] <bbbbbb>",
            ),
            (
                27,
                &[("0:a", false), ("1:bbbbbbb*", true), ("2:c", false)],
                "[a] <bb>",
            ),
            (
                26,
                &[("0:a", false), ("1:bbbbbbb*", true), ("2:c", false)],
                "[a] <b>",
            ),
            (
                27,
                &[("0:a", false), ("1:b", false), ("2:c*", true)],
                "[a] <:c*",
            ),
        ] {
            assert_eq!(status_row(cols, windows, right).0, format!("{row}{right}"));
        }
    }
}
