//! Safe wrapper over one libghostty-vt terminal, shaped like a tmux pane.
//!
//! Grid state follows Ghostty (D4). Everything a pane program can observe from
//! outside the grid (query replies, cursor position, title, path, history
//! size) follows tmux 3.8, read from `input.c`, `format.c` and `screen.c`.

use std::cell::{Cell as StdCell, RefCell};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use libghostty_vt::screen::{Screen, TrackedGridRef};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributes, DeviceType, Mode, Point, PointCoordinate, PointSpace,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes, SizeReportSize, TertiaryDeviceAttributes,
};
use libghostty_vt::{Terminal, TerminalOptions};

use crate::capture::FrozenRow;

/// Where a history row is read from.
pub(crate) enum HistoryRow<'a> {
    /// Row of Ghostty's active screen (history included).
    Screen(usize),
    /// The primary history copied when the alternate screen came up.
    Frozen(&'a FrozenRow),
}

/// A line flag OSC 133 sets on the cursor's line (input.c:3264-3330).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineMark {
    /// `GRID_LINE_START_PROMPT`: `OSC 133;A`, `N`, or `P` without `k=s`.
    Prompt,
    /// `GRID_LINE_START_OUTPUT`: `OSC 133;C`.
    Output,
}

/// The mark an OSC 133 payload (after `133;`) sets, if any.
fn osc133_mark(payload: &[u8]) -> Option<LineMark> {
    match payload.first()? {
        b'A' | b'N' => Some(LineMark::Prompt),
        b'P' => {
            // `k=s` is a secondary prompt (GRID_LINE_SECOND_PROMPT).
            let at = payload.windows(4).position(|w| w == b";k=s");
            let secondary = at.is_some_and(|i| matches!(payload.get(i + 4), None | Some(b';')));
            (!secondary).then_some(LineMark::Prompt)
        }
        b'C' => Some(LineMark::Output),
        _ => None,
    }
}

/// Pixel size of one cell in XTWINOPS replies: tmux's `DEFAULT_XPIXEL` and
/// `DEFAULT_YPIXEL` (tmux.h:122-123), used when no client reports a size.
const CELL_WIDTH_PX: u32 = 16;
const CELL_HEIGHT_PX: u32 = 32;

/// Bytes in one pooled Ghostty page (`PagePool.item_size`, PageList.zig):
/// the standard page layout rounded up to Zig's `std.heap.page_size_min`
/// (`Page.layout`, page.zig), 4 KiB on Linux and x86-64 macOS, 16 KiB on
/// aarch64 macOS. Measured by probing retention (unchanged from ghostty
/// a887df42c to 3bd54e025); `history_budget_steps_by_page` guards it.
#[cfg(test)]
const PAGE_BYTES: usize = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    409_600
} else {
    401_408
};
/// Bytes of a standard page left for row headers and cells once the
/// styles, graphemes, strings and hyperlinks are laid out
/// (`Capacity.availableBytesForGrid`, page.zig): the page minus a fixed
/// 26 128 bytes of metadata. Fitted exactly to the rows per page Ghostty
/// keeps at every width from 1 to 700 columns and at 800-20 000;
/// `page_rows_matches_ghostty` guards it.
const PAGE_GRID_BYTES: usize = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    409_600 - 26_128
} else {
    401_408 - 26_128
};
/// Bytes per cell and per row header (`@sizeOf(Cell)`, `@sizeOf(Row)`).
const CELL_BYTES: usize = 8;
/// Alignment of a page's cell array: Zig's `std.atomic.cache_line`, 128 on
/// x86-64 and aarch64 (`cells_align`, page.zig).
const CELLS_ALIGN: usize = 128;

/// One pane's terminal. Owns the libghostty terminal; `'static` for callers.
#[derive(Debug)]
pub struct PaneTerm {
    pub(crate) term: Terminal<'static, 'static>,
    replies: Rc<RefCell<Vec<u8>>>,
    /// BELs seen since the last [`PaneTerm::take_bells`] (input.c
    /// `input_c0_dispatch`, `case '\007'`).
    bells: Rc<std::cell::Cell<u32>>,
    /// A title set by the server (`select-pane -T`); the next OSC 0/2 from
    /// the program replaces it, as both write the one `s->title`.
    title_override: Rc<RefCell<Option<String>>>,
    history_lines: usize,
    /// tmux's `GRID_HISTORY`: the pane was made with a non-zero limit
    /// (`grid_create`, grid.c:381-382), so it keeps history for good,
    /// whatever its limit becomes. A pane made without never gets any.
    history: bool,
    /// Rows [`PaneTerm::set_history_limit`] collected while the alternate
    /// screen was up, taken off again when the primary screen's history is
    /// re-read on the way back.
    alt_collected: usize,
    /// Rows one standard Ghostty page holds at the current width.
    page_rows: usize,
    /// The line limit last given to Ghostty (see [`PaneTerm::retain`]).
    line_limit: Option<usize>,
    /// tmux's `hsize` for the primary grid, modelled (see [`PaneTerm::feed`]).
    hsize: usize,
    /// Rows ever scrolled into the primary history: the newest history row
    /// has id `scrolled - 1` (tmux's `hscrolled`, never reduced by
    /// collection), so a history row keeps its id while more scroll in.
    scrolled: u64,
    /// Changes whenever history ids stop naming the same rows: a clear,
    /// a reset or a resize (tmux's `scroll_generation`, which a clear bumps).
    generation: u64,
    /// When each retained history row scrolled in (`grid_line_time`), the
    /// first entry for history id `times_base`.
    times: VecDeque<i64>,
    times_base: u64,
    /// Tracked pin on the first active row, re-armed before every piece fed,
    /// to count the rows a piece scrolled into history.
    pin: Option<TrackedGridRef>,
    scan: AltModeScanner,
    /// tmux's cursor style state, shared with the scanner and the reply hook.
    cursor: Rc<StdCell<CursorState>>,
    /// The newest `hsize` history rows, copied when the alternate screen was
    /// entered: libghostty only reads the active screen, and tmux keeps
    /// showing the primary history while an alternate screen is up.
    frozen: Option<Vec<FrozenRow>>,
    /// The oldest row of tmux's primary history (the active top when it
    /// has none), pinned as the alternate screen came up, and the history
    /// rows Ghostty kept then. Ghostty reflows the primary screen at every
    /// resize under the alternate screen; tmux leaves it alone and
    /// reflows it once on the way back (screen.c `screen_alternate_off`).
    /// Either way tmux's history starts at this row again.
    alt_entry: Option<(TrackedGridRef, usize)>,
    /// ED3 arrived under the alternate screen: tmux cleared the primary
    /// history (input.c:1758), which libghostty cannot reach until the
    /// primary screen is back.
    clear_primary_history: bool,
    /// The program did something tmux answers with a full pane redraw
    /// (`PANE_REDRAW`): a screen switch (screen-write.c:3154-3197) or RIS
    /// (input.c:1411), since the last [`PaneTerm::take_redraw`].
    redraw: bool,
    /// The lines OSC 133 marked on the primary screen, each pinned to its
    /// row so the mark follows scrolling and reflow as tmux's line flags
    /// follow the line. Ghostty's own semantic prompt state marks rows
    /// differently (a `C` then an `A` on one line keeps only the prompt).
    marks: Vec<(TrackedGridRef, LineMark)>,
    /// The marks of the history copied when the alternate screen came up.
    frozen_marks: Vec<(usize, LineMark)>,
}

impl PaneTerm {
    /// Create a terminal of `cols` x `rows` keeping `history_lines` lines of
    /// scrollback (tmux's `history-limit`). Zero sizes are raised to one.
    ///
    /// Ghostty keeps history by line count (`OPT_SCROLLBACK_MAX_LINES`), see
    /// [`PaneTerm::retain`]; the byte limit is off. `history_lines` 0 uses
    /// a byte limit of 0, which keeps no history at all: a line limit can't
    /// go below one page.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    #[must_use]
    pub fn new(cols: u16, rows: u16, history_lines: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut term = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: if history_lines == 0 { 0 } else { usize::MAX },
        })
        .expect("libghostty terminal allocation");

        let replies = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&replies);
        let cursor = Rc::new(StdCell::new(CursorState::default()));
        let cursor_seen = Rc::clone(&cursor);
        let cursor_scan = Rc::clone(&cursor);
        let bells = Rc::new(std::cell::Cell::new(0u32));
        let belled = Rc::clone(&bells);
        let title_override = Rc::new(RefCell::new(None));
        let overridden = Rc::clone(&title_override);
        let registered = term
            // Every reply Ghostty wants to send goes through `tmux_reply`.
            .on_pty_write(move |term, data| {
                if let Some(reply) = tmux_reply(term, cursor_seen.get(), data) {
                    sink.borrow_mut().extend_from_slice(&reply);
                }
            })
            .and_then(|t| {
                // Registering the handler is what makes Ghostty ask; the
                // content is replaced by `tmux_reply` (input.c:1582-1603).
                t.on_device_attributes(|_| {
                    Some(DeviceAttributes {
                        primary: PrimaryDeviceAttributes::new(ConformanceLevel::VT220, &[]),
                        secondary: SecondaryDeviceAttributes {
                            device_type: DeviceType::VT220,
                            firmware_version: 0,
                            rom_cartridge: 0,
                        },
                        tertiary: TertiaryDeviceAttributes::default(),
                    })
                })
            })
            .and_then(|t| {
                t.on_title_changed(move |_| {
                    overridden.borrow_mut().take();
                })
            })
            .and_then(|t| t.on_bell(move |_| belled.set(belled.get().saturating_add(1))))
            // XTVERSION: `DCS > | tmux 3.8 ST` (input.c:1885-1890).
            .and_then(|t| t.on_xtversion(|_| Some("tmux 3.8")))
            // XTWINOPS 14/16/18 t (input.c:2160-2183) with tmux's default
            // cell size.
            .and_then(|t| {
                t.on_size(|term| {
                    Some(SizeReportSize {
                        rows: term.rows().ok()?,
                        columns: term.cols().ok()?,
                        cell_width: CELL_WIDTH_PX,
                        cell_height: CELL_HEIGHT_PX,
                    })
                })
            });
        registered.expect("libghostty effect registration");

        // tmux implements neither Kitty graphics nor the Glyph Protocol, so
        // keep their state (and memory) out. Both calls only fail when the
        // feature is compiled out, which is the outcome we want anyway.
        let _ = term.set_kitty_image_storage_limit(0);
        let _ = term.set_glyph_protocol_enabled(false);

        let mut pane = Self {
            term,
            replies,
            bells,
            title_override,
            history_lines,
            history: history_lines > 0,
            alt_collected: 0,
            page_rows: page_rows(cols),
            line_limit: None,
            hsize: 0,
            scrolled: 0,
            generation: 0,
            times: VecDeque::new(),
            times_base: 0,
            pin: None,
            cursor,
            scan: AltModeScanner {
                cursor: cursor_scan,
                ..AltModeScanner::default()
            },
            frozen: None,
            alt_entry: None,
            clear_primary_history: false,
            redraw: false,
            marks: Vec::new(),
            frozen_marks: Vec::new(),
        };
        pane.retain();
        pane
    }

    /// Give Ghostty the line limit: one page more than tmux's history can
    /// hold, `max(history-limit, hsize)` (a narrowing reflow leaves `hsize`
    /// above the limit until scrolling collects it, as in tmux; grid.c
    /// `grid_reflow` never collects).
    ///
    /// Ghostty prunes whole pages from the top and never the page holding
    /// the active area's first row, so it keeps between `limit + 1 -
    /// page_rows` and `limit` history rows (PageList.zig `Limits.enforce`):
    /// at least `max(history-limit, hsize) + 1`, so every row tmux still
    /// has, and at most one page over it, whatever the styling or width.
    fn retain(&mut self) {
        if !self.history {
            return;
        }
        let limit = Some(self.history_lines.max(self.hsize) + self.page_rows);
        if limit != self.line_limit {
            let _ = self.term.set_scrollback_max_lines(limit);
            self.line_limit = limit;
        }
    }

    /// Feed bytes the pane program wrote.
    ///
    /// History size follows tmux's collection sawtooth (`grid_collect_history`,
    /// grid.c:449-474, called per scrolled line from grid-view.c:85,110): once
    /// `hsize` reaches the limit, the oldest `limit / 10` lines go before the
    /// next line is added. Ghostty prunes by page and cannot say how many
    /// rows scrolled, so the bytes are fed in pieces of at most `limit / 2`
    /// (a byte scrolls at most one row), and the rows each piece scrolled are
    /// the distance from a pin on the first active row to the active top. The
    /// model is exact while rows scroll off the top of a full-width region;
    /// see `DIVERGENCES-PENDING.md` for where Ghostty's grid differs.
    pub fn feed(&mut self, bytes: &[u8]) {
        let step = if self.history {
            // A pane lowered to limit 0 still keeps the row tmux lets in
            // per line; a byte at a time would be slow.
            if self.history_lines == 0 {
                16
            } else {
                (self.history_lines / 2).max(1)
            }
        } else {
            usize::MAX
        };
        let mut i = 0;
        while i < bytes.len() {
            let mut j = i;
            let mut event = None;
            while j < bytes.len() && j - i < step {
                if let Some(e) = self.scan.push(bytes[j]) {
                    event = Some(e);
                    break;
                }
                j += 1;
            }
            self.feed_piece(&bytes[i..j]);
            i = j;
            if let Some(e) = event {
                let alternate = self.alternate_on();
                match e {
                    // The alternate screen is about to switch: last chance to
                    // read the primary history.
                    ScanEvent::AltMode(b'h') if !self.alternate_on() => {
                        self.frozen = Some(self.freeze_history());
                        self.frozen_marks = self.line_marks();
                        self.alt_entry = self.pin_oldest().map(|pin| (pin, self.retained_rows()));
                    }
                    // tmux's ED3 clears the grid's history, which under the
                    // alternate screen is the primary one (input.c:1758).
                    ScanEvent::ClearHistory if self.alternate_on() => {
                        self.frozen = Some(Vec::new());
                        self.hsize = 0;
                        self.clear_primary_history = true;
                    }
                    _ => {}
                }
                if matches!(e, ScanEvent::ClearHistory | ScanEvent::Reset) {
                    self.generation += 1;
                }
                // RIS clears the grid, its line flags with it.
                if e == ScanEvent::Reset && !self.alternate_on() {
                    self.marks.clear();
                }
                // tmux's RIS (input.c:1407-1412, screen_write_reset) keeps
                // the title; Ghostty's full reset clears it.
                let title = (e == ScanEvent::Reset).then(|| self.title());
                self.feed_piece(&bytes[i..=i]);
                if let Some(title) = title {
                    *self.title_override.borrow_mut() = Some(title);
                }
                i += 1;
                if e == ScanEvent::Reset {
                    let mut c = self.cursor.get();
                    c.reset();
                    self.cursor.set(c);
                }
                if e == ScanEvent::Decrqss {
                    self.answer_decrqss();
                }
                if let ScanEvent::Osc133(mark) = e {
                    self.mark_cursor_line(mark);
                }
                if matches!(e, ScanEvent::AltMode(_) | ScanEvent::Reset)
                    && (e == ScanEvent::Reset || self.alternate_on() != alternate)
                {
                    self.redraw = true;
                }
                if self.scan.take_resync() {
                    self.sync_charset();
                }
            }
        }
    }

    /// Answer the DECRQSS the scanner just saw (`input_handle_decrqss`,
    /// input.c:2553-2617): `DCS 1 $ r SP q Ps SP q ST` for a request that
    /// starts `SP q`, `DCS 0 $ r ST` for any other. Ghostty's own DECRQSS
    /// replies (SGR, DECSTBM, DECSLRM, cursor) are dropped by `tmux_reply`:
    /// tmux knows only the cursor style.
    fn answer_decrqss(&mut self) {
        let Some(recognised) = self.scan.decrqss.take() else {
            return;
        };
        let reply = if recognised {
            format!("\x1bP1$r q{} q\x1b\\", self.cursor.get().decrqss_ps())
        } else {
            "\x1bP0$r\x1b\\".to_owned()
        };
        self.replies
            .borrow_mut()
            .extend_from_slice(reply.as_bytes());
    }

    /// Make Ghostty's DECSCA protection bit, which every printed cell copies
    /// from the cursor, equal to tmux's `GRID_ATTR_CHARSET` for the next
    /// print. `CSI 1 " q` first so the most recent protection mode is DEC
    /// and ED/EL/ECH ignore the bit (Ghostty `setProtectedMode`); only
    /// DECSED/DECSEL read it, and tmux implements neither.
    fn sync_charset(&mut self) {
        self.term.vt_write(b"\x1b[1\"q");
        if !self.scan.charset.acs() {
            self.term.vt_write(b"\x1b[0\"q");
        }
    }

    fn feed_piece(&mut self, piece: &[u8]) {
        if piece.is_empty() {
            return;
        }
        if !self.history {
            self.term.vt_write(piece);
            return;
        }
        let track = !self.alternate_on();
        if track {
            self.arm_pin();
        }
        self.term.vt_write(piece);
        if !self.alternate_on() {
            self.frozen = None;
            let entry = self.alt_entry.take();
            if std::mem::take(&mut self.clear_primary_history) {
                // Back on the primary screen: apply the ED3 that arrived
                // under the alternate one.
                self.term.vt_write(b"\x1b[3J");
                self.hsize = 0;
            } else if track {
                self.account_scroll();
            } else {
                // Back from the alternate screen (or a reset): the primary
                // history may have changed under us (a resize reflowed it,
                // a reset cleared it, a budget lowered while the alternate
                // screen was up pruned it).
                if let Some((oldest, retained)) = entry {
                    self.history_from(&oldest, retained);
                }
                self.clamp_history_to_retained();
            }
            self.alt_collected = 0;
            // Resizes under the alternate screen leave the limit lifted.
            self.retain();
        }
    }

    /// Pin the oldest row of tmux's primary history (the active top when it
    /// has none). Primary screen only: a pin belongs to the active screen.
    fn pin_oldest(&mut self) -> Option<TrackedGridRef> {
        let y = self.retained_rows().saturating_sub(self.hsize);
        let y = u32::try_from(y).ok()?;
        self.term
            .track_grid_ref(Point::Screen(PointCoordinate { x: 0, y }))
            .ok()
    }

    /// After a reflow (`retained` history rows before it): tmux's history
    /// starts at `oldest` again. A pin Ghostty pruned leaves `hsize` as it
    /// was. Ghostty's reflow does not say which row went where, so the
    /// rows' times are lost.
    fn history_from(&mut self, oldest: &TrackedGridRef, retained: usize) {
        let Some(hidden) = oldest
            .point(PointSpace::Screen)
            .ok()
            .flatten()
            .and_then(|p| usize::try_from(p.y).ok())
        else {
            return;
        };
        let after = self.retained_rows();
        // What `set_history_limit` collected under the alternate screen.
        let hsize = after
            .saturating_sub(hidden)
            .saturating_sub(self.alt_collected);
        if hsize == self.hsize && after == retained {
            return;
        }
        self.hsize = hsize;
        self.scrolled = (self.scrolled + after as u64).saturating_sub(retained as u64);
        self.times.clear();
        self.times_base = self.scrolled;
    }

    fn arm_pin(&mut self) {
        let top = Point::Active(PointCoordinate { x: 0, y: 0 });
        match &mut self.pin {
            Some(pin) => {
                let _ = pin.set(&mut self.term, top);
            }
            None => self.pin = self.term.track_grid_ref(top).ok(),
        }
    }

    /// Add the rows the last piece scrolled into history to the model.
    fn account_scroll(&mut self) {
        let retained = self.term.scrollback_rows().unwrap_or(0);
        let above_pin = self
            .pin
            .as_ref()
            .and_then(|p| p.point(PointSpace::Screen).ok().flatten())
            .and_then(|c| usize::try_from(c.y).ok());
        // A pin that lost its page (more scrolled than retained) leaves the
        // count unknown; assume everything retained is new.
        let scrolled = retained.saturating_sub(above_pin.unwrap_or(0));
        self.hsize = tmux_hsize(self.hsize, self.history_lines, scrolled).min(retained);
        self.retain();
        self.scrolled += scrolled as u64;
        if scrolled > 0 {
            // grid_line_set_time: the server loop's `current_time`.
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
            self.times.extend(std::iter::repeat_n(now, scrolled));
            while self.times.len() > retained {
                self.times.pop_front();
                self.times_base += 1;
            }
            self.times_base = self.times_base.max(self.scrolled - self.times.len() as u64);
        }
    }

    /// The id of history row `y` (`0..history_size()`, oldest first), stable
    /// while more rows scroll in, until [`PaneTerm::history_generation`]
    /// changes.
    #[must_use]
    pub fn history_id(&self, y: usize) -> u64 {
        (self.scrolled + y as u64).saturating_sub(self.history_size() as u64)
    }

    /// See [`PaneTerm::history_id`].
    #[must_use]
    pub fn history_generation(&self) -> u64 {
        self.generation
    }

    /// When history row `id` scrolled in, seconds since the epoch, 0 when
    /// unknown (`grid_line_time`).
    #[must_use]
    pub fn history_time(&self, id: u64) -> i64 {
        id.checked_sub(self.times_base)
            .and_then(|i| self.times.get(usize::try_from(i).ok()?))
            .copied()
            .unwrap_or(0)
    }

    /// History row `id` while it is still retained: the primary screen's,
    /// or the copy taken when the alternate screen came up.
    pub(crate) fn history_screen_row(&self, id: u64) -> Option<HistoryRow<'_>> {
        let back = usize::try_from(self.scrolled.checked_sub(id)?).ok()?;
        if back == 0 {
            return None;
        }
        if self.alternate_on() {
            let frozen = self.frozen.as_ref()?;
            return frozen
                .get(frozen.len().checked_sub(back)?)
                .map(HistoryRow::Frozen);
        }
        let retained = self.term.scrollback_rows().unwrap_or(0);
        retained.checked_sub(back).map(HistoryRow::Screen)
    }

    /// Copy the newest `hsize` primary history rows.
    fn freeze_history(&self) -> Vec<FrozenRow> {
        let cols = usize::from(self.cols());
        let hidden = self.term.scrollback_rows().unwrap_or(0) - self.hsize;
        (0..self.hsize)
            .map(|y| FrozenRow::capture(&self.term, hidden + y, cols))
            .collect()
    }

    /// Pin `mark` to the cursor's line. Under the alternate screen the
    /// mark would be on the alternate grid, which copy mode never reads
    /// after the screen goes, so it is not kept.
    fn mark_cursor_line(&mut self, mark: LineMark) {
        if self.alternate_on() {
            return;
        }
        let y = self.term.cursor_y().unwrap_or(0);
        let point = Point::Active(PointCoordinate {
            x: 0,
            y: u32::from(y),
        });
        if let Ok(pin) = self.term.track_grid_ref(point) {
            self.marks.push((pin, mark));
        }
        // Drop the marks of rows gone from history now and then.
        if self.marks.len().is_power_of_two() && self.marks.len() >= 64 {
            let floor = self.hidden_rows();
            self.marks.retain(|(pin, _)| {
                pin.point(PointSpace::Screen)
                    .ok()
                    .flatten()
                    .is_some_and(|p| usize::try_from(p.y).is_ok_and(|y| y >= floor))
            });
        }
    }

    /// Rows Ghostty retains above tmux's history.
    fn hidden_rows(&self) -> usize {
        self.term
            .scrollback_rows()
            .unwrap_or(0)
            .saturating_sub(self.hsize)
    }

    /// The lines OSC 133 marked, as rows of tmux's grid (see
    /// [`PaneTerm::row`]), oldest first. Under the alternate screen, those
    /// of the primary history copied on entry.
    #[must_use]
    pub fn line_marks(&self) -> Vec<(usize, LineMark)> {
        if self.alternate_on() {
            return self.frozen_marks.clone();
        }
        let hidden = self.hidden_rows();
        let last = self.hsize + usize::from(self.rows());
        let mut out: Vec<(usize, LineMark)> = self
            .marks
            .iter()
            .filter_map(|(pin, mark)| {
                let p = pin.point(PointSpace::Screen).ok().flatten()?;
                let y = usize::try_from(p.y).ok()?.checked_sub(hidden)?;
                (y < last).then_some((y, *mark))
            })
            .collect();
        out.sort_by_key(|&(y, _)| y);
        out
    }

    /// History row `y` (0 = oldest tmux still has) while the alternate screen
    /// is up.
    pub(crate) fn frozen_row(&self, y: usize) -> Option<&FrozenRow> {
        self.frozen.as_ref()?.get(y)
    }

    /// Resize the pane. Primary-screen content reflows (Ghostty).
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let before = self.term.scrollback_rows().unwrap_or(0);
        let same_width = cols.max(1) == self.cols();
        let on_primary = !self.alternate_on();
        // The oldest row tmux has (the active top when it has none): rows
        // above it stay hidden however the reflow splits or joins them.
        let oldest = if on_primary { self.pin_oldest() } else { None };
        // Lift the line limit so the reflow keeps every row; `retain`
        // sets it again for the new width. Under the alternate screen it
        // stays lifted until the primary screen is back: `hsize` is
        // tmux's unreflowed count there, and pruning to it would drop the
        // rows a narrowing reflow split off (see `alt_entry`).
        if self.history {
            let _ = self.term.set_scrollback_max_lines(None);
            self.line_limit = None;
        }
        self.term
            .resize(cols.max(1), rows.max(1), CELL_WIDTH_PX, CELL_HEIGHT_PX)
            .expect("libghostty resize");
        self.page_rows = page_rows(cols.max(1));
        if on_primary {
            // Reflow moves rows in and out of history: tmux's history
            // starts at the row that was its oldest. (While the alternate
            // screen is up the frozen copy is not reflowed.)
            let after = self.term.scrollback_rows().unwrap_or(0);
            let hidden = oldest
                .and_then(|pin| pin.point(PointSpace::Screen).ok().flatten())
                .and_then(|p| usize::try_from(p.y).ok())
                .unwrap_or(before.saturating_sub(self.hsize));
            self.hsize = after.saturating_sub(hidden);
            let scrolled = (self.scrolled + after as u64).saturating_sub(before as u64);
            if same_width {
                // No reflow: history rows keep their ids and times; rows a
                // shorter screen pushed into history have none
                // (screen_resize_y, screen.c:449-455), rows a taller one
                // pulled back lose theirs.
                if scrolled >= self.scrolled {
                    let pushed = usize::try_from(scrolled - self.scrolled).unwrap_or(0);
                    self.times.extend(std::iter::repeat_n(0, pushed));
                } else {
                    let pulled = usize::try_from(self.scrolled - scrolled).unwrap_or(0);
                    self.times.truncate(self.times.len().saturating_sub(pulled));
                }
                while self.times.len() > after {
                    self.times.pop_front();
                    self.times_base += 1;
                }
                self.scrolled = scrolled;
                self.times_base = self.times_base.max(self.scrolled - self.times.len() as u64);
            } else {
                // Ghostty's reflow does not say which row went where.
                self.scrolled = scrolled;
                self.times.clear();
                self.times_base = self.scrolled;
            }
        }
        if on_primary {
            self.retain();
            self.clamp_history_to_retained();
        }
        self.generation += 1;
    }

    /// Width in cells.
    #[must_use]
    pub fn cols(&self) -> u16 {
        self.term.cols().unwrap_or(1)
    }

    /// Height in cells.
    #[must_use]
    pub fn rows(&self) -> u16 {
        self.term.rows().unwrap_or(1)
    }

    /// Cursor `(x, y)` as tmux reports it (`#{cursor_x}`, `#{cursor_y}`, CPR).
    ///
    /// Differs from Ghostty only while a wrap is pending: tmux's `cx` is then
    /// the column count, Ghostty keeps the last column plus a flag
    /// (format.c:1881-1894; reference `printf` of 20 chars on 20 columns
    /// reports `cursor_x` 20).
    #[must_use]
    pub fn cursor(&self) -> (u16, u16) {
        tmux_cursor(&self.term)
    }

    /// Pane title as tmux stores it (`screen_set_title`, screen.c:264-274).
    #[must_use]
    pub fn title(&self) -> String {
        if let Some(t) = self.title_override.borrow().as_ref() {
            return t.clone();
        }
        self.term
            .title()
            .map(|t| clean_name(t, true))
            .unwrap_or_default()
    }

    /// Set the title from the server side (`select-pane -T`,
    /// `split-window -T`): `screen_set_title(s, title, 0)`, a trusted
    /// clean, so `#(` stays.
    pub fn set_title(&mut self, title: &str) {
        *self.title_override.borrow_mut() = Some(clean_name(title, false));
    }

    /// Path from OSC 7 as tmux stores it (`screen_set_path`, screen.c:278-288).
    #[must_use]
    pub fn pwd(&self) -> String {
        self.term
            .pwd()
            .map(|p| clean_name(p, true))
            .unwrap_or_default()
    }

    /// DECCKM is set: cursor keys are sent as `ESC O x` (tmux's
    /// `MODE_KCURSOR`, input-keys.c:652-655).
    #[must_use]
    pub fn app_cursor(&self) -> bool {
        self.term.mode(Mode::DECCKM).unwrap_or(false)
    }

    /// DECKPAM (`ESC =`): keypad keys are sent as `ESC O x` (tmux's
    /// `MODE_KKEYPAD`, input-keys.c:648-649).
    #[must_use]
    pub fn app_keypad(&self) -> bool {
        self.term.mode(Mode::KEYPAD_KEYS).unwrap_or(false)
    }

    /// `?1004h`: the pane asked for focus reports (`MODE_FOCUSON`).
    #[must_use]
    pub fn focus_events(&self) -> bool {
        self.term.mode(Mode::FOCUS_EVENT).unwrap_or(false)
    }

    /// Bracketed paste markers are accepted by the pane (input-keys.c).
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false)
    }

    /// True while the alternate screen is active (`#{alternate_on}`).
    #[must_use]
    pub fn alternate_on(&self) -> bool {
        matches!(self.term.active_screen(), Ok(Screen::Alternate))
    }

    /// `#{history_size}`: tmux's `hsize`, which saw-tooths between 90% of the
    /// limit and the limit (see [`PaneTerm::feed`]). Unchanged while the
    /// alternate screen is on: tmux keeps the primary history.
    #[must_use]
    pub fn history_size(&self) -> usize {
        if self.alternate_on() && self.frozen.is_none() {
            0
        } else {
            self.hsize
        }
    }

    /// History rows Ghostty retains on the active screen: at least
    /// [`PaneTerm::history_size`] and at most one page more than
    /// `max(history-limit, history_size)` (see [`PaneTerm::retain`]).
    #[must_use]
    pub fn retained_rows(&self) -> usize {
        self.term.scrollback_rows().unwrap_or(0)
    }

    /// Cap the bytes Ghostty retains for history (`OPT_SCROLLBACK_MAX_BYTES`),
    /// on top of the line limit that normally bounds it (the pane's own
    /// byte limit is off, see [`PaneTerm::new`]). Lowering it prunes whole history pages at once; history ids, times
    /// and `history_size` follow what is still retained. Ghostty keeps at
    /// least about one page whatever the budget. `bytes` is raised to 1:
    /// Ghostty's 0 erases history and stops keeping any, which is
    /// `history-limit 0`'s job (see [`PaneTerm::new`]). On a pane created
    /// with `history_lines` 0 a budget only costs memory: `history_size`
    /// stays 0 whatever Ghostty retains.
    ///
    /// Ghostty applies the budget to the primary screen only, and can only
    /// count the active screen's history rows: under the alternate screen
    /// the model is brought in line when the primary screen is back.
    pub fn set_history_budget(&mut self, bytes: usize) {
        let _ = self.term.set_scrollback_max_bytes(Some(bytes.max(1)));
        if !self.alternate_on() {
            self.clamp_history_to_retained();
        }
    }

    /// `grid_clear_history` (grid.c:515-525), as `clear-history` calls it on
    /// the pane's base grid (cmd-capture-pane.c:426): every history row
    /// goes, the visible screen stays, and history ids stop naming rows
    /// (`scroll_generation`). Under the alternate screen the primary
    /// history is the one cleared, as for ED3 (input.c:1758): it goes
    /// when the primary screen is back.
    ///
    /// Ghostty has no scrollback-clear call that is safe in the middle of
    /// an escape sequence (`CSI 3 J` fed here would corrupt one in flight),
    /// so the history is erased by taking its byte budget to zero, which
    /// "erases retained history", and putting the budget back.
    pub fn clear_history(&mut self) {
        self.generation += 1;
        if self.alternate_on() {
            self.frozen = Some(Vec::new());
            self.hsize = 0;
            self.clear_primary_history = true;
            return;
        }
        if !self.history {
            return;
        }
        let budget = self.term.scrollback_max_bytes().ok().flatten();
        let _ = self.term.set_scrollback_max_bytes(Some(0));
        let _ = self.term.set_scrollback_max_bytes(budget);
        self.hsize = 0;
        self.times.clear();
        self.times_base = self.scrolled;
        self.clamp_history_to_retained();
    }

    /// A new `history-limit` for a running pane (`session_update_history`,
    /// session.c:825-847): the limit is the pane's own from now on, and a
    /// history at or over it is collected at once down to exactly the limit
    /// (`grid_collect_history(gd, 1)`, grid.c:449-474), at least one row
    /// (setting the limit a full history already has costs a row). Raising
    /// the limit gives back nothing. A pane made with limit 0 keeps no
    /// history whatever the limit becomes (see the `history` field).
    ///
    /// Ghostty prunes whole pages, so the rows collected here may stay
    /// retained beneath `history_size` until its line limit catches up. The
    /// copy of the history taken for the alternate screen is trimmed too.
    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_lines = limit;
        if !self.history {
            return;
        }
        let hsize = self.history_size();
        if hsize > 0 && hsize >= limit {
            let ny = (hsize - limit).max(1).min(hsize);
            self.hsize -= ny;
            while self.times.len() > self.hsize {
                self.times.pop_front();
                self.times_base += 1;
            }
            if let Some(frozen) = &mut self.frozen {
                frozen.drain(..ny.min(frozen.len()));
                self.frozen_marks = std::mem::take(&mut self.frozen_marks)
                    .into_iter()
                    .filter_map(|(y, m)| Some((y.checked_sub(ny)?, m)))
                    .collect();
            }
            if self.alternate_on() {
                self.alt_collected += ny;
            }
        }
        if !self.alternate_on() {
            self.retain();
            self.clamp_history_to_retained();
        }
    }

    /// Drop the modelled history rows (`hsize`, times) Ghostty no longer
    /// retains. Primary screen only: `scrollback_rows` reads the active one.
    fn clamp_history_to_retained(&mut self) {
        let retained = self.term.scrollback_rows().unwrap_or(0);
        self.hsize = self.hsize.min(retained);
        while self.times.len() > retained {
            self.times.pop_front();
            self.times_base += 1;
        }
    }

    /// Whether the program asked for a full pane redraw since the last call
    /// (see the `redraw` field).
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// How many BELs the program wrote since the last call.
    pub fn take_bells(&mut self) -> u32 {
        self.bells.replace(0)
    }

    /// The pane's `cursor-style` option (0..=6): what DECRQSS and DECRQM 12
    /// report while the program has set no style (input.c:1666-1680,
    /// 2590-2600). Larger values count as 0.
    pub fn set_cursor_style_option(&mut self, value: u8) {
        let mut c = self.cursor.get();
        c.option = if value > 6 { 0 } else { value };
        self.cursor.set(c);
    }

    /// Take the bytes to write back to the pane program (tmux's replies).
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.replies.borrow_mut())
    }
}

/// tmux's `hsize` after `lines` more rows scrolled into history: before each
/// row, a grid at its limit drops the oldest `limit / 10` (at least one)
/// (`grid_collect_history`, grid.c:449-474).
fn tmux_hsize(mut hsize: usize, limit: usize, lines: usize) -> usize {
    for _ in 0..lines {
        if hsize > 0 && hsize >= limit {
            hsize -= (limit / 10).clamp(1, hsize);
        }
        hsize += 1;
    }
    hsize
}

/// tmux's charset state: G0 and G1 each designated ACS (`ESC ( 0`, `ESC ) 0`)
/// or not (`ESC ( B`, `ESC ) B`), SO/SI selecting one, saved and restored with
/// the cursor (input.c `input_reset_cell`, `input_save_state`,
/// `input_restore_state`, `input_print`).
///
/// Ghostty keeps no charset per cell, so it cannot say which cells tmux
/// would flag `GRID_ATTR_CHARSET`, which `tty_draw_line` never erases. The
/// flag is carried in the cell's DECSCA protection bit instead
/// ([`PaneTerm::sync_charset`]); tmux ignores DECSCA, SPA and EPA, so a
/// program's own use of them is overridden at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Charset {
    g0: bool,
    g1: bool,
    shifted: bool,
    saved: (bool, bool, bool),
}

impl Charset {
    fn acs(self) -> bool {
        if self.shifted { self.g1 } else { self.g0 }
    }

    fn apply(&mut self, op: CharsetOp) {
        match op {
            CharsetOp::G0(on) => self.g0 = on,
            CharsetOp::G1(on) => self.g1 = on,
            CharsetOp::Shift(on) => self.shifted = on,
            CharsetOp::Save => self.saved = (self.g0, self.g1, self.shifted),
            CharsetOp::Restore => (self.g0, self.g1, self.shifted) = self.saved,
            CharsetOp::Reset => *self = Self::default(),
        }
    }
}

/// A sequence that changes [`Charset`] or Ghostty's protection state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharsetOp {
    /// `ESC ( 0` / `ESC ( B`.
    G0(bool),
    /// `ESC ) 0` / `ESC ) B`.
    G1(bool),
    /// SO (true) / SI (false).
    Shift(bool),
    /// DECSC, `CSI s`.
    Save,
    /// DECRC, `CSI u`.
    Restore,
    /// RIS.
    Reset,
}

/// Finds `CSI ? ... h|l` sequences that name the alternate screen (modes 47,
/// 1047, 1049): the one place the primary history must be copied before
/// libghostty hides it. A false positive costs only a copy; a miss loses the
/// history while the alternate screen is up. Also finds `CSI 3 J` (ED3), which
/// tmux applies to the primary history even under the alternate screen, and
/// the sequences that touch [`Charset`].
#[allow(clippy::struct_excessive_bools)] // independent parser flags
#[derive(Debug, Default)]
struct AltModeScanner {
    state: ScanState,
    private: bool,
    other: bool,
    /// The CSI's intermediate byte (0x20-0x2f), if any.
    intermediate: Option<u8>,
    params: Vec<u8>,
    charset: Charset,
    /// Ghostty's protection bit may differ from `charset`: set by a charset
    /// change, and by what changes Ghostty's protection or cursor state
    /// where tmux changes nothing (DECSCA, SPA, EPA, DECSTR, a screen
    /// switch). Fixed once the parser is back in the ground state.
    resync: bool,
    /// The start of the OSC being skipped, kept to find OSC 133.
    osc: Option<Vec<u8>>,
    /// An ESC inside an OSC: a backslash next ends it (ST).
    osc_esc: bool,
    /// Intermediate bytes (0x20-0x2f) in the CSI being scanned.
    interm_count: u8,
    /// The CSI had a byte tmux does not take as a parameter or intermediate
    /// (`:` aside), or a parameter after an intermediate: tmux ignores it.
    csi_ignored: bool,
    /// tmux's view of the DCS in progress, kept apart from `state` (which
    /// follows Ghostty's parser) so DECRQSS is answered the way tmux sees it.
    dcs: Option<Dcs>,
    /// The DECRQSS just terminated: whether tmux recognises the request.
    decrqss: Option<bool>,
    /// The cursor style tmux would hold, shared with the reply handler.
    cursor: Rc<StdCell<CursorState>>,
}

/// tmux's cursor style state: `s->cstyle`, `MODE_CURSOR_BLINKING`,
/// `MODE_CURSOR_BLINKING_SET`, and the `cursor-style` option they fall
/// back to (input.c:1666-1680, 2553-2617).
#[derive(Clone, Copy, Debug, Default)]
struct CursorState {
    /// DECSCUSR shape: 0 default (`SCREEN_CURSOR_DEFAULT`), 1 block,
    /// 2 underline, 3 bar.
    shape: u8,
    blinking: bool,
    blinking_set: bool,
    /// The pane's `cursor-style` option, 0..=6.
    option: u8,
}

impl CursorState {
    /// DECSCUSR `n` (`screen_set_cursor_style`, screen.c:221-253; the
    /// blink-set clear for 0, input.c:1879-1884). Above 6 changes nothing.
    fn decscusr(&mut self, n: u64) {
        match n {
            0 => {
                self.shape = 0;
                self.blinking_set = false;
            }
            1..=6 => {
                self.shape = u8::try_from(n.div_ceil(2)).unwrap_or(1);
                self.blinking = n % 2 == 1;
            }
            _ => {}
        }
    }

    /// `CSI ? 12 h|l` (input.c:1949-1951, 2046-2048).
    fn set_blinking(&mut self, on: bool) {
        self.blinking = on;
        self.blinking_set = true;
    }

    /// RIS resets the mode bits, not the style (`screen_write_reset`,
    /// screen-write.c:430).
    fn reset(&mut self) {
        self.blinking = false;
        self.blinking_set = false;
    }

    /// The `Ps` of the DECRQSS cursor reply (input.c:2571-2604).
    fn decrqss_ps(self) -> u8 {
        if self.shape == 0 {
            return self.option.min(6);
        }
        (self.shape - 1) * 2 + if self.blinking { 1 } else { 2 }
    }

    /// DECRQM 12: 1 blinking, 2 steady (input.c:1666-1680).
    fn blink_report(self) -> u8 {
        let blink = if self.shape != 0 || self.blinking_set {
            self.blinking
        } else {
            matches!(self.option, 1 | 3 | 5)
        };
        if blink { 1 } else { 2 }
    }
}

/// One CSI parameter as tmux's `input_split` types it (input.c:1094-1142).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Param {
    Missing,
    Num(u64),
    /// Contains `:`; `input_get` answers -1 for it.
    Text,
}

/// `input_split`: `None` when tmux gives up on the sequence (a number above
/// `INT_MAX`, or 24 parameters: `param_list[24]`).
fn parse_params(raw: &[u8]) -> Option<Vec<Param>> {
    if raw.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for piece in raw.split(|b| *b == b';') {
        out.push(if piece.is_empty() {
            Param::Missing
        } else if piece.contains(&b':') {
            Param::Text
        } else {
            let n: u64 = std::str::from_utf8(piece).ok()?.parse().ok()?;
            if n > i32::MAX as u64 {
                return None;
            }
            Param::Num(n)
        });
        if out.len() == 24 {
            return None;
        }
    }
    Some(out)
}

/// tmux's DCS collection (input.c:632-720): just enough to know whether the
/// string is `DCS $ q ... ST` and what it asks.
#[derive(Debug, Default)]
struct Dcs {
    stage: DcsStage,
    /// `ESC` seen in the handler stage.
    escaped: bool,
    interm: Vec<u8>,
    params: usize,
    /// The first bytes of the data (the final byte first).
    head: Vec<u8>,
    data_len: usize,
    discard: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DcsStage {
    #[default]
    Enter,
    Param,
    Intermediate,
    Handler,
    Ignore,
}

/// `input_buffer_size` (`INPUT_BUF_DEFAULT_SIZE`, tmux.h): the data buffer
/// doubles while `len + 1 >= space` and the string is discarded past this.
const DCS_INPUT_LIMIT: usize = 1 << 20;

impl Dcs {
    /// One byte after `ESC P`. `Err` when tmux leaves the DCS without
    /// dispatching (CAN, SUB, or ESC outside the handler stage: the ESC
    /// starts a new sequence, `Err(true)`); `Ok(true)` at ST.
    fn push(&mut self, b: u8) -> Result<bool, bool> {
        use DcsStage::{Enter, Handler, Ignore, Intermediate, Param};
        if self.stage == Handler {
            if self.escaped {
                self.escaped = false;
                if b == b'\\' {
                    return Ok(true);
                }
                self.input(b);
            } else if b == 0x1b {
                self.escaped = true;
            } else {
                self.input(b);
            }
            return Ok(false);
        }
        match b {
            0x18 | 0x1a => return Err(false),
            0x1b => return Err(true),
            _ => {}
        }
        match (self.stage, b) {
            (Ignore, _) => {}
            (_, 0x20..=0x2f) => {
                // `interm_buf[4]` keeps 3 bytes.
                if self.interm.len() == 3 {
                    self.discard = true;
                } else {
                    self.interm.push(b);
                }
                self.stage = Intermediate;
            }
            (Enter | Param, 0x30..=0x39 | 0x3b) => {
                // `param_buf[64]` keeps 63 bytes.
                if self.params == 63 {
                    self.discard = true;
                } else {
                    self.params += 1;
                }
                self.stage = Param;
            }
            (Enter | Param, 0x3a) | (Param, 0x3c..=0x3f) | (Intermediate, 0x30..=0x3f) => {
                self.stage = Ignore;
            }
            (Enter, 0x3c..=0x3f) => {
                if self.interm.len() == 3 {
                    self.discard = true;
                } else {
                    self.interm.push(b);
                }
                self.stage = Param;
            }
            (Enter | Param | Intermediate, 0x40..=0x7e) => {
                self.input(b);
                self.stage = Handler;
            }
            _ => {}
        }
        Ok(false)
    }

    fn input(&mut self, b: u8) {
        if self.data_len + 1 >= DCS_INPUT_LIMIT {
            self.discard = true;
            return;
        }
        self.data_len += 1;
        if self.head.len() < 3 {
            self.head.push(b);
        }
    }

    /// At ST: `Some(recognised)` when this is a DECRQSS
    /// (`input_dcs_dispatch`, input.c:2641-2665); tmux only checks that the
    /// request starts with `SP q` (`input_handle_decrqss`, input.c:2564).
    fn into_decrqss(self) -> Option<bool> {
        (!self.discard && self.interm == b"$" && self.head.first() == Some(&b'q'))
            .then(|| self.data_len >= 3 && self.head[1] == b' ' && self.head[2] == b'q')
    }
}

/// What [`AltModeScanner`] found; reported on the sequence's final byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanEvent {
    /// Alternate screen set (`h`) or reset (`l`).
    AltMode(u8),
    /// `CSI 3 J`.
    ClearHistory,
    /// `ESC c` (RIS).
    Reset,
    /// Back in the ground state with [`AltModeScanner::take_resync`] due.
    Resync,
    /// An `OSC 133` that marks the cursor's line ended.
    Osc133(LineMark),
    /// A DECRQSS ended: [`AltModeScanner::decrqss`] says what to answer.
    Decrqss,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    /// `ESC` then an intermediate byte (`(`, `)`, `#`, ...).
    EscIntermediate(u8),
    Csi,
    /// OSC, DCS, APC, PM or SOS: skipped to ST (or BEL).
    Str,
}

impl AltModeScanner {
    /// Feed one byte; `Some` when it ends a sequence of interest.
    fn push(&mut self, b: u8) -> Option<ScanEvent> {
        let dcs_end = self.push_dcs(b);
        let event = self.push_state(b);
        // Ghostty leaves a DCS at any ESC not followed by `\`, where tmux
        // would keep collecting; follow Ghostty so the two never drift.
        if !matches!(self.state, ScanState::Str | ScanState::Esc) {
            self.dcs = None;
        }
        dcs_end.then_some(ScanEvent::Decrqss).or(event)
    }

    /// Feed tmux's view of a DCS; true when a DECRQSS ends here.
    fn push_dcs(&mut self, b: u8) -> bool {
        let Some(dcs) = self.dcs.as_mut() else {
            return false;
        };
        match dcs.push(b) {
            Ok(false) => false,
            Ok(true) => {
                self.decrqss = self.dcs.take().and_then(Dcs::into_decrqss);
                self.decrqss.is_some()
            }
            Err(_) => {
                self.dcs = None;
                false
            }
        }
    }

    fn push_state(&mut self, b: u8) -> Option<ScanEvent> {
        // An OSC's end: BEL, or ST (`ESC \`).
        let osc_esc = std::mem::take(&mut self.osc_esc);
        if self.osc.is_some() {
            let ends = (self.state == ScanState::Str && b == 0x07)
                || (osc_esc && self.state == ScanState::Esc && b == b'\\');
            if ends {
                self.state = ScanState::Ground;
                let osc = self.osc.take().unwrap_or_default();
                return osc
                    .strip_prefix(b"133;")
                    .and_then(osc133_mark)
                    .map(ScanEvent::Osc133);
            }
            match (self.state, b) {
                (ScanState::Str, 0x1b) => self.osc_esc = true,
                (ScanState::Str, 0x18 | 0x1a) => self.osc = None,
                (ScanState::Str, _) => {
                    let osc = self.osc.as_mut().expect("osc");
                    if osc.len() < 32 {
                        osc.push(b);
                    }
                }
                _ => self.osc = None,
            }
        }
        match (self.state, b) {
            (_, 0x18 | 0x1a) | (ScanState::Str, 0x07) => self.state = ScanState::Ground,
            (_, 0x1b) => self.state = ScanState::Esc,
            // Strings are skipped; further intermediates change nothing we track.
            (ScanState::Str, _) | (ScanState::EscIntermediate(_), 0x20..=0x2f) => {}
            // SO and SI are executed anywhere outside a string, even inside
            // an escape sequence.
            (_, 0x0e | 0x0f) => self.charset_op(CharsetOp::Shift(b == 0x0e)),
            // Ground, and other C0 controls inside a sequence (executed).
            (ScanState::Ground, _)
            | (ScanState::Csi | ScanState::Esc | ScanState::EscIntermediate(_), 0x00..=0x1f) => {}
            (ScanState::Esc, b'[') => {
                self.state = ScanState::Csi;
                self.private = false;
                self.other = false;
                self.intermediate = None;
                self.interm_count = 0;
                self.csi_ignored = false;
                self.params.clear();
            }
            (ScanState::Esc, b']') => {
                self.state = ScanState::Str;
                self.osc = Some(Vec::new());
            }
            (ScanState::Esc, b'P') => {
                self.state = ScanState::Str;
                self.dcs = Some(Dcs::default());
            }
            (ScanState::Esc, b'_' | b'^' | b'X') => self.state = ScanState::Str,
            (ScanState::Esc, 0x20..=0x2f) => self.state = ScanState::EscIntermediate(b),
            (ScanState::Esc, _) => {
                self.state = ScanState::Ground;
                match b {
                    b'7' => self.charset_op(CharsetOp::Save),
                    b'8' => self.charset_op(CharsetOp::Restore),
                    b'c' => {
                        self.charset_op(CharsetOp::Reset);
                        return Some(ScanEvent::Reset);
                    }
                    b'V' | b'W' => self.resync = true,
                    _ => {}
                }
            }
            (ScanState::EscIntermediate(i), _) => {
                self.state = ScanState::Ground;
                match (i, b) {
                    (b'(', b'0' | b'B') => self.charset_op(CharsetOp::G0(b == b'0')),
                    (b')', b'0' | b'B') => self.charset_op(CharsetOp::G1(b == b'0')),
                    _ => {}
                }
            }
            (ScanState::Csi, b'?')
                if self.params.is_empty() && !self.private && self.interm_count == 0 =>
            {
                self.private = true;
            }
            (ScanState::Csi, b'0'..=b'9' | b';' | b':') => {
                // `param_buf[64]` keeps 63 bytes, then discards; a parameter
                // after an intermediate goes to csi_ignore (input.c:572-585).
                if self.params.len() < 63 {
                    self.params.push(b);
                } else {
                    self.csi_ignored = true;
                }
                self.csi_ignored |= self.interm_count > 0;
                // A sub-parameter is a string parameter to tmux, not a
                // number (`input_split`): it stays out of the digit tests.
                self.other |= b == b':';
            }
            (ScanState::Csi, 0x40..=0x7e) => {
                self.state = ScanState::Ground;
                return self.csi_event(b);
            }
            (ScanState::Csi, 0x20..=0x2f) => {
                self.other = true;
                self.intermediate = Some(b);
                self.interm_count = self.interm_count.saturating_add(1);
            }
            (ScanState::Csi, _) => {
                self.other = true;
                self.csi_ignored = true;
            }
        }
        (self.resync && self.state == ScanState::Ground).then_some(ScanEvent::Resync)
    }

    fn charset_op(&mut self, op: CharsetOp) {
        self.charset.apply(op);
        self.resync = true;
    }

    /// True once if Ghostty's protection bit needs setting from
    /// [`Self::charset`]; only call in the ground state.
    fn take_resync(&mut self) -> bool {
        self.state == ScanState::Ground && std::mem::take(&mut self.resync)
    }

    fn csi_event(&mut self, b: u8) -> Option<ScanEvent> {
        let op = match (self.private, self.intermediate, b) {
            (false, Some(b'"'), b'q') | (false, Some(b'!'), b'p') => {
                self.resync = true;
                return Some(ScanEvent::Resync);
            }
            // DECSCUSR: the table entry is exactly `CSI Ps SP q`.
            (false, Some(b' '), b'q') => {
                if self.interm_count == 1
                    && !self.csi_ignored
                    && let Some(params) = parse_params(&self.params)
                {
                    let n = match params.first() {
                        None | Some(Param::Missing) => Some(0),
                        Some(Param::Num(n)) => Some(*n),
                        Some(Param::Text) => None,
                    };
                    if let Some(n) = n {
                        let mut c = self.cursor.get();
                        c.decscusr(n);
                        self.cursor.set(c);
                    }
                }
                None
            }
            // Private SM/RM: only the blink mode (12) touches our state.
            (true, None, b'h' | b'l') if !self.csi_ignored => {
                if let Some(params) = parse_params(&self.params) {
                    let mut c = self.cursor.get();
                    for _ in params.into_iter().filter(|p| *p == Param::Num(12)) {
                        c.set_blinking(b == b'h');
                    }
                    self.cursor.set(c);
                }
                None
            }
            _ if self.other => None,
            (false, None, b's') => Some(CharsetOp::Save),
            (false, None, b'u') => Some(CharsetOp::Restore),
            _ => None,
        };
        if let Some(op) = op {
            self.charset_op(op);
            return Some(ScanEvent::Resync);
        }
        if !self.private && b == b'J' && matches!(self.params.as_slice(), b"3" | b"3;0") {
            return Some(ScanEvent::ClearHistory);
        }
        if self.private
            && !self.other
            && matches!(b, b'h' | b'l')
            && self
                .params
                .split(|c| *c == b';')
                .any(|p| matches!(p, b"47" | b"1047" | b"1049"))
        {
            // Ghostty keeps a cursor, so a protection bit, per screen.
            self.resync = true;
            return Some(ScanEvent::AltMode(b));
        }
        self.resync.then_some(ScanEvent::Resync)
    }
}

/// Rows one standard Ghostty page holds at `cols` columns
/// (`initialCapacity`, PageList.zig, through `Capacity.adjust`, page.zig):
/// as many row headers and rows of cells as fit in [`PAGE_GRID_BYTES`],
/// with the cell array aligned to [`CELLS_ALIGN`]. This is also Ghostty's
/// smallest line limit (`Limits.minMaxLines`). A width no row fits in gets
/// a bigger page of `std_capacity`'s 215 rows.
fn page_rows(cols: u16) -> usize {
    let row_bytes = CELL_BYTES * (usize::from(cols) + 1);
    let mut rows = (PAGE_GRID_BYTES / row_bytes).min(usize::from(u16::MAX));
    while rows > 0
        && (CELL_BYTES * rows).next_multiple_of(CELLS_ALIGN) + CELL_BYTES * rows * usize::from(cols)
            > PAGE_GRID_BYTES
    {
        rows -= 1;
    }
    if rows == 0 { 215 } else { rows }
}

fn tmux_cursor(term: &Terminal<'_, '_>) -> (u16, u16) {
    let x = if term.is_cursor_pending_wrap().unwrap_or(false) {
        term.cols().unwrap_or(0)
    } else {
        term.cursor_x().unwrap_or(0)
    };
    (x, term.cursor_y().unwrap_or(0))
}

/// Map one reply Ghostty produced to the reply tmux 3.8 would give to the same
/// query, or `None` when tmux stays silent.
///
/// The allowlist (anything not listed is dropped):
/// - DA1 `CSI c` -> `ESC[?1;2c` (input.c:1582-1597)
/// - DA2 `CSI > c` -> `ESC[>84;0;0c` (input.c:1598-1608)
/// - DA3 `CSI = c`: tmux has no entry in its CSI table (input.c:326-327), so
///   Ghostty's `DCS ! |` reply is dropped
/// - DSR 5 -> `ESC[0n` (input.c:1727-1729), identical
/// - CPR -> `ESC[row;colR` from tmux's cursor, ignoring origin mode
///   (input.c:1730-1732)
/// - DECRQM -> tmux's mode table (input.c:1638-1722)
/// - DECRQSS: answered by [`PaneTerm::answer_decrqss`] from the scanner
///   (Ghostty's own SGR/DECSTBM/DECSLRM replies are dropped here)
/// - XTVERSION `DCS > | tmux 3.8 ST` (input.c:1885-1890), size reports
///   14/16/18 t (input.c:2160-2183): produced by the registered effects
///
/// Dropped, tmux does not answer: DA3, DECXCPR `CSI ? 6 n` and DSR ?998
/// (tmux's private DSR knows only 996, input.c:1631-1637), DSR ?996 (needs a
/// client theme), mode 2033/2048 reports, kitty keyboard `CSI ? u`, XTWINOPS
/// 21 t (input.c:2143-2149), every OSC query (colours, clipboard, OSC 5522,
/// 7501; tmux answers colours only from client state), XTGETTCAP, Kitty
/// graphics and Glyph Protocol APC.
fn tmux_reply(term: &Terminal<'_, '_>, cursor: CursorState, data: &[u8]) -> Option<Vec<u8>> {
    if let Some(body) = data.strip_prefix(b"\x1b[") {
        if let Some(params) = body.strip_suffix(b"$y") {
            return decrqm_reply(term, cursor, params);
        }
        return match body.last()? {
            b'c' if body.starts_with(b"?") => Some(b"\x1b[?1;2c".to_vec()),
            b'c' if body.starts_with(b">") => Some(b"\x1b[>84;0;0c".to_vec()),
            b'n' if body == b"0n" => Some(data.to_vec()),
            // CPR only: tmux answers DECXCPR (`CSI ? 6 n`, Ghostty's
            // `CSI ? row ; col ; 1 R`) with nothing, its private DSR knows
            // only 996 (input.c:1631-1637).
            b'R' if !body.starts_with(b"?") => {
                let (x, y) = tmux_cursor(term);
                Some(format!("\x1b[{};{}R", y + 1, x + 1).into_bytes())
            }
            b't' if matches!(body.first(), Some(b'4' | b'6' | b'8'))
                && body.get(1) == Some(&b';') =>
            {
                Some(data.to_vec())
            }
            _ => None,
        };
    }
    data.starts_with(b"\x1bP>|").then(|| data.to_vec())
}

/// DECRQM reply from tmux's table. `params` is the Ghostty reply between
/// `ESC[` and `$y`: `[?]mode;state`.
fn decrqm_reply(term: &Terminal<'_, '_>, cursor: CursorState, params: &[u8]) -> Option<Vec<u8>> {
    let (private, rest) = match params.strip_prefix(b"?") {
        Some(rest) => (true, rest),
        None => (false, params),
    };
    let mode: u16 = std::str::from_utf8(rest.split(|b| *b == b';').next()?)
        .ok()?
        .parse()
        .ok()?;
    if mode == 0 {
        return None; // `if (m > 0)`, input.c:1649, 1721
    }
    let on = |m: Mode| if term.mode(m).unwrap_or(false) { 1 } else { 2 }; // 1 set, 2 reset
    let state: u8 = if private {
        match mode {
            1 => on(Mode::DECCKM),
            3 => 4, // DECCOLM: always reset
            6 => on(Mode::ORIGIN),
            7 => on(Mode::WRAPAROUND),
            12 => cursor.blink_report(),
            25 => on(Mode::CURSOR_VISIBLE),
            47 | 1047 | 1049 => {
                if matches!(term.active_screen(), Ok(Screen::Alternate)) {
                    1
                } else {
                    2
                }
            }
            1000 => on(Mode::NORMAL_MOUSE),
            1002 => on(Mode::BUTTON_MOUSE),
            1003 => on(Mode::ANY_MOUSE),
            1004 => on(Mode::FOCUS_EVENT),
            1005 => on(Mode::UTF8_MOUSE),
            1006 => on(Mode::SGR_MOUSE),
            2004 => on(Mode::BRACKETED_PASTE),
            2026 => on(Mode::SYNC_OUTPUT),
            2031 => on(Mode::COLOR_SCHEME_REPORT),
            _ => 0,
        }
    } else if mode == 4 {
        // IRM, the only ANSI mode tmux reports (input.c:1638-1650).
        on(Mode::INSERT)
    } else {
        0
    };
    let q = if private { "?" } else { "" };
    Some(format!("\x1b[{q}{mode};{state}$y").into_bytes())
}

/// tmux's `clean_name` (tmux.c:303-317): reject nothing here (Ghostty already
/// guarantees UTF-8), neutralise `#(` from untrusted sources, then `vis`
/// with `VIS_OCTAL|VIS_CSTYLE|VIS_TAB|VIS_NL`.
fn clean_name(raw: &str, untrusted: bool) -> String {
    let raw = if untrusted {
        raw.replace("#(", "_(")
    } else {
        raw.to_owned()
    };
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            '\x1b' => out.push_str("\\E"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\{:03o}", c as u8);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replies_to(cols: u16, rows: u16, bytes: &[u8]) -> Vec<u8> {
        let mut t = PaneTerm::new(cols, rows, 100);
        t.feed(bytes);
        t.take_replies()
    }

    #[test]
    fn tmux_identity_replies() {
        assert_eq!(replies_to(20, 5, b"\x1b[c"), b"\x1b[?1;2c");
        assert_eq!(replies_to(20, 5, b"\x1b[>c"), b"\x1b[>84;0;0c");
        assert_eq!(replies_to(20, 5, b"\x1b[5n"), b"\x1b[0n");
        assert_eq!(replies_to(20, 5, b"\x1b[>q"), b"\x1bP>|tmux 3.8\x1b\\");
    }

    #[test]
    fn replies_tmux_does_not_send_are_dropped() {
        for q in [
            &b"\x1b[=c"[..],            // DA3
            b"\x1b[?u",                 // kitty keyboard
            b"\x1b[21t",                // title report
            b"\x1b[?996n",              // theme
            b"\x1b]4;1;?\x07",          // OSC colour query
            b"\x1b[?2048$p\x1b[?2048h", // mode 2048 resize report (after DECRQM)
        ] {
            let r = replies_to(20, 5, q);
            assert!(r.is_empty() || r == b"\x1b[?2048;0$y", "{q:?} -> {r:?}");
        }
    }

    /// tmux's private DSR answers only 996 (input.c:1631-1637); Ghostty
    /// answers DECXCPR with `CSI ? row ; col ; 1 R`.
    #[test]
    fn decxcpr_is_not_answered() {
        assert_eq!(replies_to(20, 5, b"\x1b[?6n\x1b[5n"), b"\x1b[0n");
        assert_eq!(replies_to(20, 5, b"\x1b[?998n\x1b[5n"), b"\x1b[0n");
        let pending = replies_to(20, 5, b"abcdefghijklmnopqrst\x1b[?6n\x1b[6n");
        assert_eq!(pending, b"\x1b[1;21R");
    }

    /// input.c:2553-2617: DECRQSS answers only `SP q`; Ghostty's own SGR,
    /// DECSTBM and DECSLRM replies are dropped.
    #[test]
    fn decrqss_answers_cursor_style_only() {
        assert_eq!(
            replies_to(20, 5, b"\x1bP$q q\x1b\\"),
            b"\x1bP1$r q0 q\x1b\\"
        );
        assert_eq!(replies_to(20, 5, b"\x1bP$qm\x1b\\"), b"\x1bP0$r\x1b\\");
        assert_eq!(replies_to(20, 5, b"\x1bP$qr\x1b\\"), b"\x1bP0$r\x1b\\");
        assert_eq!(replies_to(20, 5, b"\x1bP$q\x1b\\"), b"\x1bP0$r\x1b\\");
        // Other DCS strings stay unanswered.
        assert_eq!(replies_to(20, 5, b"\x1bP+q544e\x1b\\"), b"");
        // DECSCUSR, mode 12 and the `cursor-style` fallback.
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_cursor_style_option(3);
        t.feed(b"\x1bP$q q\x1b\\\x1b[?12$p");
        assert_eq!(t.take_replies(), b"\x1bP1$r q3 q\x1b\\\x1b[?12;1$y");
        t.feed(b"\x1b[6 q\x1bP$q q\x1b\\\x1b[?12$p");
        assert_eq!(t.take_replies(), b"\x1bP1$r q6 q\x1b\\\x1b[?12;2$y");
        // Split across feeds, with a stray ESC-less byte stream between.
        t.feed(b"\x1bP$");
        t.feed(b"q ");
        t.feed(b"q\x1b");
        t.feed(b"\\");
        assert_eq!(t.take_replies(), b"\x1bP1$r q6 q\x1b\\");
        // RIS keeps the shape and drops the blink bits.
        t.feed(b"\x1b[1 q\x1bc\x1bP$q q\x1b\\");
        assert_eq!(t.take_replies(), b"\x1bP1$r q2 q\x1b\\");
    }

    #[test]
    fn size_reports_use_tmux_cell_size() {
        assert_eq!(replies_to(20, 5, b"\x1b[18t"), b"\x1b[8;5;20t");
        assert_eq!(replies_to(20, 5, b"\x1b[14t"), b"\x1b[4;160;320t");
        assert_eq!(replies_to(20, 5, b"\x1b[16t"), b"\x1b[6;32;16t");
    }

    #[test]
    fn decrqm_follows_tmux_table() {
        assert_eq!(replies_to(20, 5, b"\x1b[?7$p"), b"\x1b[?7;1$y");
        assert_eq!(replies_to(20, 5, b"\x1b[?1$p"), b"\x1b[?1;2$y");
        assert_eq!(replies_to(20, 5, b"\x1b[?3$p"), b"\x1b[?3;4$y");
        assert_eq!(replies_to(20, 5, b"\x1b[?2027$p"), b"\x1b[?2027;0$y");
        assert_eq!(replies_to(20, 5, b"\x1b[?1049h\x1b[?47$p"), b"\x1b[?47;1$y");
        assert_eq!(replies_to(20, 5, b"\x1b[?12h\x1b[?12$p"), b"\x1b[?12;1$y");
    }

    #[test]
    fn cursor_reports_pending_wrap_as_columns() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"abcdefghijklmnopqrst");
        assert_eq!(t.cursor(), (20, 0));
        t.feed(b"\x1b[6n");
        assert_eq!(t.take_replies(), b"\x1b[1;21R");
        t.feed(b"u");
        assert_eq!(t.cursor(), (1, 1));
    }

    #[test]
    fn cpr_ignores_origin_mode() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"\x1b[2;4r\x1b[?6h\x1b[1;1H\x1b[6n");
        // Absolute row 2 (region top), column 1: tmux reports 2;1, Ghostty 1;1.
        assert_eq!(t.take_replies(), b"\x1b[2;1R");
    }

    #[test]
    fn title_and_pwd_are_cleaned_like_tmux() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"\x1b]2;a\\b#(x)\x07\x1b]7;file:///tmp/a b\x1b\\");
        assert_eq!(t.title(), "a\\\\b_(x)");
        assert_eq!(t.pwd(), "file:///tmp/a b");
    }

    #[test]
    fn server_title_holds_until_the_program_sets_one() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"\x1b]2;prog\x07");
        t.set_title("a#(b)\tc");
        assert_eq!(t.title(), "a#(b)\\tc");
        t.feed(b"x");
        assert_eq!(t.title(), "a#(b)\\tc");
        t.feed(b"\x1b]2;next\x07");
        assert_eq!(t.title(), "next");
    }

    #[test]
    fn ris_keeps_the_title() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"\x1b]2;host\x07x\x1bc");
        assert_eq!(t.title(), "host");
        t.feed(b"\x1b]2;next\x07");
        assert_eq!(t.title(), "next");
    }

    #[test]
    fn alternate_screen_flag() {
        let mut t = PaneTerm::new(20, 5, 100);
        assert!(!t.alternate_on());
        t.feed(b"\x1b[?1049h");
        assert!(t.alternate_on());
    }

    #[test]
    fn history_follows_tmux_sawtooth() {
        // Reference tmux 3.8 (goldens fmt_history_size_sawtooth*).
        for (cols, rows, limit, feed, want) in [
            (20u16, 5u16, 100usize, 150usize, 96usize),
            (80, 24, 2000, 2100, 1877),
        ] {
            let mut t = PaneTerm::new(cols, rows, limit);
            for i in 0..feed {
                t.feed(format!("l{i}\r\n").as_bytes());
            }
            assert_eq!(t.history_size(), want, "cols={cols} limit={limit}");
        }
    }

    #[test]
    fn history_stays_between_nine_tenths_and_the_limit() {
        // (columns, limit, lines to feed): enough lines to force pruning.
        for (cols, limit, feed) in [
            (10u16, 100usize, 1600usize),
            (80, 2000, 3500),
            (300, 500, 1700),
        ] {
            let mut t = PaneTerm::new(cols, 24, limit);
            t.feed(&b"x\r\n".repeat(feed));
            let h = t.history_size();
            assert!(
                (limit * 9 / 10..=limit).contains(&h),
                "{h} cols={cols} limit={limit}"
            );
        }
    }

    #[test]
    fn history_matches_the_model_through_ghostty_pruning() {
        // Narrow panes make Ghostty prune pages long before tmux would; the
        // count must not notice, however the bytes arrive.
        for (cols, chunk) in [(10u16, 1usize), (10, 7), (10, 100_000), (300, 4096)] {
            let data = b"x\r\n".repeat(3000);
            let mut t = PaneTerm::new(cols, 5, 100);
            for part in data.chunks(chunk) {
                t.feed(part);
            }
            // The cursor row stays visible: 3000 newlines scroll 2996 rows.
            assert_eq!(
                t.history_size(),
                tmux_hsize(0, 100, 3000 - 4),
                "cols={cols} chunk={chunk}"
            );
        }
    }

    #[test]
    fn history_survives_widening_within_design_width() {
        let mut t = PaneTerm::new(80, 24, 2000);
        t.feed(&b"x\r\n".repeat(2500));
        t.resize(160, 24);
        t.feed(&b"x\r\n".repeat(2500));
        assert!(
            (1800..=2000).contains(&t.history_size()),
            "{}",
            t.history_size()
        );
    }

    #[test]
    fn history_is_kept_while_the_alternate_screen_is_on() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(&b"x\r\n".repeat(10));
        assert_eq!(t.history_size(), 6);
        t.feed(b"\x1b[?1049hALT");
        assert_eq!(t.history_size(), 6);
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.history_size(), 6);
    }

    /// screen-write.c:3154-3197 and input.c:1411: a screen switch or RIS
    /// asks for a full pane redraw; setting a mode already set does not.
    #[test]
    fn screen_switch_and_reset_ask_for_a_redraw() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"plain\x1b[2J");
        assert!(!t.take_redraw());
        t.feed(b"\x1b[?1049h");
        assert!(t.take_redraw());
        assert!(!t.take_redraw());
        t.feed(b"\x1b[?1049h");
        assert!(!t.take_redraw(), "already on the alternate screen");
        t.feed(b"\x1b[?1049l");
        assert!(t.take_redraw());
        t.feed(b"\x1bc");
        assert!(t.take_redraw());
    }

    /// OSC 133 marks the cursor's line (input.c `input_osc_133`), with BEL
    /// or ST, and the mark follows the line into history.
    #[test]
    fn osc133_marks_follow_their_lines() {
        let mut t = PaneTerm::new(20, 3, 100);
        t.feed(b"\x1b]133;A\x07$ ls\r\n\x1b]133;C\x1b\\out\r\n");
        t.feed(b"\x1b]133;P;k=s\x07> \x1b]133;D;0\x07x\r\n\x1b]133;P;k=i\x07$ ");
        assert_eq!(
            t.line_marks(),
            [
                (0, LineMark::Prompt),
                (1, LineMark::Output),
                (3, LineMark::Prompt)
            ]
        );
        assert_eq!(t.history_size(), 1);
        t.feed(b"\x1b[?1049h\x1b]133;A\x07");
        assert_eq!(t.line_marks()[0], (0, LineMark::Prompt));
    }

    #[test]
    fn alt_mode_scanner_matches_only_alternate_screen_modes() {
        let find = |bytes: &[u8]| {
            let mut s = AltModeScanner::default();
            bytes
                .iter()
                .filter_map(|b| match s.push(*b)? {
                    ScanEvent::AltMode(f) => Some(f),
                    ScanEvent::ClearHistory => Some(b'J'),
                    ScanEvent::Resync
                    | ScanEvent::Reset
                    | ScanEvent::Osc133(_)
                    | ScanEvent::Decrqss => None,
                })
                .collect::<Vec<u8>>()
        };
        assert_eq!(find(b"\x1b[?1049h"), b"h");
        assert_eq!(find(b"a\x1b[?25;1047l"), b"l");
        assert_eq!(find(b"\x1b[?47h\x1b[?47l"), b"hl");
        assert_eq!(find(b"\x1b[?25h"), b"");
        assert_eq!(find(b"\x1b[1049h"), b"");
        assert_eq!(find(b"\x1b[?1049$p"), b"");
        assert_eq!(
            find(b"\x1b[3J\x1b[3;0J\x1b[J\x1b[2J\x1b[?3J\x1b[3;1J"),
            b"JJ"
        );
    }

    /// tmux's charset state reaches the cell's protection bit, which a
    /// snapshot reports as `acs`: G0/G1 designation, SO/SI, DECSC/DECRC,
    /// RIS; DECSCA and EPA (ignored by tmux) cannot clear it, and erasing
    /// still clears ACS cells.
    #[test]
    fn charset_state_follows_tmux() {
        let acs = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 2, 0);
            t.feed(bytes);
            let s = crate::snapshot(&t);
            (0..20)
                .take_while(|&x| !s.cell(x, 0).unwrap().text.is_empty())
                .map(|x| if s.cell(x, 0).unwrap().acs { '1' } else { '0' })
                .collect::<String>()
        };
        assert_eq!(acs(b"a\x1b(0q\x1b(Bq"), "010");
        assert_eq!(acs(b"\x1b)0a\x0eq\x0fq"), "010");
        // Restoring also moves the cursor back over the first cell.
        assert_eq!(acs(b"\x1b(0\x1b7\x1b(Ba\x1b8q"), "1");
        assert_eq!(acs(b"\x1b7\x1b(0a\x1b8q"), "0");
        assert_eq!(acs(b"\x1b(0\x1b[s\x1b(B\x1b[uq"), "1");
        assert_eq!(acs(b"\x1b(0q\x1bcq"), "0");
        assert_eq!(acs(b"\x1b(0q\x1b[0\"qq\x1bWq\x1b[!pq"), "1111");
        assert_eq!(acs(b"\x1b[1\"qa"), "0");
        // SO inside a CSI still takes effect after it.
        assert_eq!(acs(b"\x1b)0\x1b[\x0e1mq"), "1");
        assert_eq!(acs(b"\x1b(0qqq\x1b[1;2H\x1b[K\x1b[3G\x1b[X"), "1");
        assert_eq!(acs(b"\x1b(0q\x1b[?1049hq\x1b(Bq\x1b[?1049lq"), "10");
    }

    #[test]
    fn history_budget_lowered_prunes_rows() {
        let mut t = PaneTerm::new(80, 5, 2000);
        t.feed(&b"x\r\n".repeat(2500));
        let before = t.term.scrollback_rows().unwrap();
        assert_eq!(t.history_size(), tmux_hsize(0, 2000, 2500 - 4));
        let newest = t.history_id(t.history_size() - 1);

        t.set_history_budget(3 * PAGE_BYTES);
        let after = t.term.scrollback_rows().unwrap();
        assert!(after < before && after < 2000, "{before} -> {after}");
        assert_eq!(t.history_size(), after);
        // The newest rows keep their ids.
        assert_eq!(t.history_id(t.history_size() - 1), newest);

        // Raising it again keeps what scrolls in from now on.
        t.set_history_budget(usize::MAX);
        t.feed(&b"x\r\n".repeat(2500));
        assert!(t.term.scrollback_rows().unwrap() > 2000);
        assert_eq!(t.history_size(), tmux_hsize(after, 2000, 2500));
    }

    /// Ghostty counts only the active screen's history, and the alternate
    /// screen has none: a budget set under it must not clear tmux's
    /// primary history.
    #[test]
    fn history_budget_under_the_alternate_screen_keeps_history() {
        let mut t = PaneTerm::new(80, 5, 2000);
        t.feed(&b"x\r\n".repeat(100));
        let newest = t.history_id(95);
        let time = t.history_time(newest);
        assert_eq!(t.history_size(), 96);
        t.feed(b"\x1b[?1049h");
        t.set_history_budget(usize::MAX);
        assert_eq!(t.history_size(), 96);
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.history_size(), 96);
        assert_eq!(t.history_id(95), newest);
        assert_eq!(t.history_time(newest), time);
        assert_ne!(time, 0);
    }

    /// A budget lowered under the alternate screen prunes the primary
    /// history in Ghostty; the model follows when the primary is back.
    #[test]
    fn history_budget_lowered_under_the_alternate_screen() {
        let mut t = PaneTerm::new(80, 5, 2000);
        t.feed(&b"x\r\n".repeat(2500));
        t.feed(b"\x1b[?1049h");
        t.set_history_budget(3 * PAGE_BYTES);
        t.feed(b"\x1b[?1049l");
        let retained = t.term.scrollback_rows().unwrap();
        assert!(retained < 2000, "{retained}");
        assert_eq!(t.history_size(), retained);
    }

    #[test]
    fn history_budget_of_zero_still_keeps_history() {
        let mut t = PaneTerm::new(80, 5, 2000);
        t.feed(&b"x\r\n".repeat(100));
        t.set_history_budget(0);
        assert_eq!(t.history_size(), 96);
        t.feed(&b"x\r\n".repeat(10));
        assert_eq!(t.history_size(), 106);
    }

    /// `PAGE_BYTES` is Ghostty's page: a budget of `k` pages keeps a page
    /// more than one byte less does.
    #[test]
    fn history_budget_steps_by_page() {
        let kept = |bytes: usize| {
            let mut t = PaneTerm::new(80, 5, 100_000);
            t.set_history_budget(bytes);
            t.feed(&b"x\r\n".repeat(5000));
            t.term.scrollback_rows().unwrap()
        };
        let pages = 3;
        let fewer = kept(pages * PAGE_BYTES - 1);
        let full = kept(pages * PAGE_BYTES);
        let rows_per_page = page_rows(80);
        assert!(
            full - fewer >= rows_per_page,
            "{fewer} -> {full}, page holds >= {rows_per_page}"
        );
        assert_eq!(kept(pages * PAGE_BYTES + PAGE_BYTES - 1), full);
    }

    /// `page_rows` is the row count of Ghostty's standard page: with the
    /// smallest line limit (one page), the history drops by exactly a page
    /// once the second page fills.
    #[test]
    fn page_rows_matches_ghostty() {
        for cols in [1, 2, 7, 80, 81, 120, 200, 215, 216, 333, 500, 1000, 5000] {
            let mut t = PaneTerm::new(cols, 1, 1);
            let mut prev = 0;
            let mut dropped = None;
            for _ in 0..3 * page_rows(cols) {
                t.term.vt_write(b"\n");
                let n = t.term.scrollback_rows().unwrap();
                if n < prev {
                    dropped = Some(prev + 1 - n);
                    break;
                }
                prev = n;
            }
            assert_eq!(dropped, Some(page_rows(cols)), "{cols} columns");
        }
    }

    /// 70-digit lines numbered from `from`; with `styled` every cell has
    /// its own truecolour, so pages fill their style tables and grow.
    fn lines(from: usize, n: usize, styled: bool) -> Vec<u8> {
        let mut out = String::new();
        for i in from..from + n {
            let line = format!("{i:070}");
            if styled {
                for (j, c) in line.chars().enumerate() {
                    let _ = write!(out, "\x1b[38;2;{};{j};{}m{c}", i % 256, (i / 256) % 256);
                }
                out.push_str("\x1b[m");
            } else {
                out.push_str(&line);
            }
            out.push_str("\r\n");
        }
        out.into_bytes()
    }

    /// Ghostty keeps every row tmux has (`hsize`) and at most one page
    /// more than `max(history-limit, hsize)`.
    fn assert_retention(t: &PaneTerm, what: &str) {
        let retained = t.term.scrollback_rows().unwrap();
        let ceiling = t.history_lines.max(t.hsize) + page_rows(t.cols());
        assert!(
            t.hsize <= retained && retained <= ceiling,
            "{what}: hsize {} retained {retained} ceiling {ceiling}",
            t.hsize
        );
    }

    /// The history-memory spec's criterion 2: retained rows stay within
    /// one page of `history-limit` at any width, plain or styled, and
    /// `history_size` stays tmux's sawtooth.
    #[test]
    fn retained_rows_track_history_limit() {
        for cols in [80, 200, 500] {
            for styled in [false, true] {
                let limit = 1000;
                let mut t = PaneTerm::new(cols, 10, limit);
                let mut fed = 0;
                for chunk in [300, 700, 1, 1500, 2345] {
                    t.feed(&lines(fed, chunk, styled));
                    fed += chunk;
                    let what = format!("{cols} cols styled={styled} after {fed}");
                    assert_retention(&t, &what);
                    let scrolled = (fed + 1).saturating_sub(10);
                    assert_eq!(t.history_size(), tmux_hsize(0, limit, scrolled), "{what}");
                    if scrolled >= limit {
                        assert!(t.term.scrollback_rows().unwrap() >= limit, "{what}");
                    }
                }
                // The oldest row tmux has is the right one.
                let oldest = fed + 1 - 10 - t.history_size();
                let opts = crate::capture::CaptureOpts {
                    start: crate::capture::LineRef::Start,
                    end: crate::capture::LineRef::Line(-i64::try_from(t.history_size()).unwrap()),
                    ..Default::default()
                };
                let row = crate::capture::capture(&t, opts);
                assert_eq!(
                    String::from_utf8_lossy(&row).trim_end(),
                    format!("{oldest:070}"),
                    "{cols} styled={styled}"
                );
            }
        }
    }

    /// tmux's `session_update_history`: lowering trims to exactly the limit
    /// at once, raising gives nothing back, an equal limit costs a row, and a
    /// pane made without history never gets any.
    #[test]
    fn history_limit_changes_on_a_running_pane() {
        let mut t = PaneTerm::new(80, 5, 2000);
        t.feed(&lines(0, 1500, false));
        assert_eq!(t.history_size(), 1496);
        t.set_history_limit(500);
        assert_eq!(t.history_size(), 500);
        assert!(t.retained_rows() <= 500 + page_rows(80));
        t.set_history_limit(800);
        assert_eq!(t.history_size(), 500);
        t.set_history_limit(500);
        assert_eq!(t.history_size(), 499);
        // The row ids and the rows themselves are still the newest ones.
        let id = t.history_id(0);
        assert_eq!(
            t.history_row(id).unwrap().text(),
            format!("{:070}", 1500 - 4 - 499)
        );
        t.feed(&lines(1500, 600, false));
        assert!(t.history_size() <= 800);
        assert_retention(&t, "after raise");

        let mut none = PaneTerm::new(80, 5, 0);
        none.set_history_limit(100);
        none.feed(&lines(0, 300, false));
        assert_eq!(none.history_size(), 0);

        let mut off = PaneTerm::new(80, 5, 100);
        off.feed(&lines(0, 300, false));
        off.set_history_limit(0);
        assert_eq!(off.history_size(), 0);
    }

    /// Lowering under the alternate screen trims the primary history now,
    /// and the copy shown meanwhile and the history on the way back agree.
    #[test]
    fn history_limit_lowered_under_the_alternate_screen() {
        let mut t = PaneTerm::new(80, 5, 1000);
        t.feed(&lines(0, 800, false));
        t.feed(b"\x1b[?1049h");
        assert!(t.alternate_on());
        let before = t.history_size();
        t.set_history_limit(100);
        assert!(before > 100);
        assert_eq!(t.history_size(), 100);
        let id = t.history_id(0);
        assert_eq!(
            t.history_row(id).unwrap().text(),
            format!("{:070}", 800 - 4 - 100)
        );
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.history_size(), 100);
        assert_eq!(t.row(0).text(), format!("{:070}", 800 - 4 - 100));
    }

    /// Reflow moves rows in and out of history; the line limit follows the
    /// width so the bound holds after every resize, and narrowing never
    /// drops a row tmux keeps (its `hsize` may pass the limit, grid.c
    /// `grid_reflow`).
    #[test]
    fn retained_rows_track_history_limit_across_resizes() {
        for styled in [false, true] {
            let mut t = PaneTerm::new(80, 10, 1000);
            t.feed(&lines(0, 3000, styled));
            assert_retention(&t, "80");
            for (cols, rows) in [(200, 10), (500, 20), (40, 10), (20, 5), (80, 10), (80, 30)] {
                let before = t.history_size();
                t.resize(cols, rows);
                let what = format!("{cols}x{rows} styled={styled}");
                assert_retention(&t, &what);
                if cols < 80 {
                    assert!(
                        t.history_size() > before,
                        "{what}: {before} -> {}",
                        t.history_size()
                    );
                }
                t.feed(&lines(0, 1500, styled));
                assert_retention(&t, &format!("{what} then output"));
                assert!(t.history_size() <= 1000, "{what}");
            }
        }
    }

    /// `clear-history` (`grid_clear_history`): the history goes, the
    /// visible rows stay, ids change, and the pane keeps collecting history
    /// within its limit afterwards, on the primary screen or under the
    /// alternate one.
    #[test]
    fn clear_history_keeps_the_screen_and_the_limit() {
        for styled in [false, true] {
            let mut t = PaneTerm::new(80, 10, 1000);
            t.feed(&lines(0, 3000, styled));
            let screen = |t: &PaneTerm| {
                (0..10)
                    .map(|y| t.row(t.history_size() + y).text())
                    .collect::<Vec<_>>()
            };
            let visible = screen(&t);
            let generation = t.history_generation();
            assert!(t.history_size() > 0);
            t.clear_history();
            assert_eq!(
                (t.history_size(), t.retained_rows()),
                (0, 0),
                "styled={styled}"
            );
            assert_ne!(t.history_generation(), generation);
            assert_eq!(screen(&t), visible, "styled={styled}");
            // A second clear on an empty history is a no-op.
            t.clear_history();
            assert_eq!(t.history_size(), 0);
            t.feed(&lines(3000, 3000, styled));
            assert!(t.history_size() > 0 && t.history_size() <= 1000);
            assert_retention(&t, &format!("after clear styled={styled}"));

            // Under the alternate screen the primary history goes, and is
            // gone when the primary screen is back.
            t.feed(b"\x1b[?1049h");
            t.clear_history();
            assert_eq!(t.history_size(), 0);
            t.feed(b"\x1b[?1049l");
            assert_eq!((t.history_size(), t.retained_rows()), (0, 0));
            t.feed(&lines(0, 3000, styled));
            assert_retention(&t, &format!("after alt clear styled={styled}"));
        }
        // A pane with no history has nothing to clear and stays so.
        let mut t = PaneTerm::new(80, 10, 0);
        t.feed(&lines(0, 100, false));
        t.clear_history();
        assert_eq!(t.history_size(), 0);
    }

    /// A reset (RIS), a history clear (ED3) and the alternate screen keep
    /// the line limit.
    #[test]
    fn retained_rows_track_history_limit_after_reset_and_clear() {
        for seq in [&b"\x1bc"[..], b"\x1b[3J", b"\x1b[?1049h\x1b[?1049l"] {
            let mut t = PaneTerm::new(80, 10, 1000);
            t.feed(&lines(0, 3000, false));
            t.feed(seq);
            t.feed(&lines(0, 3000, false));
            assert_retention(&t, &String::from_utf8_lossy(seq));
            assert!(t.term.scrollback_rows().unwrap() >= 1000);
        }
    }

    /// Ghostty reflows the primary screen at a resize under the alternate
    /// screen; tmux reflows it once when the alternate screen goes
    /// (screen.c `screen_alternate_off`). A narrowing splits every history
    /// row in Ghostty, so the line limit must not prune to tmux's unsplit
    /// count until then, and tmux's history starts at the same row after.
    #[test]
    fn resize_under_the_alternate_screen_keeps_history() {
        for styled in [false, true] {
            let mut t = PaneTerm::new(80, 24, 2000);
            t.feed(&lines(0, 5000, styled));
            let before = t.history_size();
            let oldest = 5000 + 1 - 24 - before;
            t.feed(b"\x1b[?1049h");
            for (cols, rows) in [(40, 24), (20, 10), (90, 30), (80, 24)] {
                t.resize(cols, rows);
                assert_eq!(t.history_size(), before, "{cols}x{rows} styled={styled}");
            }
            t.feed(b"\x1b[?1049l");
            assert_eq!(t.history_size(), before, "styled={styled}");
            assert_retention(&t, &format!("after alt styled={styled}"));
            let opts = crate::capture::CaptureOpts {
                start: crate::capture::LineRef::Start,
                end: crate::capture::LineRef::Line(-i64::try_from(before).unwrap()),
                ..Default::default()
            };
            let row = crate::capture::capture(&t, opts);
            assert_eq!(
                String::from_utf8_lossy(&row).trim_end(),
                format!("{oldest:070}"),
                "styled={styled}"
            );
            // The limit is back: more output stays within a page of it.
            t.feed(&lines(0, 3000, styled));
            assert_retention(&t, &format!("then output styled={styled}"));
        }
    }

    #[test]
    fn zero_history_keeps_none() {
        let mut t = PaneTerm::new(20, 5, 0);
        for _ in 0..50 {
            t.feed(b"x\r\n");
        }
        assert_eq!(t.history_size(), 0);
    }
}
