//! Compose the final picture before diffing it (D14). Never stream pane
//! output directly: overlays and status are in the same snapshot.
//!
//! A window is drawn in two steps, as tmux's screen-redraw.c does: a
//! [`scene::Scene`] says what every cell is (pane, border, pane status line,
//! fill), then [`compose::compose`] draws it with the styles the model
//! gives ([`window::Source`]).
use std::fmt::Write as _;

pub mod blend;
pub mod compose;
pub mod menu;
pub mod panes;
pub mod popup;
pub mod scene;
pub mod status;
pub mod tree;
pub mod window;
use vtmux_core::colour::{self, Colour, tty::Depth};
use vtmux_core::protocol::Hello;
use vtmux_core::term_features::{Cap, OpenError, Term};
use vtmux_vt::{Color, Snapshot, Style, Underline};

/// What the compositor needs of a client's terminal, from its
/// [`Term`] (terminfo-free, D8): `terminal-features`, `terminal-overrides`,
/// `-T` and `COLORTERM` have already been applied to it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent terminal capabilities"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub rgb: bool,
    /// `Term::palette_size`: what `tty_check_fg` believes the terminal has.
    pub palette_colours: u32,
    pub theme_palette: bool,
    pub sync: bool,
    /// terminfo's `bce` (`tty_term_flag(TTYC_BCE)`, tty.c:1097).
    pub bce: bool,
    /// The terminal has `setaf` and `setab`, which palette colours are
    /// written with (`tty_colours_fg`, `tty_colours_bg`).
    pub setaf: bool,
    pub setab: bool,
    /// `#{client_colours}`.
    pub colours: u32,
    /// An overlay (a popup) is up: `tty_draw_line_clear` writes spaces,
    /// never erases (`overlay_check != NULL`, tty-draw.c:61).
    pub overlay: bool,
    /// `TERM_NOAM`: the terminal has no `am`, so the bottom-right cell is
    /// never written (`tty_cell`, tty.c:2259).
    pub no_am: bool,
    /// The client's theme is light (`c->theme`, else its background's;
    /// [`crate::term_queries::Reported::light`]): the theme colours come
    /// from the `light-theme-*` options (`server_client_update_theme_colours`).
    /// Not a terminal capability, but per client and part of what every
    /// drawing of it depends on, as caps are.
    pub light_theme: bool,
    /// `REDRAW_ISOLATES` (screen-redraw.c:1620-1621): the client is UTF-8
    /// (`CLIENT_UTF8`) and its terminal has `Bidi`, so vertical borders
    /// are drawn between bidi isolates.
    pub isolates: bool,
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

    /// `tty_term_create` for a client's hello: `None` for a client
    /// without a terminal.
    ///
    /// # Errors
    ///
    /// Why the terminal cannot be opened ([`OpenError`]).
    #[must_use]
    pub fn open_term(
        hello: &Hello,
        features: &[String],
        overrides: &[String],
    ) -> Option<Result<(Term, Self), OpenError>> {
        let tty = hello.tty.as_ref()?;
        let colorterm = hello
            .env
            .iter()
            .find(|(k, _)| k == "COLORTERM")
            .map(|(_, v)| v.as_str());
        Some(
            Term::open(&tty.term, &tty.features, colorterm, features, overrides).map(|term| {
                let caps = Self::from_term(&term);
                (term, caps)
            }),
        )
    }
    #[must_use]
    pub fn from_term(term: &Term) -> Self {
        Self {
            rgb: term.rgb(),
            palette_colours: term.palette_size(),
            theme_palette: term.colours256(),
            sync: term.has(Cap::Sync),
            bce: term.flag("bce"),
            setaf: term.has_name("setaf"),
            setab: term.has_name("setab"),
            colours: term.colours(),
            overlay: false,
            no_am: !term.flag("am"),
            light_theme: false,
            isolates: term.feature_present("utf8") && term.has_name("Bidi"),
        }
    }
}

/// `tty_start_tty`, then `tty_send_requests` (tty.c:406-424): DA, DA2,
/// XTVERSION, the synchronised-update DECRQM and the default colours (OSC 10
/// and 11); then, vtmux-only, palette entries 0-15 (OSC 4) for the pane
/// blend (D26, [`crate::term_queries::colour_queries`]). tmux sends the
/// queries only to a VT100-like terminal; every terminal vtmux knows is one
/// (an override can make one that is not, which vtmux still queries).
/// `clear` is `clear-on-attach`; `rows` the terminal's height.
#[must_use]
pub fn startup(term: &Term, clear: bool, rows: u32) -> Vec<u8> {
    let mut out = restart(term, clear, rows);
    out.extend_from_slice(b"\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p");
    out.extend_from_slice(&crate::term_queries::colour_queries());
    out
}

/// `tty_start_tty` (tty.c:357-395) alone: a suspended client coming back.
/// `clear-on-attach` on writes `smcup` and `clear`; off, the terminal's
/// contents are scrolled up into its history instead: `csr`, `cup`, then
/// `indn` of one more than the height (`ind` that many times without it,
/// `clear` without either). tmux gives `cup` 0 and the last row in that
/// order, which terminfo takes as row then column: row 1, column `rows`.
/// Then `smkx`, `cnorm`, the mouse modes off (with `kmous`), `Enbp`, and
/// the theme subscription for a VT100-like terminal. `csr`, `cup` and
/// `indn` are the xterm forms every terminal vtmux knows has.
#[must_use]
pub fn restart(term: &Term, clear: bool, rows: u32) -> Vec<u8> {
    let s = |name| term.string(name).unwrap_or_default();
    let mut out = if clear {
        [s("smcup"), s("clear")].concat()
    } else {
        let rows = rows.max(1);
        let scroll = if term.has_name("indn") {
            format!("\x1b[{}S", rows + 1)
        } else if term.has_name("ind") {
            s("ind").repeat(rows as usize + 1)
        } else {
            s("clear").to_owned()
        };
        format!("\x1b[1;{rows}r\x1b[1;{rows}H{scroll}")
    };
    out.push_str(s("smkx"));
    out.push_str(s("cnorm"));
    if term.has(Cap::Kmous) {
        out.push_str("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l");
    }
    if term.has_name("Enbp") {
        out.push_str("\x1b[?2004h");
    }
    if term.vt100_like() {
        out.push_str("\x1b[?2031h\x1b[?996n");
    }
    out.into_bytes()
}

/// `server_lock_client`'s `tty_raw` calls (server-fn.c:211-214): `smcup`,
/// `clear` and `E3`.
#[must_use]
pub fn lock_screen(term: &Term) -> Vec<u8> {
    let s = |name| term.string(name).unwrap_or_default();
    [s("smcup"), s("clear"), s("E3")].concat().into_bytes()
}

/// `tty_stop_tty` (tty.c:470-520). `clear` is `clear-on-attach`: on, the
/// screen is cleared and `rmcup` leaves the alternate screen; off, there is
/// none to leave and the clear comes last. `csr` and `sgr0` are xterm's
/// forms; the focus reports and extended keys go off as `Dsfcs` and
/// `Dseks` say.
#[must_use]
pub fn teardown(term: &Term, clear: bool) -> Vec<u8> {
    let s = |name| term.string(name).unwrap_or_default();
    let mut out = String::from("\x1b[r\x1b[0m");
    out.push_str(s("rmkx"));
    if clear {
        out.push_str(s("clear"));
    }
    out.push_str(s("cnorm"));
    if term.has(Cap::Kmous) {
        out.push_str("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l");
    }
    if term.has_name("Dsbp") {
        out.push_str("\x1b[?2004l");
    }
    if term.has_name("Dsfcs") {
        out.push_str("\x1b[?1004l");
    }
    out.push_str(term.disable_extended_keys().unwrap_or_default());
    out.push_str(s(if clear { "rmcup" } else { "clear" }));
    if term.vt100_like() {
        out.push_str("\x1b[?2031l");
    }
    out.into_bytes()
}

/// The shortest gap between two frames to one client while a pane is
/// flooding (it still had output when its read budget ran out). Frames are
/// composed whole (D14), so their cost follows how often they are drawn,
/// not how much output arrived; tmux instead writes each pane update as it
/// happens. A frame that comes sooner waits for the rest of the interval
/// and then shows everything since. Without a flood, or when the frame
/// also carries a change that is not flood output (a key's echo in a quiet
/// pane, a mode or status redraw: [`Compositor::expedite`]), the frame goes
/// at once.
pub const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// What a client's last window drawing can lend the next one.
#[derive(Debug, Default)]
pub struct WindowCache {
    pub scene: scene::SceneCache,
    /// Each pane's border style as this client last expanded it.
    pub borders: window::BorderStyles,
    pub compose: std::cell::RefCell<compose::ComposeCache>,
    /// The window's drawing buffer, kept for the next frame.
    pub buffer: std::cell::RefCell<Option<Snapshot>>,
    /// What the last frame was drawn from, when it was a plain one (see
    /// [`PlainFrame`]); `None` otherwise.
    pub plain: std::cell::Cell<Option<PlainFrame>>,
    /// Each pane's border status line ranges as last drawn
    /// (`wp->border_status_line.ranges`), for the mouse.
    pub border_ranges: std::cell::RefCell<
        std::collections::BTreeMap<vtmux_core::ids::PaneId, Vec<vtmux_core::styled::StyleRange>>,
    >,
    /// Plain frames [`crate::server::Server`] skipped outright, for tests
    /// to tell the skip from the compositor's own empty diff.
    #[cfg(test)]
    pub skipped: std::cell::Cell<u32>,
}

/// The inputs of a frame with nothing over the window but the status
/// line: no popup, message, prompt or pane prompt, no frozen pane area.
/// When the next frame has the same, and the window composes to the same
/// rows and cursor ([`compose::compose_cached`]), it is the same frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlainFrame {
    pub caps: Caps,
    pub size: (u16, u16),
    /// The window's first row and height.
    pub area: (u16, u16),
    /// The window cursor ([`window::cursor_of`]), once drawn.
    pub cursor: (Option<(u16, u16)>, bool),
}

impl PlainFrame {
    /// Drawn from the same inputs, whatever the cursor.
    #[must_use]
    pub fn same_inputs(self, other: Self) -> bool {
        (self.caps, self.size, self.area) == (other.caps, other.size, other.area)
    }
}

/// `buffer` emptied for a `cols` x `rows` drawing, or a new one: reusing
/// it spares a frame the allocation and the first touch of its cells.
#[must_use]
pub fn blank(buffer: Option<Snapshot>, cols: u16, rows: u16) -> Snapshot {
    match buffer {
        Some(mut b) if (b.cols, b.rows) == (cols, rows) => {
            clear_cells(&mut b.cells);
            b.wrapped.clear();
            b.cursor = None;
            b.cursor_visible = true;
            b
        }
        _ => Snapshot {
            cols,
            rows,
            cells: vec![vtmux_vt::Cell::default(); usize::from(cols) * usize::from(rows)],
            wrapped: Vec::new(),
            cursor: None,
            cursor_visible: true,
        },
    }
}

/// Empty `cells`, as [`blank`] leaves them.
pub fn clear_cells(cells: &mut [vtmux_vt::Cell]) {
    for c in cells {
        c.text.clear();
        c.width = 0;
        c.style = Style::default();
        c.acs = false;
        c.link = false;
        c.tab = false;
    }
}

#[derive(Default)]
pub struct Compositor {
    last: Option<Snapshot>,
    /// Per row of `last`, the composed row it was copied from
    /// ([`compose::ComposeCache::copy_rows`]); 0 for a row drawn otherwise.
    /// Two frames' rows with the same nonzero tag hold the same cells.
    last_tags: Vec<u64>,
    /// The frame before `last`, its buffer kept for the next one.
    spare: Option<Snapshot>,
    /// `spare`'s row tags, as `last_tags`.
    spare_tags: Vec<u64>,
    /// The client is not keeping up: compose nothing until it drains.
    blocked: bool,
    /// When the last frame was sent.
    sent: Option<std::time::Instant>,
    /// A frame was held back by [`FRAME_INTERVAL`] until then.
    due: Option<std::time::Instant>,
    /// The next frame carries a change that is not flood output, so the
    /// flood's pacing does not hold it back.
    expedited: bool,
    /// Cells the next frame writes even where the terminal already shows
    /// them, as (row, first column, count): the lines of a pane whose
    /// synchronized output ended ([`Self::force`]).
    forced: Vec<(u16, u16, u16)>,
}
impl Compositor {
    /// Whether a frame may be composed `now`, with a pane flooding or not.
    /// If not, the frame is due once the interval since the last one is
    /// over ([`Self::due`]).
    pub fn paced(&mut self, now: std::time::Instant, flood: bool) -> bool {
        let wait = self
            .sent
            .map(|t| t + FRAME_INTERVAL)
            .filter(|&d| flood && !self.expedited && d > now);
        self.due = wait;
        if wait.is_none() {
            self.expedited = false;
        }
        wait.is_none()
    }
    /// The next frame shows a change that is not flood output: it is not
    /// paced. The next frame [`Self::paced`] lets through uses it up.
    pub fn expedite(&mut self) {
        self.expedited = true;
    }
    /// `screen_write_flush_dirty` (screen-write.c:1267-1290): the next
    /// frame writes `n` cells of row `y` from column `x` whole, not as a
    /// diff against what the terminal shows, as tmux redraws each line of
    /// a pane written under synchronized output when it ends.
    pub fn force(&mut self, y: u16, x: u16, n: u16) {
        if n != 0 {
            self.forced.push((y, x, n));
        }
    }
    /// When a held-back frame is due: the loop wakes then.
    #[must_use]
    pub fn due(&self) -> Option<std::time::Instant> {
        self.due
    }
    pub fn invalidate(&mut self) {
        self.spare = self.last.take();
        self.forced.clear();
        self.spare_tags = std::mem::take(&mut self.last_tags);
    }
    /// The last frame sent is still what the terminal shows, at this size.
    #[must_use]
    pub fn shows(&self, cols: u16, rows: u16) -> bool {
        !self.blocked
            && self
                .last
                .as_ref()
                .is_some_and(|s| (s.cols, s.rows) == (cols, rows))
    }
    /// The last frame sent and its row tags.
    #[cfg(test)]
    #[must_use]
    pub fn last(&self) -> Option<(&Snapshot, &[u64])> {
        self.last.as_ref().map(|l| (l, self.last_tags.as_slice()))
    }
    /// Keep `screen` and its row tags, unsent, as the next frame's buffer.
    pub fn recycle(&mut self, screen: Snapshot, tags: Vec<u64>) {
        self.spare = Some(screen);
        self.spare_tags = tags;
    }
    /// A `cols` x `rows` screen for the next frame and its row tags: empty
    /// ([`blank`], tags 0) but for rows `keep`, which hold what they held
    /// in the frame before last, with its tags. The caller draws every
    /// kept row whose tag is not the one it wants
    /// ([`compose::ComposeCache::copy_rows`]).
    pub fn buffer(
        &mut self,
        cols: u16,
        rows: u16,
        keep: std::ops::Range<u16>,
    ) -> (Snapshot, Vec<u64>) {
        let mut tags = std::mem::take(&mut self.spare_tags);
        match self.spare.take() {
            Some(mut b) if (b.cols, b.rows) == (cols, rows) && tags.len() == usize::from(rows) => {
                let width = usize::from(cols).max(1);
                let lines = b.cells.chunks_mut(width).zip(tags.iter_mut());
                for (y, (row, tag)) in (0..rows).zip(lines) {
                    if !keep.contains(&y) {
                        clear_cells(row);
                        *tag = 0;
                    }
                }
                b.wrapped.clear();
                b.cursor = None;
                b.cursor_visible = true;
                (b, tags)
            }
            spare => (blank(spare, cols, rows), vec![0; usize::from(rows)]),
        }
    }
    /// Move the terminal's cursor back to where the last frame left it,
    /// after raw bytes (passthrough) may have moved it.
    #[must_use]
    pub fn cursor_bytes(&self, caps: Caps) -> Vec<u8> {
        let Some(last) = self.last.as_ref() else {
            return Vec::new();
        };
        let mut out = String::new();
        place_cursor(&mut out, &mut Style::default(), last, caps);
        out.into_bytes()
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
        // Only [`Self::paced`], later in the same frame, holds a frame back
        // by time; a blocked client waits for its acks instead.
        self.due = None;
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
    /// Per row of `screen`, whether it differs from what the terminal shows
    /// (every row when the size changed); `None` when nothing does, the
    /// cursor included.
    /// Rows tagged alike in both are equal without a look (see
    /// `last_tags`).
    fn changed_rows(&self, screen: &Snapshot, tags: &[u64], same_size: bool) -> Option<Vec<bool>> {
        let Some(last) = self.last.as_ref().filter(|_| same_size) else {
            return Some(vec![true; usize::from(screen.rows)]);
        };
        let width = usize::from(screen.cols).max(1);
        let mut rows: Vec<bool> = screen
            .cells
            .chunks(width)
            .zip(last.cells.chunks(width))
            .enumerate()
            .map(|(y, (a, b))| {
                let tag = tags.get(y).copied().unwrap_or(0);
                !(tag != 0 && self.last_tags.get(y) == Some(&tag)) && a != b
            })
            .collect();
        for &(y, _, _) in &self.forced {
            if let Some(r) = rows.get_mut(usize::from(y)) {
                *r = true;
            }
        }
        let same = (last.cursor, last.cursor_visible, &last.wrapped)
            == (screen.cursor, screen.cursor_visible, &screen.wrapped);
        (rows.contains(&true) || !same).then_some(rows)
    }
    /// One frame per tick. The caller supplies a fully composed screen;
    /// therefore changes behind opaque overlays cannot escape into a frame.
    pub fn frame(&mut self, screen: Snapshot, caps: Caps) -> Vec<u8> {
        let tags = vec![0; usize::from(screen.rows)];
        self.frame_tagged(screen, tags, caps)
    }
    /// [`Self::frame`] of a screen with row tags ([`Self::buffer`]).
    pub fn frame_tagged(&mut self, screen: Snapshot, tags: Vec<u64>, caps: Caps) -> Vec<u8> {
        let same_size = self
            .last
            .as_ref()
            .is_some_and(|s| (s.cols, s.rows) == (screen.cols, screen.rows));
        let Some(changed_rows) = self.changed_rows(&screen, &tags, same_size) else {
            self.spare = Some(screen);
            self.spare_tags = tags;
            return Vec::new();
        };
        let forced = std::mem::take(&mut self.forced);
        let is_forced = |x: u16, y: u16| {
            forced
                .iter()
                .any(|&(fy, fx, n)| fy == y && (fx..fx.saturating_add(n)).contains(&x))
        };
        let mut out = String::new();
        if caps.sync {
            out.push_str("\x1b[?2026h");
        }
        out.push_str("\x1b[?25l");
        let mut style = Style::default();
        out.push_str("\x1b[0m");
        // Where the terminal's cursor is, when known: a cell written there
        // needs no cursor move (tty_cursor skips one to where it already
        // is, tty.c:2468), so a changed run goes out as its text. Unknown
        // after the last column (the pending wrap) and after an erase.
        let mut at: Option<(u16, u16)> = None;
        for y in 0..screen.rows {
            if !changed_rows[usize::from(y)] {
                continue;
            }
            let mut x = 0;
            while x < screen.cols {
                let cell = screen.cell(x, y).expect("screen cell");
                let changed = !same_size
                    || is_forced(x, y)
                    || self.last.as_ref().and_then(|s| s.cell(x, y)) != Some(cell);
                if !changed || cell.width == 0 {
                    x += 1;
                    continue;
                }
                // tty_cell: a terminal without `am` would scroll.
                if caps.no_am && y + 1 == screen.rows && x + 1 == screen.cols {
                    break;
                }
                if at != Some((x, y)) {
                    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                }
                at = None;
                // tty-draw.c tty_draw_line_clear: a run of ten or more empty
                // cells is erased (EL to the end of the line, else ECH), not
                // written: erased cells keep only the background and hold no
                // text, which a terminal (and capture -e) can tell apart.
                // compose::draw_line has already decided which pane and pane
                // status cells tmux erases (empty text, background only);
                // borders and fill keep their text, so they never join a run.
                // An erased cell the terminal already shows erased in the
                // same style joins the run (erasing it again changes
                // nothing): one changed cell in an erased row is erased with
                // the rest, as tmux's EL leaves it, not written as a space.
                let run = (x..screen.cols)
                    .take_while(|&cx| {
                        screen.cell(cx, y).is_some_and(|c| {
                            c.text.is_empty() && c.width == 1 && c.style == cell.style
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
                at = (x < screen.cols).then_some((x, y));
            }
        }
        place_cursor(&mut out, &mut style, &screen, caps);
        if screen.cursor_visible {
            out.push_str("\x1b[?25h");
        }
        if caps.sync {
            out.push_str("\x1b[?2026l");
        }
        self.spare = self.last.replace(screen);
        self.spare_tags = std::mem::replace(&mut self.last_tags, tags);
        self.sent = Some(std::time::Instant::now());
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
    // tty_attributes (tty.c:2689-2702): without `setab`, a background other
    // than black is shown as reverse video, and reverse with a foreground
    // other than white is taken off.
    let mut inverse = wanted.inverse;
    if !caps.setab {
        let coloured = |c: Colour, plain: u8| {
            !matches!(c, Colour::Default | Colour::Terminal) && c != Colour::Ansi(plain)
        };
        if inverse {
            inverse = !coloured(core_colour(wanted.fg), 7);
        } else {
            inverse = coloured(core_colour(wanted.bg), 0);
        }
    }
    // tty_check_fg/bg: colours the terminal cannot show are downgraded; an
    // aixterm foreground on a terminal without them is bold instead.
    let depth = Depth {
        rgb: caps.rgb,
        colours: caps.palette_colours,
    };
    let (fg, bright) = colour::tty::check_fg(core_colour(wanted.fg), core_colour(wanted.bg), depth);
    let bg = colour::tty::check_bg(core_colour(wanted.bg), depth);
    // tty_colours_fg/bg: a colour below 16 goes through `setaf`/`setab`,
    // which writes nothing on a terminal without it; under
    // `TERM_256COLOURS` aixterm and palette colours are written directly.
    let written = |c: Colour, has: bool| match c {
        Colour::Ansi(_) if !has => Colour::Default,
        Colour::Bright(_) | Colour::Palette(_) if !has && !caps.theme_palette => Colour::Default,
        c => c,
    };
    let (fg, bg) = (written(fg, caps.setaf), written(bg, caps.setab));
    for (on, code) in [
        (wanted.bold || bright, 1),
        (wanted.faint, 2),
        (wanted.italic, 3),
        (wanted.underline != Underline::None, 4),
        (wanted.blink, 5),
        (inverse, 7),
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
/// 8-15 are the plain and aixterm colours, unless set as `38;5;N`
/// (`COLOUR_FLAG_256`).
fn core_colour(c: Color) -> Colour {
    match c {
        Color::Default => Colour::Default,
        Color::Palette(n @ 0..=7) => Colour::Ansi(n),
        Color::Palette(n @ 8..=15) => Colour::Bright(n - 8),
        Color::Palette(n) | Color::Palette256(n) => Colour::Palette(n),
        Color::Rgb(r, g, b) => Colour::Rgb(r, g, b),
    }
}
/// The SGR parameters of a colour already fitted to the terminal.
fn color(out: &mut String, c: Colour, bg: bool) {
    let base = if bg { 40 } else { 30 };
    match c {
        // `tty_try_colour` writes a `COLOUR_FLAG_256` colour through
        // `setaf`/`setab`, which gives 30-37 and 90-97 below 16.
        Colour::Ansi(n) | Colour::Palette(n @ 0..=7) => {
            let _ = write!(out, "\x1b[{}m", base + u16::from(n));
        }
        Colour::Bright(n) | Colour::Palette(n @ 8..=15) => {
            let _ = write!(out, "\x1b[{}m", base + 60 + u16::from(n & 7));
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
            setaf: true,
            setab: true,
            colours: 256,
            overlay: false,
            no_am: false,
            light_theme: false,
            isolates: false,
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

    fn test_caps() -> Caps {
        Caps {
            rgb: false,
            palette_colours: 256,
            theme_palette: false,
            sync: true,
            bce: true,
            setaf: true,
            setab: true,
            colours: 256,
            overlay: false,
            no_am: false,
            light_theme: false,
            isolates: false,
        }
    }

    /// A blank `cols` x 1 screen of erased cells (no text).
    fn erased(cols: u16) -> Snapshot {
        let mut s = blank(None, cols, 1);
        for c in &mut s.cells {
            c.width = 1;
        }
        s
    }

    /// One changed cell in a run the terminal shows erased is erased with
    /// the rest of the run (EL), as tmux's `tty_draw_line_clear` leaves it,
    /// not written as a space (the regress scene marked-pane-tb, on a
    /// terminal without BCE: tmux-256color).
    #[test]
    fn a_changed_cell_in_an_erased_run_is_erased() {
        let caps = Caps {
            bce: false,
            setaf: true,
            setab: true,
            ..test_caps()
        };
        let mut c = Compositor::default();
        let mut first = erased(20);
        first.cells[3].text = " ".into();
        first.cells[3].width = 1;
        let _ = c.frame(first, caps);
        let frame = String::from_utf8(c.frame(erased(20), caps)).unwrap();
        assert!(frame.contains("\x1b[1;4H\x1b[K"), "{frame:?}");
        assert!(!frame.contains("H "), "no space written: {frame:?}");
    }

    /// A changed run is written as its text: the cursor is moved once, not
    /// before every cell (`tty_cursor` skips a move to where it already is).
    #[test]
    fn a_changed_run_needs_one_cursor_move() {
        let caps = test_caps();
        let mut c = Compositor::default();
        let _ = c.frame(erased(20), caps);
        let mut pane = vtmux_vt::PaneTerm::new(20, 1, 0);
        pane.feed(b"\x1b[3Gabc def");
        let frame = String::from_utf8(c.frame(vtmux_vt::snapshot(&pane), caps)).unwrap();
        assert!(frame.contains("\x1b[1;3Habc def"), "{frame:?}");
    }

    /// `screen_write_flush_dirty`: forced cells are written even where the
    /// terminal already shows them, so a line drawn under synchronized
    /// output arrives whole; once only.
    #[test]
    fn forced_cells_are_written_whole_once() {
        let caps = test_caps();
        let mut pane = vtmux_vt::PaneTerm::new(20, 2, 0);
        pane.feed(b"FRAME_0_ROW\r\nother");
        let mut c = Compositor::default();
        let _ = c.frame(vtmux_vt::snapshot(&pane), caps);
        pane.feed(b"\x1b[1;7H1");
        c.force(0, 0, 20);
        let frame = String::from_utf8(c.frame(vtmux_vt::snapshot(&pane), caps)).unwrap();
        assert!(frame.contains("FRAME_1_ROW"), "{frame:?}");
        assert!(
            !frame.contains("other"),
            "an unforced row is a diff: {frame:?}"
        );
        assert_eq!(c.frame(vtmux_vt::snapshot(&pane), caps), b"", "used up");
    }

    /// Under a flood a frame waits out `FRAME_INTERVAL` since the last one and
    /// is then due; without one it goes at once. Blocking forgets the due
    /// time (the acks wake the loop instead).
    #[test]
    fn frames_are_paced_only_under_a_flood() {
        let caps = Caps {
            rgb: false,
            palette_colours: 256,
            theme_palette: false,
            sync: false,
            bce: true,
            setaf: true,
            setab: true,
            colours: 256,
            overlay: false,
            no_am: false,
            light_theme: false,
            isolates: false,
        };
        let mut c = Compositor::default();
        let t0 = std::time::Instant::now();
        assert!(c.paced(t0, true), "the first frame is never held");
        let _ = c.frame(vtmux_vt::snapshot(&vtmux_vt::PaneTerm::new(4, 2, 0)), caps);
        let sent = c.sent.expect("sent");
        assert!(c.paced(sent, false), "no flood: at once");
        assert_eq!(c.due(), None);
        assert!(!c.paced(sent, true));
        assert_eq!(c.due(), Some(sent + FRAME_INTERVAL));
        assert!(c.ready(0, 4, 2));
        assert_eq!(c.due(), None);
        assert!(c.paced(sent + FRAME_INTERVAL, true));
        assert_eq!(c.due(), None);
        // A change that is not flood output goes at once, flood or not,
        // until a frame has carried it.
        c.expedite();
        assert!(c.paced(sent, true), "expedited: at once");
        assert_eq!(c.due(), None);
        assert!(!c.paced(sent, true), "that frame used it up");
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
            setaf: true,
            setab: true,
            colours: 256,
            overlay: false,
            no_am: false,
            light_theme: false,
            isolates: false,
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
