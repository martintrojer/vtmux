//! Server, session, window, pane and client records.
//!
//! These are the format's own types, not the server's structs (D27 decision
//! 18): the server converts its model to and from them (task
//! `lu_model_snapshot`). Ids are the numbers after `$`, `@`, `%` and the
//! client id. Values tmux prints are kept in the form tmux prints them
//! (option choices by name, keys and command lists as text), so a change to
//! an internal numbering cannot change what a snapshot means.
//!
//! Modes carried across an upgrade: copy mode (its frozen grid, D-010, and
//! position) and clock mode. View mode is closed for now: its backing
//! terminal is pane terminal state, and [`PaneMode::View`] is kept for when
//! that is carried. Tree, buffer, client, customize, switch and
//! display-panes modes, prompts and menus are closed by the upgrade and
//! re-opened by the user (spec, *Open questions*); a mode a reader doesn't
//! know reads as [`PaneMode::Closed`], so carrying a new mode later doesn't
//! make older readers refuse the stream. Popups are carried with their
//! pty; one editing a buffer or an option (its editor's temp file) is
//! closed.

use crate::row::{HistoryPage, Row};
use crate::term::TermState;
use crate::wire::{oneof, record, value_enum};

oneof! {
    /// An option value, in tmux's own terms.
    pub enum Value {
        1 => String(String),
        2 => Number(i64),
        /// A key, as `key_string_lookup_key` prints it; a key whose name
        /// reads back as another key is `0x` and its tmux key code.
        3 => Key(String),
        /// A colour, as `colour_tostring` prints it.
        4 => Colour(String),
        5 => Flag(bool),
        /// A choice, by name.
        6 => Choice(String),
        /// A command list, as `cmd_list_print` prints it.
        7 => Command(String),
    }
}

oneof! {
    /// An array option's key.
    pub enum ArrayKey {
        1 => Index(u32),
        2 => Name(String),
    }
}

record! {
    pub struct ArrayItem {
        1 => one key: ArrayKey,
        2 => one value: Value,
    }
}

oneof! {
    pub enum OptionValue {
        1 => Single(Value),
        2 => Array(Vec<ArrayItem>),
    }
}

/// An array as one field: its items as repeated fields.
impl crate::wire::Value for Vec<ArrayItem> {
    fn put(&self, out: &mut Vec<u8>) {
        crate::wire::many::put(out, 1, self);
    }
    fn get(buf: &[u8], cx: &crate::wire::Ctx) -> Result<Self, crate::Error> {
        let mut v = Vec::new();
        let mut r = crate::wire::Reader::new(buf);
        while let Some((tag, body)) = r.field()? {
            if tag == 1 {
                crate::wire::many::get(&mut v, body, cx)?;
            }
        }
        Ok(v)
    }
}

record! {
    /// One option set in a tree (inherited options are not listed).
    pub struct OptionEntry {
        1 => one name: String,
        2 => one value: OptionValue,
    }
}

oneof! {
    /// Which options tree.
    pub enum Scope {
        1 => Server(()),
        2 => GlobalSession(()),
        3 => GlobalWindow(()),
        4 => Session(u32),
        5 => Window(u32),
        6 => Pane(u32),
    }
}

record! {
    /// A hook's `fire_count` and `fire_time` (`options_hook_fired`).
    pub struct HookFire {
        1 => one scope: Scope,
        2 => one name: String,
        3 => one count: u32,
        4 => one time: i64,
    }
}

record! {
    /// A monitor hook (`set-hook -B`).
    pub struct Monitor {
        1 => one scope: Scope,
        /// The option entry holding it.
        2 => one name: String,
        /// What it watches, as the server names it.
        3 => one what: String,
        4 => one format: String,
        5 => one notify_true: bool,
        6 => opt session: Option<u32>,
        7 => one serial: u64,
        8 => one fire_count: u32,
        9 => one fire_time: i64,
        /// The value it last saw.
        10 => opt last: Option<String>,
        /// Pane or window monitors: the last value per (id, index).
        11 => many each: Vec<SubValue>,
        /// When it looks next.
        12 => opt due_in_ms: Option<u64>,
    }
}

record! {
    /// An environment entry. `value` `None` is a `-r` entry (removed from
    /// new processes' environment).
    pub struct EnvVar {
        1 => one name: String,
        2 => opt value: Option<String>,
        3 => one hidden: bool,
    }
}

record! {
    pub struct Binding {
        /// The key, as `key_string_lookup_key` prints it (or `0x` and its
        /// tmux key code, as [`Value::Key`]).
        1 => one key: String,
        2 => opt note: Option<String>,
        3 => one repeat: bool,
        /// The commands, as `cmd_list_print` prints them.
        4 => one command: String,
    }
}

record! {
    pub struct KeyTable {
        1 => one name: String,
        2 => many bindings: Vec<Binding>,
        /// The bindings the table started with (`key_bindings_reset`).
        3 => many defaults: Vec<Binding>,
    }
}

record! {
    pub struct PasteBuffer {
        1 => one name: String,
        2 => one data: Vec<u8>,
        3 => one automatic: bool,
        /// `paste_next_order` when it was added.
        4 => one order: u64,
        5 => one created: i64,
    }
}

record! {
    /// A message log entry (`show-messages`).
    pub struct Message {
        1 => one number: u32,
        /// Microseconds since the epoch.
        2 => one time_us: i64,
        3 => one text: String,
    }
}

record! {
    /// A `wait-for` channel.
    pub struct WaitChannel {
        1 => one name: String,
        2 => one locked: bool,
        3 => one woken: bool,
        /// Client ids blocked in `wait-for`, in order.
        4 => many waiters: Vec<u32>,
        /// Client ids blocked in `wait-for -L`, in order.
        5 => many lockers: Vec<u32>,
    }
}

record! {
    /// The marked pane (`select-pane -m`).
    pub struct Marked {
        1 => one session: u32,
        2 => one window_index: u32,
        3 => one window: u32,
        4 => one pane: u32,
    }
}

record! {
    pub struct Server {
        1 => one pid: i32,
        2 => one socket_path: String,
        /// Microseconds since the epoch (`#{start_time}`).
        3 => one start_time_us: i64,
        4 => many server_options: Vec<OptionEntry>,
        5 => many session_options: Vec<OptionEntry>,
        6 => many window_options: Vec<OptionEntry>,
        7 => many environment: Vec<EnvVar>,
        /// User options that are hooks (`set-hook`), hidden by
        /// `show-options` without `-H`.
        8 => many hook_events: Vec<String>,
        9 => many hook_fires: Vec<HookFire>,
        10 => many monitors: Vec<Monitor>,
        11 => one monitor_serial: u64,
        12 => many key_tables: Vec<KeyTable>,
        13 => many buffers: Vec<PasteBuffer>,
        14 => one buffer_next_index: u32,
        15 => one buffer_next_order: u64,
        16 => many messages: Vec<Message>,
        17 => one message_next: u32,
        /// The next `$`, `@`, `%` and client ids.
        18 => one next_session: u32,
        19 => one next_window: u32,
        20 => one next_pane: u32,
        21 => one next_client: u32,
        22 => many command_history: Vec<String>,
        23 => many search_history: Vec<String>,
        24 => many wait_channels: Vec<WaitChannel>,
        25 => opt marked: Option<Marked>,
        /// `TMUX_TERM`.
        26 => one default_term: String,
        /// Server-wide activity counter (the ordering stamp sessions and
        /// windows take).
        27 => one activity: u64,
        /// The config files the server started with, in order
        /// (`#{config_files}`, format.c `format_cb_config_files`).
        28 => many config_files: Vec<String>,
        /// Config errors not yet shown, `file:line: message` (cfg.c
        /// `cfg_add_cause`): the next attach shows them
        /// (`cfg_show_causes`).
        29 => many config_causes: Vec<String>,
        /// The last command number handed out (cmd-queue.c `cmdq_next`):
        /// a control client's `%begin` numbers go on from it.
        30 => one cmd_number: u32,
        /// `next_active_point` (window.c:63).
        31 => one active_points: u64,
        /// The last winlink attachment stamp ([`Winlink::seq`]).
        32 => one link_stamp: u64,
        /// The session the marked winlink really is in when a link into
        /// another session moved it there (server-fn.c:335-336).
        33 => opt marked_elsewhere: Option<u32>,
        /// `server-access` entries (server-acl.c).
        34 => many access: Vec<Access>,
        /// The path the server was started as, absolute, symlinks kept
        /// (live-upgrade spec, decision 22): `vtmux-upgrade` without `-b`
        /// resolves it when asked. Added within v1 and empty in the golden
        /// sample: a v1 reader without it falls back to its own binary.
        35 => one start_binary: String,
        /// What a `vtmux-persist` checkpoint the server found at its start
        /// leaves it free to write ([`PERSIST_HOLD_WHILE_THERE`],
        /// [`PERSIST_HOLD_ALWAYS`]; 0: free), so the image a live upgrade
        /// starts keeps the old one's promise not to write over a
        /// checkpoint it refused. Added within v1, 0 in the golden sample:
        /// a v1 reader without it holds nothing back, as builds before it
        /// did. Always 0 in a checkpoint.
        36 => one persist_hold: u32,
        /// The next OSC 8 external id's number (`hyperlinks_next_external_id`,
        /// the server's one counter): links drawn after a live upgrade, in
        /// a status line or popup, never take a number one still on screen
        /// has. Added within v1, 0 in the golden sample: a reader without
        /// it numbers on from the highest number a pane kept, so a link
        /// that is not a pane's may be given a number that was in use.
        37 => one link_number: u64,
    }
}

/// [`Server::persist_hold`]: the checkpoint a restore refused is not
/// written over while its manifests are there.
pub const PERSIST_HOLD_WHILE_THERE: u32 = 1;
/// [`Server::persist_hold`]: a newer binary's checkpoint; nothing is
/// written to the directory for the server's lifetime. So is any value
/// this build does not know.
pub const PERSIST_HOLD_ALWAYS: u32 = 2;

record! {
    /// A `server-access` entry.
    pub struct Access {
        /// A group id, else a user id.
        1 => one group: bool,
        2 => one id: u32,
        3 => one read_only: bool,
    }
}

record! {
    /// A node of a session's winlinks red-black tree, in preorder: the
    /// tree's shape decides the order `session_destroy` unlinks windows
    /// in (session.c:221-224), which hooks and control mode show.
    pub struct RbNode {
        1 => one key: u32,
        2 => one red: bool,
    }
}

record! {
    /// A window in a session at an index.
    pub struct Winlink {
        1 => one index: u32,
        2 => one window: u32,
        /// Bell, activity and silence flags of this link.
        3 => one alerts: u32,
        /// The order links were made in.
        4 => one seq: u64,
    }
}

record! {
    pub struct Session {
        1 => one id: u32,
        2 => one name: String,
        3 => one created: i64,
        4 => one activity: u64,
        5 => one activity_secs: i64,
        6 => one cwd: String,
        7 => many windows: Vec<Winlink>,
        /// The current window's index.
        8 => one current: u32,
        /// Last-window stack, most recent first.
        9 => many last: Vec<u32>,
        10 => many environment: Vec<EnvVar>,
        11 => opt last_attached: Option<i64>,
        12 => many options: Vec<OptionEntry>,
        13 => opt group: Option<String>,
        14 => many link_tree: Vec<RbNode>,
        /// When `lock-after-time` locks it (`s->lock_timer`), if armed.
        15 => opt lock_in_ms: Option<u64>,
        /// `cwd`'s bytes when it is not UTF-8 (`cwd` has it lossily, as a
        /// reader before this field takes it). Ignorable.
        16 => opt cwd_bytes: Option<Vec<u8>>,
    }
}

value_enum! {
    pub enum Dir {
        0 => LeftRight,
        1 => TopBottom,
    }
}

record! {
    pub struct Geometry {
        1 => one sx: u32,
        2 => one sy: u32,
        3 => one xoff: i32,
        4 => one yoff: i32,
    }
}

record! {
    pub struct Leaf {
        1 => one pane: u32,
        2 => one g: Geometry,
    }
}

record! {
    pub struct Node {
        1 => one dir: Dir,
        2 => one g: Geometry,
        3 => many children: Vec<LayoutCell>,
    }
}

oneof! {
    /// A layout cell; a floating leaf may appear in any node.
    pub enum LayoutCell {
        1 => Pane(Leaf),
        2 => Floating(Leaf),
        3 => Node(Node),
    }
}

record! {
    /// A window's layout tree and the facts its arithmetic keeps.
    pub struct Layout {
        1 => one sx: u32,
        2 => one sy: u32,
        3 => one root: LayoutCell,
        /// `pane-border-status`: 0 off, 1 top, 2 bottom.
        4 => one pane_status: u32,
        5 => opt scrollbar_always: Option<u32>,
        6 => one last_new_x: i32,
        7 => one last_new_y: i32,
        /// Floating geometry of cells tiled again (`break-pane -W`).
        8 => many saved_float: Vec<Leaf>,
        /// Floating panes drawn without a border.
        9 => many borderless: Vec<u32>,
    }
}

record! {
    pub struct Zoom {
        1 => one pane: u32,
        /// The layout before the zoom.
        2 => one saved: Layout,
    }
}

record! {
    pub struct Window {
        1 => one id: u32,
        2 => one name: String,
        3 => one activity: u64,
        4 => one activity_secs: i64,
        5 => one layout: Layout,
        /// Pane ids in window order.
        6 => many panes: Vec<u32>,
        7 => opt active: Option<u32>,
        /// Last-pane stack, most recent first.
        8 => many last: Vec<u32>,
        /// Topmost first.
        9 => many z_order: Vec<u32>,
        10 => one base_index: u32,
        11 => opt modal: Option<u32>,
        12 => opt modal_last: Option<u32>,
        13 => opt zoom: Option<Zoom>,
        14 => opt was_zoomed: Option<u32>,
        /// The named layout last applied (`next-layout` cycles from it).
        15 => opt last_layout: Option<String>,
        /// The layout string before the last change (`select-layout -o`).
        16 => opt old_layout: Option<String>,
        17 => opt pending_size: Option<(u32, u32)>,
        18 => one manual_size: (u32, u32),
        /// The client id that last used the window (`window-size latest`).
        19 => opt latest_client: Option<u32>,
        /// Bell, activity and silence flags.
        20 => one alerts: u32,
        21 => opt silence_in_ms: Option<u64>,
        22 => many options: Vec<OptionEntry>,
        /// A cell's width and height in pixels (`w->xpixel`, `w->ypixel`),
        /// which panes' PTYs and XTWINOPS replies use. Added within v1, 0
        /// in the golden sample: a reader without it (and a stream
        /// without it) gives tmux's default 16x32 until the next resize.
        23 => one cell_pixels: (u32, u32),
        /// The cell size that comes with `pending_size` (`w->new_xpixel`,
        /// `new_ypixel`), 0 for the default. Added within v1, as above.
        24 => one pending_pixels: (u32, u32),
    }
}

record! {
    /// A process's end (`#{pane_dead_status}`, `#{pane_dead_signal}`).
    pub struct ExitStatus {
        1 => opt code: Option<i32>,
        2 => opt signal: Option<i32>,
        3 => one core_dumped: bool,
    }
}

record! {
    pub struct PipePane {
        1 => one pid: i32,
        /// Unused: tmux keeps no command text (`#{pane_pipe}` is 0 or 1,
        /// format.c:2589), nor does vtmux.
        2 => one command: String,
        /// `-I`: the command's output goes to the pane.
        3 => one input: bool,
        /// `-O`: the command's stdin is the pane's output (else
        /// `/dev/null`).
        4 => one output: bool,
        /// Pane output the command has not taken yet
        /// (`wp->pipe_event`'s output buffer).
        5 => one pending: Vec<u8>,
    }
}

record! {
    /// A copy-mode position: `x`, and `y` counted from the top of history.
    pub struct Pos {
        1 => one x: u64,
        2 => one y: u64,
    }
}

record! {
    /// Copy mode (window-copy.c): the frozen grid it reads (D-010) and
    /// where it is.
    pub struct CopyMode {
        /// History of the frozen grid, oldest first.
        1 => many history: Vec<HistoryPage>,
        /// Its visible rows.
        2 / 55 => many rows: Vec<Row>,
        /// `mode-keys`: false emacs, true vi.
        3 => one vi: bool,
        4 => one sx: u32,
        5 => one sy: u32,
        6 => one oy: u64,
        7 => one cx: u32,
        8 => one cy: u32,
        9 => one lastcx: u32,
        10 => one lastsx: u32,
        11 => one scroll_exit: bool,
        12 => one hide_position: bool,
        13 => opt position_clip: Option<u32>,
        14 => one line_numbers: u32,
        15 => one prefix: u32,
        16 => one selecting: bool,
        17 => one selpos: Pos,
        18 => one endpos: Pos,
        /// 0 none, 1 line, 2 word (`lineflag`, `selflag`).
        19 => one line_select: u32,
        20 => one rect: bool,
        21 => one sel_mode: u32,
        22 => one dpos: Pos,
        23 => one selr: Pos,
        24 => one endselr: Pos,
        25 => one separators: String,
        26 => opt search_text: Option<String>,
        27 => one search_type: u32,
        28 => one search_regex: bool,
        29 => opt pane_search: Option<(String, bool)>,
        30 => one jump_type: u32,
        31 => one jump_char: String,
        32 => one mark: Pos,
        33 => one show_mark: bool,
        34 => one scrolled: bool,
        /// `copy-mode -s`: the frozen grid came from another pane.
        35 => opt source_pane: Option<u32>,
        /// `#{pane_key_mode}` of the mode's screen: 0 `VT10x`, 1 `Ext 1`, 2 `Ext 2`.
        36 => one key_mode: u32,
        /// `copy-mode -k`: the pane is killed when the mode exits.
        37 => one kill: bool,
        /// History lines that scrolled there and were not cleared.
        38 => one hscrolled: u64,
        /// The frozen grid's width and screen height.
        39 => one grid_sx: u32,
        40 => one grid_sy: u32,
        /// `RECENTRE_*`: 0 top, 1 middle, 2 bottom; and its line.
        41 => one recentre: u32,
        42 => one recentre_line: u64,
        /// `cursordrag`: 0 none, 1 end, 2 start.
        43 => one drag: u32,
        /// The selection as drawn (`struct screen_sel`).
        44 => opt sel: Option<ScreenSel>,
        45 => one search_down: bool,
        /// Where an incremental search started: x, y and offset.
        46 => opt search_origin: Option<(u64, (u64, u64))>,
        47 => one search_all: bool,
        /// One mark per cell in view (`searchmark`).
        48 => opt search_marks: Option<Vec<u8>>,
        49 => one search_generation: u32,
        /// `searchcount`, -1 unknown.
        50 => one search_count: i64,
        51 => one search_more: bool,
        52 => one search_timeout: bool,
        /// When the automatic refresh runs next, while it is on.
        53 => opt refresh_in_ms: Option<u64>,
        /// The view is of another pane (`copy-mode -s`).
        54 => one foreign: bool,
    }
}

record! {
    /// A copy mode selection as drawn (`struct screen_sel`), in screen
    /// coordinates.
    pub struct ScreenSel {
        1 => one sx: u64,
        2 => one sy: u64,
        3 => one ex: u64,
        4 => one ey: u64,
        5 => one rectangle: bool,
        6 => one vi: bool,
        7 => one hidden: bool,
        /// Columns left of this are never selected (`clipx`): the line
        /// numbers' gutter when the selection was set.
        8 => one clipx: u64,
    }
}

oneof! {
    /// A mode on a pane's mode stack that survives an upgrade.
    pub enum PaneMode {
        1 => Copy(CopyMode),
        /// `window_view_mode`: copy mode over command output.
        2 => View(CopyMode),
        /// Clock mode, with its screen's key mode (as `CopyMode::key_mode`).
        3 => Clock(u32),
        /// A mode this reader doesn't know (a newer writer's), or one the
        /// writer closed: the restore drops it from the stack.
        4 => Closed(()),
    }
    unknown => Closed
}

record! {
    pub struct Pane {
        1 => one id: u32,
        2 => one window: u32,
        3 => one pid: i32,
        4 => one tty: String,
        /// The start command; empty for the default shell.
        5 => many argv: Vec<String>,
        6 => one shell: String,
        7 => one cwd: String,
        /// Unused: tmux keeps no environment per pane (`struct
        /// window_pane` has none; the spawn environment is built and
        /// freed in spawn.c), nor does vtmux.
        8 => many environment: Vec<EnvVar>,
        /// Made without a process (`split-window ''`).
        9 => one empty: bool,
        10 => one exited: bool,
        11 => opt status: Option<ExitStatus>,
        12 => opt dead_time: Option<i64>,
        13 => one changed: bool,
        14 => one unseen_changes: bool,
        15 => one geometry: Geometry,
        16 => one history_limit: u64,
        17 => one input_off: bool,
        18 => one focused: bool,
        19 => one active_point: u64,
        20 => many options: Vec<OptionEntry>,
        /// The mode stack, innermost last.
        21 => many modes: Vec<PaneMode>,
        /// The last copy-mode search (`#{pane_search_string}`).
        22 => opt search: Option<(String, bool)>,
        23 => opt pipe: Option<PipePane>,
        /// `None` when persistence is `layout`.
        24 => opt term: Option<TermState>,
        25 => one over_zoom: bool,
        26 => one capture_keys: bool,
        27 => one close_on_click: bool,
        28 => one close_on_cancel: bool,
        /// The pane title, also kept when `term` is not.
        29 => one title: String,
        30 => opt output_ago_ms: Option<u64>,
        31 => opt resize_in_ms: Option<u64>,
        /// Size changes not yet told to the program (`wp->resize_queue`),
        /// each `(from, to)`.
        32 => many resizes: Vec<((u32, u32), (u32, u32))>,
        /// How long ago `automatic-rename` last renamed its window from it.
        33 => opt rename_ago_ms: Option<u64>,
        /// The auto-hide scrollbar shown (`sb_auto_visible`), under the
        /// pointer (`sb_auto_hover`), and when its hide timer runs out
        /// (`sb_auto_timer`). Written under [`crate::SCROLLBAR_FEATURE`].
        34 => one scrollbar_visible: bool,
        35 => one scrollbar_hover: bool,
        36 => opt scrollbar_hide_in_ms: Option<u64>,
        /// `#{pane_output_generation}`: how many times output was parsed
        /// into it. Ignorable, as are the OSC 133 fields after it.
        37 => one output_generation: u64,
        /// What OSC 133 said of the shell (input.c `input_osc_133`), epoch
        /// seconds: the last prompt, the last command's start and end,
        /// whether it runs, and its exit status.
        38 => opt last_prompt_time: Option<i64>,
        39 => opt command_start_time: Option<i64>,
        40 => opt command_end_time: Option<i64>,
        41 => one command_running: bool,
        42 => opt command_status: Option<u8>,
        /// The process groups whose kitty keyboard grants (D29) the
        /// primary and the alternate screen hold, whose end ends them.
        /// Ignorable: a snapshot without them (a build before) has its
        /// grants settled on import.
        43 => opt kitty_owner_primary: Option<i32>,
        44 => opt kitty_owner_alternate: Option<i32>,
        /// `#{pane_last_output_time}`: epoch seconds of the program's last
        /// output. Ignorable: an older reader leaves it empty.
        45 => opt last_output_time: Option<i64>,
        /// `cwd`'s bytes when it is not UTF-8, as for [`Session`].
        46 => opt cwd_bytes: Option<Vec<u8>>,
        /// Input for the program its PTY has not taken yet (keys, a paste
        /// it has not read). Ignorable: a reader before drops it, as
        /// every build did.
        47 => one input_queued: Vec<u8>,
    }
}

record! {
    /// A `refresh-client -B` subscription.
    pub struct Subscription {
        1 => one name: String,
        /// What it watches: `""` the session, `%N` / `@N` one pane or
        /// window, `%*` / `@*` every pane or window.
        2 => one target: String,
        3 => one format: String,
        /// The session subscription's last value.
        4 => opt last: Option<String>,
        /// Pane or window subscriptions: the last value per (id, index).
        5 => many each: Vec<SubValue>,
    }
}

record! {
    pub struct SubValue {
        1 => one id: u32,
        2 => one index: u32,
        3 => one value: String,
    }
}

value_enum! {
    /// `refresh-client -A`.
    pub enum PaneOutput {
        0 => On,
        1 => Off,
        2 => Paused,
    }
}

record! {
    pub struct ControlPane {
        1 => one pane: u32,
        2 => one output: PaneOutput,
        /// Bytes of the pane's output already written to this client.
        3 => one offset: u64,
    }
}

record! {
    /// A line or a range of pane output a control client is still owed.
    pub struct ControlQueued {
        /// The pane for `%output`; `None` for a ready line.
        1 => opt pane: Option<u32>,
        2 => one data: Vec<u8>,
        /// Since the output was read (`pause-after` ages it).
        3 => one age_ms: u64,
    }
}

record! {
    pub struct ControlClient {
        /// 1 for `-C`, 2 for `-CC`.
        1 => one level: u32,
        2 => many subscriptions: Vec<Subscription>,
        /// `pause-after`, seconds.
        3 => opt pause_after: Option<u32>,
        4 => many panes: Vec<ControlPane>,
        /// `refresh-client -C @N:XxY`.
        5 => many window_sizes: Vec<WindowSize>,
        6 => opt size: Option<(u32, u32)>,
        7 => one retval: i32,
        8 => many queued: Vec<ControlQueued>,
        9 => one ready: bool,
        10 => one eof: bool,
        /// Unconsumed stdin.
        11 => one inbuf: Vec<u8>,
        12 => one guard_depth: u32,
        13 => opt subs_due_ms: Option<u64>,
    }
}

record! {
    pub struct WindowSize {
        1 => one window: u32,
        2 => one sx: u32,
        3 => one sy: u32,
    }
}

record! {
    pub struct Popup {
        1 => one pid: i32,
        2 => many argv: Vec<String>,
        3 => one cwd: String,
        4 => one x: i32,
        5 => one y: i32,
        6 => one sx: u32,
        7 => one sy: u32,
        8 => one title: String,
        /// `-s` and `-S`.
        9 => opt style: Option<String>,
        10 => opt border_style: Option<String>,
        11 => one border_lines: String,
        /// `-E` / `-EE`: 0 never, 1 on exit, 2 on success.
        12 => one close_on_exit: u32,
        13 => opt term: Option<TermState>,
        /// The pane it was opened from.
        14 => opt pane: Option<u32>,
        /// The job closed its output and was reaped.
        15 => one exited: bool,
        16 => opt status: Option<ExitStatus>,
        /// The position and size it was opened with: x, y, then sx, sy.
        17 => one preferred: ((u32, u32), (u32, u32)),
        /// `-k`: any key closes it once the job has exited.
        18 => one any_key: bool,
        /// `pd->status`: `128 + SIGHUP` until the job exits.
        19 => one status_code: i32,
        /// The client whose command opened it.
        20 => opt caller: Option<u32>,
        /// Input its PTY has not taken yet, as for [`Pane`].
        21 => one input_queued: Vec<u8>,
    }
}

record! {
    pub struct Client {
        1 => one id: u32,
        2 => one name: String,
        3 => one tty: String,
        4 => one pid: u32,
        /// The protocol version of the client's binary.
        5 => one version: u32,
        6 => many argv: Vec<Vec<u8>>,
        7 => one cwd: String,
        8 => many environment: Vec<(String, String)>,
        /// `TERM` and the terminal's features.
        9 => one term_name: String,
        10 => many term_features: Vec<String>,
        11 => one utf8: bool,
        12 => opt session: Option<u32>,
        13 => opt last_session: Option<u32>,
        14 => one was_attached: bool,
        15 => one created: i64,
        16 => one activity: u64,
        17 => one activity_secs: i64,
        18 => one size: (u32, u32),
        /// A cell's width and height in pixels (`tty->xpixel`,
        /// `tty->ypixel`), 0 while unknown.
        19 => one pixel_size: (u32, u32),
        /// Flags by the names `#{client_flags}` and `refresh-client -f`
        /// use (`read-only`, `ignore-size`, `active-pane`, ...).
        20 => many flags: Vec<String>,
        21 => one key_table: String,
        22 => one repeating: bool,
        23 => opt last_key: Option<String>,
        24 => opt repeat_in_ms: Option<u64>,
        25 => one table_ago_ms: u64,
        26 => one focused: bool,
        27 => one suspended: bool,
        28 => opt title: Option<String>,
        29 => opt control: Option<ControlClient>,
        30 => opt popup: Option<Popup>,
        /// The one-time token a re-attaching client presents (decision 21).
        31 => one token: u64,
        /// The terminal's OSC 10 and 11 replies.
        32 => opt fg: Option<Rgb>,
        33 => opt bg: Option<Rgb>,
        /// The last `CSI ? 997` report: `light` or `dark`.
        34 => opt theme: Option<String>,
        /// Its OSC 4 replies for palette entries 0-15.
        35 => many palette: Vec<PaletteEntry>,
        /// Frame bytes sent (`c->written`, `#{client_written}`).
        36 => one written: u64,
        /// The terminal's replies to the attach queries, in order: they
        /// shape its features (`tty_keys_device_attributes` and friends).
        37 => many term_replies: Vec<TermReply>,
        /// Invoked as `tmux` (else `vtmux`).
        38 => one tmux_invocation: bool,
        /// The session a client that lost its own was last in (the
        /// `client-detached` payload).
        39 => opt left_session: Option<u32>,
        /// The client has a terminal (`tty` may still be empty: a
        /// terminal without a name).
        40 => one has_tty: bool,
        /// `terminal-features` and `terminal-overrides` as they were when
        /// the terminal opened (`tty_term_create` reads them only then,
        /// tty-term.c:457, 619).
        41 => many open_features: Vec<String>,
        42 => many open_overrides: Vec<String>,
        /// The status-line message on its screen and its timer.
        43 => opt status_message: Option<StatusMessage>,
        /// Where its terminal's queries stand. A reader may drop it (not
        /// required, [`crate::TERM_QUERIES_FEATURE`]).
        44 => opt queries: Option<TermQueries>,
        /// The title its terminal shows from its output (`set-titles` or
        /// a pane's passthrough). A reader may drop it (not required,
        /// [`crate::TERMINAL_TITLE_FEATURE`]).
        45 => opt terminal_title: Option<String>,
        /// The view panned by `refresh-client` (`c->pan_window`), while
        /// it holds. Like [`Self::terminal_title`], only how the window
        /// is shown: not required.
        46 => opt pan: Option<ClientPan>,
        /// The part of its window the client shows (`tty->oox`, `ooy`,
        /// `osx`, `osy`, `oflag`), as last worked out: tmux works it out
        /// only on some events, so it may differ from what the window
        /// would give now. Not required: without it the view is worked
        /// out afresh.
        47 => opt view: Option<ClientView>,
        /// `cwd`'s bytes when it is not UTF-8, as for [`Session`].
        48 => opt cwd_bytes: Option<Vec<u8>>,
    }
}

record! {
    /// A client's view: `sx` x `sy` cells from column `ox`, row `oy` of
    /// its window, and whether the window is bigger than the client.
    pub struct ClientView {
        1 => one ox: u32,
        2 => one oy: u32,
        3 => one sx: u32,
        4 => one sy: u32,
        5 => one bigger: bool,
    }
}

record! {
    /// A client's panned view: from column `ox`, row `oy` of window
    /// `window`.
    pub struct ClientPan {
        1 => one window: u32,
        2 => one ox: u32,
        3 => one oy: u32,
    }
}

record! {
    /// A client terminal's startup queries (DA, DA2, XTVERSION, `DECRQM
    /// 2026`) and colour queries (OSC 10/11): which are answered (the
    /// `TTY_HAVE*` flags, `TTY_WAITFG|TTY_WAITBG` clear) and, while some
    /// are out, when the start timer gives up on them
    /// (`tty_start_timer_callback`).
    pub struct TermQueries {
        1 => opt start_in_ms: Option<u64>,
        2 => one da: bool,
        3 => one da2: bool,
        4 => one xtversion: bool,
        5 => one sync: bool,
        6 => opt colours_in_ms: Option<u64>,
        7 => one fg: bool,
        8 => one bg: bool,
        /// `TTY_WINSIZEQUERY`: the size queries are out. Added within v1
        /// (`a_clients_terminal_queries_round_trip`).
        9 => one winsize: bool,
    }
}

record! {
    /// A client's status-line message (`c->message_string`) and timer
    /// (`c->message_timer`, which outlives a cleared message).
    pub struct StatusMessage {
        1 => opt text: Option<String>,
        /// `#[...]` in the text is literal.
        2 => one ignore_styles: bool,
        /// `c->message_ignore_keys`.
        3 => one ignore_keys: bool,
        /// `TTY_FREEZE`: the pane area is held under it (not `-C`).
        4 => one frozen: bool,
        5 => opt timer_in_ms: Option<u64>,
    }
}

record! {
    pub struct Rgb {
        1 => one r: u8,
        2 => one g: u8,
        3 => one b: u8,
    }
}

record! {
    pub struct PaletteEntry {
        1 => one index: u32,
        2 => one rgb: Rgb,
    }
}

record! {
    /// One reply of a client terminal to the attach queries.
    pub struct TermReply {
        /// 0 synchronized output supported, 1 DA, 2 secondary DA, 3
        /// XTVERSION.
        1 => one kind: u32,
        /// DA's parameters; secondary DA's first one.
        2 => many params: Vec<u32>,
        /// XTVERSION's text.
        3 => one text: String,
    }
}

oneof! {
    /// What a descriptor kept across `execve` is.
    pub enum Role {
        1 => Listener(()),
        2 => Client(u32),
        3 => PanePty(u32),
        /// A client's popup pty, by client id.
        4 => PopupPty(u32),
        5 => PipePane(u32),
        6 => Snapshot(()),
    }
}

record! {
    pub struct Descriptor {
        1 => one fd: i32,
        2 => one role: Role,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::row::tests::sample_row;
    use crate::term::tests::sample_term;
    use crate::wire::{Ctx, Value as _};

    pub(crate) fn round_trip<T: crate::wire::Value + PartialEq + std::fmt::Debug>(v: &T) {
        let mut out = Vec::new();
        v.put(&mut out);
        assert_eq!(&T::get(&out, &Ctx::new(1)).unwrap(), v);
    }

    fn opt(name: &str, value: OptionValue) -> OptionEntry {
        OptionEntry {
            name: name.into(),
            value,
        }
    }

    pub(crate) fn sample_options() -> Vec<OptionEntry> {
        vec![
            opt(
                "status-right",
                OptionValue::Single(Value::String("x".into())),
            ),
            opt("history-limit", OptionValue::Single(Value::Number(-5))),
            opt("prefix", OptionValue::Single(Value::Key("C-a".into()))),
            opt(
                "status-bg",
                OptionValue::Single(Value::Colour("red".into())),
            ),
            opt("mouse", OptionValue::Single(Value::Flag(false))),
            opt(
                "status-position",
                OptionValue::Single(Value::Choice("top".into())),
            ),
            opt(
                "default-command",
                OptionValue::Single(Value::Command("display x".into())),
            ),
            opt(
                "command-alias",
                OptionValue::Array(vec![
                    ArrayItem {
                        key: ArrayKey::Index(0),
                        value: Value::String("a=b".into()),
                    },
                    ArrayItem {
                        key: ArrayKey::Name("k".into()),
                        value: Value::String(String::new()),
                    },
                ]),
            ),
            opt("empty-array", OptionValue::Array(Vec::new())),
        ]
    }

    fn env() -> Vec<EnvVar> {
        vec![
            EnvVar {
                name: "A".into(),
                value: Some("1".into()),
                hidden: false,
            },
            EnvVar {
                name: "B".into(),
                value: None,
                hidden: true,
            },
            EnvVar {
                name: "C".into(),
                value: Some(String::new()),
                hidden: false,
            },
        ]
    }

    #[expect(clippy::too_many_lines, reason = "one field after another")]
    pub(crate) fn sample_server() -> Server {
        let bind = Binding {
            key: "C-b".into(),
            note: Some("send prefix".into()),
            repeat: true,
            command: "send-prefix".into(),
        };
        Server {
            pid: 4242,
            socket_path: "/tmp/tmux-1000/default".into(),
            start_time_us: 1_790_000_000_123_456,
            server_options: sample_options(),
            session_options: sample_options(),
            window_options: sample_options(),
            environment: env(),
            hook_events: vec!["after-new-window".into()],
            hook_fires: vec![HookFire {
                scope: Scope::Session(3),
                name: "after-new-window".into(),
                count: 2,
                time: 1_790_000_001,
            }],
            monitors: vec![Monitor {
                scope: Scope::GlobalWindow(()),
                name: "@m".into(),
                what: "pane".into(),
                format: "#{pane_title}".into(),
                notify_true: true,
                session: Some(1),
                serial: 7,
                fire_count: 1,
                fire_time: 9,
                last: Some("t".into()),
                each: vec![SubValue {
                    id: 2,
                    index: 1,
                    value: "v".into(),
                }],
                due_in_ms: Some(700),
            }],
            monitor_serial: 7,
            key_tables: vec![KeyTable {
                name: "prefix".into(),
                bindings: vec![bind.clone(), Binding::default()],
                defaults: vec![bind],
            }],
            buffers: vec![PasteBuffer {
                name: "buffer0".into(),
                data: vec![0, 0xff, b'\n'],
                automatic: true,
                order: 3,
                created: 1_790_000_002,
            }],
            buffer_next_index: 1,
            buffer_next_order: 4,
            messages: vec![Message {
                number: 1,
                time_us: 1_790_000_000_000_001,
                text: "hi".into(),
            }],
            message_next: 2,
            next_session: 4,
            next_window: 5,
            next_pane: 6,
            next_client: 7,
            command_history: vec!["ls".into()],
            search_history: vec!["foo".into(), "bar".into()],
            wait_channels: vec![WaitChannel {
                name: "ch".into(),
                locked: true,
                woken: true,
                waiters: vec![1, 2],
                lockers: vec![3],
            }],
            marked: Some(Marked {
                session: 0,
                window_index: 1,
                window: 2,
                pane: 3,
            }),
            default_term: "screen".into(),
            activity: 99,
            config_files: vec!["/etc/tmux.conf".into(), "/home/u/.tmux.conf".into()],
            config_causes: vec!["/home/u/.tmux.conf:3: unknown command: nope".into()],
            cmd_number: 41,
            active_points: 17,
            link_stamp: 5,
            marked_elsewhere: Some(1),
            access: vec![
                Access {
                    group: false,
                    id: 1000,
                    read_only: false,
                },
                Access {
                    group: true,
                    id: 20,
                    read_only: true,
                },
            ],
            // Added within v1, so out of the sample the golden file pins.
            start_binary: String::new(),
            persist_hold: 0,
            link_number: 0,
        }
    }

    pub(crate) fn sample_session() -> Session {
        Session {
            id: 3,
            name: "main".into(),
            created: 1_790_000_000,
            activity: 12,
            activity_secs: 1_790_000_100,
            cwd: "/home/u".into(),
            windows: vec![
                Winlink {
                    index: 0,
                    window: 1,
                    alerts: 1,
                    seq: 0,
                },
                Winlink {
                    index: 4,
                    window: 2,
                    alerts: 0,
                    seq: 1,
                },
            ],
            current: 4,
            last: vec![0],
            environment: env(),
            last_attached: Some(1_790_000_050),
            options: sample_options(),
            group: Some("g".into()),
            link_tree: vec![RbNode { key: 0, red: false }, RbNode { key: 4, red: true }],
            lock_in_ms: Some(1500),
            // Added within v1: out of the golden sample
            // (a_path_that_is_not_utf8_round_trips).
            cwd_bytes: None,
        }
    }

    pub(crate) fn sample_layout() -> Layout {
        let g = |sx, sy, xoff, yoff| Geometry { sx, sy, xoff, yoff };
        Layout {
            sx: 80,
            sy: 24,
            root: LayoutCell::Node(Node {
                dir: Dir::TopBottom,
                g: g(80, 24, 0, 0),
                children: vec![
                    LayoutCell::Pane(Leaf {
                        pane: 1,
                        g: g(80, 12, 0, 0),
                    }),
                    LayoutCell::Node(Node {
                        dir: Dir::LeftRight,
                        g: g(80, 11, 0, 13),
                        children: vec![
                            LayoutCell::Pane(Leaf {
                                pane: 2,
                                g: g(40, 11, 0, 13),
                            }),
                            LayoutCell::Pane(Leaf {
                                pane: 3,
                                g: g(39, 11, 41, 13),
                            }),
                        ],
                    }),
                    LayoutCell::Floating(Leaf {
                        pane: 4,
                        g: g(20, 5, -3, 2),
                    }),
                ],
            }),
            pane_status: 1,
            scrollbar_always: Some(1),
            last_new_x: 5,
            last_new_y: -1,
            saved_float: vec![Leaf {
                pane: 2,
                g: g(10, 10, 1, 1),
            }],
            borderless: vec![4],
        }
    }

    pub(crate) fn sample_window() -> Window {
        Window {
            id: 2,
            name: "w".into(),
            activity: 13,
            activity_secs: 1_790_000_101,
            layout: sample_layout(),
            panes: vec![1, 2, 3, 4],
            active: Some(4),
            last: vec![1, 2],
            z_order: vec![4, 1, 2, 3],
            base_index: 1,
            modal: Some(4),
            modal_last: Some(1),
            zoom: Some(Zoom {
                pane: 1,
                saved: sample_layout(),
            }),
            was_zoomed: Some(1),
            last_layout: Some("tiled".into()),
            old_layout: Some("b25d,80x24,0,0,1".into()),
            pending_size: Some((100, 30)),
            manual_size: (90, 20),
            latest_client: Some(7),
            alerts: 3,
            silence_in_ms: Some(1500),
            options: sample_options(),
            // Added within v1, so out of the sample the golden file pins.
            cell_pixels: (0, 0),
            pending_pixels: (0, 0),
        }
    }

    fn sample_copy() -> CopyMode {
        let p = |x, y| Pos { x, y };
        CopyMode {
            history: vec![HistoryPage::from_rows(&[sample_row()], true)],
            rows: vec![sample_row(), Row::default()],
            vi: true,
            sx: 20,
            sy: 2,
            oy: 1,
            cx: 3,
            cy: 1,
            lastcx: 3,
            lastsx: 20,
            scroll_exit: true,
            hide_position: true,
            position_clip: Some(10),
            line_numbers: 2,
            prefix: 5,
            selecting: true,
            selpos: p(1, 2),
            endpos: p(3, 4),
            line_select: 1,
            rect: true,
            sel_mode: 2,
            dpos: p(5, 6),
            selr: p(7, 8),
            endselr: p(9, 10),
            separators: " -_".into(),
            search_text: Some("needle".into()),
            search_type: 1,
            search_regex: true,
            pane_search: Some(("needle".into(), true)),
            jump_type: 2,
            jump_char: "x".into(),
            mark: p(0, 1),
            show_mark: true,
            scrolled: true,
            source_pane: Some(9),
            key_mode: 1,
            kill: true,
            hscrolled: 3,
            grid_sx: 21,
            grid_sy: 2,
            recentre: 2,
            recentre_line: 6,
            drag: 1,
            sel: Some(ScreenSel {
                sx: 1,
                sy: 2,
                ex: 3,
                ey: 4,
                rectangle: true,
                vi: true,
                hidden: true,
                // Came within v1: the golden sample leaves it out.
                clipx: 0,
            }),
            search_down: true,
            search_origin: Some((1, (2, 3))),
            search_all: true,
            search_marks: Some(vec![0, 1, 2]),
            search_generation: 2,
            search_count: -1,
            search_more: true,
            search_timeout: true,
            refresh_in_ms: Some(50),
            foreign: true,
        }
    }

    pub(crate) fn sample_pane() -> Pane {
        Pane {
            id: 1,
            window: 2,
            pid: 31337,
            tty: "/dev/pts/9".into(),
            argv: vec!["cat".into(), "-".into()],
            shell: "/bin/sh".into(),
            cwd: "/tmp".into(),
            environment: env(),
            empty: true,
            exited: true,
            status: Some(ExitStatus {
                code: Some(0),
                signal: Some(9),
                core_dumped: true,
            }),
            dead_time: Some(1_790_000_200),
            changed: true,
            unseen_changes: true,
            geometry: Geometry {
                sx: 80,
                sy: 12,
                xoff: 0,
                yoff: 0,
            },
            history_limit: 2000,
            input_off: true,
            focused: true,
            active_point: 8,
            options: sample_options(),
            modes: vec![
                PaneMode::Copy(sample_copy()),
                PaneMode::View(CopyMode::default()),
                PaneMode::Clock(2),
                PaneMode::Closed(()),
            ],
            search: Some(("s".into(), false)),
            pipe: Some(PipePane {
                pid: 77,
                command: "cat >/tmp/log".into(),
                input: true,
                output: true,
                pending: b"not yet taken".to_vec(),
            }),
            term: Some(sample_term()),
            over_zoom: true,
            capture_keys: true,
            close_on_click: true,
            close_on_cancel: true,
            title: "t".into(),
            output_ago_ms: Some(30),
            resize_in_ms: Some(250),
            resizes: vec![((80, 24), (40, 24)), ((40, 24), (39, 24))],
            rename_ago_ms: Some(120),
            // Added within v1: out of the golden sample
            // (an_auto_hide_scrollbar_round_trips).
            scrollbar_visible: false,
            scrollbar_hover: false,
            scrollbar_hide_in_ms: None,
            // Added within v1 too (a_panes_shell_commands_round_trip).
            output_generation: 0,
            last_output_time: None,
            last_prompt_time: None,
            command_start_time: None,
            command_end_time: None,
            command_running: false,
            command_status: None,
            kitty_owner_primary: None,
            kitty_owner_alternate: None,
            // Added within v1 too (a_path_that_is_not_utf8_round_trips).
            cwd_bytes: None,
            // Added within v1 too (input_not_yet_written_round_trips).
            input_queued: Vec::new(),
        }
    }

    #[expect(clippy::too_many_lines, reason = "one client with every field set")]
    pub(crate) fn sample_client() -> Client {
        Client {
            id: 7,
            name: "/dev/pts/3".into(),
            tty: "/dev/pts/3".into(),
            pid: 999,
            version: 8,
            argv: vec![b"attach".to_vec(), vec![0xff]],
            cwd: "/home/u".into(),
            environment: vec![("TERM".into(), "xterm-256color".into())],
            term_name: "xterm-256color".into(),
            term_features: vec!["RGB".into(), "sync".into()],
            utf8: true,
            session: Some(3),
            last_session: Some(0),
            was_attached: true,
            created: 1_790_000_010,
            activity: 14,
            activity_secs: 1_790_000_102,
            size: (80, 24),
            pixel_size: (800, 480),
            flags: vec!["read-only".into(), "ignore-size".into()],
            key_table: "prefix".into(),
            repeating: true,
            last_key: Some("Up".into()),
            repeat_in_ms: Some(400),
            table_ago_ms: 20,
            focused: true,
            suspended: true,
            title: Some("title".into()),
            control: Some(ControlClient {
                level: 2,
                subscriptions: vec![
                    Subscription {
                        name: "s".into(),
                        target: String::new(),
                        format: "#{session_name}".into(),
                        last: Some("main".into()),
                        each: Vec::new(),
                    },
                    Subscription {
                        name: "p".into(),
                        target: "%*".into(),
                        format: "#{pane_title}".into(),
                        last: None,
                        each: vec![SubValue {
                            id: 1,
                            index: 0,
                            value: "t".into(),
                        }],
                    },
                ],
                pause_after: Some(5),
                panes: vec![
                    ControlPane {
                        pane: 1,
                        output: PaneOutput::Paused,
                        offset: 4096,
                    },
                    ControlPane {
                        pane: 2,
                        output: PaneOutput::Off,
                        offset: 0,
                    },
                ],
                window_sizes: vec![WindowSize {
                    window: 2,
                    sx: 100,
                    sy: 40,
                }],
                size: Some((120, 50)),
                retval: 1,
                queued: vec![
                    ControlQueued {
                        pane: None,
                        data: b"%begin 1 2 0\n".to_vec(),
                        age_ms: 0,
                    },
                    ControlQueued {
                        pane: Some(1),
                        data: b"out\x1b[m".to_vec(),
                        age_ms: 120,
                    },
                ],
                ready: true,
                eof: true,
                inbuf: b"ls\n".to_vec(),
                guard_depth: 1,
                subs_due_ms: Some(1000),
            }),
            popup: Some(Popup {
                pid: 55,
                argv: vec!["sh".into(), "-c".into(), "top".into()],
                cwd: "/".into(),
                x: 10,
                y: -1,
                sx: 40,
                sy: 10,
                title: "pop".into(),
                style: Some("bg=red".into()),
                border_style: Some("fg=blue".into()),
                border_lines: "rounded".into(),
                close_on_exit: 2,
                term: Some(sample_term()),
                pane: Some(1),
                exited: true,
                status: Some(ExitStatus::default()),
                preferred: ((1, 2), (3, 4)),
                any_key: true,
                status_code: 129,
                caller: Some(7),
                // Added within v1: out of the golden sample
                // (input_not_yet_written_round_trips).
                input_queued: Vec::new(),
            }),
            token: 0xdead_beef,
            fg: Some(Rgb { r: 1, g: 2, b: 3 }),
            bg: None,
            theme: Some("dark".into()),
            palette: vec![PaletteEntry {
                index: 4,
                rgb: Rgb {
                    r: 0,
                    g: 0xff,
                    b: 9,
                },
            }],
            written: 12_345,
            term_replies: vec![
                TermReply {
                    kind: 1,
                    params: vec![62, 22],
                    text: String::new(),
                },
                TermReply {
                    kind: 3,
                    params: Vec::new(),
                    text: "ghostty 1.2".into(),
                },
            ],
            tmux_invocation: true,
            left_session: Some(2),
            has_tty: true,
            open_features: vec!["xterm*:sixel".into()],
            open_overrides: vec!["xterm*:Tc".into()],
            status_message: Some(StatusMessage {
                text: Some("hello".into()),
                ignore_styles: true,
                ignore_keys: false,
                frozen: true,
                timer_in_ms: Some(900),
            }),
            // Added within v1: out of the golden sample
            // (a_clients_terminal_queries_round_trip).
            queries: None,
            // Added within v1 too (a_clients_terminal_title_round_trips).
            terminal_title: None,
            // Added within v1 too (a_clients_pan_round_trips).
            pan: None,
            // Added within v1 too (a_clients_view_round_trips).
            view: None,
            // Added within v1 too (a_path_that_is_not_utf8_round_trips).
            cwd_bytes: None,
        }
    }

    #[test]
    fn every_model_record_round_trips() {
        let server = sample_server();
        round_trip(&server);
        for o in &server.server_options {
            round_trip(o);
            round_trip(&o.value);
        }
        round_trip(&server.environment[1]);
        round_trip(&server.hook_fires[0]);
        round_trip(&server.monitors[0]);
        round_trip(&server.key_tables[0]);
        round_trip(&server.key_tables[0].bindings[0]);
        round_trip(&server.buffers[0]);
        round_trip(&server.messages[0]);
        round_trip(&server.wait_channels[0]);
        round_trip(server.marked.as_ref().unwrap());
        for s in [
            Scope::Server(()),
            Scope::GlobalSession(()),
            Scope::GlobalWindow(()),
            Scope::Session(1),
            Scope::Window(0),
            Scope::Pane(2),
        ] {
            round_trip(&s);
        }

        let session = sample_session();
        round_trip(&session);
        round_trip(&session.windows[0]);

        let window = sample_window();
        round_trip(&window);
        round_trip(&window.layout);
        round_trip(&window.layout.root);
        round_trip(window.zoom.as_ref().unwrap());

        let pane = sample_pane();
        round_trip(&pane);
        round_trip(pane.status.as_ref().unwrap());
        round_trip(pane.pipe.as_ref().unwrap());
        for m in &pane.modes {
            round_trip(m);
        }

        let client = sample_client();
        round_trip(&client);
        let control = client.control.as_ref().unwrap();
        round_trip(control);
        round_trip(&control.subscriptions[1]);
        round_trip(&control.panes[0]);
        round_trip(&control.queued[1]);
        round_trip(&control.window_sizes[0]);
        round_trip(client.popup.as_ref().unwrap());

        for role in [
            Role::Listener(()),
            Role::Client(1),
            Role::PanePty(2),
            Role::PopupPty(3),
            Role::PipePane(4),
            Role::Snapshot(()),
        ] {
            round_trip(&Descriptor { fd: 5, role });
        }
    }

    #[test]
    fn defaults_and_some_default_survive() {
        round_trip(&Pane::default());
        round_trip(&Client::default());
        // `opt` keeps Some(default) apart from None.
        let w = Window {
            active: Some(0),
            pending_size: Some((0, 0)),
            ..Window::default()
        };
        round_trip(&w);
        assert_ne!(w, Window::default());
    }

    #[test]
    fn a_oneof_with_no_known_variant_is_an_error() {
        let mut out = Vec::new();
        crate::wire::put_field(&mut out, 99, |_| {});
        assert!(LayoutCell::get(&out, &Ctx::new(1)).is_err());
    }

    #[test]
    fn an_unknown_pane_mode_reads_as_closed() {
        // A mode from a newer writer, with a payload this reader can't read.
        let mut out = Vec::new();
        crate::wire::put_field(&mut out, 99, |o| o.extend_from_slice(b"\xff\xff"));
        assert_eq!(PaneMode::get(&out, &Ctx::new(1)), Ok(PaneMode::Closed(())));
        // In a pane's stack, beside the modes it knows.
        let mut pane = Vec::new();
        crate::wire::put_field(&mut pane, 21, |o| PaneMode::Clock(1).put(o));
        crate::wire::put_field(&mut pane, 21, |o| o.extend_from_slice(&out));
        let p = Pane::get(&pane, &Ctx::new(1)).unwrap();
        assert_eq!(p.modes, [PaneMode::Clock(1), PaneMode::Closed(())]);
    }
}
