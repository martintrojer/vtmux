//! Clock mode (window-clock.c): the time in big `#` glyphs drawn from
//! `window_clock_table`, or as plain text when the pane is too small,
//! redrawn every second. Any key from a client leaves it
//! (`window_clock_key`).
//!
//! The time comes from [`now`], which a debug build lets `VTMUX_FAKE_TIME`
//! (epoch seconds) pin, so tests can compare exact glyphs. Release builds
//! never read it.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vtmux_core::ids::PaneId;
use vtmux_core::time::{SystemWallClock, Tm, WallClock};
use vtmux_vt::{Cell, Color, Snapshot};

use super::{Mode, PaneMode, copy};
use crate::model::Model;
use crate::server::Server;

/// `window_clock_table`: 0-9, `:`, `A`, `P`, `M`; each glyph five rows of
/// five cells, `#` for 1 and `.` for 0.
pub const GLYPHS: [[&str; 5]; 14] = [
    // 0
    ["#####", "#...#", "#...#", "#...#", "#####"],
    // 1
    ["....#", "....#", "....#", "....#", "....#"],
    // 2
    ["#####", "....#", "#####", "#....", "#####"],
    // 3
    ["#####", "....#", "#####", "....#", "#####"],
    // 4
    ["#...#", "#...#", "#####", "....#", "....#"],
    // 5
    ["#####", "#....", "#####", "....#", "#####"],
    // 6
    ["#####", "#....", "#####", "#...#", "#####"],
    // 7
    ["#####", "....#", "....#", "....#", "....#"],
    // 8
    ["#####", "#...#", "#####", "#...#", "#####"],
    // 9
    ["#####", "#...#", "#####", "....#", "#####"],
    // :
    [".....", "..#..", ".....", "..#..", "....."],
    // A
    ["#####", "#...#", "#####", "#...#", "#...#"],
    // P
    ["#####", "#...#", "#####", "#....", "#...."],
    // M
    ["#...#", "##.##", "#.#.#", "#...#", "#...#"],
];

/// `clock-mode-style` as the options table numbers it.
const STYLE_12: i64 = 0;
const STYLE_12_SECONDS: i64 = 2;
const STYLE_24_SECONDS: i64 = 3;

/// One clock mode on a pane's stack (`struct window_clock_mode_data`).
#[derive(Clone, Debug)]
pub struct ClockMode {
    /// The key mode of the mode's screen (`#{pane_key_mode}`).
    pub key_mode: vtmux_vt::KeyMode,
    sx: usize,
    sy: usize,
    /// `data->tim`: the time last drawn, to see a new second.
    tim: i64,
    /// `data->timer`: when the next second boundary is.
    due: Instant,
}

/// Seconds since the epoch: `time(NULL)`, or `VTMUX_FAKE_TIME` in a debug
/// build.
#[must_use]
pub fn now() -> i64 {
    if cfg!(debug_assertions)
        && let Some(t) = std::env::var("VTMUX_FAKE_TIME")
            .ok()
            .and_then(|v| v.parse().ok())
    {
        return t;
    }
    SystemWallClock.now()
}

/// `window_clock_start_timer`: the time to the next whole second.
fn until_next_second() -> Duration {
    let sub = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_micros());
    Duration::from_micros(u64::from(1_000_000 - sub))
}

/// The text `window_clock_draw_screen` shows for `tm` in `style`.
#[must_use]
pub fn time_text(tm: &Tm, style: i64) -> String {
    if style == STYLE_12 || style == STYLE_12_SECONDS {
        let fmt = if style == STYLE_12_SECONDS {
            "%l:%M:%S "
        } else {
            "%l:%M "
        };
        let ampm = if tm.hour >= 12 { "PM" } else { "AM" };
        format!("{}{ampm}", tm.strftime(fmt))
    } else if style == STYLE_24_SECONDS {
        tm.strftime("%H:%M:%S")
    } else {
        tm.strftime("%H:%M")
    }
}

impl ClockMode {
    #[must_use]
    pub fn new(sx: usize, sy: usize) -> Self {
        Self {
            key_mode: vtmux_vt::KeyMode::Standard,
            sx,
            sy,
            tim: now(),
            due: Instant::now() + until_next_second(),
        }
    }

    /// `window_clock_resize`.
    pub fn resize(&mut self, sx: usize, sy: usize) {
        self.sx = sx;
        self.sy = sy;
    }

    /// `window_clock_draw_screen`: `text` in `colour`, big if it fits.
    #[must_use]
    pub fn screen(&self, text: &str, colour: Color) -> Snapshot {
        let (sx, sy) = (self.sx, self.sy);
        let blank = Cell {
            width: 1,
            ..Cell::default()
        };
        let mut cells = vec![blank; sx * sy];
        let len = text.len();
        // Where the last screen write left the cursor, which tmux hides but
        // still moves the terminal's cursor to.
        let mut cursor = (0, 0);
        if sx < 6 * len || sy < 6 {
            if sx >= len && sy != 0 {
                let (x, y) = (sx / 2 - len / 2, sy / 2);
                for (i, ch) in text.chars().enumerate() {
                    let c = &mut cells[y * sx + x + i];
                    c.text = ch.to_string();
                    c.style.fg = colour;
                }
                cursor = (x + len, y);
            }
        } else {
            let (mut x, y) = (sx / 2 - 3 * len, sy / 2 - 3);
            for ch in text.chars() {
                let idx = match ch {
                    '0'..='9' => Some(usize::from(ch as u8 - b'0')),
                    ':' => Some(10),
                    'A' => Some(11),
                    'P' => Some(12),
                    'M' => Some(13),
                    _ => None,
                };
                if let Some(idx) = idx {
                    for (j, row) in GLYPHS[idx].iter().enumerate() {
                        for (i, b) in row.bytes().enumerate() {
                            cursor = (x + i, y + j);
                            if b == b'#' {
                                cursor.0 += 1;
                                let c = &mut cells[(y + j) * sx + x + i];
                                c.text = "#".into();
                                c.style.fg = colour;
                                c.style.bg = colour;
                            }
                        }
                    }
                }
                x += 6;
            }
        }
        let clamp = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        Snapshot {
            cols: clamp(sx),
            rows: clamp(sy),
            cells,
            wrapped: vec![false; sy],
            cursor: Some((clamp(cursor.0.min(sx.saturating_sub(1))), clamp(cursor.1))),
            cursor_visible: false,
        }
    }
}

impl PaneMode for ClockMode {
    fn name(&self) -> &'static str {
        "clock-mode"
    }

    /// No key table: any key leaves the mode.
    fn key_table(&self, _vi: bool) -> Option<&'static str> {
        None
    }

    /// No `command` callback: `send-keys -X` is "not in a mode".
    fn takes_commands(&self) -> bool {
        false
    }

    fn set_prefix(&mut self, _n: u32) {}

    fn command(
        &mut self,
        _env: &copy::Env,
        name: &str,
        _args: &[String],
        _read_only_client: bool,
    ) -> Result<copy::Outcome, copy::CommandError> {
        Err(copy::CommandError::NotImplemented(name.to_owned()))
    }
}

impl Model {
    /// `window_pane_set_mode` for clock mode: one already on top is left
    /// alone, one further down comes to the top.
    pub fn set_clock_mode(&mut self, p: PaneId) -> bool {
        let key_mode = super::screen_key_mode(self);
        let Some(pane) = self.panes.get_mut(&p) else {
            return false;
        };
        if matches!(pane.modes.first(), Some(Mode::Clock(_))) {
            return false;
        }
        let entry = if let Some(i) = pane.modes.iter().position(|m| matches!(m, Mode::Clock(_))) {
            pane.modes.remove(i)
        } else {
            let mut m =
                ClockMode::new(usize::from(pane.term.cols()), usize::from(pane.term.rows()));
            m.key_mode = key_mode;
            Mode::Clock(m)
        };
        let previous = pane.modes.first().map(Mode::name);
        let name = entry.name();
        pane.modes.insert(0, entry);
        self.mode_changed(p);
        self.fire_mode_changed(p, previous, Some(name), true);
        true
    }
}

impl Server {
    /// `window_clock_key`: a key from a client leaves a clock on top.
    pub(super) fn clock_key(&mut self, p: PaneId) {
        self.reset_mode(p);
    }

    /// The soonest second boundary of a pane with a clock mode.
    pub(crate) fn clock_deadline(&self) -> Option<Instant> {
        self.clocks().map(|(_, due)| due).min()
    }

    fn clocks(&self) -> impl Iterator<Item = (PaneId, Instant)> + '_ {
        self.model.panes.iter().flat_map(|(&id, pane)| {
            pane.modes.iter().filter_map(move |m| {
                let Mode::Clock(c) = m else { return None };
                Some((id, c.due))
            })
        })
    }

    /// `window_clock_timer_callback`: at a second boundary, a pane whose
    /// clock sees a new second is redrawn, and the timer is armed again.
    pub(crate) fn clock_timers(&mut self, at: Instant) {
        let t = now();
        let due: Vec<PaneId> = self
            .clocks()
            .filter(|&(_, d)| d <= at)
            .map(|(id, _)| id)
            .collect();
        for p in due {
            let Some(pane) = self.model.panes.get_mut(&p) else {
                continue;
            };
            let mut redraw = false;
            for m in &mut pane.modes {
                if let Mode::Clock(c) = m
                    && c.due <= at
                {
                    // gmtime_r tm_sec differs.
                    if c.tim.rem_euclid(60) != t.rem_euclid(60) {
                        c.tim = t;
                        redraw = true;
                    }
                    c.due = at + until_next_second();
                }
            }
            if redraw {
                self.model.redraw_panes.insert(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// window-clock.c's `window_clock_table`, verbatim (0/1 rows).
    const C_TABLE: &str = "
        11111 10001 10001 10001 11111
        00001 00001 00001 00001 00001
        11111 00001 11111 10000 11111
        11111 00001 11111 00001 11111
        10001 10001 11111 00001 00001
        11111 10000 11111 00001 11111
        11111 10000 11111 10001 11111
        11111 00001 00001 00001 00001
        11111 10001 11111 10001 11111
        11111 10001 11111 00001 11111
        00000 00100 00000 00100 00000
        11111 10001 11111 10001 10001
        11111 10001 11111 10000 10000
        10001 11011 10101 10001 10001";

    #[test]
    fn glyph_table_is_the_c_one() {
        for (glyph, line) in GLYPHS.iter().zip(C_TABLE.trim().lines()) {
            let want: Vec<String> = line
                .split_whitespace()
                .map(|r| r.replace('1', "#").replace('0', "."))
                .collect();
            assert_eq!(glyph.to_vec(), want);
        }
    }

    fn at(epoch: i64) -> Tm {
        Tm::utc(epoch)
    }

    #[test]
    fn text_per_style() {
        // 2026-11-01 13:07:45 UTC and 00:05:09 UTC.
        let (pm, am) = (at(1_793_538_465), at(1_793_491_509));
        assert_eq!(time_text(&pm, 0), " 1:07 PM");
        assert_eq!(time_text(&pm, 1), "13:07");
        assert_eq!(time_text(&pm, 2), " 1:07:45 PM");
        assert_eq!(time_text(&pm, 3), "13:07:45");
        assert_eq!(time_text(&am, 0), "12:05 AM");
        assert_eq!(time_text(&am, 3), "00:05:09");
    }

    fn rows(s: &Snapshot) -> Vec<String> {
        (0..s.rows)
            .map(|y| {
                (0..s.cols)
                    .map(|x| {
                        let t = &s.cell(x, y).unwrap().text;
                        if t.is_empty() { " " } else { t }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// The rows `window_clock_draw_screen` writes for `text` at `x0, y0`,
    /// built from the 0/1 C table by its own loops.
    fn expected(text: &str, (sx, sy): (usize, usize), (x0, y0): (usize, usize)) -> Vec<String> {
        let table: Vec<Vec<&str>> = C_TABLE
            .trim()
            .lines()
            .map(|l| l.split_whitespace().collect())
            .collect();
        let mut grid = vec![vec![' '; sx]; sy];
        let mut x = x0;
        for ch in text.chars() {
            let idx = match ch {
                '0'..='9' => ch as usize - '0' as usize,
                ':' => 10,
                'A' => 11,
                'P' => 12,
                'M' => 13,
                _ => {
                    x += 6;
                    continue;
                }
            };
            for j in 0..5 {
                for i in 0..5 {
                    if table[idx][j].as_bytes()[i] == b'1' {
                        grid[y0 + j][x + i] = '#';
                    }
                }
            }
            x += 6;
        }
        grid.iter()
            .map(|r| r.iter().collect::<String>().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn large_clock_glyphs_are_centred_and_coloured() {
        let colour = Color::Palette(4);
        for (size, text) in [
            ((30, 7), "13:07"),
            ((40, 10), "13:07"),
            ((80, 24), " 1:07:45 PM"),
            ((80, 24), "00:05:09"),
        ] {
            let s = ClockMode::new(size.0, size.1).screen(text, colour);
            let at = (size.0 / 2 - 3 * text.len(), size.1 / 2 - 3);
            assert_eq!(rows(&s), expected(text, size, at), "{text} in {size:?}");
            for c in s.cells.iter().filter(|c| c.text == "#") {
                assert_eq!((c.style.fg, c.style.bg), (colour, colour));
            }
        }
    }

    #[test]
    fn small_pane_gets_the_text_in_the_colour() {
        // 29 < 6 * 5: too narrow for glyphs.
        let s = ClockMode::new(29, 7).screen("13:07", Color::Palette(1));
        let mut want = vec![String::new(); 7];
        want[3] = format!("{}13:07", " ".repeat(29 / 2 - 5 / 2));
        assert_eq!(rows(&s), want);
        let c = s.cell(29 / 2 - 5 / 2, 3).unwrap();
        assert_eq!(
            (c.style.fg, c.style.bg),
            (Color::Palette(1), Color::Default)
        );
        // Too short: 5 rows.
        let s = ClockMode::new(80, 5).screen("13:07", Color::Default);
        assert_eq!(rows(&s)[2].trim(), "13:07");
        // Too small for the text itself: nothing.
        let s = ClockMode::new(4, 5).screen("13:07", Color::Default);
        assert!(rows(&s).iter().all(String::is_empty));
        let empty = ClockMode::new(10, 0).screen("13:07", Color::Default);
        assert_eq!(empty.cells, []);
    }
}
