//! Copy mode (window-copy.c): a scrollable view of a pane's history and
//! screen with its own cursor and selection, over a copy of the pane's grid
//! taken on entry ([`Grid`]).
//!
//! Drawing line numbers is not here yet.

mod search;

use std::time::{Duration, Instant};

use vtmux_core::cmd::{Args, CmdEntry};
use vtmux_vt::{Cell, PaneTerm, Row, Snapshot};

use super::grid::Grid;
use super::reader::{Reader, WHITESPACE, cell_text};
use search::{Search, SearchKind};

/// What a copy mode command asks for afterwards
/// (`enum window_copy_cmd_action`); only leaving is visible to vtmux, which
/// draws the whole mode screen every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Stay,
    Cancel,
}

/// `RECENTRE_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recentre {
    Top,
    Middle,
    Bottom,
}

/// Which end of the selection follows the cursor (`cursordrag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drag {
    /// The selection is independent of the cursor.
    None,
    /// The end follows the cursor.
    End,
    /// The start follows the cursor.
    Start,
}

/// Which way a line selection runs (`lineflag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineSel {
    None,
    LeftRight,
    RightLeft,
}

/// The unit a selection grows by (`selflag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    Char,
    Word,
    Line,
}

impl SelectionMode {
    /// `#{selection_mode}`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Char => "char",
            Self::Word => "word",
            Self::Line => "line",
        }
    }
}

/// The options copy mode reads as it goes.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the options and the client state a copy mode command reads"
)]
pub struct Env {
    /// `mode-keys` is `vi`.
    pub vi: bool,
    /// `copy-mode-line-numbers`: off, default, absolute, relative, hybrid.
    pub line_numbers: i64,
    /// The session's `word-separators` (`cs->s->options`).
    pub separators: String,
    /// `wrap-search`.
    pub wrap_search: bool,
    /// The command came with a valid mouse event (`cs->m`): `begin-selection`
    /// starts a mouse drag instead.
    pub mouse: bool,
    /// What `scroll-to-mouse` needs ([`CopyMode::scroll_slider`]): the
    /// pointer's row against the pane's top, where the slider was grabbed,
    /// the slider's height and the bar's. `None` without a client.
    pub slider: Option<(i64, i64, u32, u32)>,
    /// The command's client holds a mouse button it has moved
    /// (`cs->c->tty.mouse_drag_flag`): the scroll commands move the cursor,
    /// and the selection's end with it, rather than only the view.
    pub dragging: bool,
}

/// How a cell in view is painted (`window_copy_update_style`,
/// `screen_select_cell`), for the renderer to give it the option's style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paint {
    /// `copy-mode-mark-style`, its colours swapped on the mark's cell.
    Mark { inverse: bool },
    /// `copy-mode-match-style`.
    Match { inverse: bool },
    /// `copy-mode-current-match-style`.
    CurrentMatch { inverse: bool },
    /// `copy-mode-selection-style`.
    Selection,
    /// `copy-mode-line-number-style`, or
    /// `copy-mode-current-line-number-style` on the cursor's row: a cell
    /// of the line numbers' gutter.
    LineNumber { current: bool },
    /// `copy-mode-current-line-style` over a cell of the cursor's row: its
    /// colours where set, its attributes added.
    CurrentLine,
}

/// What the last jump looked for (`jumptype`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jump {
    Off,
    Forward,
    Backward,
    ToForward,
    ToBackward,
}

/// Whether a command clears the search marks (`enum
/// window_copy_cmd_clear`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clear {
    Always,
    Never,
    EmacsOnly,
}

/// The table's `.clear` for `name` (window-copy.c:3298-3891).
fn clear_kind(name: &str) -> Clear {
    match name {
        "copy-pipe-no-clear"
        | "copy-selection-no-clear"
        | "line-numbers-on"
        | "line-numbers-off"
        | "line-numbers-toggle"
        | "pipe-no-clear"
        | "refresh-on"
        | "refresh-off"
        | "refresh-now"
        | "refresh-toggle"
        | "toggle-position" => Clear::Never,
        "bottom-line"
        | "cursor-down"
        | "cursor-left"
        | "cursor-right"
        | "cursor-up"
        | "cursor-centre-vertical"
        | "cursor-centre-horizontal"
        | "end-of-line"
        | "goto-line"
        | "halfpage-down"
        | "halfpage-up"
        | "history-bottom"
        | "history-top"
        | "jump-again"
        | "jump-backward"
        | "jump-forward"
        | "jump-reverse"
        | "jump-to-backward"
        | "jump-to-forward"
        | "middle-line"
        | "next-paragraph"
        | "next-space"
        | "next-space-end"
        | "next-word"
        | "next-word-end"
        | "other-end"
        | "page-down"
        | "page-up"
        | "previous-paragraph"
        | "previous-space"
        | "previous-word"
        | "scroll-down"
        | "scroll-to-mouse"
        | "scroll-up"
        | "start-of-line"
        | "top-line" => Clear::EmacsOnly,
        _ => Clear::Always,
    }
}

/// A grid position: column and backing line.
type Pos = (usize, usize);

/// The selection as drawn (`struct screen_sel`): screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScreenSel {
    sx: usize,
    sy: usize,
    ex: usize,
    ey: usize,
    rectangle: bool,
    /// `mode-keys` when copy mode was entered (`data->modekeys`).
    vi: bool,
    hidden: bool,
    /// Columns left of this are never selected (`clipx`): the gutter of
    /// the line numbers when the selection was set.
    clipx: usize,
}

impl ScreenSel {
    /// `screen_check_selection` (screen.c:534-637).
    #[allow(clippy::similar_names)]
    fn contains(&self, px: usize, py: usize) -> bool {
        if self.hidden || px < self.clipx {
            return false;
        }
        let (sx, sy, ex, ey) = (self.sx, self.sy, self.ex, self.ey);
        if self.rectangle {
            if py < sy.min(ey) || py > sy.max(ey) {
                return false;
            }
            // The start's column is in, the cursor's is too.
            let (l, r) = if ex < sx { (ex, sx) } else { (sx, ex) };
            return px >= l && px <= r;
        }
        // Like emacs, keep the top-left-most character and drop the
        // bottom-right-most; vi keeps both.
        let back = |x: usize| if self.vi { x } else { x.saturating_sub(1) };
        if sy < ey {
            if py < sy || py > ey || (py == sy && px < sx) {
                return false;
            }
            !(py == ey && px > back(ex))
        } else if sy > ey {
            if py > sy || py < ey || (py == ey && px < ex) {
                return false;
            }
            !(py == sy && (sx == 0 || px > back(sx)))
        } else if py != sy {
            false
        } else if ex < sx {
            px >= ex && px <= back(sx)
        } else {
            px >= sx && px <= back(ex)
        }
    }
}

/// One copy mode (`struct window_copy_mode_data`).
#[allow(clippy::struct_excessive_bools)] // tmux's independent mode flags
#[derive(Debug)]
pub struct CopyMode {
    /// The key mode of the mode's screen (`#{pane_key_mode}`).
    pub key_mode: vtmux_vt::KeyMode,
    backing: Grid,
    sx: usize,
    sy: usize,
    /// Lines scrolled up into history.
    oy: usize,
    cx: usize,
    cy: usize,
    /// Position and size of the last line with content the cursor left.
    lastcx: usize,
    lastsx: usize,
    scroll_exit: bool,
    hide_position: bool,
    /// The last column of the position indicator drawn, when a window grew
    /// under the mode: see [`CopyMode::clip_position`].
    position_clip: Option<usize>,
    /// 0 off, 1 from the option, 2 the default numbers when the option is off.
    line_numbers: u8,
    recentre: Recentre,
    recentre_line: usize,
    /// `wme->prefix`: the repeat count for the next command.
    pub prefix: u32,
    /// `copy-mode -k`: kill the pane when the mode exits.
    pub kill: bool,
    /// `mode-keys` was vi on entry (`data->modekeys`): how the selection
    /// is drawn.
    modekeys_vi: bool,
    /// The selection on screen (`data->screen.sel`), `None` without one.
    sel: Option<ScreenSel>,
    /// Start and end of the selection on the grid.
    selpos: Pos,
    endpos: Pos,
    drag: Drag,
    lineflag: LineSel,
    rectflag: bool,
    selflag: SelectionMode,
    /// Where a word or line selection started (`dx`, `dy`).
    dpos: Pos,
    /// Where the start and end go back to when the cursor turns
    /// (`selrx`, `selry`, `endselrx`, `endselry`).
    selr: Pos,
    endselr: Pos,
    /// `data->separators`, set by word selection.
    separators: String,
    search: Search,
    /// The pane's last search (`wp->searchstr`, `wp->searchregex`), kept
    /// by the pane between copy modes.
    pub pane_search: Option<(String, bool)>,
    jump: Jump,
    jumpchar: String,
    /// The mark (`mx`, `my`) and whether it is drawn (`showmark`).
    mark: Pos,
    show_mark: bool,
    /// The view is of another pane (`copy-mode -s`): it is never refreshed
    /// (`window_copy_refresh_allowed`).
    foreign: bool,
    /// When the automatic refresh runs next, while it is on
    /// (`refresh_active`, `refresh_timer`).
    refresh_due: Option<Instant>,
    /// When a mouse drag held at the top or bottom row scrolls again
    /// (`dragtimer`).
    drag_due: Option<Instant>,
    /// The view moved since [`Self::take_scrolled`]: where tmux calls
    /// `window_pane_scrollbar_show(wp, 1)`.
    scrolled: bool,
    /// The cursor, scroll offset and selection as of the last
    /// [`Self::moved_since_offset`].
    offset_seen: (usize, usize, usize, bool),
}

/// How often a drag held at the top or bottom row scrolls
/// (`WINDOW_COPY_DRAG_REPEAT_TIME`, 50 ms).
pub const DRAG_REPEAT: Duration = Duration::from_millis(50);

/// How often the automatic refresh looks for new output
/// (`WINDOW_COPY_REFRESH_INTERVAL`, 50 ms).
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(50);

/// The copy mode commands tmux has (`window_copy_cmd_table`), so one vtmux
/// has not got yet is an error rather than ignored like an unknown name.
const TMUX_COMMANDS: &[&str] = &[
    "append-selection",
    "append-selection-and-cancel",
    "back-to-indentation",
    "begin-selection",
    "bottom-line",
    "cancel",
    "clear-selection",
    "copy-end-of-line",
    "copy-end-of-line-and-cancel",
    "copy-pipe-end-of-line",
    "copy-pipe-end-of-line-and-cancel",
    "copy-line",
    "copy-line-and-cancel",
    "copy-pipe-line",
    "copy-pipe-line-and-cancel",
    "copy-pipe-no-clear",
    "copy-pipe",
    "copy-pipe-and-cancel",
    "copy-selection-no-clear",
    "copy-selection",
    "copy-selection-and-cancel",
    "cursor-down",
    "cursor-down-and-cancel",
    "cursor-left",
    "cursor-right",
    "cursor-up",
    "cursor-centre-vertical",
    "cursor-centre-horizontal",
    "end-of-line",
    "goto-line",
    "halfpage-down",
    "halfpage-down-and-cancel",
    "halfpage-up",
    "history-bottom",
    "history-top",
    "jump-again",
    "jump-backward",
    "jump-forward",
    "jump-reverse",
    "jump-to-backward",
    "jump-to-forward",
    "jump-to-mark",
    "line-numbers-on",
    "line-numbers-off",
    "line-numbers-toggle",
    "next-prompt",
    "previous-prompt",
    "middle-line",
    "next-matching-bracket",
    "next-paragraph",
    "next-space",
    "next-space-end",
    "next-word",
    "next-word-end",
    "other-end",
    "page-down",
    "page-down-and-cancel",
    "page-up",
    "pipe-no-clear",
    "pipe",
    "pipe-and-cancel",
    "previous-matching-bracket",
    "previous-paragraph",
    "previous-space",
    "previous-word",
    "recentre-top-bottom",
    "rectangle-on",
    "rectangle-off",
    "rectangle-toggle",
    "refresh-on",
    "refresh-off",
    "refresh-now",
    "refresh-toggle",
    "scroll-bottom",
    "scroll-down",
    "scroll-down-and-cancel",
    "scroll-exit-on",
    "scroll-exit-off",
    "scroll-exit-toggle",
    "scroll-middle",
    "scroll-to-mouse",
    "scroll-top",
    "scroll-up",
    "search-again",
    "search-backward",
    "search-backward-text",
    "search-backward-incremental",
    "search-forward",
    "search-forward-text",
    "search-forward-incremental",
    "search-reverse",
    "select-line",
    "select-word",
    "selection-mode",
    "set-mark",
    "start-of-line",
    "stop-selection",
    "toggle-position",
    "top-line",
];

/// The copy mode commands a read-only client may run
/// (`WINDOW_COPY_CMD_FLAG_READONLY` in `window_copy_cmd_table`,
/// window-copy.c:3293-3890): movement, scrolling, marks and cancel. Every
/// other command in the table writes (selections, copies, pipes) or
/// searches and is refused (window-copy.c:3925-3933).
const READ_ONLY_COMMANDS: &[&str] = &[
    "back-to-indentation",
    "bottom-line",
    "cancel",
    "cursor-down",
    "cursor-down-and-cancel",
    "cursor-left",
    "cursor-right",
    "cursor-up",
    "cursor-centre-vertical",
    "cursor-centre-horizontal",
    "end-of-line",
    "goto-line",
    "halfpage-down",
    "halfpage-down-and-cancel",
    "halfpage-up",
    "history-bottom",
    "history-top",
    "jump-to-mark",
    "line-numbers-on",
    "line-numbers-off",
    "line-numbers-toggle",
    "next-prompt",
    "previous-prompt",
    "middle-line",
    "next-matching-bracket",
    "next-paragraph",
    "next-space",
    "next-space-end",
    "next-word",
    "next-word-end",
    "page-down",
    "page-down-and-cancel",
    "page-up",
    "previous-matching-bracket",
    "previous-paragraph",
    "previous-space",
    "previous-word",
    "recentre-top-bottom",
    "refresh-on",
    "refresh-off",
    "refresh-now",
    "refresh-toggle",
    "scroll-bottom",
    "scroll-down",
    "scroll-down-and-cancel",
    "scroll-middle",
    "scroll-to-mouse",
    "scroll-top",
    "scroll-up",
    "set-mark",
    "start-of-line",
    "toggle-position",
    "top-line",
];

/// A command [`CopyMode::command`] could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// tmux has it, vtmux not yet.
    NotImplemented(String),
    /// The client is read-only and the command is not
    /// `WINDOW_COPY_CMD_FLAG_READONLY`: nothing ran, and the client's
    /// status line says `client is read-only`.
    ReadOnly,
}

/// Text a copy command took (`window_copy_copy_buffer`,
/// `window_copy_append_selection`), for the server to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copied {
    pub data: Vec<u8>,
    /// The new buffer's name prefix (`paste_add`), already expanded.
    pub prefix: Option<String>,
    /// Add a paste buffer (no `-P`).
    pub paste: bool,
    /// Send it to the clipboard if `set-clipboard` allows (no `-C`).
    pub clip: bool,
    /// Append to the top buffer rather than add one.
    pub append: bool,
}

/// Text a pipe command feeds a job (`window_copy_pipe_run`), for the
/// server to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipe {
    /// The command, already expanded; empty means the `copy-command` option.
    pub command: String,
    /// The selection: empty when there is none (the job still runs).
    pub data: Vec<u8>,
}

/// What a copy mode command did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub action: Action,
    pub copied: Option<Copied>,
    pub pipe: Option<Pipe>,
    /// `refresh-now`: the server copies the pane's screen again.
    pub refresh: bool,
    /// `begin-selection` with a mouse event: the server starts a drag
    /// (`window_copy_start_drag`) on the mouse's pane.
    pub start_drag: bool,
}

impl From<Action> for Outcome {
    fn from(action: Action) -> Self {
        Self {
            action,
            copied: None,
            pipe: None,
            refresh: false,
            start_drag: false,
        }
    }
}

/// The copy commands whose arguments (a buffer name prefix, a command)
/// are expanded as formats before the command runs (`format_single`).
const PREFIX_COMMANDS: &[&str] = &[
    "copy-end-of-line",
    "copy-end-of-line-and-cancel",
    "copy-line",
    "copy-line-and-cancel",
    "copy-selection-no-clear",
    "copy-selection",
    "copy-selection-and-cancel",
];

/// `window_copy_copy_buffer`'s arguments for `data`, when there is some.
fn copied(data: Option<Vec<u8>>, prefix: Option<&str>, args: &Args) -> Option<Copied> {
    Some(Copied {
        data: data?,
        prefix: prefix.map(str::to_owned),
        paste: !args.flag('P'),
        clip: !args.flag('C'),
        append: false,
    })
}

/// A command's arguments (`window_copy_cmd_table` `.args`): flags and the
/// most positionals it takes; every other command takes none.
fn arg_spec(name: &str) -> (&'static str, usize, usize) {
    match name {
        n if PREFIX_COMMANDS.contains(&n) => ("CP", 0, 1),
        n if n.starts_with("copy-pipe") => ("CP", 0, 2),
        "goto-line"
        | "jump-backward"
        | "jump-forward"
        | "jump-to-backward"
        | "jump-to-forward"
        | "search-backward-incremental"
        | "search-forward-incremental" => ("", 1, 1),
        "pipe-no-clear"
        | "pipe"
        | "pipe-and-cancel"
        | "search-backward"
        | "search-backward-text"
        | "search-forward"
        | "search-forward-text"
        | "selection-mode" => ("", 0, 1),
        "next-prompt" | "previous-prompt" => ("o", 0, 0),
        "scroll-to-mouse" => ("e", 0, 0),
        _ => ("", 0, 0),
    }
}

/// `name`'s arguments with the ones tmux runs through `format_single`
/// (the buffer prefix and the pipe command, window-copy.c:1380-1560,
/// 2660-2720) expanded by `expand`. They are expanded before the command
/// runs, so the formats see the cursor and selection as they were. Other
/// commands, and arguments `name` rejects, come back as given.
pub fn expand_args(name: &str, args: &[String], expand: &dyn Fn(&str) -> String) -> Vec<String> {
    let formatted = PREFIX_COMMANDS.contains(&name)
        || name.starts_with("copy-pipe")
        || matches!(name, "pipe" | "pipe-no-clear" | "pipe-and-cancel");
    let Some(parsed) = parse_args(name, args).filter(|_| formatted) else {
        return args.to_vec();
    };
    // Only boolean flags (`-C`, `-P`), so the arguments rebuild as flags,
    // `--`, then the expanded positionals.
    let mut out: Vec<String> = parsed
        .flags
        .iter()
        .flat_map(|(f, v)| std::iter::repeat_n(format!("-{f}"), v.len()))
        .collect();
    out.push("--".to_owned());
    out.extend(parsed.positionals.iter().map(|p| expand(&p.text())));
    out
}

/// `args_parse` with the command's spec; `None` for arguments it rejects
/// (the command then does nothing).
fn parse_args(name: &str, args: &[String]) -> Option<Args> {
    let (template, min, max) = arg_spec(name);
    let entry = CmdEntry {
        name: "",
        alias: None,
        template,
        min_args: min,
        max_args: Some(max),
        usage: "",
        target: None,
        source: None,
        starts_server: false,
        implemented: true,
    };
    let words: Vec<String> = std::iter::once(name.to_owned())
        .chain(args.iter().cloned())
        .collect();
    Args::parse(&entry, &words).ok()
}

/// `#{selection_start_x}` and the rest, while there is a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionInfo {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub active: bool,
    pub present: bool,
}

impl CopyMode {
    /// `window_copy_init`: a copy of `source`'s grid (`copy-mode -s`, or
    /// the pane itself) in a `sx` x `sy` view, `-e` and `-H` given. A
    /// source other than the pane loses its trailing empty lines
    /// (`window_copy_clone_screen`'s `trim`). `vi` is `mode-keys`.
    #[must_use]
    pub fn new(
        source: &PaneTerm,
        (sx, sy): (usize, usize),
        trim: bool,
        (scroll_exit, hide_position): (bool, bool),
        vi: bool,
        pane_search: Option<(String, bool)>,
    ) -> Self {
        let (backing, cx, cursor_row) = Grid::clone_of(source, sx, sy, trim);
        let hsize = backing.hsize();
        let (cy, oy) = if cursor_row < hsize {
            (0, hsize - cursor_row)
        } else {
            (cursor_row - hsize, 0)
        };
        Self {
            key_mode: vtmux_vt::KeyMode::Standard,
            backing,
            sx,
            sy,
            oy,
            cx,
            cy,
            lastcx: 0,
            lastsx: 0,
            scroll_exit,
            hide_position,
            position_clip: None,
            line_numbers: 1,
            recentre: Recentre::Middle,
            recentre_line: 0,
            prefix: 1,
            kill: false,
            modekeys_vi: vi,
            sel: None,
            selpos: (0, 0),
            endpos: (0, 0),
            drag: Drag::None,
            lineflag: LineSel::None,
            rectflag: false,
            selflag: SelectionMode::Char,
            dpos: (0, 0),
            selr: (0, 0),
            endselr: (0, 0),
            separators: String::new(),
            // window_copy_common_init: the pane's last search is searched
            // up again by search-again.
            search: Search {
                kind: if pane_search.is_some() {
                    SearchKind::Up
                } else {
                    SearchKind::Off
                },
                regex: pane_search.as_ref().is_some_and(|(_, r)| *r),
                string: pane_search.as_ref().map(|(s, _)| s.clone()),
                all: true,
                ..Search::default()
            },
            pane_search,
            jump: Jump::Off,
            jumpchar: String::new(),
            mark: (cx, hsize + cy - oy),
            show_mark: false,
            foreign: trim,
            refresh_due: None,
            drag_due: None,
            scrolled: false,
            offset_seen: (cx, cy, oy, false),
        }
    }

    /// Whether the mode moved its cursor, scrolled or changed its
    /// selection since the last call (or since it began): tmux draws such a
    /// change through the pane (`screen_write_start_pane`), moving the
    /// cursor, which works the window's offset out again
    /// (screen-write.c:92-120). Its first draw is on its own screen
    /// (`window_copy_init`, window-copy.c:639-643), so it moves nothing.
    pub fn moved_since_offset(&mut self) -> bool {
        let now = (self.cx, self.cy, self.oy, self.sel.is_some());
        std::mem::replace(&mut self.offset_seen, now) != now
    }

    /// `window_copy_view_init` (window-copy.c:652-674): view mode's copy of
    /// its own empty `backing`, the cursor at the top left, line numbers
    /// off, never refreshed from a pane (`viewmode`).
    #[must_use]
    pub fn view(
        backing: &PaneTerm,
        size: (usize, usize),
        vi: bool,
        pane_search: Option<(String, bool)>,
    ) -> Self {
        let mut m = Self::new(backing, size, false, (false, false), vi, pane_search);
        m.line_numbers = 0;
        m.foreign = true;
        m
    }

    /// The end of `window_copy_vadd` (window-copy.c:741-747): the lines
    /// view mode wrote to `backing` from line `from` on are read into the
    /// copy (all of it again for `None`), and history they pushed out is
    /// added to the scroll offset, so the view stays where it was.
    pub(super) fn view_added(&mut self, backing: &PaneTerm, from: Option<usize>) {
        let old = self.hsize();
        match from {
            Some(from) => self.backing.extend_from(backing, from),
            None => self.backing = Grid::clone_of(backing, self.sx, self.sy, false).0,
        }
        self.oy = (self.oy + self.hsize().saturating_sub(old)).min(self.hsize());
    }

    /// `window_copy_resize` (window-copy.c:1212-1243): the copy is resized
    /// (reflowed if the width changed) keeping the cursor on its text; a
    /// cursor line now in history is shown at the top, else the view goes
    /// to the bottom. The selection is cleared (`window_copy_size_changed`).
    pub fn resize(&mut self, sx: usize, sy: usize) {
        self.position_clip = None;
        let (sx, sy) = (sx.max(1), sy.max(1));
        let hsize = self.hsize();
        self.oy = self.oy.min(hsize + self.cy);
        let cy = hsize + self.cy - self.oy;
        let (cx, cy) = self.backing.resize_keeping(sx, sy, self.cx, cy);
        (self.sx, self.sy) = (sx, sy);
        let hsize = self.hsize();
        self.cx = cx;
        (self.cy, self.oy) = if cy < hsize {
            (0, hsize - cy)
        } else {
            (cy - hsize, 0)
        };
        // window_copy_clear_selection reads mode-keys; a resize has no
        // command to give it, so it is the value on entry.
        let env = Env {
            vi: self.modekeys_vi,
            line_numbers: 0,
            separators: String::new(),
            wrap_search: false,
            mouse: false,
            slider: None,
            dragging: false,
        };
        self.size_changed(&env);
    }

    /// `window_copy_size_changed`: the selection goes and the marks are
    /// made again for the new size, counting all of history.
    fn size_changed(&mut self, env: &Env) {
        self.clear_selection(env);
        let had = self.search.marks.is_some();
        self.clear_marks();
        if had && !self.search.timeout {
            self.search_marks(None, self.search.regex, false);
        }
        self.search.origin = Some((self.cx, self.cy, self.oy));
    }

    /// `refresh_active`.
    #[must_use]
    pub fn refresh_active(&self) -> bool {
        self.refresh_due.is_some()
    }

    /// When the automatic refresh next runs, if it is on.
    #[must_use]
    pub fn refresh_due(&self) -> Option<Instant> {
        self.refresh_due
    }

    /// `window_copy_refresh_start`: not for another pane's view, nor when
    /// already on.
    fn refresh_start(&mut self) {
        if !self.foreign && self.refresh_due.is_none() {
            self.refresh_due = Some(Instant::now() + REFRESH_INTERVAL);
        }
    }

    /// `window_copy_refresh_arm`: the next refresh, if it is on.
    pub fn refresh_arm(&mut self, now: Instant) {
        if self.refresh_due.is_some() {
            self.refresh_due = Some(now + REFRESH_INTERVAL);
        }
    }

    /// Whether the timer should refresh now (`window_copy_refresh_timer`):
    /// not while a selection is being made, so it does not move.
    #[must_use]
    pub fn refresh_ready(&self) -> bool {
        self.sel.is_none() && self.drag == Drag::None
    }

    /// Whether the cursor is at the bottom of the view, so a refresh
    /// follows new output.
    #[must_use]
    pub fn follows(&self) -> bool {
        self.oy == 0 && self.cy == self.sy - 1
    }

    /// `window_copy_do_refresh`: copy the pane's screen again, then put the
    /// view back: at the bottom when `follow`, else on the same lines (the
    /// distance from the top of history kept).
    pub fn refresh(&mut self, env: &Env, source: &PaneTerm, follow: bool) {
        self.position_clip = None;
        self.oy = self.oy.min(self.hsize());
        let from_top = self.hsize() - self.oy;
        let (backing, _, _) = Grid::clone_of(source, self.sx, self.sy, false);
        self.backing = backing;
        let hsize = self.hsize();
        if follow {
            self.cy = self.sy - 1;
            self.cx = self.cursor_limit(env, hsize + self.cy, false);
            self.oy = 0;
        } else if from_top <= hsize {
            self.oy = hsize - from_top;
        } else {
            self.cy = 0;
            self.oy = hsize;
        }
        self.size_changed(env);
    }

    // --- grid access ------------------------------------------------------

    /// The mode's screen as `capture-pane -M` reads it.
    #[must_use]
    pub fn backing(&self) -> &Grid {
        &self.backing
    }

    fn hsize(&self) -> usize {
        self.backing.hsize()
    }

    /// Whether the view moved since the last call.
    pub fn take_scrolled(&mut self) -> bool {
        std::mem::take(&mut self.scrolled)
    }

    /// `window_copy_get_current_offset`: the top of the view counted from
    /// the top of history, and the history's size.
    #[must_use]
    pub fn current_offset(&self) -> (usize, usize) {
        (self.hsize() - self.oy, self.hsize())
    }

    /// Last row of the backing grid (`hsize + sy - 1`).
    fn last_row(&self) -> usize {
        self.backing.last()
    }

    /// Backing row `y`, history first.
    fn row(&self, y: usize) -> &Row {
        self.backing.row(y)
    }

    fn wrapped(&self, y: usize) -> bool {
        self.backing.wrapped(y)
    }

    /// `window_copy_find_length`: `grid_line_length`.
    fn find_length(&self, py: usize) -> usize {
        self.backing.length(py)
    }

    /// `window_copy_cursor_limit`: vi keys stop on the last character.
    fn cursor_limit(&self, env: &Env, py: usize, allow_onemore: bool) -> usize {
        if allow_onemore || !env.vi {
            self.backing.length(py)
        } else {
            self.backing.limit(py)
        }
    }

    /// The backing row under screen row `cy`.
    fn abs(&self, cy: usize) -> usize {
        self.hsize() + cy - self.oy
    }

    fn reader(&self) -> Reader<'_> {
        Reader::new(&self.backing, self.cx, self.abs(self.cy))
    }

    // --- formats ----------------------------------------------------------

    /// `scroll_position`.
    #[must_use]
    pub fn scroll_position(&self) -> usize {
        self.oy
    }

    /// `copy_cursor_x`, `copy_cursor_y`.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.cx, self.cy)
    }

    /// `rectangle_toggle`.
    #[must_use]
    pub fn rectangle(&self) -> bool {
        self.rectflag
    }

    /// `selection_mode`.
    #[must_use]
    pub fn selection_mode(&self) -> SelectionMode {
        self.selflag
    }

    /// The `selection_*` variables, `None` without a selection
    /// (window-copy.c:1136-1153).
    #[must_use]
    pub fn selection(&self) -> Option<SelectionInfo> {
        self.sel.map(|_| SelectionInfo {
            start: self.selpos,
            end: self.endpos,
            active: self.drag != Drag::None,
            present: self.selpos != self.endpos,
        })
    }

    /// `search_present`, `search_timed_out`, and `search_count` with
    /// `search_count_partial` unless the count is unknown.
    #[must_use]
    pub fn search_info(&self) -> (bool, bool, Option<(i64, bool)>) {
        let s = &self.search;
        (
            s.marks.is_some(),
            s.timeout,
            (s.count != -1).then_some((s.count, s.more)),
        )
    }

    /// `window_copy_line_number_mode`.
    fn line_number_mode(&self, option: i64) -> i64 {
        match self.line_numbers {
            0 => 0,
            2 if option == 0 => 1,
            _ => option,
        }
    }

    /// `copy_line_numbers`.
    #[must_use]
    pub fn line_numbers_active(&self, option: i64) -> bool {
        self.line_number_mode(option) != 0
    }

    /// `copy_position` and `copy_position_limit`
    /// (`window_copy_formats`, window-copy.c:1106-1130).
    #[must_use]
    pub fn position(&self, option: i64) -> (usize, usize) {
        let hsize = self.hsize();
        if self.line_number_mode(option) >= 2 {
            (hsize - self.oy + 1, hsize + self.sy)
        } else {
            (self.oy, hsize)
        }
    }

    /// `top_line_time`: when the top line of the view scrolled into
    /// history, 0 for a line of the screen.
    #[must_use]
    pub fn top_line_time(&self) -> i64 {
        self.backing.time(self.hsize() - self.oy)
    }

    /// `copy_cursor_line` (`window_copy_get_line`, `format_grid_line`).
    #[must_use]
    pub fn cursor_line(&self) -> String {
        self.row(self.abs(self.cy)).text()
    }

    /// `copy_cursor_word` (`window_copy_get_word`, `format_grid_word`):
    /// the word around the cursor.
    #[must_use]
    pub fn cursor_word(&self, separators: &str) -> String {
        self.word_at(self.cx, self.cy, separators)
    }

    /// `window_copy_get_word` (window-copy.c:1034-1041): the word at
    /// screen position `(x, y)` of the view (`mouse_word`).
    #[must_use]
    pub fn word_at(&self, x: usize, y: usize, separators: &str) -> String {
        let Some(line) = (self.hsize() + y).checked_sub(self.oy) else {
            return String::new();
        };
        super::reader::grid_word(
            &|y| self.row(y).clone(),
            self.last_row(),
            self.backing.sx(),
            (x, line),
            separators,
        )
    }

    /// `window_copy_get_hyperlink` (window-copy.c:1052-1061): the link of
    /// the cell at screen position `(x, y)` of the view.
    #[must_use]
    pub fn hyperlink_at(&self, x: usize, y: usize) -> Option<String> {
        let line = (self.hsize() + y).checked_sub(self.oy)?;
        self.backing.hyperlink(x, line)
    }

    /// `window_copy_get_line` (window-copy.c:1043-1050): the line at
    /// screen row `y` of the view (`mouse_line`).
    #[must_use]
    pub fn line_at(&self, y: usize) -> String {
        (self.hsize() + y)
            .checked_sub(self.oy)
            .map(|line| self.row(line).text())
            .unwrap_or_default()
    }

    // --- drawing ----------------------------------------------------------

    /// The redraw of a resize that grew the window drew the indicator while
    /// the window still had its old size (resize.c `resize_window` resizes
    /// the layout, and so the pane and its mode, before the window).
    /// `format_draw` copies the indicator with `screen_write_fast_copy`,
    /// which stops after the first cell outside the window
    /// (`window_position_is_visible`, screen-write.c:701-707): the cells
    /// after column `last` (of the pane; `last` is the old window width
    /// less the pane's offset) are left out, but the indicator's first
    /// cell is always drawn. It stays so until the mode draws the
    /// indicator again.
    pub fn clip_position(&mut self, last: usize) {
        self.position_clip = Some(last);
    }

    /// The last column of the indicator to draw, given its first cell's.
    #[must_use]
    pub fn position_clip(&self, first: usize) -> Option<usize> {
        self.position_clip.map(|last| last.max(first))
    }

    /// Whether the position indicator is drawn (`!data->hide_position`).
    #[must_use]
    pub fn shows_position(&self) -> bool {
        !self.hide_position
    }

    /// Whether screen cell `(x, y)` is in the selection
    /// (`screen_check_selection`).
    #[must_use]
    pub fn selected(&self, x: usize, y: usize) -> bool {
        self.sel.is_some_and(|s| s.contains(x, y))
    }

    /// Which screen cells are selected or painted for the mark or a
    /// search, row-major; empty without any. tmux's terminal shows those
    /// cells written, never erased with the blanks around them (reference
    /// tmux 3.8: the blank end of the mark's line is written as spaces in
    /// `copy-mode-mark-style`). `numbers` is `copy-mode-line-numbers`:
    /// with line numbers on, tmux writes every line into the mode's screen
    /// and redraws the pane from it (`window_copy_redraw_lines`,
    /// `PANE_REDRAW`), where only selected cells are always written
    /// (`tty_draw_line_get_empty`), so the mark's blanks are erased.
    #[must_use]
    pub fn selection_mask(&self, vi: bool, numbers: i64) -> Vec<bool> {
        let selection = self.sel.is_some_and(|s| !s.hidden);
        let numbered = self.number_width(numbers) != 0;
        if !selection && (numbered || (!self.show_mark && self.search.marks.is_none())) {
            return Vec::new();
        }
        if numbered {
            return (0..self.sy)
                .flat_map(|y| (0..self.sx).map(move |x| self.selected(x, y)))
                .collect();
        }
        let text = self.text_columns(numbers);
        (0..self.sy)
            .flat_map(|y| (0..self.sx).map(move |x| (x, y)))
            .map(|(x, y)| {
                self.selected(x, y)
                    || (text.contains(&x) && self.paint_of(x - text.start, y, vi).is_some())
            })
            .collect()
    }

    /// The screen columns the text is drawn in (`window_copy_write_line`'s
    /// `px` and `content_sx`): after the line numbers' gutter, at least one.
    /// `numbers` is `copy-mode-line-numbers`.
    #[must_use]
    pub fn text_columns(&self, numbers: i64) -> std::ops::Range<usize> {
        let width = self.number_width(numbers);
        let first = width.min(self.sx - 1);
        let n = if width >= self.sx { 1 } else { self.sx - width };
        first..first + n
    }

    /// The number in the gutter of screen row `py` (window-copy.c:
    /// 5450-5466): `default` counts from the view's offset, `absolute`
    /// is the line in the history, `relative` the distance from the
    /// cursor's row, `hybrid` relative but absolute on the cursor's row.
    fn line_number(&self, numbers: i64, py: usize) -> usize {
        let absolute = self.hsize() - self.oy + py + 1;
        match self.line_number_mode(numbers) {
            1 => py.abs_diff(self.oy),
            2 => absolute,
            4 if py == self.cy => absolute,
            _ => py.abs_diff(self.cy),
        }
    }

    /// How cell `(x, py)` of the view is painted before the selection
    /// (`window_copy_update_style`): the mark's line in the mark style, a
    /// search match in the match or current match style, the mark's own
    /// cell with the colours swapped. `vi` is `mode-keys` now.
    fn paint_of(&self, x: usize, py: usize, vi: bool) -> Option<Paint> {
        let fy = self.abs(py);
        let inverse = self.show_mark && fy == self.mark.1 && x == self.mark.0;
        let mark = (self.show_mark && fy == self.mark.1).then_some(Paint::Mark { inverse });
        match self.match_paint(x, fy, vi) {
            Some(true) => Some(Paint::CurrentMatch { inverse }),
            Some(false) => Some(Paint::Match { inverse }),
            None => mark,
        }
    }

    /// The mode's screen (`window_copy_write_line` for every row, without
    /// the indicator): the line numbers' gutter in their styles when on,
    /// the backing rows in view after it, the cursor's row painted
    /// [`Paint::CurrentLine`] and `paint` applied to the marked, matched
    /// and selected cells (`window_copy_update_style`,
    /// `screen_select_cell`), and the cursor. `vi` is `mode-keys` now,
    /// `numbers` `copy-mode-line-numbers`.
    #[must_use]
    pub fn screen(&self, vi: bool, numbers: i64, paint: &dyn Fn(&mut Cell, Paint)) -> Snapshot {
        let (sx, sy) = (self.sx, self.sy);
        let width = self.number_width(numbers);
        let text = self.text_columns(numbers);
        let nx = text.len();
        let blank = || Cell {
            width: 1,
            ..Cell::default()
        };
        let mut cells = Vec::with_capacity(sx * sy);
        for py in 0..sy {
            // screen_write_nputs: "%*u " in the gutter, cut at the edge.
            // Each cell goes through screen_write_cell, which selects it as
            // screen_check_selection says (screen-write.c:2765): a
            // selection set while the gutter was narrower (its `clipx`)
            // covers it.
            if width != 0 {
                let current = py == self.cy;
                let label = format!("{:>w$} ", self.line_number(numbers, py), w = width - 1);
                for (x, ch) in label.chars().take(width.min(sx)).enumerate() {
                    let mut c = Cell {
                        text: ch.to_string().as_str().into(),
                        width: 1,
                        ..Cell::default()
                    };
                    paint(&mut c, Paint::LineNumber { current });
                    if self.selected(x, py) {
                        paint(&mut c, Paint::Selection);
                    }
                    cells.push(c);
                }
                // A gutter as wide as the pane: the text's one column
                // goes over its last cell.
                cells.truncate(py * sx + text.start);
            }
            let row = self.row(self.abs(py));
            // Columns left of the TAB cell being drawn as spaces, and
            // where it starts: all of it takes the style of its first
            // column (window_copy_write_one writes the TAB cell's width in
            // spaces with the cell window_copy_update_style gave it).
            let mut tab_left = 0;
            let mut tab_at = 0;
            for fx in 0..nx {
                let mut c = row.cells.get(fx).cloned().unwrap_or_else(blank);
                // window_copy_write_one: a TAB cell is written as its width
                // in spaces and its padding is skipped; one cut by the edge
                // is a single space. A cell cut by the edge is written as
                // grid_default_cell (window-copy.c:5242), with no current
                // line, mark or match style; only the selection applies.
                let mut cut = false;
                if c.tab {
                    tab_left = usize::from(c.width);
                    tab_at = fx;
                    if fx + tab_left > nx {
                        c = blank();
                        cut = true;
                    }
                }
                let styled_at = if tab_left > 0 { tab_at } else { fx };
                if tab_left > 0 {
                    tab_left -= 1;
                    c = Cell {
                        text: " ".into(),
                        width: 1,
                        tab: false,
                        ..c
                    };
                }
                // window_copy_write_one: a wide character cut by the edge is
                // a space.
                if c.width == 2 && fx + 1 >= nx {
                    c = blank();
                    cut = true;
                }
                if !cut {
                    if py == self.cy {
                        paint(&mut c, Paint::CurrentLine);
                    }
                    if let Some(p) = self.paint_of(styled_at, py, vi) {
                        paint(&mut c, p);
                    }
                }
                if self.selected(text.start + fx, py) {
                    paint(&mut c, Paint::Selection);
                }
                cells.push(c);
            }
        }
        let clamp = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        Snapshot {
            cols: clamp(sx),
            rows: clamp(sy),
            cells,
            wrapped: vec![false; sy],
            cursor: Some((
                clamp(self.offset_by(width, self.cx).min(sx - 1)),
                clamp(self.cy.min(sy - 1)),
            )),
            cursor_visible: true,
        }
    }

    /// A cursor past the last column of the text shows as `$` in the
    /// pane's last column, drawn last (window-copy.c:5483-5487), in the
    /// selection's style if selected. Written over the second half of a
    /// wide character, it clears the character (`screen_write_overwrite`,
    /// screen-write.c:3007-3025), so the screen says what the terminal
    /// shows and the character is drawn again once the `$` goes.
    pub fn mark_overflow(
        &self,
        screen: &mut Snapshot,
        numbers: i64,
        paint: &dyn Fn(&mut Cell, Paint),
    ) {
        if self.cx < self.text_columns(numbers).len() || self.cy >= self.sy {
            return;
        }
        let row = self.cy * self.sx;
        let mut x = self.sx - 1;
        while x > 0 && screen.cells.get(row + x).is_some_and(|c| c.width == 0) {
            x -= 1;
            if let Some(c) = screen.cells.get_mut(row + x) {
                *c = Cell {
                    width: 1,
                    ..Cell::default()
                };
            }
        }
        let selected = self.selected(self.sx - 1, self.cy);
        if let Some(c) = screen.cells.get_mut(row + self.sx - 1) {
            *c = Cell {
                text: "$".into(),
                width: 1,
                ..Cell::default()
            };
            if selected {
                paint(c, Paint::Selection);
            }
        }
    }

    // --- commands ---------------------------------------------------------

    /// `window_copy_command`: run `name`, `prefix` times where it repeats.
    /// Unknown names, and arguments a command does not take, do nothing,
    /// as in tmux.
    ///
    /// # Errors
    /// A copy mode command tmux has that vtmux does not yet.
    pub fn command(
        &mut self,
        env: &Env,
        name: &str,
        args: &[String],
        read_only_client: bool,
    ) -> Result<Outcome, CommandError> {
        // Before anything else, and before the repeat count is used up:
        // the refusal leaves the mode as it was (window-copy.c:3921-3929).
        if read_only_client && TMUX_COMMANDS.contains(&name) && !READ_ONLY_COMMANDS.contains(&name)
        {
            return Err(CommandError::ReadOnly);
        }
        let np = std::mem::replace(&mut self.prefix, 1).max(1);
        // tmux draws the indicator again (window_copy_write_line, row 0)
        // for a command; vtmux draws the whole screen after every one.
        self.position_clip = None;
        let Some(args) = parse_args(name, args) else {
            return Ok(Action::Stay.into());
        };
        let outcome = self.run(env, name, &args, np);
        if outcome.is_none() && TMUX_COMMANDS.contains(&name) {
            return Err(CommandError::NotImplemented(name.to_owned()));
        }
        // window_copy_command: any command but a search clears the marks
        // as its table entry says (window-copy.c:3953-3963).
        if !name.starts_with("search-") && self.search.marks.is_some() {
            let clear = match clear_kind(name) {
                Clear::EmacsOnly if env.vi => Clear::Never,
                c => c,
            };
            if clear != Clear::Never {
                self.clear_marks();
                self.search.origin = None;
            }
        }
        Ok(outcome.unwrap_or_else(|| Action::Stay.into()))
    }

    #[allow(clippy::too_many_lines)] // one arm per command
    fn run(&mut self, env: &Env, name: &str, args: &Args, np: u32) -> Option<Outcome> {
        let repeat = |m: &mut Self, f: &mut dyn FnMut(&mut Self)| {
            for _ in 0..np {
                f(m);
            }
        };
        let separators = env.separators.as_str();
        match name {
            "cancel" => return Some(Action::Cancel.into()),
            "cursor-up" => repeat(self, &mut |m| m.cursor_up(env, false)),
            "cursor-down" => repeat(self, &mut |m| m.cursor_down(env, false)),
            "cursor-left" => repeat(self, &mut |m| m.cursor_left(env)),
            "cursor-right" => repeat(self, &mut |m| m.cursor_right(env)),
            "cursor-down-and-cancel" => {
                let cy = self.cy;
                repeat(self, &mut |m| m.cursor_down(env, false));
                if cy == self.cy && self.oy == 0 {
                    return Some(Action::Cancel.into());
                }
            }
            "start-of-line" => self.cursor_start_of_line(env),
            "back-to-indentation" => self.cursor_back_to_indentation(env),
            "end-of-line" => self.cursor_end_of_line(env),
            "top-line" | "middle-line" | "bottom-line" => {
                self.cx = 0;
                self.cy = match name {
                    "top-line" => 0,
                    "middle-line" => (self.sy - 1) / 2,
                    _ => self.sy - 1,
                };
                self.update_selection(env, false);
            }
            "cursor-centre-vertical" | "cursor-centre-horizontal" => {
                if name.ends_with("vertical") {
                    self.update_cursor(env, self.cx, self.sy / 2);
                } else {
                    self.update_cursor(env, self.sx / 2, self.cy);
                }
                self.update_selection(env, false);
            }
            "page-up" => repeat(self, &mut |m| m.pageup(env, false)),
            "halfpage-up" => repeat(self, &mut |m| m.pageup(env, true)),
            "page-down" | "halfpage-down" | "page-down-and-cancel" | "halfpage-down-and-cancel" => {
                let half = name.starts_with("half");
                let exit = name.ends_with("-and-cancel") || self.scroll_exit;
                for _ in 0..np {
                    if self.pagedown(env, half, exit) {
                        return Some(Action::Cancel.into());
                    }
                }
            }
            "history-top" => {
                if self.lineflag == LineSel::LeftRight && self.abs(self.cy) == self.selpos.1 {
                    self.other_end(env);
                }
                self.scrolled |= self.oy != self.hsize();
                (self.cx, self.cy, self.oy) = (0, 0, self.hsize());
                self.refresh_marks();
                self.update_selection(env, false);
            }
            "history-bottom" => {
                if self.lineflag == LineSel::RightLeft && self.abs(self.cy) == self.endpos.1 {
                    self.other_end(env);
                }
                self.cy = self.sy - 1;
                self.cx = self.cursor_limit(env, self.hsize() + self.cy, false);
                self.scrolled |= self.oy != 0;
                self.oy = 0;
                self.refresh_marks();
                self.update_selection(env, false);
            }
            "scroll-up" => {
                if self.oy != self.hsize() {
                    if self.sel.is_some() && !env.dragging {
                        // With a selection, only the view moves.
                        self.drag = Drag::None;
                        self.lineflag = LineSel::None;
                        self.scroll_down(env, np as usize);
                    } else {
                        repeat(self, &mut |m| m.cursor_up(env, true));
                    }
                }
            }
            "scroll-down" => {
                if self.oy == 0 {
                    let exit = self.scroll_exit && self.sel.is_none();
                    return Some(if exit { Action::Cancel } else { Action::Stay }.into());
                }
                if self.sel.is_some() && !env.dragging {
                    self.drag = Drag::None;
                    self.lineflag = LineSel::None;
                    self.scroll_up(env, np as usize);
                    return Some(Action::Stay.into());
                }
                repeat(self, &mut |m| m.cursor_down(env, true));
                // A drag's selection keeps the mode (window_copy_cmd_scroll_down).
                if self.scroll_exit && self.oy == 0 && self.sel.is_none() {
                    return Some(Action::Cancel.into());
                }
            }
            "scroll-down-and-cancel" => {
                if self.sel.is_some() && !env.dragging {
                    self.drag = Drag::None;
                    self.lineflag = LineSel::None;
                    self.scroll_up(env, np as usize);
                    return Some(Action::Stay.into());
                }
                repeat(self, &mut |m| m.cursor_down(env, true));
                if self.oy == 0 {
                    return Some(Action::Cancel.into());
                }
            }
            "scroll-to-mouse" => {
                // window_copy_cmd_scroll_to_mouse: needs a client.
                if let Some(slider) = env.slider {
                    let exit = args.flag('e');
                    if self.scroll_slider(env, slider, exit) {
                        return Some(Action::Cancel.into());
                    }
                }
            }
            "scroll-top" => self.scroll_to(env, 0),
            "scroll-middle" => self.scroll_to(env, (self.sy - 1) / 2),
            "scroll-bottom" => self.scroll_to(env, self.sy - 1),
            "scroll-exit-on" => self.scroll_exit = true,
            "scroll-exit-off" => self.scroll_exit = false,
            "scroll-exit-toggle" => self.scroll_exit = !self.scroll_exit,
            "toggle-position" => self.hide_position = !self.hide_position,
            "goto-line" => {
                if let Some(line) = args.positional(0).filter(|a| !a.is_empty()) {
                    self.goto_line(env, line);
                }
            }
            "recentre-top-bottom" => self.recentre_top_bottom(env),
            "line-numbers-on" => self.set_line_numbers(env, true, true),
            "line-numbers-off" => self.set_line_numbers(env, false, false),
            "line-numbers-toggle" => {
                let on = !self.line_numbers_active(env.line_numbers);
                self.set_line_numbers(env, on, true);
            }
            "refresh-toggle" if self.refresh_active() => self.refresh_due = None,
            "refresh-on" | "refresh-toggle" => self.refresh_start(),
            "refresh-off" => self.refresh_due = None,
            "refresh-now" => {
                if !self.foreign {
                    return Some(Outcome {
                        refresh: true,
                        ..Outcome::from(Action::Stay)
                    });
                }
            }

            // Selections.
            "begin-selection" if env.mouse => {
                return Some(Outcome {
                    start_drag: true,
                    ..Outcome::from(Action::Stay)
                });
            }
            "begin-selection" => {
                self.lineflag = LineSel::None;
                self.selflag = SelectionMode::Char;
                self.start_selection(env);
            }
            "stop-selection" => {
                self.drag = Drag::None;
                self.lineflag = LineSel::None;
                self.selflag = SelectionMode::Char;
            }
            "clear-selection" => self.clear_selection(env),
            "other-end" => {
                self.selflag = SelectionMode::Char;
                if !np.is_multiple_of(2) {
                    self.other_end(env);
                }
            }
            "rectangle-on" | "rectangle-off" | "rectangle-toggle" => {
                self.lineflag = LineSel::None;
                let on = match name {
                    "rectangle-on" => true,
                    "rectangle-off" => false,
                    _ => !self.rectflag,
                };
                self.rectangle_set(env, on);
            }
            "selection-mode" => self.selection_mode_cmd(env, args.positional(0)),
            "select-line" => self.select_line(env, np),
            "select-word" => self.select_word(env),

            // Word and space movement.
            "next-word" => repeat(self, &mut |m| m.cursor_next_word(env, separators)),
            "next-space" => repeat(self, &mut |m| m.cursor_next_word(env, "")),
            "next-word-end" => {
                repeat(self, &mut |m| {
                    m.cursor_next_word_end(env, separators, false);
                });
            }
            "next-space-end" => repeat(self, &mut |m| m.cursor_next_word_end(env, "", false)),
            "previous-word" => {
                repeat(self, &mut |m| m.cursor_previous_word(env, separators, true));
            }
            "previous-space" => repeat(self, &mut |m| m.cursor_previous_word(env, "", true)),

            // Copying.
            "copy-selection-no-clear" | "copy-selection" | "copy-selection-and-cancel" => {
                let copied = copied(self.get_selection(env), args.positional(0), args);
                let action = match name {
                    "copy-selection-no-clear" => Action::Stay,
                    "copy-selection" => {
                        self.clear_selection(env);
                        Action::Stay
                    }
                    _ => {
                        self.clear_selection(env);
                        Action::Cancel
                    }
                };
                return Some(Outcome {
                    action,
                    copied,
                    pipe: None,
                    refresh: false,
                    start_drag: false,
                });
            }
            "copy-pipe-no-clear" | "copy-pipe" | "copy-pipe-and-cancel" => {
                let data = self.get_selection(env);
                let pipe = Pipe {
                    command: args.positional(0).unwrap_or_default().to_owned(),
                    data: data.clone().unwrap_or_default(),
                };
                let copied = copied(data, args.positional(1), args);
                return Some(self.after_pipe(env, name, copied, pipe));
            }
            "pipe-no-clear" | "pipe" | "pipe-and-cancel" => {
                let pipe = Pipe {
                    command: args.positional(0).unwrap_or_default().to_owned(),
                    data: self.get_selection(env).unwrap_or_default(),
                };
                return Some(self.after_pipe(env, name, None, pipe));
            }
            "append-selection" | "append-selection-and-cancel" => {
                let copied = self.get_selection(env).map(|data| Copied {
                    data,
                    prefix: None,
                    paste: true,
                    clip: true,
                    append: true,
                });
                self.clear_selection(env);
                let action = if name.ends_with("-and-cancel") {
                    Action::Cancel
                } else {
                    Action::Stay
                };
                return Some(Outcome {
                    action,
                    copied,
                    pipe: None,
                    refresh: false,
                    start_drag: false,
                });
            }
            "copy-line"
            | "copy-line-and-cancel"
            | "copy-end-of-line"
            | "copy-end-of-line-and-cancel"
            | "copy-pipe-line"
            | "copy-pipe-line-and-cancel"
            | "copy-pipe-end-of-line"
            | "copy-pipe-end-of-line-and-cancel" => {
                return Some(self.copy_lines(env, args, np, name));
            }

            // Search.
            "search-forward"
            | "search-backward"
            | "search-forward-text"
            | "search-backward-text" => {
                if let Some(string) = args.positional(0).filter(|s| !s.is_empty()) {
                    self.search.string = Some(string.to_owned());
                    let down = name.starts_with("search-forward");
                    let regex = !name.ends_with("-text");
                    self.search.kind = if down {
                        SearchKind::Down
                    } else {
                        SearchKind::Up
                    };
                    self.search.regex = regex;
                    self.search.timeout = false;
                    for _ in 0..np {
                        self.search(env, down, regex);
                    }
                }
            }
            "search-again" | "search-reverse" => {
                let reverse = name == "search-reverse";
                let down = match self.search.kind {
                    SearchKind::Off => return Some(Action::Stay.into()),
                    SearchKind::Down => !reverse,
                    SearchKind::Up => reverse,
                };
                for _ in 0..np {
                    self.search(env, down, self.search.regex);
                }
            }
            "search-forward-incremental" | "search-backward-incremental" => {
                let arg = args.positional(0).unwrap_or_default();
                self.search_incremental(env, arg, name.contains("forward"));
            }

            // Jumps.
            "jump-forward" | "jump-backward" | "jump-to-forward" | "jump-to-backward" => {
                let arg = args.positional(0).unwrap_or_default();
                if !arg.is_empty() {
                    self.jump = match name {
                        "jump-forward" => Jump::Forward,
                        "jump-backward" => Jump::Backward,
                        "jump-to-forward" => Jump::ToForward,
                        _ => Jump::ToBackward,
                    };
                    // utf8_fromcstr: the jump looks for the first character.
                    arg.clone_into(&mut self.jumpchar);
                    let kind = self.jump;
                    repeat(self, &mut |m| m.cursor_jump(env, kind));
                }
            }
            "jump-again" | "jump-reverse" => {
                let kind = match (self.jump, name == "jump-reverse") {
                    (Jump::Off, _) => return Some(Action::Stay.into()),
                    (k, false) => k,
                    (Jump::Forward, true) => Jump::Backward,
                    (Jump::Backward, true) => Jump::Forward,
                    (Jump::ToForward, true) => Jump::ToBackward,
                    (Jump::ToBackward, true) => Jump::ToForward,
                };
                repeat(self, &mut |m| m.cursor_jump(env, kind));
            }
            "set-mark" => {
                self.mark = (self.cx, self.abs(self.cy));
                self.show_mark = true;
            }
            "jump-to-mark" => self.jump_to_mark(env),

            // Brackets, paragraphs and prompts.
            "previous-matching-bracket" => {
                for _ in 0..np {
                    if !self.previous_matching_bracket(env) {
                        break;
                    }
                }
            }
            "next-matching-bracket" => {
                for _ in 0..np {
                    if !self.next_matching_bracket(env) {
                        break;
                    }
                }
            }
            "previous-paragraph" => repeat(self, &mut |m| m.previous_paragraph(env)),
            "next-paragraph" => repeat(self, &mut |m| m.next_paragraph(env)),
            "previous-prompt" | "next-prompt" => {
                self.cursor_prompt(env, name == "next-prompt", args.flag('o'));
            }
            _ => return None,
        }
        Some(Action::Stay.into())
    }

    /// `window_copy_set_line_numbers1`.
    pub fn set_line_numbers(&mut self, env: &Env, enabled: bool, force: bool) {
        self.line_numbers = if !enabled {
            0
        } else if force && env.line_numbers == 0 {
            2
        } else {
            1
        };
    }

    // --- the selection ----------------------------------------------------

    /// `window_copy_start_selection`: both ends at the cursor, the end
    /// following it.
    fn start_selection(&mut self, env: &Env) {
        self.selpos = (self.cx, self.abs(self.cy));
        self.endpos = self.selpos;
        self.drag = Drag::End;
        self.set_selection(env, false);
    }

    /// `window_copy_adjust_selection`: `pos` on screen, clipped to the view
    /// (`Less` above it, `Greater` below).
    fn adjust_selection(&self, (x, y): Pos) -> (std::cmp::Ordering, Pos) {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let ty = self.hsize() - self.oy;
        if y < ty {
            (Less, (if self.rectflag { x } else { 0 }, 0))
        } else if y > ty + self.sy - 1 {
            let x = if self.rectflag { x } else { self.sx - 1 };
            (Greater, (x, self.sy - 1))
        } else {
            (Equal, (x, y - ty))
        }
    }

    /// `window_copy_set_selection`: follow the cursor, then place the
    /// selection on screen; whether any of it is in view.
    fn set_selection(&mut self, env: &Env, no_reset: bool) -> bool {
        self.synchronize_cursor(env, no_reset);
        let (start, (sx, sy)) = self.adjust_selection(self.selpos);
        let (end, (ex, ey)) = self.adjust_selection(self.endpos);
        if start == end && start != std::cmp::Ordering::Equal {
            if let Some(s) = &mut self.sel {
                s.hidden = true;
            }
            return false;
        }
        // window-copy.c:5939-5946: the selection is clear of the line
        // numbers' gutter and its columns are where the text is drawn.
        let clipx = self.line_number_width(env).min(self.sx - 1);
        let (sx, ex) = if self.line_numbers_active(env.line_numbers) {
            (self.cursor_offset(env, sx), self.cursor_offset(env, ex))
        } else {
            (sx, ex)
        };
        self.sel = Some(ScreenSel {
            sx,
            sy,
            ex,
            ey,
            rectangle: self.rectflag,
            vi: self.modekeys_vi,
            hidden: false,
            clipx,
        });
        true
    }

    /// `window_copy_update_selection`.
    fn update_selection(&mut self, env: &Env, no_reset: bool) -> bool {
        if self.sel.is_none() && self.lineflag == LineSel::None {
            return false;
        }
        self.set_selection(env, no_reset)
    }

    /// `window_copy_update_selection_view`: after the view moved, a
    /// selection the cursor does not drag keeps its text.
    fn update_selection_view(&mut self, env: &Env) {
        if self.drag != Drag::None {
            self.update_selection(env, false);
            return;
        }
        let (selpos, endpos) = (self.selpos, self.endpos);
        self.update_selection(env, true);
        (self.selpos, self.endpos) = (selpos, endpos);
    }

    /// `window_copy_synchronize_cursor`.
    fn synchronize_cursor(&mut self, env: &Env, no_reset: bool) {
        match self.drag {
            Drag::End => self.synchronize_cursor_end(env, false, no_reset),
            Drag::Start => self.synchronize_cursor_end(env, true, no_reset),
            Drag::None => {}
        }
    }

    /// `window_copy_synchronize_cursor_end`: the dragged end goes to the
    /// cursor, widened to a word or line in those modes.
    fn synchronize_cursor_end(&mut self, env: &Env, mut begin: bool, no_reset: bool) {
        let (mut xx, mut yy) = (self.cx, self.abs(self.cy));
        match self.selflag {
            SelectionMode::Word if !no_reset => {
                begin = false;
                let (dx, dy) = self.dpos;
                if dy > yy || (dy == yy && dx > xx) {
                    // Right to left.
                    (xx, yy) = self.previous_word_pos(&self.separators.clone());
                    begin = true;
                    self.endpos = self.endselr;
                } else {
                    if xx >= self.find_length(yy)
                        || self.backing.in_set(xx + 1, yy, WHITESPACE) == 0
                    {
                        (xx, yy) = self.next_word_end_pos(env, &self.separators.clone());
                    }
                    self.selpos = self.selr;
                }
            }
            SelectionMode::Line if !no_reset => {
                begin = false;
                if self.dpos.1 > yy {
                    xx = 0;
                    begin = true;
                    self.endpos = self.endselr;
                } else {
                    yy = yy.max(self.endselr.1);
                    xx = self.find_length(yy);
                    self.selpos = self.selr;
                }
            }
            _ => {}
        }
        if begin {
            self.selpos = (xx, yy);
        } else {
            self.endpos = (xx, yy);
        }
    }

    /// `window_copy_clear_selection`.
    fn clear_selection(&mut self, env: &Env) {
        self.sel = None;
        self.drag = Drag::None;
        self.lineflag = LineSel::None;
        self.selflag = SelectionMode::Char;
        let px = self.cursor_limit(env, self.abs(self.cy), self.rectflag);
        if self.cx > px {
            self.update_cursor(env, px, self.cy);
        }
    }

    /// `window_copy_other_end`: the cursor goes to the other end of the
    /// selection, which it then drags.
    fn other_end(&mut self, env: &Env) {
        if self.sel.is_none() && self.lineflag == LineSel::None {
            return;
        }
        self.lineflag = match self.lineflag {
            LineSel::LeftRight => LineSel::RightLeft,
            LineSel::RightLeft => LineSel::LeftRight,
            LineSel::None => LineSel::None,
        };
        self.drag = if self.drag == Drag::End {
            Drag::Start
        } else {
            Drag::End
        };
        let (to_x, to_y) = if self.drag == Drag::Start {
            self.selpos
        } else {
            self.endpos
        };
        self.cx = to_x;
        let top = self.hsize() - self.oy;
        if to_y < top {
            self.oy = self.hsize() - to_y;
            self.cy = 0;
        } else if to_y > top + self.sy {
            // As tmux: one line below the view is "on screen" (cy == sy).
            self.oy = self.hsize() + self.sy - 1 - to_y;
            self.cy = self.sy - 1;
        } else {
            self.cy = to_y - top;
        }
        let limit = self.cursor_limit(env, self.abs(self.cy), self.rectflag);
        self.cx = self.cx.min(limit);
        self.update_selection(env, true);
    }

    /// `window_copy_rectangle_set`.
    fn rectangle_set(&mut self, env: &Env, on: bool) {
        self.rectflag = on;
        let px = self.cursor_limit(env, self.abs(self.cy), self.rectflag);
        if self.cx > px {
            self.update_cursor(env, px, self.cy);
        }
        self.update_selection(env, false);
    }

    /// `window_copy_scroll_to` (window-copy.c:3981-4015): put the cursor at
    /// grid position `(px, py)`, scrolling it into view a quarter of the
    /// screen from the edge.
    fn scroll_to_pos(&mut self, env: &Env, px: usize, py: usize, no_redraw: bool) {
        let (hsize, sy) = (self.hsize(), self.sy);
        self.cx = px;
        let top = hsize - self.oy;
        if py >= top && py < top + sy {
            self.cy = py - top;
        } else {
            let gap = sy / 4;
            let offset = if py < sy {
                0
            } else if py > hsize + sy - gap {
                hsize
            } else {
                py + gap - sy
            };
            self.cy = py - offset;
            self.scrolled |= self.oy != hsize - offset;
            self.oy = hsize - offset;
        }
        if !no_redraw {
            self.refresh_marks();
        }
        self.update_selection(env, false);
    }

    /// `window_copy_cmd_selection_mode` (window-copy.c:2205-2282).
    fn selection_mode_cmd(&mut self, env: &Env, mode: Option<&str>) {
        let is = |a: &str, b: &str| {
            mode.is_some_and(|m| m.eq_ignore_ascii_case(a) || m.eq_ignore_ascii_case(b))
        };
        if mode.is_none() || is("char", "c") {
            self.selflag = SelectionMode::Char;
        } else if is("word", "w") {
            self.separators.clone_from(&env.separators);
            self.selflag = SelectionMode::Word;
        } else if is("line", "l") {
            self.selflag = SelectionMode::Line;
            if self.sel.is_none() {
                return;
            }
            // As select-line does, set up where the ends go back to when
            // the cursor changes direction.
            let fixed = if self.drag == Drag::Start {
                self.endpos
            } else {
                self.selpos
            };
            let (mut first, mut last) = (self.selpos, self.endpos);
            if last.1 < first.1 || (last.1 == first.1 && last.0 < first.0) {
                std::mem::swap(&mut first, &mut last);
            }
            let mut reader = Reader::new(&self.backing, first.0, first.1);
            reader.start_of_line(true);
            let first = reader.get();
            let mut reader = Reader::new(&self.backing, last.0, last.1);
            reader.end_of_line(true, false);
            let last = reader.get();
            self.rectflag = false;
            (self.selr, self.selpos) = (first, first);
            (self.endselr, self.endpos) = (last, last);
            let cursor = (self.cx, self.abs(self.cy));
            self.dpos = fixed;
            let before = cursor.1 < fixed.1 || (cursor.1 == fixed.1 && cursor.0 < fixed.0);
            if self.drag != Drag::None && before {
                self.lineflag = LineSel::RightLeft;
                self.drag = Drag::Start;
                self.scroll_to_pos(env, first.0, first.1, true);
            } else {
                self.lineflag = LineSel::LeftRight;
                if self.drag != Drag::None {
                    self.drag = Drag::End;
                    let limit = self.cursor_limit(env, last.1, false);
                    self.scroll_to_pos(env, limit, last.1, true);
                }
            }
            if self.drag == Drag::None {
                self.set_selection(env, false);
            }
        }
    }

    /// `window_copy_cmd_select_line`.
    fn select_line(&mut self, env: &Env, np: u32) {
        self.lineflag = LineSel::LeftRight;
        self.rectflag = false;
        self.selflag = SelectionMode::Line;
        self.dpos = (self.cx, self.abs(self.cy));
        self.cursor_start_of_line(env);
        self.selr = (self.cx, self.abs(self.cy));
        self.endselr.1 = self.selr.1;
        self.start_selection(env);
        self.cursor_end_of_line(env);
        self.endselr.1 = self.abs(self.cy);
        self.endselr.0 = self.find_length(self.endselr.1);
        for _ in 1..np {
            self.cursor_down(env, false);
            self.cursor_end_of_line(env);
        }
    }

    /// `window_copy_cmd_select_word`.
    fn select_word(&mut self, env: &Env) {
        self.lineflag = LineSel::LeftRight;
        self.rectflag = false;
        self.selflag = SelectionMode::Word;
        self.dpos = (self.cx, self.abs(self.cy));
        self.separators.clone_from(&env.separators);
        let separators = self.separators.clone();
        self.cursor_previous_word(env, &separators, false);
        let (px, py) = (self.cx, self.abs(self.cy));
        self.selr = (px, py);
        self.start_selection(env);
        // A word of one character.
        let mut next = (px + 1, py);
        if self.wrapped(next.1) && next.0 > self.backing.sx() - 1 {
            next = (0, next.1 + 1);
        }
        if px >= self.find_length(py) || self.backing.in_set(next.0, next.1, WHITESPACE) == 0 {
            self.cursor_next_word_end(env, &separators, true);
        } else {
            self.update_cursor(env, px, self.cy);
            self.update_selection(env, true);
        }
        self.endselr = (self.cx, self.abs(self.cy));
        if self.dpos.1 > self.endselr.1 {
            self.dpos = self.endselr;
        } else if self.dpos.0 > self.endselr.0 {
            self.dpos.0 = self.endselr.0;
        }
    }

    // --- the mouse --------------------------------------------------------

    /// `window_copy_line_number_width`: the columns line numbers take.
    fn line_number_width(&self, env: &Env) -> usize {
        self.number_width(env.line_numbers)
    }

    /// [`Self::line_number_width`] under `copy-mode-line-numbers` `option`.
    fn number_width(&self, option: i64) -> usize {
        if !self.line_numbers_active(option) {
            return 0;
        }
        let mut lines = self.hsize() + self.sy + 1;
        let mut digits = 1;
        while lines >= 10 {
            lines /= 10;
            digits += 1;
        }
        digits.max(3) + 1
    }

    /// `window_copy_cursor_unoffset`: screen column `vx` as a column of the
    /// text, past the line numbers.
    fn cursor_unoffset(&self, env: &Env, vx: usize) -> usize {
        let width = self.line_number_width(env);
        if width == 0 {
            return vx;
        }
        let content = if width >= self.sx { 1 } else { self.sx - width };
        if vx < width {
            0
        } else {
            (vx - width).min(content - 1)
        }
    }

    /// `window_copy_cursor_offset`: text column `cx` on screen.
    fn cursor_offset(&self, env: &Env, cx: usize) -> usize {
        self.offset_by(self.line_number_width(env), cx)
    }

    /// [`Self::cursor_offset`] past a gutter `width` columns wide.
    fn offset_by(&self, width: usize, cx: usize) -> usize {
        if width == 0 {
            return cx;
        }
        let content = if width >= self.sx { 1 } else { self.sx - width };
        if cx >= content {
            self.sx - 1
        } else {
            width + cx
        }
    }

    /// `window_copy_move_mouse` (window-copy.c:7059-7082): the cursor goes
    /// to the mouse, at `(x, y)` in the pane, before a command runs.
    pub fn move_mouse(&mut self, env: &Env, (x, y): (usize, usize)) {
        let x = self.cursor_unoffset(env, x);
        self.update_cursor(env, x, y);
    }

    /// `window_copy_mouse_in_selection` (window-copy.c:5763-5826): whether
    /// screen cell `(x, y)` is in the selection, and whether it is nearer
    /// its start or its end.
    #[allow(clippy::similar_names)] // tmux's names for the two ends
    fn mouse_in_selection(&self, env: &Env, x: usize, y: usize) -> (bool, bool, bool) {
        let Some(sel) = self.sel else {
            return (false, false, false);
        };
        let screeny = self.hsize() - self.oy;
        let selx = self.cursor_offset(env, self.selpos.0);
        if self.selpos.1 >= screeny && self.selpos.1 - screeny < self.sy {
            let sely = self.selpos.1 - screeny;
            if x == selx && y == sely {
                return (true, true, false);
            }
        }
        let endselx = self.cursor_offset(env, self.endpos.0);
        if self.endpos.1 >= screeny && self.endpos.1 - screeny < self.sy {
            let endsely = self.endpos.1 - screeny;
            if x == endselx && y == endsely {
                return (true, false, true);
            }
        }
        let cursorx = self.cursor_offset(env, self.cx);
        if (x != cursorx || y != self.cy) && !sel.contains(x, y) {
            return (false, false, false);
        }
        let width = i128::try_from(self.sx).unwrap_or(0) + 1;
        let at = |(px, py): Pos| {
            i128::try_from(py).unwrap_or(0) * width + i128::try_from(px).unwrap_or(0)
        };
        let mpos = at((self.cursor_unoffset(env, x), screeny + y));
        let dstart = (mpos - at(self.selpos)).abs();
        let dend = (mpos - at(self.endpos)).abs();
        (true, dstart <= dend, dstart > dend)
    }

    /// `window_copy_start_drag` (window-copy.c:7084-7151) once the mouse is
    /// at `(x, y)` in the pane (its last position, `cmd_mouse_at(..., 1)`):
    /// a selection starts there, or an end of the one there is taken, in
    /// the selection's mode; then the first [`Self::drag_update`] at `now`
    /// (`current`, the position now).
    pub fn start_drag(
        &mut self,
        env: &Env,
        (x, y): (usize, usize),
        current: Option<(usize, usize)>,
        now: Instant,
    ) {
        let (inside, on_start, on_end) = self.mouse_in_selection(env, x, y);
        let x = self.cursor_unoffset(env, x);
        let yg = self.hsize() + y - self.oy;
        if on_start
            || on_end
            || !inside
            || x < self.selr.0
            || x > self.endselr.0
            || yg != self.selr.1
        {
            self.lineflag = LineSel::None;
            self.selflag = SelectionMode::Char;
        }
        match self.selflag {
            SelectionMode::Word => {
                let (mut x, mut y) = (x, y);
                if !self.separators.is_empty() {
                    self.update_cursor(env, x, y);
                    let (px, py) = self.previous_word_pos(&self.separators.clone());
                    x = px;
                    // A word starting above the view: its top row (tmux's
                    // unsigned arithmetic wraps here).
                    y = (py + self.oy).saturating_sub(self.hsize());
                }
                self.update_cursor(env, x, y);
            }
            SelectionMode::Line => self.update_cursor(env, 0, y),
            SelectionMode::Char => {
                self.update_cursor(env, x, y);
                if inside {
                    if on_start {
                        self.drag = Drag::Start;
                    } else if on_end {
                        self.drag = Drag::End;
                    }
                    self.update_selection(env, false);
                } else {
                    self.start_selection(env);
                }
            }
        }
        self.drag_update(env, current, now);
    }

    /// `window_copy_drag_update` (window-copy.c:7153-7197): the cursor and
    /// the selection follow the mouse at `at` (`None`: not in the pane); at
    /// the top or bottom row the view scrolls, and again on a timer.
    pub fn drag_update(&mut self, env: &Env, at: Option<(usize, usize)>, now: Instant) {
        self.drag_due = None;
        let Some((x, y)) = at else {
            return;
        };
        let x = self.cursor_unoffset(env, x);
        let old = (self.cx, self.cy);
        self.update_cursor(env, x, y);
        self.update_selection(env, false);
        if old.1 != self.cy || old.0 == self.cx {
            self.drag_scroll(env, y, now);
        }
    }

    /// The top or bottom row of a drag scrolls a line and arms the timer:
    /// `window_copy_cursor_up`/`_down` with `scroll_only` (in vi mode the
    /// cursor steps back a row first, so at the end of the history it
    /// leaves the edge row).
    fn drag_scroll(&mut self, env: &Env, y: usize, now: Instant) {
        if y == 0 {
            self.drag_due = Some(now + DRAG_REPEAT);
            self.cursor_up(env, true);
        } else if y == self.sy - 1 {
            self.drag_due = Some(now + DRAG_REPEAT);
            self.cursor_down(env, true);
        }
    }

    /// `window_copy_drag_release` (window-copy.c:7199-7219): with line
    /// numbers on the last position counts; the drag ends.
    pub fn drag_release(&mut self, env: &Env, at: Option<(usize, usize)>, now: Instant) {
        if self.line_numbers_active(env.line_numbers) {
            self.drag_update(env, at, now);
        }
        self.drag_due = None;
    }

    /// When the drag timer fires, while a drag waits at an edge.
    #[must_use]
    pub fn drag_due(&self) -> Option<Instant> {
        self.drag_due
    }

    /// `window_copy_scroll_timer` (window-copy.c:368-388): the cursor still
    /// at the top or bottom row scrolls another line.
    pub fn drag_timer(&mut self, env: &Env, now: Instant) {
        self.drag_due = None;
        let y = self.cy;
        self.drag_scroll(env, y, now);
    }

    /// `window_copy_get_selection` (window-copy.c:5974-6094): the selected
    /// text, `None` when there is none or it is empty.
    #[allow(clippy::similar_names)] // tmux's names for the four edges
    fn get_selection(&self, env: &Env) -> Option<Vec<u8>> {
        if self.sel.is_none() && self.lineflag == LineSel::None {
            // No selection: the search match at the cursor, if any
            // (`window_copy_match_at_cursor`).
            return self
                .search_match_at()
                .filter(|m| !m.is_empty())
                .map(String::into_bytes);
        }
        let ((sx, sy), (mut ex, ey)) = if self.endpos.1 < self.selpos.1
            || (self.endpos.1 == self.selpos.1 && self.endpos.0 < self.selpos.0)
        {
            (self.endpos, self.selpos)
        } else {
            (self.selpos, self.endpos)
        };
        let ey_last = self.find_length(ey);
        ex = ex.min(ey_last);
        // Like emacs, keep the top-left-most character and drop the
        // bottom-right-most; vi keeps both.
        let (firstsx, restsx, lastex, restex);
        if self.rectflag {
            // Leave out the cursor's column, so which side it is on counts.
            let selx = if self.drag == Drag::End {
                self.selpos.0
            } else {
                self.endpos.0
            };
            if selx < self.cx {
                lastex = if env.vi { self.cx + 1 } else { self.cx };
                restex = lastex;
                (firstsx, restsx) = (selx, selx);
            } else {
                (lastex, restex) = (selx + 1, selx + 1);
                (firstsx, restsx) = (self.cx, self.cx);
            }
        } else {
            lastex = if env.vi { ex + 1 } else { ex };
            restex = self.sx;
            (firstsx, restsx) = (sx, 0);
        }
        let mut buf = Vec::new();
        for i in sy..=ey {
            let from = if i == sy { firstsx } else { restsx };
            let to = if i == ey { lastex } else { restex };
            self.copy_line(&mut buf, i, from, to);
        }
        if buf.is_empty() {
            return None;
        }
        // The last newline goes (unless at the end in vi mode).
        if (!env.vi || lastex <= ey_last) && (!self.wrapped(ey) || lastex != ey_last) {
            buf.pop();
        }
        Some(buf)
    }

    /// `window_copy_copy_line`: columns `sx..ex` of line `y`, and a newline
    /// unless the line wraps onto the next and all of it was taken. A TAB
    /// cell is copied as `\t`.
    fn copy_line(&self, buf: &mut Vec<u8>, y: usize, sx: usize, ex: usize) {
        if sx > ex {
            return;
        }
        let row = self.row(y);
        let wrapped = row.wrapped && row.cells.len() <= self.backing.sx();
        let xx = if wrapped {
            row.cells.len()
        } else {
            self.find_length(y)
        };
        let (sx, ex) = (sx.min(xx), ex.min(xx));
        for x in sx..ex {
            let c = self.backing.cell(x, y);
            if c.tab {
                buf.push(b'\t');
            } else if c.width != 0 {
                buf.extend_from_slice(cell_text(&c).as_bytes());
            }
        }
        if !wrapped || ex != xx {
            buf.push(b'\n');
        }
    }

    /// What the pipe commands do after the job and the copy: the table's
    /// `copy-pipe-no-clear` (nothing), `copy-pipe` (clear) and
    /// `copy-pipe-and-cancel` (clear, cancel), and the `pipe` ones.
    fn after_pipe(&mut self, env: &Env, name: &str, copied: Option<Copied>, pipe: Pipe) -> Outcome {
        if !name.ends_with("-no-clear") {
            self.clear_selection(env);
        }
        let action = if name.ends_with("-and-cancel") {
            Action::Cancel
        } else {
            Action::Stay
        };
        Outcome {
            action,
            copied,
            pipe: Some(pipe),
            refresh: false,
            start_drag: false,
        }
    }

    /// `window_copy_do_copy_line` and `window_copy_do_copy_end_of_line`:
    /// select from the cursor (or its line's start) to the end of the line
    /// `np - 1` lines down, copy (or pipe), then put everything back.
    fn copy_lines(&mut self, env: &Env, args: &Args, np: u32, name: &str) -> Outcome {
        let saved = (self.cx, self.cy, self.oy);
        if name.starts_with("copy-line") || name.starts_with("copy-pipe-line") {
            self.selflag = SelectionMode::Char;
            self.cursor_start_of_line(env);
        }
        self.start_selection(env);
        for _ in 1..np {
            self.cursor_down(env, false);
        }
        self.cursor_end_of_line(env);
        let data = self.get_selection(env);
        let (pipe, prefix) = if name.starts_with("copy-pipe") {
            let pipe = Pipe {
                command: args.positional(0).unwrap_or_default().to_owned(),
                data: data.clone().unwrap_or_default(),
            };
            (Some(pipe), args.positional(1))
        } else {
            (None, args.positional(0))
        };
        let copied = copied(data, prefix, args);
        if name.ends_with("-and-cancel") {
            return Outcome {
                action: Action::Cancel,
                copied,
                pipe,
                refresh: false,
                start_drag: false,
            };
        }
        self.clear_selection(env);
        (self.cx, self.cy, self.oy) = saved;
        Outcome {
            action: Action::Stay,
            copied,
            pipe,
            refresh: false,
            start_drag: false,
        }
    }

    // --- movement ---------------------------------------------------------

    /// `window_copy_update_cursor`, the cursor part: clamped to the line
    /// unless a rectangle is on.
    fn update_cursor(&mut self, env: &Env, cx: usize, cy: usize) {
        let mut cx = cx;
        if !self.rectflag && cy < self.sy {
            cx = cx.min(self.cursor_limit(env, self.abs(cy), false));
        }
        (self.cx, self.cy) = (cx, cy);
    }

    /// `window_copy_scroll_up`: the view moves down `ny` lines (towards the
    /// screen).
    fn scroll_up(&mut self, env: &Env, ny: usize) {
        let ny = ny.min(self.oy);
        if ny == 0 {
            return;
        }
        self.oy -= ny;
        self.scrolled = true;
        self.refresh_marks();
        self.update_selection_view(env);
    }

    /// `window_copy_scroll_down`: the view moves up `ny` lines into history.
    fn scroll_down(&mut self, env: &Env, ny: usize) {
        let hsize = self.hsize();
        if ny > hsize {
            return;
        }
        let ny = ny.min(hsize - self.oy);
        if ny == 0 {
            return;
        }
        self.oy += ny;
        self.scrolled = true;
        self.refresh_marks();
        self.update_selection_view(env);
    }

    /// A line selection keeps the cursor at its line's end (left to right)
    /// or start (right to left) (window-copy.c:6547-6566).
    fn line_selection_cursor(&mut self, env: &Env) {
        match self.lineflag {
            LineSel::LeftRight => {
                let px = if self.rectflag {
                    self.backing.sx()
                } else {
                    self.find_length(self.abs(self.cy))
                };
                self.update_cursor(env, px, self.cy);
                self.update_selection(env, false);
            }
            LineSel::RightLeft => {
                self.update_cursor(env, 0, self.cy);
                self.update_selection(env, false);
            }
            LineSel::None => {}
        }
    }

    /// Remember where the cursor was on its line before a vertical move
    /// (`lastcx`, `lastsx`).
    fn remember_column(&mut self) {
        let ox = self.find_length(self.abs(self.cy));
        if self.cx != ox {
            self.lastcx = self.cx;
            self.lastsx = ox;
        }
    }

    /// Back at the end of the new line if the cursor was at the end.
    fn keep_end_of_line(&mut self, env: &Env) {
        let px = self.find_length(self.abs(self.cy));
        if (self.cx >= self.lastsx && self.cx != px) || self.cx > px {
            self.update_cursor(env, px, self.cy);
            self.update_selection(env, false);
        }
    }

    /// `window_copy_cursor_up` (window-copy.c:6489-6567).
    fn cursor_up(&mut self, env: &Env, scroll_only: bool) {
        let norectsel = self.sel.is_none() || !self.rectflag;
        if norectsel {
            self.remember_column();
        }
        if self.lineflag == LineSel::LeftRight && self.abs(self.cy) == self.selpos.1 {
            self.other_end(env);
        }
        if scroll_only && env.vi && self.cy < self.sy - 1 {
            self.update_cursor(env, self.cx, self.cy + 1);
        }
        if scroll_only || self.cy == 0 {
            if norectsel {
                self.cx = self.lastcx;
            }
            self.scroll_down(env, 1);
        } else {
            let cx = if norectsel { self.lastcx } else { self.cx };
            self.update_cursor(env, cx, self.cy - 1);
            self.update_selection(env, false);
        }
        if norectsel {
            self.keep_end_of_line(env);
        }
        self.line_selection_cursor(env);
    }

    /// `window_copy_cursor_down` (window-copy.c:6569-6639).
    fn cursor_down(&mut self, env: &Env, scroll_only: bool) {
        let norectsel = self.sel.is_none() || !self.rectflag;
        if norectsel {
            self.remember_column();
        }
        if self.lineflag == LineSel::RightLeft && self.abs(self.cy) == self.endpos.1 {
            self.other_end(env);
        }
        if scroll_only && env.vi && self.cy > 0 {
            self.update_cursor(env, self.cx, self.cy - 1);
        }
        if scroll_only || self.cy == self.sy - 1 {
            if norectsel {
                self.cx = self.lastcx;
            }
            self.scroll_up(env, 1);
        } else {
            let cx = if norectsel { self.lastcx } else { self.cx };
            self.update_cursor(env, cx, self.cy + 1);
            self.update_selection(env, false);
        }
        if norectsel {
            self.keep_end_of_line(env);
        }
        self.line_selection_cursor(env);
    }

    /// `window_copy_acquire_cursor_up`: scroll up if the reader's position
    /// `(px, py)` is above the view, then put the cursor there.
    fn acquire_cursor_up(&mut self, env: &Env, px: usize, py: usize) {
        let top = self.hsize() - self.oy;
        let (ny, cy) = if py < top {
            (top - py, 0)
        } else {
            (0, py - top)
        };
        for _ in 0..ny {
            self.cursor_up(env, true);
        }
        self.update_cursor(env, px, cy);
        self.update_selection(env, false);
    }

    /// `window_copy_acquire_cursor_down`.
    fn acquire_cursor_down(&mut self, env: &Env, px: usize, py: usize, no_reset: bool) {
        let cy = py + self.oy - self.hsize();
        let yy = self.sy - 1;
        for _ in 0..cy.saturating_sub(yy) {
            self.cursor_down(env, true);
        }
        self.update_cursor(env, px, cy.min(yy));
        self.update_selection(env, no_reset);
    }

    /// `window_copy_cursor_left`.
    fn cursor_left(&mut self, env: &Env) {
        let mut r = self.reader();
        r.cursor_left(true);
        let (px, py) = r.get();
        self.acquire_cursor_up(env, px, py);
    }

    /// `window_copy_cursor_right`: one past the end for emacs keys, to the
    /// edge in a rectangle.
    fn cursor_right(&mut self, env: &Env) {
        let all = self.sel.is_some() && self.rectflag;
        let mut r = self.reader();
        r.cursor_right(true, all, !env.vi);
        let (px, py) = r.get();
        self.acquire_cursor_down(env, px, py, false);
    }

    /// `window_copy_cursor_start_of_line`.
    fn cursor_start_of_line(&mut self, env: &Env) {
        let mut r = self.reader();
        r.start_of_line(true);
        let (px, py) = r.get();
        self.acquire_cursor_up(env, px, py);
    }

    /// `window_copy_cursor_back_to_indentation`.
    fn cursor_back_to_indentation(&mut self, env: &Env) {
        let mut r = self.reader();
        r.back_to_indentation();
        let (px, py) = r.get();
        self.acquire_cursor_up(env, px, py);
    }

    /// `window_copy_cursor_end_of_line`: the end of the wrapped line.
    fn cursor_end_of_line(&mut self, env: &Env) {
        let rect = self.sel.is_some() && self.rectflag;
        let mut r = self.reader();
        r.end_of_line(true, rect);
        let (mut px, py) = r.get();
        if !rect {
            px = self.cursor_limit(env, py, false);
        }
        self.acquire_cursor_down(env, px, py, false);
    }

    /// `window_copy_cursor_next_word`.
    fn cursor_next_word(&mut self, env: &Env, separators: &str) {
        let mut r = self.reader();
        r.next_word(separators);
        let (px, py) = r.get();
        self.acquire_cursor_down(env, px, py, false);
    }

    /// `window_copy_cursor_next_word_end_pos`: vi stops on the word's last
    /// character, emacs after it.
    fn next_word_end_pos(&self, env: &Env, separators: &str) -> Pos {
        let mut r = self.reader();
        if env.vi {
            if r.in_set(WHITESPACE) == 0 {
                r.cursor_right(false, false, false);
            }
            r.next_word_end(separators);
            r.cursor_left(true);
        } else {
            r.next_word_end(separators);
        }
        r.get()
    }

    /// `window_copy_cursor_next_word_end`.
    fn cursor_next_word_end(&mut self, env: &Env, separators: &str, no_reset: bool) {
        let (px, py) = self.next_word_end_pos(env, separators);
        self.acquire_cursor_down(env, px, py, no_reset);
    }

    /// `window_copy_cursor_previous_word_pos`.
    fn previous_word_pos(&self, separators: &str) -> Pos {
        let mut r = self.reader();
        r.previous_word(separators, false, true);
        r.get()
    }

    /// `window_copy_cursor_previous_word`: emacs keys stop at a line end
    /// with whitespace before it.
    fn cursor_previous_word(&mut self, env: &Env, separators: &str, already: bool) {
        let mut r = self.reader();
        r.previous_word(separators, already, !env.vi);
        let (px, py) = r.get();
        self.acquire_cursor_up(env, px, py);
    }

    /// The page size for page and half-page moves.
    fn page(&self, half: bool) -> usize {
        match (self.sy > 2, half) {
            (false, _) => 1,
            (true, true) => self.sy / 2,
            (true, false) => self.sy - 2,
        }
    }

    /// The shared start of the page moves: remember the column and go back
    /// to it.
    fn page_start(&mut self) {
        self.remember_column();
        self.cx = self.lastcx;
    }

    fn page_end(&mut self, env: &Env) {
        if self.sel.is_some() && self.rectflag {
            return;
        }
        let px = self.find_length(self.abs(self.cy));
        if (self.cx >= self.lastsx && self.cx != px) || self.cx > px {
            self.cursor_end_of_line(env);
        }
    }

    /// `window_copy_pageup1`.
    pub fn pageup(&mut self, env: &Env, half: bool) {
        self.page_start();
        let n = self.page(half);
        if self.oy + n > self.hsize() {
            self.oy = self.hsize();
            self.cy = self.cy.saturating_sub(n);
        } else {
            self.oy += n;
        }
        self.page_end(env);
        self.scrolled = true;
        self.refresh_marks();
        self.update_selection(env, false);
    }

    /// `window_copy_pagedown1`: whether the mode should exit.
    pub fn pagedown(&mut self, env: &Env, half: bool, scroll_exit: bool) -> bool {
        self.page_start();
        let n = self.page(half);
        if self.oy < n {
            self.oy = 0;
            // As tmux: the whole page, not what was left of it.
            self.cy = (self.cy + n).min(self.sy - 1);
        } else {
            self.oy -= n;
        }
        self.page_end(env);
        if scroll_exit && self.oy == 0 && self.sel.is_none() {
            return true;
        }
        self.scrolled = true;
        self.refresh_marks();
        self.update_selection(env, false);
        false
    }

    /// `window_copy_scroll1` (window-copy.c:776-877): the slider of a bar
    /// `sb_h` rows tall, grabbed at row `mpos` of it, follows the pointer
    /// to row `my` of the pane (`slider_h` is its height). The view moves
    /// by what that changes of the history offset, the cursor keeping its
    /// row as far as it can. Whether the mode should exit.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "tmux's float arithmetic, truncated to u_int"
    )]
    pub fn scroll_slider(
        &mut self,
        env: &Env,
        (my, mpos, slider_h, sb_h): (i64, i64, u32, u32),
        scroll_exit: bool,
    ) -> bool {
        let (slider_h, sb_h) = (i64::from(slider_h), i64::from(sb_h));
        let new_slider_y = if my <= mpos {
            0
        } else if my - mpos > sb_h - slider_h {
            sb_h - slider_h
        } else {
            my - mpos
        };
        let (offset, size) = self.current_offset();
        if sb_h == 0 {
            return false;
        }
        // The inverse of the slider's position (redraw_draw_scrollbar_span),
        // in tmux's float, stored in a u_int.
        let ratio = (size as f32 + sb_h as f32) / sb_h as f32;
        let new_offset = (new_slider_y as f32 * ratio) as u32;
        let delta = offset as i64 - i64::from(new_offset);
        self.page_start();
        let hsize = self.hsize();
        if delta >= 0 {
            let n = delta as usize;
            if self.oy + n > hsize {
                self.oy = hsize;
                self.cy = self.cy.saturating_sub(n);
            } else {
                self.oy += n;
            }
        } else {
            let n = (-delta) as usize;
            if self.oy < n {
                // As tmux: n less the old offset, after the offset is 0.
                self.oy = 0;
                self.cy = (self.cy + n).min(self.sy - 1);
            } else {
                self.oy -= n;
            }
        }
        self.drag = Drag::None;
        self.page_end(env);
        if scroll_exit && self.oy == 0 && self.sel.is_none() {
            return true;
        }
        self.scrolled = true;
        self.refresh_marks();
        self.update_selection_view(env);
        false
    }

    /// `window_copy_cmd_scroll_to`: scroll the cursor's line to row `to`.
    fn scroll_to(&mut self, env: &Env, to: usize) {
        if self.cy > to && self.oy >= self.cy - to {
            let delta = self.cy - to;
            self.scroll_up(env, delta);
            self.cy -= delta;
        } else if self.cy < to && self.hsize() - self.oy >= to - self.cy {
            let delta = to - self.cy;
            self.scroll_down(env, delta);
            self.cy += delta;
        }
        self.update_selection_view(env);
    }

    /// `window_copy_goto_line`.
    fn goto_line(&mut self, env: &Env, line: &str) {
        let Ok(lineno) = vtmux_core::cmd::strtonum(line, -1, i64::from(i32::MAX)) else {
            return;
        };
        let hsize = self.hsize();
        if self.line_number_mode(env.line_numbers) >= 2 {
            let line = usize::try_from(lineno).unwrap_or(0).clamp(1, hsize + 1);
            self.oy = hsize - (line - 1);
        } else {
            self.oy = usize::try_from(lineno)
                .ok()
                .filter(|&n| n <= hsize)
                .unwrap_or(hsize);
        }
        self.update_selection(env, false);
    }

    // --- search, jumps, marks, brackets, paragraphs, prompts --------------

    /// `window_copy_cmd_search_*_incremental` (window-copy.c:2936-3053):
    /// `arg` is a prefix (`=`, `+` or `-`, the direction) and the string so
    /// far; a changed string searches again from where the search began.
    fn search_incremental(&mut self, env: &Env, arg: &str, forward: bool) {
        self.search.timeout = false;
        let mut chars = arg.chars();
        let prefix = chars.next();
        let rest = chars.as_str();
        match self.search.origin {
            None => self.search.origin = Some((self.cx, self.cy, self.oy)),
            Some((x, y, o)) if self.search.string.as_deref().is_some_and(|s| s != rest) => {
                (self.cx, self.cy, self.oy) = (x, y, o);
                self.cx = self.cursor_limit(env, self.abs(self.cy), false);
            }
            Some(_) => {}
        }
        if rest.is_empty() {
            self.clear_marks();
            return;
        }
        let down = match prefix {
            Some('=') => forward,
            Some('+') => true,
            Some('-') => false,
            _ => return,
        };
        self.search.kind = if down {
            SearchKind::Down
        } else {
            SearchKind::Up
        };
        self.search.regex = false;
        self.search.string = Some(rest.to_owned());
        if !self.search(env, down, false) {
            self.clear_marks();
        }
    }

    /// `grid_reader_cell_equals_data`: the cell holds the jump character.
    fn cell_is_jumpchar(&self, x: usize, y: usize) -> bool {
        let Some(want) = self.jumpchar.chars().next() else {
            return false;
        };
        let c = self.backing.cell(x, y);
        if c.tab && want == '\t' {
            return true;
        }
        let mut text = cell_text(&c).chars();
        c.width != 0 && text.next() == Some(want) && text.next().is_none()
    }

    /// `grid_reader_cursor_jump`: the next jump character from `(px, py)`
    /// on, following wrapped lines.
    fn find_jump(&self, mut px: usize, py: usize) -> Option<Pos> {
        let last = self.last_row();
        for y in py..=last {
            let len = self.find_length(y);
            while px < len {
                if self.cell_is_jumpchar(px, y) {
                    return Some((px, y));
                }
                px += 1;
            }
            if y == last || !self.wrapped(y) {
                return None;
            }
            px = 0;
        }
        None
    }

    /// `grid_reader_cursor_jump_back`: the previous one at or before
    /// `(px, py)`.
    fn find_jump_back(&self, px: usize, py: usize) -> Option<Pos> {
        let mut xx = px + 1;
        let mut y = py + 1;
        while y > 0 {
            for x in (0..xx).rev() {
                if self.cell_is_jumpchar(x, y - 1) {
                    return Some((x, y - 1));
                }
            }
            if y == 1 || !self.wrapped(y - 2) {
                return None;
            }
            xx = self.find_length(y - 2);
            y -= 1;
        }
        None
    }

    /// `window_copy_cursor_jump`, `_jump_back`, `_jump_to`,
    /// `_jump_to_back`.
    fn cursor_jump(&mut self, env: &Env, kind: Jump) {
        let py = self.abs(self.cy);
        match kind {
            Jump::Off => {}
            Jump::Forward | Jump::ToForward => {
                let skip = if kind == Jump::Forward { 1 } else { 2 };
                if let Some((x, y)) = self.find_jump(self.cx + skip, py) {
                    let mut r = Reader::new(&self.backing, x, y);
                    if kind == Jump::ToForward {
                        r.cursor_left(true);
                    }
                    let (x, y) = r.get();
                    self.acquire_cursor_down(env, x, y, false);
                }
            }
            Jump::Backward | Jump::ToBackward => {
                let mut r = self.reader();
                r.cursor_left(false);
                if kind == Jump::ToBackward {
                    r.cursor_left(false);
                }
                let (x, y) = r.get();
                if let Some((x, y)) = self.find_jump_back(x, y) {
                    let mut r = Reader::new(&self.backing, x, y);
                    if kind == Jump::ToBackward {
                        r.cursor_right(true, false, !env.vi);
                    }
                    let (x, y) = r.get();
                    self.acquire_cursor_up(env, x, y);
                }
            }
        }
    }

    /// `window_copy_jump_to_mark`: the cursor and the mark change places.
    fn jump_to_mark(&mut self, env: &Env) {
        let here = (self.cx, self.abs(self.cy));
        let (mx, my) = self.mark;
        let hsize = self.hsize();
        self.cx = mx;
        if my < hsize {
            self.cy = 0;
            self.oy = hsize - my;
        } else {
            self.cy = my - hsize;
            self.oy = 0;
        }
        self.mark = here;
        self.show_mark = true;
        self.update_selection(env, false);
    }

    /// The cell's one-byte character, `None` for padding or a longer one
    /// (`gc.data.size != 1`).
    fn byte_at(&self, x: usize, y: usize) -> Option<u8> {
        let c = self.backing.cell(x, y);
        let t = cell_text(&c);
        (c.width != 0 && t.len() == 1).then(|| t.as_bytes()[0])
    }

    /// `window_copy_cmd_previous_matching_bracket`, one repeat; false to
    /// stop repeating (an empty line).
    fn previous_matching_bracket(&mut self, env: &Env) -> bool {
        const OPEN: &[u8] = b"{[(";
        const CLOSE: &[u8] = b"}])";
        let (mut px, mut py) = (self.cx, self.abs(self.cy));
        if self.find_length(py) == 0 {
            return false;
        }
        let mut tried = false;
        let found = loop {
            if let Some(b) = self.byte_at(px, py).filter(|b| CLOSE.contains(b)) {
                break b;
            }
            if !self.modekeys_vi {
                if !tried && px > 0 {
                    px -= 1;
                    tried = true;
                    continue;
                }
                self.cursor_previous_word(env, "}])", true);
            }
            return true;
        };
        let start = OPEN[CLOSE.iter().position(|&c| c == found).expect("close")];
        let mut n = 1;
        while n != 0 {
            if px == 0 {
                if py == 0 {
                    return true;
                }
                let mut xx;
                loop {
                    py -= 1;
                    xx = self.find_length(py);
                    if xx != 0 || py == 0 {
                        break;
                    }
                }
                if xx == 0 && py == 0 {
                    return true;
                }
                px = xx - 1;
            } else {
                px -= 1;
            }
            match self.byte_at(px, py) {
                Some(b) if b == found => n += 1,
                Some(b) if b == start => n -= 1,
                _ => {}
            }
        }
        self.scroll_to_pos(env, px, py, false);
        true
    }

    /// `window_copy_cmd_next_matching_bracket`, one repeat; false to stop.
    fn next_matching_bracket(&mut self, env: &Env) -> bool {
        const OPEN: &[u8] = b"{[(";
        const CLOSE: &[u8] = b"}])";
        let (mut px, mut py) = (self.cx, self.abs(self.cy));
        let mut xx = self.find_length(py);
        let yy = self.last_row();
        if xx == 0 {
            return false;
        }
        let mut tried = false;
        let found = loop {
            let b = self.byte_at(px, py);
            // vi: a closing bracket first goes back to its opening one,
            // or stays put if there is none.
            if self.modekeys_vi && b.is_some_and(|b| CLOSE.contains(&b)) {
                let saved = (self.cx, self.abs(self.cy));
                self.scroll_to_pos(env, px, py, false);
                self.previous_matching_bracket(env);
                let (cx, cy) = (self.cx, self.abs(self.cy));
                if self.byte_at(cx, cy).is_some_and(|b| CLOSE.contains(&b)) {
                    self.scroll_to_pos(env, saved.0, saved.1, false);
                }
                return false;
            }
            if let Some(b) = b.filter(|b| OPEN.contains(b)) {
                break b;
            }
            if !self.modekeys_vi {
                if !tried && px <= xx {
                    px += 1;
                    tried = true;
                    continue;
                }
                self.cursor_next_word_end(env, "{[(", false);
                return true;
            }
            // vi: on along the line, onto the lines it wraps onto.
            if px > xx {
                if py == yy || !self.wrapped(py) || self.row(py).cells.len() > self.backing.sx() {
                    return true;
                }
                px = 0;
                py += 1;
                xx = self.find_length(py);
            } else {
                px += 1;
            }
        };
        let end = CLOSE[OPEN.iter().position(|&c| c == found).expect("open")];
        let mut n = 1;
        while n != 0 {
            if px > xx {
                if py == yy {
                    return true;
                }
                px = 0;
                py += 1;
                xx = self.find_length(py);
            } else {
                px += 1;
            }
            match self.byte_at(px, py) {
                Some(b) if b == found => n += 1,
                Some(b) if b == end => n -= 1,
                _ => {}
            }
        }
        self.scroll_to_pos(env, px, py, false);
        true
    }

    /// `window_copy_previous_paragraph`.
    fn previous_paragraph(&mut self, env: &Env) {
        let mut oy = self.abs(self.cy);
        while oy > 0 && self.find_length(oy) == 0 {
            oy -= 1;
        }
        while oy > 0 && self.find_length(oy) > 0 {
            oy -= 1;
        }
        self.scroll_to_pos(env, 0, oy, false);
    }

    /// `window_copy_next_paragraph`.
    fn next_paragraph(&mut self, env: &Env) {
        let mut oy = self.abs(self.cy);
        let maxy = self.hsize() + self.sy - 1;
        while oy < maxy && self.find_length(oy) == 0 {
            oy += 1;
        }
        while oy < maxy && self.find_length(oy) > 0 {
            oy += 1;
        }
        let ox = self.find_length(oy);
        self.scroll_to_pos(env, ox, oy, false);
    }

    /// `window_copy_cursor_prompt`: to the start of the next or previous
    /// line where a prompt (or with `-o` command output) starts.
    fn cursor_prompt(&mut self, env: &Env, down: bool, output: bool) {
        let mut line = self.abs(self.cy);
        let end = if down { self.last_row() } else { 0 };
        loop {
            if line == end {
                return;
            }
            line = if down { line + 1 } else { line - 1 };
            if self.backing.starts(line, output) {
                break;
            }
        }
        let hsize = self.hsize();
        self.cx = 0;
        // As tmux: `line > hsize`, so the first screen line is shown from
        // the top of history's end.
        if line > hsize {
            self.cy = line - hsize;
            self.oy = 0;
        } else {
            self.cy = 0;
            self.oy = hsize - line;
        }
        self.update_selection(env, false);
    }

    /// `window_copy_cmd_recentre_top_bottom`: the cursor's line to the
    /// middle, then the top, then the bottom.
    fn recentre_top_bottom(&mut self, env: &Env) {
        let (cy, oy) = (self.cy, self.oy);
        let sy = self.sy - 1;
        let sm = sy / 2;
        let line = self.abs(cy);
        if self.recentre_line != line {
            self.recentre = Recentre::Middle;
            self.recentre_line = line;
        }
        let target = self.recentre;
        self.recentre = match target {
            Recentre::Middle => Recentre::Top,
            Recentre::Top => Recentre::Bottom,
            Recentre::Bottom => Recentre::Middle,
        };
        match target {
            Recentre::Middle => {
                if cy < sm {
                    self.scroll_down(env, sm - cy);
                } else if cy > sm {
                    self.scroll_up(env, cy - sm);
                }
                if self.oy != oy {
                    self.cy = cy + self.oy - oy;
                }
            }
            Recentre::Top => {
                self.scroll_up(env, cy);
                self.cy = cy - (oy - self.oy);
            }
            Recentre::Bottom => {
                self.scroll_down(env, sy - cy);
                self.cy = cy + (self.oy - oy);
            }
        }
        self.update_selection_view(env);
    }
}

/// `x` as a snapshot number.
fn n64(x: usize) -> u64 {
    x as u64
}

/// A snapshot number as a position; one that does not fit is the end.
fn us(x: u64) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

fn pos_out((x, y): Pos) -> vtmux_snapshot::Pos {
    vtmux_snapshot::Pos {
        x: n64(x),
        y: n64(y),
    }
}

fn pos_in(p: &vtmux_snapshot::Pos) -> Pos {
    (us(p.x), us(p.y))
}

/// `#{pane_key_mode}` as a snapshot number, and back.
#[must_use]
pub fn key_mode_out(k: vtmux_vt::KeyMode) -> u32 {
    match k {
        vtmux_vt::KeyMode::Standard => 0,
        vtmux_vt::KeyMode::Extended1 => 1,
        vtmux_vt::KeyMode::Extended2 => 2,
    }
}

#[must_use]
pub fn key_mode_in(n: u32) -> vtmux_vt::KeyMode {
    match n {
        1 => vtmux_vt::KeyMode::Extended1,
        2 => vtmux_vt::KeyMode::Extended2,
        _ => vtmux_vt::KeyMode::Standard,
    }
}

impl CopyMode {
    /// The mode as a snapshot carries it (D27): the frozen grid, the view,
    /// the selection, the search and the jump. A mouse drag in progress is
    /// not carried.
    #[must_use]
    #[expect(clippy::too_many_lines, reason = "one field after another")]
    pub fn export(&self, now: Instant) -> vtmux_snapshot::CopyMode {
        let (rows, hsize, hscrolled, gsx, gsy) = self.backing.export();
        let mut rows = rows;
        let screen = rows.split_off(hsize.min(rows.len()));
        let history = if rows.is_empty() {
            Vec::new()
        } else {
            vec![vtmux_snapshot::HistoryPage::from_rows(&rows, false)]
        };
        let u = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
        vtmux_snapshot::CopyMode {
            history,
            rows: screen,
            vi: self.modekeys_vi,
            sx: u(self.sx),
            sy: u(self.sy),
            oy: n64(self.oy),
            cx: u(self.cx),
            cy: u(self.cy),
            lastcx: u(self.lastcx),
            lastsx: u(self.lastsx),
            scroll_exit: self.scroll_exit,
            hide_position: self.hide_position,
            position_clip: self.position_clip.map(u),
            line_numbers: u32::from(self.line_numbers),
            prefix: self.prefix,
            selecting: self.sel.is_some(),
            selpos: pos_out(self.selpos),
            endpos: pos_out(self.endpos),
            line_select: match self.lineflag {
                LineSel::None => 0,
                LineSel::LeftRight => 1,
                LineSel::RightLeft => 2,
            },
            rect: self.rectflag,
            sel_mode: match self.selflag {
                SelectionMode::Char => 0,
                SelectionMode::Word => 1,
                SelectionMode::Line => 2,
            },
            dpos: pos_out(self.dpos),
            selr: pos_out(self.selr),
            endselr: pos_out(self.endselr),
            separators: self.separators.clone(),
            search_text: self.search.string.clone(),
            search_type: match self.search.kind {
                SearchKind::Off => 0,
                SearchKind::Up => 1,
                SearchKind::Down => 2,
            },
            search_regex: self.search.regex,
            pane_search: self.pane_search.clone(),
            jump_type: match self.jump {
                Jump::Off => 0,
                Jump::Forward => 1,
                Jump::Backward => 2,
                Jump::ToForward => 3,
                Jump::ToBackward => 4,
            },
            jump_char: self.jumpchar.clone(),
            mark: pos_out(self.mark),
            show_mark: self.show_mark,
            scrolled: self.scrolled,
            source_pane: None,
            key_mode: key_mode_out(self.key_mode),
            kill: self.kill,
            hscrolled: n64(hscrolled),
            grid_sx: u(gsx),
            grid_sy: u(gsy),
            recentre: match self.recentre {
                Recentre::Top => 0,
                Recentre::Middle => 1,
                Recentre::Bottom => 2,
            },
            recentre_line: n64(self.recentre_line),
            drag: match self.drag {
                Drag::None => 0,
                Drag::End => 1,
                Drag::Start => 2,
            },
            sel: self.sel.map(|s| vtmux_snapshot::ScreenSel {
                sx: n64(s.sx),
                sy: n64(s.sy),
                ex: n64(s.ex),
                ey: n64(s.ey),
                rectangle: s.rectangle,
                vi: s.vi,
                hidden: s.hidden,
                clipx: n64(s.clipx),
            }),
            search_down: self.search.down,
            search_origin: self
                .search
                .origin
                .map(|(x, y, o)| (n64(x), (n64(y), n64(o)))),
            search_all: self.search.all,
            search_marks: self.search.marks.clone(),
            search_generation: u32::from(self.search.generation),
            search_count: self.search.count,
            search_more: self.search.more,
            search_timeout: self.search.timeout,
            refresh_in_ms: self.refresh_due.map(|d| {
                u64::try_from(d.saturating_duration_since(now).as_millis()).unwrap_or(u64::MAX)
            }),
            foreign: self.foreign,
        }
    }

    /// A copy mode from [`Self::export`].
    ///
    /// # Errors
    /// A history page that does not decode.
    #[expect(clippy::too_many_lines, reason = "one field after another")]
    pub fn import(
        r: &vtmux_snapshot::CopyMode,
        now: Instant,
    ) -> Result<Self, vtmux_snapshot::Error> {
        let mut rows = Vec::new();
        for page in &r.history {
            page.each_row(|row| rows.push(row.clone()))?;
        }
        let hsize = rows.len();
        rows.extend(r.rows.iter().cloned());
        let backing = Grid::import(
            &rows,
            hsize,
            us(r.hscrolled),
            r.grid_sx as usize,
            r.grid_sy as usize,
        );
        Ok(Self {
            key_mode: key_mode_in(r.key_mode),
            backing,
            sx: r.sx as usize,
            sy: r.sy as usize,
            oy: us(r.oy),
            cx: r.cx as usize,
            cy: r.cy as usize,
            lastcx: r.lastcx as usize,
            lastsx: r.lastsx as usize,
            scroll_exit: r.scroll_exit,
            hide_position: r.hide_position,
            position_clip: r.position_clip.map(|x| x as usize),
            line_numbers: u8::try_from(r.line_numbers).unwrap_or(0),
            recentre: match r.recentre {
                0 => Recentre::Top,
                2 => Recentre::Bottom,
                _ => Recentre::Middle,
            },
            recentre_line: us(r.recentre_line),
            prefix: r.prefix,
            kill: r.kill,
            modekeys_vi: r.vi,
            sel: r.sel.as_ref().map(|s| ScreenSel {
                sx: us(s.sx),
                sy: us(s.sy),
                ex: us(s.ex),
                ey: us(s.ey),
                rectangle: s.rectangle,
                vi: s.vi,
                hidden: s.hidden,
                clipx: us(s.clipx),
            }),
            selpos: pos_in(&r.selpos),
            endpos: pos_in(&r.endpos),
            drag: match r.drag {
                1 => Drag::End,
                2 => Drag::Start,
                _ => Drag::None,
            },
            lineflag: match r.line_select {
                1 => LineSel::LeftRight,
                2 => LineSel::RightLeft,
                _ => LineSel::None,
            },
            rectflag: r.rect,
            selflag: match r.sel_mode {
                1 => SelectionMode::Word,
                2 => SelectionMode::Line,
                _ => SelectionMode::Char,
            },
            dpos: pos_in(&r.dpos),
            selr: pos_in(&r.selr),
            endselr: pos_in(&r.endselr),
            separators: r.separators.clone(),
            search: Search {
                kind: match r.search_type {
                    1 => SearchKind::Up,
                    2 => SearchKind::Down,
                    _ => SearchKind::Off,
                },
                regex: r.search_regex,
                string: r.search_text.clone(),
                down: r.search_down,
                origin: r.search_origin.map(|(x, (y, o))| (us(x), us(y), us(o))),
                all: r.search_all,
                marks: r.search_marks.clone(),
                generation: u8::try_from(r.search_generation).unwrap_or(u8::MAX),
                count: r.search_count,
                more: r.search_more,
                timeout: r.search_timeout,
            },
            pane_search: r.pane_search.clone(),
            jump: match r.jump_type {
                1 => Jump::Forward,
                2 => Jump::Backward,
                3 => Jump::ToForward,
                4 => Jump::ToBackward,
                _ => Jump::Off,
            },
            jumpchar: r.jump_char.clone(),
            mark: pos_in(&r.mark),
            show_mark: r.show_mark,
            foreign: r.foreign,
            refresh_due: r.refresh_in_ms.map(|ms| now + Duration::from_millis(ms)),
            drag_due: None,
            scrolled: r.scrolled,
            // The frames before the snapshot saw every change.
            offset_seen: (r.cx as usize, r.cy as usize, us(r.oy), r.sel.is_some()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten numbered lines on a 30x6 pane leave five in history and the
    /// cursor on the empty sixth row, as the reference's
    /// `printf 'line%d\n' 1..10` in `new -x 30 -y 6`.
    fn pane() -> PaneTerm {
        let mut t = PaneTerm::new(30, 6, 2000);
        for i in 1..=10 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        t
    }

    fn env() -> Env {
        Env {
            vi: false,
            line_numbers: 0,
            separators: String::new(),
            wrap_search: true,
            mouse: false,
            slider: None,
            dragging: false,
        }
    }

    fn enter(t: &PaneTerm, scroll_exit: bool) -> CopyMode {
        let size = (usize::from(t.cols()), usize::from(t.rows()));
        CopyMode::new(t, size, false, (scroll_exit, false), false, None)
    }

    fn cmd(m: &mut CopyMode, name: &str) -> Action {
        m.command(&env(), name, &[], false).unwrap().action
    }

    /// Reference (tmux 3.8, 30x6, emacs keys): entering puts the cursor at
    /// the pane's (0, 5); page-up scrolls 4 with the cursor following the
    /// end of `line7`; history-top is (0, 0) at offset 5.
    #[test]
    fn entering_and_paging() {
        let t = pane();
        let mut m = enter(&t, false);
        assert_eq!((m.scroll_position(), m.cursor()), (0, (0, 5)));
        assert_eq!(m.position(0), (0, 5));
        cmd(&mut m, "page-up");
        assert_eq!((m.scroll_position(), m.cursor()), (4, (5, 5)));
        assert_eq!(m.cursor_line(), "line7");
        cmd(&mut m, "history-top");
        assert_eq!((m.scroll_position(), m.cursor()), (5, (0, 0)));
        assert_eq!(m.cursor_line(), "line1");
        cmd(&mut m, "history-bottom");
        assert_eq!((m.scroll_position(), m.cursor()), (0, (0, 5)));
        cmd(&mut m, "cursor-up");
        assert_eq!(m.cursor(), (6, 4));
        assert_eq!(m.cursor_line(), "line10");
        cmd(&mut m, "cursor-up");
        assert_eq!(m.cursor(), (5, 3));
    }

    #[test]
    fn scroll_exit_and_cancel() {
        let t = pane();
        let mut m = enter(&t, true);
        cmd(&mut m, "page-up");
        assert_eq!(cmd(&mut m, "page-down"), Action::Cancel);
        let mut m = enter(&t, false);
        assert_eq!(cmd(&mut m, "scroll-down"), Action::Stay);
        assert_eq!(cmd(&mut m, "cursor-down-and-cancel"), Action::Cancel);
        assert_eq!(cmd(&mut m, "cancel"), Action::Cancel);
        assert_eq!(cmd(&mut m, "nosuch"), Action::Stay);
    }

    /// The view is the copy taken on entry: later output does not show.
    #[test]
    fn view_survives_new_output() {
        let mut t = pane();
        let mut m = enter(&t, false);
        cmd(&mut m, "history-top");
        t.feed(b"more\r\nand more\r\n");
        assert_eq!(m.cursor_line(), "line1");
        let s = m.screen(false, 0, &|_, _| {});
        assert_eq!(s.cell(0, 5).unwrap().text, "l");
        assert_eq!(s.cell(4, 5).unwrap().text, "6");
        cmd(&mut m, "history-bottom");
        assert_eq!(m.cursor_line(), "");
        assert_eq!(m.position(0), (0, 5));
    }

    /// `window_copy_resize`: a shorter view keeps the cursor on its line,
    /// shown at the top when that line is now in history; growing again
    /// pulls the lines back. Reference (tmux 3.8, 40x9 pane of line1..30,
    /// page-up then cursor-up, split -v): `[5] [6,0] line23`.
    #[test]
    fn resize_keeps_the_cursor_line() {
        let mut t = PaneTerm::new(40, 9, 2000);
        for i in 1..=30 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        let mut m = enter(&t, false);
        cmd(&mut m, "page-up");
        cmd(&mut m, "cursor-up");
        assert_eq!((m.scroll_position(), m.cursor()), (7, (6, 7)));
        assert_eq!(m.cursor_line(), "line23");
        t.resize(40, 4);
        m.resize(40, 4);
        assert_eq!((m.scroll_position(), m.cursor()), (5, (6, 0)));
        assert_eq!(m.cursor_line(), "line23");
        m.resize(40, 9);
        assert_eq!((m.scroll_position(), m.cursor()), (0, (6, 0)));
        assert_eq!(m.cursor_line(), "line23");
    }

    /// The pipe commands (reference: the CLI test `copy_pipe_commands`):
    /// the selection and the command, the buffer prefix at the second
    /// argument, what each does to the selection and the mode.
    #[test]
    fn pipe_commands() {
        let term = pane();
        let environ = env();
        let go = |name: &str, args: &[&str]| {
            let mut mode = enter(&term, false);
            let args: Vec<String> = args.iter().map(|&a| a.to_owned()).collect();
            for c in [
                "history-top",
                "start-of-line",
                "begin-selection",
                "cursor-right",
                "cursor-right",
            ] {
                mode.command(&environ, c, &[], false).unwrap();
            }
            let out = mode.command(&environ, name, &args, false).unwrap();
            (mode, out)
        };
        let (mode, out) = go("copy-pipe", &["cat", "pre"]);
        assert_eq!(out.action, Action::Stay);
        let pipe = out.pipe.unwrap();
        assert_eq!(
            (pipe.command.as_str(), pipe.data.as_slice()),
            ("cat", &b"li"[..])
        );
        let copy = out.copied.unwrap();
        assert_eq!(
            (copy.prefix.as_deref(), copy.data.as_slice()),
            (Some("pre"), &b"li"[..])
        );
        assert!(mode.selection().is_none_or(|s| !s.present));
        let (mode, out) = go("copy-pipe-no-clear", &["cat"]);
        assert!(out.pipe.is_some() && out.copied.is_some());
        assert!(mode.selection().is_some_and(|s| s.present));
        let (_, out) = go("copy-pipe-and-cancel", &["-P", "cat"]);
        assert_eq!(out.action, Action::Cancel);
        assert!(out.copied.is_some_and(|c| !c.paste));
        // `pipe` copies nothing, and an empty command is the option's.
        let (mode, out) = go("pipe-no-clear", &[""]);
        assert!(out.copied.is_none());
        assert_eq!(out.pipe.unwrap().command, "");
        assert!(mode.selection().is_some_and(|s| s.present));
        let (_, out) = go("pipe-and-cancel", &["cat"]);
        assert_eq!(out.action, Action::Cancel);
        // The line variants take the whole line, and the cursor is restored.
        let (mode, out) = go("copy-pipe-line", &["cat"]);
        assert_eq!(out.pipe.unwrap().data, b"line1");
        assert_eq!(mode.cursor(), (2, 0));
    }

    /// `expand_args` expands the buffer prefix and the command and keeps
    /// the flags; other commands' arguments are left alone.
    #[test]
    fn expand_args_formats_only_the_format_arguments() {
        let up = |t: &str| t.to_uppercase();
        let args = |a: &[&str]| a.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
        assert_eq!(
            expand_args("copy-pipe", &args(&["-P", "cat", "pre"]), &up),
            args(&["-P", "--", "CAT", "PRE"])
        );
        assert_eq!(
            expand_args("copy-selection", &args(&["pre"]), &up),
            args(&["--", "PRE"])
        );
        assert_eq!(expand_args("pipe", &args(&["a"]), &up), args(&["--", "A"]));
        assert_eq!(
            expand_args("search-forward", &args(&["a"]), &up),
            args(&["a"])
        );
        // Too many arguments: the command rejects them, nothing to expand.
        assert_eq!(
            expand_args("pipe", &args(&["a", "b"]), &up),
            args(&["a", "b"])
        );
    }

    /// `window_copy_do_refresh`: the copy follows new output when the
    /// cursor is at the bottom, else keeps the lines on screen.
    #[test]
    fn refresh_follows_only_at_the_bottom() {
        let mut t = pane();
        let e = env();
        let mut m = enter(&t, false);
        assert!(m.follows());
        cmd(&mut m, "history-top");
        assert!(!m.follows());
        let top = m.cursor_line();
        t.feed(b"line11\r\nline12\r\n");
        m.refresh(&e, &t, false);
        assert_eq!(m.cursor_line(), top);
        cmd(&mut m, "history-bottom");
        cmd(&mut m, "bottom-line");
        assert!(m.follows());
        t.feed(b"line13\r\n");
        m.refresh(&e, &t, true);
        assert_eq!(m.cursor_line(), "");
        assert_eq!(m.scroll_position(), 0);
    }

    /// `refresh-on`/`-off`/`-toggle` arm and stop the timer; `refresh-now`
    /// asks for a refresh; another pane's view never refreshes.
    #[test]
    fn refresh_commands() {
        let t = pane();
        let mut m = enter(&t, false);
        assert!(!m.refresh_active());
        cmd(&mut m, "refresh-on");
        assert!(m.refresh_active());
        cmd(&mut m, "refresh-toggle");
        assert!(!m.refresh_active());
        cmd(&mut m, "refresh-toggle");
        cmd(&mut m, "refresh-off");
        assert!(!m.refresh_active());
        assert!(
            m.command(&env(), "refresh-now", &[], false)
                .unwrap()
                .refresh
        );
        let mut other = CopyMode::new(&t, (30, 6), true, (false, false), false, None);
        cmd(&mut other, "refresh-on");
        assert!(!other.refresh_active());
        assert!(
            !other
                .command(&env(), "refresh-now", &[], false)
                .unwrap()
                .refresh
        );
    }

    #[test]
    fn cursor_word_follows_separators() {
        let mut t = PaneTerm::new(30, 3, 100);
        t.feed(b"foo bar-baz qux");
        let mut m = enter(&t, false);
        m.cx = 5;
        m.cy = 0;
        assert_eq!(m.cursor_word("-"), "bar");
        m.cx = 8;
        assert_eq!(m.cursor_word("-"), "baz");
        assert_eq!(m.cursor_word(""), "bar-baz");
        // Past the end of the line: a blank, so no word.
        m.cx = 30;
        assert_eq!(m.cursor_word(""), "");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn env(vi: bool) -> Env {
        Env {
            vi,
            line_numbers: 0,
            separators: "-".into(),
            wrap_search: true,
            mouse: false,
            slider: None,
            dragging: false,
        }
    }

    fn mode(text: &str, vi: bool) -> CopyMode {
        let mut t = PaneTerm::new(20, 4, 100);
        t.feed(text.as_bytes());
        CopyMode::new(&t, (20, 4), false, (false, false), vi, None)
    }

    fn run(m: &mut CopyMode, e: &Env, cmds: &[&str]) -> Option<Copied> {
        let mut copied = None;
        for c in cmds {
            let mut words = c.split(' ');
            let name = words.next().unwrap();
            let args: Vec<String> = words.map(str::to_owned).collect();
            copied = m.command(e, name, &args, false).unwrap().copied.or(copied);
        }
        copied
    }

    /// `screen_check_selection` and `window_copy_get_selection`: emacs leaves
    /// out the character under the cursor, vi keeps it.
    #[test]
    fn emacs_drops_the_cursor_character_vi_keeps_it() {
        for (vi, want) in [(false, "abc"), (true, "abcd")] {
            let e = env(vi);
            let mut m = mode("abcdef\r\n", vi);
            let c = run(&mut m, &e, &["history-top", "begin-selection"]);
            assert!(c.is_none());
            for _ in 0..3 {
                run(&mut m, &e, &["cursor-right"]);
            }
            assert_eq!(m.selected(3, 0), vi);
            assert!(m.selected(0, 0) && !m.selected(4, 0));
            let c = run(&mut m, &e, &["copy-selection-no-clear pfx"]).unwrap();
            assert_eq!(
                (c.data.as_slice(), c.prefix.as_deref()),
                (want.as_bytes(), Some("pfx"))
            );
            assert!(m.selection().is_some());
            run(&mut m, &e, &["copy-selection"]);
            assert!(m.selection().is_none());
        }
    }

    /// A rectangle takes the same columns of every line.
    /// `window_copy_write_line`: the gutter of relative numbers, the text
    /// after it, the selection in screen columns clear of the gutter and
    /// the cursor past it.
    #[test]
    fn line_numbers_draw_a_gutter() {
        let e = Env {
            line_numbers: 3,
            ..env(true)
        };
        let mut m = mode("abcdef\r\nghijkl\r\n", true);
        run(
            &mut m,
            &e,
            &["history-top", "begin-selection", "cursor-down"],
        );
        let painted = |c: &mut Cell, p: Paint| {
            if let Paint::LineNumber { current: true } | Paint::Selection = p {
                c.style.bold = true;
            }
        };
        let s = m.screen(true, 3, &painted);
        let row = |y: usize| -> String {
            s.cells[y * 20..y * 20 + 10]
                .iter()
                .map(|c| {
                    if c.text.is_empty() {
                        " ".into()
                    } else {
                        c.text.to_string()
                    }
                })
                .collect()
        };
        assert_eq!(row(0), "  1 abcdef");
        assert_eq!(row(1), "  0 ghijkl");
        assert_eq!(row(2), "  1       ");
        let bold: Vec<bool> = s.cells[20..30].iter().map(|c| c.style.bold).collect();
        // The current number, then the selection up to the cursor's column.
        assert_eq!(
            bold,
            [
                true, true, true, true, true, false, false, false, false, false
            ]
        );
        assert!(!s.cells[0].style.bold && s.cells[4].style.bold);
        assert_eq!(s.cursor, Some((4, 1)));
        assert!(!m.selection_mask(true, 3)[3]);
    }

    /// A selection set without line numbers keeps its columns and `clipx`
    /// when they come on, so it covers the gutter (`screen_write_cell`); a
    /// wide character cut at the text's edge is a default cell, without
    /// `copy-mode-current-line-style` (window-copy.c:5242).
    #[test]
    fn gutter_selection_and_cut_cells() {
        let e = Env {
            line_numbers: 2,
            ..env(true)
        };
        let mut m = mode("aaaaaaaaaaaaaaa\u{4e16}\r\nb\r\n", true);
        run(
            &mut m,
            &e,
            &[
                "line-numbers-off",
                "history-top",
                "begin-selection",
                "cursor-down",
                "line-numbers-on",
            ],
        );
        let painted = |c: &mut Cell, p: Paint| match p {
            Paint::Selection => c.style.bold = true,
            Paint::CurrentLine => c.style.underline = vtmux_vt::Underline::Single,
            _ => {}
        };
        let s = m.screen(true, 2, &painted);
        let bold = |y: usize| -> Vec<bool> {
            s.cells[y * 20..y * 20 + 6]
                .iter()
                .map(|c| c.style.bold)
                .collect()
        };
        assert_eq!(bold(0), [true; 6]);
        assert_eq!(bold(1), [true, false, false, false, false, false]);
        // The cursor's row is the first again: its wide character is cut.
        run(&mut m, &e, &["clear-selection", "cursor-up"]);
        let s = m.screen(true, 2, &painted);
        let underlined = |x: usize| s.cells[x].style.underline != vtmux_vt::Underline::None;
        assert!(underlined(4) && underlined(18));
        assert!(!underlined(19) && s.cells[19].text.is_empty());
    }

    #[test]
    fn rectangle_copies_columns() {
        let e = env(true);
        let mut m = mode("abcdef\r\nghijkl\r\n", true);
        let c = run(
            &mut m,
            &e,
            &[
                "history-top",
                "cursor-right",
                "begin-selection",
                "rectangle-toggle",
                "cursor-down",
                "cursor-right",
                "copy-selection-and-cancel",
            ],
        )
        .unwrap();
        assert_eq!(c.data, b"bc\nhi");
    }

    /// A wide whitespace character covers both its cells (`grid_in_set`).
    #[test]
    fn wide_whitespace_is_one_character() {
        let mut t = PaneTerm::new(20, 2, 100);
        t.feed("aa\u{3000}bb".as_bytes());
        let (g, _, _) = Grid::clone_of(&t, 20, 2, false);
        assert_eq!(g.in_set(2, 0, WHITESPACE), 2);
        assert_eq!(g.in_set(3, 0, WHITESPACE), 1);
        assert_eq!(g.in_set(3, 0, "-"), 0);
        assert_eq!(g.in_set(0, 0, "a"), 1);
    }

    /// window-copy.c:3921-3933 with the table's READONLY flags
    /// (3293-3890): 53 of the 99 commands are read-only; a read-only client
    /// is refused the other 46 before anything runs, the repeat count
    /// is kept, and the mode is not touched.
    #[test]
    fn read_only_client_runs_only_read_only_commands() {
        assert_eq!(READ_ONLY_COMMANDS.len(), 53);
        assert_eq!(TMUX_COMMANDS.len(), 99);
        assert!(READ_ONLY_COMMANDS.iter().all(|c| TMUX_COMMANDS.contains(c)));
        let mut m = mode("a b\r\nc d\r\n", false);
        let e = env(false);
        m.prefix = 4;
        for name in TMUX_COMMANDS {
            let r = m.command(&e, name, &[], true);
            if READ_ONLY_COMMANDS.contains(name) {
                assert_ne!(r, Err(CommandError::ReadOnly), "{name}");
                m.prefix = 4;
            } else {
                assert_eq!(r, Err(CommandError::ReadOnly), "{name}");
                assert_eq!(m.prefix, 4, "{name} used the count up");
            }
        }
        // Not a read-only client: begin-selection runs.
        assert!(m.command(&e, "begin-selection", &[], false).is_ok());
        // A name tmux does not have is not refused, as it is not found.
        assert!(m.command(&e, "no-such", &[], true).is_ok());
    }
}
