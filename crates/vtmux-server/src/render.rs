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
pub mod palette;
pub mod panes;
pub mod popup;
pub mod scene;
pub mod share;
pub mod status;
pub mod tree;
pub mod window;
use std::sync::Arc;
use vtmux_core::colour::{self, Colour, tty::Depth};
use vtmux_core::protocol::Hello;
use vtmux_core::term_features::{Cap, OpenError, Term};
use vtmux_core::width;
use vtmux_vt::{Color, Link, Snapshot, Style, Underline};

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
    /// The attributes it can show.
    pub attrs: Attrs,
    /// `Hls` (the `hyperlinks` feature): cells' OSC 8 links are written.
    pub hyperlinks: bool,
}

/// The attribute capabilities `tty_attributes` writes with (tty.c:2727-
/// 2758) and `tty_colours_us` (tty.c:3060-3104), by presence: tmux writes
/// nothing for one the terminal lacks. vtmux writes each in the form
/// tmux's own feature gives it (tty-features.c), not the entry's template
/// (D-007).
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent terminal capabilities"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
    /// `bold` (also for an aixterm colour the terminal lacks), `dim`,
    /// `smul`, `blink`, `invis`, `smxx`.
    pub bold: bool,
    pub dim: bool,
    pub underline: bool,
    pub blink: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    /// `sitm`.
    pub italics: bool,
    /// The SGR parameter of `smso` (7, or screen's 3), if it has one.
    pub standout: Option<u8>,
    /// `rev`, else `smso` (`tty_attributes`).
    pub reverse: Option<u8>,
    /// `default-terminal` is `screen` or `screen-*`: `tty_set_italics`
    /// writes italics as `smso` even with `sitm`. Not a terminal
    /// capability; set for each frame.
    pub italics_as_standout: bool,
    /// `Smulx`: the double, curly, dotted and dashed underlines.
    pub underline_styles: bool,
    /// `Setulc`: an RGB underline colour; `Setulc1`: a palette one.
    pub underline_rgb: bool,
    pub underline_palette: bool,
    /// `Smol`.
    pub overline: bool,
}
impl Attrs {
    /// What xterm-256color has.
    pub const XTERM: Self = Self {
        bold: true,
        dim: true,
        underline: true,
        blink: true,
        invisible: true,
        strikethrough: true,
        italics: true,
        standout: Some(7),
        reverse: Some(7),
        italics_as_standout: false,
        underline_styles: false,
        underline_rgb: false,
        underline_palette: false,
        overline: false,
    };

    #[must_use]
    pub fn from_term(term: &Term) -> Self {
        // A plain SGR (`\E[7m`), padding already stripped; else xterm's.
        let standout = term.code("smso", &[]).map(|s| {
            s.strip_prefix("\x1b[")
                .and_then(|s| s.strip_suffix('m'))
                .and_then(|n| n.parse().ok())
                .unwrap_or(7)
        });
        Self {
            bold: term.has_name("bold"),
            dim: term.has_name("dim"),
            underline: term.has_name("smul"),
            blink: term.has_name("blink"),
            invisible: term.has_name("invis"),
            strikethrough: term.has_name("smxx"),
            italics: term.has_name("sitm"),
            standout,
            reverse: if term.has_name("rev") {
                Some(7)
            } else {
                standout
            },
            italics_as_standout: false,
            underline_styles: term.has_name("Smulx"),
            underline_rgb: term.has_name("Setulc"),
            underline_palette: term.has_name("Setulc1"),
            overline: term.has_name("Smol"),
        }
    }
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
            attrs: Attrs::from_term(term),
            hyperlinks: term.has_name("Hls"),
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
    out.push_str(&term.code("Enbp", &[]).unwrap_or_default());
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
/// none to leave and the clear comes last. A cursor style or colour the
/// terminal was given goes back to its default first. `csr` and `sgr0`
/// are xterm's forms; bracketed paste, the application escape mode, focus
/// reports and extended keys go off as `Dsbp`, `Dsesc`, `Dsfcs` and
/// `Dseks` say.
#[must_use]
pub fn teardown(term: &Term, clear: bool, cursor: &crate::outer_cursor::OuterCursor) -> Vec<u8> {
    let s = |name| term.string(name).unwrap_or_default();
    let mut out = String::from("\x1b[r\x1b[0m");
    out.push_str(s("rmkx"));
    if clear {
        out.push_str(s("clear"));
    }
    out.push_str(&cursor.teardown(term));
    out.push_str(s("cnorm"));
    if term.has(Cap::Kmous) {
        out.push_str("\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l");
    }
    for name in ["Dsbp", "Dsesc", "Dsfcs"] {
        out.push_str(&term.code(name, &[]).unwrap_or_default());
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
    /// The scene and composed rows, shared with the clients that drew the
    /// same window alike ([`share`]).
    pub composition: std::cell::RefCell<std::rc::Rc<share::Composition>>,
    /// The [`compose::ComposeCache::generation`] this client last drew,
    /// `None` when it has not drawn from `composition` yet.
    pub seen: std::cell::Cell<Option<u64>>,
    /// Each pane's border style as this client last expanded it.
    pub borders: window::BorderStyles,
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
    /// Drawings this client took from another ([`share`]), frames it drew
    /// and found another's, and plain frames it took without drawing them.
    #[cfg(test)]
    pub shared: std::cell::Cell<(u32, u32, u32)>,
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
        c.link = None;
        c.tab = false;
    }
}

#[derive(Clone, Debug, Default)]
pub struct Compositor {
    /// What the terminal shows; shared with the clients that took the
    /// same frame ([`share`]).
    last: Option<std::rc::Rc<Snapshot>>,
    /// Per row of `last`, the composed row it was copied from
    /// ([`compose::ComposeCache::copy_rows`]); 0 for a row drawn otherwise.
    /// Two frames' rows with the same nonzero tag hold the same cells.
    last_tags: Vec<u64>,
    /// The frame before `last`, its buffer kept for the next one (copied
    /// first if another client still shows it).
    spare: Option<std::rc::Rc<Snapshot>>,
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
    /// What a frame hides the cursor with and shows it with.
    cursor_codes: CursorCodes,
}

/// The terminal's `civis` and what ends a frame's hidden cursor. tmux
/// hides the cursor with `civis` (`tty_update_cursor`, tty.c:805) and
/// shows it with `cnorm`; on the Linux console `civis` is `ESC[?25l
/// ESC[?1c`, which `ESC[?25h` alone does not undo, so there a frame ends
/// with `cnorm`. A terminal with no `civis` gets none (`tty_putcode`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorCodes {
    hide: String,
    show: String,
}

impl Default for CursorCodes {
    /// xterm's: DECTCEM.
    fn default() -> Self {
        Self {
            hide: "\x1b[?25l".into(),
            show: "\x1b[?25h".into(),
        }
    }
}

impl CursorCodes {
    #[must_use]
    pub fn from_term(term: &Term) -> Self {
        let hide = term.code("civis", &[]).unwrap_or_default();
        // The style and blink bits come after the frame, from
        // `OuterCursor`: `cnorm` itself only where `civis` is more than
        // DECTCEM.
        let show = if hide == Self::default().hide {
            Self::default().show
        } else {
            term.code("cnorm", &[]).unwrap_or_default()
        };
        Self { hide, show }
    }
}

impl Compositor {
    /// Hide and show the cursor with `term`'s codes from now on.
    pub fn set_cursor_codes(&mut self, codes: CursorCodes) {
        self.cursor_codes = codes;
    }
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
    /// The last frame sent left the cursor shown.
    #[must_use]
    pub fn cursor_shown(&self) -> bool {
        self.last.as_ref().is_some_and(|l| l.cursor_visible)
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
        self.last.as_deref().map(|l| (l, self.last_tags.as_slice()))
    }
    /// Keep `screen` and its row tags, unsent, as the next frame's buffer.
    pub fn recycle(&mut self, screen: Snapshot, tags: Vec<u64>) {
        self.spare = Some(std::rc::Rc::new(screen));
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
        match self.spare.take().map(std::rc::Rc::unwrap_or_clone) {
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
        place_cursor(&mut out, &mut Pen::default(), last, caps);
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
    /// Per row of `screen`, the columns `first..end` from the first to the
    /// last cell that differs from what the terminal shows (`links`: the
    /// terminal is given links, so a cell's link is part of what it shows,
    /// [`cells_differ`]), `None` for a row that does not; every column of
    /// every row when the size changed, and of a row with forced cells.
    /// `None` when nothing differs, the cursor included. Rows tagged alike
    /// in both are equal without a look (see `last_tags`).
    fn changed_rows(
        &self,
        screen: &Snapshot,
        tags: &[u64],
        same_size: bool,
        links: bool,
    ) -> Option<Vec<Option<(u16, u16)>>> {
        let whole = Some((0, screen.cols));
        let Some(last) = self.last.as_ref().filter(|_| same_size) else {
            return Some(vec![whole; usize::from(screen.rows)]);
        };
        let width = usize::from(screen.cols).max(1);
        let mut rows: Vec<Option<(u16, u16)>> = screen
            .cells
            .chunks(width)
            .zip(last.cells.chunks(width))
            .enumerate()
            .map(|(y, (new, shown))| {
                let tag = tags.get(y).copied().unwrap_or(0);
                if tag != 0 && self.last_tags.get(y) == Some(&tag) {
                    return None;
                }
                let differs = |(shown, new): (&vtmux_vt::Cell, &vtmux_vt::Cell)| {
                    cells_differ(shown, new, links)
                };
                let first = shown.iter().zip(new).position(differs)?;
                let end = width - shown.iter().zip(new).rev().position(differs)?;
                let col = |i: usize| u16::try_from(i).unwrap_or(u16::MAX);
                Some((col(first), col(end)))
            })
            .collect();
        for &(y, _, _) in &self.forced {
            if let Some(r) = rows.get_mut(usize::from(y)) {
                *r = whole;
            }
        }
        let same = (last.cursor, last.cursor_visible, &last.wrapped)
            == (screen.cursor, screen.cursor_visible, &screen.wrapped);
        (rows.iter().any(Option::is_some) || !same).then_some(rows)
    }
    /// One frame per tick. The caller supplies a fully composed screen;
    /// therefore changes behind opaque overlays cannot escape into a frame.
    pub fn frame(&mut self, screen: Snapshot, caps: Caps) -> Vec<u8> {
        let tags = vec![0; usize::from(screen.rows)];
        self.frame_tagged(screen, tags, caps)
    }
    /// [`Self::frame`] of a screen with row tags ([`Self::buffer`]).
    #[expect(clippy::too_many_lines, reason = "one pass over the screen's cells")]
    pub fn frame_tagged(&mut self, screen: Snapshot, tags: Vec<u64>, caps: Caps) -> Vec<u8> {
        let same_size = self
            .last
            .as_ref()
            .is_some_and(|s| (s.cols, s.rows) == (screen.cols, screen.rows));
        let Some(changed_rows) = self.changed_rows(&screen, &tags, same_size, caps.hyperlinks)
        else {
            self.recycle(screen, tags);
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
        out.push_str(&self.cursor_codes.hide);
        let mut style = Pen::default();
        out.push_str("\x1b[0m");
        // Where the terminal's cursor is, when known: a cell written there
        // needs no cursor move (tty_cursor skips one to where it already
        // is, tty.c:2468), so a changed run goes out as its text. Unknown
        // after the last column (the pending wrap) and after an erase.
        let mut at: Option<(u16, u16)> = None;
        for y in 0..screen.rows {
            let Some((first, end)) = changed_rows[usize::from(y)] else {
                continue;
            };
            // Cells before `first` and from `end` on are what the terminal
            // shows, so only a lone regional indicator just before `first`
            // (written with the changed cell after it) and the cell a
            // regional indicator forces are written outside them.
            let mut x = first.saturating_sub(1);
            // The cell after a lone regional indicator, written whatever
            // the last frame showed there.
            let mut after_regional = None;
            while x < screen.cols {
                if x >= end && after_regional.is_none_or(|a| a < x) {
                    break;
                }
                let cell = screen.cell(x, y).expect("screen cell");
                let changed_at = |x: u16| {
                    !same_size
                        || is_forced(x, y)
                        || after_regional == Some(x)
                        || match (self.last.as_ref().and_then(|s| s.cell(x, y)), cell) {
                            (Some(shown), new) => cells_differ(shown, new, caps.hyperlinks),
                            (None, _) => true,
                        }
                };
                let regional = lone_regional(cell);
                let changed =
                    changed_at(x) || (regional && x + 1 < screen.cols && changed_at(x + 1));
                if !changed || cell.width == 0 {
                    x += 1;
                    continue;
                }
                // tty_cell: a terminal without `am` would scroll.
                if caps.no_am && y + 1 == screen.rows && x + 1 == screen.cols {
                    break;
                }
                if regional {
                    let (next, force) =
                        write_regional(&mut out, &mut style, &screen, (x, y), at, caps);
                    after_regional = force.then_some(next);
                    x = next;
                    at = None;
                    continue;
                }
                if joins_left(cell)
                    && let Some(next) =
                        write_after_base(&mut out, &mut style, &screen, (x, y), caps)
                {
                    x = next;
                    at = None;
                    continue;
                }
                let narrow = may_draw_narrow(cell);
                if narrow && x + 1 < screen.cols {
                    // Its second column first, as a blank that a terminal
                    // drawing the cell two wide covers, and one drawing it
                    // one wide keeps (else it shows what it showed before).
                    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 2);
                    set_style(&mut out, &mut style, cell.style, None, caps);
                    out.push(' ');
                    at = None;
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
                            c.text.is_empty()
                                && c.width == 1
                                && c.style == cell.style
                                && c.link.is_none()
                        })
                    })
                    .count();
                let erasable = cell.style.bg == Color::Default || caps.bce;
                if run >= 10 && erasable {
                    set_style(&mut out, &mut style, cell.style, None, caps);
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
                // A cell in a link is never empty (`gc->link == 0` there):
                // erasing it would end the link.
                if blank
                    && cell.style == Style::default()
                    && (x..screen.cols).all(|cx| {
                        screen.cell(cx, y).is_some_and(|c| {
                            (c.text.is_empty() || c.text == " ")
                                && c.style == cell.style
                                && c.link.is_none()
                        })
                    })
                    && caps.bce
                    && !caps.overlay
                {
                    set_style(&mut out, &mut style, cell.style, None, caps);
                    out.push_str("\x1b[K");
                    break;
                }
                set_style(&mut out, &mut style, cell.style, cell.link.as_ref(), caps);
                out.push_str(&outer_text(cell));
                x += u16::from(cell.width.max(1));
                at = (x < screen.cols && !narrow).then_some((x, y));
            }
        }
        place_cursor(&mut out, &mut style, &screen, caps);
        if screen.cursor_visible {
            out.push_str(&self.cursor_codes.show);
        }
        if caps.sync {
            out.push_str("\x1b[?2026l");
        }
        self.spare = self.last.replace(std::rc::Rc::new(screen));
        self.spare_tags = std::mem::replace(&mut self.last_tags, tags);
        self.sent = Some(std::time::Instant::now());
        out.into_bytes()
    }
}
/// Whether the terminal, which shows `shown`, must be written `new`. A
/// terminal that is given no links (`links` false) shows none, so a cell
/// that differs only in its link (a program printing the same linked text
/// with a new id each time) is already what it shows.
fn cells_differ(shown: &vtmux_vt::Cell, new: &vtmux_vt::Cell, links: bool) -> bool {
    shown != new
        && (links
            || (&shown.text, shown.width, &shown.style, shown.acs, shown.tab)
                != (&new.text, new.width, &new.style, new.acs, new.tab))
}

/// A two-column cell whose first character a terminal draws one column
/// wide on its own: one a joined character made two columns wide (U+FE0F,
/// a regional indicator pair, a skin-tone modifier, one after a ZWJ on a
/// text-presentation emoji: `screen_write_combine`'s `force_wide`, D-045),
/// or one `codepoint-widths` widened. The terminal may still count it as
/// one, so where its cursor is after it is not known: tmux sends a joined
/// cell with `TTY_CTX_CELL_INVALIDATE`, which forgets the cursor
/// (`tty_cmd_cell`, tty.c), and so the next cell gets a cursor move.
fn may_draw_narrow(cell: &vtmux_vt::Cell) -> bool {
    cell.width == 2
        && cell
            .text
            .chars()
            .next()
            .is_some_and(|c| width::table_width(c) != 2)
}

/// A cell of a skin-tone modifier alone, which tmux gave a cell of its own
/// (its base is not one of `utf8_should_combine`'s, such as U+270C): a
/// terminal that clusters by Unicode's rules (Ghostty) joins it to the
/// emoji before it as it is printed, wherever the cursor came from, which
/// draws both in one column and moves nothing after it into place (D-045).
fn joins_left(cell: &vtmux_vt::Cell) -> bool {
    let mut chars = cell.text.chars();
    chars.next().is_some_and(is_skin_tone) && chars.next().is_none()
}

fn is_skin_tone(c: char) -> bool {
    ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
}

/// Write the [`joins_left`] cell at `x`, `y` so that it joins nothing: the
/// cell before it is blanked first, then this cell is written after the
/// blank (which nothing joins), then the cell before it again (a character
/// joins only what is before it). `None` when there is no text before it
/// to join. Returns the next column to look at.
fn write_after_base(
    out: &mut String,
    style: &mut Pen,
    screen: &Snapshot,
    (x, y): (u16, u16),
    caps: Caps,
) -> Option<u16> {
    let cell = screen.cell(x, y)?;
    let lx = match x.checked_sub(1).and_then(|lx| screen.cell(lx, y))? {
        c if c.width == 0 => x.checked_sub(2)?,
        _ => x - 1,
    };
    let left = screen
        .cell(lx, y)
        .filter(|c| !c.text.is_empty() && c.text != " " && !lone_regional(c))?;
    let _ = write!(out, "\x1b[{};{}H", y + 1, lx + 1);
    set_style(out, style, left.style, None, caps);
    out.push(' ');
    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
    set_style(out, style, cell.style, cell.link.as_ref(), caps);
    out.push_str(&outer_text(cell));
    let _ = write!(out, "\x1b[{};{}H", y + 1, lx + 1);
    set_style(out, style, left.style, left.link.as_ref(), caps);
    out.push_str(&outer_text(left));
    Some(x + u16::from(cell.width.max(1)))
}

/// A one-column cell of a regional indicator alone (tmux's width,
/// `utf8_default_width_cache`), which a terminal may count two columns
/// wide (Ghostty does).
fn lone_regional(cell: &vtmux_vt::Cell) -> bool {
    let mut chars = cell.text.chars();
    cell.width == 1 && chars.next().is_some_and(is_regional) && !chars.any(joins_wide)
}

fn is_regional(c: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
}

/// A character that, after another in a cell, may make a terminal draw
/// the cell two columns wide: U+FE0F, a ZWJ (and what it joins), or one
/// with a width of its own (a regional indicator, a skin-tone modifier).
fn joins_wide(c: char) -> bool {
    c == '\u{fe0f}' || c == '\u{200d}' || width::char_width(c) != 0
}

/// Write the lone regional indicator at `x`, `y` ([`lone_regional`]) so
/// that it moves nothing else whether the terminal draws it one column
/// wide or two. tmux writes it as any other cell, so on a terminal that
/// draws it two wide everything after it on the row is drawn a column
/// right, the row's last character wraps (scrolling the screen on the
/// bottom row) and a border beside it is covered (D-044). Here the cursor
/// is moved after it, and the cell after it is written too: before it when
/// that is a blank that looks the same in its style (only the background
/// shows), which the indicator then covers on such a terminal (a program
/// that counts it two wide leaves that blank, as vim does), else after it,
/// which wipes out the indicator there. In the last column, where such a
/// terminal would wrap it onto the next line, it is a space. Returns the
/// next column to look at and whether it must be written.
fn write_regional(
    out: &mut String,
    style: &mut Pen,
    screen: &Snapshot,
    (x, y): (u16, u16),
    at: Option<(u16, u16)>,
    caps: Caps,
) -> (u16, bool) {
    let cell = screen.cell(x, y).expect("screen cell");
    // A blank looks the same in the indicator's style when only its
    // background shows.
    let plain =
        |s: &Style| !s.inverse && !s.strikethrough && !s.overline && s.underline == Underline::None;
    let next = screen.cell(x + 1, y).filter(|n| {
        n.width == 1
            && (n.text.is_empty() || n.text == " ")
            && (n.style == cell.style
                || (plain(&n.style) && plain(&cell.style) && n.style.bg == cell.style.bg))
    });
    if let Some(next) = next {
        let _ = write!(out, "\x1b[{};{}H", y + 1, x + 2);
        set_style(out, style, next.style, None, caps);
        out.push(' ');
    }
    if next.is_some() || at != Some((x, y)) {
        let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
    }
    set_style(out, style, cell.style, cell.link.as_ref(), caps);
    if x + 1 == screen.cols {
        out.push(' ');
        return (x + 1, false);
    }
    out.push_str(&cell.text);
    if next.is_some() {
        (x + 2, false)
    } else {
        (x + 1, true)
    }
}

/// What a cell is written as. A one-column cell holding characters a
/// terminal may join into a two-column cluster is written without them,
/// a flag as a space (D-044): U+FE0F, a second regional indicator or a
/// skin-tone modifier, which `screen_write_combine` makes two columns
/// (`force_wide`) but which the grid keeps one in the pane's last column,
/// and an emoji after a ZWJ, which tmux joins to any cell. A terminal that
/// counts the cluster two columns wide (Ghostty) would wrap it onto the
/// next line, scrolling the screen at the bottom, or cover the border
/// beside it. tmux does not send them in the last column either: its cell
/// is two columns, which do not fit, so `screen_write_combine` does not
/// redraw it (the narrow base stays on the terminal) and `tty_draw_line`
/// draws it as empty (`tty_draw_line_get_empty`, tty-draw.c). Zero-width
/// marks (combining accents, the border's bidi isolate) stay.
fn outer_text(cell: &vtmux_vt::Cell) -> std::borrow::Cow<'_, str> {
    let text: &str = &cell.text;
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return " ".into();
    };
    if cell.width != 1 || !chars.clone().any(joins_wide) {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    out.push(if is_regional(first) || width::char_width(first) != 1 {
        ' '
    } else {
        first
    });
    out.extend(chars.filter(|&c| !joins_wide(c)));
    out.into()
}

/// End the frame's drawing (a link left open ended, then `\x1b[0m`: what
/// comes between frames, passthrough or a detach's message, is in none)
/// and move the cursor. A column past the last one is the terminal's
/// pending wrap after the row's last cell, which `tty_cursor` leaves alone
/// (tty.c:2468); writing that cell again puts the terminal there. A
/// terminal without `am` never has the bottom-right cell written
/// (`tty_cell`), so there `tty_cursor` clamps to the last column.
fn place_cursor(out: &mut String, style: &mut Pen, screen: &Snapshot, caps: Caps) {
    let end = |out: &mut String, style: &mut Pen| {
        set_link(out, style, None, caps);
        out.push_str("\x1b[0m");
    };
    let bottom_right = |y: u16| caps.no_am && y + 1 == screen.rows;
    match screen.cursor {
        Some((x, y)) if x >= screen.cols && bottom_right(y) => {
            end(out, style);
            let _ = write!(out, "\x1b[{};{}H", y + 1, screen.cols);
        }
        Some((x, y)) if x >= screen.cols => {
            let last = (0..screen.cols)
                .rev()
                .find_map(|cx| screen.cell(cx, y).filter(|c| c.width != 0).map(|c| (cx, c)));
            if let Some((cx, cell)) = last {
                let _ = write!(out, "\x1b[{};{}H", y + 1, cx + 1);
                set_style(out, style, cell.style, cell.link.as_ref(), caps);
                // A lone regional indicator there went out as a space.
                if lone_regional(cell) {
                    out.push(' ');
                } else {
                    out.push_str(&outer_text(cell));
                }
            }
            end(out, style);
        }
        Some((x, y)) => {
            end(out, style);
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
        }
        None => end(out, style),
    }
}
/// What the terminal writes with: the attributes and the hyperlink.
#[derive(Debug, Default)]
struct Pen {
    style: Style,
    link: Option<Arc<Link>>,
}

/// Write `wanted` with `link` (tty.c `tty_attributes`, which ends with
/// `tty_hyperlink`).
fn set_style(out: &mut String, pen: &mut Pen, wanted: Style, link: Option<&Arc<Link>>, caps: Caps) {
    set_attrs(out, &mut pen.style, wanted, caps);
    set_link(out, pen, link, caps);
}

/// `tty_hyperlink` (tty.c:2598-2615): when the link changes, `Hls` with
/// its external id and URI, or with neither to end it; nothing on a
/// terminal without `Hls`. tmux's form (`tty_feature_hyperlinks`,
/// tty-features.c:101), as for the attributes (D-007).
fn set_link(out: &mut String, pen: &mut Pen, link: Option<&Arc<Link>>, caps: Caps) {
    if !caps.hyperlinks || pen.link.as_ref() == link {
        return;
    }
    match link {
        Some(l) => {
            let _ = write!(out, "\x1b]8;id={};{}\x1b\\", l.id(), l.uri());
        }
        None => out.push_str("\x1b]8;;\x1b\\"),
    }
    pen.link = link.cloned();
}

fn set_attrs(out: &mut String, current: &mut Style, wanted: Style, caps: Caps) {
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
    // tty_attributes (tty.c:2727-2758): each with the terminal's
    // capability, nothing without it; italics by tty_set_italics.
    let a = caps.attrs;
    let italics = if a.italics && !a.italics_as_standout {
        Some(3)
    } else {
        a.standout
    };
    // A double, curly, dotted or dashed underline is `Smulx` with its
    // number; a terminal without it gets a plain one (D-058), where tmux
    // writes nothing (tty.c:2735-2743).
    let styled = match wanted.underline {
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
        Underline::None | Underline::Single => 0,
    };
    let plain_underline =
        wanted.underline == Underline::Single || (styled != 0 && !a.underline_styles);
    for (on, code) in [
        ((wanted.bold || bright) && a.bold, Some(1)),
        (wanted.faint && a.dim, Some(2)),
        (wanted.italic, italics),
        (plain_underline && a.underline, Some(4)),
        (wanted.blink && a.blink, Some(5)),
        (inverse, a.reverse),
        (wanted.invisible && a.invisible, Some(8)),
        (wanted.strikethrough && a.strikethrough, Some(9)),
        (wanted.overline && a.overline, Some(53)),
    ] {
        if let (true, Some(code)) = (on, code) {
            let _ = write!(out, "\x1b[{code}m");
        }
    }
    if styled != 0 && a.underline_styles {
        let _ = write!(out, "\x1b[4:{styled}m");
    }
    color(out, fg, false);
    color(out, bg, true);
    underline_colour(out, wanted.underline_color, caps.attrs);
    *current = wanted;
}

/// `tty_check_us` and `tty_colours_us` (tty.c:2965-2984, 3060-3104): a
/// palette underline colour is `Setulc1` with its number, or, without it,
/// the colour as RGB (`colour_force_rgb`); RGB is `Setulc`.
fn underline_colour(out: &mut String, c: Color, attrs: Attrs) {
    let rgb = match c {
        Color::Default => return,
        Color::Palette(n) | Color::Palette256(n) if attrs.underline_palette => {
            let _ = write!(out, "\x1b[58:5:{n}m");
            return;
        }
        Color::Palette(n) | Color::Palette256(n) => colour::palette_rgb(n),
        Color::Rgb(r, g, b) => (r, g, b),
    };
    if attrs.underline_rgb {
        let (r, g, b) = rgb;
        let _ = write!(out, "\x1b[58:2:{r}:{g}:{b}m");
    }
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
            attrs: Attrs::XTERM,
            hyperlinks: false,
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
            attrs: Attrs::XTERM,
            hyperlinks: false,
        }
    }

    /// tty.c `tty_hyperlink`: a terminal with `Hls` is told each cell's
    /// link as tmux words it, `OSC 8 ; id=tmuxN ; URI ST` where the link
    /// changes and `OSC 8 ; ; ST` where it ends; a link goes on from one
    /// row to the next (a wrapped link is one link), and the frame ends
    /// with none open. A terminal without `Hls` is told nothing, the cells
    /// written as ever.
    #[test]
    fn links_are_written_where_they_change() {
        let mut pane = vtmux_vt::PaneTerm::new(6, 3, 0);
        pane.feed(b"\x1b]8;id=a;http://a\x1b\\ab\x1b]8;;\x1b\\c\x1b]8;;http://b\\x\x1b\\defghi\x1b]8;;\x1b\\");
        let screen = pane.screen().snapshot().clone();
        let id = |x: u16, y: u16| screen.cell(x, y).unwrap().link.as_ref().unwrap().id();
        let (a, b) = (id(0, 0), id(3, 0));
        assert_ne!(a, b);
        let caps = Caps {
            hyperlinks: true,
            ..test_caps()
        };
        let frame = String::from_utf8(Compositor::default().frame(screen.clone(), caps)).unwrap();
        let want = format!(
            "\x1b]8;id={a};http://a\x1b\\ab\x1b]8;;\x1b\\c\x1b]8;id={b};http://b\\\\x\x1b\\def\x1b[2;1Hghi"
        );
        assert!(frame.contains(&want), "{frame:?}");
        let end = frame.rfind("ghi").unwrap();
        assert!(frame[end..].starts_with("ghi\x1b]8;;\x1b\\"), "{frame:?}");
        let plain = String::from_utf8(Compositor::default().frame(screen, test_caps())).unwrap();
        assert!(!plain.contains("\x1b]8"), "{plain:?}");
        assert!(plain.contains("abcdef\x1b[2;1Hghi"), "{plain:?}");
    }

    /// A cell whose link changed is written again with it, even when its
    /// text and style did not change.
    #[test]
    fn a_changed_link_redraws_the_cell() {
        let caps = Caps {
            hyperlinks: true,
            ..test_caps()
        };
        let mut c = Compositor::default();
        let mut screen = erased(4);
        screen.cells[0].text = "x".into();
        let _ = c.frame(screen.clone(), caps);
        let link = vtmux_vt::LinkNumbers::default().fresh("http://l");
        screen.cells[0].link = Some(std::sync::Arc::clone(&link));
        let frame = String::from_utf8(c.frame(screen, caps)).unwrap();
        let want = format!(
            "\x1b[1;1H\x1b]8;id={};http://l\x1b\\x\x1b]8;;\x1b\\",
            link.id()
        );
        assert!(frame.contains(&want), "{frame:?}");
    }

    /// tty-draw.c `tty_draw_line_get_empty`: a cell in a link is never
    /// empty (`gc->link == 0`), so spaces at the end of a row are written
    /// with their link, not erased with `\e[K` (which would drop it), and
    /// a row of only linked spaces is written whole. A terminal without
    /// `Hls` is written the same cells (the link is in tmux's grid whatever
    /// the terminal is).
    #[test]
    fn linked_spaces_at_the_end_of_a_row_keep_their_link() {
        let mut pane = vtmux_vt::PaneTerm::new(12, 2, 0);
        pane.feed(b"\x1b]8;;http://sp/\x1b\\cd        \x1b]8;;\x1b\\\r\n\x1b]8;;http://sp/\x1b\\");
        pane.feed(b"            \x1b]8;;\x1b\\");
        let screen = pane.screen().snapshot().clone();
        let id = screen.cell(0, 0).unwrap().link.as_ref().unwrap().id();
        let caps = Caps {
            hyperlinks: true,
            ..test_caps()
        };
        let frame = String::from_utf8(Compositor::default().frame(screen.clone(), caps)).unwrap();
        let spaces = "        ";
        let want = format!("\x1b]8;id={id};http://sp/\x1b\\cd{spaces}\x1b]8;;\x1b\\");
        assert!(frame.contains(&want), "{frame:?}");
        let id2 = screen.cell(0, 1).unwrap().link.as_ref().unwrap().id();
        let row = format!("\x1b]8;id={id2};http://sp/\x1b\\{}", " ".repeat(12));
        assert!(frame.contains(&row), "{frame:?}");
        for caps in [caps, test_caps()] {
            let frame =
                String::from_utf8(Compositor::default().frame(screen.clone(), caps)).unwrap();
            assert!(frame.contains(&format!("cd{spaces}")), "{frame:?}");
            assert!(frame.contains(&" ".repeat(12)), "{frame:?}");
        }
    }

    /// A terminal without `Hls` shows no link, so cells that differ from
    /// what it shows only in their link are not written again (a program
    /// that prints the same linked text with a new id each time costs it
    /// nothing); one with `Hls` is written the new link.
    #[test]
    fn a_link_only_change_is_sent_to_a_terminal_with_hyperlinks_only() {
        let mut screen = erased(8);
        for (c, t) in screen.cells.iter_mut().zip("SAMELINK".chars()) {
            c.text = t.to_string().as_str().into();
        }
        let numbers = vtmux_vt::LinkNumbers::default();
        let relink = |screen: &mut Snapshot| {
            let link = numbers.fresh("http://l");
            for c in &mut screen.cells {
                c.link = Some(std::sync::Arc::clone(&link));
            }
        };
        for hyperlinks in [false, true] {
            let caps = Caps {
                hyperlinks,
                ..test_caps()
            };
            let mut c = Compositor::default();
            relink(&mut screen);
            let first = c.frame(screen.clone(), caps);
            assert!(first.windows(8).any(|w| w == b"SAMELINK"));
            relink(&mut screen);
            let again = c.frame(screen.clone(), caps);
            assert_eq!(
                again.is_empty(),
                !hyperlinks,
                "{:?}",
                String::from_utf8_lossy(&again)
            );
            if hyperlinks {
                assert!(again.windows(8).any(|w| w == b"SAMELINK"));
            }
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

    /// `tty_attributes` and `tty_colours_us` by what the terminal has:
    /// without `Smulx` the styled underlines are not written at all
    /// (xterm-256color), without `Setulc` no underline colour, without
    /// `Smol` no overline; with them, tmux's feature forms, a palette
    /// colour as RGB unless there is `Setulc1`.
    #[test]
    fn underline_styles_colours_and_overline_follow_the_terminal() {
        let frame_of = |attrs: Attrs| {
            let mut pane = vtmux_vt::PaneTerm::new(8, 1, 0);
            pane.feed(b"\x1b[4:3;53mc\x1b[0;4;58:2::1:2:3mu\x1b[0;4:2;58;5;196md");
            let caps = Caps {
                attrs,
                ..test_caps()
            };
            String::from_utf8(Compositor::default().frame(vtmux_vt::snapshot(&pane), caps)).unwrap()
        };
        // D-058: without `Smulx` the styled underlines are a plain one,
        // where tmux writes none (tty.c:2735-2743).
        let plain = frame_of(Attrs::XTERM);
        assert!(
            plain.contains("\x1b[1;1H\x1b[0m\x1b[4mc\x1b[0m\x1b[4mu\x1b[0m\x1b[4md"),
            "{plain:?}"
        );
        // No `smul` either: no underline at all.
        let none = frame_of(Attrs {
            underline: false,
            ..Attrs::XTERM
        });
        assert!(
            none.contains("\x1b[1;1H\x1b[0mc\x1b[0mu\x1b[0md"),
            "{none:?}"
        );
        let styled = frame_of(Attrs {
            underline_styles: true,
            underline_rgb: true,
            underline_palette: false,
            overline: true,
            ..Attrs::XTERM
        });
        assert!(
            styled.contains(
                "\x1b[0m\x1b[53m\x1b[4:3mc\x1b[0m\x1b[4m\x1b[58:2:1:2:3mu\x1b[0m\x1b[4:2m\x1b[58:2:255:0:0md"
            ),
            "{styled:?}"
        );
        let palette = frame_of(Attrs {
            underline_styles: true,
            underline_rgb: true,
            underline_palette: true,
            overline: false,
            ..Attrs::XTERM
        });
        assert!(palette.contains("\x1b[4:2m\x1b[58:5:196md"), "{palette:?}");
    }

    /// `tty_update_cursor` hides the cursor with `civis` (tty.c:805), which
    /// is `ESC[?25l ESC[?1c` on the console: a frame hides it so, and ends
    /// with `cnorm`, since `ESC[?25h` alone leaves the console's hardware
    /// cursor off. xterm's are DECTCEM; a terminal with no `civis` gets
    /// nothing.
    #[test]
    fn a_frame_hides_the_cursor_with_the_terminals_civis() {
        let frame_of = |name: &str| {
            let term = Term::open(name, &[], None, &[], &[]).unwrap();
            let mut pane = vtmux_vt::PaneTerm::new(4, 1, 0);
            pane.feed(b"ab");
            let mut compositor = Compositor::default();
            compositor.set_cursor_codes(CursorCodes::from_term(&term));
            let caps = Caps {
                sync: false,
                ..test_caps()
            };
            String::from_utf8(compositor.frame(vtmux_vt::snapshot(&pane), caps)).unwrap()
        };
        let xterm = frame_of("xterm-256color");
        assert!(xterm.starts_with("\x1b[?25l\x1b[0m"), "{xterm:?}");
        assert!(xterm.ends_with("\x1b[?25h"), "{xterm:?}");
        let linux = frame_of("linux");
        assert!(linux.starts_with("\x1b[?25l\x1b[?1c\x1b[0m"), "{linux:?}");
        assert!(linux.ends_with("\x1b[?25h\x1b[?0c"), "{linux:?}");
        let vt100 = frame_of("vt100");
        assert!(!vt100.contains("\x1b[?25"), "{vt100:?}");
    }

    /// `tty_attributes` writes an attribute with the terminal's
    /// capability or not at all, and italics as `tty_set_italics` does:
    /// `sitm`, else `smso` (reverse on the console, italics on screen),
    /// and `smso` whatever the terminal when `default-terminal` is screen.
    #[test]
    fn attributes_follow_the_terminal() {
        let frame_of = |name: &str, screen_default: bool| {
            let term = Term::open(name, &[], None, &[], &[]).unwrap();
            let mut attrs = Attrs::from_term(&term);
            attrs.italics_as_standout = screen_default;
            let mut pane = vtmux_vt::PaneTerm::new(9, 1, 0);
            pane.feed(b"\x1b[1mb\x1b[0;2md\x1b[0;3mi\x1b[0;5mk\x1b[0;7mr\x1b[0;8mh\x1b[0;9ms");
            let caps = Caps {
                attrs,
                ..test_caps()
            };
            String::from_utf8(Compositor::default().frame(vtmux_vt::snapshot(&pane), caps)).unwrap()
        };
        let run = |a: [&str; 7]| {
            let mut s = String::new();
            for (code, c) in a.iter().zip("bdikrhs".chars()) {
                s.push_str("\x1b[0m");
                s.push_str(code);
                s.push(c);
            }
            s
        };
        let xterm = frame_of("xterm-256color", false);
        let all = [
            "\x1b[1m", "\x1b[2m", "\x1b[3m", "\x1b[5m", "\x1b[7m", "\x1b[8m", "\x1b[9m",
        ];
        assert!(xterm.contains(&run(all)), "{xterm:?}");
        let mut standout = all;
        standout[2] = "\x1b[7m";
        let screened = frame_of("xterm-256color", true);
        assert!(screened.contains(&run(standout)), "{screened:?}");
        let linux = frame_of("linux", false);
        let want = [
            "\x1b[1m", "\x1b[2m", "\x1b[7m", "\x1b[5m", "\x1b[7m", "", "",
        ];
        assert!(linux.contains(&run(want)), "{linux:?}");
        let screen = frame_of("screen", false);
        let want = [
            "\x1b[1m", "\x1b[2m", "\x1b[3m", "\x1b[5m", "\x1b[7m", "", "",
        ];
        assert!(screen.contains(&run(want)), "{screen:?}");
        let vt100 = frame_of("vt100", false);
        let want = ["\x1b[1m", "", "\x1b[7m", "\x1b[5m", "\x1b[7m", "", ""];
        assert!(vt100.contains(&run(want)), "{vt100:?}");
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

    /// A one-column cell that a joined character would have made two
    /// columns wide had it fitted (U+FE0F after a narrow character, a
    /// regional indicator pair: the pane's last column) goes out as what
    /// draws one column wide on any terminal: the narrow base without the
    /// joiners, a space for a flag. Sent whole, a terminal that draws it
    /// two wide (Ghostty) wraps it onto the next line, scrolling at the
    /// bottom, or covers the border beside it.
    #[test]
    fn a_cell_that_did_not_fit_wide_goes_out_narrow() {
        let caps = test_caps();
        let mut screen = erased(4);
        screen.cells[1].text = "\u{2764}\u{fe0f}\u{2069}".into();
        screen.cells[2].text = "e\u{301}".into();
        screen.cells[3].text = "\u{1f1ef}\u{1f1f5}".into();
        screen.cursor = Some((4, 0));
        let frame = String::from_utf8(Compositor::default().frame(screen, caps)).unwrap();
        assert!(!frame.contains('\u{fe0f}'), "{frame:?}");
        assert!(!frame.contains('\u{1f1ef}'), "{frame:?}");
        assert!(!frame.contains('\u{1f1f5}'), "{frame:?}");
        assert!(frame.contains("\u{2764}\u{2069}e\u{301} "), "{frame:?}");
        // The pending wrap is put back by writing the last cell again.
        assert!(
            frame.ends_with("\x1b[1;4H \x1b[0m\x1b[?25h\x1b[?2026l"),
            "{frame:?}"
        );
    }

    /// A terminal without `am` never has its bottom-right cell written
    /// (`tty_cell`, tty.c:2259), not even to put the cursor in the pending
    /// wrap there: `tty_cursor` clamps it to the last column.
    #[test]
    fn a_terminal_without_am_never_gets_the_bottom_right_cell() {
        let caps = Caps {
            no_am: true,
            ..test_caps()
        };
        let mut screen = erased(4);
        screen.cells[3].text = "z".into();
        screen.cursor = Some((4, 0));
        let frame = String::from_utf8(Compositor::default().frame(screen, caps)).unwrap();
        assert!(!frame.contains('z'), "{frame:?}");
        assert!(
            frame.ends_with("\x1b[0m\x1b[1;4H\x1b[?25h\x1b[?2026l"),
            "{frame:?}"
        );
    }

    /// A lone regional indicator is one column in the grid (tmux's table)
    /// and two on some terminals (Ghostty): the cursor is moved after it,
    /// the blank after it is written first so the indicator covers it
    /// there, other text after it is written after it (and so wins), and
    /// in the last column it is a space, which no terminal wraps.
    #[test]
    fn a_lone_regional_indicator_moves_nothing_after_it() {
        let caps = test_caps();
        let mut screen = erased(8);
        for (x, t) in [
            (0, "a"),
            (1, "\u{1f1ef}"),
            (3, "\u{1f1f5}"),
            (4, "b"),
            (5, "c"),
            (7, "\u{1f1ef}"),
        ] {
            screen.cells[x].text = t.into();
        }
        // A foreground colour does not show on the blank it covers.
        screen.cells[1].style.fg = Color::Palette(1);
        screen.cursor = Some((0, 0));
        let mut c = Compositor::default();
        let frame = String::from_utf8(c.frame(screen.clone(), caps)).unwrap();
        assert!(
            frame.contains(
                "a\x1b[1;3H \x1b[1;2H\x1b[0m\x1b[31m\u{1f1ef}\x1b[1;4H\x1b[0m\u{1f1f5}\x1b[1;5Hbc"
            ),
            "{frame:?}"
        );
        assert!(frame.contains("bc  \x1b[0m"), "{frame:?}");
        assert_eq!(frame.matches('\u{1f1ef}').count(), 1, "{frame:?}");
        // Only the cell after an indicator changed: the two go out again.
        screen.cells[4].text = "B".into();
        screen.cells[2].text = "x".into();
        let frame = String::from_utf8(c.frame(screen, caps)).unwrap();
        assert!(
            frame.contains("\x1b[1;2H\x1b[0m\x1b[31m\u{1f1ef}\x1b[1;3H\x1b[0mx\u{1f1f5}\x1b[1;5HB"),
            "{frame:?}"
        );
    }

    /// Cells `x` and `x + 1` of the first row as one two-column cell of `text`.
    fn put_wide(screen: &mut Snapshot, x: usize, text: &str) {
        screen.cells[x].text = text.into();
        screen.cells[x].width = 2;
        screen.cells[x + 1].width = 0;
    }

    /// A two-column cell whose first character a terminal draws one column
    /// wide on its own (D-045: U+1F590, which tmux joins to a skin tone, a
    /// cell `codepoint-widths` widened) gets its second column written as a
    /// blank first, which a terminal drawing the cell one wide keeps, and a
    /// cursor move after it. An emoji drawn two wide anyway gets neither.
    #[test]
    fn a_wide_cell_drawn_narrow_blanks_its_second_column() {
        let caps = test_caps();
        let mut screen = erased(8);
        screen.cells[0].text = "a".into();
        put_wide(&mut screen, 1, "\u{1f590}\u{1f3fb}");
        screen.cells[3].text = "b".into();
        put_wide(&mut screen, 4, "\u{1f44d}\u{1f3fb}");
        screen.cells[6].text = "c".into();
        screen.cursor = Some((0, 0));
        let frame = String::from_utf8(Compositor::default().frame(screen, caps)).unwrap();
        assert!(
            frame.contains("a\x1b[1;3H \x1b[1;2H\u{1f590}\u{1f3fb}\x1b[1;4Hb\u{1f44d}\u{1f3fb}c"),
            "{frame:?}"
        );
    }

    /// A skin-tone modifier in a cell of its own after U+270C (not one of
    /// tmux's bases): Ghostty would join it to the emoji as it is printed,
    /// so the emoji's cell is blanked, the modifier written after the
    /// blank, then the emoji again (D-045).
    #[test]
    fn a_lone_skin_tone_is_written_before_the_emoji_before_it() {
        let caps = test_caps();
        let mut screen = erased(6);
        screen.cells[0].text = "\u{270c}".into();
        put_wide(&mut screen, 1, "\u{1f3ff}");
        screen.cells[3].text = "z".into();
        screen.cursor = Some((0, 0));
        let mut c = Compositor::default();
        let frame = String::from_utf8(c.frame(screen.clone(), caps)).unwrap();
        assert!(
            frame.contains("\x1b[1;1H \x1b[1;2H\u{1f3ff}\x1b[1;1H\u{270c}\x1b[1;4Hz"),
            "{frame:?}"
        );
        // Only the emoji changed: written over, it joins nothing after it.
        screen.cells[0].text = "\u{261d}".into();
        let frame = String::from_utf8(c.frame(screen.clone(), caps)).unwrap();
        assert!(frame.contains("\x1b[1;1H\u{261d}"), "{frame:?}");
        assert!(!frame.contains('\u{1f3ff}'), "{frame:?}");
        // Nothing before it to join: written as any cell.
        screen.cells[0].text = "".into();
        let frame = String::from_utf8(Compositor::default().frame(screen, caps)).unwrap();
        assert!(frame.contains("\x1b[1;1H \u{1f3ff}z"), "{frame:?}");
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
            attrs: Attrs::XTERM,
            hyperlinks: false,
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
            attrs: Attrs::XTERM,
            hyperlinks: false,
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
