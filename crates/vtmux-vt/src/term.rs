//! Safe wrapper over one libghostty-vt terminal, shaped like a tmux pane.
//!
//! Grid state follows Ghostty (D4). Everything a pane program can observe from
//! outside the grid (query replies, cursor position, title, path, history
//! size) follows tmux 3.8, read from `input.c`, `format.c` and `screen.c`.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use libghostty_vt::screen::{Screen, TrackedGridRef};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributes, DeviceType, Mode, Point, PointCoordinate, PointSpace,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes, SizeReportSize, TertiaryDeviceAttributes,
};
use libghostty_vt::{Terminal, TerminalOptions};

use crate::capture::FrozenRow;

/// Pixel size of one cell in XTWINOPS replies: tmux's `DEFAULT_XPIXEL` and
/// `DEFAULT_YPIXEL` (tmux.h:122-123), used when no client reports a size.
const CELL_WIDTH_PX: u32 = 16;
const CELL_HEIGHT_PX: u32 = 32;

/// Bytes in one libghostty-vt 0.2.2 pooled page (`PagePool.item_size`,
/// PageList.zig). Measured by probing retention; `history_budget_keeps_the_limit`
/// guards the number against a libghostty upgrade.
const PAGE_BYTES: usize = 401_408;
/// Bytes of a page that hold styles, graphemes and hyperlinks rather than
/// rows. Measured as ~27 KB at 200 columns; rounded up so row counts per page
/// are under-estimated, never over-estimated.
const PAGE_OVERHEAD_BYTES: usize = 40_000;
/// Bytes per cell and per row header (`@bitSizeOf(Cell)`, `@bitSizeOf(Row)` = 64).
const CELL_BYTES: usize = 8;

/// One pane's terminal. Owns the libghostty terminal; `'static` for callers.
#[derive(Debug)]
pub struct PaneTerm {
    pub(crate) term: Terminal<'static, 'static>,
    replies: Rc<RefCell<Vec<u8>>>,
    history_lines: usize,
    /// tmux's `hsize` for the primary grid, modelled (see [`PaneTerm::feed`]).
    hsize: usize,
    /// Tracked pin on the first active row, re-armed before every piece fed,
    /// to count the rows a piece scrolled into history.
    pin: Option<TrackedGridRef>,
    scan: AltModeScanner,
    /// The newest `hsize` history rows, copied when the alternate screen was
    /// entered: libghostty only reads the active screen, and tmux keeps
    /// showing the primary history while an alternate screen is up.
    frozen: Option<Vec<FrozenRow>>,
}

impl PaneTerm {
    /// Create a terminal of `cols` x `rows` keeping `history_lines` lines of
    /// scrollback (tmux's `history-limit`). Zero sizes are raised to one.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    #[must_use]
    pub fn new(cols: u16, rows: u16, history_lines: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut term = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: history_budget(cols, rows, history_lines),
        })
        .expect("libghostty terminal allocation");

        let replies = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&replies);
        let registered = term
            // Every reply Ghostty wants to send goes through `tmux_reply`.
            .on_pty_write(move |term, data| {
                if let Some(reply) = tmux_reply(term, data) {
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

        Self {
            term,
            replies,
            history_lines,
            hsize: 0,
            pin: None,
            scan: AltModeScanner::default(),
            frozen: None,
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
        if self.history_lines == 0 {
            self.term.vt_write(bytes);
            return;
        }
        let step = (self.history_lines / 2).max(1);
        let mut i = 0;
        while i < bytes.len() {
            let mut j = i;
            let mut switch = None;
            while j < bytes.len() && j - i < step {
                if let Some(fin) = self.scan.push(bytes[j]) {
                    switch = Some(fin);
                    break;
                }
                j += 1;
            }
            self.feed_piece(&bytes[i..j]);
            i = j;
            if let Some(fin) = switch {
                // The alternate screen is about to switch: last chance to read
                // the primary history.
                if fin == b'h' && !self.alternate_on() {
                    self.frozen = Some(self.freeze_history());
                }
                self.feed_piece(&bytes[i..=i]);
                i += 1;
            }
        }
    }

    fn feed_piece(&mut self, piece: &[u8]) {
        if piece.is_empty() {
            return;
        }
        let track = !self.alternate_on();
        if track {
            self.arm_pin();
        }
        self.term.vt_write(piece);
        if !self.alternate_on() {
            self.frozen = None;
            if track {
                self.account_scroll();
            } else {
                // Back from the alternate screen (or a reset): the primary
                // history may have changed under us.
                let retained = self.term.scrollback_rows().unwrap_or(0);
                self.hsize = self.hsize.min(retained);
            }
        }
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
    }

    /// Copy the newest `hsize` primary history rows.
    fn freeze_history(&self) -> Vec<FrozenRow> {
        let cols = usize::from(self.cols());
        let hidden = self.term.scrollback_rows().unwrap_or(0) - self.hsize;
        (0..self.hsize)
            .map(|y| FrozenRow::capture(&self.term, hidden + y, cols))
            .collect()
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
        let on_primary = !self.alternate_on();
        self.term
            .resize(cols.max(1), rows.max(1), CELL_WIDTH_PX, CELL_HEIGHT_PX)
            .expect("libghostty resize");
        if on_primary {
            // Reflow moves rows in and out of history: keep the rows hidden
            // from tmux's view constant. (While the alternate screen is up
            // the frozen copy is not reflowed.)
            let after = self.term.scrollback_rows().unwrap_or(0);
            self.hsize = (self.hsize + after).saturating_sub(before).min(after);
        }
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
        self.term
            .title()
            .map(|t| clean_name(t, true))
            .unwrap_or_default()
    }

    /// Path from OSC 7 as tmux stores it (`screen_set_path`, screen.c:278-288).
    #[must_use]
    pub fn pwd(&self) -> String {
        self.term
            .pwd()
            .map(|p| clean_name(p, true))
            .unwrap_or_default()
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

/// Finds `CSI ? ... h|l` sequences that name the alternate screen (modes 47,
/// 1047, 1049): the one place the primary history must be copied before
/// libghostty hides it. A false positive costs only a copy; a miss loses the
/// history while the alternate screen is up.
#[derive(Debug, Default)]
struct AltModeScanner {
    state: ScanState,
    private: bool,
    other: bool,
    params: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl AltModeScanner {
    /// Feed one byte; `Some(final byte)` when it ends an alternate-screen
    /// set or reset.
    fn push(&mut self, b: u8) -> Option<u8> {
        match (self.state, b) {
            (_, 0x18 | 0x1a) => self.state = ScanState::Ground,
            (_, 0x1b) => self.state = ScanState::Esc,
            // Ground, and C0 controls inside a CSI (executed, not parameters).
            (ScanState::Ground, _) | (ScanState::Csi, 0x00..=0x1f) => {}
            (ScanState::Esc, b'[') => {
                self.state = ScanState::Csi;
                self.private = false;
                self.other = false;
                self.params.clear();
            }
            (ScanState::Esc, _) => self.state = ScanState::Ground,
            (ScanState::Csi, b'?') if self.params.is_empty() && !self.private => {
                self.private = true;
            }
            (ScanState::Csi, b'0'..=b'9' | b';') => {
                if self.params.len() < 64 {
                    self.params.push(b);
                }
            }
            (ScanState::Csi, 0x40..=0x7e) => {
                self.state = ScanState::Ground;
                if self.private
                    && !self.other
                    && matches!(b, b'h' | b'l')
                    && self
                        .params
                        .split(|c| *c == b';')
                        .any(|p| matches!(p, b"47" | b"1047" | b"1049"))
                {
                    return Some(b);
                }
            }
            (ScanState::Csi, _) => self.other = true,
        }
        None
    }
}

/// Byte budget for Ghostty's `max_scrollback` (bytes, despite the docs saying
/// lines: Screen.zig:282-296) that keeps at least `lines` lines of history.
///
/// Ghostty cannot trim by lines, so this over-retains and `history_size`
/// hides the excess. The budget assumes a design width of twice the current
/// width (at least 256 columns) so a later widening does not drop below the
/// limit; narrower panes then keep far more than `lines` rows in memory.
fn history_budget(cols: u16, rows: u16, lines: usize) -> usize {
    if lines == 0 {
        return 0;
    }
    let design_cols = (usize::from(cols) * 2).max(256);
    let row_bytes = CELL_BYTES * (design_cols + 1);
    let rows_per_page = ((PAGE_BYTES - PAGE_OVERHEAD_BYTES) / row_bytes).max(1);
    // Two extra pages: the active area straddles a page boundary, and pruning
    // works a whole page at a time.
    let pages = (lines + usize::from(rows)).div_ceil(rows_per_page) + 2;
    pages.saturating_mul(PAGE_BYTES)
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
/// - XTVERSION `DCS > | tmux 3.8 ST` (input.c:1885-1890), size reports
///   14/16/18 t (input.c:2160-2183): produced by the registered effects
///
/// Dropped, tmux does not answer: DA3, DSR ?996 (needs a client theme),
/// kitty keyboard `CSI ? u`, XTWINOPS 21 t (input.c:2143-2149), every OSC
/// colour query (tmux answers only from client state), Kitty graphics and
/// Glyph Protocol APC.
fn tmux_reply(term: &Terminal<'_, '_>, data: &[u8]) -> Option<Vec<u8>> {
    if let Some(body) = data.strip_prefix(b"\x1b[") {
        if let Some(params) = body.strip_suffix(b"$y") {
            return decrqm_reply(term, params);
        }
        return match body.last()? {
            b'c' if body.starts_with(b"?") => Some(b"\x1b[?1;2c".to_vec()),
            b'c' if body.starts_with(b">") => Some(b"\x1b[>84;0;0c".to_vec()),
            b'n' if body == b"0n" => Some(data.to_vec()),
            b'R' => {
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
fn decrqm_reply(term: &Terminal<'_, '_>, params: &[u8]) -> Option<Vec<u8>> {
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
            12 => on(Mode::CURSOR_BLINKING),
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
        // tmux answers `CSI 4 $ p` (input.c:1638-1650) but libghostty-vt 0.2.2
        // only parses the `?` form (stream.zig DECRQM, `intermediates.len == 2`),
        // so this arm is unreachable today; kept so an upgrade needs no change.
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

    #[test]
    fn alt_mode_scanner_matches_only_alternate_screen_modes() {
        let find = |bytes: &[u8]| {
            let mut s = AltModeScanner::default();
            bytes.iter().filter_map(|b| s.push(*b)).collect::<Vec<u8>>()
        };
        assert_eq!(find(b"\x1b[?1049h"), b"h");
        assert_eq!(find(b"a\x1b[?25;1047l"), b"l");
        assert_eq!(find(b"\x1b[?47h\x1b[?47l"), b"hl");
        assert_eq!(find(b"\x1b[?25h"), b"");
        assert_eq!(find(b"\x1b[1049h"), b"");
        assert_eq!(find(b"\x1b[?1049$p"), b"");
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
