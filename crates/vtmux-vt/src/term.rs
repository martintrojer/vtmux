//! Safe wrapper over one libghostty-vt terminal, shaped like a tmux pane.
//!
//! Grid state follows Ghostty (D4). Everything a pane program can observe from
//! outside the grid (query replies, cursor position, title, path, history
//! size) follows tmux 3.8, read from `input.c`, `format.c` and `screen.c`.

use std::cell::{Cell as StdCell, RefCell};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use libghostty_vt::screen::{Screen, TrackedGridRef};
use libghostty_vt::terminal::{
    CompressionMode, CompressionResult, ConformanceLevel, DeviceAttributes, DeviceType, Mode,
    Point, PointCoordinate, PointSpace, PrimaryDeviceAttributes, SecondaryDeviceAttributes,
    TertiaryDeviceAttributes,
};
use libghostty_vt::{Terminal, TerminalOptions};

use crate::capture::FrozenRow;
use crate::notify::Notification;
use crate::progress::ProgressBar;

mod state;
use crate::dcs::{self, Dcs, Dispatch, Step};
use crate::palette::{self, OscEnd, Palette, Reply};
pub use state::*;

thread_local! {
    /// Reads of history Ghostty holds, by any pane on this thread (see
    /// [`history_reads`]).
    static HISTORY_READS: StdCell<u64> = const { StdCell::new(0) };
}

/// How many history rows were read out of Ghostty on this thread, by any
/// pane. A read restores the compressed page it falls in without changing
/// Ghostty's compression activity token (PageList.zig `Node.page`), so a
/// caller that saw this change should look at
/// [`PaneTerm::compression_activity`] again.
#[must_use]
pub fn history_reads() -> u64 {
    HISTORY_READS.with(StdCell::get)
}

/// Compared with an earlier value: whether scrollback may have pages to
/// compress that it did not have then. Ghostty's token (pages added,
/// pruned or reflowed) plus the pane's own history reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompressionActivity {
    ghostty: Option<libghostty_vt::terminal::CompressionActivity>,
    reads: u64,
}

impl CompressionActivity {
    /// Explicit history reads restore pages even without new scrollback.
    #[must_use]
    pub fn history_was_read(self, previous: Self) -> bool {
        self.reads != previous.reads
    }
}

/// What one [`PaneTerm::compress_step`] left to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// More steps would compress more (or verify the pass).
    Pending,
    /// The pass is over. Another step would start a new one, so call again
    /// only once [`PaneTerm::compression_activity`] changed.
    Complete,
    /// Ghostty cannot give compressed memory back on this target
    /// (`mem.zig` `canReclaim`).
    Unsupported,
}

/// A line flag OSC 133 sets on the cursor's line (input.c:3264-3330).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineMark {
    /// `GRID_LINE_START_PROMPT`: `OSC 133;A`, `N`, or `P` without `k=s`.
    Prompt,
    /// `GRID_LINE_START_OUTPUT`: `OSC 133;C`.
    Output,
}

/// What an OSC 133 does to the cursor's line (`input_osc_133`,
/// input.c:3264-3335), as `capture-pane -R` prints it: the line flags it
/// sets, the column it records, and `D`'s exit status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Osc133 {
    /// tmux's `GRID_LINE_*` bits: `START_PROMPT` 0x8, `SECOND_PROMPT` 0x10,
    /// `START_COMMAND` 0x20, `START_OUTPUT` 0x40, `END_OUTPUT` 0x80.
    pub flags: u32,
    pub prompt_col: u16,
    pub cmd_col: u16,
    pub out_start_col: u16,
    pub out_end_col: u16,
    pub exit_status: u8,
}

impl Osc133 {
    /// Apply one OSC 133 payload (after `133;`) at column `cx`.
    fn apply(&mut self, payload: &[u8], cx: u16) {
        match payload.first() {
            Some(b'A' | b'N') => {
                // memset of osc133_data only: the line's flags stay.
                *self = Self {
                    flags: self.flags | 0x8,
                    prompt_col: cx,
                    ..Self::default()
                };
            }
            Some(b'P') => {
                self.flags |= if osc133_mark(payload).is_some() {
                    0x8
                } else {
                    0x10
                };
                self.prompt_col = cx;
            }
            Some(b'B' | b'I') => {
                self.flags |= 0x20;
                self.cmd_col = cx;
            }
            Some(b'C') => {
                self.flags |= 0x40;
                self.out_start_col = cx;
            }
            Some(b'D') => {
                self.flags |= 0x80;
                self.out_end_col = cx;
                self.exit_status = osc133_exit_status(payload);
            }
            _ => {}
        }
    }
}

/// `input_osc_133_exit_status` (input.c:3206-3230): `D;n`, 0 when absent
/// or a key=value, 255 when not 0-255.
fn osc133_exit_status(p: &[u8]) -> u8 {
    if p.get(1) != Some(&b';') || p.len() < 3 || p[2] == b'=' {
        return 0;
    }
    let rest = &p[2..];
    let field = rest.split(|&b| b == b';').next().unwrap_or(b"");
    if field.is_empty() || field.contains(&b'=') {
        return 0;
    }
    // strtonum: an optional sign and digits, nothing else.
    std::str::from_utf8(field)
        .ok()
        .and_then(|f| f.parse::<i64>().ok())
        .filter(|n| (0..=255).contains(n))
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(255)
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

/// Pixel size of one cell until [`PaneTerm::set_cell_pixels`]: tmux's
/// `DEFAULT_XPIXEL` and `DEFAULT_YPIXEL` (tmux.h:122-123), a window's when
/// no client gives one.
const DEFAULT_CELL_PX: (u32, u32) = (16, 32);

/// In-band resize, mode 2048 (spec D31, an extension under D28): whether
/// the server offers it (`vtmux-inband-resize`, read at every feed) and
/// whether the pane's program holds it. A program is granted when it sets
/// `?2048h` while it is offered, and keeps the grant until `?2048l`, RIS
/// or a respawn, whatever the option says later (D28 rule 5). tmux 3.8
/// knows no mode 2048 (`input_csi_dispatch_sm_private`, input.c:2022), so
/// the sequence never reaches Ghostty: vtmux keeps the mode itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct InbandResize {
    offered: bool,
    granted: bool,
}

impl InbandResize {
    /// DECRQM `?2048`: 1 when granted, 2 when offered, else tmux's 0 (its
    /// table has no 2048, input.c:1651-1722).
    fn decrqm(self) -> u8 {
        if self.granted {
            1
        } else if self.offered {
            2
        } else {
            0
        }
    }
}

/// A mode 2048 report for a pane of `cols` x `rows` cells of `cell` pixels:
/// Ghostty's `size_report.encode(.mode_2048)` (size_report.zig:49-58),
/// with the cell size of tmux's XTWINOPS replies, as
/// [`PaneTerm::size_report`].
fn inband_report(cols: u16, rows: u16, (xp, yp): (u32, u32)) -> Vec<u8> {
    let (x, y) = (u32::from(cols), u32::from(rows));
    format!(
        "\x1b[48;{y};{x};{};{}t",
        y.wrapping_mul(yp),
        x.wrapping_mul(xp)
    )
    .into_bytes()
}

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

/// How long synchronized output may hold a pane (`screen_write_start_sync`'s
/// `tv_sec = 1`, screen-write.c:1035).
const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// The default `input-buffer-size` (`INPUT_BUF_DEFAULT_SIZE`, also the
/// option's minimum), which [`PaneTerm::pending_max`] starts from.
const PENDING_MAX: usize = dcs::DEFAULT_BUFFER_SIZE;

/// A DCS's introducer, parameters and intermediates, which come before the
/// payload `input-buffer-size` counts: `ESC P`, at most 63 parameter bytes
/// and 3 intermediates, one more of either discards it (`dcs.rs`), with
/// room to spare.
const PENDING_SLACK: usize = 128;

/// An OSC Ghostty rejects at its first byte (`osc.Parser`'s `invalid`
/// state): it keeps none of the string and dispatches nothing at its end.
const OSC_REJECTED: &[u8] = b"\x1b];";

/// tmux's ground timer (input.c `input_start_ground_timer`).
const DCS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Pops eight kitty keyboard entries, which empties the stack whole
/// (`FlagStack.pop`, ghostty kitty/key.zig: `n >= len` resets it), after
/// CAN, so Ghostty's parser starts from the ground state.
const KITTY_EMPTY: &[u8] = b"\x18\x1b[<8u";

/// How the pane wants modified keys reported (`#{pane_key_mode}`): tmux's
/// `MODE_KEYS_EXTENDED` and `MODE_KEYS_EXTENDED_2`, which the program sets
/// with xterm's modifyOtherKeys request `CSI > 4 ; Pv m`. Ghostty keeps
/// its own modifyOtherKeys state with other semantics, so this is tracked
/// here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyMode {
    /// Standard keys (`VT10x`).
    #[default]
    Standard,
    /// modifyOtherKeys 1 (`Ext 1`): keys standard mode can send stay so.
    Extended1,
    /// modifyOtherKeys 2 (`Ext 2`): every modified key is extended.
    Extended2,
}

/// The server's `extended-keys` option, which decides what the program's
/// key mode requests do (input.c:1527-1559).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtendedKeys {
    /// Requests are ignored.
    #[default]
    Off,
    /// Requests are honoured.
    On,
    /// The pane is always at least in [`KeyMode::Extended1`].
    Always,
}

impl ExtendedKeys {
    /// The mode a reset screen starts in (`screen_reinit`,
    /// `screen_write_reset`).
    fn reset_mode(self) -> KeyMode {
        if self == Self::Always {
            KeyMode::Extended1
        } else {
            KeyMode::Standard
        }
    }
}

/// A value that notes each mutable borrow of it: the pane's libghostty
/// terminal, whose screen is read again only after something could have
/// written it ([`PaneTerm::screen`]). libghostty-vt takes `&mut` for every
/// change of a terminal, so a terminal not borrowed mutably is unchanged.
#[derive(Debug)]
pub(crate) struct Watched<T> {
    value: T,
    touched: StdCell<bool>,
}

impl<T> Watched<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            touched: StdCell::new(true),
        }
    }

    /// Whether it was borrowed mutably since the last call.
    fn take_touched(&self) -> bool {
        self.touched.replace(false)
    }
}

impl<T> std::ops::Deref for Watched<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for Watched<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.touched.set(true);
        &mut self.value
    }
}

/// One pane's terminal. Owns the libghostty terminal; `'static` for callers.
#[allow(clippy::struct_excessive_bools)] // independent pane flags
#[derive(Debug)]
pub struct PaneTerm {
    pub(crate) term: Watched<Terminal<'static, 'static>>,
    /// What the program's input asks back, in input order: Ghostty's
    /// replies as tmux words them, and what only the server can answer.
    replies: Rc<RefCell<Vec<Reply>>>,
    /// What the program's output did that the server reports, since the
    /// last [`PaneTerm::take_events`], in input order.
    events: Rc<RefCell<Events>>,
    /// The pane's title as tmux keeps it, and tmux's title stack.
    titles: Titles,
    /// The pane's `allow-set-title`: OSC 0/2 and APC may set the title.
    allow_set_title: bool,
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
    /// tmux's `hscrolled` (grid.c `struct grid`), the newest `hsize` rows a
    /// taller screen pulls back into view: rows scroll in
    /// (`grid_scroll_history`), a clear into history or of it resets it
    /// (`grid_view_clear_history`, `grid_clear_history`), and a resize moves
    /// it as `screen_resize_y` and `grid_reflow` do (Ghostty patch 0025).
    /// Under the alternate screen only a clear changes it; the resize that
    /// ends it moves it ([`PaneTerm::alternate_ended`]). At most `hsize`.
    hscrolled: usize,
    /// Rows scrolled into the primary history, and those a resize moved
    /// into it less those it took back: the newest history row has id
    /// `scrolled - 1`. Never reduced by collection (as tmux's
    /// `scroll_added`), so a history row keeps its id while more scroll in.
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
    /// tmux's `MODE_MOUSE_*` bits, shared with the scanner and the reply hook.
    mouse: Rc<StdCell<MouseModes>>,
    /// Mode 2048 ([`InbandResize`]), shared with the reply hook for DECRQM.
    inband: Rc<StdCell<InbandResize>>,
    /// A cell's width and height in pixels (`w->xpixel`, `w->ypixel`):
    /// XTWINOPS and mode 2048 report with it, and Ghostty is sized by it.
    cell_px: (u32, u32),
    /// The primary screen's visible rows, copied as the alternate screen
    /// came up (tmux's `saved_grid`, screen.c:704-705), while it is up.
    saved: Option<Rc<Vec<FrozenRow>>>,
    /// The oldest row of tmux's primary history (the active top when it
    /// has none), pinned as the alternate screen came up, and the history
    /// rows Ghostty kept then: a lowered limit may collect history under
    /// the alternate screen, and tmux's history starts at this row again.
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
    marks: Vec<(TrackedGridRef, Osc133)>,
    /// The marks of the primary history as the alternate screen came up.
    frozen_marks: Vec<(usize, Osc133)>,
    /// The input since the parser last left the ground state, while it has
    /// not returned to it (tmux's `since_ground`, input.c:1022-1024).
    pending: Vec<u8>,
    /// Where in `pending` the current sequence's ESC is.
    pending_start: usize,
    /// tmux's own parser for DCS strings, which Ghostty never sees.
    dcs: Dcs,
    /// `input-buffer-size`, the most a DCS payload may hold.
    input_buffer_size: usize,
    /// `DCS tmux; ... ST` payloads since the last
    /// [`PaneTerm::take_passthrough`].
    passthrough: Vec<Passthrough>,
    /// When the DCS being read began (tmux's five-second ground timer).
    dcs_since: Option<std::time::Instant>,
    /// The screen-style window name (`ESC k name ST`) being read, and
    /// those finished since the last [`PaneTerm::take_renames`].
    rename: Rename,
    /// History rows read out of Ghostty (see [`history_reads`]).
    reads: StdCell<u64>,
    /// The visible grid, kept up to date from Ghostty's dirty rows. It is
    /// the only reader of Ghostty's render state, whose dirty flags one
    /// reader consumes.
    pub(crate) view: RefCell<crate::snapshot::ScreenView>,
    /// The pane's `scroll-on-clear` option (default on): a clear of the
    /// whole screen moves its used rows into history first.
    scroll_on_clear: bool,
    /// tmux's `MODE_SYNC` ([`SyncHold`]).
    sync: SyncHold,
    /// The server's `vtmux-kitty-keys` (D29) the next input is read under.
    kitty_option: bool,
    /// The kitty keyboard stack of the screen not in use was emptied
    /// ([`PaneTerm::reset_kitty_keys`]) and Ghostty has it still: emptied
    /// as that screen comes up, and exported empty.
    kitty_stale: bool,
    /// A screen's kitty keyboard flags went from 0 to non-zero in the last
    /// feed (primary, alternate): a program was granted
    /// ([`PaneTerm::take_kitty_grants`]).
    kitty_new: [bool; 2],
    /// The server's `vtmux-graphics` (D33) the next input is read under.
    graphics_option: bool,
    /// D33: each screen's grant, primary then alternate.
    graphics_granted: [bool; 2],
    /// D33: a screen granted in the last input, for the server to note
    /// its owner ([`PaneTerm::take_graphics_grants`]).
    graphics_new: [bool; 2],
    /// D33: the storage limit a screen gets when it is granted (the
    /// server's share of `vtmux-graphics-limit`).
    graphics_share: u64,
    /// D33: set while a granted screen's graphics command ends, so its
    /// reply reaches the program (`tmux_reply`).
    graphics_reply: Rc<StdCell<bool>>,
    /// The links cells of this pane are drawn with (tmux's
    /// `s->hyperlinks`), shared with its grid's copies.
    pub(crate) links: Rc<RefCell<crate::link::Links>>,
}

/// tmux's `MODE_SYNC` (screen-write.c:211-240).
#[derive(Debug, Default)]
struct SyncHold {
    /// The screen as it was when the program last entered synchronized
    /// output (`?2026h`), which is what the clients keep showing while the
    /// mode is on, and when the 1 s sync timer was last armed
    /// (`screen_write_start_sync` re-arms it on every `?2026h`).
    hold: Option<(HeldScreen, std::time::Instant)>,
    /// The row stamps of the hold that synchronized output last ended with
    /// (`?2026l`, the timer, [`PaneTerm::stop_sync`]), until
    /// [`PaneTerm::take_sync_flush`].
    ended: Option<Rc<[u64]>>,
}

/// What a pane in synchronized output keeps showing ([`PaneTerm::sync_held`]):
/// its grid when the update began, and that grid's row stamps
/// ([`crate::ScreenView::row_stamps`]), which still describe it. The grid
/// is the view's own, shared: a hold copies nothing, and the view copies
/// its grid only if the pane's rows are read while the hold is on (pi and
/// other ink apps end the update in the same read).
#[derive(Clone, Debug)]
pub struct HeldScreen {
    pub screen: Rc<crate::Snapshot>,
    pub stamps: Rc<[u64]>,
}

/// A `DCS tmux;` payload (input.c `input_dcs_dispatch`), unescaped, for the
/// server to write to the clients when `allow-passthrough` says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passthrough {
    pub data: Vec<u8>,
    /// The pane already had a redraw coming from earlier in the same input
    /// (`PANE_REDRAW`, screen-write.c:155).
    pub redraw: bool,
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
        Self::try_new(cols, rows, history_lines).expect("libghostty terminal allocation")
    }

    /// [`Self::new`], or `None` if libghostty can't allocate the terminal
    /// (a size from untrusted input, such as a snapshot).
    #[must_use]
    #[allow(clippy::too_many_lines)] // one registration per Ghostty effect
    pub fn try_new(cols: u16, rows: u16, history_lines: usize) -> Option<Self> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut term = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: if history_lines == 0 { 0 } else { usize::MAX },
        })
        .ok()?;

        let replies: Rc<RefCell<Vec<Reply>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&replies);
        let theme_sink = Rc::clone(&replies);
        let cursor = Rc::new(StdCell::new(CursorState::default()));
        let cursor_seen = Rc::clone(&cursor);
        let cursor_scan = Rc::clone(&cursor);
        let mouse = Rc::new(StdCell::new(MouseModes::default()));
        let mouse_seen = Rc::clone(&mouse);
        let mouse_scan = Rc::clone(&mouse);
        let inband = Rc::new(StdCell::new(InbandResize::default()));
        let inband_seen = Rc::clone(&inband);
        let graphics_reply = Rc::new(StdCell::new(false));
        let graphics_seen = Rc::clone(&graphics_reply);
        let events = Rc::new(RefCell::new(Events::default()));
        let belled = Rc::clone(&events);
        let notified = Rc::clone(&events);
        let registered = term
            // Every reply Ghostty wants to send goes through `tmux_reply`.
            .on_pty_write(move |term, data| {
                let modes = SeenModes {
                    cursor: cursor_seen.get(),
                    mouse: mouse_seen.get(),
                    inband: inband_seen.get(),
                    graphics: graphics_seen.get(),
                };
                if let Some(reply) = tmux_reply(term, modes, data) {
                    push_bytes(&mut sink.borrow_mut(), &reply);
                }
            })
            // DSR 996 (input.c:1631-1637): the server knows the theme, so
            // Ghostty answers nothing.
            .and_then(|t| {
                t.on_color_scheme(move |_| {
                    theme_sink.borrow_mut().push(Reply::Theme);
                    None
                })
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
            .and_then(|t| t.on_bell(move |_| belled.borrow_mut().push(TermEvent::Bells(1))))
            // D30: the server decides who gets it (`vtmux-notifications`).
            .and_then(|t| {
                t.on_desktop_notification(move |_, title, body| {
                    notified
                        .borrow_mut()
                        .push(TermEvent::Notification(Notification {
                            title: title.to_vec(),
                            body: body.to_vec(),
                        }));
                })
            })
            // XTVERSION: `DCS > | tmux 3.8 ST` (input.c:1885-1890).
            .and_then(|t| t.on_xtversion(|_| Some("tmux 3.8")));
        registered.expect("libghostty effect registration");

        // tmux implements neither Kitty graphics nor the Glyph Protocol, so
        // keep their state (and memory) out. Both calls only fail when the
        // feature is compiled out, which is the outcome we want anyway.
        let _ = term.set_kitty_image_storage_limit(0);
        // D33: images arrive only as data in the escape (`t=d`). File,
        // temporary-file and shared-memory media are refused for every
        // pane (Ghostty answers `EINVAL: unsupported medium`, `LoadingImage
        // .init`), so no pane, nor a program on another host through ssh,
        // makes the server open a path. Ghostty's default
        // (`LoadingImage.Limits.direct`), set here to say so.
        let _ = term.set_kitty_image_from_file_allowed(false);
        let _ = term.set_kitty_image_temp_dir(None);
        let _ = term.set_kitty_image_from_shared_mem_allowed(false);
        let _ = term.set_glyph_protocol_enabled(false);
        keep_clustering(&mut term);
        // Modes 47, 1047 and 1049 as tmux switches screens, the primary
        // screen left at its size under the alternate one (patch 0028).
        term.vtmux_set_tmux_alternate(true)
            .expect("libghostty tmux alternate screen");

        let mut pane = Self {
            term: Watched::new(term),
            replies,
            events,
            titles: Titles::default(),
            allow_set_title: true,
            history_lines,
            history: history_lines > 0,
            alt_collected: 0,
            page_rows: page_rows(cols),
            line_limit: None,
            hsize: 0,
            hscrolled: 0,
            scrolled: 0,
            generation: 0,
            times: VecDeque::new(),
            times_base: 0,
            pin: None,
            cursor,
            mouse,
            inband,
            cell_px: DEFAULT_CELL_PX,
            scan: AltModeScanner {
                cursor: cursor_scan,
                mouse: mouse_scan,
                osc_cap: dcs::capacity(dcs::DEFAULT_BUFFER_SIZE),
                ..AltModeScanner::default()
            },
            saved: None,
            alt_entry: None,
            clear_primary_history: false,
            redraw: false,
            marks: Vec::new(),
            frozen_marks: Vec::new(),
            pending: Vec::new(),
            pending_start: 0,
            dcs: Dcs::default(),
            input_buffer_size: dcs::DEFAULT_BUFFER_SIZE,
            passthrough: Vec::new(),
            dcs_since: None,
            rename: Rename::default(),
            reads: StdCell::new(0),
            view: RefCell::default(),
            scroll_on_clear: true,
            sync: SyncHold::default(),
            kitty_option: false,
            kitty_stale: false,
            kitty_new: [false; 2],
            graphics_option: false,
            graphics_granted: [false; 2],
            graphics_new: [false; 2],
            graphics_share: 0,
            graphics_reply,
            links: Rc::default(),
        };
        pane.retain();
        // D33: a graphics command is held to what no screen can store yet
        // (Ghostty's default would keep 65 MiB of one with no end).
        pane.bound_graphics_command();
        Some(pane)
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
    #[expect(
        clippy::too_many_lines,
        reason = "one dispatch over the sequences tmux handles itself"
    )]
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
        // `input_start_ground_timer` (input.c:2548 and the other string
        // starts): a string not ended in five seconds is abandoned. Nothing
        // shows until the next byte, so noticing then is the same.
        if self.scan.in_dcs() && self.dcs_since.is_some_and(|t| t.elapsed() >= DCS_TIMEOUT) {
            self.scan.end_dcs();
        }
        self.rename_timeout();
        self.scan.line_marks = !self.marks.is_empty();
        let mut i = 0;
        while i < bytes.len() {
            if self.scan.in_dcs() {
                i = self.feed_dcs(bytes, i);
                continue;
            }
            if self.scan.state == ScanState::Rename {
                i = self.feed_rename(bytes, i);
                continue;
            }
            let mut j = i;
            let mut event = None;
            let mut plain_char = None;
            while j < bytes.len() && j - i < step {
                if self.scan.state == ScanState::Ground && !self.scan.utf8.started() {
                    // Printable ASCII changes nothing the scanner keeps.
                    let end = i + step.min(bytes.len() - i);
                    let run = bytes[j..end]
                        .iter()
                        .position(|b| !(0x20..0x7f).contains(b))
                        .unwrap_or(end - j);
                    if run > 0 {
                        self.pending.clear();
                        self.scan.last_print = true;
                        j += run;
                        continue;
                    }
                    // A whole, valid UTF-8 character goes to Ghostty as it
                    // is; anything else goes through tmux's decoder in the
                    // scanner.
                    if bytes[j] >= 0x80
                        && let Some(n) = utf8_char_len(&bytes[j..])
                    {
                        self.pending.clear();
                        self.scan.last_print = true;
                        if self.scan.charset.acs() {
                            plain_char = Some(n);
                            break;
                        }
                        j += n;
                        continue;
                    }
                }
                let e = self.scan.push(bytes[j]);
                self.note_pending(bytes[j], self.scan.state != ScanState::Ground);
                // A palette OSC ended: tmux redraws the pane at once, ahead
                // of whatever follows in this input.
                self.redraw |= std::mem::take(&mut self.scan.palette_redraw);
                if std::mem::take(&mut self.scan.progress_set) {
                    self.events.borrow_mut().push(TermEvent::Progress);
                }
                if let Some(e) = e {
                    event = Some(e);
                    break;
                }
                j += 1;
            }
            self.feed_piece(&bytes[i..j]);
            i = j;
            if let Some(n) = plain_char {
                self.feed_plain_char(&bytes[i..i + n]);
                i += n;
                continue;
            }
            if event == Some(ScanEvent::DcsStart) {
                // `ESC P` is tmux's alone: Ghostty has the ESC (it ended
                // whatever string was open, as in tmux) and CAN takes it
                // back to the ground state.
                self.term.vt_write(b"\x18");
                self.dcs.begin();
                self.dcs_since = Some(std::time::Instant::now());
                i += 1;
                continue;
            }
            if event == Some(ScanEvent::RenameStart) {
                self.rename_start();
                i += 1;
                continue;
            }
            if event == Some(ScanEvent::Skip) {
                i += 1;
                continue;
            }
            if event == Some(ScanEvent::Utf8) {
                if let Some(out) = self.scan.utf8_out.take() {
                    self.feed_plain_char(&out);
                }
                i += 1;
                continue;
            }
            if let Some(e) = event {
                // The modes tmux knows of an SM or RM: Ghostty drops what
                // it has of the sequence and gets them, then the final.
                if let Some(start) = self.scan.rewrite.take() {
                    self.term.vt_write(b"\x18");
                    self.term.vt_write(&start);
                }
                for set in std::mem::take(&mut self.scan.inband) {
                    self.inband_mode(set);
                }
                match self.scan.sync.take() {
                    Some(true) => self.start_sync(),
                    // input.c:1981: screen_write_end_sync; the clients
                    // show the screen as it is now.
                    Some(false) => self.end_sync_hold(),
                    None => {}
                }
                // RIS: screen_write_reset leaves only MODE_CURSOR|MODE_WRAP
                // (screen-write.c:430), and the pane is redrawn whole.
                if e == ScanEvent::Reset {
                    self.sync.hold = None;
                    // Ghostty's own RIS resets mode 2048 with the rest
                    // (`Terminal.fullReset`): the program let it go.
                    self.end_inband_resize();
                }
                let alternate = self.alternate_on();
                // `?1049l` gives the pen the copy `screen_alternate_on` took,
                // if the alternate screen ever saved the cursor.
                let restores_pen = e == ScanEvent::AltMode(b'l')
                    && self.scan.alt_cursor
                    && self.alternate_saved().is_some();
                // The size an ending alternate screen leaves the pane at.
                let size = (alternate && e == ScanEvent::AltMode(b'l'))
                    .then(|| (self.cols(), self.rows()));
                match e {
                    // The alternate screen is about to switch. tmux goes on
                    // showing the primary history (screen.c:700-721 saves
                    // only the visible rows), which Ghostty leaves as it is
                    // and reads in place (patch 0038): only the visible
                    // rows and the marks are kept.
                    ScanEvent::AltMode(b'h') if !self.alternate_on() => {
                        self.saved = Some(Rc::new(self.freeze_visible()));
                        self.frozen_marks = self.osc133_lines();
                        self.alt_entry = self.pin_oldest().map(|pin| (pin, self.retained_rows()));
                    }
                    // tmux's ED3 clears the grid's history, which under the
                    // alternate screen is the primary one (input.c:1758).
                    ScanEvent::ClearHistory if self.alternate_on() => {
                        self.drop_oldest_frozen_marks(self.hsize);
                        self.hsize = 0;
                        self.hscrolled = 0;
                        self.clear_primary_history = true;
                    }
                    _ => {}
                }
                if e == ScanEvent::ClearHistory {
                    self.generation += 1;
                }
                match e {
                    // RIS is tmux's (input.c:1407-1412): `screen_write_reset`,
                    // the same reset as `send-keys -R`, which keeps history,
                    // the alternate screen and the title. Ghostty's own RIS
                    // is a full reset, so it never sees the `c`.
                    ScanEvent::Reset => {
                        self.term.vt_write(b"\x18");
                        self.reset_screen(self.scroll_on_clear);
                        // D29: Ghostty's full reset empties both screens'
                        // kitty stacks (Terminal.zig `fullReset`), so a
                        // `reset` gives the shell tmux's keys back.
                        self.reset_kitty_keys();
                        // D33: the grants end and the images go.
                        self.reset_graphics();
                    }
                    // D29: a kitty keyboard push, pop, set or query reaches
                    // Ghostty while `vtmux-kitty-keys` is on or the screen
                    // is granted; otherwise it is dropped as tmux drops it
                    // (no `u` entry with a prefix, input.c:347).
                    ScanEvent::Kitty if self.kitty_option || self.kitty_granted() => {
                        let before = self.kitty_granted();
                        self.feed_piece(&bytes[i..=i]);
                        if !before && self.kitty_granted() {
                            self.kitty_new[usize::from(self.alternate_on())] = true;
                        }
                    }
                    ScanEvent::Kitty => self.term.vt_write(b"\x18"),
                    // OSC 133 only marks the line in tmux (input.c
                    // `input_osc_133`): Ghostty's `133;A` would start a
                    // fresh line, and its prompt rows are erased on resize
                    // and change ED 2.
                    ScanEvent::Osc133 => self.cancel_string(bytes[i]),
                    // tmux discards an OSC past `input-buffer-size`
                    // (`input_input`, `INPUT_DISCARD`). Ghostty drops
                    // what it holds (CAN, which frees nothing until the
                    // next OSC starts) and starts one it rejects at
                    // its first byte, which it skips without keeping
                    // anything through the BEL or ST.
                    ScanEvent::OscOver => {
                        self.term.vt_write(b"\x18");
                        self.term.vt_write(OSC_REJECTED);
                    }
                    // An OSC 0 or 2 is tmux's alone (`input_exit_osc`):
                    // Ghostty never keeps a title of its own.
                    // D33: a graphics command (`APC G`) is Ghostty's while
                    // `vtmux-graphics` is on or the screen is granted; else
                    // it is a title, as every APC is in tmux
                    // (`input_exit_apc`), and Ghostty never runs it.
                    ScanEvent::Title if self.scan.title_done.as_ref().is_some_and(|t| t.kitty) => {
                        if self.graphics_option
                            || self.graphics_granted[usize::from(self.alternate_on())]
                        {
                            self.scan.title_done = None;
                            self.graphics_command(&bytes[i..=i]);
                        } else {
                            if bytes[i] == 0x1b {
                                self.term.vt_write(b"\x18");
                            }
                            self.feed_piece(&bytes[i..=i]);
                        }
                    }
                    ScanEvent::Title if self.scan.title_done.as_ref().is_some_and(|t| !t.apc) => {
                        self.cancel_string(bytes[i]);
                    }
                    ScanEvent::EraseBelow | ScanEvent::EraseScreen
                        if self.clear_into_history(e) =>
                    {
                        // grid_view_clear_history: the used rows went into
                        // history with their marks, the rest are cleared.
                        self.drop_marks(0..self.rows());
                    }
                    ScanEvent::EraseBelow
                    | ScanEvent::EraseAbove
                    | ScanEvent::EraseScreen
                    | ScanEvent::EraseChars => {
                        let rows = self.erased_rows(e);
                        self.feed_piece(&bytes[i..=i]);
                        self.drop_marks(rows);
                    }
                    ScanEvent::Drop => {
                        self.term.vt_write(b"\x18");
                        // An ESC that ended an OSC starts a new sequence.
                        if bytes[i] == 0x1b {
                            self.feed_piece(&bytes[i..=i]);
                        }
                    }
                    // input_stop_utf8: the unfinished character is one
                    // U+FFFD, then the byte that ended it; CAN and SUB
                    // first, as they may end a string Ghostty is in.
                    ScanEvent::Utf8Stop if matches!(bytes[i], 0x18 | 0x1a) => {
                        self.feed_piece(&bytes[i..=i]);
                        self.feed_piece(REPLACEMENT);
                    }
                    ScanEvent::Utf8Stop => {
                        self.feed_piece(REPLACEMENT);
                        self.feed_piece(&bytes[i..=i]);
                    }
                    _ => self.feed_piece(&bytes[i..=i]),
                }
                i += 1;
                if let Some(size) = size
                    && !self.alternate_on()
                {
                    self.alternate_ended(size);
                }
                if std::mem::take(&mut self.scan.deccolm) {
                    self.deccolm();
                }
                self.scanned_strings(e);
                if matches!(e, ScanEvent::AltMode(_) | ScanEvent::Reset)
                    && (e == ScanEvent::Reset || self.alternate_on() != alternate)
                {
                    self.redraw = true;
                }
                if let ScanEvent::AltMode(set) = e {
                    self.alternate_links(set, alternate, restores_pen);
                    if self.alternate_on() != alternate && std::mem::take(&mut self.kitty_stale) {
                        self.term.vt_write(KITTY_EMPTY);
                    }
                }
                if self.scan.take_resync() {
                    self.sync_pen();
                }
                if e == ScanEvent::Reset {
                    keep_clustering(&mut self.term);
                }
                // After the bytes before them, whose replies Ghostty gave.
                self.replies.borrow_mut().append(&mut self.scan.answers);
            }
        }
    }

    /// Ghostty gets CAN for the OSC the scanner's byte `end` ended, so it
    /// acts on none of it: a BEL that ended it is not a bell, and the ESC
    /// that ended it starts a new sequence, as in tmux.
    fn cancel_string(&mut self, end: u8) {
        self.term.vt_write(b"\x18");
        if end != 0x07 {
            self.feed_piece(&[end]);
        }
    }

    /// The pen's link over a screen switch sequence ([`PenLinks`]): entering
    /// the alternate screen copies it, `?1049l` may restore the copy, and a
    /// switch ends Ghostty's link.
    fn alternate_links(&mut self, set: u8, was_alternate: bool, restores_pen: bool) {
        let switched = self.alternate_on() != was_alternate;
        let links = &mut self.scan.links;
        if set == b'h' && !was_alternate {
            links.alternate.clone_from(&links.pen);
        }
        if restores_pen {
            links.pen.clone_from(&links.alternate);
        }
        if switched {
            links.ghostty = None;
        }
    }

    /// What the scanner collected for an OSC 133 or an XTWINOPS title
    /// stack sequence that just ended, applied after Ghostty saw it.
    fn scanned_strings(&mut self, e: ScanEvent) {
        if e == ScanEvent::Osc133
            && let Some(payload) = self.scan.osc133.take()
        {
            self.mark_cursor_line(&payload);
            // input_osc_133 (input.c:3289-3328): the pane's events, on
            // either screen.
            let event = match payload.first() {
                Some(b'A' | b'N') => Some(TermEvent::Prompt),
                Some(b'C') => Some(TermEvent::CommandStarted),
                Some(b'D') => Some(TermEvent::CommandFinished(osc133_exit_status(&payload))),
                _ => None,
            };
            if let Some(event) = event {
                self.events.borrow_mut().push(event);
            }
        }
        if e == ScanEvent::TitleStack {
            for op in std::mem::take(&mut self.scan.title_ops) {
                match op {
                    WinOp::Push => self.title_stack_op(true),
                    WinOp::Pop => self.title_stack_op(false),
                    WinOp::Report(n) => {
                        let reply = self.size_report(n);
                        push_bytes(&mut self.replies.borrow_mut(), &reply);
                    }
                }
            }
        }
        if e == ScanEvent::Title
            && let Some(TitleDone {
                text: Some(text), ..
            }) = self.scan.title_done.take()
        {
            self.program_title(&text);
        }
        self.scan.title_done = None;
    }

    /// Answer a DECRQSS that just ended (`input_handle_decrqss`,
    /// input.c:2553-2617): `DCS 1 $ r SP q Ps SP q ST` for a request that
    /// starts `SP q`, `DCS 0 $ r ST` for any other. Ghostty's own DECRQSS
    /// replies (SGR, DECSTBM, DECSLRM, cursor) are dropped by `tmux_reply`:
    /// tmux knows only the cursor style.
    fn answer_decrqss(&mut self, recognised: bool) {
        let reply = if recognised {
            format!("\x1bP1$r q{} q\x1b\\", self.cursor.get().decrqss_ps())
        } else {
            "\x1bP0$r\x1b\\".to_owned()
        };
        push_bytes(&mut self.replies.borrow_mut(), reply.as_bytes());
    }

    /// The bytes of a DCS from `bytes[i..]`, through its end; the index
    /// after them. An ESC that is not a doubled one or ST starts a new
    /// sequence and is left for the caller (input.c's `ANYWHERE` rule).
    fn feed_dcs(&mut self, bytes: &[u8], mut i: usize) -> usize {
        while i < bytes.len() {
            match self.dcs.push(bytes[i], self.input_buffer_size) {
                Step::Stay => {
                    self.note_pending(bytes[i], true);
                    i += 1;
                }
                Step::End(dispatch) => {
                    match dispatch {
                        Dispatch::Passthrough(data) => {
                            let redraw = self.redraw;
                            self.passthrough.push(Passthrough { data, redraw });
                        }
                        Dispatch::Decrqss(recognised) => self.answer_decrqss(recognised),
                        Dispatch::Ignore => {}
                    }
                    self.scan.end_dcs();
                    self.pending.clear();
                    return i + 1;
                }
                Step::Cancel => {
                    self.scan.end_dcs();
                    self.pending.clear();
                    return i + 1;
                }
                Step::Escape => {
                    self.scan.end_dcs();
                    return i;
                }
            }
        }
        i
    }

    /// Keep `b` in the pending input while the parser is out of the ground
    /// state after it; back in the ground state the input is dropped
    /// (`input_ground`). Past [`Self::pending_max`] it keeps the current
    /// sequence, from its ESC: [`Self::ghostty_partial`] needs its start,
    /// and over the limit it is the only one there.
    fn note_pending(&mut self, b: u8, in_sequence: bool) {
        if !in_sequence {
            self.pending.clear();
            return;
        }
        if b == 0x1b && self.scan.state == ScanState::Esc {
            self.pending_start = self.pending.len();
        }
        let max = self.pending_max();
        if self.pending.len() > max && self.pending_start > 0 {
            self.pending
                .drain(..self.pending_start.min(self.pending.len()));
            self.pending_start = 0;
        }
        if self.pending.len() <= max {
            self.pending.push(b);
        }
    }

    /// The most pending input [`PaneTerm::pending_input`] keeps (one byte
    /// more says it was cut). tmux keeps all of it but discards a string
    /// past `input-buffer-size` (input.c `input_input`), so this keeps
    /// every string tmux does not discard, whole: the live upgrade carries
    /// it, and a sequence cut short there would end as another one after
    /// it (a DCS passthrough with its middle gone). A DCS payload takes two
    /// bytes for each ESC it holds (`ESC ESC`, input.c
    /// `input_state_dcs_escape_table` keeps the second), hence twice the
    /// limit, and its header comes first ([`PENDING_SLACK`]).
    fn pending_max(&self) -> usize {
        self.input_buffer_size
            .max(PENDING_MAX)
            .saturating_mul(2)
            .saturating_add(PENDING_SLACK)
    }

    /// `capture-pane -P`: the start of an escape sequence the pane has not
    /// finished yet (`input_pending`).
    #[must_use]
    pub fn pending_input(&self) -> &[u8] {
        &self.pending
    }

    /// `history-limit` as the pane has it (`hlimit`).
    #[must_use]
    pub fn history_limit(&self) -> usize {
        self.history_lines
    }

    /// The bytes of a rename string from `bytes[i..]` (`input_state_rename_string_table`,
    /// input.c:742-752): 0x20-0xff are collected (`input_input`, discarded
    /// past `input-buffer-size`), other C0 controls ignored. ESC, CAN and
    /// SUB leave the state, which runs `input_exit_rename`: the name is
    /// done, and the byte is left for the caller to parse as usual (`ESC \`
    /// is then the ST it usually is). The index of that byte.
    fn feed_rename(&mut self, bytes: &[u8], mut i: usize) -> usize {
        while i < bytes.len() {
            match bytes[i] {
                0x1b | 0x18 | 0x1a => {
                    self.scan.state = ScanState::Ground;
                    let buf = std::mem::take(&mut self.rename.buf);
                    if !std::mem::take(&mut self.rename.discard) {
                        self.rename.done.push(buf);
                    }
                    return i;
                }
                b @ 0x20..=0xff => {
                    // input_input: the buffer doubles up to the limit.
                    if self.rename.buf.len() >= self.scan.osc_cap {
                        self.rename.discard = true;
                    } else if !self.rename.discard {
                        self.rename.buf.push(b);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        i
    }

    /// The window names the program set with `ESC k name ST` since the
    /// last call, in order, as raw bytes (not checked: `input_exit_rename`
    /// checks `allow-rename` and UTF-8 then).
    pub fn take_renames(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.rename.done)
    }

    /// `ESC k` (`input_enter_rename`): the name starts empty, under the
    /// ground timer. It is tmux's alone, as `ESC P` is: Ghostty would
    /// print the name, so it gets CAN for the ESC it has seen.
    fn rename_start(&mut self) {
        self.term.vt_write(b"\x18");
        self.rename.buf.clear();
        self.rename.discard = false;
        self.rename.since = Some(std::time::Instant::now());
    }

    /// The ground timer abandons a rename not ended in five seconds:
    /// `input_reset`, which runs no exit handler, so no name.
    fn rename_timeout(&mut self) {
        if self.scan.state == ScanState::Rename
            && self
                .rename
                .since
                .is_some_and(|t| t.elapsed() >= DCS_TIMEOUT)
        {
            self.scan.state = ScanState::Ground;
        }
    }

    /// The `extended-keys` option the next input is read under.
    pub fn set_extended_keys(&mut self, option: ExtendedKeys) {
        self.scan.extended_keys = option;
    }

    /// The key mode a new or reinitialised screen has (`screen_reinit`):
    /// [`KeyMode::Extended1`] under `extended-keys always`, else standard.
    pub fn reset_key_mode(&mut self) {
        self.scan.key_mode = self.scan.extended_keys.reset_mode();
    }

    /// How the program wants modified keys sent.
    #[must_use]
    pub fn key_mode(&self) -> KeyMode {
        self.scan.key_mode
    }

    /// The server's `vtmux-kitty-keys` (D29) the next input is read under:
    /// whether a kitty keyboard push, set or query that is not granted
    /// reaches the terminal.
    pub fn set_kitty_keys(&mut self, on: bool) {
        self.kitty_option = on;
    }

    /// The kitty keyboard flags the active screen's keys are encoded with
    /// ([`crate::kitty_keys::encode`]): the screen's own, of those vtmux
    /// applies. 0 for a screen not granted, whose keys are tmux's.
    #[must_use]
    pub fn kitty_keys(&self) -> u8 {
        self.kitty_flags() & crate::kitty_keys::APPLIED
    }

    /// The active screen's kitty keyboard flags as Ghostty keeps them
    /// (`Terminal::kitty_keyboard_flags`, the top of the screen's stack).
    fn kitty_flags(&self) -> u8 {
        self.term.kitty_keyboard_flags().map_or(0, |f| f.bits())
    }

    /// D29's grant: the active screen's program set non-zero flags while
    /// the option was on (nothing else reaches the stack, so its flags are
    /// non-zero exactly while it is granted), and they stay its own until
    /// its stack is back to 0, whatever the option says.
    #[must_use]
    pub fn kitty_granted(&self) -> bool {
        self.kitty_flags() != 0
    }

    /// The screens (primary, alternate) whose program was granted since
    /// the last call: its flags went from 0 to non-zero by a push or set.
    /// The server notes which process group that was, to end the grant
    /// with it ([`PaneTerm::end_kitty_grant`]).
    pub fn take_kitty_grants(&mut self) -> [bool; 2] {
        std::mem::take(&mut self.kitty_new)
    }

    /// End one screen's grant: empty its kitty keyboard stack, because the
    /// process group that pushed it is gone (a killed program never pops).
    /// The screen not in use is emptied when it comes up, as for
    /// [`PaneTerm::reset_kitty_keys`]. `false` when it cannot be done now,
    /// the parser being inside a sequence or character that the cancel
    /// would cut: the caller asks again.
    pub fn end_kitty_grant(&mut self, alternate: bool) -> bool {
        if alternate != self.alternate_on() {
            self.kitty_stale = true;
            return true;
        }
        if self.scan.state != ScanState::Ground || self.scan.utf8.started() {
            return false;
        }
        self.term.vt_write(KITTY_EMPTY);
        true
    }

    /// Empty both screens' kitty keyboard stacks, ending any grant: a
    /// respawn and RIS. The screen not in use is emptied when it comes up.
    pub fn reset_kitty_keys(&mut self) {
        self.term.vt_write(KITTY_EMPTY);
        self.kitty_stale = true;
    }

    /// `input_reset(ictx, 1)` as `send-keys -R` calls it (input.c:929-949):
    /// the parser back to the ground state, the cell, charsets and palette
    /// to their defaults, then `screen_write_reset` (screen-write.c:
    /// 423-437): default tabs, the whole screen as the scroll region, modes
    /// back to cursor and wrap (and the `extended-keys always` key mode),
    /// the screen cleared and the cursor home.
    ///
    /// tmux's clear is `screen_write_clearscreen` with `scroll-on-clear`
    /// (`scroll_on_clear`): the screen's used rows scroll into history
    /// (`grid_view_clear_history`), except under an alternate screen, which
    /// has no history and is erased. History is kept and an alternate
    /// screen stays up: unlike RIS in Ghostty, which drops both. Ghostty
    /// does it as DECSTR (soft reset), the mode resets DECSTR leaves, tab
    /// stops, then `CSI 22 J` (scroll the active area into scrollback) or
    /// `CSI 2 J`.
    pub fn reset(&mut self, scroll_on_clear: bool) {
        self.reset_input();
        self.reset_screen(scroll_on_clear);
        let _ = self.take_replies();
    }

    /// `screen_write_reset` with the cell, charsets, palette, cursor style
    /// and mouse modes as `input_reset` and RIS leave them (see
    /// [`PaneTerm::reset`]).
    fn reset_screen(&mut self, scroll_on_clear: bool) {
        self.scan.palette.clear();
        self.scan.mouse.set(MouseModes::default());
        self.scan.key_mode = self.scan.extended_keys.reset_mode();
        self.scan.charset_op(CharsetOp::Reset);
        let mut c = self.cursor.get();
        c.reset();
        self.cursor.set(c);
        let clear: &[u8] = if scroll_on_clear && self.history && !self.alternate_on() {
            b"\x1b[22J"
        } else {
            b"\x1b[2J"
        };
        // DECSTR, then what screen_write_reset turns off that DECSTR keeps:
        // application keypad, mouse, focus, bracketed paste, LNM, origin;
        // then default tab stops (DECST8C), the clear and the cursor home.
        let bytes: &[u8] = b"\x1b[!p\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[20l\x1b[?6l\x1b[?7h\x1b[?25h\x1b[?5W";
        // Past the scanner, which would drop DECSTR and take the clear for a
        // program's ED; its state was reset above.
        self.feed_piece(bytes);
        self.feed_piece(clear);
        // screen_write_clearscreen: what stays on the screen is emptied.
        self.drop_marks(0..self.rows());
        self.feed_piece(b"\x1b[H");
        keep_clustering(&mut self.term);
        if self.scan.take_resync() {
            self.sync_pen();
        }
        self.redraw = true;
    }

    /// Set the pane's `scroll-on-clear` option.
    pub fn set_scroll_on_clear(&mut self, on: bool) {
        self.scroll_on_clear = on;
    }

    /// ED 2, or ED 0 at the top left, under `scroll-on-clear`
    /// (`screen_write_clearscreen`, `screen_write_clearendofscreen`,
    /// screen-write.c:1936-1942, 2090-2096): the screen's used rows scroll
    /// into history before the clear (`grid_view_clear_history`), where
    /// Ghostty erases them. Only a pane with history (`GRID_HISTORY`), so
    /// never the alternate screen. Returns whether it did the erase; if
    /// not, the final byte is still to be fed.
    ///
    /// Ghostty has the sequence up to its final byte: CAN drops it, then
    /// `CSI 22 J` scrolls the rows up to the last one used into
    /// scrollback, the cursor goes back where it was (tmux does not move
    /// it, Ghostty's scroll does), and `CSI 2 J` clears the rest in the
    /// current background as tmux's clear does.
    fn clear_into_history(&mut self, e: ScanEvent) -> bool {
        if !(self.history && self.scroll_on_clear) || self.alternate_on() {
            return false;
        }
        if e == ScanEvent::EraseBelow && tmux_cursor(&self.term) != (0, 0) {
            return false;
        }
        let (x, y) = (
            self.term.cursor_x().unwrap_or(0),
            self.term.cursor_y().unwrap_or(0),
        );
        let pending = self.term.is_cursor_pending_wrap().unwrap_or(false);
        self.term.vt_write(b"\x18");
        let scrolled = self.scrolled;
        self.feed_piece(b"\x1b[22J");
        // grid_view_clear_history: the rows it scrolled into history are
        // not pulled back by a taller screen (nor anything before them); a
        // screen with no line used only clears.
        if self.scrolled != scrolled {
            self.hscrolled = 0;
        }
        // CUP is relative to the scroll region under origin mode.
        let origin = self.term.mode(Mode::ORIGIN).unwrap_or(false);
        let _ = self.term.set_mode(Mode::ORIGIN, false);
        self.term
            .vt_write(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
        // A wrap pending stays pending (tmux's cx stays past the last
        // column): CUP dropped it and only a print under autowrap sets it
        // (patch 0016), so a space goes in the last column with autowrap
        // on, which the clear then erases (Ghostty's ED 2 keeps the
        // pending wrap, patch 0014).
        if pending {
            let wrap = self.term.mode(Mode::WRAPAROUND).unwrap_or(true);
            let _ = self.term.set_mode(Mode::WRAPAROUND, true);
            self.term.vt_write(b" ");
            let _ = self.term.set_mode(Mode::WRAPAROUND, wrap);
        }
        self.term.vt_write(b"\x1b[2J");
        let _ = self.term.set_mode(Mode::ORIGIN, origin);
        true
    }

    /// DECCOLM set or reset (input.c `input_csi_dispatch_sm_private`,
    /// `input_csi_dispatch_rm_private`): tmux has no 132-column mode, so
    /// either homes the cursor (in the scroll region under origin mode,
    /// `screen_write_cursormove` with `origin` 1) and clears the screen in
    /// the current background (`screen_write_clearscreen`, which scrolls it
    /// into history under `scroll-on-clear`, as ED 2 does). Ghostty, without
    /// DEC mode 40, does nothing. Several modes in one sequence run in
    /// tmux's parameter order except this one, which comes last.
    fn deccolm(&mut self) {
        self.term.vt_write(b"\x1b[H");
        if !self.clear_into_history(ScanEvent::EraseScreen) {
            self.term.vt_write(b"\x1b[2J");
        }
        self.drop_marks(0..self.rows());
    }

    /// Start from the ground state, as a fresh pane process does: nothing
    /// half-parsed is carried over (a respawned pane).
    pub fn reset_input(&mut self) {
        self.pending.clear();
        self.scan.last_print = false;
        self.scan.utf8 = Utf8::default();
        self.scan.end_dcs();
        self.scan.state = ScanState::Ground;
        self.scan.palette_osc = None;
        self.scan.title = None;
        self.scan.title_done = None;
    }

    /// Set `input-buffer-size` (`input_set_buffer_size`).
    pub fn set_input_buffer_size(&mut self, size: usize) {
        self.input_buffer_size = size;
        self.scan.osc_cap = dcs::capacity(size);
    }

    /// Tell the pane which strings are colour names (`colour_byname`); the
    /// table lives above this crate. Until set, no name parses.
    pub fn set_colour_names(&mut self, is_name: palette::NameCheck) {
        self.scan.palette.set_name_check(is_name);
    }

    /// The `DCS tmux;` payloads since the last call, in order.
    pub fn take_passthrough(&mut self) -> Vec<Passthrough> {
        std::mem::take(&mut self.passthrough)
    }

    /// Make Ghostty's DECSCA protection bit, which every printed cell copies
    /// from the cursor, equal to tmux's `GRID_ATTR_CHARSET` for the next
    /// print. `CSI 1 " q` first so the most recent protection mode is DEC
    /// and ED/EL/ECH ignore the bit (Ghostty `setProtectedMode`); only
    /// DECSED/DECSEL read it, and tmux implements neither.
    /// Give Ghostty's cursor tmux's charset mark and link after a sequence
    /// that may have parted them.
    fn sync_pen(&mut self) {
        self.sync_charset();
        if let Some(osc) = self.scan.links.resync() {
            self.term.vt_write(&osc);
        }
    }

    fn sync_charset(&mut self) {
        self.term.vt_write(b"\x1b[1\"q");
        if !self.scan.charset.acs() {
            self.term.vt_write(b"\x1b[0\"q");
        }
    }

    /// A character from UTF-8: tmux gives the line-drawing charset only to
    /// an ASCII one (input.c `input_print`; `input_top_bit_set` prints
    /// with the attribute clear), so under it Ghostty prints this one
    /// without the protection bit that marks it ([`Self::sync_charset`]).
    fn feed_plain_char(&mut self, ch: &[u8]) {
        if !self.scan.charset.acs() {
            self.feed_piece(ch);
            return;
        }
        self.term.vt_write(b"\x1b[0\"q");
        self.feed_piece(ch);
        self.term.vt_write(b"\x1b[1\"q");
    }

    fn feed_piece(&mut self, piece: &[u8]) {
        if piece.is_empty() {
            return;
        }
        if !self.history {
            self.term.vt_write(piece);
            // Back on the primary screen: tmux frees `saved_grid`
            // (screen.c `screen_alternate_off`), with history or without.
            if !self.alternate_on() {
                self.saved = None;
                self.alt_entry = None;
                self.clear_primary_history = false;
            }
            return;
        }
        let track = !self.alternate_on();
        if track {
            self.arm_pin();
        }
        self.term.vt_write(piece);
        if !self.alternate_on() {
            self.saved = None;
            let entry = self.alt_entry.take();
            if std::mem::take(&mut self.clear_primary_history) {
                // Back on the primary screen: apply the ED3 that arrived
                // under the alternate one.
                self.term.vt_write(b"\x1b[3J");
                self.hsize = 0;
                self.hscrolled = 0;
            } else if track {
                self.account_scroll();
            } else {
                // Back from the alternate screen: the primary history may
                // have changed under us (a budget lowered while the
                // alternate screen was up pruned it). Ghostty left the
                // primary screen alone meanwhile, as tmux does (patch
                // 0028); `alternate_ended` resizes it.
                if let Some((oldest, retained)) = entry {
                    self.history_from(&oldest, retained);
                }
                self.clamp_history_to_retained();
                self.hscrolled = self.hscrolled.min(self.hsize);
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
        let retained = self.retained_rows();
        let above_pin = self
            .pin
            .as_ref()
            .and_then(|p| p.point(PointSpace::Screen).ok().flatten())
            .and_then(|c| usize::try_from(c.y).ok());
        // A pin that lost its page (more scrolled than retained) leaves the
        // count unknown; assume everything retained is new.
        let scrolled = retained.saturating_sub(above_pin.unwrap_or(0));
        self.hsize = tmux_hsize(self.hsize, self.history_lines, scrolled).min(retained);
        self.hscrolled = (self.hscrolled + scrolled).min(self.hsize);
        self.retain();
        self.scrolled += scrolled as u64;
        if scrolled > 0 {
            // grid_line_set_time: the server loop's `current_time`.
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
            self.times.extend(std::iter::repeat_n(now, scrolled));
            self.trim_times(retained);
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

    /// The screen row history row `id` is while it is still retained, as
    /// `capture.rs` looks rows up: under the alternate screen a row of the
    /// primary screen's history ([`crate::capture::PRIMARY`]).
    pub(crate) fn history_screen_row(&self, id: u64) -> Option<usize> {
        let back = usize::try_from(self.scrolled.checked_sub(id)?).ok()?;
        if back == 0 {
            return None;
        }
        if self.alternate_on() {
            if back > self.hsize {
                return None;
            }
            let y = self.primary_history_rows().checked_sub(back)?;
            return Some(crate::capture::PRIMARY + y);
        }
        let retained = self.retained_rows();
        retained.checked_sub(back)
    }

    /// The primary screen's history rows Ghostty retains, active or not
    /// (patch 0038): while the alternate screen is up the primary is left
    /// as it was (patch 0028), and tmux's history is its newest `hsize`.
    pub(crate) fn primary_history_rows(&self) -> usize {
        self.term.vtmux_screen_scrollback_rows(0).unwrap_or(0)
    }

    /// Copy the visible rows of the primary screen.
    fn freeze_visible(&self) -> Vec<FrozenRow> {
        let cols = usize::from(self.cols());
        let top = self.retained_rows();
        let mut last = None;
        (top..top + usize::from(self.rows()))
            .map(|y| FrozenRow::capture(&self.term, y, cols, &mut last))
            .collect()
    }

    /// The primary screen's visible rows as the alternate screen came up,
    /// while it is up (`capture-pane -a`).
    pub(crate) fn saved_rows(&self) -> Option<Rc<Vec<FrozenRow>>> {
        self.saved.clone()
    }

    /// Apply an OSC 133 payload to the cursor's line, pinned so the data
    /// follows the line. Under the alternate screen the line is on the
    /// alternate grid, which copy mode never reads after the screen goes,
    /// so it is not kept.
    fn mark_cursor_line(&mut self, payload: &[u8]) {
        if self.alternate_on() {
            return;
        }
        let (cx, cy) = tmux_cursor(&self.term);
        let floor = self.hidden_rows();
        let screen_y = self.retained_rows() + usize::from(cy);
        // The cursor's line is one of the last few marked.
        let line = self.marks.iter_mut().rev().take(16).find(|(pin, _)| {
            pin.point(PointSpace::Screen)
                .ok()
                .flatten()
                .is_some_and(|p| usize::try_from(p.y).is_ok_and(|y| y == screen_y))
        });
        if let Some((_, data)) = line {
            data.apply(payload, cx);
            return;
        }
        let mut data = Osc133::default();
        data.apply(payload, cx);
        let point = Point::Active(PointCoordinate {
            x: 0,
            y: u32::from(cy),
        });
        if let Ok(pin) = self.term.track_grid_ref(point) {
            self.marks.push((pin, data));
            self.scan.line_marks = true;
        }
        // Drop the marks of rows gone from history now and then.
        if self.marks.len().is_power_of_two() && self.marks.len() >= 64 {
            self.marks.retain(|(pin, _)| {
                pin.point(PointSpace::Screen)
                    .ok()
                    .flatten()
                    .is_some_and(|p| usize::try_from(p.y).is_ok_and(|y| y >= floor))
            });
        }
    }

    /// The visible rows an ED or ECH clears whole, so loses its OSC 133
    /// data with the rest of the line: `grid_clear` over the full width
    /// is `grid_clear_lines`, which empties the line, flags and all
    /// (grid.c:725-736, 593-598). EL never does: `screen_write_clearline`
    /// puts the line's OSC 133 flags and data back (screen-write.c:1618-
    /// 1650), and EL 0 at the first column and EL 1 at the last are it.
    /// ED 0 at the top left and ED 2 under `scroll-on-clear` go through
    /// [`Self::clear_into_history`] instead.
    fn erased_rows(&self, e: ScanEvent) -> std::ops::Range<u16> {
        if self.marks.is_empty() || self.alternate_on() {
            return 0..0;
        }
        let (cx, cy) = tmux_cursor(&self.term);
        let (sx, sy) = (self.cols(), self.rows());
        match e {
            // screen_write_clearendofscreen: the cursor's line from cx,
            // whole when cx is 0, then every line below.
            ScanEvent::EraseBelow if cx == 0 => cy..sy,
            ScanEvent::EraseBelow => cy.saturating_add(1).min(sy)..sy,
            // screen_write_clearstartofscreen: every line above, then the
            // cursor's to cx, whole from the last column on.
            ScanEvent::EraseAbove if cx >= sx.saturating_sub(1) => 0..cy.saturating_add(1).min(sy),
            ScanEvent::EraseAbove => 0..cy,
            // screen_write_clearscreen without history to scroll into.
            ScanEvent::EraseScreen => 0..sy,
            // screen_write_clearcharacter: nx cells from cx, at most to the
            // end of the line.
            ScanEvent::EraseChars if cx == 0 && self.scan.ech >= u64::from(sx) => {
                cy..cy.saturating_add(1).min(sy)
            }
            _ => 0..0,
        }
    }

    /// Drop the OSC 133 data of the visible rows `rows` (tmux empties those
    /// lines, [`Self::erased_rows`]). The visible rows' marks are among the
    /// newest, so the walk stops after a run of marks above the screen.
    fn drop_marks(&mut self, rows: std::ops::Range<u16>) {
        if rows.is_empty() || self.alternate_on() {
            return;
        }
        let rows = u32::from(rows.start)..u32::from(rows.end);
        let mut above = 0;
        let mut i = self.marks.len();
        while i > 0 && above < 64 {
            i -= 1;
            match self.marks[i].0.point(PointSpace::Active).ok().flatten() {
                Some(p) => {
                    above = 0;
                    if rows.contains(&p.y) {
                        self.marks.remove(i);
                    }
                }
                None => above += 1,
            }
        }
        self.scan.line_marks = !self.marks.is_empty();
    }

    /// Rows Ghostty retains above tmux's history.
    fn hidden_rows(&self) -> usize {
        self.retained_rows().saturating_sub(self.hsize)
    }

    /// The lines OSC 133 marked as a prompt's or output's start, as rows
    /// of tmux's grid (see [`PaneTerm::row`]), oldest first. Under the
    /// alternate screen, those of the primary history as it came up.
    #[must_use]
    pub fn line_marks(&self) -> Vec<(usize, LineMark)> {
        let mut out = Vec::new();
        for (y, data) in self.osc133_lines() {
            if data.flags & 0x8 != 0 {
                out.push((y, LineMark::Prompt));
            }
            if data.flags & 0x40 != 0 {
                out.push((y, LineMark::Output));
            }
        }
        out
    }

    /// Every line OSC 133 touched, with what it recorded, as rows of
    /// tmux's grid, oldest first (see [`PaneTerm::line_marks`]).
    #[must_use]
    pub fn osc133_lines(&self) -> Vec<(usize, Osc133)> {
        if self.alternate_on() {
            return self.frozen_marks.clone();
        }
        let hidden = self.hidden_rows();
        let last = self.hsize + usize::from(self.rows());
        let mut out: Vec<(usize, Osc133)> = self
            .marks
            .iter()
            .filter_map(|(pin, data)| {
                let p = pin.point(PointSpace::Screen).ok().flatten()?;
                let y = usize::try_from(p.y).ok()?.checked_sub(hidden)?;
                (y < last).then_some((y, *data))
            })
            .collect();
        out.sort_by_key(|&(y, _)| y);
        out
    }

    /// The visible grid, brought up to date (only the rows Ghostty marked
    /// dirty are read again).
    ///
    /// # Panics
    /// Only if libghostty fails to allocate, or while a returned view is
    /// still held.
    pub fn screen(&self) -> std::cell::Ref<'_, crate::snapshot::ScreenView> {
        // Read again only once the terminal may have changed: a frame
        // reads each pane's screen more than once (each client's drawing,
        // the cursor, a synchronized update), and even with no dirty row
        // a read costs Ghostty's render state update and a pass over every
        // row. Unwritten, the read would find nothing dirty.
        if self.term.take_touched() || !self.view.borrow().is_live() {
            self.view.borrow_mut().update(self);
        }
        self.view.borrow()
    }

    /// Drop the view [`Self::screen`] keeps (the copied grid and Ghostty's
    /// render state) for a pane no client shows; the next
    /// [`Self::screen`] reads every row again. A 100x50 pane's view is
    /// about 0.7 MB (`docs/test-health/2026-11-09-performance-detail.md`, *Hidden panes*).
    ///
    /// # Panics
    /// Only while a view returned by [`Self::screen`] is still held.
    pub fn release_screen(&self) {
        let mut view = self.view.borrow_mut();
        if view.is_live() {
            *view = crate::snapshot::ScreenView::default();
        }
    }

    /// Whether [`Self::screen`] keeps a view now: it was read since the
    /// last [`Self::release_screen`].
    ///
    /// # Panics
    /// Only while a view returned by [`Self::screen`] is still held
    /// mutably, which never happens outside this crate.
    #[must_use]
    pub fn holds_screen(&self) -> bool {
        self.view.borrow().is_live()
    }

    /// Resize the pane. Primary-screen content reflows (Ghostty).
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        // window_pane_resize (window.c:1712) ends synchronized output.
        if (cols.max(1), rows.max(1)) != (self.cols(), self.rows()) {
            self.stop_sync();
        }
        self.resize_grid(cols, rows);
    }

    /// `screen_alternate_off` after the alternate screen ended at `size`
    /// (screen.c): Ghostty left the primary screen at the size it had
    /// when the alternate one came up, gave the terminal that size and put
    /// the cursor where tmux resizes it with (patch 0028); tmux then
    /// resizes it to the pane's size with history on and reflow
    /// (`screen_resize`, so the rows under the cursor go before any are
    /// pushed into history, and `hscrolled` moves as in any resize), and
    /// puts a cursor past the last column on it.
    fn alternate_ended(&mut self, size: (u16, u16)) {
        if (self.cols(), self.rows()) != size {
            self.resize_grid(size.0, size.1);
        }
        if self.term.is_cursor_pending_wrap().unwrap_or(false) {
            let (x, y) = self.cursor();
            self.write_screen_moves(&format!("\x1b[{};{}H", y + 1, x + 1));
        }
    }

    fn resize_grid(&mut self, cols: u16, rows: u16) {
        let before = self.retained_rows();
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
        // tmux's hscrolled goes through the height change and the reflow
        // (Ghostty patch 0025); the rows above tmux's oldest are not tmux's.
        let carry = on_primary && self.history;
        if carry {
            let origin = self.retained_rows().saturating_sub(self.hsize);
            let _ = self.term.vtmux_set_hscrolled(self.hscrolled, origin);
        }
        if on_primary {
            self.eat_below_cursor(rows.max(1));
        }
        // A cursor at the end of its line stays at the end of the
        // reflowed line, past the last column when that ends a row
        // (grid_unwrap_position; Ghostty patch 0023 leaves a pending wrap
        // there instead of a row of its own).
        if cols.max(1) != self.cols() && on_primary {
            // The height first, as screen_resize_y does before the reflow.
            if rows.max(1) != self.rows() {
                let cols = self.cols();
                self.term
                    .resize(cols, rows.max(1), self.cell_px.0, self.cell_px.1)
                    .expect("libghostty resize");
            }
            let blank = self.trailing_blank_rows();
            self.term
                .resize(cols.max(1), rows.max(1), self.cell_px.0, self.cell_px.1)
                .expect("libghostty resize");
            self.take_hscrolled(carry);
            if blank < rows.max(1) {
                self.reflow_blank_rows(blank, oldest.as_ref());
            }
        } else {
            self.term
                .resize(cols.max(1), rows.max(1), self.cell_px.0, self.cell_px.1)
                .expect("libghostty resize");
            self.take_hscrolled(carry);
        }
        // At the primary screen's width: the alternate screen leaves it at
        // its own (patch 0028).
        if on_primary {
            self.page_rows = page_rows(cols.max(1));
        }
        if on_primary {
            // Reflow moves rows in and out of history: tmux's history
            // starts at the row that was its oldest. (While the alternate
            // screen is up the primary is not resized, patch 0028.)
            let after = self.retained_rows();
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
                self.trim_times(after);
                // The times end at the newest row: rows pulled back take
                // their ids with them, also those without a time.
                self.scrolled = scrolled;
                self.times_base = self.scrolled.saturating_sub(self.times.len() as u64);
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
            // grid_reflow: at most the new history.
            self.hscrolled = self.hscrolled.min(self.hsize);
        }
        self.generation += 1;
    }

    /// Read tmux's `hscrolled` back after the resizes that carried it,
    /// before any of vtmux's own fix-ups resize again.
    fn take_hscrolled(&mut self, carry: bool) {
        if carry && let Ok(h) = self.term.vtmux_take_hscrolled() {
            self.hscrolled = h;
        }
    }

    /// `screen_resize_y` (screen.c:430-445) with `eat_empty`: a shorter
    /// screen first deletes the lines below the cursor, whatever they hold,
    /// as many as it gets shorter by; only the rest of the shrink pushes
    /// lines into history. Ghostty trims only blank rows, so the lines are
    /// erased first and the resize trims them (the cursor's row is pinned,
    /// so nothing above it goes). Lines with no text are left to Ghostty's
    /// trim: erasing them would write every cell and fault in the page
    /// memory of a pane that never drew there (an idle split pane). A
    /// cursor waiting to wrap keeps its lines: moving it back would lose
    /// the wrap.
    fn eat_below_cursor(&mut self, rows: u16) {
        let old = self.rows();
        if rows >= old || self.term.is_cursor_pending_wrap().unwrap_or(false) {
            return;
        }
        let (cx, cy) = self.cursor();
        let eat = (old - 1 - cy.min(old - 1)).min(old - rows);
        if eat == 0 || self.bottom_rows_blank(eat) {
            return;
        }
        self.write_screen_moves(&format!(
            "\x1b[{};1H\x1b[J\x1b[{};{}H",
            old - eat + 1,
            cy + 1,
            cx + 1
        ));
    }

    /// Whether the bottom `n` rows of the screen hold no text (Ghostty's
    /// `trailingBlankLines` test). Reads those rows' cells only: the
    /// pane's [`Self::screen`] view would read, and keep, the whole grid.
    fn bottom_rows_blank(&self, n: u16) -> bool {
        let rows = self.rows();
        let mut cells = Vec::new();
        (rows.saturating_sub(n)..rows).all(|y| {
            let at = Point::Active(PointCoordinate {
                x: 0,
                y: u32::from(y),
            });
            self.term.grid_ref(at).is_ok_and(|row| {
                row.row_cells(&mut cells).is_ok()
                    && cells.iter().all(|c| !c.has_text().unwrap_or(true))
            })
        })
    }

    /// Write `seq` (cursor moves in screen coordinates) with origin mode
    /// off, so CUP is not relative to the scroll region, and back on after.
    ///
    /// A resize does not touch tmux's input parser (`screen_resize` leaves
    /// `wp->ictx` alone), so a sequence the program left half-written is
    /// taken out of Ghostty's parser first and given back after: CAN ends
    /// it without dispatching it (an OSC is dropped, `osc.Parser.end`), and
    /// the program's next bytes finish it.
    fn write_screen_moves(&mut self, seq: &str) {
        let partial = self.ghostty_partial();
        if !partial.is_empty() {
            self.term.vt_write(b"\x18");
        }
        let origin = self.term.mode(Mode::ORIGIN).unwrap_or(false);
        if origin {
            let _ = self.term.set_mode(Mode::ORIGIN, false);
        }
        self.term.vt_write(seq.as_bytes());
        if origin {
            let _ = self.term.set_mode(Mode::ORIGIN, true);
        }
        self.term.vt_write(&partial);
    }

    /// The bytes of the sequence Ghostty's parser is in the middle of: the
    /// input since the last ESC of [`PaneTerm::pending_input`] (every ESC
    /// starts a sequence afresh in both parsers), less what it must not
    /// see twice or at all. Nothing at the ground state, nor in a DCS or a
    /// rename, which the scanner reads alone (Ghostty had CAN).
    ///
    /// - In a CSI or an escape, C0 controls ran when they arrived, and
    ///   the state stays (tmux's `input_c0_dispatch` in
    ///   `input_state_csi_enter_table` and the others, Ghostty's
    ///   `execute`): run again, a newline would move the cursor twice.
    ///   CAN and SUB never reach the tail: they end the sequence.
    /// - In a CSI, 0x7f-0xff are the scanner's alone ([`ScanEvent::Skip`]).
    /// - A string longer than the pending input keeps has lost its middle
    ///   (the input stops growing at [`Self::pending_max`], past
    ///   `input-buffer-size`, and tmux discards a string past that limit:
    ///   `input_input` sets `INPUT_DISCARD`, `input_exit_osc` and the
    ///   other exits then do nothing). Ghostty gets an OSC it rejects at
    ///   its first byte instead, which swallows the rest of the string
    ///   through its BEL or ST and dispatches nothing (`osc.Parser`'s
    ///   `invalid` state).
    fn ghostty_partial(&self) -> Vec<u8> {
        let state = self.scan.state;
        match state {
            ScanState::Ground | ScanState::Dcs | ScanState::Rename => return Vec::new(),
            ScanState::Str if self.scan.osc_over => return OSC_REJECTED.to_vec(),
            ScanState::Str | ScanState::Apc if self.pending.len() > self.pending_max() => {
                return OSC_REJECTED.to_vec();
            }
            _ => {}
        }
        let from = self.pending.iter().rposition(|&b| b == 0x1b).unwrap_or(0);
        let keep = |b: u8| match state {
            ScanState::Str => true,
            // What the scanner skipped ([`AltModeScanner::push_state`]).
            ScanState::Apc => !(0x80..=0x9f).contains(&b),
            ScanState::Csi => b == 0x1b || (0x20..0x7f).contains(&b),
            _ => b == 0x1b || b >= 0x20,
        };
        self.pending[from..]
            .iter()
            .copied()
            .filter(|&b| keep(b))
            .collect()
    }

    /// How many rows at the bottom of the screen hold no text, below the
    /// cursor's (its line is content, however empty: both reflows keep it).
    fn trailing_blank_rows(&self) -> u16 {
        let below = self.rows().saturating_sub(1 + self.cursor().1);
        let view = self.screen();
        let snap = view.snapshot();
        let width = usize::from(snap.cols).max(1);
        let blank = snap
            .cells
            .rchunks(width)
            .take_while(|row| row.iter().all(|c| c.text.is_empty()))
            .count();
        u16::try_from(blank).unwrap_or(u16::MAX).min(below)
    }

    /// After Ghostty reflowed the screen to a new width: give it the blank
    /// rows at the bottom tmux's reflow leaves. `grid_reflow`
    /// (grid.c:1528-1608) keeps every line, so the `blank` rows that ended
    /// the screen before still end it, and a grid shorter than the screen
    /// gets blank rows added at the bottom (`grid_reflow_add`); the rest is
    /// history. Ghostty's reflow instead drops trailing blank rows when
    /// narrowing and keeps the cursor's screen row when widening. Too few
    /// blank rows: more are added, scrolling the top rows into history.
    /// Too many: they go and as many history rows come back. The cursor
    /// stays on its line. `oldest` is the pin on tmux's oldest history row.
    fn reflow_blank_rows(&mut self, blank: u16, oldest: Option<&TrackedGridRef>) {
        let rows = self.rows();
        let have = self.trailing_blank_rows();
        let hidden = oldest
            .and_then(|pin| pin.point(PointSpace::Screen).ok().flatten())
            .and_then(|p| usize::try_from(p.y).ok())
            .unwrap_or(0);
        let history = self.retained_rows().saturating_sub(hidden);
        let content = history + usize::from(rows - have);
        let short = u16::try_from(usize::from(rows).saturating_sub(content)).unwrap_or(rows);
        let want = blank.max(short).min(rows);
        let cy = self.cursor().1;
        let resize = |t: &mut Self, r: u16| {
            let cols = t.cols();
            t.term
                .resize(cols, r, t.cell_px.0, t.cell_px.1)
                .expect("libghostty resize");
        };
        // The cursor changes rows only by VPA, which keeps its column and
        // a pending wrap (patch 0014): a cursor past the end of a row
        // stays there.
        if want > have {
            // Blank rows at the bottom (Ghostty grows the screen with blank
            // rows while the cursor is off its last row, else it pulls
            // history back), then a shorter screen with the bottom row
            // pinned, so Ghostty trims none of them: the top rows go into
            // history and the cursor goes up with its line.
            let n = want - have;
            let on_last = cy == rows - 1;
            if on_last {
                self.write_screen_moves("\x1b[1d");
            }
            resize(self, rows + n);
            let bottom = PointCoordinate {
                x: 0,
                y: u32::from(rows + n - 1),
            };
            let pin = self.term.track_grid_ref(Point::Active(bottom)).ok();
            if on_last {
                self.write_screen_moves(&format!("\x1b[{}d", rows + n));
            }
            resize(self, rows);
            drop(pin);
            // screen_resize_cursor: a cursor line now in history leaves the
            // cursor at the top left.
            if cy < n {
                self.write_screen_moves("\x1b[H");
            } else if on_last {
                self.write_screen_moves(&format!("\x1b[{}d", cy - n + 1));
            }
        } else if want < have {
            // Ghostty trims the blank rows when the screen gets shorter,
            // and pulls history back when it grows with the cursor on the
            // last row. The cursor's own line stays.
            let n = (have - want).min(rows - 1 - cy);
            if n == 0 {
                return;
            }
            resize(self, rows - n);
            self.write_screen_moves(&format!("\x1b[{}d", rows - n));
            resize(self, rows);
            self.write_screen_moves(&format!("\x1b[{}d", cy + n + 1));
        }
    }

    /// `resize-pane -T` (cmd-resize-pane.c:71-81, `grid_remove_history`):
    /// the rows below the cursor, as many as there is history for, go from
    /// the bottom and that many history rows come back at the top; the
    /// cursor moves down with its row. The terminal keeps its size.
    ///
    /// Ghostty does it as a resize: the bottom rows are blanked, a smaller
    /// screen trims them, and growing it again with the cursor on the last
    /// row pulls the newest history rows back. The cursor is then put back
    /// on its row and column.
    pub fn trim_below_cursor(&mut self) {
        let (cols, rows) = (self.cols(), self.rows());
        let (cx, cy) = self.cursor();
        let below = usize::from(rows - 1 - cy.min(rows - 1));
        let adjust = below.min(self.history_size());
        let Ok(n) = u16::try_from(adjust) else {
            return;
        };
        if n == 0 || self.alternate_on() {
            return;
        }
        let keep = rows - n;
        self.term
            .vt_write(format!("\x1b[{};1H\x1b[J\x1b[{keep};1H", keep + 1).as_bytes());
        self.resize(cols, keep);
        self.term.vt_write(format!("\x1b[{keep};1H").as_bytes());
        self.resize(cols, rows);
        self.term
            .vt_write(format!("\x1b[{};{}H", cy + n + 1, cx.min(cols - 1) + 1).as_bytes());
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
        if let Some(t) = self.titles.server.as_ref() {
            return t.clone();
        }
        // Only a state imported from a build that kept the program's title
        // in Ghostty: OSC 0 and 2 never reach it now.
        self.term
            .title()
            .map(|t| clean_name(t, true))
            .unwrap_or_default()
    }

    /// What the program's output did since the last call that the server
    /// reports (title changes, BELs, OSC 133 prompts and commands), oldest
    /// first, as tmux fires them while it parses (`input_parse_buffer`).
    /// Nothing is dropped, so a caller takes them after every
    /// [`Self::feed`].
    pub fn take_events(&mut self) -> Vec<TermEvent> {
        std::mem::take(&mut self.events.borrow_mut().list)
    }

    /// A cell's size in pixels, its window's (`w->xpixel`, `w->ypixel`),
    /// from the next reply or resize on: XTWINOPS `CSI 14/15/16 t`
    /// (input.c:2163-2180) and mode 2048 report with it, and Ghostty's
    /// own size in pixels follows at the next resize. A zero stands for
    /// tmux's default on its axis (16x32).
    pub fn set_cell_pixels(&mut self, (x, y): (u32, u32)) {
        let before = self.cell_px;
        self.cell_px = (
            if x == 0 { DEFAULT_CELL_PX.0 } else { x },
            if y == 0 { DEFAULT_CELL_PX.1 } else { y },
        );
        // D33: a granted screen sizes its placements by it now.
        if self.cell_px != before && self.graphics_granted != [false; 2] {
            self.push_cell_pixels();
        }
    }

    /// Give Ghostty the cell size in pixels without a resize: a resize to
    /// the size it has, which also ends Ghostty's synchronized output
    /// mode (`Terminal.resize`), so that is set back.
    fn push_cell_pixels(&mut self) {
        let sync = self.term.mode(Mode::SYNC_OUTPUT).unwrap_or(false);
        let (cols, rows) = (self.cols(), self.rows());
        let _ = self.term.resize(cols, rows, self.cell_px.0, self.cell_px.1);
        if sync {
            let _ = self.term.set_mode(Mode::SYNC_OUTPUT, true);
        }
    }

    /// The cell size in pixels replies use ([`Self::set_cell_pixels`]).
    #[must_use]
    pub fn cell_pixels(&self) -> (u32, u32) {
        self.cell_px
    }

    /// `vtmux-inband-resize` (D31): whether a `?2048h` from now on grants
    /// mode 2048, and what DECRQM says of it. A grant already held stays
    /// either way (D28 rule 5).
    pub fn set_inband_resize(&mut self, offered: bool) {
        let mut m = self.inband.get();
        m.offered = offered;
        self.inband.set(m);
    }

    /// The program holds mode 2048 (D31): every size change of the pane is
    /// reported to it ([`Self::inband_resize_report`]).
    #[must_use]
    pub fn inband_resize_granted(&self) -> bool {
        self.inband.get().granted
    }

    /// The mode 2048 report a granted program gets once its pty is set to
    /// `cols` x `rows`, or `None` when it is not granted.
    #[must_use]
    pub fn inband_resize_report(&self, cols: u16, rows: u16) -> Option<Vec<u8>> {
        self.inband_resize_granted()
            .then(|| inband_report(cols, rows, self.cell_px))
    }

    /// The program's grant of mode 2048 ends: a respawn (the program is
    /// gone) or RIS.
    pub fn end_inband_resize(&mut self) {
        self.set_inband_granted(false);
    }

    /// The server's `vtmux-graphics` (D33) the next input is read under:
    /// whether a graphics command (`APC G`) on a screen not granted grants
    /// it. A granted screen keeps its grant either way (D28 rule 5).
    pub fn set_graphics(&mut self, on: bool) {
        self.graphics_option = on;
    }

    /// D33: each screen's grant, primary then alternate.
    #[must_use]
    pub fn graphics_grants(&self) -> [bool; 2] {
        self.graphics_granted
    }

    /// D33: the screens granted since the last call, primary then
    /// alternate, whose owner the server notes.
    pub fn take_graphics_grants(&mut self) -> [bool; 2] {
        std::mem::take(&mut self.graphics_new)
    }

    /// D33: a screen's grant ends or, from a snapshot, is given back. Its
    /// images stay: a program that sent them and left keeps its picture.
    pub fn set_graphics_grant(&mut self, alternate: bool, granted: bool) {
        self.graphics_granted[usize::from(alternate)] = granted;
        if granted {
            self.enable_graphics(usize::from(alternate));
        }
    }

    /// D33: RIS or a respawn: both grants end and every image goes.
    pub fn reset_graphics(&mut self) {
        self.graphics_granted = [false; 2];
        self.graphics_new = [false; 2];
        for screen in 0..2 {
            let _ = self.term.vtmux_drop_kitty_loading(screen);
            if self.graphics_storage(screen == 1) != (0, 0) {
                let _ = self.term.vtmux_set_kitty_limit(screen, 0);
            }
        }
    }

    /// D33: screen `alternate`'s image storage: the bytes its images hold
    /// and its limit (0: no images can load).
    #[must_use]
    pub fn graphics_storage(&self, alternate: bool) -> (u64, u64) {
        self.term
            .vtmux_kitty_storage(u8::from(alternate))
            .unwrap_or((0, 0))
    }

    /// D33: the bytes screen `alternate`'s pending load holds, a chunked
    /// transmission not finished yet. It is never more than the screen's
    /// limit: a chunk past it fails (`ENOMEM`) and adds nothing (patch
    /// 0043).
    #[must_use]
    pub fn graphics_pending(&self, alternate: bool) -> u64 {
        self.term
            .vtmux_kitty_loading(u8::from(alternate))
            .unwrap_or(0)
    }

    /// D33: abandon screen `alternate`'s pending load; its program's next
    /// chunk starts a new transmission.
    pub fn drop_graphics_pending(&mut self, alternate: bool) {
        let _ = self.term.vtmux_drop_kitty_loading(u8::from(alternate));
    }

    /// D33: set screen `alternate`'s storage limit; lowering it evicts as
    /// Ghostty does (unused images first, then the oldest).
    pub fn set_graphics_limit(&mut self, alternate: bool, limit: u64) {
        let _ = self.term.vtmux_set_kitty_limit(u8::from(alternate), limit);
        self.bound_graphics_command();
    }

    /// D33: the storage limit a screen granted from now on starts with.
    pub fn set_graphics_share(&mut self, limit: u64) {
        self.graphics_share = limit;
        self.bound_graphics_command();
    }

    /// D33: one graphics command carries at most what the largest limit
    /// a screen has, or the share a screen granted now starts with, could
    /// store, in base64 with room for its keys; never more than Ghostty's
    /// own 65 MiB (`apc.Protocol.defaultMaxBytes`). Ghostty drops a longer
    /// one unanswered, as it drops one past 65 MiB.
    fn bound_graphics_command(&mut self) {
        const GHOSTTY_MAX: u64 = 65 * 1024 * 1024;
        let limit = self
            .graphics_share
            .max(self.graphics_storage(false).1)
            .max(self.graphics_storage(true).1);
        let max = (limit.div_ceil(3) * 4 + 4096).min(GHOSTTY_MAX);
        let _ = self.term.set_apc_max_bytes_kitty(usize::try_from(max).ok());
    }

    /// A graphics command on the active screen ends with `end`: the
    /// screen is granted if it was not, and Ghostty runs the command and
    /// answers it, honouring its `q`.
    fn graphics_command(&mut self, end: &[u8]) {
        let screen = usize::from(self.alternate_on());
        if !self.graphics_granted[screen] {
            self.graphics_granted[screen] = true;
            self.graphics_new[screen] = true;
            self.enable_graphics(screen);
        }
        self.graphics_reply.set(true);
        self.feed_piece(end);
        self.graphics_reply.set(false);
    }

    /// A granted screen's storage: a limit if it has none and the PNG
    /// decoder. Media stay direct only (see [`Self::new`]).
    fn enable_graphics(&mut self, screen: usize) {
        crate::graphics::install_png_decoder();
        self.push_cell_pixels();
        let alternate = screen == 1;
        if self.graphics_storage(alternate).1 == 0 {
            let share = self.graphics_share;
            self.set_graphics_limit(alternate, share);
        }
    }

    pub(crate) fn set_inband_granted(&mut self, granted: bool) {
        let mut m = self.inband.get();
        m.granted = granted;
        self.inband.set(m);
    }

    /// `?2048h` or `?2048l` from the program. A set while offered grants
    /// the mode; a set while granted reports the size at once, as Ghostty
    /// does on every set (`setMode`, stream_terminal.zig:2051-2054,
    /// `reportMode2048` :1881-1890), after the replies to what came before.
    fn inband_mode(&mut self, set: bool) {
        let mut m = self.inband.get();
        if !set {
            m.granted = false;
        } else if m.offered {
            m.granted = true;
        }
        self.inband.set(m);
        if set && m.granted {
            let report = inband_report(self.cols(), self.rows(), self.cell_px);
            push_bytes(&mut self.scan.answers, &report);
        }
    }

    /// `CSI 22 t` (`push`) or `CSI 23 t` (input.c:2187-2212). A push keeps
    /// the title on a stack of at most ten (`screen_push_title`); a pop
    /// restores the newest, if any (`screen_pop_title`), and reports the
    /// title either way.
    /// tmux's reply to XTWINOPS report `n` (input.c:2160-2186), with the
    /// cell size in pixels [`Self::set_cell_pixels`] gave.
    fn size_report(&self, n: u64) -> Vec<u8> {
        let (x, y) = (u32::from(self.cols()), u32::from(self.rows()));
        let (xp, yp) = self.cell_px;
        let (width, height) = (x.wrapping_mul(xp), y.wrapping_mul(yp));
        match n {
            14 => format!("\x1b[4;{height};{width}t"),
            15 => format!("\x1b[5;{height};{width}t"),
            16 => format!("\x1b[6;{yp};{xp}t"),
            18 => format!("\x1b[8;{y};{x}t"),
            19 => format!("\x1b[9;{y};{x}t"),
            _ => String::new(),
        }
        .into_bytes()
    }

    fn title_stack_op(&mut self, push: bool) {
        let title = self.title();
        let titles = &mut self.titles;
        if push {
            titles.stack.truncate(9);
            titles.stack.insert(0, title);
            return;
        }
        if !titles.stack.is_empty() {
            let title = titles.stack.remove(0);
            titles.server = Some(title);
        }
        let title = self.title();
        self.events.borrow_mut().push(TermEvent::Title(title));
    }

    /// Set the title from the server side (`select-pane -T`,
    /// `split-window -T`): `screen_set_title(s, title, 0)`, a trusted
    /// clean, so `#(` stays. False, and the title unchanged, for a title
    /// `clean_name` refuses (`utf8_isvalid`: an ASCII control character
    /// or DEL).
    pub fn set_title(&mut self, title: &str) -> bool {
        if !valid_name(title) {
            return false;
        }
        self.titles.server = Some(clean_name(title, false));
        true
    }

    /// The pane's `allow-set-title` (input.c:2726, 2804): whether the
    /// program may set the title with OSC 0, OSC 2 or an APC string.
    pub fn set_allow_set_title(&mut self, allow: bool) {
        self.allow_set_title = allow;
    }

    /// A title from the program, an OSC 0 or 2's text or an APC string
    /// (`input_exit_osc`, `input_exit_apc`): with `allow-set-title` on and
    /// a text `utf8_isvalid` takes, the title, untrusted (`#(` becomes
    /// `_(`), and a `pane-title-changed` with the text as it came.
    fn program_title(&mut self, text: &[u8]) {
        if !self.allow_set_title {
            return;
        }
        let Ok(text) = std::str::from_utf8(text) else {
            return;
        };
        if !valid_name(text) {
            return;
        }
        self.titles.server = Some(clean_name(text, true));
        self.events
            .borrow_mut()
            .push(TermEvent::Title(text.to_owned()));
    }

    /// The progress bar OSC 9;4 set (`s->progress_bar`).
    #[must_use]
    pub fn progress_bar(&self) -> ProgressBar {
        self.scan.progress
    }

    /// `screen_reinit` (screen.c:139, a respawn): the bar hidden at 0.
    /// RIS keeps it, as tmux's does.
    pub fn reset_progress_bar(&mut self) {
        self.scan.progress = ProgressBar::default();
    }

    /// Give back a title [`Self::title`] read (a snapshot's): it is
    /// already cleaned, so it is kept as it is.
    pub fn restore_title(&mut self, title: &str) {
        self.titles.server = Some(title.to_owned());
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

    /// tmux's `MODE_CRLF`, which only the server sets (an empty pane,
    /// spawn.c:462): a line feed also returns the carriage (input.c
    /// `input_c0_dispatch`). Ghostty's LNM, which a program's SM 20 does
    /// not reach.
    pub fn set_crlf(&mut self, on: bool) {
        let _ = self.term.set_mode(Mode::LINEFEED, on);
    }

    /// The screen modes tmux's `s->mode` holds that its formats read
    /// (`#{wrap_flag}`, `#{pane_private_modes}`, ...; format.c:1482-1491,
    /// 1837-1992, 2131-2206, 3441-3450).
    #[must_use]
    pub fn screen_modes(&self) -> ScreenModes {
        let on = |m: Mode| self.term.mode(m).unwrap_or(false);
        let style = self.cursor_style();
        ScreenModes {
            app_cursor: on(Mode::DECCKM),
            app_keypad: on(Mode::KEYPAD_KEYS),
            origin: on(Mode::ORIGIN),
            wrap: on(Mode::WRAPAROUND),
            insert: on(Mode::INSERT),
            cursor_visible: on(Mode::CURSOR_VISIBLE),
            focus: on(Mode::FOCUS_EVENT),
            bracketed_paste: on(Mode::BRACKETED_PASTE),
            sync: self.sync_held().is_some(),
            theme_updates: on(Mode::COLOR_SCHEME_REPORT),
            cursor_shape: style.shape,
            cursor_blinking: style.blinking,
            cursor_blinking_set: style.blinking_set,
            cursor_very_visible: style.very_visible,
        }
    }

    /// The cursor fields of [`Self::screen_modes`], without reading the
    /// terminal's modes: what a frame's cursor style is written from.
    #[must_use]
    pub fn cursor_style(&self) -> CursorStyle {
        let c = self.cursor.get();
        CursorStyle {
            shape: match c.shape {
                1 => CursorShape::Block,
                2 => CursorShape::Underline,
                3 => CursorShape::Bar,
                _ => CursorShape::Default,
            },
            blinking: c.blinking,
            blinking_set: c.blinking_set,
            very_visible: c.very_visible,
        }
    }

    /// The scroll region's top and bottom rows, inclusive (tmux's
    /// `s->rupper`, `s->rlower`).
    #[must_use]
    pub fn scroll_region(&self) -> (u16, u16) {
        match self.term.vtmux_state_export() {
            Ok(st) => (st.raw.scroll_top, st.raw.scroll_bottom),
            Err(_) => (0, self.rows().saturating_sub(1)),
        }
    }

    /// The columns with a tab stop, in order (tmux's `s->tabs`).
    #[must_use]
    pub fn tab_stops(&self) -> Vec<u16> {
        let Ok(st) = self.term.vtmux_state_export() else {
            return Vec::new();
        };
        (0..st.raw.cols)
            .filter(|&c| {
                st.raw
                    .tabstops
                    .get(usize::from(c / 8))
                    .is_some_and(|b| b & (1 << (c % 8)) != 0)
            })
            .collect()
    }

    /// The default tab stops (every 8 columns: DECST8C), written past the
    /// scanner, which drops that sequence from a program (tmux has no such
    /// sequence).
    pub fn reset_tab_stops(&mut self) {
        self.feed_piece(b"\x1b[?5W");
    }

    /// `screen_reinit`'s `s->mode = MODE_CURSOR|MODE_WRAP` for the bits
    /// vtmux holds itself: the cursor's blinking and very visible bits go,
    /// its shape stays (screen.c:117). The caller feeds the rest.
    pub fn reinit_cursor_modes(&mut self) {
        let mut c = self.cursor.get();
        c.reset();
        self.cursor.set(c);
    }

    /// The cursor colour OSC 12 set (`s->ccolour`); `None` is unset.
    #[must_use]
    pub fn cursor_colour(&self) -> Option<palette::Rgb> {
        self.scan.palette.state().cursor
    }

    /// The mouse modes the program asked for, as tmux holds them: what
    /// `#{mouse_any_flag}` and the other `mouse_*_flag` formats read, what
    /// DECRQM `?1000`-`?1006` report, and what decides forwarding of
    /// mouse events to the pane and the outer terminal's mouse mode.
    /// Not Ghostty's bits (see [`MouseModes`]). Alternate screen switches
    /// leave it alone, RIS clears it.
    #[must_use]
    pub fn mouse_modes(&self) -> MouseModes {
        self.mouse.get()
    }

    /// True while the alternate screen is active (`#{alternate_on}`).
    #[must_use]
    pub fn alternate_on(&self) -> bool {
        matches!(self.term.active_screen(), Ok(Screen::Alternate))
    }

    /// Forget the cursor `?1049h` saved, as `screen_reinit` does
    /// (screen.c:124-125, for `respawn-pane`).
    pub fn forget_alternate_saved(&mut self) {
        let _ = self.term.vtmux_set_alternate_saved(None);
    }

    /// The cursor `?1049h` saved (`#{alternate_saved_x}`,
    /// `#{alternate_saved_y}`: tmux's `saved_cx` and `saved_cy`, the column
    /// count while a wrap was pending), kept after the alternate screen
    /// ends; `None` before any (tmux's `UINT_MAX`, screen.c `screen_reinit`).
    #[must_use]
    pub fn alternate_saved(&self) -> Option<(u16, u16)> {
        self.term
            .vtmux_alternate_saved()
            .ok()
            .flatten()
            .map(|(x, y, _)| (x, y))
    }

    /// `#{history_size}`: tmux's `hsize`, which saw-tooths between 90% of the
    /// limit and the limit (see [`PaneTerm::feed`]). Unchanged while the
    /// alternate screen is on: tmux keeps the primary history.
    #[must_use]
    pub fn history_size(&self) -> usize {
        self.hsize
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
    /// Ghostty applies the budget to the primary screen only, also under
    /// the alternate screen, whose history is the primary's: the pruned
    /// rows leave it now (patch 0038 counts the primary's rows), and the
    /// ids and times follow when the primary screen is back.
    pub fn set_history_budget(&mut self, bytes: usize) {
        let _ = self.term.set_scrollback_max_bytes(Some(bytes.max(1)));
        if self.alternate_on() {
            let retained = self.primary_history_rows();
            if self.hsize > retained {
                self.drop_oldest_history(self.hsize - retained);
            }
        } else {
            self.clamp_history_to_retained();
        }
    }

    /// The `n` oldest rows of tmux's history went from the primary screen
    /// under the alternate one (a budget pruned them).
    fn drop_oldest_history(&mut self, n: usize) {
        self.drop_oldest_frozen_marks(n);
        self.hsize -= n;
        self.hscrolled = self.hscrolled.min(self.hsize);
    }

    /// The `n` oldest history rows went under the alternate screen: the
    /// marks kept for them go and the others move up.
    fn drop_oldest_frozen_marks(&mut self, n: usize) {
        self.frozen_marks = std::mem::take(&mut self.frozen_marks)
            .into_iter()
            .filter_map(|(y, m)| Some((y.checked_sub(n)?, m)))
            .collect();
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
            self.drop_oldest_frozen_marks(self.hsize);
            self.hsize = 0;
            self.hscrolled = 0;
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
        self.hscrolled = 0;
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
    /// retained beneath `history_size` until its line limit catches up.
    /// Under the alternate screen tmux's history is the primary's newest
    /// `hsize` rows, so it loses its oldest.
    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_lines = limit;
        if !self.history {
            return;
        }
        let hsize = self.history_size();
        if hsize > 0 && hsize >= limit {
            let ny = (hsize - limit).max(1).min(hsize);
            self.hsize -= ny;
            self.hscrolled = self.hscrolled.min(self.hsize);
            self.trim_times(self.hsize);
            if self.alternate_on() {
                self.drop_oldest_frozen_marks(ny);
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
        let retained = self.retained_rows();
        self.hsize = self.hsize.min(retained);
        self.hscrolled = self.hscrolled.min(self.hsize);
        self.trim_times(retained);
    }

    /// Keep the times of the newest `rows` history rows only.
    fn trim_times(&mut self, rows: usize) {
        let gone = self.times.len().saturating_sub(rows);
        self.times.drain(..gone);
        self.times_base += gone as u64;
    }

    /// `screen_write_start_sync` (screen-write.c:1033-1046), on `?2026h`
    /// (input.c:2091): keep what the clients show now and (re)arm the
    /// 1 s timer. A second `?2026h` keeps the first hold.
    fn start_sync(&mut self) {
        let now = std::time::Instant::now();
        if let Some((_, since)) = self.sync.hold.as_mut() {
            *since = now;
            return;
        }
        let view = self.screen();
        let hold = HeldScreen {
            screen: view.shared(),
            stamps: view.row_stamps().into(),
        };
        drop(view);
        self.sync.hold = Some((hold, now));
    }

    /// The screen the clients keep showing while the program has
    /// synchronized output on (tmux's `MODE_SYNC`, screen-write.c:211-240:
    /// nothing of the pane is drawn until it ends); `None` when it is off.
    #[must_use]
    pub fn sync_held(&self) -> Option<&HeldScreen> {
        self.sync.hold.as_ref().map(|(s, _)| s)
    }

    /// When the sync timer ends the hold (`screen_write_sync_callback`,
    /// one second after the last `?2026h`), if it is on.
    #[must_use]
    pub fn sync_deadline(&self) -> Option<std::time::Instant> {
        self.sync
            .hold
            .as_ref()
            .map(|(_, since)| *since + SYNC_TIMEOUT)
    }

    /// `screen_write_stop_sync` (screen-write.c:1049-1062): synchronized
    /// output ends without the program asking (the timer, a resize, a full
    /// redraw); DECRQM reports it off from now on. Whether it was on.
    pub fn stop_sync(&mut self) -> bool {
        if self.sync.hold.is_none() {
            return false;
        }
        self.end_sync_hold();
        let _ = self.term.set_mode(Mode::SYNC_OUTPUT, false);
        true
    }

    fn end_sync_hold(&mut self) {
        if let Some((held, _)) = self.sync.hold.take() {
            self.sync.ended = Some(held.stamps);
        }
    }

    /// `screen_write_flush_dirty` (screen-write.c:1267-1290): when
    /// synchronized output ends, the lines written under it are drawn
    /// whole, not as a diff. Per visible row, whether it changed since the
    /// hold began: its stamp moved (every row after a size change, which
    /// stamps them all afresh); `None` when no hold ended since the last
    /// call. Stamps, not cells: keeping the held cells would make the view
    /// copy its grid on every synchronized update.
    pub fn take_sync_flush(&mut self) -> Option<Vec<bool>> {
        let held = self.sync.ended.take()?;
        let view = self.screen();
        let now = view.row_stamps();
        if held.len() != now.len() {
            return Some(vec![true; now.len()]);
        }
        Some(now.iter().zip(held.iter()).map(|(a, b)| a != b).collect())
    }

    /// Whether the program asked for a full pane redraw since the last call
    /// (see the `redraw` field).
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// The pane's `cursor-style` option (0..=6): what DECRQSS and DECRQM 12
    /// report while the program has set no style (input.c:1666-1680,
    /// 2590-2600). Larger values count as 0.
    pub fn set_cursor_style_option(&mut self, value: u8) {
        let mut c = self.cursor.get();
        c.option = if value > 6 { 0 } else { value };
        self.cursor.set(c);
    }

    /// Take the bytes to write back to the pane program (tmux's replies),
    /// dropping what only the server could answer: for a pane that has no
    /// program or no window (a popup, `input_osc_10` with no `wp`).
    pub fn take_replies(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for r in self.take_answers() {
            if let Reply::Bytes(b) = r {
                out.extend_from_slice(&b);
            }
        }
        out
    }

    /// Take what the program asked back since the last call, in order:
    /// bytes to write back and the queries the server answers.
    pub fn take_answers(&mut self) -> Vec<Reply> {
        std::mem::take(&mut *self.replies.borrow_mut())
    }

    /// `MODE_THEME_UPDATES` (`CSI ? 2031 h`): the program wants a theme
    /// report whenever the theme changes.
    #[must_use]
    pub fn theme_updates(&self) -> bool {
        self.term.mode(Mode::COLOR_SCHEME_REPORT).unwrap_or(false)
    }

    /// The default colours OSC 10 and 11 set (`palette->fg`, `->bg`):
    /// `None` is the default.
    #[must_use]
    pub fn default_colours(&self) -> (Option<palette::Rgb>, Option<palette::Rgb>) {
        (self.scan.palette.fg, self.scan.palette.bg)
    }

    /// The palette entries OSC 4 set (`palette->palette`): `None` until
    /// the first is set, and after RIS or `OSC 104` with no argument.
    #[must_use]
    pub fn palette_entries(&self) -> Option<&[Option<palette::Rgb>; 256]> {
        self.scan.palette.entries()
    }

    /// A history row was read out of Ghostty (see [`history_reads`]).
    pub(crate) fn note_history_read(&self) {
        self.reads.set(self.reads.get().wrapping_add(1));
        HISTORY_READS.with(|r| r.set(r.get().wrapping_add(1)));
    }

    /// See [`CompressionActivity`]. Cheap: one Ghostty query.
    #[must_use]
    pub fn compression_activity(&self) -> CompressionActivity {
        CompressionActivity {
            ghostty: self.term.compression_activity().ok(),
            reads: self.reads.get(),
        }
    }

    /// Scroll progress and the minimum batch before starting another pass.
    #[must_use]
    pub fn compression_progress(&self) -> (u64, usize) {
        (self.scrolled, self.page_rows)
    }

    /// Whether Ghostty may hold history to compress. It compresses only
    /// the primary screen's pages before the one holding the active area's
    /// top row (PageList.zig `CompressionIterator`), so under a page of
    /// history there is none. Under the alternate screen the primary
    /// history can't be counted, so the answer is yes.
    #[must_use]
    pub fn has_history(&self) -> bool {
        self.alternate_on() || self.retained_rows() >= self.page_rows
    }

    /// One bounded step of scrollback compression. Standard pages use
    /// one LZ4 call; oversized pages use 128 KiB snapshot blocks (vendored
    /// patch 0004). Skips and failed attempts advance the pass. Reading a
    /// compressed page restores exactly the original bytes.
    pub fn compress_step(&mut self) -> Compression {
        match self.term.compress(CompressionMode::Incremental) {
            Ok(CompressionResult::Pending) => Compression::Pending,
            Ok(CompressionResult::Complete) => Compression::Complete,
            Ok(CompressionResult::Unsupported) | Err(_) => Compression::Unsupported,
        }
    }

    /// Compress every page of history Ghostty can, in one call (the pages
    /// [`PaneTerm::compress_step`] would reach, a pass's worth at once).
    /// For a live upgrade to a binary of the same [`crate::PATCH_STACK`],
    /// whose stream carries compressed pages as they are: compressing
    /// first costs less than exporting and importing them as rows twice
    /// (the dry run and the new image). A partial incremental pass is
    /// dropped; the next [`PaneTerm::compress_step`] starts a new one.
    pub fn compress_all(&mut self) -> Compression {
        match self.term.compress(CompressionMode::Full) {
            Ok(CompressionResult::Pending | CompressionResult::Complete) => Compression::Complete,
            Ok(CompressionResult::Unsupported) | Err(_) => Compression::Unsupported,
        }
    }

    /// Primary-screen pages Ghostty holds compressed now.
    #[must_use]
    pub fn compressed_pages(&self) -> u64 {
        self.term
            .memory_usage()
            .map_or(0, |u| u.primary_compressed_pages)
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
    /// where tmux changes nothing (DECSCA, DECSTR, a screen
    /// switch). Fixed once the parser is back in the ground state.
    resync: bool,
    /// The start of the OSC being skipped, kept to find OSC 133.
    osc: Option<Vec<u8>>,
    /// The payload of the OSC 133 that just ended.
    osc133: Option<Vec<u8>>,
    /// The OSC in progress as tmux collects it (`input_input`), kept only
    /// while it can be a palette OSC; `osc_cap` is its size limit.
    palette_osc: Option<Vec<u8>>,
    palette_discard: bool,
    osc_cap: usize,
    /// Bytes `input_input` has collected (0x20-0xff) of the OSC in
    /// progress, and whether that passed `osc_cap` (tmux sets
    /// `INPUT_DISCARD`: the string does nothing).
    osc_len: usize,
    osc_over: bool,
    /// tmux's palette, to tell which palette OSCs redraw the pane and to
    /// answer the colour queries.
    palette: Palette,
    /// What the OSC or mode just ended asks back; [`PaneTerm::feed`] moves
    /// it to the replies after Ghostty has the bytes before it.
    answers: Vec<Reply>,
    /// A palette OSC just ended with a redraw ([`PaneTerm::feed`] takes it).
    palette_redraw: bool,
    /// tmux's progress bar (`s->progress_bar`, OSC 9;4).
    progress: ProgressBar,
    /// An OSC 9;4 just set it ([`PaneTerm::feed`] reports it).
    progress_set: bool,
    /// Intermediate bytes (0x20-0x2f) in the CSI being scanned.
    interm_count: u8,
    /// The CSI had a byte tmux does not take as a parameter or intermediate
    /// (`:` aside), or a parameter after an intermediate: tmux ignores it.
    csi_ignored: bool,
    /// The cursor style tmux would hold, shared with the reply handler.
    cursor: Rc<StdCell<CursorState>>,
    /// The mouse modes tmux would hold, shared with the reply handler.
    mouse: Rc<StdCell<MouseModes>>,
    /// The CSI began `CSI >` (tmux's `>` intermediate, input.c:577).
    gt: bool,
    /// The CSI began with `>`, `<`, `=` or `?` and has had only numeric
    /// parameters since: a `u` ends a kitty keyboard sequence (D29).
    kitty: bool,
    key_mode: KeyMode,
    extended_keys: ExtendedKeys,
    /// The private mode sequence just ended set (`true`) or reset
    /// synchronized output (`?2026`); [`PaneTerm::feed`] takes it.
    sync: Option<bool>,
    /// The sets (`true`) and resets of mode 2048 in the private mode
    /// sequence just ended, in order; [`PaneTerm::feed`] takes them.
    inband: Vec<bool>,
    /// The title stack operations and size reports of the XTWINOPS that
    /// just ended, in order.
    title_ops: Vec<WinOp>,
    /// The OSC (while it can be an OSC 0 or 2) or APC string in progress,
    /// collected as tmux's `input_buf`, for the pane title.
    title: Option<TitleString>,
    /// The title string that just ended ([`ScanEvent::Title`]).
    title_done: Option<TitleDone>,
    /// tmux's UTF-8 decoder state (`utf8started`, `utf8data`).
    utf8: Utf8,
    /// What a character the decoder finished turns into, for
    /// [`PaneTerm::feed`] to write.
    utf8_out: Option<Vec<u8>>,
    /// The start of the SM or RM just ended as Ghostty must see it, with
    /// only the modes tmux knows ([`ScanEvent::Rewrite`]).
    rewrite: Option<Vec<u8>>,
    /// tmux's `INPUT_LAST`: the last thing parsed was a printed character,
    /// which REP repeats. Any control, escape or control sequence tmux
    /// dispatches, and the start of a string, clears it (input.c
    /// `input_c0_dispatch`, `input_esc_dispatch`, `input_csi_dispatch`,
    /// `input_enter_osc` and the other string starts, `input_top_bit_set`).
    /// Ghostty keeps its last character until RIS.
    last_print: bool,
    /// The private mode sequence just ended set or reset DECCOLM (`?3`);
    /// [`PaneTerm::feed`] takes it.
    deccolm: bool,
    /// The screen switch just ended names `?1049`, which saves and restores
    /// the cursor (input.c: `screen_write_alternateon` and `_off` with
    /// `cursor` 1); `?47` and `?1047` do not.
    alt_cursor: bool,
    /// The pane has OSC 133 marks, so the erases that clear whole lines
    /// (ED 1, ECH) end a scan for [`PaneTerm::erase_marks`].
    line_marks: bool,
    /// The count of the ECH ([`ScanEvent::EraseChars`]) that just ended.
    ech: u64,
    /// tmux's pen link and its copies ([`PenLinks`]).
    links: PenLinks,
    /// The OSC 8 being collected passed the input buffer's size: tmux
    /// discards it (`INPUT_DISCARD`, `input_exit_osc`).
    osc8_discard: bool,
}

/// The hyperlink in tmux's pen (`ictx->cell.cell.link`) and the copies tmux
/// keeps of the pen, each as the OSC 8 payload (`params;uri`) that set it;
/// `None` is no link. DECRC and RCP restore DECSC's copy
/// (`input_restore_state`), `?1049l` the copy the last alternate screen
/// entry took (`screen_alternate_on`, `screen_alternate_off`), RIS clears
/// both (`input_reset_cell`); a screen switch keeps the pen. Ghostty's link
/// is in its cursor: DECSC does not save it and a screen switch ends it, so
/// where the two part, vtmux gives Ghostty tmux's link again
/// ([`PaneTerm::feed`]).
#[derive(Debug, Default)]
struct PenLinks {
    pen: Option<Rc<[u8]>>,
    /// `ictx->old_cell`.
    saved: Option<Rc<[u8]>>,
    /// `s->saved_cell`.
    alternate: Option<Rc<[u8]>>,
    /// The link Ghostty's cursor has.
    ghostty: Option<Rc<[u8]>>,
}

impl PenLinks {
    fn state(&self) -> PenLinkState {
        let out = |l: &Option<Rc<[u8]>>| l.as_deref().map(<[u8]>::to_vec);
        PenLinkState {
            pen: out(&self.pen),
            saved: out(&self.saved),
            alternate: out(&self.alternate),
            emulator: out(&self.ghostty),
        }
    }

    fn from_state(s: &PenLinkState) -> Self {
        let link = |l: &Option<Vec<u8>>| l.as_deref().map(Rc::from);
        Self {
            pen: link(&s.pen),
            saved: link(&s.saved),
            alternate: link(&s.alternate),
            ghostty: link(&s.emulator),
        }
    }

    /// The OSC 8 that gives Ghostty tmux's link, if Ghostty's differs; it
    /// then has it.
    fn resync(&mut self) -> Option<Vec<u8>> {
        if self.ghostty == self.pen {
            return None;
        }
        self.ghostty.clone_from(&self.pen);
        let mut osc = b"\x1b]8;".to_vec();
        osc.extend_from_slice(self.pen.as_deref().unwrap_or(b";"));
        osc.extend_from_slice(b"\x1b\\");
        Some(osc)
    }
}

/// Which mouse events the pane program asked for: tmux's mutually exclusive
/// `MODE_MOUSE_STANDARD`, `MODE_MOUSE_BUTTON` and `MODE_MOUSE_ALL`
/// (`?1000`, `?1002`, `?1003`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTracking {
    /// No mouse mode: the pane did not ask for mouse events.
    #[default]
    None,
    /// `?1000`: button press and release.
    Standard,
    /// `?1002`: also motion while a button is down.
    Button,
    /// `?1003`: all motion.
    All,
}

/// The pane's mouse modes as tmux keeps them (`s->mode`, input.c:1960-1972,
/// 2054-2073), tracked from the byte stream by [`AltModeScanner`] and
/// independent of Ghostty's mode bits, which are not mutually exclusive and
/// answer DECRQM differently. Read it with [`PaneTerm::mouse_modes`].
///
/// The server uses it three ways: forwarding mouse events to the pane
/// (`tracking` says whether and how much, `utf8` and `sgr` pick the
/// encoding), the outer terminal's mode (`tty_update_mode`, tty.c:904-920),
/// and the `mouse_*_flag` formats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseModes {
    /// `?1000`, `?1002` or `?1003`: setting one clears the others, and
    /// resetting any of `?1000`-`?1003` clears all.
    pub tracking: MouseTracking,
    /// `?1005` (`MODE_MOUSE_UTF8`), set and reset on its own.
    pub utf8: bool,
    /// `?1006` (`MODE_MOUSE_SGR`), set and reset on its own.
    pub sgr: bool,
}

impl MouseModes {
    /// `ALL_MOUSE_MODES`: any of the three tracking modes is set
    /// (`#{mouse_any_flag}`).
    #[must_use]
    pub fn any(self) -> bool {
        self.tracking != MouseTracking::None
    }

    /// `CSI ? n h|l` for one mode number; other numbers change nothing.
    fn apply(&mut self, n: u64, set: bool) {
        let tracking = match n {
            1000 => MouseTracking::Standard,
            1002 => MouseTracking::Button,
            1003 => MouseTracking::All,
            // `case 1001` resets only (input.c:1957-1960); setting it is
            // not in the set switch.
            1001 if !set => MouseTracking::None,
            1005 => return self.utf8 = set,
            1006 => return self.sgr = set,
            _ => return,
        };
        self.tracking = if set { tracking } else { MouseTracking::None };
    }
}

/// The cursor shape a program set with DECSCUSR (`s->cstyle`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Default,
    Block,
    Underline,
    Bar,
}

/// See [`PaneTerm::cursor_style`].
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorStyle {
    pub shape: CursorShape,
    /// `MODE_CURSOR_BLINKING`, `MODE_CURSOR_BLINKING_SET`,
    /// `MODE_CURSOR_VERY_VISIBLE`.
    pub blinking: bool,
    pub blinking_set: bool,
    pub very_visible: bool,
}

/// See [`PaneTerm::screen_modes`].
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenModes {
    /// `MODE_KCURSOR` (DECCKM).
    pub app_cursor: bool,
    /// `MODE_KKEYPAD` (DECKPAM).
    pub app_keypad: bool,
    /// `MODE_ORIGIN` (DECOM).
    pub origin: bool,
    /// `MODE_WRAP` (DECAWM).
    pub wrap: bool,
    /// `MODE_INSERT` (IRM).
    pub insert: bool,
    /// `MODE_CURSOR` (DECTCEM).
    pub cursor_visible: bool,
    /// `MODE_FOCUSON`.
    pub focus: bool,
    /// `MODE_BRACKETPASTE`.
    pub bracketed_paste: bool,
    /// `MODE_SYNC`.
    pub sync: bool,
    /// `MODE_THEME_UPDATES`.
    pub theme_updates: bool,
    pub cursor_shape: CursorShape,
    /// `MODE_CURSOR_BLINKING` and `MODE_CURSOR_BLINKING_SET`.
    pub cursor_blinking: bool,
    pub cursor_blinking_set: bool,
    /// `MODE_CURSOR_VERY_VISIBLE`.
    pub cursor_very_visible: bool,
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
    /// `MODE_CURSOR_VERY_VISIBLE`: `CSI 34 l` sets it, `CSI 34 h` clears it
    /// (input.c:1913-1915, 2010-2012).
    very_visible: bool,
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
        self.very_visible = false;
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

/// tmux's SGR (input.c `input_csi_dispatch_sgr`) over the raw parameters
/// `raw` (already through `input_split`): `out` gets SGR parameters that
/// Ghostty reads the same way; false if they change nothing.
fn tmux_sgr(raw: &[u8], out: &mut String) -> bool {
    use std::fmt::Write as _;
    let params: Vec<&[u8]> = raw.split(|b| *b == b';').collect();
    // input_get: a missing parameter is `def`, a string -1.
    let get = |i: usize, def: i64| -> i64 {
        match params.get(i) {
            None | Some([]) => def,
            Some(p) if p.contains(&b':') => -1,
            Some(p) => std::str::from_utf8(p)
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(-1),
        }
    };
    let mut push = |s: std::fmt::Arguments<'_>| {
        if !out.is_empty() {
            out.push(';');
        }
        let _ = out.write_fmt(s);
    };
    let mut i = 0;
    while i < params.len() {
        if params[i].contains(&b':') {
            sgr_colon(params[i], &mut push);
            i += 1;
            continue;
        }
        let n = get(i, 0);
        if matches!(n, 38 | 48 | 58) {
            i += 1;
            match get(i, -1) {
                2 => {
                    if sgr_rgb(n, get(i + 1, -1), get(i + 2, -1), get(i + 3, -1), &mut push) {
                        i += 3;
                    }
                }
                5 => {
                    sgr_256(n, get(i + 1, -1), &mut push);
                    i += 1;
                }
                _ => {}
            }
        } else if n == 6 {
            push(format_args!("5"));
        } else if u64::try_from(n).is_ok_and(sgr_known) {
            push(format_args!("{n}"));
        }
        i += 1;
    }
    !out.is_empty()
}

/// True for SGR parameters Ghostty already reads as tmux does, which is
/// what most programs send: every parameter there, without a colon or a
/// leading zero, an attribute both know, and 38, 48 and 58 followed by
/// `5;n` or `2;r;g;b` in range. Checked without allocating.
fn sgr_is_plain(raw: &[u8]) -> bool {
    let mut params = raw.split(|b| *b == b';').map(|p| {
        if p.is_empty() || (p.len() > 1 && p[0] == b'0') {
            None
        } else {
            param_number(p)
        }
    });
    let byte = |v: Option<Option<u64>>| matches!(v, Some(Some(0..=255)));
    while let Some(p) = params.next() {
        match p {
            Some(n) if sgr_known(n) => {}
            Some(38 | 48 | 58) => match params.next() {
                Some(Some(5)) if byte(params.next()) => {}
                Some(Some(2))
                    if byte(params.next()) && byte(params.next()) && byte(params.next()) => {}
                _ => return false,
            },
            _ => return false,
        }
    }
    true
}

/// An SGR attribute both tmux and Ghostty read alike, as one parameter
/// (`input_csi_dispatch_sgr`; 6, rapid blink, tmux reads as 5).
fn sgr_known(n: u64) -> bool {
    matches!(
        n,
        0..=5 | 7..=9 | 21..=25 | 27..=37 | 39..=47 | 49 | 53 | 55 | 59 | 90..=97 | 100..=107
    )
}

/// `input_csi_dispatch_sgr_256_do`: out of range is the default colour
/// (none for the underline).
fn sgr_256(kind: i64, c: i64, push: &mut impl FnMut(std::fmt::Arguments<'_>)) {
    match (kind, c) {
        (58, -1 | 256..) => {}
        (_, -1 | 256..) => push(format_args!("{}", kind + 1)),
        _ => push(format_args!("{kind};5;{c}")),
    }
}

/// `input_csi_dispatch_sgr_rgb_do`: false, setting nothing, if a part is
/// missing or out of range.
fn sgr_rgb(
    kind: i64,
    r: i64,
    g: i64,
    b: i64,
    push: &mut impl FnMut(std::fmt::Arguments<'_>),
) -> bool {
    let ok = |v: i64| (0..=255).contains(&v);
    if !(ok(r) && ok(g) && ok(b)) {
        return false;
    }
    push(format_args!("{kind};2;{r};{g};{b}"));
    true
}

/// `input_csi_dispatch_sgr_colon`: one parameter with sub-parameters, at
/// most seven of them, each a number or missing.
fn sgr_colon(param: &[u8], push: &mut impl FnMut(std::fmt::Arguments<'_>)) {
    let mut p = [-1i64; 8];
    let mut n = 0;
    for piece in param.split(|b| *b == b':') {
        if !piece.is_empty() {
            let Some(v) = std::str::from_utf8(piece)
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v <= i64::from(i32::MAX))
            else {
                return;
            };
            p[n] = v;
        }
        n += 1;
        if n == p.len() {
            return;
        }
    }
    match (p[0], p[1]) {
        (4, style @ 0..=5) if n == 2 => push(format_args!("4:{style}")),
        (38 | 48 | 58, 2) if n >= 3 => {
            let i = if n == 5 { 2 } else { 3 };
            if n >= i + 3 {
                sgr_rgb(p[0], p[i], p[i + 1], p[i + 2], push);
            }
        }
        (38 | 48 | 58, 5) if n >= 3 => sgr_256(p[0], p[2], push),
        _ => {}
    }
}

/// `input_split`: `None` when tmux gives up on the sequence (a number above
/// `INT_MAX`, or 24 parameters: `param_list[24]`).
/// [`parse_params`] would take `raw`; without building the list.
fn params_split(raw: &[u8]) -> bool {
    let mut count = 0;
    for piece in raw.split(|b| *b == b';') {
        if !piece.is_empty() && !piece.contains(&b':') && param_number(piece).is_none() {
            return false;
        }
        count += 1;
        if count == 24 {
            return false;
        }
    }
    true
}

/// A parameter of digits as `input_split` takes it: at most `INT_MAX`.
fn param_number(piece: &[u8]) -> Option<u64> {
    let mut n: u64 = 0;
    for &d in piece {
        if !d.is_ascii_digit() {
            return None;
        }
        n = n * 10 + u64::from(d - b'0');
        if n > i32::MAX as u64 {
            return None;
        }
    }
    Some(n)
}

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
            Param::Num(param_number(piece)?)
        });
        if out.len() == 24 {
            return None;
        }
    }
    Some(out)
}

/// Something a pane's program did that the server reports
/// ([`PaneTerm::take_events`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// An OSC 0/2 title or a `CSI 23 t` pop, with the title it reported
    /// (`input_fire_pane_title_changed`, input.c:795-809).
    Title(String),
    /// This many BELs in a row (input.c `input_c0_dispatch`, `case '\007'`).
    Bells(u32),
    /// `OSC 133;A` or `N`: a shell prompt starts (`pane-shell-prompt`).
    Prompt,
    /// `OSC 133;C`: a command starts (`pane-command-started`).
    CommandStarted,
    /// `OSC 133;D`: a command finished, with its exit status
    /// (`input_osc_133_exit_status`; `pane-command-finished`).
    CommandFinished(u8),
    /// An OSC 9;4 set the progress bar ([`PaneTerm::progress_bar`]),
    /// changed or not: tmux redraws the window's borders and status
    /// (`input_set_progress_bar`, input.c:3016-3026).
    Progress,
    /// A desktop notification libghostty found (OSC 9 not `ConEmu`, OSC
    /// 777;notify), which tmux drops (spec D30).
    Notification(Notification),
}

/// The events not yet taken, BELs in a row counted in one. Unbounded:
/// tmux fires every one as it parses, so whoever feeds a terminal takes
/// them after each feed (a popup's are dropped then), which bounds the
/// list by one read.
#[derive(Debug, Default)]
struct Events {
    list: Vec<TermEvent>,
}

impl Events {
    fn push(&mut self, event: TermEvent) {
        if let (TermEvent::Bells(n), Some(TermEvent::Bells(last))) = (&event, self.list.last_mut())
        {
            *last = last.saturating_add(*n);
            return;
        }
        // One redraw stands for any number in a row.
        if event == TermEvent::Progress && self.list.last() == Some(&TermEvent::Progress) {
            return;
        }
        self.list.push(event);
    }
}

/// A pane's title as tmux keeps it (`s->title`).
#[derive(Debug, Default)]
struct Titles {
    /// The title, from the server (`select-pane -T`), the program (OSC 0
    /// or 2, APC) or a title pop. `None` only before the first: the
    /// title is then Ghostty's (an imported state's).
    server: Option<String>,
    /// tmux's title stack (`s->titles`, `CSI 22 t` / `CSI 23 t`), newest
    /// first; Ghostty ignores both.
    stack: Vec<String>,
}

/// What an XTWINOPS does that tmux handles itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WinOp {
    /// `22;0` or `22;2`: push the title.
    Push,
    /// `23;0` or `23;2`: pop it.
    Pop,
    /// 14, 15, 16, 18 or 19: a size report (input.c:2160-2186).
    Report(u64),
}

/// The title stack operations and size reports in an XTWINOPS
/// (`input_csi_dispatch_winops`, input.c:2124-2220), walking the
/// parameters as tmux does: tmux answers every report in it, in order. A
/// missing or string parameter where a number is read stops the walk.
fn winops(raw: &[u8]) -> Vec<WinOp> {
    let mut ops = Vec::new();
    let Some(params) = parse_params(raw) else {
        return ops;
    };
    // input_get(ictx, m, 0, -1).
    let get = |m: usize| match params.get(m) {
        Some(Param::Num(n)) => Some(*n),
        _ => None,
    };
    let mut m = 0;
    while let Some(n) = get(m) {
        match n {
            3 | 4 | 8 => {
                m += 1;
                if get(m).is_none() {
                    return ops;
                }
                m += 1;
                if get(m).is_none() {
                    return ops;
                }
            }
            9 | 10 => {
                m += 1;
                if get(m).is_none() {
                    return ops;
                }
            }
            22 | 23 => {
                m += 1;
                match get(m) {
                    None => return ops,
                    Some(0 | 2) => ops.push(if n == 22 { WinOp::Push } else { WinOp::Pop }),
                    Some(_) => {}
                }
            }
            14 | 15 | 16 | 18 | 19 => ops.push(WinOp::Report(n)),
            _ => {}
        }
        m += 1;
    }
    ops
}

/// What [`AltModeScanner`] found; reported on the sequence's final byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanEvent {
    /// Alternate screen set (`h`) or reset (`l`).
    AltMode(u8),
    /// `CSI 3 J`.
    ClearHistory,
    /// `CSI J` or `CSI 0 J`.
    EraseBelow,
    /// `CSI 2 J`.
    EraseScreen,
    /// `CSI 1 J`, while the pane has OSC 133 marks.
    EraseAbove,
    /// `CSI Ps X` (ECH), while the pane has OSC 133 marks;
    /// `AltModeScanner::ech` has its count.
    EraseChars,
    /// `ESC c` (RIS).
    Reset,
    /// `CSI ? 2026 h` or `l`: [`AltModeScanner::sync`] says which.
    Sync,
    /// `CSI ? 3 h` or `l` (DECCOLM): [`AltModeScanner::deccolm`] is set.
    Deccolm,
    /// Back in the ground state with [`AltModeScanner::take_resync`] due.
    Resync,
    /// An `OSC 133` ended; `AltModeScanner::osc133` has its payload.
    Osc133,
    /// `ESC P`: a DCS starts (on the `P`).
    DcsStart,
    /// An XTWINOPS tmux handles: `AltModeScanner::title_ops` has what.
    TitleStack,
    /// An OSC 0 or 2, or an APC string, ended:
    /// `AltModeScanner::title_done` has it.
    Title,
    /// `ESC k`: a screen-style window name starts (on the `k`).
    RenameStart,
    /// The final byte of a CSI tmux ignores and Ghostty acts on, or the
    /// end of an OSC tmux rejects: Ghostty gets CAN instead, which drops
    /// it.
    Drop,
    /// A byte of a UTF-8 character tmux's decoder holds; Ghostty gets
    /// [`AltModeScanner::utf8_out`] when one is ready, never the byte.
    Utf8,
    /// The byte ends an unfinished UTF-8 character (U+FFFD before it).
    Utf8Stop,
    /// A byte tmux ignores inside a CSI (0x7f-0xff, input.c:579, 597, 612,
    /// 628) without leaving it: Ghostty must not see it.
    Skip,
    /// The sequence just ended asks something back
    /// ([`AltModeScanner::answers`]).
    Answer,
    /// The OSC in progress passed `input-buffer-size`: tmux discards it
    /// whole. Ghostty, which would buffer up to 8 MiB of it, gets CAN and
    /// an OSC it rejects at its first byte ([`PaneTerm::feed`]).
    OscOver,
    /// An SM or RM with modes tmux does not know: Ghostty gets CAN and
    /// [`AltModeScanner::rewrite`] before the final byte.
    Rewrite,
    /// The `u` of a kitty keyboard push, pop, set or query (`CSI > u`,
    /// `CSI < u`, `CSI = u`, `CSI ? u`), which tmux ignores (D29).
    Kitty,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    /// `ESC` then an intermediate byte (`(`, `)`, `#`, ...); `None` after a
    /// second one, which no entry of `input_esc_table` has.
    EscIntermediate(Option<u8>),
    Csi,
    /// OSC: skipped to ST or BEL.
    Str,
    /// APC, PM or SOS: skipped to ST; a BEL is part of the string
    /// (`input_state_apc_string_table`, `input_state_consume_st_table`).
    Apc,
    /// Inside a DCS, which [`Dcs`] reads.
    Dcs,
    /// Inside `ESC k ... ST`, which [`PaneTerm::feed`] reads.
    Rename,
}

/// An OSC or APC string read for the pane title: tmux's `input_buf`
/// and `INPUT_DISCARD`.
#[derive(Debug, Default)]
struct TitleString {
    apc: bool,
    buf: Vec<u8>,
    discard: bool,
}

/// A title string that ended: an APC (`apc`) or an OSC 0 or 2, and its
/// text (`None`: tmux discarded it, it was longer than
/// `input-buffer-size`).
#[derive(Debug)]
struct TitleDone {
    apc: bool,
    /// An APC that is a kitty graphics command (`APC G`, D33).
    kitty: bool,
    text: Option<Vec<u8>>,
}

/// A window name being read (`ESC k`): tmux's `input_buf` and
/// `INPUT_DISCARD` for the rename state; `done` the names finished.
#[derive(Debug, Default)]
struct Rename {
    buf: Vec<u8>,
    discard: bool,
    since: Option<std::time::Instant>,
    done: Vec<Vec<u8>>,
}

impl AltModeScanner {
    /// Feed one byte; `Some` when it ends a sequence of interest.
    fn push(&mut self, b: u8) -> Option<ScanEvent> {
        if self.state == ScanState::Str {
            self.push_palette_osc(b);
        }
        if matches!(self.state, ScanState::Str | ScanState::Apc) {
            self.push_title(b);
        }
        let was_over = self.osc_over;
        if self.state == ScanState::Str && b >= 0x20 {
            self.osc_len = self.osc_len.saturating_add(1);
            self.osc_over |= self.osc_len > self.osc_cap;
        }
        let event = self.push_state(b);
        if event.is_none() && self.osc_over && !was_over && self.state == ScanState::Str {
            return Some(ScanEvent::OscOver);
        }
        if event.is_none() && self.title_done.is_some() {
            return Some(ScanEvent::Title);
        }
        if event.is_none() && !self.answers.is_empty() {
            return Some(ScanEvent::Answer);
        }
        event
    }

    /// tmux's view of an OSC (`input_state_osc_string_table`): collected
    /// until BEL, or any CAN, SUB or ESC, which end it where they stand
    /// (`input_set_state` runs the state's exit, `input_exit_osc`).
    fn push_palette_osc(&mut self, b: u8) {
        let Some(buf) = self.palette_osc.as_mut() else {
            return;
        };
        match b {
            0x07 | 0x18 | 0x1a | 0x1b => {
                let osc = self.palette_osc.take().unwrap_or_default();
                // input_end_bel: only BEL; ST and the rest end as ST.
                let end = if b == 0x07 { OscEnd::Bel } else { OscEnd::St };
                if self.palette_discard {
                    return;
                }
                if let Some((9, p)) = palette::osc_number(&osc) {
                    self.progress_set |= self.progress.osc_9(p);
                } else if self.palette.osc(&osc, end, &mut self.answers) {
                    self.palette_redraw = true;
                }
            }
            0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => {}
            _ if buf.len() >= self.osc_cap => self.palette_discard = true,
            _ => {
                buf.push(b);
                // Keep only what may be a palette OSC: after the number,
                // anything else is not one.
                if !b.is_ascii_digit() && !palette::is_palette_osc(buf) {
                    self.palette_osc = None;
                }
            }
        }
    }

    /// tmux's view of a string that may set the title (`input_input`
    /// collects 0x20-0xff; `input_exit_osc`, `input_exit_apc` read it): an
    /// OSC ends on BEL, either on CAN, SUB or ESC where they stand. An
    /// OSC whose number is not 0 or 2 is dropped as soon as that shows.
    fn push_title(&mut self, b: u8) {
        let Some(t) = self.title.as_mut() else {
            return;
        };
        match b {
            0x07 if !t.apc => {}
            0x18 | 0x1a | 0x1b => {}
            0x20.. => {
                if t.buf.len() >= self.osc_cap {
                    t.discard = true;
                } else if !t.discard {
                    t.buf.push(b);
                    if !t.apc
                        && !t.buf.iter().all(u8::is_ascii_digit)
                        && !matches!(palette::osc_number(&t.buf), Some((0 | 2, _)))
                    {
                        self.title = None;
                    }
                }
                return;
            }
            _ => return,
        }
        let Some(t) = self.title.take() else {
            return;
        };
        let kitty = t.apc && t.buf.first() == Some(&b'G');
        let text = if t.apc {
            Some(t.buf)
        } else {
            match palette::osc_number(&t.buf) {
                Some((0 | 2, rest)) => Some(rest.to_vec()),
                // An empty OSC, which tmux ignores.
                _ => return,
            }
        };
        self.title_done = Some(TitleDone {
            apc: t.apc,
            kitty,
            text: text.filter(|_| !t.discard),
        });
    }

    /// Track the OSC being skipped, to find OSC 133 and the OSC 8s tmux
    /// rejects; `Some` at its end. tmux ends an OSC on BEL, and on CAN, SUB
    /// or ESC where they stand (`input_set_state` runs `input_exit_osc`).
    fn push_osc(&mut self, b: u8) -> ControlFlow<Option<ScanEvent>> {
        let Some(osc) = self.osc.as_mut() else {
            return ControlFlow::Continue(());
        };
        match b {
            0x07 | 0x18 | 0x1a | 0x1b => {
                self.state = if b == 0x1b {
                    ScanState::Esc
                } else {
                    ScanState::Ground
                };
                let osc = self.osc.take().unwrap_or_default();
                let discard = std::mem::take(&mut self.osc8_discard);
                let event = if let Some(payload) = osc.strip_prefix(b"133;") {
                    self.osc133 = Some(payload.to_vec());
                    Some(ScanEvent::Osc133)
                } else if let Some(p) = osc.strip_prefix(b"8;") {
                    if discard || !hyperlink_ok(p) {
                        Some(ScanEvent::Drop)
                    } else {
                        // input_osc_8: an empty URI ends the link.
                        let uri = p
                            .iter()
                            .position(|&c| c == b';')
                            .map_or(&[][..], |n| &p[n + 1..]);
                        let link = (!uri.is_empty()).then(|| Rc::from(p));
                        self.links.pen.clone_from(&link);
                        self.links.ghostty = link;
                        None
                    }
                } else {
                    None
                };
                ControlFlow::Break(event)
            }
            // `input_input` collects 0x20-0xff; an OSC 8 whole, to the
            // input buffer's size.
            0x20.. if osc.starts_with(b"8;") => {
                if osc.len() >= self.osc_cap {
                    self.osc8_discard = true;
                } else {
                    osc.push(b);
                }
                ControlFlow::Continue(())
            }
            0x20.. if osc.len() < 32 => {
                osc.push(b);
                ControlFlow::Continue(())
            }
            _ => ControlFlow::Continue(()),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one match over the parser states, as input.c's tables"
    )]
    fn push_state(&mut self, b: u8) -> Option<ScanEvent> {
        if let ControlFlow::Break(end) = self.push_osc(b) {
            return end;
        }
        let rep_repeats_nothing = self.track_last_print(b);
        let prior = self.state;
        match (self.state, b) {
            (_, 0x18 | 0x1a) | (ScanState::Str, 0x07) => self.state = ScanState::Ground,
            (_, 0x1b) => self.state = ScanState::Esc,
            // A C1 byte is text in an APC, PM or SOS, as all of 0x20-0xff
            // are in tmux (`input_state_apc_string_table`); Ghostty would
            // end the string or start a sequence at it (`consumeApcString`),
            // and ignores the bytes it keeps none of (0xa0-0xff) anyway.
            #[allow(clippy::match_same_arms)] // the CSI arm below, kept by state
            (ScanState::Apc, 0x80..=0x9f) => return Some(ScanEvent::Skip),
            // Strings are skipped; further intermediates change nothing we track.
            (ScanState::Str | ScanState::Apc | ScanState::Dcs | ScanState::Rename, _) => {}
            (ScanState::EscIntermediate(_), 0x20..=0x2f) => {
                self.state = ScanState::EscIntermediate(None);
            }
            // SO and SI are executed anywhere outside a string, even inside
            // an escape sequence.
            (_, 0x0e | 0x0f) => self.charset_op(CharsetOp::Shift(b == 0x0e)),
            (ScanState::Ground, 0x80..) => {
                self.utf8_out = self.utf8.push(b);
                // input_utf8_close; an invalid character is printed by
                // input_stop_utf8, which does not set INPUT_LAST.
                self.last_print = self.utf8_out.is_some() && !self.utf8.bad;
                return Some(ScanEvent::Utf8);
            }
            // DEL does nothing in tmux's ground state (input.c
            // `input_state_ground_table`); Ghostty prints it.
            #[allow(clippy::match_same_arms)] // the CSI arm below, kept by state
            (ScanState::Ground, 0x7f) => return Some(ScanEvent::Skip),
            // Ground, and other C0 controls inside a sequence (executed).
            (ScanState::Ground, _)
            | (ScanState::Csi | ScanState::Esc | ScanState::EscIntermediate(_), 0x00..=0x1f) => {}
            // 0x7f-0xff after an ESC (and its intermediates) are no action
            // and no state change (input.c `input_state_esc_enter_table`,
            // `input_state_esc_intermediate_table`); Ghostty would print
            // U+FFFD and leave the escape.
            (ScanState::Esc | ScanState::EscIntermediate(_), 0x7f..) => {
                return Some(ScanEvent::Skip);
            }
            (ScanState::Esc, b'[') => {
                self.state = ScanState::Csi;
                self.private = false;
                self.gt = false;
                self.kitty = false;
                self.other = false;
                self.intermediate = None;
                self.interm_count = 0;
                self.csi_ignored = false;
                self.params.clear();
            }
            (ScanState::Esc, b']') => {
                self.state = ScanState::Str;
                self.osc_len = 0;
                self.osc_over = false;
                self.palette_osc = Some(Vec::new());
                self.palette_discard = false;
                self.osc = Some(Vec::new());
                self.title = Some(TitleString::default());
            }
            (ScanState::Esc, b'P' | b'k') => {
                let (state, event) = if b == b'P' {
                    (ScanState::Dcs, ScanEvent::DcsStart)
                } else {
                    (ScanState::Rename, ScanEvent::RenameStart)
                };
                self.state = state;
                return Some(event);
            }
            (ScanState::Esc, b'_' | b'^' | b'X') => {
                self.state = ScanState::Apc;
                self.title = (b == b'_').then(|| TitleString {
                    apc: true,
                    ..TitleString::default()
                });
            }
            (ScanState::Esc, 0x20..=0x2f) => self.state = ScanState::EscIntermediate(Some(b)),
            (ScanState::Esc, _) => {
                self.state = ScanState::Ground;
                match b {
                    b'7' => self.charset_op(CharsetOp::Save),
                    b'8' => self.charset_op(CharsetOp::Restore),
                    b'c' => {
                        // RIS: `colour_palette_clear` (input.c:1408), and
                        // `screen_write_reset` resets the key mode.
                        self.palette.clear();
                        // `screen_write_reset` leaves only cursor and wrap
                        // in `s->mode` (screen-write.c:430).
                        self.mouse.set(MouseModes::default());
                        self.key_mode = self.extended_keys.reset_mode();
                        self.charset_op(CharsetOp::Reset);
                        return Some(ScanEvent::Reset);
                    }
                    b'=' | b'>' | b'D' | b'E' | b'H' | b'M' | b'\\' => {}
                    // A final input_esc_table lacks dispatches nothing
                    // (input_esc_dispatch); Ghostty would run DECBI, DECFI,
                    // SS2, SS3, LS2, LS3, SPA, EPA or answer DECID.
                    _ => return Some(ScanEvent::Drop),
                }
            }
            (ScanState::EscIntermediate(i), _) => {
                self.state = ScanState::Ground;
                match (i, b) {
                    (Some(b'('), b'0' | b'B') => self.charset_op(CharsetOp::G0(b == b'0')),
                    (Some(b')'), b'0' | b'B') => self.charset_op(CharsetOp::G1(b == b'0')),
                    (Some(b'#'), b'8') => {}
                    // G2 and G3, the national sets, DECDHL, S7C1T and the
                    // rest: not in input_esc_table.
                    _ => return Some(ScanEvent::Drop),
                }
            }
            (ScanState::Csi, b'?')
                if self.params.is_empty()
                    && !self.private
                    && !self.gt
                    && self.interm_count == 0 =>
            {
                self.private = true;
                self.kitty = true;
            }
            // `CSI >` sequences are none of the others this scanner reads
            // (`other`); a second prefix byte is csi_ignore.
            (ScanState::Csi, b'>')
                if self.params.is_empty()
                    && !self.private
                    && !self.gt
                    && self.interm_count == 0 =>
            {
                self.gt = true;
                self.other = true;
                self.kitty = true;
            }
            // tmux has no `<` or `=` prefix (csi_ignore); a kitty keyboard
            // pop or set starts with one.
            (ScanState::Csi, b'<' | b'=') => {
                self.kitty = self.params.is_empty()
                    && !self.private
                    && !self.gt
                    && self.interm_count == 0
                    && !self.other;
                self.other = true;
                self.csi_ignored = true;
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
                self.kitty &= b != b':';
            }
            (ScanState::Csi, 0x40..=0x7e) => {
                self.state = ScanState::Ground;
                return if rep_repeats_nothing {
                    Some(ScanEvent::Drop)
                } else {
                    self.csi_event(b)
                };
            }
            (ScanState::Csi, 0x20..=0x2f) => {
                self.other = true;
                self.kitty = false;
                self.intermediate = Some(b);
                self.interm_count = self.interm_count.saturating_add(1);
            }
            // 0x7f-0xff are no action and no state change in every CSI
            // state (input.c:579, 597, 612, 628); Ghostty would print them.
            (ScanState::Csi, 0x7f..=0xff) => return Some(ScanEvent::Skip),
            (ScanState::Csi, _) => {
                self.other = true;
                self.kitty = false;
                self.csi_ignored = true;
            }
        }
        // `input_print` and `input_c0_dispatch` end an unfinished UTF-8
        // character; an escape sequence does not (input.c:1235, 1315). A
        // C0 control inside a sequence is left to the next printed byte:
        // Ghostty would take U+FFFD there as part of the sequence.
        let stops = match (prior, b) {
            (_, 0x18 | 0x1a) => true,
            (ScanState::Ground, 0x00..=0x7e) => b != 0x1b,
            _ => false,
        };
        if stops && self.utf8.stop() {
            return Some(ScanEvent::Utf8Stop);
        }
        (self.resync && self.state == ScanState::Ground).then_some(ScanEvent::Resync)
    }

    /// Follow tmux's `INPUT_LAST` ([`Self::last_print`]) over byte `b`, read
    /// in the state before it. True for a REP with nothing to repeat
    /// (`INPUT_CSI_REP` checks the flag), which Ghostty must not see: it
    /// would repeat its own last character.
    fn track_last_print(&mut self, b: u8) -> bool {
        if self.state == ScanState::Ground && (0x20..=0x7e).contains(&b) {
            self.last_print = true;
            return false;
        }
        if self.state == ScanState::Csi
            && b == b'b'
            && !self.private
            && !self.gt
            && self.interm_count == 0
            && !self.csi_ignored
            && !self.last_print
        {
            return true;
        }
        let clears = match (self.state, b) {
            // CAN and SUB anywhere; other C0 controls outside a string
            // (inside one the flag is already clear, its start cleared
            // it); a top-bit byte until its character ends
            // (`input_top_bit_set`, the Utf8 arm). ESC only starts a
            // sequence and DEL does nothing.
            (_, 0x18 | 0x1a) => true,
            (ScanState::Str | ScanState::Apc | ScanState::Dcs | ScanState::Rename, _)
            | (_, 0x1b | 0x7f) => false,
            (ScanState::Ground, 0x80..) | (_, 0x00..=0x1f) => true,
            // OSC, DCS, APC and the rename string clear it as they start
            // (`input_enter_osc` and the others); PM and SOS (`^`, `X`)
            // only consume to ST. input_esc_table: an ESC final tmux does
            // not know dispatches nothing and leaves the flag.
            (ScanState::Esc, _) => b"]P_k78=>DEHM\\c".contains(&b),
            (ScanState::EscIntermediate(i), _) => {
                matches!(
                    (i, b),
                    (Some(b'(' | b')'), b'0' | b'B') | (Some(b'#'), b'8')
                )
            }
            (ScanState::Csi, 0x40..=0x7e) => !self.csi_ignored && self.csi_in_tmux_table(b),
            _ => false,
        };
        if clears {
            self.last_print = false;
        }
        false
    }

    /// DA, DA2 and XTVERSION answer only parameter 0 (`input_get(ictx, 0,
    /// 0, 0)`, missing counts as 0; input.c:1582-1608, 1885-1890); Ghostty
    /// answers any.
    fn query_tmux_ignores(&self, b: u8) -> bool {
        (b == b'c' || (b == b'q' && self.gt))
            && !self.private
            && self.intermediate.is_none()
            && !self.query_param_zero()
    }

    fn query_param_zero(&self) -> bool {
        !self.csi_ignored
            && parse_params(&self.params)
                .is_some_and(|p| matches!(p.first(), None | Some(Param::Missing | Param::Num(0))))
    }

    /// Whether tmux's `input_csi_table` has the CSI ending in `b`, as
    /// `input_table_compare` matches it: the final byte and the collected
    /// prefix and intermediate bytes. One tmux ignores (`csi_ignored`)
    /// never gets there.
    fn csi_in_tmux_table(&self, b: u8) -> bool {
        match (self.private, self.gt, self.intermediate, self.interm_count) {
            (false, false, None, 0) => b"@ABCDEFGHJKLMPSTXZ`bcdfghlmnrstu".contains(&b),
            (true, false, None, 0) => matches!(b, b'S' | b'h' | b'l' | b'n'),
            (false, true, None, 0) => matches!(b, b'c' | b'm' | b'n' | b'q'),
            (_, false, Some(b'$'), 1) => b == b'p',
            (false, false, Some(b' '), 1) => b == b'q',
            _ => false,
        }
    }

    fn in_dcs(&self) -> bool {
        self.state == ScanState::Dcs
    }

    fn end_dcs(&mut self) {
        if self.in_dcs() {
            self.state = ScanState::Ground;
        }
    }

    fn charset_op(&mut self, op: CharsetOp) {
        self.charset.apply(op);
        let links = &mut self.links;
        match op {
            CharsetOp::Save => links.saved.clone_from(&links.pen),
            CharsetOp::Restore => links.pen.clone_from(&links.saved),
            CharsetOp::Reset => {
                links.pen = None;
                links.saved = None;
            }
            CharsetOp::G0(_) | CharsetOp::G1(_) | CharsetOp::Shift(_) => {}
        }
        self.resync = true;
    }

    /// True once if Ghostty's protection bit needs setting from
    /// [`Self::charset`]; only call in the ground state.
    fn take_resync(&mut self) -> bool {
        self.state == ScanState::Ground && std::mem::take(&mut self.resync)
    }

    /// `INPUT_CSI_MODSET` and `INPUT_CSI_MODOFF` (input.c:1527-1559):
    /// `CSI > 4 ; Pv m` and `CSI > 4 n`; other resources are ignored.
    fn key_mode_request(&mut self, set: bool) {
        let Some(params) = parse_params(&self.params) else {
            return;
        };
        // input_get(ictx, i, 0, 0): missing is 0, a string is -1.
        let get = |i: usize| match params.get(i) {
            None | Some(Param::Missing) => Some(0),
            Some(Param::Num(n)) => Some(*n),
            Some(Param::Text) => None,
        };
        if get(0) != Some(4) {
            return;
        }
        let always = self.extended_keys == ExtendedKeys::Always;
        self.key_mode = if set {
            if self.extended_keys == ExtendedKeys::Off {
                return;
            }
            match get(1) {
                Some(2) => KeyMode::Extended2,
                Some(1) => KeyMode::Extended1,
                _ if always => KeyMode::Extended1,
                _ => KeyMode::Standard,
            }
        } else if always {
            KeyMode::Extended1
        } else {
            KeyMode::Standard
        };
    }

    /// What Ghostty has and tmux's CSI table does not (input.c
    /// `input_csi_table`), such as CHT, HPR, VPR, DECIC, DECSTR or XTSAVE:
    /// tmux ignores them; Ghostty would move the cursor or the cells, or
    /// change a mode. And the EDs tmux ignores (ED 22 scrolls in Ghostty),
    /// or does with history (ED 0, ED 2).
    fn csi_tmux_lacks(&self, b: u8) -> Option<ScanEvent> {
        // input_csi_dispatch: nothing for a sequence that went to
        // csi_ignore, whose parameters input_split rejects, or that
        // input_csi_table does not have (DECSTR, DECSCA, XTSAVE, DA3, ...).
        if self.csi_ignored || !self.csi_in_tmux_table(b) || !params_split(&self.params) {
            return Some(ScanEvent::Drop);
        }
        if self.private || self.gt || b != b'J' || self.intermediate.is_some() {
            return None;
        }
        // `input_get(ictx, 0, 0, 0)` (input.c:1739-1765).
        match parse_params(&self.params).map(|p| p.first().copied()) {
            Some(None | Some(Param::Missing | Param::Num(0))) => Some(ScanEvent::EraseBelow),
            Some(Some(Param::Num(2))) => Some(ScanEvent::EraseScreen),
            Some(Some(Param::Num(1))) if self.line_marks => Some(ScanEvent::EraseAbove),
            Some(Some(Param::Num(1 | 3))) => None,
            _ => Some(ScanEvent::Drop),
        }
    }

    /// SM or RM: mode 34, the very visible cursor, is tmux's own
    /// (input.c:1913-1915, 2010-2012; RM sets it).
    fn ansi_mode(&mut self, set: bool) {
        if let Some(params) = parse_params(&self.params) {
            let mut c = self.cursor.get();
            for p in params {
                if p == Param::Num(34) {
                    c.very_visible = !set;
                }
            }
            self.cursor.set(c);
        }
    }

    /// ECH, while the pane has OSC 133 marks: its count is
    /// `input_get(ictx, 0, 1, 1)` (input.c, `INPUT_CSI_ECH`); a string
    /// parameter is -1, which clears nothing.
    fn ech_event(&mut self, b: u8) -> Option<ScanEvent> {
        if !self.line_marks || self.private || self.intermediate.is_some() || b != b'X' {
            return None;
        }
        self.ech = match parse_params(&self.params).map(|p| p.first().copied()) {
            Some(None | Some(Param::Missing)) => 1,
            Some(Some(Param::Num(n))) => n.max(1),
            _ => return None,
        };
        Some(ScanEvent::EraseChars)
    }

    /// DECSTBM (input.c:1620-1625): tmux reads the first two parameters
    /// with `input_get(ictx, i, 1, ...)`, so an explicit 0 is 1, where
    /// Ghostty takes a bottom of 0 for the last row and ignores the
    /// sequence with more than two. Ghostty is given the region tmux sets,
    /// or nothing when tmux sets none (`screen_write_scrollregion` refuses
    /// a region of one line or less, and `CSI n;0 r` is always one).
    fn decstbm(&mut self) -> Option<ScanEvent> {
        if self.private || self.gt || self.csi_ignored || self.intermediate.is_some() {
            return None;
        }
        let params = parse_params(&self.params)?;
        let bottom_zero = params.get(1) == Some(&Param::Num(0));
        if !bottom_zero && params.len() <= 2 {
            return None;
        }
        let get = |i: usize| match params.get(i) {
            None | Some(Param::Missing) => Some(None),
            Some(Param::Num(n)) => Some(Some(*n)),
            Some(Param::Text) => None,
        };
        let (Some(top), Some(bottom)) = (get(0), get(1)) else {
            return Some(ScanEvent::Drop);
        };
        if bottom_zero {
            return Some(ScanEvent::Drop);
        }
        // Ghostty gets the first two parameters, then the final byte.
        let num = |n: Option<u64>| n.map(|n| n.to_string()).unwrap_or_default();
        self.rewrite = Some(format!("\x1b[{};{}", num(top), num(bottom)).into_bytes());
        Some(ScanEvent::Rewrite)
    }

    /// DECRQM with more than one parameter: tmux reads only the first
    /// (`input_get(ictx, 0, 0, 0)`, input.c:1638-1722) and answers for it,
    /// where Ghostty answers nothing. Ghostty is given the first, and the
    /// reply hook maps its answer to tmux's ([`decrqm_reply`]); with no
    /// mode there (missing, a string, 0) tmux answers nothing (`if (m >
    /// 0)`). One parameter goes to Ghostty as it is.
    fn decrqm(&mut self) -> Option<ScanEvent> {
        let params = parse_params(&self.params)?;
        if params.len() <= 1 {
            return None;
        }
        match params[0] {
            Param::Num(n) if n > 0 => {
                let q = if self.private { "?" } else { "" };
                self.rewrite = Some(format!("\x1b[{q}{n}$").into_bytes());
                Some(ScanEvent::Rewrite)
            }
            _ => Some(ScanEvent::Drop),
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per CSI final byte
    fn csi_event(&mut self, b: u8) -> Option<ScanEvent> {
        if b == b'u' && self.kitty {
            return Some(ScanEvent::Kitty);
        }
        if let Some(e) = self
            .csi_tmux_lacks(b)
            .or_else(|| if b == b'r' { self.decstbm() } else { None })
        {
            return Some(e);
        }
        if self.query_tmux_ignores(b) {
            return Some(ScanEvent::Drop);
        }
        if b == b'p'
            && self.intermediate == Some(b'$')
            && let Some(e) = self.decrqm()
        {
            return Some(e);
        }
        if self.gt {
            if self.intermediate.is_none() && !self.csi_ignored && matches!(b, b'm' | b'n') {
                self.key_mode_request(b == b'm');
            }
            return self.resync.then_some(ScanEvent::Resync);
        }
        let op = match (self.private, self.intermediate, b) {
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
            (true, None, b'h' | b'l') => {
                self.private_modes(b == b'h');
                None
            }
            (false, None, b'h' | b'l') if !self.csi_ignored => {
                self.ansi_mode(b == b'h');
                None
            }
            // SGR: Ghostty gets what tmux reads, or nothing.
            (false, None, b'm') if !self.params.is_empty() && !self.tmux_sgr() => {
                return Some(ScanEvent::Drop);
            }
            (false, None, b't') if !self.other && !self.csi_ignored => {
                self.title_ops = winops(&self.params);
                if !self.title_ops.is_empty() {
                    return Some(ScanEvent::TitleStack);
                }
                None
            }
            // SCP and RCP whatever their parameters (INPUT_CSI_SCP,
            // INPUT_CSI_RCP); Ghostty would take an SCP with parameters
            // for DECSLRM.
            (false, None, b's') => {
                if !self.params.is_empty() {
                    self.rewrite = Some(b"\x1b[".to_vec());
                }
                Some(CharsetOp::Save)
            }
            (false, None, b'u') => Some(CharsetOp::Restore),
            _ if self.other => None,
            _ => None,
        };
        if let Some(op) = op {
            self.charset_op(op);
            return Some(ScanEvent::Resync);
        }
        if !self.gt
            && self.intermediate.is_none()
            && matches!(b, b'h' | b'l')
            && !self.keep_tmux_modes()
        {
            return Some(ScanEvent::Drop);
        }
        if !self.private && b == b'J' && matches!(self.params.as_slice(), b"3" | b"3;0") {
            return Some(ScanEvent::ClearHistory);
        }
        if let Some(e) = self.ech_event(b) {
            return Some(e);
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
            self.alt_cursor = self.params.split(|c| *c == b';').any(|p| p == b"1049");
            return Some(ScanEvent::AltMode(b));
        }
        if self.sync.is_some() {
            return Some(ScanEvent::Sync);
        }
        if self.deccolm {
            return Some(ScanEvent::Deccolm);
        }
        if self.rewrite.is_some() {
            return Some(ScanEvent::Rewrite);
        }
        self.resync.then_some(ScanEvent::Resync)
    }

    /// A private SM (`set`) or RM: the blink mode (12) and the mouse modes
    /// touch our state, in parameter order.
    fn private_modes(&mut self, set: bool) {
        let Some(params) = parse_params(&self.params) else {
            return;
        };
        let mut c = self.cursor.get();
        let mut m = self.mouse.get();
        for p in params {
            match p {
                Param::Num(12) => c.set_blinking(set),
                Param::Num(2026) => self.sync = Some(set),
                Param::Num(3) => self.deccolm = true,
                Param::Num(2031) => self.answers.push(Reply::ThemeUpdates(set)),
                Param::Num(2048) => self.inband.push(set),
                Param::Num(n) => m.apply(n, set),
                _ => {}
            }
        }
        self.cursor.set(c);
        self.mouse.set(m);
    }

    /// False if the SGR just ended changes nothing for tmux; else, if
    /// Ghostty would read it otherwise, [`Self::rewrite`] has it as tmux
    /// reads it.
    fn tmux_sgr(&mut self) -> bool {
        // What most programs send, as it is.
        if sgr_is_plain(&self.params) {
            return true;
        }
        let mut sgr = String::new();
        if !tmux_sgr(&self.params, &mut sgr) {
            return false;
        }
        if sgr.as_bytes() != self.params.as_slice() {
            self.rewrite = Some(format!("\x1b[{sgr}").into_bytes());
        }
        true
    }

    /// The modes of an SM or RM that tmux knows (input.c
    /// `input_csi_dispatch_sm`, `_sm_private`, `_rm`, `_rm_private`);
    /// others, such as LNM, reverse wrap, DECLRMM, 1048 or Ghostty's
    /// clustering (2027, which stays on), do nothing there. False when it
    /// has none tmux knows; else, if some are not, [`Self::rewrite`] has
    /// the sequence's start as Ghostty sees it instead.
    fn keep_tmux_modes(&mut self) -> bool {
        const PRIVATE: [u64; 18] = [
            1, 3, 6, 7, 12, 25, 47, 1000, 1002, 1003, 1004, 1005, 1006, 1047, 1049, 2004, 2026,
            2031,
        ];
        // Missing and string parameters are -1 to input_get: no mode.
        let private = self.private;
        let known = |p: &&[u8]| {
            !p.is_empty()
                && param_number(p).is_some_and(|n| {
                    if private {
                        PRIVATE.contains(&n)
                    } else {
                        n == 4
                    }
                })
        };
        let mut pieces = self.params.split(|b| *b == b';');
        if pieces.clone().all(|p| known(&p)) {
            return true;
        }
        let mut start = format!("\x1b[{}", if private { "?" } else { "" }).into_bytes();
        let mut any = false;
        for p in pieces.by_ref().filter(known) {
            if any {
                start.push(b';');
            }
            start.extend_from_slice(p);
            any = true;
        }
        if any {
            self.rewrite = Some(start);
        }
        any
    }
}

/// Rows one standard Ghostty page holds at `cols` columns
/// (`initialCapacity`, PageList.zig, through `Capacity.adjust`, page.zig):
/// as many row headers and rows of cells as fit in [`PAGE_GRID_BYTES`],
/// with the cell array aligned to [`CELLS_ALIGN`]. This is also Ghostty's
/// smallest line limit (`Limits.minMaxLines`). A width no row fits in gets
/// a bigger page of `std_capacity`'s 215 rows. Public for importers
/// that split rows into pages with no recorded layout.
#[must_use]
pub fn page_rows(cols: u16) -> usize {
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

/// The most threads [`compress_all_of`] runs.
const COMPRESS_THREADS: usize = 16;

/// [`PaneTerm::compress_all`] for every terminal of `terms`, shared out
/// over up to [`COMPRESS_THREADS`] threads (no more than the host's
/// parallelism or the terminals), each taking the next terminal left;
/// returns once all are done. A live upgrade compresses every pane's
/// history before it writes the stream (decision 23): one thread took
/// 0.85 s for 10 panes x 100k lines of 200 columns (`lu_upgrade_perf`),
/// most of decision 13's second.
pub fn compress_all_of<'a>(terms: impl IntoIterator<Item = &'a mut PaneTerm>) {
    /// The one access to a terminal, for one [`Terminal::compress`] on
    /// another thread.
    struct Exclusive<'a>(&'a mut Terminal<'static, 'static>);
    // SAFETY: an `Exclusive` holds the only reference to its terminal for
    // as long as it lives, and is used for one call,
    // `Terminal::compress(Full)`, which reads and writes only that
    // terminal's own page list and memory pools, and gets memory from the
    // process-wide page allocator (`PageList.zig` `pageAllocator`:
    // `std.heap.page_allocator`, Mach VM on macOS; both thread-safe).
    // It runs none of the terminal's callbacks, so none of the `Rc`s
    // `PaneTerm` shares with them is touched, and Ghostty's terminal code
    // keeps no thread-local state (no `threadlocal` under `src/terminal`
    // at the pinned commit; VENDOR.md, *Bumping ghostty*, says to check
    // again). Moving that access to another thread for the call is
    // therefore sound, though libghostty-vt does not promise it in
    // general (its crate docs, *Thread safety*).
    unsafe impl Send for Exclusive<'_> {}
    let jobs: Vec<Exclusive<'a>> = terms.into_iter().map(|t| Exclusive(&mut t.term)).collect();
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(COMPRESS_THREADS)
        .min(jobs.len());
    if threads <= 1 {
        for job in jobs {
            let _ = job.0.compress(CompressionMode::Full);
        }
        return;
    }
    let queue = std::sync::Mutex::new(jobs);
    let next = || queue.lock().map_or(None, |mut q| q.pop());
    std::thread::scope(|scope| {
        // This thread is one of them.
        for _ in 1..threads {
            // The stack the server's main thread has, for Ghostty's frames
            // (an LZ4 table is 16 KiB of it).
            let worker = std::thread::Builder::new()
                .name("vtmux-compress".into())
                .stack_size(8 << 20)
                .spawn_scoped(scope, || {
                    while let Some(job) = next() {
                        let _ = job.0.compress(CompressionMode::Full);
                    }
                });
            if worker.is_err() {
                // No thread: the ones running, or this one, do the rest.
                break;
            }
        }
        while let Some(job) = next() {
            let _ = job.0.compress(CompressionMode::Full);
        }
    });
}

/// Append reply bytes, joining them to the bytes before.
fn push_bytes(replies: &mut Vec<Reply>, bytes: &[u8]) {
    if let Some(Reply::Bytes(last)) = replies.last_mut() {
        last.extend_from_slice(bytes);
    } else {
        replies.push(Reply::Bytes(bytes.to_vec()));
    }
}

const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

/// The length of the whole, valid UTF-8 character `bytes` starts with.
fn utf8_char_len(bytes: &[u8]) -> Option<usize> {
    let n = match bytes.first()? {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    std::str::from_utf8(bytes.get(..n)?).ok().map(|_| n)
}

/// tmux's UTF-8 decoder (input.c `input_top_bit_set`, utf8.c `utf8_open`,
/// `utf8_append`): the lead byte fixes the length, every top-bit byte up to
/// it is taken, and a character that is not valid then (a byte that is not
/// a continuation, an overlong form, a surrogate) is one U+FFFD. Ghostty
/// gives one U+FFFD per bad byte. Only a character that is valid as tmux
/// sees it but past U+10FFFF differs: tmux keeps the bytes as a cell
/// (glibc `mbtowc`), Ghostty cannot, so it is U+FFFD too.
#[derive(Clone, Copy, Debug, Default)]
struct Utf8 {
    buf: [u8; 4],
    have: usize,
    /// 0 when no character is started.
    size: usize,
    /// The character being read, or the last one ended, is not valid.
    bad: bool,
}

impl Utf8 {
    fn started(self) -> bool {
        self.size != 0
    }

    /// Take a top-bit byte; the bytes for Ghostty once the character ends.
    fn push(&mut self, b: u8) -> Option<Vec<u8>> {
        if self.size == 0 {
            self.size = match b {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => {
                    self.bad = true;
                    return Some(REPLACEMENT.to_vec());
                }
            };
            self.buf[0] = b;
            self.have = 1;
            self.bad = false;
            return None;
        }
        self.bad |= b & 0xc0 != 0x80;
        self.buf[self.have] = b;
        self.have += 1;
        if self.have < self.size {
            return None;
        }
        let seq = &self.buf[..self.size];
        self.size = 0;
        self.bad |= std::str::from_utf8(seq).is_err();
        Some(if self.bad {
            REPLACEMENT.to_vec()
        } else {
            seq.to_vec()
        })
    }

    /// `input_stop_utf8`: drop an unfinished character; whether there was
    /// one.
    fn stop(&mut self) -> bool {
        std::mem::take(&mut self.size) != 0
    }
}

/// `input_osc_8` (input.c:2977-3014) takes the parameters `p` (after
/// `8;`): the URI starts after the first `;`, with at most one `id=`.
/// tmux ignores any other OSC 8 and the link stays as it was.
fn hyperlink_ok(p: &[u8]) -> bool {
    let mut id = false;
    let mut start = 0;
    while let Some(n) = p[start..].iter().position(|c| matches!(c, b':' | b';')) {
        let end = start + n;
        if end - start >= 4 && p[start..end].starts_with(b"id=") {
            if id {
                return false;
            }
            id = true;
        }
        if p[end] == b';' {
            return true;
        }
        start = end + 1;
    }
    false
}

/// Ghostty's grapheme clustering (mode 2027), set on every pane terminal
/// and after anything that may have reset it (RIS, `CSI ? 2027 l`). tmux
/// clusters characters itself (`screen_write_combine`, screen-write.c:
/// 2850-2960): regional-indicator pairs, emoji modifiers, ZWJ sequences
/// and VS16 make one cell, which is what Ghostty does in this mode and not
/// without it. tmux has no mode 2027 (DECRQM answers 0), so a program
/// cannot turn it off.
fn keep_clustering(term: &mut Terminal<'_, '_>) {
    let _ = term.set_mode(Mode::GRAPHEME_CLUSTER, true);
}

/// tmux's `cx`, `cy` from Ghostty's cursor. A print that fills the last
/// column leaves tmux's `cx` at the column count only under autowrap; with
/// DECAWM reset it stays on the last column (screen-write.c
/// `screen_write_cell`, `not_wrap`). Ghostty keeps the last column plus a
/// pending-wrap flag, which it sets only under autowrap (patch 0016), so
/// the flag is tmux's `cx` past the last column, whatever the mode is now.
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
/// - DECRQM -> tmux's mode table (input.c:1638-1722), plus mode 2048
///   while offered or granted ([`InbandResize`], D31)
/// - DECRQSS: answered by [`PaneTerm::answer_decrqss`] from the DCS parser
///   (Ghostty's own SGR/DECSTBM/DECSLRM replies are dropped here)
/// - XTVERSION `DCS > | tmux 3.8 ST` (input.c:1885-1890): produced by the
///   registered effect; the size reports (14, 15, 16, 18, 19 t) are
///   [`PaneTerm::size_report`]s, Ghostty's dropped
///
/// Dropped, tmux does not answer: DA3, DECXCPR `CSI ? 6 n` and DSR ?998
/// (tmux's private DSR knows only 996, input.c:1631-1637), mode 2033/2048
/// reports (a granted pane's 2048 reports are vtmux's own, D31: `?2048`
/// never reaches Ghostty), kitty keyboard `CSI ? u`, XTWINOPS
/// 21 t (input.c:2143-2149), every OSC query (colours, clipboard, OSC 5522,
/// 7501: the colour and clipboard queries are [`Reply`]s from tmux's own
/// palette model, `palette.rs`), XTGETTCAP, Kitty graphics and Glyph
/// Protocol APC. DSR ?996 is [`Reply::Theme`], from the colour-scheme
/// effect.
fn tmux_reply(term: &Terminal<'_, '_>, modes: SeenModes, data: &[u8]) -> Option<Vec<u8>> {
    // D33: Ghostty's answer to a granted screen's graphics command.
    if modes.graphics && data.starts_with(b"\x1b_G") {
        return Some(data.to_vec());
    }
    if let Some(body) = data.strip_prefix(b"\x1b[") {
        if let Some(params) = body.strip_suffix(b"$y") {
            return decrqm_reply(term, modes, params);
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
            // D29: Ghostty answers `CSI ? u` only when the scanner let it
            // through (`vtmux-kitty-keys` on, or the screen granted), with
            // the flags vtmux applies.
            b'u' => {
                let flags: u8 = std::str::from_utf8(body.strip_prefix(b"?")?.strip_suffix(b"u")?)
                    .ok()?
                    .parse()
                    .ok()?;
                Some(format!("\x1b[?{}u", flags & crate::kitty_keys::APPLIED).into_bytes())
            }
            _ => None,
        };
    }
    data.starts_with(b"\x1bP>|").then(|| data.to_vec())
}

/// The mode state tmux keeps beside Ghostty, as the reply hook reads it.
#[derive(Clone, Copy)]
struct SeenModes {
    cursor: CursorState,
    mouse: MouseModes,
    inband: InbandResize,
    /// D33: a granted screen's graphics command is ending.
    graphics: bool,
}

/// DECRQM reply from tmux's table. `params` is the Ghostty reply between
/// `ESC[` and `$y`: `[?]mode;state`.
fn decrqm_reply(term: &Terminal<'_, '_>, modes: SeenModes, params: &[u8]) -> Option<Vec<u8>> {
    let SeenModes {
        cursor,
        mouse,
        inband,
        ..
    } = modes;
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
    let reported = |set: bool| if set { 1 } else { 2 }; // 1 set, 2 reset
    let on = |m: Mode| reported(term.mode(m).unwrap_or(false));
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
            // tmux's own bits, not Ghostty's (input.c:1689-1706).
            1000 => reported(mouse.tracking == MouseTracking::Standard),
            1002 => reported(mouse.tracking == MouseTracking::Button),
            1003 => reported(mouse.tracking == MouseTracking::All),
            1004 => on(Mode::FOCUS_EVENT),
            1005 => reported(mouse.utf8),
            1006 => reported(mouse.sgr),
            2004 => on(Mode::BRACKETED_PASTE),
            2026 => on(Mode::SYNC_OUTPUT),
            2031 => on(Mode::COLOR_SCHEME_REPORT),
            // D31, vtmux's own: tmux's 0 unless offered or granted.
            2048 => inband.decrqm(),
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
/// `utf8_isvalid` (utf8.c:827-847) for a `&str`: no ASCII control
/// character or DEL.
fn valid_name(s: &str) -> bool {
    !s.chars().any(|c| c.is_ascii_control())
}

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
    use crate::snapshot::snapshot;

    fn replies_to(cols: u16, rows: u16, bytes: &[u8]) -> Vec<u8> {
        let mut t = PaneTerm::new(cols, rows, 100);
        t.feed(bytes);
        t.take_replies()
    }

    fn mouse_after(bytes: &[u8]) -> MouseModes {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(bytes);
        t.mouse_modes()
    }

    fn decrqm(bytes: &[u8], mode: u16) -> String {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(bytes);
        t.feed(format!("\x1b[?{mode}$p").as_bytes());
        String::from_utf8(t.take_replies()).unwrap()
    }

    /// `MODE_SYNC` (screen-write.c:1017-1062): `?2026h` keeps the screen
    /// shown then; `?2026l`, RIS, a resize or `stop_sync` end it, and DECRQM
    /// follows. A second `?2026h` keeps the first hold and re-arms the timer;
    /// a mode among others in one SM counts.
    #[test]
    fn synchronized_output_holds_the_screen() {
        let text = |s: &crate::Snapshot| -> String {
            s.cells.iter().map(|c| c.text.as_str()).collect::<String>()
        };
        let mut t = PaneTerm::new(10, 2, 100);
        t.feed(b"old");
        assert!(t.sync_held().is_none() && t.sync_deadline().is_none());
        t.feed(b"[?2026h[2Jnew");
        assert_eq!(text(&t.sync_held().unwrap().screen), "old");
        let first = t.sync_deadline().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        t.feed(b"[?25;2026hmore");
        assert_eq!(
            text(&t.sync_held().unwrap().screen),
            "old",
            "the first hold stays"
        );
        assert!(t.sync_deadline().unwrap() > first, "the timer is re-armed");
        t.feed(b"[?2026l");
        assert!(t.sync_held().is_none());
        assert!(text(&snapshot(&t)).contains("newmore"));

        for end in [&b"c"[..], b"[?2026l"] {
            let mut t = PaneTerm::new(10, 2, 100);
            t.feed(b"[?2026h");
            t.feed(end);
            assert!(t.sync_held().is_none(), "{end:?}");
        }
        let mut t = PaneTerm::new(10, 2, 100);
        t.feed(b"[?2026h");
        t.resize(10, 2);
        assert!(t.sync_held().is_some(), "the same size is no resize");
        t.resize(12, 2);
        assert!(t.sync_held().is_none(), "window_pane_resize stops it");
        t.feed(b"[?2026h[?2026$p");
        assert_eq!(t.take_replies(), b"\x1b[?2026;1$y");
        assert!(t.stop_sync());
        assert!(!t.stop_sync(), "already off");
        // screen_write_flush_dirty: the rows that differ from the held
        // screen when it ends, once.
        let mut t = PaneTerm::new(10, 3, 100);
        t.feed(b"a\r\nb\r\nc");
        assert_eq!(t.take_sync_flush(), None, "no hold ended");
        t.feed(b"\x1b[?2026h\x1b[2;1HB\x1b[?2026l");
        assert_eq!(t.take_sync_flush(), Some(vec![false, true, false]));
        assert_eq!(t.take_sync_flush(), None, "taken");
        t.feed(b"\x1b[?2026h\x1b[1;1HA");
        assert!(t.stop_sync());
        assert_eq!(t.take_sync_flush(), Some(vec![true, false, false]));
        // The hold shares the view's grid; the view copies it only to
        // read rows while the hold is on. A row written with what it held
        // is not flushed.
        t.feed(b"\x1b[?2026h");
        let held = Rc::clone(&t.sync_held().unwrap().screen);
        assert!(Rc::ptr_eq(&held, &t.screen().shared()), "no copy");
        t.feed(b"\x1b[3;1Hc\x1b[?2026l");
        assert_eq!(t.take_sync_flush(), Some(vec![false, false, false]));
        t.feed(b"[?2026$p");
        assert_eq!(
            t.take_replies(),
            b"\x1b[?2026;2$y",
            "the timer's stop reports off"
        );
    }

    #[test]
    fn mouse_tracking_modes_are_exclusive() {
        use MouseTracking::{All, Button, None, Standard};
        for (seq, want) in [
            (&b"\x1b[?1000h"[..], Standard),
            (b"\x1b[?1002h", Button),
            (b"\x1b[?1003h", All),
            (b"\x1b[?1000h\x1b[?1002h", Button),
            (b"\x1b[?1002h\x1b[?1003h", All),
            (b"\x1b[?1003h\x1b[?1000h", Standard),
            (b"\x1b[?1000;1002h", Button),
            // Any reset of 1000-1003 clears all of them.
            (b"\x1b[?1000h\x1b[?1002h\x1b[?1002l", None),
            (b"\x1b[?1003h\x1b[?1000l", None),
            (b"\x1b[?1002h\x1b[?1001l", None),
            (b"\x1b[?1002h\x1b[?1003l", None),
            // 1001 is not a set mode.
            (b"\x1b[?1001h", None),
        ] {
            assert_eq!(mouse_after(seq).tracking, want, "{seq:?}");
        }
    }

    #[test]
    fn mouse_format_modes_are_independent() {
        let m = mouse_after(b"\x1b[?1006h");
        assert!(m.sgr && !m.utf8 && !m.any());
        let m = mouse_after(b"\x1b[?1005h\x1b[?1006h\x1b[?1000h");
        assert!(m.sgr && m.utf8 && m.any());
        // Resetting tracking leaves the format bits.
        let m = mouse_after(b"\x1b[?1005h\x1b[?1006h\x1b[?1000h\x1b[?1000l");
        assert!(m.sgr && m.utf8 && !m.any());
        let m = mouse_after(b"\x1b[?1005h\x1b[?1006h\x1b[?1005l");
        assert!(m.sgr && !m.utf8);
        let m = mouse_after(b"\x1b[?1005h\x1b[?1006h\x1b[?1006l");
        assert!(!m.sgr && m.utf8);
    }

    #[test]
    fn mouse_modes_survive_alternate_screen_and_reset_on_ris() {
        let m = mouse_after(b"\x1b[?1002h\x1b[?1006h\x1b[?1049h");
        assert_eq!((m.tracking, m.sgr), (MouseTracking::Button, true));
        let m = mouse_after(b"\x1b[?1002h\x1b[?1049h\x1b[?1049l");
        assert_eq!(m.tracking, MouseTracking::Button);
        assert_eq!(
            mouse_after(b"\x1b[?1003h\x1b[?1005h\x1b[?1006h\x1bc"),
            MouseModes::default()
        );
    }

    #[test]
    fn mouse_sequences_tmux_ignores_change_nothing() {
        // Intermediate byte, or a sub-parameter: not tmux's private SM.
        assert!(!mouse_after(b"\x1b[?1000$h").any());
        assert!(!mouse_after(b"\x1b[?1000:1h").any());
        assert!(!mouse_after(b"\x1b[1000h").any());
        // Unknown modes beside a mouse mode do not stop it.
        assert_eq!(
            mouse_after(b"\x1b[?7;1003;2004h").tracking,
            MouseTracking::All
        );
    }

    #[test]
    fn csi_top_bit_bytes_are_ignored_not_fatal() {
        // 0x7f-0xff: no action, no state change (input.c:579, 597, 612, 628).
        for b in [&b"\xa0"[..], b"\x7f", b"\xff", b"\x80", b"\xc3\xa9"] {
            let seq = [&b"\x1b[?1000h\x1b[?1002"[..], b, b"h"].concat();
            let mut t = PaneTerm::new(20, 5, 100);
            t.feed(&seq);
            assert_eq!(t.mouse_modes().tracking, MouseTracking::Button, "{b:?}");
            // Nothing printed: Ghostty never saw the bytes.
            assert_eq!(t.row(0).length(), 0, "{b:?}");
        }
        assert_eq!(
            // Mid-number: the digits join, as in tmux's `param_buf`.
            mouse_after(b"\x1b[?100\xa02h").tracking,
            MouseTracking::Button
        );
    }

    /// DECRQM with more parameters answers for the first, as tmux's
    /// `input_get(ictx, 0, 0, 0)` (input.c:1638-1722): `CSI ?25;1004$p` is
    /// `?25;1$y`. No mode first (missing, 0, a string): no reply.
    #[test]
    fn decrqm_reads_only_the_first_parameter() {
        let mut t = PaneTerm::new(20, 3, 100);
        t.feed(b"\x1b[?25;1004$p\x1b[?1004;25$p\x1b[4;2$p\x1b[4h\x1b[4;1$p");
        assert_eq!(
            t.take_replies(),
            b"\x1b[?25;1$y\x1b[?1004;2$y\x1b[4;2$y\x1b[4;1$y"
        );
        t.feed(b"\x1b[?;25$p\x1b[?0;25$p\x1b[?1:2;25$p\x1b[5n");
        assert_eq!(t.take_replies(), b"\x1b[0n");
        t.feed(b"\x1b[?2048;25$p");
        assert_eq!(t.take_replies(), b"\x1b[?2048;0$y");
        t.set_inband_resize(true);
        t.feed(b"\x1b[?2048;25$pX");
        assert_eq!(t.take_replies(), b"\x1b[?2048;2$y");
        assert_eq!(captured(&t), "X");
    }

    #[test]
    fn decrqm_mouse_follows_tmux_bits() {
        // tmux answers 2 for 1000 after ?1000h?1002h (Ghostty: 1).
        let seq = b"\x1b[?1000h\x1b[?1002h";
        assert_eq!(decrqm(seq, 1000), "\x1b[?1000;2$y");
        assert_eq!(decrqm(seq, 1002), "\x1b[?1002;1$y");
        assert_eq!(decrqm(seq, 1003), "\x1b[?1003;2$y");
        let seq = b"\x1b[?1000h\x1b[?1002h\x1b[?1002l";
        assert_eq!(decrqm(seq, 1000), "\x1b[?1000;2$y");
        assert_eq!(decrqm(seq, 1002), "\x1b[?1002;2$y");
        assert_eq!(decrqm(b"\x1b[?1006h", 1006), "\x1b[?1006;1$y");
        assert_eq!(decrqm(b"\x1b[?1005h", 1005), "\x1b[?1005;1$y");
        assert_eq!(decrqm(b"\x1b[?1003h\x1bc", 1003), "\x1b[?1003;2$y");
    }

    /// A DCS is tmux's, not Ghostty's: a `tmux;` payload comes out for the
    /// server and nothing of it reaches the grid or the title, even though
    /// Ghostty would end the string at the first ESC.
    #[test]
    fn dcs_passthrough_is_taken_before_ghostty() {
        let mut t = PaneTerm::new(20, 3, 100);
        t.feed(b"A\x1bPtmux;\x1b\x1b]2;INNER\x07\x1b\x1b[31mXY\x1b\\B");
        let p = t.take_passthrough();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].data, b"\x1b]2;INNER\x07\x1b[31mXY");
        assert_eq!(t.title(), "");
        let mut plain = PaneTerm::new(20, 3, 100);
        plain.feed(b"AB");
        assert_eq!(snapshot(&t).cells, snapshot(&plain).cells);
        // Split across feeds, byte by byte.
        for b in b"\x1bPtmux;a\x1b\x1bb\x1b\\C" {
            t.feed(&[*b]);
        }
        assert_eq!(t.take_passthrough()[0].data, b"a\x1bb");
        // Other DCS strings are consumed and give nothing.
        t.feed(b"\x1bP1$rx\x1b\\\x1bPqabc\x1b\\");
        assert_eq!(t.take_passthrough().len(), 0);
    }

    #[test]
    fn dcs_ended_by_a_new_escape_sequence() {
        let mut t = PaneTerm::new(20, 3, 100);
        // ESC [ inside a DCS (outside its payload) starts a CSI.
        t.feed(b"\x1bP1:\x1b[31mR");
        assert_eq!(t.take_passthrough().len(), 0);
        assert_eq!(t.cursor(), (1, 0));
    }

    #[test]
    fn tmux_identity_replies() {
        assert_eq!(replies_to(20, 5, b"\x1b[c"), b"\x1b[?1;2c");
        assert_eq!(replies_to(20, 5, b"\x1b[>c"), b"\x1b[>84;0;0c");
        assert_eq!(replies_to(20, 5, b"\x1b[5n"), b"\x1b[0n");
        assert_eq!(replies_to(20, 5, b"\x1b[>q"), b"\x1bP>|tmux 3.8\x1b\\");
        assert_eq!(
            replies_to(20, 5, b"\x1b[0c\x1b[>0c\x1b[>0q"),
            b"\x1b[?1;2c\x1b[>84;0;0c\x1bP>|tmux 3.8\x1b\\"
        );
    }

    /// input.c:1582-1608, 1885-1890: DA, DA2 and XTVERSION answer only
    /// parameter 0, and DECID (`ESC Z`) is not in tmux's ESC table.
    /// Reference tmux 3.8: none of these is answered.
    #[test]
    fn identity_queries_with_other_parameters_are_not_answered() {
        for q in [
            &b"\x1b[1c"[..],
            b"\x1b[>1c",
            b"\x1b[>1q",
            b"\x1b[1;0c",
            b"\x1b[:c",
            b"\x1bZ",
        ] {
            let mut t = PaneTerm::new(20, 5, 100);
            t.feed(q);
            t.feed(b"\x1b[5n");
            assert_eq!(t.take_replies(), b"\x1b[0n", "{q:?}");
        }
        // The sequence is consumed: nothing of it prints.
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"a\x1bZb\x1b[1cc");
        assert_eq!(t.cursor(), (3, 0));
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

    /// D29 off: tmux's CSI table has no `u` with a prefix (input.c:347), so
    /// a kitty keyboard push, pop, set or query changes nothing, prints
    /// nothing and gets no reply.
    #[test]
    fn kitty_keyboard_off_is_ignored() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"a\x1b[>1ub\x1b[=5;1uc\x1b[?ud\x1b[<ue\x1b[5n");
        assert_eq!(t.take_replies(), b"\x1b[0n");
        assert_eq!(t.kitty_keys(), 0);
        assert_eq!(t.cursor(), (5, 0));
        // Turned on later, the earlier push is not there: the program
        // never had it.
        t.set_kitty_keys(true);
        t.feed(b"\x1b[?u");
        assert_eq!(t.take_replies(), b"\x1b[?0u");
        assert_eq!(t.kitty_keys(), 0);
    }

    /// D29 on: the query is answered with the active screen's flags, of
    /// those vtmux applies (1, 4 and 8); push, pop and set follow Ghostty's
    /// stack.
    #[test]
    fn kitty_keyboard_on_follows_the_stack() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_kitty_keys(true);
        let step = |t: &mut PaneTerm, bytes: &[u8]| {
            t.feed(bytes);
            t.feed(b"\x1b[?u");
            (String::from_utf8(t.take_replies()).unwrap(), t.kitty_keys())
        };
        assert_eq!(step(&mut t, b""), ("\x1b[?0u".into(), 0));
        assert_eq!(step(&mut t, b"\x1b[>1u"), ("\x1b[?1u".into(), 1));
        assert_eq!(step(&mut t, b"\x1b[>31u"), ("\x1b[?13u".into(), 13));
        assert_eq!(step(&mut t, b"\x1b[=4;3u"), ("\x1b[?9u".into(), 9));
        assert_eq!(step(&mut t, b"\x1b[<u"), ("\x1b[?1u".into(), 1));
        assert_eq!(step(&mut t, b"\x1b[=8;2u"), ("\x1b[?9u".into(), 9));
        assert_eq!(step(&mut t, b"\x1b[<u"), ("\x1b[?0u".into(), 0));
        // Nothing printed, the cursor where it was.
        assert_eq!(t.cursor(), (0, 0));
    }

    /// D29 rule 5: a granted screen keeps its flags, and its own push, pop
    /// and set, after the option is turned off; once its stack is back to
    /// 0 the grant is over and a push is ignored again.
    #[test]
    fn kitty_keyboard_grant_outlives_the_option() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_kitty_keys(true);
        t.feed(b"\x1b[>1u");
        t.set_kitty_keys(false);
        assert_eq!(t.kitty_keys(), 1);
        t.feed(b"\x1b[>9u\x1b[?u");
        assert_eq!(t.take_replies(), b"\x1b[?9u");
        t.feed(b"\x1b[<2u");
        assert_eq!(t.kitty_keys(), 0);
        t.feed(b"\x1b[>1u\x1b[?u\x1b[5n");
        assert_eq!(t.take_replies(), b"\x1b[0n");
        assert_eq!(t.kitty_keys(), 0);
    }

    /// D29: each screen has its own stack and grant (Ghostty's per-screen
    /// `kitty_keyboard`), so leaving the alternate screen gives the shell
    /// its own flags back; RIS empties both.
    #[test]
    fn kitty_keyboard_stack_per_screen_and_ris() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_kitty_keys(true);
        t.feed(b"\x1b[>1u\x1b[?1049h");
        assert_eq!(t.kitty_keys(), 0);
        t.feed(b"\x1b[>8u");
        assert_eq!(t.kitty_keys(), 8);
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.kitty_keys(), 1);
        // Off, the alternate screen (still at 8) is granted, the primary
        // too: RIS empties the screen in use and then the other as it
        // comes up.
        t.set_kitty_keys(false);
        t.feed(b"\x1bc");
        assert_eq!(t.kitty_keys(), 0);
        t.feed(b"\x1b[?1049h");
        assert_eq!(t.kitty_keys(), 0);
        t.feed(b"\x1b[>1u\x1b[?u\x1b[5n");
        assert_eq!(t.take_replies(), b"\x1b[0n");
        assert_eq!(t.kitty_keys(), 0);
    }

    /// D29: a respawn's [`PaneTerm::reset_kitty_keys`] under the alternate
    /// screen: the primary screen's stack, which Ghostty still holds, is
    /// exported empty (a live upgrade carries no stale grant) and emptied
    /// when the screen comes up.
    #[test]
    fn kitty_keyboard_reset_empties_the_other_screen() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_kitty_keys(true);
        t.feed(b"\x1b[>1u\x1b[?1049h\x1b[>4u");
        assert_eq!(t.export_state().primary.kitty_flags[1], 1);
        t.reset_kitty_keys();
        assert_eq!(t.kitty_keys(), 0);
        let state = t.export_state();
        assert_eq!(state.primary.kitty_flags, [0; 8]);
        assert_eq!(state.alternate.as_ref().unwrap().kitty_flags, [0; 8]);
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.kitty_keys(), 0);
        // A granted screen's stack goes through an export and import.
        t.feed(b"\x1b[>5u");
        let back = PaneTerm::import_state(&t.export_state()).unwrap();
        assert_eq!(back.kitty_keys(), 5);
    }

    /// D29: the screen a push granted is named once
    /// ([`PaneTerm::take_kitty_grants`]); a second push, a pop that leaves
    /// flags and a refused push (option off, not granted) name none.
    #[test]
    fn kitty_keyboard_grants_are_noted() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"\x1b[>1u");
        assert_eq!(t.take_kitty_grants(), [false; 2], "refused while off");
        t.set_kitty_keys(true);
        t.feed(b"\x1b[>1u");
        assert_eq!(t.take_kitty_grants(), [true, false]);
        assert_eq!(t.take_kitty_grants(), [false; 2], "taken");
        t.feed(b"\x1b[>4u\x1b[<u");
        assert_eq!(t.take_kitty_grants(), [false; 2], "already granted");
        t.feed(b"\x1b[?1049h\x1b[>8u");
        assert_eq!(t.take_kitty_grants(), [false, true]);
        t.feed(b"\x1b[<u\x1b[=1;1u");
        assert_eq!(t.take_kitty_grants(), [false, true], "a set from 0 grants");
    }

    /// D29: ending one screen's grant empties that screen's stack only,
    /// at once for the screen in use and when the other comes up, and not
    /// in the middle of a sequence or character, which the cancel would
    /// cut.
    #[test]
    fn ending_a_kitty_grant_empties_that_screen_only() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_kitty_keys(true);
        t.feed(b"\x1b[>1u\x1b[?1049h\x1b[>4u");
        assert_eq!(t.kitty_keys(), 4);
        // The primary screen's, not in use: its stack is exported empty.
        assert!(t.end_kitty_grant(false));
        assert_eq!(t.kitty_keys(), 4, "the alternate screen keeps its own");
        assert_eq!(t.export_state().primary.kitty_flags, [0; 8]);
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.kitty_keys(), 0);
        // The screen in use, but not inside a sequence or a character.
        t.feed(b"\x1b[>1u\x1b[3");
        assert_eq!(t.kitty_keys(), 1);
        assert!(!t.end_kitty_grant(false), "inside a CSI");
        assert_eq!(t.kitty_keys(), 1);
        t.feed(b"1mx\xe4\xb8");
        assert!(!t.end_kitty_grant(false), "inside a character");
        t.feed(b"\x96");
        assert!(t.end_kitty_grant(false));
        assert_eq!(t.kitty_keys(), 0);
        // `x` and a width 2 character: nothing of the CSI printed.
        assert_eq!(t.cursor(), (3, 0), "no output was cut");
    }

    /// D31 with `vtmux-inband-resize` off: mode 2048 is tmux's, unknown
    /// (DECRQM 0, input.c:1651-1722; the set ignored, input.c:2022). A set
    /// while off grants nothing when the option is turned on later.
    #[test]
    fn inband_resize_off_is_tmux() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"\x1b[?2048$p\x1b[?2048h\x1b[?2048$p");
        assert_eq!(t.take_replies(), b"\x1b[?2048;0$y\x1b[?2048;0$y");
        t.set_inband_resize(true);
        t.feed(b"\x1b[?2048$p");
        assert_eq!(t.take_replies(), b"\x1b[?2048;2$y");
        assert!(!t.inband_resize_granted());
        assert_eq!(t.inband_resize_report(30, 6), None);
    }

    /// D31 with it on: DECRQM 2, then a set grants and reports at once
    /// (Ghostty's `reportMode2048`), in order with the other replies, and
    /// DECRQM says 1. The grant outlives the option (D28 rule 5) and ends
    /// at `?2048l`, RIS or a respawn.
    #[test]
    fn inband_resize_grant() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_inband_resize(true);
        t.feed(b"\x1b[?2048$p\x1b[5n\x1b[?2048h\x1b[?2048$p");
        assert_eq!(
            t.take_replies(),
            b"\x1b[?2048;2$y\x1b[0n\x1b[48;5;20;160;320t\x1b[?2048;1$y"
        );
        assert_eq!(t.cursor(), (0, 0), "nothing printed");
        assert_eq!(
            t.inband_resize_report(30, 6).unwrap(),
            b"\x1b[48;6;30;192;480t"
        );
        // Turned off: the program keeps it, and a set reports again.
        t.set_inband_resize(false);
        t.feed(b"\x1b[?2048$p\x1b[?2048h");
        assert_eq!(t.take_replies(), b"\x1b[?2048;1$y\x1b[48;5;20;160;320t");
        t.feed(b"\x1b[?2048l\x1b[?2048$p\x1b[?2048h");
        assert_eq!(t.take_replies(), b"\x1b[?2048;0$y");
        assert_eq!(t.inband_resize_report(30, 6), None);
        // Among modes tmux knows, which still apply.
        t.set_inband_resize(true);
        t.feed(b"\x1b[?25l\x1b[?25;2048h\x1b[?25$p");
        assert_eq!(t.take_replies(), b"\x1b[48;5;20;160;320t\x1b[?25;1$y");
        // RIS ends it.
        t.feed(b"\x1bc\x1b[?2048$p");
        assert_eq!(t.take_replies(), b"\x1b[?2048;2$y");
        // So does a respawn.
        t.feed(b"\x1b[?2048h");
        assert!(t.inband_resize_granted());
        t.end_inband_resize();
        t.feed(b"\x1b[?2048$p");
        assert_eq!(t.take_replies(), b"\x1b[48;5;20;160;320t\x1b[?2048;2$y");
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
        // input.c:2166-2186: 15 and 19 too, and every report in one
        // sequence, in order (reference tmux 3.8 answers `CSI 19 t` with
        // `CSI 9 ; rows ; cols t`).
        assert_eq!(replies_to(20, 5, b"\x1b[15t"), b"\x1b[5;160;320t");
        assert_eq!(replies_to(20, 5, b"\x1b[19t"), b"\x1b[9;5;20t");
        assert_eq!(
            replies_to(20, 5, b"\x1b[18;1;16t"),
            b"\x1b[8;5;20t\x1b[6;32;16t"
        );
        // A report's position read as another's argument is not one.
        assert_eq!(replies_to(20, 5, b"\x1b[9;18t\x1b[3;1;18t"), b"");
        assert_eq!(replies_to(20, 5, b"\x1b[3;1t\x1b[18t"), b"\x1b[8;5;20t");
    }

    /// D-014: the reports use the window's cell size once the server gives
    /// it (input.c:2163-2180 read `w->xpixel`, `w->ypixel`), mode 2048's
    /// too; a zero is tmux's default on that axis.
    #[test]
    fn size_reports_use_the_cell_size_given() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_cell_pixels((10, 21));
        t.feed(b"\x1b[14t\x1b[15t\x1b[16t");
        assert_eq!(
            t.take_replies(),
            b"\x1b[4;105;200t\x1b[5;105;200t\x1b[6;21;10t"
        );
        t.set_inband_resize(true);
        t.feed(b"\x1b[?2048h");
        assert_eq!(t.take_replies(), b"\x1b[48;5;20;105;200t");
        assert_eq!(
            t.inband_resize_report(30, 6).unwrap(),
            b"\x1b[48;6;30;126;300t"
        );
        t.set_cell_pixels((0, 7));
        assert_eq!(t.cell_pixels(), (16, 7));
        t.feed(b"\x1b[16t");
        assert_eq!(t.take_replies(), b"\x1b[6;7;16t");
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

    /// REP repeats only a character printed right before it (tmux's
    /// `INPUT_LAST`), whole buffers at a time so the printable runs take
    /// the fast path. Reference tmux 3.8, 20x5, `capture-pane -p`.
    #[test]
    fn rep_repeats_only_a_character_just_printed() {
        let screen = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 5, 100);
            t.feed(bytes);
            captured(&t)
        };
        assert_eq!(screen(b"ab\x1b[3bz"), "abbbbz");
        assert_eq!(screen(b"ab\x1b[31m\x1b[3bz"), "abz");
        assert_eq!(screen(b"ab\r\x1b[3bz"), "zb");
        assert_eq!(screen(b"ab\x1b7\x1b[3bz"), "abz");
        assert_eq!(screen(b"ab\x1b]2;t\x07\x1b[3bz"), "abz");
        assert_eq!(screen(b"ab\x1b[2b\x1b[2bz"), "abbbz");
        assert_eq!(screen("a\u{e9}\x1b[2bz".as_bytes()), "a\u{e9}\u{e9}\u{e9}z");
        // Sequences tmux does not know dispatch nothing and keep it.
        assert_eq!(screen(b"ab\x1bA\x1b[2bz"), "abbbz");
        assert_eq!(screen(b"ab\x1b[1x\x1b[2bz"), "abbbz");
        // A respawned pane starts with nothing to repeat.
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"ab");
        t.reset_input();
        t.feed(b"\x1b[2bz");
        assert_eq!(captured(&t), "abz");
    }

    /// DECCOLM homes the cursor and clears the screen as ED 2 does, into
    /// history under scroll-on-clear. Reference tmux 3.8, 20x5.
    #[test]
    fn deccolm_clears_the_screen() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"l0\r\nl1\x1b[3;5H\x1b[?3hX");
        assert_eq!((t.history_size(), t.cursor()), (2, (1, 0)));
        assert_eq!(captured(&t), "l0\nl1\nX");
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"l0\r\nl1\x1b[2;4r\x1b[?6h\x1b[?3lX");
        assert_eq!((t.history_size(), t.cursor()), (2, (1, 1)));
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"l0\x1b[?1049hab\x1b[?3hX");
        assert_eq!((t.history_size(), t.cursor()), (0, (1, 0)));
        assert_eq!(captured(&t), "X");
    }

    /// The whole capture (`-S - -E -`), trailing blank lines dropped.
    fn captured(t: &PaneTerm) -> String {
        let opts = crate::capture::CaptureOpts {
            start: crate::capture::LineRef::Start,
            end: crate::capture::LineRef::End,
            ..Default::default()
        };
        let text = String::from_utf8_lossy(&crate::capture::capture(t, opts)).into_owned();
        text.trim_end_matches('\n').to_owned()
    }

    /// Bytes 0x7f-0xff after an ESC, or an ESC and intermediates, are no
    /// action and no state change (input.c `input_state_esc_enter_table`,
    /// `input_state_esc_intermediate_table`): the next 0x30-0x7e ends the
    /// escape. Ghostty would print U+FFFD and go back to the ground state.
    #[test]
    fn high_bytes_in_an_escape_are_ignored() {
        let after = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 3, 100);
            t.feed(bytes);
            (captured(&t), t.cursor())
        };
        // →│ then `e` (ESC e, unknown) and `x` printed.
        assert_eq!(
            after("\x1b\u{2192}\u{2502}ex".as_bytes()),
            ("x".to_owned(), (1, 0))
        );
        assert_eq!(after(b"\x1b\x7f\x80Dy"), ("\ny".to_owned(), (1, 1)));
        // ESC ( é 0: G0 is the line drawing set.
        let mut t = PaneTerm::new(20, 3, 100);
        t.feed("a\x1b(\u{e9}0q".as_bytes());
        assert_eq!((captured(&t), t.cursor()), ("aq".to_owned(), (2, 0)));
        assert!(crate::snapshot(&t).cell(1, 0).unwrap().acs);
    }

    /// An escape tmux's `input_esc_table` lacks dispatches nothing
    /// (input.c `input_esc_dispatch`): DECBI and DECFI do not move, SS2,
    /// SS3, the G2/G3 designations and the national sets change no
    /// character, two intermediates match no entry, DECID gets no reply.
    #[test]
    fn escapes_tmux_lacks_do_nothing() {
        let after = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 3, 100);
            t.feed(bytes);
            let acs = crate::snapshot(&t).cell(0, 0).unwrap().acs;
            (captured(&t), t.cursor(), acs, t.take_replies())
        };
        let plain = |s: &str, x: u16| (s.to_owned(), (x, 0), false, Vec::new());
        assert_eq!(after(b"ab\x1b6c"), plain("abc", 3));
        assert_eq!(after(b"\x1b9pa"), plain("pa", 2));
        assert_eq!(after(b"\x1b*0\x1bNq"), plain("q", 1));
        assert_eq!(after(b"\x1b+0\x1bOq"), plain("q", 1));
        assert_eq!(after(b"\x1b*0\x1bnq"), plain("q", 1));
        assert_eq!(after(b"\x1b(A#"), plain("#", 1));
        assert_eq!(after(b"\x1b((0q"), plain("q", 1));
        assert_eq!(after(b"\x1bZx"), plain("x", 1));
        // What tmux knows still works.
        assert_eq!(
            after(b"\x1b(0q"),
            ("q".to_owned(), (1, 0), true, Vec::new())
        );
    }

    /// A CSI tmux's `input_csi_table` lacks, or with a mode tmux's SM and
    /// RM do not know, changes nothing (input.c `input_csi_dispatch`,
    /// `input_csi_dispatch_sm`, `_sm_private`, `_rm`, `_rm_private`):
    /// LNM, reverse wrap, DECLRMM (so `CSI s` saves the cursor), mode
    /// 1048, XTSAVE and XTRESTORE, DECSTR, DA3. Known modes of a mixed
    /// list still apply.
    #[test]
    fn csi_tmux_lacks_does_nothing() {
        let after = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 3, 100);
            t.feed(bytes);
            (captured(&t), t.cursor())
        };
        assert_eq!(after(b"\x1b[20ha\x0bb"), ("a\n b".to_owned(), (2, 1)));
        assert_eq!(after(b"ab\r\n\x1b[?45h\x08x"), ("ab\nx".to_owned(), (1, 1)));
        assert_eq!(
            after(b"\x1b[?69habc\x1b[sdef\x1b[ux"),
            ("abcxef".to_owned(), (4, 0))
        );
        assert_eq!(
            after(b"abc\x1b[?1048hdef\x1b[?1048lx"),
            ("abcdefx".to_owned(), (7, 0))
        );
        assert_eq!(
            after(b"ab\r\x1b[20;4hX\x0bY"),
            ("Xab\n Y".to_owned(), (2, 1))
        );
        let modes = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 3, 100);
            t.feed(bytes);
            (t.screen_modes(), t.take_replies(), t.scroll_region())
        };
        let (m, ..) = modes(b"\x1b[?7s\x1b[?7l\x1b[?7r");
        assert!(!m.wrap);
        let (m, _, region) = modes(b"\x1b[?25l\x1b[4h\x1b[2;3r\x1b[!p");
        assert_eq!((m.cursor_visible, m.insert, region), (false, true, (1, 2)));
        assert_eq!(modes(b"\x1b[=c\x1b[>1u\x1b[?u").1, Vec::<u8>::new());
    }

    /// DECALN fills the screen with default cells holding E, puts the
    /// cursor top left and resets the region, and leaves the pen and origin
    /// mode alone (`screen_write_alignmenttest`; patch 0031).
    #[test]
    fn decaln_writes_default_cells() {
        let mut t = PaneTerm::new(4, 3, 0);
        t.feed(b"\x1b[2;3r\x1b[?6h\x1b[1;31m\x1b#8X");
        let opts = crate::capture::CaptureOpts {
            escapes: true,
            ..Default::default()
        };
        let text = String::from_utf8_lossy(&crate::capture::capture(&t, opts)).into_owned();
        assert_eq!(text, "\x1b[1m\x1b[31mX\x1b[0mEEE\nEEEE\nEEEE\n");
        let m = t.screen_modes();
        assert_eq!(
            (m.origin, t.scroll_region(), t.cursor()),
            (true, (0, 2), (1, 0))
        );
    }

    /// The forms that go to Ghostty untouched, and every other one goes
    /// through `tmux_sgr` (`sgr_reads_as_tmux`).
    #[test]
    fn plain_sgr_forms() {
        for raw in [
            "0",
            "1;31",
            "38;5;208",
            "48;2;1;2;3;1",
            "58;5;0",
            "97;107;53",
        ] {
            assert!(sgr_is_plain(raw.as_bytes()), "{raw}");
        }
        for raw in [
            "01", "5;", ";1", "6", "38;5", "38;5;300", "48;1", "4:3", "26", "38;2;1;2",
        ] {
            assert!(!sgr_is_plain(raw.as_bytes()), "{raw}");
        }
    }

    /// `CSI s` saves the cursor whatever its parameters (input.c
    /// `INPUT_CSI_SCP`); Ghostty took one with parameters for DECSLRM.
    #[test]
    fn scp_ignores_its_parameters() {
        let after = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 3, 100);
            t.feed(bytes);
            (captured(&t), t.cursor())
        };
        assert_eq!(after(b"o\x1b[0s\x1b[u7"), ("o7".to_owned(), (2, 0)));
        assert_eq!(after(b"ab\x1b[1;2scd\x1b[ux"), ("abxd".to_owned(), (3, 0)));
        assert_eq!(after(b"ab\x1b[1:2scd\x1b[ux"), ("abxd".to_owned(), (3, 0)));
    }

    /// SGR as tmux reads it (input.c `input_csi_dispatch_sgr`,
    /// `_sgr_colon`, `_sgr_256`, `_sgr_rgb`): a missing parameter is 0, 38,
    /// 48 and 58 take the next one as their kind whatever it is, a 256
    /// colour out of range is the default, an RGB one out of range sets
    /// nothing and leaves its parameters to be read as more SGR, a colon
    /// form takes its own counts. Expected: tmux 3.8's `capture-pane -e`.
    #[test]
    fn sgr_reads_as_tmux() {
        let cases: &[(&str, &str)] = &[
            ("5;", "X"),
            ("7;", "X"),
            ("48;1", "X"),
            ("1;;3", "\x1b[3mX\x1b[0m"),
            ("38:2:::3", "X"),
            ("48:2::1:2:3", "\x1b[48;2;1;2;3mX\x1b[49m"),
            ("38:2:9:1:2:3:4", "\x1b[38;2;1;2;3mX\x1b[39m"),
            ("38;5", "X"),
            ("38;5;300", "X"),
            ("48;5;300;1", "\x1b[1mX\x1b[0m"),
            ("38;2;300;1;3", "\x1b[1;3mX\x1b[0m"),
            ("38;7;1", "\x1b[1mX\x1b[0m"),
            ("4:3", "\x1b[4:3mX\x1b[0m"),
            ("4:9", "X"),
            ("6", "\x1b[5mX\x1b[0m"),
            ("01;034", "\x1b[1m\x1b[34mX\x1b[0m"),
            ("38:5:9:9", "\x1b[38;5;9mX\x1b[39m"),
            ("38:2:300:1:1", "X"),
            (
                "38;5;1;48;2;1;2;3;3",
                "\x1b[3m\x1b[38;5;1m\x1b[48;2;1;2;3mX\x1b[0m",
            ),
            ("97;107", "\x1b[97m\x1b[107mX\x1b[39m\x1b[49m"),
            ("1;0;3", "\x1b[3mX\x1b[0m"),
        ];
        for (sgr, want) in cases {
            let mut t = PaneTerm::new(20, 2, 0);
            t.feed(format!("\x1b[1;4m\x1b[0m\x1b[{sgr}mX\x1b[m").as_bytes());
            let opts = crate::capture::CaptureOpts {
                escapes: true,
                ..Default::default()
            };
            let text = String::from_utf8_lossy(&crate::capture::capture(&t, opts)).into_owned();
            assert_eq!(text.lines().next(), Some(*want), "SGR {sgr:?}");
        }
    }

    /// Reference: 20x5 after `l0\nl1\nl2`, cursor at 4,2 (row 2 col 4):
    /// ED 2 scrolls the three rows into history and leaves the cursor; ED 0
    /// at home does the same; ED 0 elsewhere, ED 22 and a pane without
    /// history only erase (`scroll-on-clear`, screen-write.c:1936, 2091).
    #[test]
    fn clears_scroll_into_history_like_tmux() {
        let after = |limit: usize, clear: &[u8]| {
            let mut t = PaneTerm::new(20, 5, limit);
            t.feed(b"l0\r\nl1\r\nl2\x1b[2;4H");
            t.feed(clear);
            t.feed(b"X");
            (t.history_size(), t.cursor(), captured(&t))
        };
        let kept = "l0\nl1\nl2\n\n   X".to_owned();
        assert_eq!(after(100, b"\x1b[2J"), (3, (4, 1), kept));
        let home = "l0\nl1\nl2\nX".to_owned();
        assert_eq!(after(100, b"\x1b[H\x1b[J"), (3, (1, 0), home.clone()));
        assert_eq!(after(100, b"\x1b[H\x1b[0J"), (3, (1, 0), home));
        assert_eq!(after(100, b"\x1b[J"), (0, (4, 1), "l0\nl1 X".to_owned()));
        assert_eq!(after(0, b"\x1b[2J"), (0, (4, 1), "\n   X".to_owned()));
        let ed22 = "l0\nl1 X\nl2".to_owned();
        assert_eq!(after(100, b"\x1b[22J"), (0, (4, 1), ed22));
        let mut off = PaneTerm::new(20, 5, 100);
        off.set_scroll_on_clear(false);
        off.feed(b"l0\r\nl1\x1b[2J");
        assert_eq!(off.history_size(), 0);
    }

    /// Reference: RIS is `screen_write_reset`: history kept, the used rows
    /// scroll into it, the alternate screen stays (D-025 closed).
    #[test]
    fn ris_keeps_history_and_the_alternate_screen() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.feed(b"h1\r\nh2\r\nh3\r\nh4\r\nh5\r\nv1\r\nv2\r\n\x1bc");
        assert_eq!(t.history_size(), 7);
        assert_eq!(t.cursor(), (0, 0));
        let mut alt = PaneTerm::new(20, 5, 100);
        alt.feed(b"main\r\n\x1b[?1049halt\x1bc");
        assert!(alt.alternate_on());
    }

    /// What the program did comes back in input order: titles, BELs (in a
    /// row counted as one entry), and OSC 133's prompt and command events,
    /// on the alternate screen too, with `D`'s exit status as
    /// `input_osc_133_exit_status` reads it (input.c:3196-3328).
    #[test]
    fn events_come_in_input_order() {
        let mut t = PaneTerm::new(20, 3, 100);
        t.feed(b"\x07\x1b]2;one\x07\x07\x07\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07");
        t.feed(b"\x1b]133;P;k=s\x07\x1b[?1049h\x1b]133;D;300\x07\x1b]133;N\x1b\\\x1b]133;D\x07");
        assert_eq!(
            t.take_events(),
            [
                TermEvent::Bells(1),
                TermEvent::Title("one".into()),
                TermEvent::Bells(2),
                TermEvent::Prompt,
                TermEvent::CommandStarted,
                TermEvent::CommandFinished(255),
                TermEvent::Prompt,
                TermEvent::CommandFinished(0),
            ]
        );
        assert_eq!(t.take_events(), Vec::<TermEvent>::new());
    }

    /// One read with more events than any cap keeps them all: tmux fires
    /// each as it parses, so a bell and title flood reaches every hook.
    #[test]
    fn a_read_keeps_every_event() {
        let mut t = PaneTerm::new(20, 3, 100);
        t.feed(&b"\x1b]2;t\x07x\x07".repeat(20_000));
        let events = t.take_events();
        let bells: u32 = events
            .iter()
            .map(|e| match e {
                TermEvent::Bells(n) => *n,
                _ => 0,
            })
            .sum();
        let titles = events
            .iter()
            .filter(|e| matches!(e, TermEvent::Title(_)))
            .count();
        assert_eq!((titles, bells), (20_000, 20_000));
        assert_eq!(events.len(), 40_000);
    }

    /// Reference: OSC 133;A only marks the line (no fresh line), a BEL
    /// ending an OSC 133 is no bell, and an OSC 8 tmux rejects links
    /// nothing (input.c `input_osc_133`, `input_osc_8`).
    #[test]
    fn oscs_tmux_reads_differently() {
        let mut t = PaneTerm::new(20, 3, 100);
        t.feed(b"xx\x1b]133;A\x07p>\x1b]133;B\x1b\\cmd");
        assert_eq!(captured(&t), "xxp>cmd");
        assert!(
            !t.take_events()
                .iter()
                .any(|e| matches!(e, TermEvent::Bells(_)))
        );
        assert_eq!(t.line_marks(), [(0, LineMark::Prompt)]);
        assert!(hyperlink_ok(b";http://x"));
        assert!(hyperlink_ok(b"id=a;http://x"));
        assert!(hyperlink_ok(b"a=b:id=c;"));
        assert!(!hyperlink_ok(b"id=a:id=b;http://x"));
        assert!(!hyperlink_ok(b"id=no-separator"));
        let mut l = PaneTerm::new(20, 3, 100);
        l.feed(b"\x1b]8;id=a:id=b;http://bad\x07X\x1b]8;;http://ok/\x07Y");
        let cell = |x| {
            let point = Point::Active(PointCoordinate { x, y: 0 });
            l.term
                .grid_ref(point)
                .ok()
                .and_then(|g| g.hyperlink_id().ok())
        };
        assert_eq!(
            cell(0),
            Some(libghostty_vt::screen::HyperlinkId::None),
            "X is not linked"
        );
        assert_ne!(
            cell(1),
            Some(libghostty_vt::screen::HyperlinkId::None),
            "Y is linked"
        );
    }

    /// Reference: tmux's UTF-8 decoder makes one U+FFFD of a bad
    /// sequence, where Ghostty makes one per byte; DEL and CHT do nothing.
    #[test]
    fn bytes_tmux_decodes_differently() {
        let line = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 3, 100);
            t.feed(bytes);
            (captured(&t), t.cursor().0)
        };
        let r = "\u{FFFD}";
        assert_eq!(line(b"x\xed\xa0\x80y"), (format!("x{r}y"), 3));
        assert_eq!(line(b"x\xe0\x80\xafy"), (format!("x{r}y"), 3));
        assert_eq!(line(b"x\xc3\xc3Ay"), (format!("x{r}Ay"), 4));
        assert_eq!(line(b"x\xe2\x82y"), (format!("x{r}y"), 3));
        assert_eq!(line(b"x\xc3\x1b[1CA"), (format!("x {r}A"), 4));
        assert_eq!(line(b"x\xe2\x82\x1b[1C\xac"), ("x \u{20AC}".to_owned(), 3));
        assert_eq!(line(b"x\xe2\x82"), ("x".to_owned(), 1));
        assert_eq!(line(b"ab\x7fc"), ("abc".to_owned(), 3));
        assert_eq!(line(b"ab\x1b[2Ix"), ("abx".to_owned(), 3));
    }

    /// Reference: 20x3, `x` + the cluster + `y`, `#{cursor_x}`.
    #[test]
    fn graphemes_cluster_like_tmux() {
        let cursor_after = |bytes: &[u8]| {
            let mut t = PaneTerm::new(20, 3, 100);
            t.feed(bytes);
            t.cursor().0
        };
        let flag = "x\u{1F1EC}\u{1F1E7}y".as_bytes();
        assert_eq!(cursor_after(flag), 4);
        assert_eq!(cursor_after("x\u{1F44D}\u{1F3FB}y".as_bytes()), 4);
        assert_eq!(cursor_after("x\u{1F468}\u{200D}\u{1F469}y".as_bytes()), 4);
        assert_eq!(cursor_after("x\u{2764}\u{FE0F}y".as_bytes()), 4);
        // A program cannot turn it off: tmux has no mode 2027, and RIS
        // keeps tmux's clustering too.
        let mut off = b"\x1b[?2027l".to_vec();
        off.extend_from_slice(flag);
        assert_eq!(cursor_after(&off), 4);
        let mut ris = b"\x1bc".to_vec();
        ris.extend_from_slice(flag);
        assert_eq!(cursor_after(&ris), 4);
    }

    #[test]
    fn cursor_without_autowrap_stays_on_the_last_column() {
        // Reference: 5x3, `printf '\033[?7labcdeF'`, `#{cursor_x}` 4.
        let mut t = PaneTerm::new(5, 3, 100);
        t.feed(b"\x1b[?7labcdeF");
        assert_eq!(t.cursor(), (4, 0));
        t.feed(b"\x1b[6n");
        assert_eq!(t.take_replies(), b"\x1b[1;5R");
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
        assert!(t.set_title("a#(b)\\c"));
        assert_eq!(t.title(), "a#(b)\\\\c");
        t.feed(b"x");
        assert_eq!(t.title(), "a#(b)\\\\c");
        t.feed(b"\x1b]2;next\x07");
        assert_eq!(t.title(), "next");
        // Reference: `select-pane -T` of a title with a tab or DEL leaves
        // the title (clean_name fails utf8_isvalid).
        assert!(!t.set_title("a\tb"));
        assert!(!t.set_title("a\x7fb"));
        assert_eq!(t.title(), "next");
    }

    /// Reference (tmux 3.8 on the same input): an APC, PM or SOS takes
    /// every byte from 0x20 to 0xff as text (input.c
    /// `input_state_apc_string_table` and the others), so a C1 byte inside
    /// (0x9c, a continuation byte of "本") neither ends it nor starts
    /// anything. Ghostty would take it as ST, a CSI or an OSC
    /// (`consumeApcString`) and print the rest.
    #[test]
    fn a_c1_byte_inside_an_apc_pm_or_sos_is_text() {
        // Invalid UTF-8 is refused as a title (`utf8_isvalid`).
        for (input, title) in [
            (&b"\x1b_\xe6\x97\xa5\xe6\x9c\xac\x1b\\X"[..], "日本"),
            (b"\x1b_abc\x9cdef\x1b\\X", ""),
            (b"\x1b_a\x9b31mb\x9d2;c\x1b\\X", ""),
            (b"\x1b^\xe6\x9c\xac\x1b\\X", ""),
            (b"\x1bX\xe6\x9c\xac\x1b\\X", ""),
        ] {
            let mut t = PaneTerm::new(20, 5, 100);
            t.feed(input);
            assert_eq!(t.row(0).text(), "X", "{input:?}");
            assert_eq!(t.cursor(), (1, 0), "{input:?}");
            assert_eq!(t.title(), title, "{input:?}");
        }
    }

    /// Reference (tmux 3.8 on the same input): the OSC number is read as
    /// tmux's `input_exit_osc` reads it (`02` is 2, no `;` is an empty
    /// title, a `u_int` that wraps), CAN and SUB end the OSC and apply it,
    /// an APC string is a title too (a BEL inside is part of it), and
    /// `allow-set-title` off keeps the title. Each title is one event.
    #[test]
    fn program_titles_as_tmux_reads_them() {
        let mut t = PaneTerm::new(20, 5, 100);
        let title = |t: &mut PaneTerm, bytes: &[u8]| {
            t.take_events();
            t.feed(bytes);
            let events = t.take_events();
            (t.title(), events)
        };
        let ev = |s: &str| vec![TermEvent::Title(s.to_owned())];
        assert_eq!(
            title(&mut t, b"\x1b]02;zero\x07"),
            ("zero".into(), ev("zero"))
        );
        assert_eq!(title(&mut t, b"\x1b]2\x07"), (String::new(), ev("")));
        assert_eq!(
            title(&mut t, b"\x1b]4294967298;w\x1b\\"),
            ("w".into(), ev("w"))
        );
        assert_eq!(title(&mut t, b"\x1b]0;can\x18x"), ("can".into(), ev("can")));
        assert_eq!(title(&mut t, b"\x1b]1;icon\x07"), ("can".into(), vec![]));
        assert_eq!(title(&mut t, b"\x1b]2x;no\x07"), ("can".into(), vec![]));
        assert_eq!(
            title(&mut t, b"\x1b_a\x07b#(c)\x1b\\"),
            ("ab_(c)".into(), ev("ab#(c)"))
        );
        // utf8_isvalid: DEL and invalid UTF-8 are refused.
        assert_eq!(
            title(&mut t, b"\x1b]2;a\x7fb\x07"),
            ("ab_(c)".into(), vec![])
        );
        assert_eq!(
            title(&mut t, b"\x1b]2;a\xffb\x07"),
            ("ab_(c)".into(), vec![])
        );
        t.set_allow_set_title(false);
        assert_eq!(
            title(&mut t, b"\x1b]2;no\x07\x1b_no\x1b\\"),
            ("ab_(c)".into(), vec![])
        );
        t.set_allow_set_title(true);
        // Longer than input-buffer-size: discarded.
        t.set_input_buffer_size(64);
        let long = [b"\x1b]2;".as_slice(), &[b'a'; 80], b"\x07"].concat();
        assert_eq!(title(&mut t, &long), ("ab_(c)".into(), vec![]));
        // The BEL that ended an OSC title is no bell; the grid is untouched.
        assert_eq!(title(&mut t, b"\x1b]2;t\x07"), ("t".into(), ev("t")));
        assert_eq!(captured(&t), "x", "only the x after the CAN");
    }

    #[test]
    fn a_title_given_back_is_kept_as_it_was_read() {
        let mut t = PaneTerm::new(20, 5, 100);
        // No TAB: `valid_name` refuses a title with a control character.
        t.set_title("a\\b#(d)");
        let shown = t.title();
        assert_eq!(shown, "a\\\\b#(d)");
        let mut back = PaneTerm::new(20, 5, 100);
        back.restore_title(&shown);
        assert_eq!(back.title(), shown);
    }

    /// D30: libghostty decides what is a notification. OSC 9 that is not a
    /// `ConEmu` command and `OSC 777;notify;title;body` are, each in order;
    /// `9;4` stays progress, the other `ConEmu` forms and other 777s are
    /// nothing, and an OSC 9 inside `DCS tmux;` is passthrough, never seen
    /// by Ghostty. The grid shows none of it.
    #[test]
    fn notifications_are_what_libghostty_reports() {
        let mut t = PaneTerm::new(20, 3, 10);
        t.feed(b"\x1b]9;hello\x07\x1b]777;notify;T;a;b\x1b\\");
        t.feed(b"\x1b]9;4;1;50\x07\x1b]9;1;100\x07\x1b]9;2;box\x07\x1b]9;3;tab\x07");
        t.feed(b"\x1b]9;12\x07\x1b]777;other;x;y\x07\x1b]777;notify;notitle\x07");
        t.feed(b"\x1bPtmux;\x1b\x1b]9;wrapped\x07\x1b\\");
        t.feed(b"\x1b]777;notify;;empty\x07X");
        let got: Vec<Notification> = t
            .take_events()
            .into_iter()
            .filter_map(|e| match e {
                TermEvent::Notification(n) => Some(n),
                _ => None,
            })
            .collect();
        let n = |title: &[u8], body: &[u8]| Notification {
            title: title.to_vec(),
            body: body.to_vec(),
        };
        assert_eq!(got, [n(b"", b"hello"), n(b"T", b"a;b"), n(b"", b"empty")]);
        assert_eq!(t.progress_bar().progress, 50);
        assert_eq!(t.take_passthrough().len(), 1);
        let shown = crate::capture::capture(&t, crate::CaptureOpts::default());
        assert!(shown.starts_with(b"X\n\n"), "{shown:?}");
    }

    #[test]
    fn osc_9_4_sets_the_progress_bar() {
        let mut t = PaneTerm::new(20, 5, 100);
        assert_eq!(t.progress_bar(), ProgressBar::default());
        // ST and BEL both end it, a split across reads too, and nothing
        // reaches the grid.
        t.feed(b"a\x1b]9;4;1;5");
        assert_eq!(t.take_events(), []);
        t.feed(b"0\x1b\\b");
        assert_eq!(t.take_events(), [TermEvent::Progress]);
        let bar = t.progress_bar();
        assert_eq!((bar.state.name(), bar.progress), ("normal", 50));
        // One event for a run of them; tmux redraws for an unchanged one.
        t.feed(b"\x1b]9;4;1;50\x07\x1b]9;4;2\x07");
        assert_eq!(t.take_events(), [TermEvent::Progress]);
        assert_eq!(t.progress_bar().state.name(), "error");
        // A leading zero in the number is still OSC 9 (input_exit_osc).
        t.feed(b"\x1b]09;4;4;7\x07");
        assert_eq!(t.take_events(), [TermEvent::Progress]);
        assert_eq!(
            (t.progress_bar().state.name(), t.progress_bar().progress),
            ("paused", 7)
        );
        // Malformed: nothing set, no redraw. libghostty's OSC 9 parser
        // takes the first two for notifications (osc9.zig falls back to
        // one when no `ConEmu` form matches), which only D30 reads.
        t.feed(b"\x1b]9;4;7;1\x07\x1b]9;1\x07\x1b]9\x07");
        let n = |body: &[u8]| {
            TermEvent::Notification(Notification {
                title: Vec::new(),
                body: body.to_vec(),
            })
        };
        assert_eq!(t.take_events(), [n(b"4;7;1"), n(b"1")]);
        assert_eq!(t.progress_bar().state.name(), "paused");
        // RIS keeps it (screen_write_reset leaves it); a respawn resets.
        t.feed(b"\x1bc");
        assert_eq!(t.progress_bar().progress, 7);
        t.reset_progress_bar();
        assert_eq!(t.progress_bar(), ProgressBar::default());
        t.feed(b"ab");
        let shown = crate::capture::capture(&t, crate::CaptureOpts::default());
        assert!(shown.starts_with(b"ab\n"), "{shown:?}");
    }

    /// An OSC past `input-buffer-size` does nothing (`input_input` sets
    /// `INPUT_DISCARD`), and Ghostty does not keep what follows the limit:
    /// the notification it would have reported is not one, the title stays,
    /// and what comes after the string works, whether the string ended or
    /// a resize came in the middle of it.
    #[test]
    fn an_osc_past_the_input_buffer_is_discarded_whole() {
        let n = |body: &str| Notification {
            title: Vec::new(),
            body: body.as_bytes().to_vec(),
        };
        let cap = dcs::capacity(1);
        let long = "a".repeat(cap + 100);
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_input_buffer_size(1);
        t.feed(format!("\x1b]2;kept\x07\x1b]777;notify;;{long}\x07").as_bytes());
        t.feed(b"\x1b]777;notify;;short\x1b\\\x1b]777;notify;;");
        t.feed(long.as_bytes());
        t.feed(b"\x1b\\\x1b]2;after\x07X");
        let got: Vec<Notification> = t
            .take_events()
            .into_iter()
            .filter_map(|e| match e {
                TermEvent::Notification(n) => Some(n),
                _ => None,
            })
            .collect();
        assert_eq!(got, [n("short")]);
        assert_eq!(t.title(), "after");
        assert_eq!(t.row(t.history_size()).text(), "X");
        // A resize in the middle of one hands Ghostty the string's start
        // again: it must be the rejected one, not the string up to the
        // limit and more.
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_input_buffer_size(1);
        t.feed(format!("\x1b]777;notify;;{long}").as_bytes());
        assert_eq!(t.ghostty_partial(), b"\x1b];");
        t.resize(20, 4);
        t.feed(b"b\x07X");
        assert_eq!(t.take_events(), []);
        assert_eq!(t.row(t.history_size()).text(), "X");
    }

    #[test]
    fn an_osc_9_past_the_input_buffer_is_dropped() {
        // Leading zeros keep the number 50 and make the payload 48 bytes:
        // past the 31 that the smallest buffer holds (`input_input`).
        let osc = format!("\x1b]9;4;1;{}50\x07", "0".repeat(40));
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_input_buffer_size(4);
        t.feed(osc.as_bytes());
        assert_eq!(t.progress_bar(), ProgressBar::default());
        assert_eq!(t.take_events(), []);
        t.set_input_buffer_size(1 << 20);
        t.feed(osc.as_bytes());
        assert_eq!(t.progress_bar().progress, 50);
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

    /// input.c:2970, 3099, 3115, 3142, 3158, 3455, 3478: the palette OSCs
    /// ask for a full pane redraw when they change something, ended by BEL,
    /// ST, or the ESC that starts the next sequence.
    #[test]
    fn palette_oscs_ask_for_a_redraw() {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_colour_names(|n| (n == "red").then_some((255, 0, 0)));
        for (bytes, redraw) in [
            (&b"\x1b]4;1;#ff0000\x07"[..], true),
            (b"\x1b]4;1;red\x1b\\", true),
            (b"\x1b]4;1;red\x1b[m", true),
            (b"\x1b]4;1;zzz\x07", false),
            (b"\x1b]4;1;?\x07", false),
            (b"\x1b]10;#ffffff\x07", true),
            (b"\x1b]11;#000000\x07", true),
            (b"\x1b]10;?\x07", false),
            (b"\x1b]12;#ff0000\x07", false),
            (b"\x1b]112\x07", false),
            (b"\x1b]104;1\x07", true),
            (b"\x1b]104\x07", true),
            (b"\x1b]110\x07\x1b]111\x07", true),
            (b"\x1b]110;x\x07", false),
            (b"\x1b_]4;1;red\x07\x1b\\", false),
            (b"\x1b_\x1b]4;1;red\x07", true),
            (b"\x1b]4;1;red\x18", true),
        ] {
            t.feed(bytes);
            assert_eq!(
                t.take_redraw(),
                redraw,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        // One OSC across two feeds; a payload after it sees the redraw.
        t.feed(b"\x1b]4;1;#ff");
        assert!(!t.take_redraw());
        t.feed(b"0000\x07\x1bPtmux;a\x1b\\");
        assert!(t.take_passthrough()[0].redraw);
        assert!(t.take_redraw());
        t.feed(b"\x1bPtmux;b\x1b\\");
        assert!(!t.take_passthrough()[0].redraw);
        // RIS clears the palette, so an `OSC 104;1` after it changes nothing.
        t.feed(b"\x1bc");
        t.take_redraw();
        t.feed(b"\x1b]104;1\x07");
        assert!(!t.take_redraw());
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
        // ED3 under the alternate screen clears the primary history
        // (input.c:1758), and the history's marks go with its lines.
        t.feed(b"\x1b[3J");
        assert_eq!(t.history_size(), 0);
        assert_eq!(
            t.line_marks(),
            [(0, LineMark::Output), (2, LineMark::Prompt)]
        );
    }

    /// An ED or ECH that clears a whole line empties it, OSC 133 flags
    /// and all (`grid_clear_lines`); EL keeps them
    /// (`screen_write_clearline`). Reference `capture-pane -pF`, 20x6.
    #[test]
    fn whole_line_erases_drop_the_osc133_marks() {
        const A: &str = "\x1b]133;A\x07";
        const C: &str = "\x1b]133;C\x07";
        let prompt = |y| (y, LineMark::Prompt);
        let output = |y| (y, LineMark::Output);
        let cases: [(String, Vec<(usize, LineMark)>); 14] = [
            (format!("{A}p\r\x1b[K"), vec![prompt(0)]),
            (format!("{A}p\x1b[2K"), vec![prompt(0)]),
            (format!("{A}p\x1b[1K"), vec![prompt(0)]),
            (format!("{A}\x1b[J"), vec![]),
            (format!("x\r\n{A}p\x1b[1J"), vec![prompt(1)]),
            (format!("{A}p\r\x1b[20X"), vec![]),
            (format!("{A}p\r\x1b[19X"), vec![prompt(0)]),
            (format!("{A}p\x1b[20X"), vec![prompt(0)]),
            (format!("x\r\n{A}p\r\n{C}o\x1b[2;1H\x1b[J"), vec![]),
            (format!("x\r\n{A}p\r\n{C}o\x1b[2;3H\x1b[J"), vec![prompt(1)]),
            (
                format!("x\r\n{A}p\r\n{C}o\x1b[2;20H\x1b[1J"),
                vec![output(2)],
            ),
            (
                format!("x\r\n{A}p\r\n{C}o\x1b[2;19H\x1b[1J"),
                vec![prompt(1), output(2)],
            ),
            (
                format!("{A}12345678901234567890{C}\x1b[J"),
                vec![prompt(0), output(0)],
            ),
            (format!("a\r\nb\r\n{A}p\x1bc"), vec![prompt(2)]),
        ];
        for (bytes, marks) in cases {
            let mut t = PaneTerm::new(20, 6, 100);
            t.feed(bytes.as_bytes());
            assert_eq!(t.line_marks(), marks, "{bytes:?}");
        }
    }

    /// zsh draws each prompt after `CR ED 0` (`zle_refresh`), so an `A` its
    /// precmd prints, and the `C` of a command with no output, are gone
    /// once the prompt shows. The first `ED 0` is at the top left, so its
    /// line scrolls into history with its mark (`scroll-on-clear`).
    /// Reference: these bytes (zsh 5.9's, with `print -n` of `D` and `A`
    /// in precmd and `C` in preexec), 40x7, `capture-pane -pF -S -`.
    #[test]
    fn zsh_redraws_erase_the_marks_before_the_prompt() {
        let mut t = PaneTerm::new(40, 7, 100);
        let prompt = "% \r \r\x1b]133;D;0\x07\x1b]133;A\x07\r\x1b[J% ";
        t.feed(format!("\x1b]133;A\x07{prompt}").as_bytes());
        for out in ["a\r\n", "", "b\r\n"] {
            t.feed(format!("cmd\r\n\x1b]133;C\x07{out}{prompt}").as_bytes());
        }
        assert_eq!(t.history_size(), 1);
        assert_eq!(
            t.line_marks(),
            [
                (0, LineMark::Prompt),
                (2, LineMark::Output),
                (5, LineMark::Output)
            ]
        );
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
                    | ScanEvent::EraseBelow
                    | ScanEvent::EraseScreen
                    | ScanEvent::EraseAbove
                    | ScanEvent::EraseChars
                    | ScanEvent::OscOver
                    | ScanEvent::Drop
                    | ScanEvent::Utf8
                    | ScanEvent::Utf8Stop
                    | ScanEvent::Osc133
                    | ScanEvent::DcsStart
                    | ScanEvent::RenameStart
                    | ScanEvent::Sync
                    | ScanEvent::Deccolm
                    | ScanEvent::TitleStack
                    | ScanEvent::Title
                    | ScanEvent::Answer
                    | ScanEvent::Rewrite
                    | ScanEvent::Kitty
                    | ScanEvent::Skip => None,
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

    /// `#{pane_key_mode}` after each request, under each `extended-keys`
    /// value. Probed on tmux 3.8 (isolated): `set -g extended-keys E`, a
    /// fresh `sh` pane prints the bytes, then `display -p
    /// '#{pane_key_mode}'`.
    #[test]
    fn key_mode_requests_follow_tmux() {
        use KeyMode::{Extended1 as E1, Extended2 as E2, Standard as S};
        let cases: [(&[u8], [KeyMode; 3]); 14] = [
            (b"", [S, S, S]),
            (b"\x1b[>4;1m", [S, E1, E1]),
            (b"\x1b[>4;2m", [S, E2, E2]),
            (b"\x1b[>4m", [S, S, E1]),
            (b"\x1b[>4;2m\x1b[>4n", [S, S, E1]),
            (b"\x1b[>4;0m", [S, S, E1]),
            (b"\x1b[>4;2m\x1b[>5n", [S, E2, E2]),
            (b"\x1b[>4;2m\x1bc", [S, S, E1]),
            (b"\x1b[>4;3m", [S, S, E1]),
            (b"\x1b[>4;2m\x1b[>4;1m", [S, E1, E1]),
            (b"\x1b[>4;2m\x1b[>m", [S, E2, E2]),
            (b"\x1b[>4;2m\x1b[?4m", [S, E2, E2]),
            (b"\x1b[>4;2;5m", [S, E2, E2]),
            (b"\x1b[>4:2m", [S, S, S]),
        ];
        for (bytes, want) in cases {
            for (option, want) in [ExtendedKeys::Off, ExtendedKeys::On, ExtendedKeys::Always]
                .into_iter()
                .zip(want)
            {
                let mut t = PaneTerm::new(10, 2, 0);
                t.set_extended_keys(option);
                for b in bytes {
                    t.feed(&[*b]);
                }
                assert_eq!(t.key_mode(), want, "{option:?} {bytes:?}");
            }
        }
        // The requests reach Ghostty too, and leave the screen alone.
        let mut t = PaneTerm::new(10, 2, 0);
        t.set_extended_keys(ExtendedKeys::On);
        t.feed(b"a\x1b[>4;2mb");
        assert_eq!(t.key_mode(), E2);
        assert_eq!(t.cursor(), (2, 0));
        // A new screen under `always` starts in mode 1 (screen_reinit).
        t.set_extended_keys(ExtendedKeys::Always);
        t.reset_key_mode();
        assert_eq!(t.key_mode(), E1);
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
        t.feed(&lines(0, 2500, false));
        t.feed(b"\x1b[?1049h");
        let newest = t.row(t.history_size() - 1).text();
        t.set_history_budget(3 * PAGE_BYTES);
        // The history shown meanwhile is the primary's, pruned now.
        let retained = t.primary_history_rows();
        assert!(retained < 2000, "{retained}");
        assert_eq!(t.history_size(), retained);
        assert_eq!(t.row(retained - 1).text(), newest);
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
    fn a_reflow_keeps_the_links_of_pages_full_of_them() {
        // Short linked lines fill pages' hyperlink maps; narrower, each
        // takes two rows of a destination page with as many cells. Ghostty
        // grew the map once where it needed twice and dropped the links
        // that did not fit (`unreachable` in its safe builds); patch 0018.
        let mut t = PaneTerm::new(80, 4, 100_000);
        t.feed(b"\x1b]8;;http://example.com\x07");
        for _ in 0..3000 {
            t.feed(format!("{}\r\n", "x".repeat(30)).as_bytes());
        }
        t.feed(b"\x1b]8;;\x07");
        t.resize(17, 4);
        let state = t.export_state();
        let (mut text, mut linked) = (0, 0);
        for page in &state.primary.pages {
            let crate::PageState::Rows(p) = page else {
                panic!("an uncompressed page")
            };
            for c in p.cells.iter().filter(|c| c.content == u32::from('x')) {
                text += 1;
                linked += usize::from(c.link != 0);
            }
        }
        assert_eq!((text, linked), (3000 * 30, 3000 * 30));
    }

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

    /// `grid_reflow` keeps the blank lines below the cursor: narrowing a
    /// pane whose wrapped lines no longer fit pushes its top rows into
    /// history and keeps the cursor on its line (the regress scene
    /// cache-rotate-after: a 19-column pane rotated into a 9-column cell);
    /// widening joins them back out of history.
    #[test]
    fn reflow_keeps_blank_rows_below_the_cursor() {
        let mut t = PaneTerm::new(19, 13, 2000);
        for i in 0..5 {
            t.feed(format!("PANE{i:02} abcdefghij\r\n").as_bytes());
        }
        t.resize(9, 13);
        assert_eq!((t.history_size(), t.cursor()), (5, (0, 5)));
        assert_eq!(t.row(0).text(), "PANE00 ab");
        assert_eq!(t.row(5).text(), "cdefghij");
        assert_eq!(t.row(6).text(), "PANE03 ab");
        assert_eq!(t.row(10).text(), "");
        // Narrower and shorter at once: the height goes first.
        let mut t = PaneTerm::new(19, 13, 2000);
        for i in 0..5 {
            t.feed(format!("PANE{i:02} abcdefghij\r\n").as_bytes());
        }
        t.resize(9, 8);
        assert_eq!(t.cursor(), (0, 5));
        assert_eq!(t.row(t.history_size()).text(), "cdefghij");
        // Wider by one and taller: the taller screen pulls the history
        // back first (screen_resize_y), so the blank rows stay below the
        // cursor (tmux 3.8: hsize 0, cursor 0,10).
        t.resize(10, 13);
        assert_eq!((t.history_size(), t.cursor()), (0, (0, 10)));
        assert_eq!(t.row(1).text(), "defghij");
        // Wide enough for the lines to join: the joined rows come back
        // from history, the cursor stays on its line.
        t.resize(30, 13);
        assert_eq!((t.history_size(), t.cursor()), (0, (0, 5)));
        assert_eq!(t.row(4).text(), "PANE04 abcdefghij");
    }

    /// Lines of 20 linked grapheme cells (one link of 100 a line, `id=`
    /// so they are not all distinct) reflowed from 80 columns to 20 fill
    /// a Ghostty page's hyperlink map. Growing its `hyperlink_bytes` to the
    /// largest value left the map as full as before, so the cell after it
    /// lost its link (patch 0018; this is what a release build shows).
    /// With the map grown until the row moves to a new page, the retry on
    /// that page wrote the cell's combining mark a second time (patch
    /// 0019; an abort in a safe build, as `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe`
    /// gives). Every cell keeps its character and its line's link, as in
    /// tmux 3.8, whose grid keeps a link per cell with no page limit
    /// (grid.c `grid_set_cell`, hyperlinks.c `hyperlinks_put`).
    #[test]
    fn reflow_keeps_every_link_past_a_full_hyperlink_map() {
        let (lines, width) = (1500, 20);
        let mut t = PaneTerm::new(80, 24, 3000);
        for i in 0..lines {
            let cells = "e\u{301}".repeat(width);
            let k = i % 100;
            t.feed(format!("\x1b]8;id={k};http://f/{k}\x07{cells}\x1b]8;;\x07\r\n").as_bytes());
        }
        t.resize(20, 24);
        assert_eq!(t.cursor(), (0, 23));
        let first = t.history_size() + 23 - lines;
        for i in 0..lines {
            let y = first + i;
            assert_eq!(t.row(y).text(), "e\u{301}".repeat(width), "line {i}");
            let link = Some(format!("http://f/{}", i % 100));
            for x in 0..width {
                assert_eq!(t.hyperlink(x, y), link, "line {i} cell {x}");
            }
        }
    }

    /// Panes given one counter (the server's) number their links from it
    /// in the order they are drawn; a pane on its own numbers apart, and
    /// one that joins a counter moves it past what it gave. `clear-history
    /// -H`, which rebuilds the pane, keeps its counter.
    #[test]
    fn panes_number_links_from_the_counter_they_are_given() {
        let link = |t: &PaneTerm, y| t.drawn_link((0, y)).unwrap().number();
        let numbers = crate::LinkNumbers::default();
        let (mut a, mut b) = (PaneTerm::new(20, 3, 100), PaneTerm::new(20, 3, 100));
        a.number_links_from(&numbers);
        b.number_links_from(&numbers);
        a.feed(b"\x1b]8;;http://a\x07A\x1b]8;;\x07");
        b.feed(b"\x1b]8;;http://b\x07B\x1b]8;;\x07");
        assert_eq!((link(&a, 0), link(&b, 0)), (1, 2));
        assert!(b.reset_hyperlinks());
        assert!(b.link_numbers().same(&numbers));
        b.feed(b"\r\n\x1b]8;;http://c\x07C\x1b]8;;\x07");
        assert_eq!(link(&b, 1), 3);

        let mut alone = PaneTerm::new(20, 3, 100);
        alone.feed(b"\x1b]8;;http://d\x07D\r\n\x1b]8;;http://e\x07E\x1b]8;;\x07");
        assert_eq!((link(&alone, 0), link(&alone, 1)), (1, 2));
        alone.number_links_from(&numbers);
        assert_eq!(numbers.next(), 4);
        let joined = crate::LinkNumbers::default();
        alone.number_links_from(&joined);
        assert_eq!(joined.next(), 4);
    }

    /// `clear-history -H` (cmd-capture-pane.c:427-428,
    /// `screen_reset_hyperlinks`): the screen in use forgets its links and
    /// the one open at the cursor, whatever else it holds stays, and a
    /// link written afterwards is read as ever. A screen without links is
    /// left as it is.
    #[test]
    fn reset_hyperlinks_forgets_the_links_of_the_screen_in_use() {
        let mut t = PaneTerm::new(20, 4, 100);
        assert!(!t.reset_hyperlinks());
        t.feed(b"\x1b[31mred \x1b[0m\x1b]8;id=a;http://a\x07ab\x1b]8;;\x07 \x1b]8;;http://b\x07\xc3\xa9x");
        let (cursor, before) = (t.cursor(), t.row(0).text());
        assert_eq!(t.hyperlink(4, 0).as_deref(), Some("http://a"));
        assert_eq!(t.hyperlink(8, 0).as_deref(), Some("http://b"));
        assert!(t.reset_hyperlinks());
        assert_eq!((0..20).find_map(|x| t.hyperlink(x, 0)), None);
        assert_eq!((t.cursor(), t.row(0).text()), (cursor, before));
        assert!(!t.reset_hyperlinks());
        // The link open at the cursor is closed with the rest: text written
        // on has none, and a new link is a link.
        t.feed(b"yz\x1b]8;;http://n\x07new\x1b]8;;\x07");
        assert_eq!(t.hyperlink(10, 0), None);
        assert_eq!(t.hyperlink(12, 0).as_deref(), Some("http://n"));
        // The alternate screen's links go with it, the primary's stay.
        t.feed(b"\x1b[?1049h\x1b[H\x1b]8;;http://alt\x07alt\x1b]8;;\x07");
        assert_eq!(t.hyperlink(0, 0).as_deref(), Some("http://alt"));
        assert!(t.reset_hyperlinks());
        assert_eq!(t.hyperlink(0, 0), None);
        assert_eq!(t.row(0).text(), "alt");
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.hyperlink(12, 0).as_deref(), Some("http://n"));
    }

    /// worker-25's reproduction of the reflow abort (patches 0018 and
    /// 0019): 4000 lines of 20 grapheme cells, each cell its own OSC 8
    /// link (`id=K`, K of 1000, so links repeat across pages), on 80x24,
    /// resized to 17 columns. A safe Ghostty build aborted in the reflow;
    /// every cell keeps its character and link, as in tmux 3.8 (grid.c
    /// `grid_set_cell`, hyperlinks.c `hyperlinks_put`).
    #[test]
    fn reflow_to_17_columns_keeps_per_cell_links_and_graphemes() {
        use std::fmt::Write as _;
        let (lines, width, ids) = (4000, 20, 1000);
        let mut t = PaneTerm::new(80, 24, 10_000);
        let link = |i: usize, x: usize| (i * width + x) % ids;
        for i in 0..lines {
            let mut text = String::new();
            for x in 0..width {
                let k = link(i, x);
                write!(text, "\x1b]8;id={k};http://f/{k}\x07e\u{301}").unwrap();
            }
            t.feed(format!("{text}\x1b]8;;\x07\r\n").as_bytes());
        }
        t.resize(17, 24);
        assert_eq!(t.cursor(), (0, 23));
        // Each line is a row of 17 cells and a wrapped row of 3.
        let first = t.history_size() + 23 - 2 * lines;
        for i in 0..lines {
            let y = first + 2 * i;
            let text = t.row(y).text() + &t.row(y + 1).text();
            assert_eq!(text, "e\u{301}".repeat(width), "line {i}");
            for x in 0..width {
                let (cx, cy) = (x % 17, y + x / 17);
                let want = Some(format!("http://f/{}", link(i, x)));
                assert_eq!(t.hyperlink(cx, cy), want, "line {i} cell {x}");
            }
        }
    }

    /// A primary-screen program with a footer under its input line (the
    /// cursor above the last content row): a narrowing splits the lines
    /// into history, the widening joins them back, and the screen and the
    /// cursor are where they were (tmux 3.8; the review repro of
    /// `gap_render_styles`).
    #[test]
    fn reflow_keeps_a_footer_under_the_cursor() {
        let mut t = PaneTerm::new(60, 20, 2000);
        t.feed(b"L1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\nL2 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\r\nfooter\x1b[1A\x1b[2G");
        t.resize(30, 20);
        assert_eq!((t.history_size(), t.cursor()), (2, (1, 0)));
        assert_eq!(t.row(4).text(), "footer");
        for _ in 0..3 {
            t.resize(60, 20);
            assert_eq!((t.history_size(), t.cursor()), (0, (1, 1)));
            assert_eq!(t.row(0).text(), "L1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
            assert_eq!(t.row(2).text(), "footer");
            t.resize(30, 20);
        }
    }

    /// `screen_resize_y` with `eat_empty`: a shorter screen deletes the
    /// lines below the cursor first, text or not, before pushing any into
    /// history.
    #[test]
    fn a_shorter_screen_eats_the_lines_below_the_cursor() {
        let mut t = PaneTerm::new(20, 6, 100);
        t.feed(b"top\r\ncur\r\nf1\r\nf2\r\nf3\x1b[2;1H");
        t.resize(20, 4);
        assert_eq!((t.history_size(), t.cursor()), (0, (0, 1)));
        assert_eq!(t.row(0).text(), "top");
        assert_eq!(t.row(2).text(), "f1");
        assert_eq!(t.row(3).text(), "f2");
        t.resize(20, 1);
        assert_eq!((t.history_size(), t.cursor()), (1, (0, 0)));
        assert_eq!(t.row(1).text(), "cur");
    }

    /// `screen_resize_y`: a taller screen pulls history back wherever the
    /// cursor is, and the cursor moves down with its line (tmux 3.8).
    #[test]
    fn a_taller_screen_pulls_history_above_the_cursor() {
        let mut t = PaneTerm::new(20, 4, 100);
        t.feed(b"h1\r\nh2\r\nh3\r\na\r\nb\r\nc\r\nfoot\x1b[2;1H");
        assert_eq!((t.history_size(), t.cursor()), (3, (0, 1)));
        t.resize(20, 6);
        assert_eq!((t.history_size(), t.cursor()), (1, (0, 3)));
        assert_eq!(t.row(1).text(), "h2");
        assert_eq!(t.row(6).text(), "foot");
    }

    #[test]
    fn zero_history_keeps_none() {
        let mut t = PaneTerm::new(20, 5, 0);
        for _ in 0..50 {
            t.feed(b"x\r\n");
        }
        assert_eq!(t.history_size(), 0);
    }

    /// tmux's `cellused` of the bottom row, on either screen: written cells
    /// count, erased or not; a full-line clear empties it.
    #[test]
    fn bottom_row_used_counts_written_cells() {
        let mut t = PaneTerm::new(20, 4, 100);
        assert_eq!(t.bottom_row_used(), 0);
        t.feed(b"\x1b[4;1Habcde\x1b[4;3H\x1b[K");
        assert_eq!(t.bottom_row_used(), 5);
        t.feed(b"\x1b[2K");
        assert_eq!(t.bottom_row_used(), 0);
        t.feed(b"\x1b[?1049h\x1b[4;1Hxy");
        assert_eq!(t.bottom_row_used(), 2);
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.bottom_row_used(), 0);
    }

    /// tmux resizes the primary grid once as the alternate screen ends,
    /// from the size it had to the current one (`screen_alternate_off`),
    /// and moves `hscrolled` then; Ghostty resized it at every resize under
    /// the alternate screen. Numbers from tmux 3.8 (oracle
    /// `alt_exit_hscrolled`): history and cursor after the end, then after a
    /// taller screen. The count survives a state export under the
    /// alternate screen.
    #[test]
    fn the_alternate_screen_ends_moving_hscrolled_as_tmux() {
        type Case = (
            &'static [(u16, u16)],
            &'static [(u16, u16)],
            (usize, (u16, u16)),
            (u16, u16),
            (usize, (u16, u16)),
        );
        let cases: [Case; 4] = [
            // A narrowing runs it on through every split line.
            (
                &[(80, 50)],
                &[(25, 50)],
                (131, (2, 49)),
                (25, 80),
                (101, (2, 79)),
            ),
            // A shorter screen adds what it pushes past the cursor.
            (
                &[(80, 50)],
                &[(80, 20)],
                (41, (2, 19)),
                (80, 60),
                (1, (2, 59)),
            ),
            // A widening takes the joined rows away.
            (
                &[(80, 50)],
                &[(25, 50), (25, 30)],
                (151, (2, 29)),
                (80, 30),
                (31, (2, 29)),
            ),
            // A taller one pulls at most hscrolled, whatever Ghostty did
            // meanwhile.
            (
                &[],
                &[(25, 20), (25, 40), (80, 40), (80, 30)],
                (31, (2, 29)),
                (80, 60),
                (1, (2, 59)),
            ),
        ];
        for (n, (before, under, back, after, end)) in cases.into_iter().enumerate() {
            for import in [false, true] {
                let mut t = PaneTerm::new(80, 20, 2000);
                for i in 10..70 {
                    t.feed(format!("{i}{}\r\n", "x".repeat(73)).as_bytes());
                }
                t.feed(b"$ ");
                for &(c, r) in before {
                    t.resize(c, r);
                }
                t.feed(b"\x1b[?1049h\x1b[H\x1b[2Jvim");
                for &(c, r) in under {
                    t.resize(c, r);
                }
                if import {
                    t = PaneTerm::import_state(&t.export_state()).expect("import");
                }
                t.feed(b"\x1b[?1049l");
                assert_eq!(
                    (t.history_size(), t.cursor()),
                    back,
                    "case {n}, import {import}: back"
                );
                t.resize(after.0, after.1);
                assert_eq!(
                    (t.history_size(), t.cursor()),
                    end,
                    "case {n}, import {import}: after"
                );
            }
        }
    }

    /// `?47l` and `?1047l` resize the primary screen with the alternate
    /// screen's cursor (screen.c `screen_alternate_off`): the rows under it
    /// go before any are pushed into history, a taller screen pulls rows
    /// back above it, and a narrower one puts it at the end of its line.
    /// Numbers from tmux 3.8 (oracle `alt_screen_exit`: `s47`, `b47bot`,
    /// `b47botn`), through a state export under the alternate screen too.
    #[test]
    fn the_alternate_cursor_resizes_the_primary_as_tmux() {
        type Case = (u16, &'static [u8], (u16, u16), (usize, (u16, u16)));
        let cases: [Case; 3] = [
            (50, b"\x1b[?47h\x1b[H\x1b[2Jvim", (80, 20), (11, (3, 0))),
            (
                20,
                b"\x1b[?47h\x1b[H\x1b[2J\x1b[20;1Hvim",
                (80, 50),
                (11, (3, 49)),
            ),
            (
                20,
                b"\x1b[?1047h\x1b[H\x1b[2J\x1b[20;1Hvim",
                (30, 20),
                (161, (0, 19)),
            ),
        ];
        for (n, (rows, enter, (c, r), back)) in cases.into_iter().enumerate() {
            for import in [false, true] {
                let mut t = PaneTerm::new(80, rows, 2000);
                for i in 10..70 {
                    t.feed(format!("{i}{}\r\n", "x".repeat(73)).as_bytes());
                }
                t.feed(enter);
                t.resize(c, r);
                if import {
                    t = PaneTerm::import_state(&t.export_state()).expect("import");
                }
                t.feed(if enter.starts_with(b"\x1b[?47") {
                    b"\x1b[?47l"
                } else {
                    b"\x1b[?1047l"
                });
                assert_eq!((t.cols(), t.rows()), (c, r), "case {n}, import {import}");
                assert_eq!(
                    (t.history_size(), t.cursor()),
                    back,
                    "case {n}, import {import}"
                );
            }
        }
    }

    /// The pen's link goes with tmux's saved cell: DECRC and RCP restore
    /// DECSC's, `?1049l` the alternate screen's, RIS clears both, and a
    /// screen switch keeps it ([`PenLinks`]). Expected: tmux 3.8's
    /// capture-pane -e (oracle `decrc_restores_link`).
    #[test]
    fn decrc_restores_the_link() {
        let capture = |t: &PaneTerm| {
            let opts = crate::capture::CaptureOpts {
                escapes: true,
                ..Default::default()
            };
            String::from_utf8_lossy(&crate::capture::capture(t, opts)).into_owned()
        };
        let mut t = PaneTerm::new(20, 4, 0);
        t.feed(b"\x1b]8;id=x;http://a/\x1b\\A\x1b7\x1b]8;;\x1b\\B\x1b8C\x1b]8;;\x1b\\\r\n");
        t.feed(b"\x1b[s\x1b]8;;http://c/\x07P\x1b[u\x1b[CQ\r\n");
        t.feed(b"\x1b]8;id=y;http://d/\x1b\\R\x1b[?1049hS");
        assert_eq!(
            capture(&t),
            "\n\n \x1b]8;id=y;http://d/\x1b\\S\x1b]8;;\x1b\\\n\n"
        );
        t.feed(b"\x1b]8;;\x1b\\T\x1b[?1049lU\x1b]8;;\x1b\\");
        assert_eq!(
            capture(&t),
            "\x1b]8;id=x;http://a/\x1b\\AC\x1b]8;;\x1b\\\n\x1b]8;;http://c/\x1b\\P\x1b]8;;\x1b\\Q\n\x1b]8;id=y;http://d/\x1b\\RU\x1b]8;;\x1b\\\n\n"
        );
        let mut t = PaneTerm::new(20, 4, 0);
        t.feed(b"\x1b]8;;http://a/\x1b\\A\x1bcB");
        assert_eq!(capture(&t), "B\n\n\n\n");
    }

    /// DECSC's slot is tmux's input context's (input.c `input_save_state`):
    /// no resize moves it, and DECRC clamps it to the screen
    /// (`input_restore_state`). Ghostty moved it with its row, to the
    /// top-left once the row left the screen, and kept the blank rows
    /// under it, so a shorter alternate screen lost the rows its cursor was
    /// on. Numbers from tmux 3.8 (oracle `alt_screen_saved_cursor_resize`).
    #[test]
    fn a_resize_leaves_the_saved_cursor_as_tmux() {
        let lines = |t: &mut PaneTerm| {
            for i in 10..40 {
                t.feed(format!("{i}xxxxxxxxxx\r\n").as_bytes());
            }
        };
        for (end, back) in [((80, 30), (1, (10, 29))), ((60, 40), (0, (10, 29)))] {
            let mut t = PaneTerm::new(80, 30, 2000);
            lines(&mut t);
            t.feed(b"\x1b[30;11H\x1b7\x1b[?47h\x1b[2;4Habc");
            t.resize(100, 20);
            assert_eq!(t.cursor(), (6, 1), "{end:?}: the alternate cursor");
            t.resize(end.0, end.1);
            t.feed(b"\x1b[?47l\x1b8");
            assert_eq!((t.history_size(), t.cursor()), back, "{end:?}");
        }
        let mut t = PaneTerm::new(80, 30, 2000);
        lines(&mut t);
        t.feed(b"\x1b[3;6H\x1b7\x1b[30;1H");
        t.resize(80, 20);
        t.feed(b"\x1b8");
        assert_eq!((t.history_size(), t.cursor()), (11, (5, 2)));
        t.resize(4, 20);
        t.feed(b"\x1b8");
        assert_eq!(t.cursor(), (3, 2));
    }

    /// What a reader from before Ghostty patch 0028 takes of a primary
    /// screen the alternate one left at its size: a copy at the terminal's
    /// size, reflowed, the alternate screen as it is.
    #[test]
    fn a_copy_has_the_primary_at_the_terminals_size() {
        let mut t = PaneTerm::new(80, 20, 2000);
        for i in 10..70 {
            t.feed(format!("{i}{}\r\n", "x".repeat(73)).as_bytes());
        }
        t.feed(b"\x1b]133;A\x07$ \x1b[?1049h\x1b[Hvim");
        assert!(t.with_primary_resized().is_none(), "at the terminal's size");
        t.resize(40, 10);
        let copy = t.with_primary_resized().expect("left at 80x20");
        let (a, b) = (t.export_state(), copy.export_state());
        assert_eq!(a.primary_size, Some((80, 20)));
        assert_eq!(b.primary_size, None);
        assert_eq!((b.cols, b.rows), (40, 10));
        assert!(b.alternate_active);
        assert_eq!(b.alternate, a.alternate, "the alternate screen");
        assert_eq!(b.pane.marks.len(), a.pane.marks.len(), "the mark follows");
        let rows =
            |s: &TermState| -> usize { s.primary.pages.iter().filter_map(PageState::rows).sum() };
        assert!(rows(&b) > rows(&a), "the 76-cell rows are split in two");
    }

    /// `?1049h` saves the cursor in a slot of its own, kept after the
    /// alternate screen and restored by `?1049l` even off it; `?1049h` on
    /// an alternate screen already up saves nothing; DECSC's slot is one
    /// for both screens; every alternate screen comes up blank (input.c,
    /// screen.c `screen_alternate_on`, `screen_alternate_off`; oracle
    /// `alt_screen_cursor_slots`).
    #[test]
    fn alternate_screen_cursor_slots_as_tmux() {
        let mut t = PaneTerm::new(20, 10, 100);
        assert_eq!(t.alternate_saved(), None);
        t.feed(b"\x1b[3;3H\x1b7\x1b[5;6H\x1b[?1049h\x1b8");
        assert_eq!(t.cursor(), (2, 2), "DECRC on the alternate screen");
        assert_eq!(t.alternate_saved(), Some((5, 4)));
        t.feed(b"\x1b[7;7H\x1b7\x1b[?1049l");
        assert_eq!(t.cursor(), (5, 4));
        t.feed(b"\x1b8");
        assert_eq!(t.cursor(), (6, 6), "DECRC after the alternate screen");
        t.feed(b"\x1b[1;1H\x1b[?1049l");
        assert_eq!(t.cursor(), (5, 4), "?1049l off the alternate screen");
        t.feed(b"\x1b[?47habc\x1b[?47l\x1b[?47h");
        assert_eq!(
            t.row(t.history_size()).cells[0].text.as_str(),
            "",
            "blank again"
        );
        t.feed(b"\x1b[2;2H\x1b[?1049h\x1b[9;9H\x1b[?1049l");
        assert_eq!(t.alternate_saved(), Some((5, 4)), "nothing saved");
        assert_eq!(t.cursor(), (5, 4));
        t.forget_alternate_saved();
        assert_eq!(t.alternate_saved(), None);
    }

    /// A taller screen pulls back only the history rows tmux counts as
    /// scrolled (`hscrolled`, `screen_resize_y`): none of those a clear
    /// scrolled into history (`grid_view_clear_history`), the rows scrolled
    /// after it. The count survives a state export.
    #[test]
    fn a_taller_screen_pulls_back_only_scrolled_rows() {
        let mut t = PaneTerm::new(20, 6, 100);
        t.feed(b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n\x1b[2Jafter\r\nA\r\nB");
        assert_eq!(t.history_size(), 10);
        let mut u = PaneTerm::import_state(&t.export_state()).expect("import");
        for p in [&mut t, &mut u] {
            p.resize(20, 12);
            assert_eq!((p.history_size(), p.cursor()), (8, (1, 7)));
        }
        // A narrowing under a short first line: the rows it pushes into
        // history are not scrolled ones (grid_reflow_split).
        let mut t = PaneTerm::new(40, 8, 100);
        t.feed(format!("$ ls\r\n{}$ ", format!("{}\r\n", "x".repeat(36)).repeat(4)).as_bytes());
        t.resize(12, 8);
        assert_eq!((t.history_size(), t.cursor()), (8, (2, 5)));
        t.resize(12, 14);
        assert_eq!((t.history_size(), t.cursor()), (8, (2, 5)));
        // A long one: they come back, and the state stays one an import
        // takes (the pulled rows' ids go with them).
        let mut t = PaneTerm::new(40, 8, 100);
        t.feed(format!("{}$ ", format!("{}\r\n", "x".repeat(36)).repeat(5)).as_bytes());
        t.resize(12, 8);
        assert_eq!(t.history_size(), 10);
        t.resize(12, 14);
        assert_eq!((t.history_size(), t.cursor()), (4, (2, 11)));
        let mut u = PaneTerm::import_state(&t.export_state()).expect("import");
        assert_eq!(u.history_size(), 4);
        t.feed(b"\r\nmore");
        u.feed(b"\r\nmore");
        assert_eq!(u.history_id(0), t.history_id(0));
    }

    /// A resize does not touch tmux's input parser (`screen_resize` leaves
    /// `wp->ictx` alone), so a sequence the program left half-written is
    /// finished by its next bytes. The resize's own cursor moves
    /// (`write_screen_moves`) must not end it in Ghostty's parser.
    #[test]
    fn a_resize_keeps_a_half_written_sequence() {
        use crate::snapshot::Color;
        // A shorter screen with text below the cursor writes moves
        // (eat_below_cursor); a taller one pulls history back without any
        // (Ghostty patch 0025).
        let resizes: [&[(u16, u16)]; 2] = [&[(20, 4)], &[(20, 4), (20, 6)]];
        let starts: [(&[u8], &[u8], Color); 4] = [
            (b"\x1b[3", b"1mX", Color::Palette(1)),
            (b"\x1b[38;5", b";1mX", Color::Palette256(1)),
            (b"\x1b", b"[31mX", Color::Palette(1)),
            (b"\x1b[3\xff", b"1mX", Color::Palette(1)),
        ];
        for sizes in resizes {
            for (head, tail, fg) in starts {
                let mut t = PaneTerm::new(20, 6, 100);
                t.feed(b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\x1b[2;1H");
                t.feed(head);
                for &(cols, rows) in sizes {
                    t.resize(cols, rows);
                }
                assert_eq!(t.pending_input(), head, "{sizes:?} {head:?}");
                t.feed(tail);
                let (cx, cy) = t.cursor();
                let row = t.row(t.history_size() + usize::from(cy));
                let x = &row.cells[usize::from(cx) - 1];
                assert_eq!((x.text.as_str(), cx), ("X", 1), "{sizes:?} {head:?}");
                assert_eq!(x.style.fg, fg, "{sizes:?} {head:?}");
            }
        }
        // A string: the title is the whole OSC.
        let mut t = PaneTerm::new(20, 6, 100);
        t.feed(b"0\r\n1\r\n2\x1b[H\x1b]2;ab");
        t.resize(20, 2);
        t.feed(b"cd\x07");
        assert_eq!(t.title(), "abcd");
        assert_eq!(t.row(t.history_size()).text(), "0");
    }

    /// A C0 control inside a half-written CSI or escape runs when it
    /// arrives and the state stays (input.c `input_c0_dispatch`); the
    /// sequence a resize gives back to Ghostty must not run it again. The
    /// pane ends as if the resize came after the program's next bytes.
    #[test]
    fn a_resize_runs_no_control_of_a_half_written_sequence_again() {
        let shown = |t: &PaneTerm| {
            let opts = crate::capture::CaptureOpts {
                escapes: true,
                start: crate::capture::LineRef::Start,
                end: crate::capture::LineRef::End,
                ..Default::default()
            };
            let text = String::from_utf8_lossy(&crate::capture::capture(t, opts)).into_owned();
            (text, t.cursor(), t.history_size())
        };
        let resizes: [&[(u16, u16)]; 2] = [&[(20, 4)], &[(20, 4), (20, 6)]];
        // The newline: the cursor's row; the tab: its column; the
        // intermediate's newline: through ESC ( 0 (DEC graphics).
        let starts: [(&[u8], &[u8]); 4] = [
            (b"\x1b[3\r\n", b"1mX"),
            (b"\x1b[3\x08", b"1mX"),
            (b"\x1b\t", b"[31mX"),
            (b"\x1b(\r\n", b"0q\x1b(B"),
        ];
        let screen = b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\x1b[2;3H";
        for sizes in resizes {
            for (head, tail) in starts {
                let mut want = PaneTerm::new(20, 6, 100);
                want.feed(screen);
                want.feed(head);
                want.feed(tail);
                let mut t = PaneTerm::new(20, 6, 100);
                t.feed(screen);
                t.feed(head);
                for &(cols, rows) in sizes {
                    t.resize(cols, rows);
                    want.resize(cols, rows);
                }
                assert_eq!(t.pending_input(), head, "{sizes:?} {head:?}");
                t.feed(tail);
                assert_eq!(shown(&t), shown(&want), "{sizes:?} {head:?}");
            }
        }
        // The reviewer's probes, as tmux has them: the red X on the
        // cursor's row, below it "5" kept; after the tab, column 9.
        let mut t = PaneTerm::new(20, 6, 100);
        t.feed(b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\x1b[2;1H\x1b[3\r\n");
        t.resize(20, 4);
        t.feed(b"1mX");
        assert_eq!((t.cursor(), t.history_size()), ((1, 2), 2));
        assert_eq!(t.row(t.history_size() + 3).text(), "5");
        let mut t = PaneTerm::new(20, 6, 100);
        t.feed(b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\x1b[2;1H\x1b\t");
        t.resize(20, 4);
        t.feed(b"[31mX");
        assert_eq!(t.cursor(), (9, 1));
    }

    /// The pending input stops at [`PaneTerm::pending_max`]. A string past it has
    /// lost its middle, and tmux discards one past `input-buffer-size`
    /// (`input_input`): a resize leaves Ghostty swallowing the rest, not
    /// a string with a hole. A sequence after such a string is kept whole.
    #[test]
    fn a_resize_drops_a_string_past_the_pending_limit() {
        use crate::snapshot::Color;
        let screen = b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\x1b[2;1H";
        let long = vec![b'a'; PaneTerm::new(1, 1, 0).pending_max() + 10];
        // A long OSC 2 still open: Ghostty must not get its start again.
        let mut t = PaneTerm::new(20, 6, 100);
        t.feed(screen);
        t.feed(b"\x1b]2;");
        t.feed(&long);
        assert_eq!(t.pending_input().len(), t.pending_max() + 1);
        assert_eq!(t.ghostty_partial(), b"\x1b];");
        t.resize(20, 4);
        t.feed(b"b\x07X");
        assert_eq!(t.title(), "");
        assert_eq!(
            (t.row(t.history_size() + 1).text(), t.cursor()),
            ("X".into(), (1, 1))
        );
        // A long OSC 2 ended by the ESC of a CSI still open: the CSI is
        // what the pending input holds, and the colour applies.
        let mut t = PaneTerm::new(20, 6, 100);
        t.feed(screen);
        t.feed(b"\x1b]2;");
        t.feed(&long);
        t.feed(b"\x1b[3");
        assert_eq!(t.pending_input(), b"\x1b[3");
        t.resize(20, 4);
        t.feed(b"1mX");
        let (cx, cy) = t.cursor();
        let row = t.row(t.history_size() + usize::from(cy));
        let x = &row.cells[usize::from(cx) - 1];
        assert_eq!(
            (x.text.as_str(), cx, x.style.fg),
            ("X", 1, Color::Palette(1))
        );
    }
}
