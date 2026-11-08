//! The server: one `mio` loop on one thread (D9) over the listening
//! socket, client connections, pane PTYs and signals.
//!
//! A client connects, sends `Hello` (argv, cwd, environment, tty), the
//! server parses and runs the argv as one batch, relays the batch's output
//! as `Stdout`/`Stderr` frames and ends with `Exit`. A client attached by
//! its command stays connected and is handed to the attach hooks
//! ([`Server::attach_client`], [`Server::on_client_input`]).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use signal_hook::consts::{SIGCHLD, SIGINT, SIGTERM, SIGUSR1};
use signal_hook_mio::v1_0::Signals;
use vtmux_core::cmd::{self, CmdError, ConfigEnv};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::layout::{Geometry, Layout as CoreLayout, WindowPanes};
use vtmux_core::options::{Defaults, OptionValue, OptionsStore, TreeId, Value};
use vtmux_core::protocol::{self, ClientMsg, Hello, ProtocolError, ServerMsg, Speaks};
use vtmux_vt::{PaneTerm, TermEvent};

use crate::alerts;
use crate::commands::{self, After, Item};
use crate::environ::Environ;
use crate::format_ctx::{ClientView, Ctx, ServerInfo};
use crate::model::{Model, Pane, Session, Window};
use crate::names::parse_window_name;
use crate::pane_io::{PaneIo, ReadResult};
use crate::sys;
use crate::target::Origin;
use crate::wait::WaitChannels;

const LISTENER: Token = Token(0);
const SIGNALS: Token = Token(1);
/// Client tokens start here; pane tokens are `PANE_BASE + pane id`.
const CLIENT_BASE: usize = 1 << 32;
const PANE_BASE: usize = 16;
/// Tokens of the PTYs a live upgrade's last resort holds
/// (`resume::Server::hold`): `HELD_BASE + index in held`, above any
/// client's.
const HELD_BASE: usize = 1 << 48;

/// `NAME_INTERVAL` (tmux.h:119): automatic-rename runs at most this often.
const NAME_INTERVAL: Duration = Duration::from_millis(500);
/// How long a client told to go may take to read the rest of its output
/// (`server_client_start_exit_timer`, server-client.c:2365).
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a pane no client shows keeps its screen view after its last
/// output ([`Server::release_hidden_screens`]).
const HIDDEN_VIEW_IDLE: Duration = Duration::from_secs(1);

pub mod persist;
pub mod restore;
pub mod resume;
pub mod snapshot;
pub mod upgrade;

pub use resume::{check_snapshot, resume};

/// A connected client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub usize);

/// How the server was started.
#[derive(Clone, Debug, Default)]
pub struct ServerConfig {
    /// `-f` files, or tmux's default search list when none was given.
    pub config_files: Vec<PathBuf>,
    /// The files came from the default list: a missing one is not an error
    /// (`cfg_quiet`, tmux.c).
    pub config_quiet: bool,
    /// The client's environment, which becomes the global environment
    /// (tmux.c:417-421).
    pub environ: Vec<(String, String)>,
    /// The starting client's working directory (`PWD` in the global
    /// environment).
    pub cwd: Option<PathBuf>,
    /// The socket is the default `-L` one, not `-S` (`CLIENT_DEFAULTSOCKET`):
    /// it decides the permissions SIGUSR1 recreates it with.
    pub default_socket: bool,
    /// A live upgrade's dry run (`server/resume.rs`): the server is built
    /// to prove the snapshot rebuilds, never run, and leaves nothing
    /// behind (no shim).
    pub dry_run: bool,
    /// The path the server was started as ([`upgrade::start_binary`]):
    /// what `vtmux-upgrade` without `-b` resolves (live-upgrade spec,
    /// decision 22).
    pub binary: Option<PathBuf>,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent client flags (CLIENT_*)"
)]
pub(crate) struct Client {
    stream: UnixStream,
    inbuf: Vec<u8>,
    /// Messages not yet written to the socket, oldest first.
    out: Outbox,
    /// Frames sent but not yet written to the client's terminal.
    unacked: Unacked,
    /// Exit has been queued; close once the output is flushed.
    pub(crate) closing: bool,
    /// `c->exit_timer` (server-client.c:2361-2387): when a closing client
    /// whose output has not drained is looked at again.
    exit_deadline: Option<Instant>,
    /// When the socket last took a byte, for the exit timer.
    last_write: Instant,
    /// `CLIENT_DEAD` (server-client.c:491): being dropped. Nothing more is
    /// sent to it, and a second drop (from a write that fails while its
    /// own lost-client hooks run) does nothing.
    pub(crate) dead: bool,
    /// `CLIENT_SUSPENDED` (`suspend-client`): the terminal is the shell's
    /// until the client sends `Wakeup`. Nothing is drawn, the client sizes
    /// no window and is not listed, but it keeps its session.
    pub(crate) suspended: bool,
    /// The connecting process's uid and gid (`getpeereid`, proc.c:331),
    /// checked against the server's access list.
    peer: Option<(u32, u32)>,
    pub(crate) hello: Option<Hello>,
    /// Which frames the client reads: a control client of another
    /// protocol version is sent the frozen ones only (protocol module
    /// docs).
    speaks: Speaks,
    pub(crate) attached: Option<SessionId>,
    /// Control mode (`-C`) state; `None` for terminal and command clients.
    pub(crate) control: Option<crate::control::ControlState>,
    /// `c->activity_time` as a model activity stamp: set when the client
    /// connects (server-client.c:289), bumped by its keys (1467).
    pub(crate) activity: u64,
    /// `c->activity_time` and `c->creation_time` in epoch seconds, for
    /// `#{client_activity}` / `#{client_created}`.
    pub(crate) activity_secs: i64,
    pub(crate) created_secs: i64,
    /// `CLIENT_FOCUSED`: set at creation, cleared by a focus-out report.
    pub(crate) focused: bool,
    /// Key table state (`c->keytable`, `CLIENT_REPEAT`, `c->last_key`,
    /// `c->repeat_timer`).
    pub(crate) keys: crate::key_dispatch::ClientKeys,
    /// `c->last_session`.
    pub(crate) last_session: Option<SessionId>,
    /// `CLIENT_ATTACHED`: client-detached fires when it goes.
    pub(crate) was_attached: bool,
    /// The session it was last in, for the client-detached payload
    /// (`cmd_find_from_client` on a client whose session went NULL finds
    /// the best session; tmux keeps `c->session` until the client is
    /// lost, server-client.c:2733).
    pub(crate) left_session: Option<SessionId>,
    /// The status lines as drawn, the message shown over them, and their
    /// redraw flag and timers (`c->status`, `c->message_string`).
    pub(crate) status: crate::render::status::StatusLine,
    /// Frame bytes sent to the client (`c->written`).
    pub(crate) written: usize,
    pub(crate) pasting: bool,
    /// Mouse drag, click and outer-mode state (`c->tty.mouse_*`).
    pub(crate) mouse: crate::mouse::ClientMouse,
    pub(crate) decoder: crate::input::Decoder,
    pub(crate) caps: Option<crate::render::Caps>,
    /// The client's terminal once attached (`tty_open`): its features and
    /// capabilities after `terminal-features` and `terminal-overrides`.
    pub(crate) term: Option<std::rc::Rc<vtmux_core::term_features::Term>>,
    /// `terminal-features` and `terminal-overrides` as `term` was opened
    /// with: a snapshot opens it again from these, not from the options
    /// now (D27).
    pub(crate) term_options: (Vec<String>, Vec<String>),
    /// The hello's environment (`c->environ`), shared with every
    /// [`ClientView`] of the client instead of copied into each.
    environ: std::rc::Rc<[(String, String)]>,
    pub(crate) compositor: crate::render::Compositor,
    /// The scene and composed rows of the client's last frame, reused
    /// where nothing under them changed.
    pub(crate) window_cache: crate::render::WindowCache,
    /// `DCS tmux;` payloads for the terminal, written after this tick's
    /// frame (`tty_cmd_rawstring`).
    pub(crate) passthrough: Vec<u8>,
    /// The command a status-line prompt answers to (`c->prompt`'s
    /// callbacks); the prompt itself is in `status`.
    pub(crate) prompt_owner: Option<crate::open_prompt::Owner>,
    /// The popup over the client's screen (`c->overlay_data` with
    /// `popup_draw_cb`).
    pub(crate) popup: Option<crate::popup::Popup>,
    /// `c->flags` the commands can change (`switch-client -r`,
    /// `refresh-client -f`).
    pub(crate) flags: crate::client_flags::ClientFlags,
    /// `TTY_OSC52QUERY`: a clipboard query (`refresh-client -l`) is out
    /// until this instant, and its reply becomes a paste buffer.
    pub(crate) clipboard_query: Option<Instant>,
    /// What its terminal reported: colours and theme.
    pub(crate) reported: crate::term_queries::Reported,
    /// `tty->xpixel`, `tty->ypixel`: a cell's width and height in pixels,
    /// 0 while unknown (`#{client_cell_width}`, `#{client_cell_height}`).
    pub(crate) cell_pixels: (u32, u32),
    /// `c->title`: the last title written to the terminal, so an
    /// unchanged one is not written again.
    title: Option<String>,
    /// The title the terminal shows from this client's output: the last
    /// written by `set-titles` or by a pane's passthrough (an OSC 0 or 2
    /// in a `DCS tmux;` payload). A re-attach after a live upgrade gives
    /// it again, as the re-executing client's teardown popped it (CSI 23
    /// t) where tmux, with no upgrade, would have left it.
    pub(crate) terminal_title: Option<String>,
    /// The terminal's replies that changed its features, in order: a
    /// snapshot replays them onto the terminal opened again (D27).
    pub(crate) term_replies: Vec<snapshot::TermReply>,
    /// Between connections across a live upgrade (`upgrade.rs`,
    /// decision 21): drawn nothing until it re-attaches.
    pub(crate) reattach: Option<upgrade::Reattach>,
    /// The part of its current window the client shows (`tty->oox` ...,
    /// [`crate::view`]).
    pub(crate) view: crate::view::View,
    /// `c->pan_window`, `pan_ox`, `pan_oy`: a view panned by
    /// `refresh-client`.
    pub(crate) pan: Option<crate::view::Pan>,
}

impl Client {
    /// `server_client_update_theme_colours` (server-client.c:1274): the
    /// theme colours follow the terminal's theme
    /// ([`crate::render::Caps::light_theme`]).
    pub(crate) fn update_theme(&mut self) {
        let light = self.reported.light();
        if let Some(caps) = self.caps.as_mut() {
            caps.light_theme = light;
        }
    }

    /// Bytes queued for the socket and not yet written: a control
    /// client's `EVBUFFER_LENGTH(cs->write_event->output)`.
    pub(crate) fn unwritten(&self) -> usize {
        self.out.bytes
    }

    /// A client just accepted on `stream`, at model activity `activity`.
    fn new(stream: UnixStream, activity: u64) -> Self {
        let peer = {
            use std::os::fd::AsFd;
            sys::user::peer_ids(stream.as_fd())
        };
        Self {
            peer,
            stream,
            inbuf: Vec::new(),
            out: Outbox::default(),
            unacked: Unacked::default(),
            closing: false,
            exit_deadline: None,
            last_write: Instant::now(),
            dead: false,
            suspended: false,
            hello: None,
            speaks: Speaks::Full,
            attached: None,
            control: None,
            activity,
            activity_secs: now_epoch(),
            created_secs: now_epoch(),
            focused: true,
            keys: crate::key_dispatch::ClientKeys::default(),
            last_session: None,
            was_attached: false,
            left_session: None,
            status: crate::render::status::StatusLine::default(),
            prompt_owner: None,
            popup: None,
            flags: crate::client_flags::ClientFlags::default(),
            clipboard_query: None,
            reported: crate::term_queries::Reported::default(),
            cell_pixels: (0, 0),
            title: None,
            terminal_title: None,
            term_replies: Vec::new(),
            written: 0,
            pasting: false,
            mouse: crate::mouse::ClientMouse::default(),
            decoder: crate::input::Decoder::default(),
            caps: None,
            term: None,
            term_options: (Vec::new(), Vec::new()),
            environ: std::rc::Rc::from([]),
            compositor: crate::render::Compositor::default(),
            window_cache: crate::render::WindowCache::default(),
            passthrough: Vec::new(),
            reattach: None,
            view: crate::view::View::default(),
            pan: None,
        }
    }
}

/// What a queued message is, for what may be dropped when the client is
/// told to exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutKind {
    /// A control client's `%output` line: tmux keeps pane output in
    /// blocks it has not written yet and drops them at exit
    /// (control.c `control_discard`).
    PaneOutput,
    /// The `Exit` or `Exec` that tells the client to go: never dropped.
    Last,
    /// Anything else.
    Other,
}

/// One encoded message and how much of it the socket has taken.
#[derive(Debug)]
struct Queued {
    bytes: Vec<u8>,
    written: usize,
    kind: OutKind,
}

/// A client's unwritten messages. Kept as messages, not one byte stream,
/// so whole messages can be dropped without cutting one in half.
#[derive(Debug, Default)]
struct Outbox {
    queue: std::collections::VecDeque<Queued>,
    /// Bytes not yet written, over the whole queue.
    bytes: usize,
}

impl Outbox {
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn push(&mut self, bytes: Vec<u8>, kind: OutKind) {
        self.bytes += bytes.len();
        self.queue.push_back(Queued {
            bytes,
            written: 0,
            kind,
        });
    }

    /// Drop every message not yet started that `drop` picks; a message
    /// part-written stays, or the stream would be corrupt.
    fn discard(&mut self, drop: impl Fn(OutKind) -> bool) {
        self.queue.retain(|q| q.written != 0 || !drop(q.kind));
        self.bytes = self.queue.iter().map(|q| q.bytes.len() - q.written).sum();
    }

    /// Write what the socket takes: `Ok(true)` if it took anything.
    fn write_to(&mut self, stream: &mut UnixStream) -> io::Result<bool> {
        let mut progress = false;
        while !self.queue.is_empty() {
            let slices: Vec<io::IoSlice<'_>> = self
                .queue
                .iter()
                .take(64)
                .map(|q| io::IoSlice::new(&q.bytes[q.written..]))
                .collect();
            let mut n = match stream.write_vectored(&slices) {
                Ok(0) => return Ok(progress),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(progress),
                Err(e) => return Err(e),
            };
            progress = true;
            self.bytes -= n;
            while n > 0 {
                let front = self.queue.front_mut().expect("written past the queue");
                let left = front.bytes.len() - front.written;
                if n >= left {
                    n -= left;
                    self.queue.pop_front();
                } else {
                    front.written += n;
                    n = 0;
                }
            }
        }
        Ok(progress)
    }
}

/// Frame bytes the client has not yet written to its terminal: in
/// `out`, the socket, or the client's own buffer. This is the queue
/// `tty_block_maybe` measures (tty.c:219-245, `tty->out`); `out` alone
/// would miss the buffers in front of a slow terminal.
#[derive(Default)]
struct Unacked {
    /// Encoded size of each unacknowledged frame, oldest first.
    frames: std::collections::VecDeque<usize>,
    /// Frames sent before the oldest in `frames`.
    acked: u64,
    bytes: usize,
}

impl Unacked {
    fn sent(&mut self, len: usize) {
        self.frames.push_back(len);
        self.bytes += len;
    }

    /// The client has written the first `seq` frames.
    fn ack(&mut self, seq: u64) {
        while self.acked < seq {
            let Some(len) = self.frames.pop_front() else {
                break;
            };
            self.bytes -= len;
            self.acked += 1;
        }
    }
}

/// What a new pane needs ([`Server::spawn_pane`]).
#[derive(Debug)]
pub struct PaneSpawn<'a> {
    pub pane: PaneId,
    pub window: WindowId,
    /// The session whose environment, options and ID the pane gets.
    pub session: SessionId,
    pub session_env: &'a Environ,
    /// `-e NAME=value` for this pane only (split-window).
    pub extra_env: &'a [String],
    /// The command client's `PATH` (spawn.c:418-424).
    pub client_path: Option<&'a str>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The pane's rectangle; its size is the PTY's.
    pub rect: Geometry,
    /// respawn-pane keeps the pane's shell (spawn.c:422-428); otherwise
    /// it comes from `default-shell`.
    pub shell: Option<PathBuf>,
    /// `SPAWN_EMPTY`: no process; the pane only shows what is written to
    /// it (`display-message -I`, `split-window -I`).
    pub empty: bool,
}

/// An empty pane's screen modes (spawn.c:460-462): no cursor, and a line
/// feed also returns the carriage (`MODE_CRLF`).
pub(crate) fn empty_pane_modes(term: &mut vtmux_vt::PaneTerm) {
    term.feed(b"\x1b[?25l");
    term.set_crlf(true);
}

/// The cause a [`Server::spawn_pane`] failure gives: `fork failed:` and
/// C's `strerror` text (spawn.c:480), without Rust's ` (os error N)`.
#[must_use]
pub fn spawn_failed(e: &io::Error) -> String {
    format!("fork failed: {}", vtmux_core::error::strerror(e))
}

/// The client's `PATH`, if it sent one.
#[must_use]
pub fn client_path(env: &[(String, String)]) -> Option<&str> {
    env.iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
}

/// What a new session needs ([`Server::create_session`]).
#[derive(Debug)]
pub struct NewSession {
    pub name: Option<String>,
    pub window_name: Option<String>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub client_env: Vec<(String, String)>,
    /// `-e NAME=value`.
    pub extra_env: Vec<String>,
    /// Copy `update-environment` variables from the client.
    pub update_env: bool,
    /// `-x`/`-y` given: the session's `default-size`.
    pub default_size: Option<(u16, u16)>,
    /// `-t`: the session group to join.
    pub group: Option<NewGroup>,
}

/// The group a new session joins (cmd-new-session.c:143-160, 313-327).
#[derive(Debug)]
pub struct NewGroup {
    /// The group's name, which is also the prefix of a generated session
    /// name.
    pub name: String,
    /// The session the target named, grouped with it first if it is in
    /// no group yet.
    pub with: Option<SessionId>,
}

/// What a new window needs ([`Server::new_window`]).
#[derive(Debug)]
pub struct NewWindow<'a> {
    pub session: SessionId,
    /// The index asked for; `None` is the first free one from base-index.
    pub index: Option<u32>,
    /// `-k`: replace the window at `index`.
    pub kill: bool,
    /// `-d`: do not make it current.
    pub detached: bool,
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub extra_env: &'a [String],
    pub client_path: Option<&'a str>,
    /// The target client (`sc->tc`): it sizes the window and becomes its
    /// latest client (spawn.c:192-202).
    pub client: Option<ClientId>,
    /// `SPAWN_EMPTY`: its pane runs nothing.
    pub empty: bool,
}

pub struct Server {
    pub model: Model,
    pub info: ServerInfo,
    pub global_env: Environ,
    /// The theme colours last worked out ([`crate::render::window::Source::theme`]).
    pub(crate) themes: crate::render::window::ThemeCache,
    /// `kill-server` ran: exit once replies are out.
    pub shutdown: bool,
    /// Nesting of `source-file`: with no client (cmd-source-file.c
    /// `cmd_source_file_depth`) and per client (`c->source_file_depth`).
    pub source_depths: BTreeMap<Option<ClientId>, usize>,
    /// Errors from config files, shown to the first client
    /// (cfg.c `cfg_add_cause`).
    pub causes: Vec<String>,
    pub(crate) socket: PathBuf,
    pub(crate) poll: Poll,
    /// `None` once the server is exiting: a client that connects then is
    /// refused, not reset (`no server running`, client.c:291-295).
    listener: Option<UnixListener>,
    signals: Signals,
    pub(crate) clients: BTreeMap<ClientId, Client>,
    next_client: usize,
    /// The last command number handed out (cmd-queue.c `cmdq_next`'s
    /// static `number`): every fired command takes one.
    pub(crate) cmd_number: u32,
    config: ServerConfig,
    config_loaded: bool,
    /// `cfg_finished`: the startup config has run.
    pub(crate) cfg_finished: bool,
    /// `cfg_client` (the client that started the server, whose directory
    /// jobs use while the config loads) and `cfg_item` (its waiting step).
    pub(crate) cfg_client: Option<ClientId>,
    pub(crate) cfg_wait: Option<crate::cmdq::StepId>,
    /// Every command queue (cmd-queue.c).
    pub(crate) cmdq: crate::cmdq::Cmdq,
    /// Keys a command handed to a client go in after this step
    /// (`server_client_handle_key_after`).
    pub(crate) key_after: Option<crate::cmdq::StepId>,
    /// Running jobs and pending timers (job.c `all_jobs`).
    pub(crate) jobs: crate::jobs::Jobs,
    /// Panes that still had output when their read budget ran out. mio is
    /// edge-triggered, so they are read again on the next turn instead of
    /// waiting for an event that will not come.
    backlog: std::collections::BTreeSet<PaneId>,
    /// Windows where a cursor moved since the last frames, whose views are
    /// worked out again before the next ([`Self::update_moved_offsets`]).
    pub(crate) offset_due: std::collections::BTreeSet<WindowId>,
    /// Pane output some control client has not been sent yet.
    pub(crate) control_streams: crate::control_queue::Streams,
    /// Panes whose output this turn started a burst (they were not in the
    /// backlog): a key's echo or a prompt, not more of a flood. A frame
    /// that shows one is not paced ([`crate::render::FRAME_INTERVAL`]).
    burst_started: std::collections::BTreeSet<PaneId>,
    /// Panes that fired `pane-activity` this turn (`PANE_ACTIVITY`, cleared
    /// in `server_client_loop`, server-client.c:1940).
    activity_fired: std::collections::BTreeSet<PaneId>,
    /// Panes whose copy or view mode drew to the terminal this turn without
    /// a pane redraw ([`Self::with_mode`]): like a burst, the frame that
    /// shows it is not paced, and the program's synchronized output stays.
    pub(crate) mode_drawn: std::collections::BTreeSet<PaneId>,
    /// Command clients whose commands are done, with their exit codes:
    /// told after this turn's frames, as `server_client_loop` runs
    /// `server_client_check_exit` for a client after the redraws of the
    /// clients before it (server-client.c:1920-1927), and a command
    /// client is newer than the attached clients it changes.
    pending_exits: Vec<(ClientId, i32)>,
    /// Windows a lost client was looking at. tmux never looks at their
    /// focus again until something changes them (`server_client_lost` runs
    /// no `window_update_focus`), so [`Server::update_focus_all`] skips them.
    focus_stale: std::collections::BTreeSet<WindowId>,
    /// `wait-for` channels.
    pub waits: WaitChannels,
    /// The file operation a parked batch waits for (file.c `client_file`).
    pub(crate) files: BTreeMap<ClientId, commands::file_io::FileOp>,
    /// Every key table (`key_tables`), the 3.8 defaults loaded at start.
    pub key_bindings: vtmux_core::keys::KeyBindings,
    /// Keys `send-keys -K` handed to a client, handled after the command
    /// (`server_client_handle_key_after`).
    pub(crate) injected_keys: Vec<(ClientId, vtmux_core::keys::Key)>,
    /// The running command's formats (`command`, and a hook's
    /// `state->formats`: `hook`, `hook_*`), merged into its format trees
    /// (`cmdq_merge_formats`).
    pub(crate) cmd_formats: Vec<(String, String)>,
    /// Event sinks that come and go (`wait-for -E`).
    pub(crate) event_waits: Vec<crate::events::EventWait>,
    /// Every prompt type's history (prompt-history.c).
    pub(crate) prompt_history: crate::prompt_history::History,
    /// Closed popups' processes not yet reaped.
    pub(crate) orphans: Vec<i32>,
    /// The `#()` jobs formats have asked for (format.c `format_jobs` and
    /// each client's `c->jobs`); see [`crate::format_jobs`].
    pub(crate) format_jobs: std::rc::Rc<std::cell::RefCell<crate::format_jobs::FormatJobs>>,
    /// `cmdq_get_client(item)` of the command running now: the client whose
    /// jobs its formats use.
    pub(crate) cmd_client: Option<ClientId>,
    /// The running command's mouse event (`state->event.m`).
    pub(crate) cmd_mouse: Option<crate::mouse::MouseEvent>,
    /// Where idle compression got to ([`crate::compress`]).
    pub(crate) compress: crate::compress::Rotation,
    /// The `tmux` shim first on every child's `PATH` (D5); `None` if it
    /// could not be made. Dropped (and removed) when the server goes.
    pub(crate) shim: Option<crate::shim::Shim>,
    /// Each session's `lock-after-time` timer (`s->lock_timer`), armed by
    /// its activity while it has a client ([`Server::arm_lock_timer`]).
    lock_timers: BTreeMap<SessionId, Instant>,
    /// The monitor hooks' last values and timers (`set-hook -B`).
    pub(crate) hook_monitors: crate::control_subs::HookMonitors,
    /// Who may connect (server-acl.c, `server-access`).
    pub(crate) acl: crate::acl::Acl,
    /// This image's executable, open since it started: a live upgrade
    /// passes it on, so a failed resume switches back to this very file
    /// (`sys::upgrade::own_exe`). `None` in a dry run and on macOS.
    pub(crate) exe: Option<std::os::fd::OwnedFd>,
    /// Descriptors a live upgrade's last resort could not attach to any
    /// pane or client (`resume.rs`, `serve_held`): held open so that no
    /// pane's process loses its terminal. While any are held the server
    /// does not exit for want of sessions; `kill-server` and SIGTERM
    /// still end it.
    pub(crate) held: Vec<std::os::fd::OwnedFd>,
    /// Held PTYs with output still to discard when their read budget ran
    /// out: the backlog for [`Self::held`].
    held_backlog: std::collections::BTreeSet<usize>,
    /// `vtmux-persist` checkpoints ([`persist`]).
    pub(crate) persist: persist::Persist,
    /// A live upgrade in its preflight ([`upgrade`]).
    pub(crate) upgrade: upgrade::Upgrade,
    /// The path the server was started as, carried across upgrades
    /// ([`ServerConfig::binary`]).
    pub(crate) start_binary: Option<PathBuf>,
}

/// tmux.c `checkshell`: absolute, not tmux itself, executable.
#[must_use]
pub fn check_shell(shell: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = shell.as_os_str().as_bytes();
    if bytes.first() != Some(&b'/') {
        return false;
    }
    let base = shell
        .file_name()
        .map(std::os::unix::ffi::OsStrExt::as_bytes)
        .unwrap_or_default();
    if base == b"tmux" || base == b"vtmux" {
        return false;
    }
    rustix::fs::access(shell, rustix::fs::Access::EXEC_OK).is_ok()
}

/// tmux.c `getshell` (80-93): `$SHELL`, else the passwd shell, else
/// /bin/sh.
fn default_shell(env: &[(String, String)]) -> PathBuf {
    if let Some((_, s)) = env.iter().find(|(k, _)| k == "SHELL")
        && check_shell(Path::new(s))
    {
        return PathBuf::from(s);
    }
    if let Some(pw) = sys::user::current_user()
        && check_shell(&pw.shell)
    {
        return pw.shell;
    }
    PathBuf::from("/bin/sh")
}

use crate::model::now_epoch;

/// `find_home` (tmux.c:366-384).
pub(crate) fn find_home(env: &Environ) -> Option<PathBuf> {
    env.get("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| sys::user::current_user().map(|p| p.home))
}

/// The server's [`ConfigEnv`]: `$VAR` from the global environment, `~`
/// and `~user` through the passwd database (cmd-parse.y
/// `yylex_token_variable`, `yylex_token_tilde`), assignments into the
/// global environment, and `%if` conditions expanded over the model.
pub struct GlobalEnv<'a> {
    pub env: &'a mut Environ,
    model: &'a Model,
    info: &'a ServerInfo,
    /// `pi->c` and the state `cmd_find_from_client` finds for it
    /// (cmd-parse.y:206-212). `None` is a parse with no client
    /// (`cmd_parse_from_string(s, NULL)`): the best session.
    client: Option<(Origin, Option<ClientView>, std::rc::Rc<[ClientView]>)>,
}

impl<'a> GlobalEnv<'a> {
    /// A parse with no client (`cmd_parse_from_string(s, NULL)`).
    pub fn new(env: &'a mut Environ, model: &'a Model, info: &'a ServerInfo) -> Self {
        Self {
            env,
            model,
            info,
            client: None,
        }
    }

    /// A parse for `client` running from `origin` (`pi->c`), whose
    /// view and the attached clients come from [`Server::format_ctx`].
    #[must_use]
    pub fn for_client(
        mut self,
        origin: Origin,
        client: Option<ClientView>,
        clients: std::rc::Rc<[ClientView]>,
    ) -> Self {
        self.client = Some((origin, client, clients));
        self
    }
}

impl ConfigEnv for GlobalEnv<'_> {
    fn var(&self, name: &str) -> Option<String> {
        self.env.get(name).map(str::to_owned)
    }

    fn home(&self, user: Option<&str>) -> Option<String> {
        let path = match user {
            None => find_home(self.env)?,
            Some(u) => sys::user::user_named(u)?.home,
        };
        Some(path.to_string_lossy().into_owned())
    }

    fn set(&mut self, name: &str, value: &str, hidden: bool) {
        self.env.set(name, value, hidden);
    }

    /// `expanded` (cmd-parse.y:197-217): `cmd_find_from_client` for the
    /// parsing client, then `format_defaults` over that state.
    /// Expanded with `FORMAT_NOJOBS`: `#()` is empty.
    fn expand(&self, template: &str) -> String {
        let origin = self.client.as_ref().map(|c| c.0).unwrap_or_default();
        let found = crate::target::current(self.model, origin).unwrap_or_default();
        let mut ctx = Ctx::new(self.model, self.info, self.env, found);
        if let Some((_, client, clients)) = &self.client {
            ctx.client.clone_from(client);
            ctx.clients = std::rc::Rc::clone(clients);
        }
        vtmux_core::format::expand_str_nojobs(template, &ctx)
    }
}

/// The global environment a server started by `config`'s client begins
/// with: the client's, and its working directory as `PWD` (tmux.c:417-421).
/// Not the test hooks (`VTMUX_TEST_*`), which are for the server process
/// alone: a scenario that shows the environment can't see them.
fn global_environ(config: &ServerConfig) -> Environ {
    let mut env = Environ::from_pairs(
        config
            .environ
            .iter()
            .filter(|(k, _)| !k.starts_with("VTMUX_TEST_"))
            .cloned(),
    );
    if let Some(cwd) = &config.cwd {
        env.set("PWD", &cwd.to_string_lossy(), false);
    }
    env
}

impl Server {
    /// Take over a bound listening socket.
    pub fn new(listener: StdListener, socket: PathBuf, config: ServerConfig) -> io::Result<Self> {
        let poll = Poll::new()?;
        let mut listener = UnixListener::from_std(listener);
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;
        // proc_set_signals (proc.c:244-276): PIPE, TSTP, TTIN, TTOU and QUIT
        // are ignored; the rest are handled in `on_signals`.
        sys::signal::ignore_job_control();
        let mut signals = Signals::new(sys::signal::HANDLED)?;
        poll.registry()
            .register(&mut signals, SIGNALS, Interest::READABLE)?;
        let global_env = global_environ(&config);
        let defaults = Defaults::from_env(
            default_shell(&config.environ)
                .to_string_lossy()
                .into_owned(),
            global_env.get("VISUAL").map(str::to_owned),
            global_env.get("EDITOR").map(str::to_owned),
        );
        let mut model = Model::default();
        model.options = OptionsStore::new(&defaults);
        let mut info = ServerInfo::new(socket.to_string_lossy().into_owned());
        info.config_files = config
            .config_files
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join(",");
        model.format_base = Some(std::rc::Rc::new((info.clone(), global_env.clone())));
        let mut server = Self {
            model,
            info,
            global_env,
            themes: crate::render::window::ThemeCache::default(),
            shutdown: false,
            source_depths: BTreeMap::new(),
            focus_stale: std::collections::BTreeSet::new(),
            causes: Vec::new(),
            socket,
            poll,
            listener: Some(listener),
            signals,
            clients: BTreeMap::new(),
            next_client: 0,
            cmd_number: 0,
            config,
            config_loaded: false,
            cfg_finished: false,
            cfg_client: None,
            cfg_wait: None,
            cmdq: crate::cmdq::Cmdq::default(),
            key_after: None,
            jobs: crate::jobs::Jobs::default(),
            backlog: std::collections::BTreeSet::new(),
            offset_due: std::collections::BTreeSet::new(),
            control_streams: crate::control_queue::Streams::default(),
            burst_started: std::collections::BTreeSet::new(),
            activity_fired: std::collections::BTreeSet::new(),
            mode_drawn: std::collections::BTreeSet::new(),
            pending_exits: Vec::new(),
            waits: WaitChannels::default(),
            files: BTreeMap::new(),
            key_bindings: vtmux_core::keys::KeyBindings::with_defaults(),
            injected_keys: Vec::new(),
            cmd_formats: Vec::new(),
            event_waits: Vec::new(),
            prompt_history: crate::prompt_history::History::default(),
            orphans: Vec::new(),
            format_jobs: std::rc::Rc::default(),
            cmd_client: None,
            cmd_mouse: None,
            compress: crate::compress::Rotation::default(),
            shim: None,
            lock_timers: BTreeMap::new(),
            hook_monitors: crate::control_subs::HookMonitors::default(),
            acl: crate::acl::Acl::new(rustix::process::getuid().as_raw()),
            exe: None,
            held: Vec::new(),
            held_backlog: std::collections::BTreeSet::new(),
            persist: persist::Persist::default(),
            upgrade: upgrade::Upgrade::default(),
            start_binary: None,
        };
        server.start_binary.clone_from(&server.config.binary);
        // A dry run rebuilds the server only to see that it can: it must
        // not touch the shim the running server's panes use.
        if !server.config.dry_run {
            server.shim = crate::shim::own_exe()
                .and_then(|exe| crate::shim::Shim::create(&server.socket, &exe).ok());
            server.exe = sys::upgrade::own_exe();
        }
        // server_start: the hourly tidy (server.c:157-172, `evtimer_add`).
        server.format_jobs_start_tidy();
        server.update_width_cache();
        // The parser's aliases are this server's (the default option).
        vtmux_core::cmd::set_command_aliases(server.array_option(TreeId::Server, "command-alias"));
        Ok(server)
    }

    /// Run until the server exits.
    pub fn run(&mut self) -> io::Result<()> {
        let mut events = Events::with_capacity(256);
        let mut full_turn = Instant::now();
        loop {
            let timeout = self.next_timeout();
            match self.poll.poll(&mut events, timeout) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
            let idle = events.is_empty() && self.backlog.is_empty() && self.held_backlog.is_empty();
            // Woken only for a compression step: nothing else is due, so
            // the rest of the turn (a full render among it) would do
            // nothing a turn without compression would have done. The
            // periodic checks (window names) still get a turn every
            // NAME_INTERVAL.
            let now = Instant::now();
            if idle && !self.timers_due() && now < full_turn + NAME_INTERVAL {
                self.compress_turn(now, true);
                continue;
            }
            full_turn = now;
            self.dispatch(&events);
            for p in std::mem::take(&mut self.backlog) {
                self.pane_readable(p, true);
            }
            for i in std::mem::take(&mut self.held_backlog) {
                self.held_readable(i);
            }
            self.expire_repeat(Instant::now());
            self.start_timers(Instant::now());
            self.click_timers(Instant::now());
            self.status_timers(Instant::now());
            self.model.alerts_timers(Instant::now());
            self.run_alerts();
            let ids: Vec<_> = self.clients.keys().copied().collect();
            for id in ids {
                let _ = self.on_client_input(id, &[]);
            }
            self.jobs_timers(Instant::now());
            self.copy_refresh_timers(Instant::now());
            self.copy_drag_timers(Instant::now());
            self.clock_timers(Instant::now());
            self.panes_timers(Instant::now());
            self.model.scrollbar_timers(Instant::now());
            self.sync_timers(Instant::now());
            self.request_timers(Instant::now());
            self.subs_timers(Instant::now());
            self.lock_timer_fire(Instant::now());
            self.exit_timers(Instant::now());
            self.reattach_timers(Instant::now());
            self.upgrade_answer_due();
            self.persist_timers(Instant::now());
            // Events from outside commands (a pane exiting, a client
            // leaving, a rename, alerts) reach their sinks before the
            // queues run the hooks they add.
            self.close_dropped();
            self.drain_events();
            self.cmdq_process();
            self.check_window_names();
            self.check_window_resizes();
            self.drain_events();
            self.cmdq_process();
            // server_client_loop (server-client.c:1945-1949).
            self.send_theme_updates();
            self.apply_redraws();
            self.tree_updates();
            self.switch_resizes();
            self.tree_draws();
            self.render_and_exit();
            self.send_pane_resizes();
            // Full turns process timers/queues/rendering too; only the
            // early compression-only path above is a quiet turn.
            self.compress_turn(Instant::now(), false);
            self.control_buffers();
            self.pane_reads();
            self.control_tick();
            // A monitor hook set or removed this turn: its timer.
            self.hook_monitors_sync();
            // Last, once this turn's output is queued: a waiting upgrade.
            self.upgrade_turn(Instant::now());
            if self.wants_flush() {
                // server_loop / server_send_exit: release every waiter.
                for c in self.waits.flush() {
                    self.wake(c);
                }
                self.cmdq_process();
            }
            if self.shutdown {
                self.send_exit();
            }
            if self.should_exit() {
                self.exit_clients();
                return Ok(());
            }
        }
    }

    /// One poll's events, each to what owns its token.
    fn dispatch(&mut self, events: &Events) {
        for event in events {
            match event.token() {
                LISTENER => self.accept(),
                SIGNALS => self.on_signals(),
                Token(t) if (crate::popup::POPUP_BASE..CLIENT_BASE).contains(&t) => {
                    let id = ClientId(t - crate::popup::POPUP_BASE);
                    if event.is_writable() {
                        self.popup_writable(id);
                    }
                    if event.is_readable() || event.is_read_closed() {
                        self.popup_readable(id);
                    }
                }
                Token(t) if (crate::pipe::PIPE_BASE..crate::popup::POPUP_BASE).contains(&t) => {
                    let p = PaneId(u32::try_from(t - crate::pipe::PIPE_BASE).unwrap_or(u32::MAX));
                    self.pipe_event(p);
                }
                Token(t) if (crate::jobs::JOB_BASE..CLIENT_BASE).contains(&t) => {
                    let id = crate::jobs::JobId(t - crate::jobs::JOB_BASE);
                    self.job_writable(id);
                    self.job_readable(id);
                }
                Token(t) if t >= HELD_BASE => self.held_readable(t - HELD_BASE),
                Token(t) if t >= CLIENT_BASE => {
                    let id = ClientId(t - CLIENT_BASE);
                    if event.is_readable() || event.is_read_closed() {
                        self.client_readable(id);
                    }
                    if event.is_writable() {
                        self.flush_client(id);
                        self.control_pump(id);
                    }
                }
                Token(t) => {
                    let p = PaneId(u32::try_from(t - PANE_BASE).unwrap_or(u32::MAX));
                    if event.is_writable() {
                        self.pane_writable(p);
                    }
                    if event.is_readable() || event.is_read_closed() {
                        let flooding = self.backlog.contains(&p);
                        self.pane_readable(p, flooding);
                    }
                }
            }
        }
    }

    /// The poll timeout: the sooner of [`Self::timers_timeout`] and the
    /// next compression step.
    fn next_timeout(&self) -> Option<Duration> {
        let compress = self
            .compress_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()));
        [self.timers_timeout(), compress]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether anything but compression wants this turn.
    fn timers_due(&self) -> bool {
        self.timers_timeout().is_some_and(|t| t.is_zero())
    }

    /// The sooner of the loop's own needs and the next silence timer
    /// (`alerts_reset`'s `event_add`), and the next queued pane size.
    fn timers_timeout(&self) -> Option<Duration> {
        let own = self.own_timeout();
        let now = Instant::now();
        [
            own,
            self.model
                .alerts_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.pane_resize_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.jobs
                .deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.copy_refresh_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.copy_drag_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.clock_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.panes_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.model
                .scrollbar_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.sync_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.hidden_view_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.request_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.subs_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.lock_timers
                .values()
                .min()
                .map(|d| d.saturating_duration_since(now)),
            self.persist_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.upgrade_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.reattach_deadline()
                .map(|d| d.saturating_duration_since(now)),
            self.clients
                .values()
                .filter_map(|c| c.exit_deadline)
                .min()
                .map(|d| d.saturating_duration_since(now)),
            self.clients
                .values()
                .filter(|c| c.attached.is_some() && c.control.is_none() && !c.suspended)
                .filter_map(|c| c.compositor.due())
                .min()
                .map(|d| d.saturating_duration_since(now)),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn own_timeout(&self) -> Option<Duration> {
        if !self.backlog.is_empty()
            || !self.held_backlog.is_empty()
            || !self.pending_exits.is_empty()
            || self.cmdq.runnable()
            || !self.format_jobs.borrow().actions.is_empty()
        {
            return Some(Duration::ZERO);
        }

        if let Some(deadline) = self
            .clients
            .values()
            .flat_map(|c| {
                [
                    c.decoder.deadline(),
                    c.decoder.start_deadline(),
                    c.keys.repeat_deadline,
                    c.mouse.deadline(),
                    c.status.deadline(),
                ]
            })
            .flatten()
            .min()
        {
            return Some(deadline.saturating_duration_since(Instant::now()));
        }
        let pending = self.model.panes.values().any(|p| p.changed);
        if self.clients.values().any(|c| c.attached.is_some()) {
            Some(NAME_INTERVAL)
        } else {
            pending.then_some(NAME_INTERVAL)
        }
    }

    /// The server would exit but for its clients: `server_send_exit`, or
    /// `server_loop` with `exit-empty` on and no sessions, or none attached
    /// with `exit-unattached` (server.c:268-306).
    fn wants_flush(&self) -> bool {
        self.shutdown || (self.exit_empty() && self.nothing_attached())
    }

    /// `server_loop` (server.c:288-296): no sessions or, with
    /// `exit-unattached` on, sessions no client is attached to.
    fn nothing_attached(&self) -> bool {
        if self.model.sessions.is_empty() {
            return true;
        }
        matches!(
            self.model
                .options
                .get(TreeId::Server, "exit-unattached")
                .map(|e| &e.value),
            Some(OptionValue::Single(Value::Flag(true)))
        ) && self.clients.values().all(|c| c.attached.is_none())
    }

    fn exit_empty(&self) -> bool {
        // A client that left before saying Hello (`-CC` without a tty)
        // still counts: tmux exits once it has no sessions and no clients.
        self.held.is_empty()
            && (self.config_loaded || self.next_client > 0)
            && matches!(
                self.model
                    .options
                    .get(TreeId::Server, "exit-empty")
                    .map(|e| &e.value),
                Some(OptionValue::Single(Value::Flag(true)))
            )
    }

    /// `server_loop` (server.c:268-306): exit when told to, or when there
    /// are no sessions left (`exit-empty`) and no client is waiting; either
    /// way not while a job the server waits for runs (`job_still_running`).
    fn should_exit(&self) -> bool {
        if self.jobs.still_running() {
            return false;
        }
        if self.shutdown {
            return self.clients.values().all(|c| c.out.is_empty());
        }
        self.exit_empty()
            && self.nothing_attached()
            && self.clients.values().all(|c| c.closing && c.out.is_empty())
    }

    /// `server_send_exit` (server.c:314-333), at once when `kill-server`
    /// runs: every session goes (its panes are hung up) and every client
    /// that is not on its way out gets `MSG_SHUTDOWN`, a waiting one
    /// included. Only the process exit waits for a running job
    /// (`job_still_running`, server.c:306).
    fn send_exit(&mut self) {
        self.hangup_panes();
        let ids: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| !c.closing && c.control.is_none() && c.attached.is_none())
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            // A command that ended this turn (a refused `vtmux-upgrade`:
            // "the server is exiting") has its status waiting; it is not
            // overridden by the shutdown's 0, which a script would take
            // for success.
            let mut code = 0;
            self.pending_exits.retain(|&(waiting, status)| {
                if waiting == id {
                    code = status;
                }
                waiting != id
            });
            self.finish_client(id, code);
        }
    }

    fn hangup_panes(&mut self) {
        let panes: Vec<_> = self.model.panes.keys().copied().collect();
        for p in panes {
            self.hangup_pane(p);
        }
    }

    /// `server_exit = 1` (server.c:442-446): the server is going away. With
    /// no job to wait for (`job_still_running`) it leaves within this turn,
    /// so the listening socket and the shim go now, before any client is
    /// told (`kill-server`'s own is answered later in the turn): a client
    /// that starts after that is refused and starts a fresh server, instead
    /// of connecting to this one and losing it ("server exited
    /// unexpectedly"). With a job running the socket (and the shim, which
    /// the job's `tmux` needs) stays, as tmux's does until it exits: a
    /// client meanwhile is accepted and dropped (`server_accept`).
    pub(crate) fn begin_shutdown(&mut self) {
        // D27 decision 16: the checkpoint of a clean shutdown holds the
        // server as it was, before `send_exit` hangs up every pane.
        self.checkpoint_exit();
        self.shutdown = true;
        if !self.jobs.still_running() {
            self.close_listener();
        }
    }

    /// Stop listening, the shim first: once the socket refuses, a client
    /// may start a fresh server on the same path, and that server's
    /// `.<socket>.bin` must not go when this one finishes exiting.
    fn close_listener(&mut self) {
        self.shim = None;
        if let Some(mut l) = self.listener.take() {
            let _ = self.poll.registry().deregister(&mut l);
        }
    }

    fn exit_clients(&mut self) {
        // Close the socket before the slower teardown below.
        self.close_listener();
        // The last session gone (`exit-empty`): its checkpoint (D27
        // decision 16). After `kill-server` or SIGTERM it is written.
        self.checkpoint_exit();
        // server_exit -> job_kill_all (server.c:260), prompt_save_history.
        self.jobs_kill_all();
        if let Some(path) = self.history_file() {
            self.prompt_history.save(&path);
        }
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.flush_client(id);
        }
        self.hangup_panes();
        // tmux leaves the socket file: a later client gets ECONNREFUSED and
        // prints `no server running on` (client.c:291-295).
    }

    // ------------------------------------------------------------ clients

    fn accept(&mut self) {
        loop {
            let Some(listener) = self.listener.as_ref() else {
                return;
            };
            match listener.accept() {
                // server_accept (server.c): once exiting, a new client is
                // closed on (`server_exit`).
                Ok(_) if self.shutdown => {}
                Ok((stream, _)) => self.add_client(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// `server_client_create`: a new client on `stream`, read from now on.
    fn add_client(&mut self, mut stream: mio::net::UnixStream) {
        let id = ClientId(self.next_client);
        self.next_client += 1;
        let activity = self.model.touch();
        if self
            .poll
            .registry()
            .register(&mut stream, Token(CLIENT_BASE + id.0), Interest::READABLE)
            .is_ok()
        {
            self.clients.insert(id, Client::new(stream, activity));
        }
    }

    /// The client that started the server, connected before the fork
    /// (proc.c `proc_fork_and_daemon` makes a socketpair, server.c
    /// `server_start` adds its end as the first client): no other client
    /// comes before it, so one that connects and leaves first cannot make
    /// the fresh server exit for having no sessions and no clients.
    ///
    /// # Errors
    /// The stream can't be made non-blocking.
    pub fn adopt_starter(&mut self, stream: std::os::unix::net::UnixStream) -> io::Result<()> {
        stream.set_nonblocking(true)?;
        self.add_client(mio::net::UnixStream::from_std(stream));
        Ok(())
    }

    fn client_readable(&mut self, id: ClientId) {
        let mut closed = false;
        if let Some(c) = self.clients.get_mut(&id) {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match c.stream.read(&mut buf) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => c.inbuf.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
        }
        loop {
            let Some(c) = self.clients.get_mut(&id) else {
                return;
            };
            match protocol::decode::<ClientMsg>(&c.inbuf) {
                Ok((msg, used)) => {
                    c.inbuf.drain(..used);
                    self.on_message(id, msg);
                }
                Err(ProtocolError::Truncated) => break,
                Err(e) => {
                    // A Hello of another version that this build can't
                    // decode still gets the mismatch: the version comes
                    // first and is frozen (protocol module docs).
                    if c.hello.is_none()
                        && !c.closing
                        && let Some(client) = protocol::hello_version(&c.inbuf)
                        && client != protocol::PROTOCOL_VERSION
                    {
                        c.inbuf.clear();
                        let e = EdgeError::VersionMismatch {
                            client,
                            server: protocol::PROTOCOL_VERSION,
                        };
                        self.fail_client(id, &e);
                    } else if c.hello.is_some() && !c.closing && !c.dead && !closed {
                        // No more bytes complete a frame that doesn't
                        // decode: the client is dropped, as tmux drops a
                        // peer on a bad message (server-client.c
                        // `server_client_dispatch`, `bad:`), and told why.
                        c.inbuf.clear();
                        self.drop_bad_frame(id, &e);
                    } else {
                        closed = true;
                    }
                    break;
                }
            }
        }
        if closed && !self.reattach_closed(id) {
            self.drop_client(id);
        }
    }

    /// `server_client_lost`: the client's blocked commands never run
    /// (`cmd_wait_for_client_lost`, then `cmdq_next` drops the dead
    /// client's queue).
    fn drop_client(&mut self, id: ClientId) {
        match self.clients.get_mut(&id) {
            Some(c) if !c.dead => c.dead = true,
            _ => return,
        }
        // server_client_lost (server-client.c:508-514): client-detached for
        // an attached client, client-closed for a control or terminal one,
        // while the client is still there to name.
        if self.clients.get(&id).is_some_and(|c| c.attached.is_some())
            && let Some(w) = self.client_window(id)
        {
            self.focus_stale.insert(w);
        }
        if let Some(c) = self.clients.get(&id) {
            let terminal = c.hello.as_ref().is_some_and(|h| h.tty.is_some());
            let control = c.control.is_some();
            if c.was_attached {
                let ev = self.lost_client_event("client-detached", id);
                self.model.push_event(ev);
            }
            if control || terminal {
                let ev = self.lost_client_event("client-closed", id);
                self.model.push_event(ev);
            }
            self.drain_events();
        }
        // server-client.c:493-494: server_client_clear_overlay, then
        // status_prompt_clear, whose free callback continues a command
        // waiting on the prompt.
        self.popup_close(id);
        self.status_prompt_clear(id);
        if let Some(mut c) = self.clients.remove(&id) {
            let _ = self.poll.registry().deregister(&mut c.stream);
        }
        // server-client.c:548.
        self.format_jobs_lost_client(id);
        self.waits.client_lost(id);
        self.event_waits.retain(|w| w.client != id);
        self.cmdq_client_lost(id);
        self.source_depths.remove(&Some(id));
        // server-client.c:489-490.
        if self.cfg_client == Some(id) {
            self.cfg_client = None;
        }
        self.attached_lost(id);
        // server-client.c:527.
        self.cancel_requests(id);
        // server-client.c:564-565.
        self.recalculate_sizes();
        self.check_unattached();
        self.files.remove(&id);
    }

    /// The client's name (server-client.c:3016-3019): its tty, else
    /// `client-PID`.
    pub fn client_name(&self, id: ClientId) -> String {
        let Some(hello) = self.clients.get(&id).and_then(|c| c.hello.as_ref()) else {
            return String::new();
        };
        match &hello.tty {
            Some(t) if !t.name.is_empty() => t.name.clone(),
            _ => format!("client-{}", hello.pid),
        }
    }

    /// The `update-environment` patterns as seen from `tree`, in key order.
    fn update_environment(&self, tree: TreeId) -> Vec<String> {
        self.array_option(tree, "update-environment")
    }

    /// The items of an array option (or the one value of a plain option)
    /// as seen from `tree`, in key order.
    pub(crate) fn array_option(&self, tree: TreeId, name: &str) -> Vec<String> {
        match self.model.options.get(tree, name) {
            Some(e) => match &e.value {
                OptionValue::Array(items) => {
                    items.values().map(|v| v.to_text(e.def, false)).collect()
                }
                OptionValue::Single(v) => vec![v.to_text(e.def, false)],
            },
            None => Vec::new(),
        }
    }

    /// `environ_update(s->options, c->environ, s->environ)`: what attach-session
    /// (cmd-attach-session.c:135,159) and switch-client (cmd-switch-client.c:160)
    /// do before `server_client_set_session`.
    pub fn update_session_environ(&mut self, client: ClientId, session: SessionId) {
        let Some(hello) = self.clients.get(&client).and_then(|c| c.hello.as_ref()) else {
            return;
        };
        let src = Environ::from_pairs(hello.env.clone());
        let patterns = self.update_environment(TreeId::Session(session));
        if let Some(s) = self.model.sessions.get_mut(&session) {
            crate::environ::update(&patterns, &src, &mut s.env);
        }
    }

    /// `tty_set_selection` (tty.c:2149-2166): OSC 52 through the client
    /// terminal's `Ms`, whatever `set-clipboard` says, once its tty has
    /// started.
    /// `tty_clipboard_query` (tty.c:3206-3216): ask the terminal for its
    /// clipboard, once at a time, for a started terminal that has `Ms`.
    /// The reply is the next OSC 52 the client sends; none within
    /// `TTY_QUERY_TIMEOUT` and the query is forgotten.
    pub(crate) fn clipboard_query(&mut self, id: ClientId) {
        let now = Instant::now();
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        if c.clipboard_query.is_some_and(|until| until > now) {
            return;
        }
        let Some(bytes) = c
            .attached
            .and(c.caps.as_ref())
            .and(c.term.as_deref())
            .and_then(vtmux_core::term_features::Term::clipboard_query)
        else {
            return;
        };
        c.clipboard_query = Some(now + Duration::from_secs(5));
        self.send(id, &ServerMsg::Frame(bytes));
    }

    /// `server_client_set_flags` (server-client.c:3120-3159).
    pub(crate) fn set_client_flags(&mut self, id: ClientId, flags: &str) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let control = c.control.is_some();
        let no_output = c.flags.no_output;
        c.flags.set(control, flags);
        let reset = c.flags.no_output && !no_output;
        let wait_exit = c.flags.wait_exit;
        if reset {
            self.control_reset_panes(id);
        }
        // server_client_set_flags ends with MSG_FLAGS.
        self.send(id, &ServerMsg::Flags { wait_exit });
        // Flags can change which clients size a window (`ignore-size`).
        self.recalculate_sizes();
    }

    pub(crate) fn set_selection(&mut self, id: ClientId, data: &[u8]) {
        self.set_selection_clip(id, "", data);
    }

    /// [`Self::set_selection`] naming the clipboard (`tty_set_selection`'s
    /// `clip`).
    pub(crate) fn set_selection_clip(&mut self, id: ClientId, clip: &str, data: &[u8]) {
        let Some(bytes) = self
            .clients
            .get(&id)
            .filter(|c| c.attached.is_some() && c.caps.is_some())
            .and_then(|c| c.term.as_ref())
            .and_then(|t| t.set_selection(clip, data))
        else {
            return;
        };
        self.send(id, &ServerMsg::Frame(bytes));
    }

    /// Whether client `id`'s queue has work besides the step firing now
    /// (a control client's next stdin line waits for it).
    pub(crate) fn is_parked(&self, id: ClientId) -> bool {
        self.cmdq.busy(Some(id))
    }

    /// Queue a message; write what the socket takes now.
    pub(crate) fn send(&mut self, id: ClientId, msg: &ServerMsg) {
        let kind = if matches!(
            msg,
            ServerMsg::Exit { .. } | ServerMsg::Exec { .. } | ServerMsg::UpgradeResult { .. }
        ) {
            OutKind::Last
        } else {
            OutKind::Other
        };
        self.send_as(id, msg, kind);
    }

    /// [`Self::send`] with what the message is.
    pub(crate) fn send_as(&mut self, id: ClientId, msg: &ServerMsg, kind: OutKind) {
        let Some(c) = self.clients.get_mut(&id).filter(|c| !c.dead) else {
            return;
        };
        // Another version's control client could misread anything but the
        // frozen frames (`MSG_FLAGS`, `MSG_EXEC`), so it gets none of them.
        if c.speaks == Speaks::Frozen && !msg.is_frozen() {
            return;
        }
        // A suspended client's terminal is not tmux's to write
        // (TTY_STARTED is off: tty_client_ready, tty.c:1634), nor is that
        // of a client told to re-attach across an upgrade (its teardown
        // goes just before the notice).
        if (c.suspended || c.reattach.is_some_and(|r| r.deadline.is_some()))
            && matches!(msg, ServerMsg::Frame(_))
        {
            return;
        }
        // Large payloads are split so no frame exceeds MAX_FRAME.
        let frames: Vec<ServerMsg> = match msg {
            ServerMsg::Stdout(b) | ServerMsg::Stderr(b) | ServerMsg::Frame(b)
                if b.len() > protocol::MAX_FRAME / 2 =>
            {
                b.chunks(protocol::MAX_FRAME / 2)
                    .map(|chunk| match msg {
                        ServerMsg::Stdout(_) => ServerMsg::Stdout(chunk.to_vec()),
                        ServerMsg::Stderr(_) => ServerMsg::Stderr(chunk.to_vec()),
                        _ => ServerMsg::Frame(chunk.to_vec()),
                    })
                    .collect()
            }
            other => vec![other.clone()],
        };
        for f in &frames {
            if let Ok(bytes) = protocol::encode(f) {
                if matches!(f, ServerMsg::Frame(_)) {
                    c.unacked.sent(bytes.len());
                    c.written += bytes.len();
                }
                c.out.push(bytes, kind);
            }
        }
        self.flush_client(id);
    }

    /// `control_discard` (control.c:1040-1048) when a control client is
    /// told to exit: the pane output it has not been sent yet goes, so it
    /// gets the rest of its lines and the exit at once.
    pub(crate) fn discard_pane_output(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.out.discard(|k| k == OutKind::PaneOutput);
        }
    }

    pub(crate) fn flush_client(&mut self, id: ClientId) {
        let Some(c) = self.clients.get_mut(&id).filter(|c| !c.dead) else {
            return;
        };
        // Re-executing across an upgrade: its old connection is closed, and
        // the new one starts afresh.
        if c.reattach.is_some_and(|r| r.gone) {
            c.out = Outbox::default();
            if c.closing {
                self.drop_client(id);
            }
            return;
        }
        match c.out.write_to(&mut c.stream) {
            Ok(true) => c.last_write = Instant::now(),
            Ok(false) => {}
            Err(_) => {
                c.out = Outbox::default();
                c.closing = true;
            }
        }
        let interest = if c.out.is_empty() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        let _ = self
            .poll
            .registry()
            .reregister(&mut c.stream, Token(CLIENT_BASE + id.0), interest);
        if c.closing && c.out.is_empty() {
            self.drop_client(id);
        } else if c.closing {
            c.exit_deadline
                .get_or_insert_with(|| Instant::now() + EXIT_TIMEOUT);
        }
    }

    /// `server_client_exit_timer` and `server_client_check_exit(c, 1)`
    /// (server-client.c:2361-2440): a client still not gone
    /// [`EXIT_TIMEOUT`] after it was told to go has everything queued for
    /// it dropped but its exit message (`control_discard_all`), and is
    /// looked at again in another [`EXIT_TIMEOUT`]. One whose socket has
    /// taken nothing for [`EXIT_TIMEOUT`] is dropped: tmux's client reads
    /// its exit on a channel of its own and closes, but vtmux's reads it
    /// after the output its stalled terminal will not take
    /// (compat/DIVERGENCES.md D-029).
    fn exit_timers(&mut self, now: Instant) {
        let due: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.exit_deadline.is_some_and(|d| d <= now))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            let Some(c) = self.clients.get_mut(&id) else {
                continue;
            };
            c.out.discard(|k| k != OutKind::Last);
            c.exit_deadline = Some(now + EXIT_TIMEOUT);
            self.flush_client(id);
            let stalled = self.clients.get(&id).is_some_and(|c| {
                !c.dead && !c.out.is_empty() && now.duration_since(c.last_write) >= EXIT_TIMEOUT
            });
            if stalled {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.out = Outbox::default();
                }
                self.drop_client(id);
            }
        }
    }

    fn on_message(&mut self, id: ClientId, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello(hello) => self.on_hello(id, hello),
            ClientMsg::Input(bytes) => {
                if let Err(e) = self.on_client_input(id, &bytes) {
                    self.fail_client(id, &e);
                }
            }
            ClientMsg::Resize {
                cols,
                rows,
                xpixel,
                ypixel,
            } => {
                // server-client.c:2716-2722: tty_resize, then the rest.
                if !self.is_control(id) {
                    self.tty_resize(id, cols, rows, (xpixel, ypixel));
                }
                self.resize_client(id, cols, rows);
            }
            ClientMsg::Stdin(bytes) => self.control_stdin(id, &bytes),
            ClientMsg::StdinEof => self.control_stdin_eof(id),
            ClientMsg::Detach => self.detach_client(id, false),
            // server-client.c:2741-2742: both end CLIENT_SUSPENDED.
            ClientMsg::Wakeup | ClientMsg::Unlock => self.wakeup_client(id),
            ClientMsg::ReadData(bytes) => self.file_read_data(id, &bytes),
            ClientMsg::FileDone { error } => self.file_done(id, error),
            ClientMsg::FrameAck { seq } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.unacked.ack(seq);
                }
            }
            ClientMsg::UpgradeRequest { binary } => self.upgrade_request(id, &binary),
        }
    }

    /// A frame from client `id` that doesn't decode: it is sent `Exit 1`
    /// (frozen, so a client of any build reads it) with the error as its
    /// reason, after the teardown of an attached client's terminal.
    fn drop_bad_frame(&mut self, id: ClientId, e: &ProtocolError) {
        let message = Some(e.to_string());
        let terminal = self
            .clients
            .get(&id)
            .is_some_and(|c| c.attached.is_some() && c.control.is_none());
        if terminal {
            self.exit_attached(id, 1, message, false);
        } else {
            self.finish_client_with(id, 1, message);
        }
    }

    fn fail_client(&mut self, id: ClientId, e: &EdgeError) {
        let mut line = e.to_bytes();
        line.push(b'\n');
        self.send(id, &ServerMsg::Stderr(line));
        self.finish_client(id, 1);
    }

    /// `CLIENT_EXIT` with `access not allowed` (server.c:402-404,
    /// server-acl.c `server_acl_update`): status 1 and the reason.
    fn refuse_client(&mut self, id: ClientId) {
        let message = Some("access not allowed".to_owned());
        if self
            .clients
            .get(&id)
            .is_some_and(|c| c.attached.is_some() && c.control.is_none())
        {
            self.exit_attached(id, 1, message, false);
            return;
        }
        self.send(
            id,
            &ServerMsg::Exit {
                code: 1,
                message,
                hup: false,
            },
        );
        if let Some(c) = self.clients.get_mut(&id) {
            c.closing = true;
        }
        self.flush_client(id);
    }

    /// `server_acl_update` (server-acl.c:96-113): after an entry is
    /// removed or its access changed, a client with no entry leaves and
    /// the others are read-only or not as their entries say.
    pub(crate) fn acl_update(&mut self) {
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for id in ids {
            let Some(c) = self.clients.get_mut(&id).filter(|c| !c.closing && !c.dead) else {
                continue;
            };
            match self.acl.check(c.peer) {
                crate::acl::Access::Denied => self.refuse_client(id),
                crate::acl::Access::ReadOnly => c.flags.read_only = true,
                crate::acl::Access::Write => c.flags.read_only = false,
            }
        }
    }

    pub(crate) fn finish_client(&mut self, id: ClientId, code: i32) {
        self.finish_client_with(id, code, None);
    }

    /// [`Self::finish_client`] with a reason the client prints
    /// (`c->exit_message`).
    pub(crate) fn finish_client_with(&mut self, id: ClientId, code: i32, message: Option<String>) {
        self.send(
            id,
            &ServerMsg::Exit {
                code,
                message,
                hup: false,
            },
        );
        if let Some(c) = self.clients.get_mut(&id) {
            c.closing = true;
        }
        self.flush_client(id);
    }

    fn on_hello(&mut self, id: ClientId, mut hello: Hello) {
        if let Some(tty) = hello.tty.as_mut() {
            (tty.cols, tty.rows) = tty_size(tty.cols, tty.rows);
        }
        let speaks = match hello.check_version() {
            Ok(speaks) => speaks,
            Err(e) => {
                self.fail_client(id, &e);
                return;
            }
        };
        if let Some(c) = self.clients.get_mut(&id) {
            c.speaks = speaks;
        }
        // server_accept (server.c:401-405): a client the access list does
        // not let in leaves with nothing run.
        let peer = self.clients.get(&id).and_then(|c| c.peer);
        match self.acl.check(peer) {
            crate::acl::Access::Denied => {
                self.refuse_client(id);
                return;
            }
            crate::acl::Access::ReadOnly => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.flags.read_only = true;
                }
            }
            crate::acl::Access::Write => {}
        }
        // A client re-executed by an upgrade, back with its token
        // (live-upgrade spec, decision 21): no command, no hook.
        if let Some(token) = hello.ext.reattach.clone() {
            self.reattach(id, &hello, &token);
            return;
        }
        let origin = Origin {
            attached: None,
            inside_pane: hello
                .env
                .iter()
                .find(|(k, _)| k == "TMUX_PANE")
                .and_then(|(_, v)| v.parse().ok()),
            current: None,
            mouse: None,
        };
        let mut item = Item::new(Some(id), hello.cwd.clone());
        item.env.clone_from(&hello.env);
        item.tty.clone_from(&hello.tty);
        item.utf8 = hello.flags.utf8;
        item.origin = origin;
        let argv = hello.argv.clone();
        let hello_flags = hello.flags;
        let control = hello_flags.control != vtmux_core::protocol::ControlMode::None;
        let terminal = hello.tty.is_some();
        if let Some(c) = self.clients.get_mut(&id) {
            c.environ = hello.env.clone().into();
            // server_client_dispatch_identify -> tty_resize (server-client.c:
            // 3035): the pixel size from the terminal; no query yet, the
            // terminal is not open (`tty->out == NULL`).
            if let Some(tty) = &hello.tty {
                c.cell_pixels =
                    crate::term_queries::cell_pixels(tty.cols, tty.rows, hello.ext.pixels);
            }
            c.hello = Some(hello);
        }
        // `control_start` (server-client.c:3029, control.c:1025-1027): a
        // `-CC` client's stream opens with the DCS that tells the terminal
        // it is in control mode, before anything the hooks or the config
        // write. The client prints the matching `ST` when it exits.
        if hello_flags.control == vtmux_core::protocol::ControlMode::ControlControl {
            self.send(id, &ServerMsg::Stdout(b"\x1bP1000p".to_vec()));
        }
        // server-client.c:3042-3043.
        if control || terminal {
            self.fire_client("client-created", id);
            self.drain_events();
        }
        // The first client loads the configuration before its own command
        // runs (server-client.c:3052-3060, cfg.c start_cfg).
        if !self.config_loaded {
            self.start_cfg(id, origin);
        }
        if control {
            // The argv command runs with guard flags 0 (cmd-queue.c:594).
            self.control_start(id);
            item.control = Some(0);
        }
        // Config causes wait for an attached session (cfg.c
        // cfg_show_causes): a command client never sees them.
        let end = if control {
            crate::cmdq::End::ControlArgv
        } else {
            crate::cmdq::End::Client
        };
        // No command: default-client-command, read when it runs
        // (server-client.c:2868-2870, server_client_default_command).
        if argv.is_empty() {
            let mut batch = crate::cmdq::NewBatch::new(item, Vec::new(), end);
            batch.before.push(crate::cmdq::Work::DefaultCommand);
            self.cmdq_append(Some(id), batch);
            self.cmdq_process();
            return;
        }
        let calls = match cmd::parse_argv_bytes(&argv) {
            Ok(calls) => calls,
            Err(e) => {
                item.error(self, cmd_error_bytes(&e));
                Vec::new()
            }
        };
        let calls = self.read_only_gate(id, &mut item, calls);
        self.cmdq_append(Some(id), crate::cmdq::NewBatch::new(item, calls, end));
        self.cmdq_process();
    }

    /// A read-only client's commands are refused unless every one is
    /// `CMD_READONLY` (`server_client_read_only`, server-client.c:2797-2800,
    /// 2884-2887; key-bindings.c:752-755, 766-769): the list becomes one
    /// error, `client is read-only`, on `item`. A control client's stdin
    /// lines are not checked (control.c:695-727).
    pub(crate) fn read_only_gate(
        &mut self,
        id: ClientId,
        item: &mut Item,
        calls: Vec<cmd::CommandCall>,
    ) -> Vec<cmd::CommandCall> {
        let refused = self.clients.get(&id).is_some_and(|c| c.flags.read_only)
            && !calls.iter().all(|c| c.entry.read_only());
        if !refused {
            return calls;
        }
        item.error(self, b"client is read-only".to_vec());
        Vec::new()
    }

    pub(crate) fn send_output(&mut self, id: ClientId, item: &mut Item) {
        for out in std::mem::take(&mut item.output) {
            let msg = match out.into_bytes(item.utf8) {
                (b, false) => ServerMsg::Stdout(b),
                (b, true) => ServerMsg::Stderr(b),
            };
            self.send(id, &msg);
        }
    }

    pub(crate) fn deliver(&mut self, id: ClientId, mut item: Item) {
        self.send_output(id, &mut item);
        if item.after == After::Exit || self.shutdown {
            self.pending_exits.push((id, item.status));
        }
    }

    // ------------------------------------------------------- attach hooks

    /// `tty_update_features` (tty.c:552-575) once the client's terminal
    /// changed (a reply to the startup queries, or the start timer):
    /// the capabilities are taken again, `Eneks` asks for extended keys
    /// when `extended-keys` is on, `Enfcs` for focus reports when
    /// `focus-events` is. tmux then redraws the client in full; vtmux's
    /// frames are diffs of finished screens (D14), so it redraws only when
    /// the capabilities it draws with changed.
    pub(crate) fn update_features(&mut self, id: ClientId) {
        let extended_keys = self.model.extended_keys() != vtmux_vt::ExtendedKeys::Off;
        let focus = self.model.number_option(TreeId::Server, "focus-events") != 0;
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let Some(term) = c.term.as_ref() else {
            return;
        };
        let mut out = Vec::new();
        if extended_keys && let Some(eneks) = term.enable_extended_keys() {
            out.extend_from_slice(eneks.as_bytes());
        }
        // `Enfcs` comes with the `focus` feature, which every terminal
        // vtmux knows has.
        if focus {
            out.extend_from_slice(b"\x1b[?1004h");
        }
        let caps = crate::render::Caps::from_term(term);
        // tty_apply_features (tty-features.c:562-563).
        if term
            .features()
            .has(vtmux_core::term_features::Feature::Utf8)
            && let Some(h) = c.hello.as_mut()
        {
            h.flags.utf8 = true;
        }
        let caps = crate::render::Caps {
            light_theme: c.reported.light(),
            ..caps
        };
        let changed = c.caps != Some(caps);
        c.caps = Some(caps);
        if !out.is_empty() {
            self.send(id, &ServerMsg::Frame(out));
        }
        if changed && let Some(c) = self.clients.get_mut(&id) {
            c.compositor.invalidate();
        }
        // tty_update_features ends in server_redraw_client whether or not
        // the capabilities changed: an open tree mode is built again
        // (server_client_check_modes), so its list and preview catch up.
        self.redraw_client(id);
    }

    /// `tty_start_timer_callback` (tty.c:311-322): `TTY_QUERY_TIMEOUT`
    /// after the queries, a terminal that answered none of DA, DA2 and
    /// XTVERSION gets `tty_update_features` anyway; later replies are not
    /// taken as answers.
    fn start_timers(&mut self, now: Instant) {
        let due: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.decoder.start_deadline().is_some_and(|t| t <= now))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            let Some(c) = self.clients.get_mut(&id) else {
                continue;
            };
            if c.decoder.queries_expired() && c.attached.is_some() {
                self.update_features(id);
            }
        }
    }

    /// `server_client_open`'s `tty_open` for a terminal client, without
    /// keeping the terminal: the error attaching it would give. tmux opens
    /// the terminal before `new-session` makes the session
    /// (cmd-new-session.c:207-213), so a client that cannot attach leaves
    /// no session behind.
    pub(crate) fn check_terminal(&self, id: ClientId) -> Result<(), EdgeError> {
        let features = self.array_option(TreeId::Server, "terminal-features");
        let overrides = self.array_option(TreeId::Server, "terminal-overrides");
        match self
            .clients
            .get(&id)
            .and_then(|c| c.hello.as_ref())
            .and_then(|h| crate::render::Caps::open_term(h, &features, &overrides))
        {
            Some(Err(e)) => Err(open_error(&e)),
            _ => Ok(()),
        }
    }

    /// `cmd_attach_session`: Ready precedes encoded terminal output.
    pub fn attach_client(&mut self, id: ClientId, s: SessionId) -> Result<(), EdgeError> {
        self.model.current_of(s).ok_or(EdgeError::NoCurrentTarget)?;
        // tty_open -> tty_term_create reads the options now, so a later
        // `set terminal-features` does not reach this client.
        let features = self.array_option(TreeId::Server, "terminal-features");
        let overrides = self.array_option(TreeId::Server, "terminal-overrides");
        let client = self
            .clients
            .get_mut(&id)
            .ok_or(EdgeError::NoCurrentClient)?;
        let (term, caps) = client
            .hello
            .as_ref()
            .and_then(|h| crate::render::Caps::open_term(h, &features, &overrides))
            .ok_or_else(|| EdgeError::OpenTerminalFailed("not a terminal".into()))?
            .map_err(|e| open_error(&e))?;
        client.decoder.set_terminal(&term);
        client.caps = Some(caps);
        client.term_options = (features, overrides);
        // server-client.c:356: server_client_update_theme_colours.
        client.update_theme();
        // tty_apply_features (tty-features.c:562-563): the utf8 feature
        // marks the client UTF-8 (`-T utf8` with a C locale).
        if term
            .features()
            .has(vtmux_core::term_features::Feature::Utf8)
            && let Some(h) = client.hello.as_mut()
        {
            h.flags.utf8 = true;
        }
        let mut term = term;
        term.set_utf8(client.hello.as_ref().is_some_and(|h| h.flags.utf8));
        // REDRAW_ISOLATES reads CLIENT_UTF8, known only now.
        if let Some(caps) = client.caps.as_mut() {
            caps.isolates = crate::render::Caps::from_term(&term).isolates;
        }
        client.term = Some(std::rc::Rc::new(term));
        client.compositor.invalidate();
        client.attached = Some(s);
        // Caps::startup ends with the terminal queries (tty.c
        // tty_send_requests); tty_start_start_timer.
        client.decoder.query_sent(Instant::now());
        client.reported.palette_sent();
        // server_client_set_session: the current window's latest client
        // (server-client.c:465), then session_update_activity (468) and
        // session_theme_changed (469).
        self.set_latest(s, id);
        self.model.session_attached_now(s);
        self.session_theme_changed(Some(s));
        self.arm_lock_timer(s);
        self.send(id, &ServerMsg::Ready);
        // tty.c tty_start_tty / tty_stop_tty; reference PTY probe pins the
        // set of modes, not their ordering. Focus reports and extended
        // keys are asked for once the terminal answers or the start timer
        // fires ([`Server::update_features`]).
        let rows = self
            .clients
            .get(&id)
            .and_then(|c| c.hello.as_ref()?.tty.as_ref())
            .map_or(24, |t| t.rows);
        let clear = self.clear_on_attach();
        let bytes = self.clients[&id]
            .term
            .as_deref()
            .map(|term| crate::render::startup(term, clear, rows));
        if let Some(bytes) = bytes {
            self.send(id, &ServerMsg::Frame(bytes));
        }
        if let Some(tty) = self
            .clients
            .get(&id)
            .and_then(|c| c.hello.as_ref())
            .and_then(|h| h.tty.clone())
        {
            self.resize_client(id, tty.cols, tty.rows);
        }
        self.update_focus(self.model.current_of(s).map(|(_, w, _)| w));
        self.session_attached_alerts(s);
        // server_client_set_session (server-client.c:473).
        self.update_view(id);
        // server_client_set_session: status_timer_start,
        // client-session-changed, then server_redraw_client.
        self.status_timer_start(id);
        self.fire_session_changed(id, None);
        self.redraw_client(id);
        Ok(())
    }

    /// The sessions with a client (`s->attached != 0`), for
    /// `CMD_FIND_PREFER_UNATTACHED`.
    pub(crate) fn attached_sessions(&self) -> std::collections::BTreeSet<SessionId> {
        self.clients
            .values()
            .filter(|c| !c.suspended)
            .filter_map(|c| c.attached)
            .collect()
    }

    /// `server_client_set_session` (server-client.c:471-472): the current
    /// window's winlink loses its alert flags and the rest are checked
    /// again now there is a client.
    pub(crate) fn session_attached_alerts(&mut self, s: SessionId) {
        if let Some(session) = self.model.sessions.get_mut(&s) {
            let current = session.current;
            session.link_alerts.remove(&current);
        }
        let attached = self.attached_sessions();
        let alerts = self.model.alerts_check_session(s, &attached);
        self.handle_alerts(&alerts);
    }

    /// `alerts_callback`: check the windows queued since the last turn.
    fn run_alerts(&mut self) {
        let attached = self.attached_sessions();
        let alerts = self.model.alerts_run(&attached);
        self.handle_alerts(&alerts);
    }

    /// The part of the alert checks that reaches clients: the
    /// `alert-*` hooks and `alerts_set_message`.
    fn handle_alerts(&mut self, alerts: &[alerts::Alert]) {
        for a in alerts {
            // alerts.c:208,244,280.
            self.model
                .fire_winlink(a.kind.hook(), a.session, a.index, a.window);
            if a.message {
                self.alerts_set_message(a);
            }
        }
    }

    /// `alerts_set_message` (alerts.c:296-326): each terminal client of
    /// the session gets a bell, a message or both, by `visual-*`.
    fn alerts_set_message(&mut self, a: &alerts::Alert) {
        const OFF: i64 = 0;
        const BOTH: i64 = 2;
        let visual = self
            .model
            .number_option(TreeId::Session(a.session), a.kind.visual());
        let current = self
            .model
            .sessions
            .get(&a.session)
            .is_some_and(|s| s.current == a.index);
        let text = if current {
            format!("{} in current window", a.kind.title())
        } else {
            format!("{} in window {}", a.kind.title(), a.index)
        };
        let ids: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| c.attached == Some(a.session) && c.control.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if visual == OFF || visual == BOTH {
                // tty_putcode(TTYC_BEL).
                self.send(id, &ServerMsg::Frame(vec![7]));
            }
            if visual == OFF {
                continue;
            }
            self.status_message_set(id, text.clone());
        }
    }

    /// `MSG_RESIZE` (server-client.c:2712-2730): the client becomes the
    /// latest of the window it looks at, its terminal takes its new size
    /// and every window is fitted again (`recalculate_sizes`); the loop's
    /// check pass applies it.
    pub(crate) fn resize_client(&mut self, id: ClientId, cols: u32, rows: u32) {
        self.refit_client(id, cols, rows, true);
    }

    /// [`Self::resize_client`]; `fire` false for a client fitted again
    /// without a `MSG_RESIZE` (a switch of session, which fires no
    /// client-resized: server-client.c `server_client_set_session`).
    pub(crate) fn refit_client(&mut self, id: ClientId, cols: u32, rows: u32, fire: bool) {
        if self.is_control(id) {
            return;
        }
        self.update_latest(id);
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        let mut old = None;
        if let Some(tty) = client.hello.as_mut().and_then(|h| h.tty.as_mut()) {
            old = Some((tty.cols, tty.rows));
            (tty.cols, tty.rows) = tty_size(cols, rows);
        }
        // server-client.c:2730-2731: client-resized once it has a session.
        if let Some((ox, oy)) = old
            && fire
            && client.attached.is_some()
            && client.was_attached
        {
            let (nx, ny) = tty_size(cols, rows);
            let ev = self
                .client_event("client-resized", id)
                .int("width", nx)
                .int("height", ny)
                .int("old_width", ox)
                .int("old_height", oy);
            self.model.push_event(ev);
        }
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        client.compositor.invalidate();
        // tty_resize -> tty_invalidate: the terminal modes are written again.
        client.mouse.invalidate();
        // server-client.c:2729: server_redraw_client.
        client.status.request_full_redraw();
        self.recalculate_sizes();
        self.popup_resize(id);
        // server-client.c:2729: server_redraw_client, the popup in full.
        self.redraw_client(id);
    }

    /// `status_line_size` (status.c:113-122) with `CLIENT_STATUSOFF`
    /// (resize.c:457-460): the client's session's status lines, none for a
    /// control client or a terminal no taller than them.
    pub(crate) fn status_line_size(&self, c: &Client) -> u32 {
        if c.control.is_some() {
            return 0;
        }
        let tree = c.attached.map_or(TreeId::GlobalSession, TreeId::Session);
        let lines = self.model.status_lines(tree);
        match c.hello.as_ref().and_then(|h| h.tty.as_ref()) {
            Some(tty) if tty.rows <= lines => 0,
            _ => lines,
        }
    }

    /// `options_push_changes` (options.c:1409-1475) for the drawing and
    /// sizing part: every window is fitted to its clients again
    /// (`recalculate_sizes`) and every attached client redrawn
    /// (`server_redraw_client`).
    pub fn push_option_changes(&mut self) {
        self.recalculate_sizes();
        for c in self.clients.values_mut() {
            if c.attached.is_some() {
                c.compositor.invalidate();
                c.status.request_full_redraw();
            }
        }
    }

    /// Every attached client (a session set), in connection order.
    pub fn attached_clients(&self) -> Vec<ClientId> {
        self.clients
            .iter()
            .filter_map(|(&id, c)| c.attached.map(|_| id))
            .collect()
    }

    /// Clients attached to a session, in connection order.
    pub fn session_clients(&self, session: SessionId) -> Vec<ClientId> {
        self.clients
            .iter()
            .filter_map(|(&id, c)| (c.attached == Some(session)).then_some(id))
            .collect()
    }

    /// `server_client_detach` (server-client.c:2733-2745): `hup` is
    /// `MSG_DETACHKILL`, which has the client hang up its parent.
    pub fn detach_client(&mut self, id: ClientId, hup: bool) {
        if self.is_control(id) {
            self.control_detach(id);
            return;
        }
        let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
            return;
        };
        let how = if hup {
            "detached and SIGHUP"
        } else {
            "detached"
        };
        let message = self
            .model
            .sessions
            .get(&s)
            .map(|s| format!("{how} (from session {})", s.name));
        self.exit_attached(id, 0, message, hup);
    }

    /// `server_client_exec` (server-client.c:630-655): the client runs
    /// `shell -c command` in place of itself, `shell` being the session's
    /// `default-shell` (the server's, with no session) unless it fails
    /// `checkshell`. An empty command does nothing.
    pub fn exec_client(&mut self, id: ClientId, command: &str) {
        if command.is_empty() {
            return;
        }
        let tree = self
            .clients
            .get(&id)
            .and_then(|c| c.attached)
            .map_or(TreeId::GlobalSession, TreeId::Session);
        let shell = self.default_shell(tree);
        if self.is_control(id) {
            self.control_exec(id, command.to_owned(), shell);
            return;
        }
        // MSG_EXEC takes the client out like MSG_DETACH: the terminal is
        // restored first, then the process is replaced.
        self.exit_attached_with(
            id,
            &ServerMsg::Exec {
                command: command.to_owned(),
                shell,
            },
        );
    }

    /// `server_client_suspend` (server-client.c:598-610): the terminal is
    /// restored (`tty_stop_tty`) and the client told to stop itself. A
    /// client without a session, on its way out or already suspended is
    /// left alone. vtmux sends nothing to a control client either: it has
    /// no terminal to give back.
    pub fn suspend_client(&mut self, id: ClientId) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        if c.attached.is_none() || c.suspended || c.closing || c.dead || c.control.is_some() {
            return;
        }
        self.send_teardown(id);
        if let Some(c) = self.clients.get_mut(&id) {
            c.suspended = true;
        }
        self.send(id, &ServerMsg::Suspend);
    }

    /// `server_lock_client` (server-fn.c:197-218): the terminal is
    /// restored, then the alternate screen entered and cleared for
    /// `lock-command`, which the client runs in the foreground before it
    /// answers `Unlock`. Control clients, suspended clients and an empty
    /// or oversized command are left alone.
    pub fn lock_client(&mut self, id: ClientId) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        if c.suspended || c.closing || c.dead || c.control.is_some() {
            return;
        }
        let Some(s) = c.attached else { return };
        let command = self
            .model
            .options
            .get(TreeId::Session(s), "lock-command")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        // MAX_IMSGSIZE - IMSG_HEADER_SIZE, with the NUL.
        if command.is_empty() || command.len() + 1 > 16384 - 16 {
            return;
        }
        self.send_teardown(id);
        // tty_raw: smcup, clear, E3.
        if let Some(term) = self.clients.get(&id).and_then(|c| c.term.as_ref()) {
            let bytes = crate::render::lock_screen(term);
            self.send(id, &ServerMsg::Frame(bytes));
        }
        if let Some(c) = self.clients.get_mut(&id) {
            c.suspended = true;
        }
        self.send(id, &ServerMsg::Lock { command });
    }

    /// `server_lock_session` (server-fn.c:185-194): every client on `s`.
    pub fn lock_session(&mut self, s: SessionId) {
        for id in self.session_clients(s) {
            self.lock_client(id);
        }
    }

    /// `server_lock` (server-fn.c:174-183): every client with a session.
    pub fn lock_server(&mut self) {
        for id in self.attached_clients() {
            self.lock_client(id);
        }
    }

    /// `session_update_activity`'s timer (session.c:262-272): disarmed,
    /// then armed for `lock-after-time` seconds if `s` has a client.
    pub(crate) fn arm_lock_timer(&mut self, s: SessionId) {
        self.lock_timers.remove(&s);
        if !self.attached_sessions().contains(&s) {
            return;
        }
        let secs = self
            .model
            .number_option(TreeId::Session(s), "lock-after-time");
        if let Ok(secs @ 1..) = u64::try_from(secs) {
            self.lock_timers
                .insert(s, Instant::now() + Duration::from_secs(secs));
        }
    }

    /// `session_lock_timer` (session.c:232-245): a session still with a
    /// client when its timer runs out is locked. The timer is not armed
    /// again until the session sees activity.
    fn lock_timer_fire(&mut self, now: Instant) {
        let due: Vec<SessionId> = self
            .lock_timers
            .iter()
            .filter(|&(_, &at)| at <= now)
            .map(|(&s, _)| s)
            .collect();
        if due.is_empty() {
            return;
        }
        let attached = self.attached_sessions();
        for s in due {
            self.lock_timers.remove(&s);
            if attached.contains(&s) {
                self.lock_session(s);
            }
        }
        self.recalculate_sizes();
    }

    /// `MSG_WAKEUP` and `MSG_UNLOCK` (server-client.c:2741-2763): the
    /// client is back (SIGCONT, or the lock command exited). The terminal
    /// is started again and redrawn in full.
    fn wakeup_client(&mut self, id: ClientId) {
        let stamp = self.model.touch();
        let Some(c) = self.clients.get_mut(&id).filter(|c| c.suspended) else {
            return;
        };
        c.suspended = false;
        let Some(s) = c.attached.filter(|_| !c.closing) else {
            return;
        };
        c.activity = stamp;
        c.activity_secs = now_epoch();
        c.compositor.invalidate();
        // tty_start_tty -> tty_invalidate: the modes are written again.
        c.mouse.invalidate();
        let term = c.term.clone();
        // tty_start_tty: the startup modes without the terminal queries,
        // which tty_send_requests sends only once.
        if let Some(term) = term {
            let rows = self.tty_size(id).map_or(24, |(_, r)| u32::from(r));
            let clear = self.clear_on_attach();
            self.send(
                id,
                &ServerMsg::Frame(crate::render::restart(&term, clear, rows)),
            );
        }
        self.redraw_client(id);
        self.recalculate_sizes();
        self.model.touch_session(s);
        self.arm_lock_timer(s);
    }

    /// `default-shell` of `tree`, or /bin/sh when `checkshell` fails.
    pub(crate) fn default_shell(&self, tree: TreeId) -> PathBuf {
        let configured = self.model.options.get(tree, "default-shell").map_or_else(
            || "/bin/sh".to_owned(),
            |e| e.value.to_text(e.def, None, false),
        );
        if check_shell(Path::new(&configured)) {
            PathBuf::from(configured)
        } else {
            PathBuf::from("/bin/sh")
        }
    }

    fn exit_attached(&mut self, id: ClientId, code: i32, message: Option<String>, hup: bool) {
        self.exit_attached_with(id, &ServerMsg::Exit { code, message, hup });
    }

    /// `tty_stop_tty`: the teardown escapes, unless the terminal is
    /// stopped already (suspended).
    fn send_teardown(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get(&id).filter(|c| !c.suspended)
            && let Some(term) = c.term.as_deref()
        {
            let bytes = crate::render::teardown(term, self.clear_on_attach());
            self.send(id, &ServerMsg::Frame(bytes));
        }
    }

    /// `clear-on-attach`: whether a client's terminal is started on the
    /// alternate screen (tty.c:357, 487, 512).
    fn clear_on_attach(&self) -> bool {
        self.model.number_option(TreeId::Server, "clear-on-attach") != 0
    }

    fn exit_attached_with(&mut self, id: ClientId, last: &ServerMsg) {
        // A suspended or locked client reads this once it is back. tmux
        // keeps it until MSG_EXITING, which comes after its MSG_WAKEUP or
        // MSG_UNLOCK; that starts the terminal again while the client
        // still has its session (server-client.c:2741-2763), so tty_close
        // writes the teardown, taking it off the lock screen. With its
        // session gone (kill-session) nothing is started or torn down.
        let session_alive = |server: &Self, c: &Client| {
            c.attached
                .is_some_and(|s| server.model.sessions.contains_key(&s))
        };
        if let Some(c) = self.clients.get(&id)
            && c.suspended
            && session_alive(self, c)
            && let Some(c) = self.clients.get_mut(&id)
        {
            c.suspended = false;
        }
        self.send_teardown(id);
        self.send(id, last);
        let left = self.client_window(id);
        if let Some(c) = self.clients.get_mut(&id) {
            c.left_session = c.attached.or(c.left_session);
            c.attached = None;
            c.closing = true;
        }
        // MSG_EXITING: server_client_set_session(c, NULL) looks at the
        // focus of the window the client left (server-client.c:462-463),
        // before server_client_lost fires client-detached.
        self.update_focus(left);
        // server_client_set_session(c, NULL) then recalculate_sizes
        // (server-client.c:2733-2738): the clients left size the windows.
        self.recalculate_sizes();
        self.flush_client(id);
    }

    /// The `escape-time` server option.
    fn escape_time(&self) -> Duration {
        let ms = self
            .model
            .number_option(vtmux_core::options::TreeId::Server, "escape-time");
        Duration::from_millis(u64::try_from(ms).unwrap_or(0))
    }

    /// Input from an attached terminal client: decode it into keys
    /// (tty-keys.c) and hand each to the key tables
    /// ([`Server::handle_key`], `server_client_key_callback`).
    pub fn on_client_input(&mut self, id: ClientId, bytes: &[u8]) -> Result<(), EdgeError> {
        // CLIENT_UNATTACHEDFLAGS (server-client.c:1460): a suspended
        // client's keys go nowhere.
        let Some(s) = self
            .clients
            .get(&id)
            .filter(|c| c.control.is_none() && !c.suspended)
            .and_then(|c| c.attached)
        else {
            return Ok(());
        };
        self.model.current_of(s).ok_or(EdgeError::NoCurrentTarget)?;
        let escape_time = self.escape_time();
        let c = self
            .clients
            .get_mut(&id)
            .ok_or(EdgeError::NoCurrentClient)?;
        c.decoder.escape_time = Some(escape_time);
        c.decoder.feed(bytes);
        let now = Instant::now();
        let events = c.decoder.drain_with_bytes(now);
        if !events.is_empty() {
            // The repeat timer fires before keys that arrive after it.
            self.expire_repeat(now);
        }
        for (event, raw) in events {
            let Some(c) = self.clients.get_mut(&id).filter(|c| c.attached.is_some()) else {
                break;
            };
            match event {
                crate::input::Event::Ignore => {}
                crate::input::Event::Mouse(m, bytes) => {
                    self.handle_mouse_input(id, m, bytes, false);
                }
                crate::input::Event::Clipboard(..)
                | crate::input::Event::PixelSize(..)
                | crate::input::Event::Colour(..)
                | crate::input::Event::Palette(..)
                | crate::input::Event::Theme(_) => self.client_report(id, event),
                crate::input::Event::Sync(sync) => {
                    // tty_keys_sync: only a supported reply changes anything,
                    // and tty_update_features redraws the client.
                    if let (true, Some(term)) = (sync, &mut c.term) {
                        std::rc::Rc::make_mut(term).sync_supported();
                        c.term_replies.push(snapshot::TermReply::Sync);
                        self.update_features(id);
                    }
                }
                crate::input::Event::DeviceAttributes(params) => {
                    if let Some(term) = &mut c.term {
                        std::rc::Rc::make_mut(term).device_attributes(&params);
                        c.term_replies.push(snapshot::TermReply::Da(params));
                    }
                    self.update_features(id);
                }
                crate::input::Event::DeviceAttributes2(first) => {
                    if let Some(term) = &mut c.term {
                        std::rc::Rc::make_mut(term).secondary_device_attributes(first);
                        c.term_replies.push(snapshot::TermReply::Da2(first));
                    }
                    self.update_features(id);
                }
                crate::input::Event::ExtendedDeviceAttributes(text) => {
                    if let Some(term) = &mut c.term {
                        std::rc::Rc::make_mut(term).extended_device_attributes(&text);
                        c.term_replies.push(snapshot::TermReply::Xtversion(text));
                    }
                    self.update_features(id);
                }
                crate::input::Event::Paste(start) => {
                    self.handle_key0(id, crate::key_dispatch::KeyKind::Paste(start), None);
                }
                crate::input::Event::Focus(focused) => {
                    // tty_keys_next (tty-keys.c:1030-1039): focus out is the
                    // pane's focus-out then client-focus-out, focus in is
                    // client-focus-in then the pane's focus-in; then the key
                    // callback's activity update. Focus events are not keys
                    // and reach no table (server-client.c:1523).
                    c.focused = focused;
                    if focused {
                        self.fire_client("client-focus-in", id);
                        self.update_focus(self.client_window(id));
                    } else {
                        self.update_focus(self.client_window(id));
                        self.fire_client("client-focus-out", id);
                    }
                    // server_client_handle_key0 with the focus key: a
                    // message or a prompt (`-k` names it) may take it, and
                    // then there is no key callback.
                    let key = if focused {
                        vtmux_core::keys::Key::FOCUS_IN
                    } else {
                        vtmux_core::keys::Key::FOCUS_OUT
                    };
                    let taken = self.clients.get_mut(&id).is_none_or(|c| c.status.key())
                        || self.popup_key(id, key, None)
                        || self.menu_key(id, key, None)
                        || self.prompts_key(id, key);
                    if !taken {
                        self.touch_client(id);
                        if focused {
                            self.update_latest(id);
                        }
                    }
                }
                crate::input::Event::Key(key) => {
                    // server_client_handle_key0 (server-client.c:1781-1880):
                    // a message and the prompt see the key at once; the
                    // key tables get it from the client's queue, after the
                    // commands earlier keys queued, and not while a
                    // command waits on a prompt.
                    self.handle_key0(id, crate::key_dispatch::KeyKind::Key(key, raw), None);
                }
            }
        }
        self.cmdq_process();
        Ok(())
    }

    /// `server_client_key_callback`'s activity update: the client's and the
    /// session's (server-client.c:1465-1469).
    pub(crate) fn touch_client(&mut self, id: ClientId) {
        let stamp = self.model.touch();
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        c.activity = stamp;
        c.activity_secs = now_epoch();
        c.keys.activity = Instant::now();
        if let Some(s) = c.attached {
            self.model.touch_session(s);
            self.arm_lock_timer(s);
        }
    }

    /// `session_set_current` and `window_set_active_pane` run
    /// `window_update_focus` for the windows they change when `focus-events`
    /// is on; looking at every window after a command finds the same changes.
    pub(crate) fn update_focus_all(&mut self) {
        if self.model.number_option(TreeId::Server, "focus-events") == 0 {
            return;
        }
        let windows: Vec<_> = self
            .model
            .windows
            .keys()
            .filter(|w| !self.focus_stale.contains(w))
            .copied()
            .collect();
        for w in windows {
            self.update_focus(Some(w));
        }
    }

    /// The window a client is looking at.
    pub(crate) fn client_window(&self, id: ClientId) -> Option<WindowId> {
        let s = self.clients.get(&id)?.attached?;
        Some(self.model.current_of(s)?.1)
    }

    /// `window_update_focus` (window.c:707-713): the window's active pane.
    pub(crate) fn update_focus(&mut self, w: Option<WindowId>) {
        let Some(p) = w
            .and_then(|w| self.model.windows.get(&w))
            .map(Window::active)
        else {
            return;
        };
        if let Some(ev) = self.pane_focus(p) {
            self.model.push_event(ev);
        }
    }

    /// `window_pane_update_focus` (window.c:716-752): a pane is focused
    /// when it is its window's active pane and a focused client looks at
    /// the window. A change is reported to a pane that asked for it
    /// (`?1004h`) and is the pane-focus-in/out event returned.
    pub(crate) fn pane_focus(&mut self, p: PaneId) -> Option<crate::events::Event> {
        let pane = self.model.panes.get(&p)?;
        if pane.exited {
            return None;
        }
        let w = pane.window;
        self.focus_stale.remove(&w);
        let window = self.model.windows.get(&w);
        let active = window.map(Window::active) == Some(p);
        // window.c:731: not while the window has a menu up.
        let focused = active
            && window.is_some_and(|x| x.menu.is_none())
            && self.clients.values().any(|c| {
                c.focused
                    && c.popup.is_none()
                    && c.attached
                        .and_then(|s| self.model.current_of(s))
                        .is_some_and(|(_, cw, _)| cw == w)
            });
        let pane = self.model.panes.get_mut(&p)?;
        if focused == pane.focused {
            return None;
        }
        pane.focused = focused;
        if pane.term.focus_events() {
            if let Some(io) = pane.io.as_mut() {
                io.write(if focused { b"\x1b[I" } else { b"\x1b[O" });
            }
            self.pane_output_queued(p);
        }
        // window.c:741-747.
        let name = if focused {
            "pane-focus-in"
        } else {
            "pane-focus-out"
        };
        let ev = crate::events::Event::new(name)
            .target(crate::target::Found::from_pane(&self.model, p))
            .pane("pane", p)
            .window(&self.model, "window", w);
        Some(ev)
    }

    /// The pane a client's keys go to (`cmd_find_from_client`).
    pub(crate) fn client_pane(&self, id: ClientId) -> Option<PaneId> {
        let s = self.clients.get(&id)?.attached?;
        Some(self.model.current_of(s)?.2)
    }

    /// `window_pane_key`: the key from client `c` goes to the pane's shown
    /// mode if it is in one, else encoded to the program, and with
    /// `synchronize-panes` to the window's other panes
    /// (`window_pane_copy_key`, each encoded for its own screen).
    pub(crate) fn forward_key(&mut self, p: PaneId, c: Option<ClientId>, key: &crate::keys::Key) {
        let Some(pane) = self.model.panes.get(&p) else {
            return;
        };
        // A pane in a mode gives the key to the mode, and copy mode takes
        // keys only through its table (no `key` callback).
        if !pane.modes.is_empty() {
            self.mode_key(p, c, key.to_core(), true);
            return;
        }
        let Some(modes) = self.model.key_modes(p) else {
            return;
        };
        let bytes = crate::keys::encode(key, modes);
        self.write_pane(p, &bytes);
        self.copy_to_synchronized(p, |server, other| {
            server
                .model
                .key_modes(other)
                .map(|m| crate::keys::encode(key, m))
                .unwrap_or_default()
        });
    }

    /// `window_pane_paste` (window.c:2053-2069): bytes of a bracketed
    /// paste (a key or a mouse report, as the terminal sent them) to pane
    /// `p` and, with `synchronize-panes`, to its peers as they are. A pane
    /// in a mode gets nothing.
    pub(crate) fn paste_to_pane(&mut self, p: PaneId, bytes: &[u8]) {
        if self.model.panes.get(&p).is_none_or(|x| !x.modes.is_empty()) {
            return;
        }
        self.write_pane(p, bytes);
        self.copy_to_synchronized(p, |_, _| bytes.to_vec());
    }

    /// `window_pane_copy_key` / `window_pane_copy_paste` (window.c:2019-2050)
    /// after a write to pane `p`: with `synchronize-panes` on for `p`, and
    /// `p` taking input (no mode, a program, input not off), every other
    /// pane of the window with the option on that is not in a mode, has a
    /// program, takes input and is visible (not hidden by a zoom) gets the
    /// bytes `bytes_for` makes for it.
    pub(crate) fn copy_to_synchronized(
        &mut self,
        p: PaneId,
        bytes_for: impl Fn(&Self, PaneId) -> Vec<u8>,
    ) {
        let Some(pane) = self.model.panes.get(&p) else {
            return;
        };
        if !pane.modes.is_empty() || pane.io.is_none() || pane.input_off {
            return;
        }
        for other in self.model.synchronized_peers(p) {
            let bytes = bytes_for(self, other);
            if let Some(io) = self.model.panes.get_mut(&other).and_then(|o| o.io.as_mut()) {
                io.write(&bytes);
            }
            self.pane_output_queued(other);
        }
    }

    /// Bytes to a pane's PTY, unless its input is off (window.c:2095).
    pub(crate) fn write_pane(&mut self, p: PaneId, bytes: &[u8]) {
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        if let Some(io) = pane.io.as_mut().filter(|_| !pane.input_off) {
            io.write(bytes);
        }
        self.pane_output_queued(p);
    }

    /// This turn's frames, then the command clients whose commands are
    /// done ([`Self::pending_exits`]) and the format jobs; the turn's pane
    /// redraws and bursts are used up.
    fn render_and_exit(&mut self) {
        self.render_clients();
        self.release_hidden_screens(Instant::now());
        for (id, code) in std::mem::take(&mut self.pending_exits) {
            self.finish_client(id, code);
        }
        self.format_jobs_flush();
        self.model.redraw_panes.clear();
        self.burst_started.clear();
        self.activity_fired.clear();
        self.mode_drawn.clear();
    }

    /// Whether client `id` takes no frame now: the server is exiting, its
    /// session is gone, or it is suspended (its terminal is not ours).
    fn render_skips(&mut self, id: ClientId, s: SessionId) -> bool {
        let suspended = self.clients[&id].suspended;
        if self.shutdown {
            // server_send_exit (server.c:321-323): a suspended client
            // is dropped, not told.
            if suspended {
                self.drop_client(id);
            } else {
                self.exit_attached(id, 1, Some("server exited".into()), false);
            }
            return true;
        }
        if self.model.current_of(s).is_none() {
            self.exit_attached(id, 0, Some("exited".into()), false);
            return true;
        }
        // server_client_check_redraw (server-client.c:2510); a client
        // re-attaching across an upgrade is drawn once it is back (one with
        // a token not sent yet, while the dry run runs, is drawn as any).
        if suspended
            || self.clients[&id]
                .reattach
                .is_some_and(|r| r.deadline.is_some())
        {
            self.clients
                .get_mut(&id)
                .expect("client")
                .passthrough
                .clear();
            return true;
        }
        false
    }

    /// Client `id` (on session `s`) redraws more than its panes this
    /// frame: `CLIENT_REDRAWWINDOW`, `CLIENT_REDRAWBORDERS` or
    /// `CLIENT_REDRAWSTATUS`. `redraw_draw` then expands the border styles of
    /// its window's panes again (screen-redraw.c:1726-1731).
    fn redraw_pending(&self, id: ClientId, s: SessionId) -> bool {
        let Some(client) = self.clients.get(&id) else {
            return false;
        };
        let pending = client.status.redraw_pending();
        if pending && let Some((_, w, _)) = self.model.current_of(s) {
            client.window_cache.borders.forget(&self.model, w);
        }
        pending
    }

    fn render_clients(&mut self) {
        let attached: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| c.control.is_none())
            .filter_map(|(&id, c)| c.attached.map(|s| (id, s)))
            .collect();
        // The rows each pane wrote under synchronized output that ended,
        // drawn whole for every client ([`Self::force_sync_flush`]).
        let mut flushed: BTreeMap<PaneId, Vec<bool>> = BTreeMap::new();
        self.update_moved_offsets();
        for (id, s) in attached {
            if self.render_skips(id, s) {
                continue;
            }
            let client = &self.clients[&id];
            let Some(tty) = client.hello.as_ref().and_then(|h| h.tty.as_ref()) else {
                continue;
            };
            let Some(mut caps) = client.caps else {
                continue;
            };
            caps.overlay = client.popup.is_some();
            let cols = u16::try_from(tty.cols).unwrap_or(u16::MAX).max(1);
            let rows = u16::try_from(tty.rows).unwrap_or(u16::MAX).max(1);
            let queued = client.unacked.bytes;
            let lines = u16::try_from(self.status_line_size(client)).unwrap_or(u16::MAX);
            let client = self.clients.get_mut(&id).expect("client");
            if !client.compositor.ready(queued, cols, rows) {
                // tty_add drops what comes while the client is blocked.
                client.passthrough.clear();
                continue;
            }
            if self.defer_frame(id, s, queued) {
                // tty_cmd_rawstring writes the payloads to `tty->out` as they
                // come, whether a redraw is deferred or not; they count
                // towards the block threshold like any output, so a client
                // that stops reading blocks and the rest is dropped
                // (tty_add) instead of piling up here.
                self.send_passthrough(id, caps);
                continue;
            }
            self.redraw_stops_sync(id, s);
            for (&p, pane) in &mut self.model.panes {
                if let Some(rows) = pane.term.take_sync_flush() {
                    flushed.insert(p, rows);
                }
            }
            // screen-redraw.c redraw_set_context: the scene sits below a
            // top status (REDRAW_STATUS_TOP) and above a bottom one.
            let area = rows.saturating_sub(lines);
            let top = self.model.status_top(TreeId::Session(s));
            let scene_y = if top { lines } else { 0 };
            // TTY_FREEZE (status.c:385-386, tty.c:1646): under a message
            // pane output is not written, so the terminal keeps the pane
            // content it shows until a full redraw (server_redraw_client)
            // or the message clears. Borders and pane status lines still
            // draw (CLIENT_REDRAWBORDERS is not frozen).
            let redrawing = self.redraw_pending(id, s);
            let (jobs, window_jobs) = self.redraw_jobs(id, redrawing);
            let client = self.clients.get_mut(&id).expect("client");
            let frozen = client.status.hold_window();
            let held = frozen
                .then(|| client.compositor.shown_rows(cols, rows, scene_y, area))
                .flatten()
                .map(<[vtmux_vt::Cell]>::to_vec);
            // The popup's rectangle is frozen the same way: its job's
            // output is not written under a prompt (tty.c:1646), so the
            // terminal keeps what it shows until a full redraw.
            let held_popup = frozen
                .then(|| client.compositor.shown_rows(cols, rows, 0, rows))
                .flatten()
                .map(<[vtmux_vt::Cell]>::to_vec);
            let repeat = !redrawing
                && !frozen
                && self.repeats_plain_frame(id, s, caps, (cols, rows), (scene_y, area));
            let (mut screen, mut tags, plain) =
                self.frame_buffer(id, s, caps, (cols, rows), (scene_y, area), frozen);
            let drawn = self.draw_window(
                id,
                s,
                caps,
                (&mut screen, &mut tags, plain),
                scene_y,
                area,
                held.as_deref(),
                &window_jobs,
                repeat,
            );
            if !drawn {
                let client = self.clients.get_mut(&id).expect("client");
                client.compositor.recycle(screen, tags);
                let mut frame = Vec::new();
                self.add_outer_mouse_modes(id, &mut frame);
                if !frame.is_empty() {
                    self.send(id, &ServerMsg::Frame(frame));
                }
                self.send_passthrough(id, caps);
                continue;
            }
            self.draw_status(id, s, caps, &mut screen, lines, top, jobs);
            // screen-redraw.c:1813: the overlay over everything else.
            self.draw_popup(id, caps, &mut screen, held_popup.as_deref());
            self.force_sync_flush(id, s, &flushed, scene_y, area);
            let mut frame = self
                .clients
                .get_mut(&id)
                .expect("client")
                .compositor
                .frame_tagged(screen, tags, caps);
            self.add_outer_mouse_modes(id, &mut frame);
            if redrawing && let Some(title) = self.client_title(id, s) {
                frame.splice(0..0, title);
            }
            if !frame.is_empty() {
                self.send(id, &ServerMsg::Frame(frame));
            }
            self.send_passthrough(id, caps);
        }
    }

    /// Drop the screen view ([`vtmux_vt::PaneTerm::release_screen`]) of
    /// every pane no attached client's window shows, once it has had no
    /// output for [`HIDDEN_VIEW_IDLE`] ([`hidden_view_kept`]): an idle
    /// hidden pane keeps only its terminal, and its view is read again
    /// when it is drawn (about 0.7 MB per full 100x50 pane;
    /// `docs/PERFORMANCE.md`, *Hidden panes*). A busy one keeps it, as
    /// each synchronized update reads it (`?2026h` holds the screen, and
    /// its end compares the rows): rebuilt from a whole-screen read at
    /// every update, that cost 5x tmux's CPU with agent TUIs in hidden
    /// windows (*Agent panes*). Output while shown does not count. A
    /// preview (choose-tree, display-panes) reads a released view again
    /// each frame it draws.
    fn release_hidden_screens(&mut self, now: Instant) {
        let shown: BTreeSet<WindowId> = self
            .clients
            .values()
            .filter(|c| c.control.is_none())
            .filter_map(|c| self.model.current_of(c.attached?))
            .map(|(_, w, _)| w)
            .collect();
        let windows = &self.model.windows;
        for pane in self.model.panes.values_mut() {
            let visible = shown.contains(&pane.window)
                && windows
                    .get(&pane.window)
                    .is_some_and(|w| w.is_visible(pane.id));
            if visible {
                pane.output_at = None;
            } else if !hidden_view_kept(pane.output_at, now) {
                pane.output_at = None;
                pane.term.release_screen();
            }
        }
    }

    /// When the next hidden pane's view is due to be released
    /// ([`Self::release_hidden_screens`]).
    fn hidden_view_deadline(&self) -> Option<Instant> {
        self.model
            .panes
            .values()
            .filter_map(|p| p.output_at)
            .min()
            .map(|t| t + HIDDEN_VIEW_IDLE)
    }

    /// Client `id`'s buffer for the next frame, its row tags, and whether
    /// the frame is plain ([`Self::plain_frame`], not frozen): then its
    /// window rows are the composed rows alone, so the buffer keeps those
    /// it already has ([`crate::render::Compositor::buffer`]).
    fn frame_buffer(
        &mut self,
        id: ClientId,
        s: SessionId,
        caps: crate::render::Caps,
        (cols, rows): (u16, u16),
        (y, area): (u16, u16),
        frozen: bool,
    ) -> (vtmux_vt::Snapshot, Vec<u64>, bool) {
        let plain = !frozen
            && self
                .plain_frame(id, s, caps, (cols, rows), (y, area))
                .is_some();
        let keep = if plain { y..y + area } else { 0..0 };
        let client = self.clients.get_mut(&id).expect("client");
        let (screen, tags) = client.compositor.buffer(cols, rows, keep);
        (screen, tags, plain)
    }

    /// Whether client `id`'s next frame has nothing over the window but the
    /// status line and the same inputs as the last one, which was too: it
    /// is that frame again unless a window row or the cursor changes
    /// ([`Self::draw_window`]'s `repeat`). Records the frame's inputs.
    fn repeats_plain_frame(
        &mut self,
        id: ClientId,
        s: SessionId,
        caps: crate::render::Caps,
        size: (u16, u16),
        area: (u16, u16),
    ) -> bool {
        let plain = self.plain_frame(id, s, caps, size, area);
        let Some(client) = self.clients.get_mut(&id) else {
            return false;
        };
        let last = client.window_cache.plain.get();
        // The last cursor stays until this frame's drawing sets it.
        client
            .window_cache
            .plain
            .set(plain.map(|p| crate::render::PlainFrame {
                cursor: last.map_or(p.cursor, |l| l.cursor),
                ..p
            }));
        plain.zip(last).is_some_and(|(p, l)| p.same_inputs(l))
            && client.compositor.shows(size.0, size.1)
    }

    /// What client `id`'s next frame is drawn from, if nothing but the
    /// status line is over the window ([`crate::render::PlainFrame`]).
    fn plain_frame(
        &self,
        id: ClientId,
        s: SessionId,
        caps: crate::render::Caps,
        size: (u16, u16),
        area: (u16, u16),
    ) -> Option<crate::render::PlainFrame> {
        let client = self.clients.get(&id)?;
        if client.popup.is_some() || !client.status.quiet() {
            return None;
        }
        let (_, w, _) = self.model.current_of(s)?;
        let window = self.model.windows.get(&w)?;
        if window.menu.is_some() {
            return None;
        }
        let prompts = window
            .panes
            .panes
            .iter()
            .any(|p| self.model.panes.get(p).is_some_and(|x| x.prompt.is_some()));
        if prompts {
            return None;
        }
        Some(crate::render::PlainFrame {
            caps,
            size,
            area,
            cursor: (None, false),
        })
    }

    /// `redraw_draw` with `REDRAW_PANE` (screen-redraw.c:1757-1770): a pane
    /// drawn whole ends its synchronized output (`screen_write_stop_sync`)
    /// — every visible pane of the window under a window redraw
    /// (`CLIENT_REDRAWWINDOW`), else each pane with `PANE_REDRAW`.
    fn redraw_stops_sync(&mut self, id: ClientId, s: SessionId) {
        let Some((_, w, _)) = self.model.current_of(s) else {
            return;
        };
        let all = self
            .clients
            .get(&id)
            .is_some_and(|c| c.status.redraw_window_pending());
        let Some(window) = self.model.windows.get(&w) else {
            return;
        };
        let panes: Vec<PaneId> = window
            .panes
            .panes
            .iter()
            .copied()
            .filter(|&p| window.is_visible(p) && (all || self.model.redraw_panes.contains(&p)))
            .collect();
        for p in panes {
            if let Some(pane) = self.model.panes.get_mut(&p) {
                pane.term.stop_sync();
            }
        }
    }

    /// `screen_write_flush_dirty` (screen-write.c:1267-1290): when a
    /// pane's synchronized output ends, each line it wrote meanwhile goes
    /// to the terminal whole (`tty_cmd_redrawline`), not as a diff, so the
    /// update arrives as whole rows in one synchronized frame. `flushed`
    /// has those rows per pane; the ones of client `id`'s current window
    /// that show the pane's own screen are forced into its next frame.
    fn force_sync_flush(
        &mut self,
        id: ClientId,
        s: SessionId,
        flushed: &BTreeMap<PaneId, Vec<bool>>,
        scene_y: u16,
        area: u16,
    ) {
        if flushed.is_empty() {
            return;
        }
        let Some((_, w, _)) = self.model.current_of(s) else {
            return;
        };
        let Some(window) = self.model.windows.get(&w) else {
            return;
        };
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        for (p, rows) in flushed {
            let Some(pane) = self.model.panes.get(p) else {
                continue;
            };
            if pane.window != w || !window.is_visible(*p) || !pane.modes.is_empty() {
                continue;
            }
            let rect = pane.rect;
            let left = i64::from(rect.xoff);
            let (Ok(left), Ok(width)) = (u16::try_from(left), u16::try_from(rect.sx)) else {
                continue;
            };
            for (py, _) in rows.iter().enumerate().filter(|(_, d)| **d) {
                let wy = i64::from(rect.yoff) + i64::try_from(py).unwrap_or(i64::MAX);
                if let Ok(wy) = u16::try_from(wy)
                    && wy < area
                {
                    client.compositor.force(scene_y + wy, left, width);
                }
            }
        }
    }

    /// When the soonest sync timer expires (`screen_write_sync_callback`).
    fn sync_deadline(&self) -> Option<Instant> {
        self.model
            .panes
            .values()
            .filter_map(|p| p.term.sync_deadline())
            .min()
    }

    /// `screen_write_sync_callback` (screen-write.c:1017-1029): a pane still
    /// in synchronized output a second after it began is drawn as it is;
    /// a hung program must not freeze it.
    fn sync_timers(&mut self, now: Instant) {
        for pane in self.model.panes.values_mut() {
            if pane.term.sync_deadline().is_some_and(|d| d <= now) {
                pane.term.stop_sync();
            }
        }
    }

    /// The `DCS tmux;` payloads queued for client `id` since its last
    /// frame, written raw (`tty_cmd_rawstring`), then the cursor put back.
    fn send_passthrough(&mut self, id: ClientId, caps: crate::render::Caps) {
        // The frame's write may have dropped the client.
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        let mut raw = std::mem::take(&mut client.passthrough);
        if let Some(title) = osc_title(&raw) {
            client.terminal_title = Some(title);
        }
        if !raw.is_empty() {
            // The program may have moved the terminal's cursor.
            raw.extend(client.compositor.cursor_bytes(caps));
            self.send(id, &ServerMsg::Frame(raw));
        }
    }

    /// `server_client_check_redraw` (server-client.c:2532-2558): while the
    /// client's terminal still has output to write (`queued`), defer the
    /// frame; the client's `FrameAck` turns the loop and lands here again. A
    /// flood is then drawn at the rate the terminal takes it, and while a
    /// pane still has output waiting (the backlog) at most once per
    /// [`crate::render::FRAME_INTERVAL`], not once per pane read, unless the
    /// frame also shows a change the flood did not make
    /// ([`Self::urgent_change`]). A pane's
    /// `PANE_REDRAW` (cleared at the end of this turn) becomes
    /// `CLIENT_REDRAWWINDOW`, as tmux's deferred redraw does.
    fn defer_frame(&mut self, id: ClientId, s: SessionId, queued: usize) -> bool {
        let flood = !self.backlog.is_empty();
        let urgent = self.urgent_change(id, s);
        let Some(client) = self.clients.get_mut(&id) else {
            return true;
        };
        if urgent {
            client.compositor.expedite();
        }
        if queued == 0 && client.compositor.paced(Instant::now(), flood) {
            return false;
        }
        let pane_redraw = self.model.current_of(s).is_some_and(|(_, w, _)| {
            self.model
                .redraw_panes
                .iter()
                .any(|p| self.model.panes.get(p).is_some_and(|x| x.window == w))
        });
        if pane_redraw {
            client.status.request_window_redraw();
        }
        true
    }

    /// Whether client `id`'s next frame shows a change that is not flood
    /// output, so the flood's pacing must not delay it: output that started
    /// a burst in a pane of its current window (a key's echo next to a
    /// flood), a pane of that window to redraw whole (a mode entered or
    /// left, a screen switch), or a status or window redraw asked for.
    fn urgent_change(&self, id: ClientId, s: SessionId) -> bool {
        let Some(client) = self.clients.get(&id) else {
            return false;
        };
        if client.status.redraw_pending() {
            return true;
        }
        self.model
            .current_of(s)
            .is_some_and(|(_, w, _)| self.unpaced_change_in(w))
    }

    /// A pane of window `w` started a burst of output this turn, had its
    /// copy mode drawn ([`Self::mode_drawn`]) or is to be redrawn whole.
    fn unpaced_change_in(&self, w: WindowId) -> bool {
        self.burst_started
            .iter()
            .chain(&self.mode_drawn)
            .chain(&self.model.redraw_panes)
            .any(|p| self.model.panes.get(p).is_some_and(|x| x.window == w))
    }

    /// The job handles for one frame: the status line's, and the pane
    /// borders'. `CLIENT_STATUSFORCE` (used up here) runs the status line's jobs
    /// again; a border's are never forced (window-border.c:199 passes
    /// `FORMAT_STATUS` alone). Pane status lines only expand, and so only start
    /// jobs, when the status or borders redraw (`redraw_screen`,
    /// screen-redraw.c:1876-1879); other frames show what the jobs last said.
    fn redraw_jobs(
        &mut self,
        id: ClientId,
        redrawing: bool,
    ) -> (crate::format_jobs::JobHandle, crate::format_jobs::JobHandle) {
        let force = self
            .clients
            .get_mut(&id)
            .is_some_and(|c| c.status.take_force());
        let border = self.job_handle(Some(id)).status(false);
        let border = if redrawing { border } else { border.peek() };
        (self.job_handle(Some(id)).status(force), border)
    }

    /// `server_client_reset_state`: the terminal's mouse modes follow `mouse`
    /// and the panes (`tty_update_mode`), written after the drawing and
    /// inside the frame's synchronized update when there is one.
    fn add_outer_mouse_modes(&mut self, id: ClientId, frame: &mut Vec<u8>) {
        let modes = self.outer_mouse_bytes(id);
        if modes.is_empty() {
            return;
        }
        let at = if frame.ends_with(b"\x1b[?2026l") {
            frame.len() - 8
        } else {
            frame.len()
        };
        frame.splice(at..at, modes);
    }

    /// `server_client_set_title` (server-client.c:2588-2610), run on a
    /// redraw with `set-titles` on: `set-titles-string` expanded for the
    /// client, as `tty_set_title` writes it unless it is the title last
    /// written.
    fn client_title(&mut self, id: ClientId, s: SessionId) -> Option<Vec<u8>> {
        let tree = TreeId::Session(s);
        if self.model.number_option(tree, "set-titles") == 0 {
            return None;
        }
        let template = self
            .model
            .options
            .get(tree, "set-titles-string")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        let mut ctx = self.format_ctx(crate::target::Found::from_session(&self.model, s), Some(id));
        // format_defaults(ft, c, NULL, NULL, NULL) (server-client.c:2624).
        ctx.kind = crate::format_ctx::Kind::Unknown;
        let title = vtmux_core::format::expand_time(&template, &ctx);
        let client = self.clients.get_mut(&id)?;
        if client.title.as_deref() == Some(title.as_str()) {
            return None;
        }
        client.title = Some(title.clone());
        let bytes = client.term.as_ref()?.set_title(&title)?;
        client.terminal_title = Some(title);
        Some(bytes)
    }

    /// Session `s`'s current window composed into rows
    /// `y..y + rows` of `screen`, with the active pane's cursor. `held` is
    /// what the terminal shows in those rows while pane output is frozen:
    /// pane content cells keep it. `screen` comes with its row tags
    /// ([`crate::render::Compositor::buffer`]) and whether the frame is
    /// plain (nothing drawn over the window), in which case its window rows
    /// still hold an earlier frame's and only the changed ones are copied.
    #[expect(clippy::too_many_arguments, reason = "the redraw's inputs")]
    #[expect(clippy::too_many_lines, reason = "one pass over the window's layers")]
    fn draw_window(
        &self,
        id: ClientId,
        s: SessionId,
        caps: crate::render::Caps,
        (screen, tags, plain): (&mut vtmux_vt::Snapshot, &mut [u64], bool),
        y: u16,
        rows: u16,
        held: Option<&[vtmux_vt::Cell]>,
        jobs: &crate::format_jobs::JobHandle,
        repeat: bool,
    ) -> bool {
        let source = self.render_source();
        let cache = self.clients.get(&id).map(|c| &c.window_cache);
        let view = self.clients.get(&id).map(|c| c.view).unwrap_or_default();
        let origin = (view.ox, view.oy);
        let Some((scene, mut draw)) =
            source.window(s, screen.cols, rows, origin, caps, Some(jobs), cache)
        else {
            // Nothing to draw: the rows are blank, as from a fresh buffer.
            crate::render::compose::ComposeCache::default().copy_rows(screen, y, rows, tags);
            return true;
        };
        let theme = source.theme(caps);
        if let Some(c) = self.clients.get(&id) {
            source.blend(s, &mut draw, &c.reported.blend_colours(), caps, &theme);
        }
        if let Some(c) = cache {
            *c.border_ranges.borrow_mut() = draw
                .panes
                .iter()
                .filter(|(_, d)| !d.status_ranges.is_empty())
                .map(|(p, d)| (*p, d.status_ranges.clone()))
                .collect();
        }
        let composed = cache.map(|c| {
            crate::render::compose::compose_cached(
                &mut c.compose.borrow_mut(),
                &scene,
                &draw,
                &theme,
                screen.cols,
                rows,
            )
        });
        let cursor = crate::render::window::cursor_of(&draw, &self.model, screen.cols, view, y);
        if repeat
            && composed == Some(false)
            && cache
                .and_then(|c| c.plain.get())
                .is_some_and(|p| p.cursor == cursor)
        {
            #[cfg(test)]
            if let Some(c) = cache {
                c.skipped.set(c.skipped.get() + 1);
            }
            return false;
        }
        if let Some(c) = cache.filter(|_| plain) {
            c.compose.borrow().copy_rows(screen, y, rows, tags);
            if let Some(mut p) = c.plain.get() {
                p.cursor = cursor;
                c.plain.set(Some(p));
            }
            (screen.cursor, screen.cursor_visible) = cursor;
            return true;
        }
        for tag in tags.iter_mut().skip(usize::from(y)).take(usize::from(rows)) {
            *tag = 0;
        }
        let cols = usize::from(screen.cols);
        let kept = cache.and_then(|c| c.buffer.borrow_mut().take());
        let mut window = crate::render::blank(kept, screen.cols, rows);
        window.wrapped.clone_from(&screen.wrapped);
        window.cursor = screen.cursor;
        window.cursor_visible = screen.cursor_visible;
        match cache {
            Some(c) => c.compose.borrow().copy_to(&mut window),
            None => crate::render::compose::compose(&scene, &draw, &theme, &mut window),
        }
        // The window's own rows: a pane prompt or the window menu may
        // place the cursor there instead.
        let local = (
            cursor.0.and_then(|(x, cy)| Some((x, cy.checked_sub(y)?))),
            cursor.1,
        );
        (window.cursor, window.cursor_visible) = local;
        if let Some(c) = cache
            && let Some(mut plain) = c.plain.get()
        {
            plain.cursor = cursor;
            c.plain.set(Some(plain));
        }
        if let Some(held) = held {
            for (i, (cell, shown)) in window.cells.iter_mut().zip(held).enumerate() {
                if let Some(crate::render::scene::SceneCell::Pane { pane, .. }) = scene.cells.get(i)
                    && !self.model.redraw_panes.contains(pane)
                {
                    cell.clone_from(shown);
                }
            }
        }
        self.draw_pane_prompts(s, &source, &theme, caps, &mut window, origin);
        self.draw_menu(s, &source, &theme, caps, &mut window, origin);
        let at = usize::from(y) * cols;
        // A swap, not a copy: the window buffer is blanked before its next
        // use anyway.
        screen.cells[at..at + window.cells.len()].swap_with_slice(&mut window.cells);
        (screen.cursor, screen.cursor_visible) = if (window.cursor, window.cursor_visible) == local
        {
            cursor
        } else {
            (
                window.cursor.map(|(x, cy)| (x, cy + y)),
                window.cursor_visible,
            )
        };
        if let Some(c) = cache {
            *c.buffer.borrow_mut() = Some(window);
        }
        true
    }

    /// Client `id`'s popup over `screen` (`popup_draw_cb`), and the cursor
    /// in it (`popup_mode_cb`, server-client.c:2198-2200).
    fn draw_popup(
        &self,
        id: ClientId,
        caps: crate::render::Caps,
        screen: &mut vtmux_vt::Snapshot,
        held: Option<&[vtmux_vt::Cell]>,
    ) {
        let Some(p) = self.clients.get(&id).and_then(|c| c.popup.as_ref()) else {
            return;
        };
        let source = self.render_source();
        let Some((defaults, border)) = self.popup_styles(id, &source) else {
            return;
        };
        let content = vtmux_vt::snapshot(&p.term);
        let draw = crate::render::popup::PopupDraw {
            px: p.px,
            py: p.py,
            sx: p.sx,
            sy: p.sy,
            lines: p.lines,
            border: &border,
            defaults: &defaults,
            title: &p.title,
            content: &content,
            redrawn: p.redrawn,
        };
        // popup_draw_cb clears overlay_check while it draws (popup.c:229-231):
        // its own empty runs are erased as without an overlay.
        let clear = crate::render::Caps {
            overlay: false,
            ..caps
        }
        .clear();
        if let Some(held) = held {
            crate::render::popup::keep(&draw, held, screen);
        } else {
            crate::render::popup::draw(&draw, &source.theme(caps), clear, screen);
        }
        // server_client_reset_state: an open prompt keeps the cursor
        // (server-client.c:2220-2222); the popup's screen still gives the
        // cursor's visibility (its mode).
        let prompt = self
            .clients
            .get(&id)
            .is_some_and(|c| c.status.prompt().is_some());
        if !prompt {
            let (cx, cy) = p.cursor();
            // A popup clipped to a narrow terminal leaves the cursor beyond
            // it (popup_resize_cb keeps the job's screen); the terminal
            // clamps a cursor move to its last column.
            screen.cursor = Some((
                cx.min(screen.cols.saturating_sub(1)),
                cy.min(screen.rows.saturating_sub(1)),
            ));
        }
        screen.cursor_visible = content.cursor_visible;
    }

    /// The menu of session `s`'s window over its scene
    /// (`redraw_draw_menu_lines`), and the cursor at its choice, hidden
    /// (`menu_get_cursor`; the menu's screen has no `MODE_CURSOR`).
    fn draw_menu(
        &self,
        s: SessionId,
        source: &crate::render::window::Source<'_>,
        theme: &crate::render::compose::Theme,
        caps: crate::render::Caps,
        window: &mut vtmux_vt::Snapshot,
        (ox, oy): (u32, u32),
    ) {
        let Some((_, w, _)) = self.model.current_of(s) else {
            return;
        };
        let Some(win) = self.model.windows.get(&w) else {
            return;
        };
        let Some(menu) = win.menu.as_ref() else {
            return;
        };
        let styles = self.menu_styles(w, menu, source);
        crate::render::menu::draw(
            menu,
            &styles,
            theme,
            caps.clear(),
            window,
            (win.sx(), win.sy()),
            (ox, oy),
        );
        // server_client_reset_state (server-client.c:2224-2233): in the
        // view, else hidden where it was.
        let (cx, cy) = menu.cursor();
        window.cursor = cx
            .checked_sub(ox)
            .zip(cy.checked_sub(oy))
            .and_then(|(x, y)| u16::try_from(x).ok().zip(u16::try_from(y).ok()))
            .filter(|&(x, y)| x < window.cols && y < window.rows);
        window.cursor_visible = false;
    }

    /// `redraw_draw_pane_prompt` for every visible pane of session `s`'s
    /// window (screen-redraw.c:1625-1673, 1780-1788): the prompt over the
    /// pane's last line, or its first under a top status; and
    /// `server_client_prompt_cursor` (server-client.c:2129-2165): with a
    /// prompt on the active pane the cursor goes to its edit point, shown.
    fn draw_pane_prompts(
        &self,
        s: SessionId,
        source: &crate::render::window::Source<'_>,
        theme: &crate::render::compose::Theme,
        caps: crate::render::Caps,
        window: &mut vtmux_vt::Snapshot,
        (ox, oy): (u32, u32),
    ) {
        let Some((_, w, active)) = self.model.current_of(s) else {
            return;
        };
        let Some(win) = self.model.windows.get(&w) else {
            return;
        };
        let tree = TreeId::Session(s);
        let top = self.model.status_top(tree);
        let cols = usize::from(window.cols);
        for &p in &win.panes.panes {
            let Some(pane) = self.model.panes.get(&p) else {
                continue;
            };
            let Some(prompt) = pane.prompt.as_ref() else {
                continue;
            };
            let (sx, sy) = (pane.rect.sx, pane.rect.sy);
            if !win.is_visible(p) || sx == 0 || sy == 0 {
                continue;
            }
            // In view coordinates (redraw_draw_pane_prompt): a prompt
            // row outside the view is not drawn, and the active pane's
            // cursor is hidden (server_client_prompt_cursor).
            let wy = pane.rect.yoff + if top { 0 } else { int(sy) - 1 } - int(oy);
            let xoff = pane.rect.xoff - int(ox);
            let (Ok(wy), Ok(width)) = (usize::try_from(wy), usize::try_from(sx)) else {
                if p == active {
                    window.cursor_visible = false;
                }
                continue;
            };
            if wy >= usize::from(window.rows) {
                if p == active {
                    window.cursor_visible = false;
                }
                continue;
            }
            let (row, cx) = crate::render::status::pane_prompt_row(source, tree, prompt, width);
            // Columns left of the scene are cut off (`xoff`), and the
            // line ends at the scene's edge.
            let skip = usize::try_from(-xoff).unwrap_or(0);
            let px = usize::try_from(xoff).unwrap_or(0);
            if px >= cols || skip >= width {
                if p == active {
                    window.cursor_visible = false;
                }
                continue;
            }
            let n = (width - skip).min(cols - px);
            let at = wy * cols + px;
            let cells = &mut window.cells[at..at + n];
            for (cell, drawn) in cells.iter_mut().zip(&row[skip..]) {
                *cell = theme.cell(drawn);
            }
            let bgs: Vec<vtmux_core::colour::Colour> =
                row[skip..skip + n].iter().map(|c| c.style.bg).collect();
            crate::render::compose::draw_line_by(
                cells,
                &bgs,
                vtmux_core::colour::Colour::Default,
                caps.clear(),
                false,
            );
            if p == active {
                let x = xoff + int(u32::try_from(cx).unwrap_or(u32::MAX));
                window.cursor = u16::try_from(x)
                    .ok()
                    .filter(|&x| x <= window.cols)
                    .map(|x| (x.min(window.cols), u16::try_from(wy).unwrap_or(0)));
                window.cursor_visible = window.cursor.is_some();
            }
        }
    }

    /// The client's status lines, or its message, over `screen`
    /// ([`crate::render::status::draw`]), expanded in the client's format
    /// tree when they need drawing again.
    #[expect(clippy::too_many_arguments, reason = "the redraw's inputs")]
    fn draw_status(
        &mut self,
        id: ClientId,
        s: SessionId,
        caps: crate::render::Caps,
        screen: &mut vtmux_vt::Snapshot,
        lines: u16,
        top: bool,
        jobs: crate::format_jobs::JobHandle,
    ) {
        let Some(mut status) = self
            .clients
            .get_mut(&id)
            .map(|c| std::mem::take(&mut c.status))
        else {
            return;
        };
        {
            let source = self.render_source();
            // status_redraw (status.c:241-244): the client's tree, FORMAT_STATUS
            // and maybe FORMAT_FORCE, format_defaults(ft, c, NULL, NULL, NULL).
            let ctx = || {
                let mut ctx =
                    self.format_ctx(crate::target::Found::from_session(&self.model, s), Some(id));
                ctx.jobs = Some(jobs);
                ctx.kind = crate::format_ctx::Kind::Unknown;
                ctx
            };
            status.draw(&source, ctx, s, caps, screen, lines, top);
        }
        if let Some(c) = self.clients.get_mut(&id) {
            c.status = status;
        }
    }

    // ------------------------------------------------------ status line

    /// `status_message_set(c, -1, 1, 0, 0, "%s", text)`: the message most
    /// callers show (errors from bound keys, alerts, `list-keys -1`):
    /// `display-time`, styles ignored, keys clear it, the pane area frozen.
    pub fn status_message_set(&mut self, id: ClientId, text: String) {
        let flags = crate::render::status::MessageFlags {
            ignore_styles: true,
            ..Default::default()
        };
        self.status_message(id, text, None, flags);
    }

    /// [`Self::status_message_set`] for every terminal client attached to
    /// session `s`.
    pub fn status_message_session(&mut self, s: SessionId, text: &str) {
        let ids: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| c.attached == Some(s) && c.control.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.status_message_set(id, text.to_owned());
        }
    }

    /// `status_message_set` (status.c:339-390) in full: `delay_ms` `None`
    /// is the session's `display-time`, 0 waits for a key. A client with
    /// no session or a control client shows nothing.
    pub fn status_message(
        &mut self,
        id: ClientId,
        text: String,
        delay_ms: Option<u64>,
        flags: crate::render::status::MessageFlags,
    ) {
        // status_message_set (status.c:354-362): logged first.
        self.add_message(format!("{} message: {text}", self.client_name(id)));
        let Some(s) = self
            .clients
            .get(&id)
            .filter(|c| c.control.is_none())
            .and_then(|c| c.attached)
        else {
            return;
        };
        let delay = delay_ms.unwrap_or_else(|| {
            u64::try_from(self.model.number_option(TreeId::Session(s), "display-time")).unwrap_or(0)
        });
        if let Some(c) = self.clients.get_mut(&id) {
            c.status.set_message(Instant::now(), text, delay, flags);
        }
    }

    /// `status_timer_start(c)`: the client's `status-interval` timer from
    /// its session's options.
    pub(crate) fn status_timer_start(&mut self, id: ClientId) {
        let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
            return;
        };
        let tree = TreeId::Session(s);
        let on = self.model.status_lines(tree) != 0;
        let interval =
            u64::try_from(self.model.number_option(tree, "status-interval")).unwrap_or(0);
        if let Some(c) = self.clients.get_mut(&id) {
            c.status.start_timer(Instant::now(), on, interval);
        }
    }

    /// `status_timer_start_all`.
    pub(crate) fn status_timer_start_all(&mut self) {
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.status_timer_start(id);
        }
    }

    /// The message and status timers that are due.
    fn status_timers(&mut self, now: Instant) {
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
                continue;
            };
            let interval = u64::try_from(
                self.model
                    .number_option(TreeId::Session(s), "status-interval"),
            )
            .unwrap_or(0);
            if let Some(c) = self.clients.get_mut(&id) {
                c.status.tick(now, interval);
            }
        }
    }

    /// `server_status_client`: the status lines are drawn again.
    pub(crate) fn status_redraw_client(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.status.request_redraw();
        }
    }

    /// `server_redraw_client`: the status and the pane area, even under a
    /// message (`CLIENT_REDRAWWINDOW` draws through `TTY_FREEZE`).
    pub(crate) fn redraw_client(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.status.request_full_redraw();
            // CLIENT_REDRAWOVERLAY: the popup is drawn again in full.
            if let Some(p) = c.popup.as_mut() {
                p.redrawn = true;
            }
        }
    }

    /// Apply the redraws the model asked for (server-fn.c
    /// `server_redraw_*` and `server_status_*`) to the clients.
    fn apply_redraws(&mut self) {
        use crate::model::Redraw;
        for r in std::mem::take(&mut self.model.redraws) {
            let ids: Vec<ClientId> = self
                .clients
                .iter()
                .filter(|(_, c)| {
                    let Some(s) = c.attached else { return false };
                    match r {
                        Redraw::Session(x) | Redraw::SessionStatus(x) => x == s,
                        Redraw::Window(w) | Redraw::WindowBorders(w) => {
                            self.model.current_of(s).is_some_and(|(_, cw, _)| cw == w)
                        }
                        Redraw::WindowStatus(w) => self
                            .model
                            .sessions
                            .get(&s)
                            .is_some_and(|x| x.windows.values().any(|v| *v == w)),
                    }
                })
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                match r {
                    Redraw::Session(_) | Redraw::Window(_) => self.redraw_client(id),
                    Redraw::WindowBorders(_) => {
                        if let Some(c) = self.clients.get_mut(&id) {
                            c.status.request_borders_redraw();
                        }
                    }
                    Redraw::SessionStatus(_) | Redraw::WindowStatus(_) => {
                        self.status_redraw_client(id);
                    }
                }
            }
        }
    }

    /// The session client `id` is attached to.
    #[must_use]
    pub fn client_session(&self, id: ClientId) -> Option<SessionId> {
        self.clients.get(&id)?.attached
    }

    /// The attached clients in connection order, as formats see them
    /// (`sort_get_clients`, sort.c:461-480).
    /// What drawing a window reads from the server.
    pub(crate) fn render_source(&self) -> crate::render::window::Source<'_> {
        crate::render::window::Source {
            model: &self.model,
            info: &self.info,
            env: &self.global_env,
            clients: crate::render::window::Clients::new(|| self.client_views()),
            themes: &self.themes,
            keys: &self.key_bindings,
        }
    }

    pub fn client_views(&self) -> std::rc::Rc<[ClientView]> {
        self.clients
            .iter()
            .filter(|(_, c)| c.attached.is_some() && !c.suspended)
            .map(|(&id, c)| self.client_view(id, c))
            .collect()
    }

    fn client_view(&self, id: ClientId, c: &Client) -> ClientView {
        let hello = c.hello.as_ref();
        let tty = hello.and_then(|h| h.tty.as_ref());
        let started = c.control.is_none() && tty.is_some();
        let term_name = tty
            .map(|t| t.term.clone())
            .or_else(|| {
                hello?
                    .env
                    .iter()
                    .find(|(k, _)| k == "TERM")
                    .map(|(_, v)| v.clone())
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        ClientView {
            order: u32::try_from(id.0).unwrap_or(u32::MAX),
            name: self.client_name(id),
            session: c.attached,
            activity: c.activity,
            activity_secs: c.activity_secs,
            created_secs: c.created_secs,
            // server-client.c:299: tty.sx starts at 80; tty.c tty_resize
            // sets it for a terminal, cmd-refresh-client.c
            // cmd_refresh_client_control_client_size (tty_set_size) for a
            // control client.
            width: if started {
                tty.map_or(80, |t| t.cols)
            } else {
                c.control.as_ref().map_or(80, |cs| cs.size().0)
            },
            height: started.then(|| tty.map_or(24, |t| t.rows)),
            cell_pixels: c.cell_pixels,
            term_name,
            tty: tty.map(|t| t.name.clone()).filter(|n| !n.is_empty()),
            control: c.control.is_some(),
            focused: c.focused,
            utf8: hello.is_some_and(|h| h.flags.utf8),
            pid: hello.map_or(0, |h| h.pid),
            key_table: c.keys.table.clone(),
            default_key_table: self.default_key_table(id),
            last_session: c.last_session,
            written: c.written,
            termfeatures: c.term.as_ref().map_or_else(
                || {
                    // c->term_features before tty_open: what the client sent.
                    let sent = c.hello.as_ref().and_then(|h| h.tty.as_ref());
                    vtmux_core::term_features::Features::from_client(
                        sent.map_or(&[][..], |t| &t.features),
                    )
                    .list()
                },
                |t| t.features().list(),
            ),
            termtype: c
                .term
                .as_ref()
                .map(|t| t.term_type().to_owned())
                .unwrap_or_default(),
            colours: c.caps.map(|caps| caps.colours),
            theme: c.reported.theme,
            flags: c.flags.clone(),
            term: c
                .term
                .as_ref()
                .filter(|_| !c.dead && !c.closing && !c.suspended)
                .map(std::rc::Rc::clone),
            environ: std::rc::Rc::clone(&c.environ),
            view: c.view,
        }
    }

    /// What `show-messages -T` says about a client's terminal: its `TERM`
    /// name, `tty_term`'s `flags` (`TERM_256COLOURS` 0x1, `TERM_NOAM` 0x2,
    /// `TERM_RGBCOLOURS` 0x10, `TERM_VT100LIKE` 0x20) and its feature list.
    /// `None` before the terminal is open.
    pub(crate) fn client_term(&self, id: ClientId) -> Option<(String, u32, String)> {
        let c = self.clients.get(&id)?;
        let term = c.term.as_ref()?;
        let name = c.hello.as_ref()?.tty.as_ref()?.term.clone();
        let flags = u32::from(term.colours256())
            | if term.flag("am") { 0 } else { 0x2 }
            | if term.rgb() { 0x10 } else { 0 }
            | if term.vt100_like() { 0x20 } else { 0 };
        Some((name, flags, term.features().list()))
    }

    /// `cmd_find_best_client` (cmd-find.c:112-130): the most recently active
    /// client attached to `s`, or to any session when `s` has none.
    pub fn best_client(&self, s: Option<SessionId>) -> Option<ClientId> {
        // s->attached == 0 (cmd-find.c:117): suspended clients do not count.
        let s = s.filter(|&s| {
            self.clients
                .values()
                .any(|c| c.attached == Some(s) && !c.suspended)
        });
        let mut best: Option<(ClientId, u64)> = None;
        for (&id, c) in &self.clients {
            let Some(cs) = c.attached else { continue };
            if s.is_some_and(|s| s != cs) {
                continue;
            }
            // timercmp(>): the first of equals stays.
            if best.is_none_or(|(_, a)| c.activity > a) {
                best = Some((id, c.activity));
            }
        }
        best.map(|(id, _)| id)
    }

    /// `cmd_find_current_client` (cmd-find.c:1278-1305) without its error:
    /// the calling client when attached, else the best client of the session
    /// the command runs in.
    pub fn target_client(&self, client: Option<ClientId>, origin: Origin) -> Option<ClientId> {
        if let Some(c) = client.filter(|&c| self.client_session(c).is_some()) {
            return Some(c);
        }
        let s = crate::target::current(&self.model, origin)?.session?;
        self.best_client(Some(s))
    }

    /// `cmd_find_client` (cmd-find.c:1308-1349) for a `-c` / `-t` client
    /// name: the first attached client, in connection order, whose name
    /// or tty is `target` (a trailing `:` dropped, `/dev/` optional).
    /// `None` is the current client ([`Self::target_client`]).
    pub fn find_client(
        &self,
        client: Option<ClientId>,
        origin: Origin,
        target: Option<&str>,
    ) -> Option<ClientId> {
        let Some(target) = target else {
            return self.target_client(client, origin);
        };
        let target = target.strip_suffix(':').unwrap_or(target);
        self.clients.iter().find_map(|(&id, c)| {
            c.attached?;
            let tty = c
                .hello
                .as_ref()
                .and_then(|h| h.tty.as_ref())
                .map_or("", |t| t.name.as_str());
            crate::model::ClientTarget::matches(&self.client_name(id), tty, target).then_some(id)
        })
    }

    /// Refresh [`crate::model::Model::client_targets`] from the attached
    /// clients, so a target's session part can name one.
    pub fn refresh_client_targets(&mut self) {
        let targets = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                Some(crate::model::ClientTarget {
                    name: self.client_name(id),
                    tty: c
                        .hello
                        .as_ref()
                        .and_then(|h| h.tty.as_ref())
                        .map(|t| t.name.clone())
                        .unwrap_or_default(),
                    session: c.attached?,
                })
            })
            .collect();
        self.model.client_targets = targets;
    }

    /// A format context over `found` with `client` as `ft->c`.
    pub fn format_ctx(&self, found: crate::target::Found, client: Option<ClientId>) -> Ctx<'_> {
        let mut ctx = Ctx::new(&self.model, &self.info, &self.global_env, found);
        // cmdq_merge_formats: a hook's commands see its formats.
        ctx.extra.extend(
            self.cmd_formats
                .iter()
                .map(|(k, v)| (k.clone().into(), v.clone())),
        );
        ctx.clients = self.client_views();
        ctx.client = client.and_then(|id| Some(self.client_view(id, self.clients.get(&id)?)));
        // format_create(cmdq_get_client(item), ...): the running command's
        // client owns the jobs, whatever client the formats describe.
        ctx.jobs = Some(self.job_handle(self.cmd_client));
        ctx.old_layouts = self.old_layouts(self.cmd_client);
        ctx
    }

    /// `client` is a control client without `new-layouts`, which is dumped
    /// v1 layouts (`LAYOUT_CUSTOM_OLD_FORMAT`, format.c:874-877).
    pub fn old_layouts(&self, client: Option<ClientId>) -> bool {
        client
            .and_then(|id| self.clients.get(&id))
            .is_some_and(|c| c.control.is_some() && !c.flags.new_layouts)
    }

    /// Most recent attached client (`cmd_find_current_client`, M0 one client).
    pub fn current_client(&self) -> Option<ClientId> {
        self.clients
            .iter()
            .rev()
            .find_map(|(&id, c)| c.attached.map(|_| id))
    }

    // ------------------------------------------------------------- config

    /// cfg.c `start_cfg` (cfg.c:64-91): the files' commands go on the
    /// global queue with no client, parsed for `first`, the client that
    /// started the server (`pi.c`, attached or not), so `%if` conditions
    /// see it; errors become causes `file:line: message`. `first` waits
    /// for them (`cfg_client_done`) before its own command.
    fn start_cfg(&mut self, first: ClientId, origin: Origin) {
        use crate::cmdq::{End, NewBatch, Work};
        self.config_loaded = true;
        self.cfg_client = Some(first);
        let cwd = self
            .clients
            .get(&first)
            .and_then(|c| c.hello.as_ref())
            .map(|h| h.cwd.clone())
            .unwrap_or_default();
        let mut wait = NewBatch::new(Item::new(Some(first), cwd.clone()), Vec::new(), End::Quiet);
        wait.before.push(Work::CfgClientWait);
        self.cmdq_append(Some(first), wait);
        let opts = commands::source_file::LoadOpts {
            quiet: self.config.config_quiet,
            parse_client: Some((first, origin)),
            ..Default::default()
        };
        let mut item = Item::new(None, cwd);
        let mut groups = Vec::new();
        for file in self.config.config_files.clone() {
            groups.extend(commands::source_file::parse(self, &mut item, &file, opts));
        }
        let mut batch = NewBatch::new(item, Vec::new(), End::Config);
        batch.groups = groups;
        self.cmdq_append(None, batch);
    }

    /// `prompt_find_history_file` for the `history-file` option.
    fn history_file(&self) -> Option<PathBuf> {
        let option = self
            .model
            .options
            .get(TreeId::Server, "history-file")
            .map(|e| e.value.to_text(e.def, None, false))?;
        crate::prompt_history::history_path(&option, find_home(&self.global_env).as_deref())
    }

    /// `prompt_load_history` (prompt-history.c:79-106), once the config has
    /// run.
    fn prompt_load_history(&mut self) {
        let limit = usize::try_from(
            self.model
                .number_option(TreeId::Server, "prompt-history-limit"),
        )
        .unwrap_or(0);
        if let Some(path) = self.history_file() {
            self.prompt_history.load(&path, limit);
        }
    }

    /// `cfg_done` (cfg.c:47-62): the config has run; the first client's
    /// command may go.
    pub(crate) fn cfg_done(&mut self) {
        if self.cfg_finished {
            return;
        }
        self.cfg_finished = true;
        self.prompt_load_history();
        self.cfg_show_causes(None);
        if let Some(step) = self.cfg_wait.take() {
            self.cmdq_continue(step);
        }
    }

    // --------------------------------------------------------------- panes

    /// `default-terminal`, the `TERM` panes and jobs get.
    pub(crate) fn default_terminal(&self) -> String {
        self.model
            .options
            .get(TreeId::Server, "default-terminal")
            .map_or_else(
                || "tmux-256color".to_owned(),
                |e| e.value.to_text(e.def, None, false),
            )
    }

    /// Create a session with one window and one pane.
    pub fn create_session(&mut self, spec: NewSession) -> io::Result<SessionId> {
        let prefix = spec.group.as_ref().map(|g| g.name.as_str());
        // session_create (session.c:135-147): a given name keeps one ID;
        // otherwise the ID, after the group's name and a dash, with a new
        // ID for each name already taken.
        let (id, name) = if let Some(n) = spec.name.clone() {
            (self.model.session_ids.allocate(), n)
        } else {
            loop {
                let id = self.model.session_ids.allocate();
                let name = match prefix {
                    Some(p) => format!("{p}-{}", id.0),
                    None => id.0.to_string(),
                };
                if self.model.session_named(&name).is_none() {
                    break (id, name);
                }
            }
        };
        // cmd-new-session.c:282-283: update-environment from the global
        // session options, then `-e`.
        let mut env = Environ::default();
        if spec.update_env {
            let patterns = self.update_environment(TreeId::GlobalSession);
            crate::environ::update(
                &patterns,
                &Environ::from_pairs(spec.client_env.clone()),
                &mut env,
            );
        }
        for e in &spec.extra_env {
            if let Some((k, v)) = e.split_once('=') {
                env.set(k, v, false);
            }
        }
        let window_id = self.model.window_ids.allocate();
        let pane_id = self.model.pane_ids.allocate();
        let (cols, rows) = (u32::from(spec.cols), u32::from(spec.rows));
        let pane = self.spawn_pane(&PaneSpawn {
            pane: pane_id,
            window: window_id,
            session: id,
            session_env: &env,
            extra_env: &[],
            client_path: client_path(&spec.client_env),
            argv: spec.argv,
            cwd: spec.cwd.clone(),
            rect: Geometry::new(cols, rows, 0, 0),
            shell: None,
            empty: false,
        })?;
        self.insert_window(window_id, pane, spec.window_name, cols, rows, None);
        // spawn_window with idx -1: the first free index from base-index
        // (spawn.c:183-185); the session is new, so that is base-index.
        let index = u32::try_from(
            self.model
                .number_option(TreeId::GlobalSession, "base-index"),
        )
        .unwrap_or(0);
        let activity = self.model.touch();
        self.model.insert_session(Session {
            id,
            name,
            created: now_epoch(),
            activity,
            activity_secs: now_epoch(),
            cwd: spec.cwd,
            windows: BTreeMap::from([(index, window_id)]),
            current: index,
            last: Vec::new(),
            env,
            link_alerts: BTreeMap::new(),
            link_seq: BTreeMap::new(),
            link_tree: {
                let mut t = crate::model::rb::Tree::default();
                t.insert(index);
                t
            },
            last_attached: None,
        });
        if let Some((x, y)) = spec.default_size {
            // cmd-new-session.c:274-280: -x/-y become the session's
            // default-size, which later windows use.
            self.model.options.set_table_value(
                TreeId::Session(id),
                "default-size",
                Value::String(format!("{x}x{y}")),
            );
        }
        // spawn_window (spawn.c:233-235), then cmd-new-session.c:328.
        self.fire_spawned(id, index, window_id, false);
        if let Some(g) = spec.group {
            // cmd-new-session.c:317-326.
            if let Some(with) = g.with {
                self.model.group_add(&g.name, with);
            }
            self.model.group_add(&g.name, id);
            self.model.group_sync_to(id);
            if let Some(first) = self.model.sessions[&id].windows.keys().next().copied() {
                self.model.set_current(id, first);
            }
        }
        self.model.fire_session("session-created", id, true);
        Ok(id)
    }

    /// The events of `spawn_window` / `spawn_pane` once the new window is
    /// linked (spawn.c:79-110, 233-235): pane-created, window-created,
    /// window-linked.
    pub(crate) fn fire_spawned(&mut self, s: SessionId, index: u32, w: WindowId, respawn: bool) {
        let Some(p) = self.model.windows.get(&w).map(Window::active) else {
            return;
        };
        self.fire_pane_created(s, index, p, false, respawn);
        if !respawn {
            self.model.fire_window("window-created", w);
            self.model.fire_winlink("window-linked", s, index, w);
        }
    }

    /// `spawn_fire_pane_created` (spawn.c:73-111).
    pub(crate) fn fire_pane_created(
        &mut self,
        s: SessionId,
        index: u32,
        p: PaneId,
        empty: bool,
        respawn: bool,
    ) {
        let m = &self.model;
        let Some(pane) = m.panes.get(&p) else { return };
        let w = pane.window;
        let cmd = crate::names::stringify_argv(&pane.argv);
        let command = if cmd.is_empty() {
            pane.shell.to_string_lossy().into_owned()
        } else {
            cmd
        };
        let cwd = pane.cwd.to_string_lossy().into_owned();
        let ev = crate::events::Event::new("pane-created")
            .target(crate::target::Found {
                pane: Some(p),
                ..crate::target::Found::from_winlink(m, s, index)
            })
            .session(m, "session", s)
            .window(m, "window", w)
            .int("window_index", index)
            .pane("pane", p)
            .text("pane_command", command)
            .text("pane_current_path", cwd)
            .int("created_empty", i64::from(empty))
            .int("created_respawn", i64::from(respawn));
        self.model.push_event(ev);
    }

    /// The window half of `spawn_window` (spawn.c:183-241) once its first
    /// pane exists: the name (`default_window_name`, or the given one with
    /// `automatic-rename` turned off in the window's options), the option
    /// trees, the layout, and the pane marked changed as
    /// `window_set_active_pane` does, so automatic-rename looks at it.
    fn insert_window(
        &mut self,
        window_id: WindowId,
        mut pane: Pane,
        name: Option<String>,
        cols: u32,
        rows: u32,
        latest: Option<ClientId>,
    ) {
        let pane_id = pane.id;
        self.model.options.add_window(window_id);
        self.model.options.add_pane(pane_id, window_id);
        let name = if let Some(n) = name {
            self.model.options.set_table_value(
                TreeId::Window(window_id),
                "automatic-rename",
                Value::Flag(false),
            );
            n
        } else {
            // default_window_name (names.c:107-121).
            let cmd = crate::names::stringify_argv(&pane.argv);
            let from = if cmd.is_empty() {
                pane.shell.to_string_lossy().into_owned()
            } else {
                cmd
            };
            parse_window_name(&from)
        };
        pane.changed = true;
        // tmux's first check_window_name runs the turn it forked, when
        // #{pane_current_command} is still the forked server ("tmux") or
        // the shell before it execs: a race that goes differently every
        // run. The name clock starts at creation instead, so the first
        // check comes one NAME_INTERVAL later and sees the command (D-012).
        pane.last_rename = Some(Instant::now());
        self.model.panes.insert(pane_id, pane);
        // spawn.c:601-609: the first pane goes through window_set_active_pane
        // (w->active is NULL even with -d), which stamps its active_point.
        self.model.stamp_active(pane_id);
        let activity = self.model.touch();
        self.model.windows.insert(
            window_id,
            Window {
                id: window_id,
                name,
                activity,
                activity_secs: now_epoch(),
                layout: CoreLayout::new(cols, rows, pane_id),
                panes: WindowPanes::new(pane_id),
                zoom: None,
                was_zoomed: None,
                last_layout: None,
                old_layout: None,
                pending_size: None,
                manual_size: (cols, rows),
                latest,
                alerts: crate::alerts::WindowAlerts::default(),
                menu: None,
                menu_last: (0, 0),
            },
        );
        // window_create -> window_update_activity.
        self.model.alerts_queue(window_id, crate::alerts::ACTIVITY);
        self.model.fix_panes(window_id);
    }

    /// `spawn_window` (spawn.c:79-241) for `new-window`: a window with one
    /// pane linked into an existing session at `index` (or the first free
    /// one from `base-index`), replacing the window there with `kill`, and
    /// made current unless `detached`. Errors are `spawn_window`'s causes.
    pub fn new_window(&mut self, nw: &NewWindow<'_>) -> Result<u32, String> {
        let s = nw.session;
        let mut detached = nw.detached;
        if let Some(index) = nw.index
            && self.model.sessions[&s].windows.contains_key(&index)
        {
            if !nw.kill {
                return Err(format!("index {index} in use"));
            }
            // spawn.c:162-180: unlink without session_detach; replacing the
            // current window always selects the new one.
            if self.model.sessions[&s].current == index {
                detached = false;
            }
            for pane in self.model.unlink_index(s, index) {
                self.close_pane_io(pane);
            }
        }
        let index = if let Some(i) = nw.index {
            i
        } else {
            let base = self.model.number_option(TreeId::Session(s), "base-index");
            let base = u32::try_from(base).unwrap_or(0);
            self.model
                .next_free_index(s, base)
                .ok_or_else(|| format!("couldn't add window -{}", i64::from(base) + 1))?
        };
        let (cols, rows) = self.default_window_size(nw.client, s, None, None);
        let window_id = self.model.window_ids.allocate();
        let pane_id = self.model.pane_ids.allocate();
        let session_env = self.model.sessions[&s].env.clone();
        let pane = self
            .spawn_pane(&PaneSpawn {
                pane: pane_id,
                window: window_id,
                session: s,
                session_env: &session_env,
                extra_env: nw.extra_env,
                client_path: nw.client_path,
                argv: nw.argv.clone(),
                cwd: nw.cwd.clone(),
                rect: Geometry::new(cols, rows, 0, 0),
                shell: None,
                empty: nw.empty,
            })
            .map_err(|e| spawn_failed(&e))?;
        self.insert_window(window_id, pane, nw.name.clone(), cols, rows, nw.client);
        let session = self.model.sessions.get_mut(&s).expect("target session");
        session.windows.insert(index, window_id);
        session.sync_link_tree();
        if !session.windows.contains_key(&session.current) {
            session.current = index;
        }
        // spawn_pane fires pane-created; spawn_window selects, then fires
        // window-created and window-linked.
        let p = self.model.windows[&window_id].active();
        self.fire_pane_created(s, index, p, nw.empty, false);
        if !detached {
            self.model.set_current(s, index);
        }
        self.model.fire_window("window-created", window_id);
        self.model
            .fire_winlink("window-linked", s, index, window_id);
        // spawn.c:238.
        self.model.group_sync_from(s);
        Ok(index)
    }

    /// `server_kill_window` (server-fn.c:239-262) with the PTYs closed and
    /// control clients told.
    pub fn kill_window(&mut self, w: WindowId, renumber: bool) {
        let panes = self.model.kill_window(w, renumber);
        for pane in panes {
            self.close_pane_io(pane);
        }
        self.recalculate_sizes();
    }

    /// The new pane's environment (`environ_for_session` plus the spawn
    /// additions, spawn.c:400-424), before `SHELL`.
    fn pane_environ(&self, ps: &PaneSpawn<'_>) -> Environ {
        let mut child = self.global_env.clone();
        ps.session_env.copy_into(&mut child);
        // environ_for_session (environ.c:243-250): TERM from default-terminal.
        child.set("TERM", &self.default_terminal(), false);
        child.set("TERM_PROGRAM", "tmux", false);
        child.set("TERM_PROGRAM_VERSION", "3.8", false);
        child.set("COLORTERM", "truecolor", false);
        child.set(
            "TMUX",
            &format!(
                "{},{},{}",
                self.socket.display(),
                std::process::id(),
                ps.session.0
            ),
            false,
        );
        for e in ps.extra_env {
            if let Some((k, v)) = e.split_once('=') {
                child.set(k, v, false);
            }
        }
        child.set("TMUX_PANE", &ps.pane.to_string(), false);
        // spawn.c:418-424: an unattached client's PATH replaces the
        // session's, so `tmux new myprogram` finds myprogram.
        if let Some(path) = ps.client_path {
            child.set("PATH", path, false);
        }
        child.set("PATH", &self.pane_path(child.get("PATH")), false);
        child
    }

    /// `spawn_pane` (spawn.c:216-617) for a new pane: the child environment
    /// (`environ_for_session` plus the spawn additions), the shell from
    /// `default-shell`, the PTY sized to `rect`, and a terminal whose title
    /// starts as the host name (window.c:1479-1480). The caller links the
    /// returned pane into the model.
    #[expect(clippy::too_many_lines, reason = "one line per field of the new pane")]
    pub fn spawn_pane(&mut self, ps: &PaneSpawn<'_>) -> io::Result<Pane> {
        let mut child = self.pane_environ(ps);
        // The shell and history budget come from the session's options,
        // not the starting client's shell.
        let session_tree = TreeId::Session(ps.session);
        let shell = if let Some(kept) = &ps.shell {
            kept.clone()
        } else {
            self.default_shell(session_tree)
        };
        // spawn.c:379-390: with no command and not respawning, the
        // session's `default-command` is the one word `$SHELL -c` runs.
        // A respawn keeps the pane's stored words.
        let mut argv = ps.argv.clone();
        if argv.is_empty() && ps.shell.is_none() {
            let cmd = self
                .model
                .options
                .get(session_tree, "default-command")
                .map(|e| e.value.to_text(e.def, None, false))
                .unwrap_or_default();
            if !cmd.is_empty() {
                argv.push(cmd);
            }
        }
        child.set("SHELL", &shell.to_string_lossy(), false);

        let cols = u16::try_from(ps.rect.sx).unwrap_or(u16::MAX).max(1);
        let rows = u16::try_from(ps.rect.sy).unwrap_or(u16::MAX).max(1);
        // spawn.c:458-464: an empty pane forks nothing.
        let spawned = if ps.empty {
            None
        } else {
            Some(sys::pty::spawn(&sys::pty::SpawnSpec {
                program: sys::pty::Program::Pane(argv.clone()),
                shell: shell.clone(),
                cwd: ps.cwd.clone(),
                home: find_home(&self.global_env),
                env: child.for_process(),
                cols,
                rows,
            })?)
        };
        let history_limit =
            usize::try_from(self.model.number_option(session_tree, "history-limit")).unwrap_or(0);
        let mut term = PaneTerm::new(cols, rows, history_limit);
        term.set_colour_names(vtmux_core::colour::x11_rgb);
        // screen_init: the new screen's key mode follows `extended-keys`.
        term.set_extended_keys(self.model.extended_keys());
        term.reset_key_mode();
        let host = &self.info.host;
        if !host.chars().any(char::is_control) {
            term.feed(format!("\x1b]2;{host}\x1b\\").as_bytes());
            // screen_init sets the title directly: no event.
            term.take_events();
        }
        if ps.empty {
            empty_pane_modes(&mut term);
        }
        let (io, pid, tty) = match spawned {
            Some(spawned) => {
                let tty = sys::pty::tty_name(&spawned.master).unwrap_or_default();
                let io = PaneIo::new(spawned.master);
                io.register(self.poll.registry(), Token(PANE_BASE + ps.pane.0 as usize))?;
                (Some(io), spawned.pid.as_raw_nonzero().get(), tty)
            }
            None => (None, 0, String::new()),
        };
        Ok(Pane {
            id: ps.pane,
            window: ps.window,
            term,
            io,
            empty: ps.empty,
            wait_step: None,
            editor: None,
            pid,
            tty,
            argv,
            shell,
            cwd: ps.cwd.clone(),
            exited: false,
            status: None,
            dead_time: None,
            changed: false,
            unseen_changes: false,
            output_at: None,
            last_rename: None,
            rect: ps.rect,
            history_limit,
            input_off: false,
            focused: false,
            active_point: 0,
            resizes: Vec::new(),
            resize_timer: None,
            modes: Vec::new(),
            search: None,
            prompt: None,
            prompt_owner: None,
            mode_prompt: None,
            mode_prompt_owner: None,
            compress: crate::compress::PaneCompress::default(),
            pipe: None,
            sb: crate::scrollbar::AutoHide::default(),
            over_zoom: false,
            capture_keys: false,
            close_on_click: false,
            close_on_cancel: false,
            requests: crate::term_queries::PaneRequests::default(),
            output_generation: 0,
            shell_commands: crate::model::ShellCommands::default(),
        })
    }

    /// `server_destroy_session` + `session_destroy`: its panes' processes
    /// get SIGHUP when their PTYs close.
    pub fn destroy_session(&mut self, s: SessionId) {
        let panes = self.model.remove_session(s);
        for pane in panes {
            self.close_pane_io(pane);
        }
        self.close_dropped();
    }

    /// `server_check_unattached` (server-fn.c:529-560), after a client
    /// leaves or changes session: every session with no attached client
    /// whose `destroy-unattached` says so goes, in name order, each
    /// judged once the ones before it are gone (`keep-last` keeps the
    /// last of a group, `keep-group` a session alone in its group).
    pub(crate) fn check_unattached(&mut self) {
        let names: Vec<SessionId> = self.model.by_name.values().copied().collect();
        for s in names {
            if !self.model.sessions.contains_key(&s) || self.attached_sessions().contains(&s) {
                continue;
            }
            let group = self.model.groups.members_of(s).map(<[_]>::len);
            let destroy = match self
                .model
                .number_option(TreeId::Session(s), "destroy-unattached")
            {
                1 => true,
                2 => group.is_some_and(|n| n > 1),
                3 => group != Some(1),
                _ => false,
            };
            if destroy {
                self.destroy_session(s);
                self.drain_events();
            }
        }
    }

    /// `server_destroy_session` (server-fn.c:486-527): the clients of a
    /// session that went move to the session `detach-on-destroy` picks, or
    /// are left to exit. A session it picks that went in the same command
    /// (a group destroyed together) is only noted: that session's own
    /// destruction moves them on again.
    pub(crate) fn session_destroyed(&mut self, d: &crate::events::Destroyed) {
        let attached = self.attached_sessions();
        let activity =
            |(s, a): (SessionId, u64)| self.model.sessions.get(&s).map_or(a, |x| x.activity);
        // server_find_session: the most recently active session but this
        // one, the first in name order on a tie.
        let newest = |detached_only: bool| {
            let mut out: Option<(SessionId, u64)> = None;
            for &(s, a) in &d.sessions {
                let a = activity((s, a));
                if s == d.session || (detached_only && attached.contains(&s)) {
                    continue;
                }
                if out.is_none_or(|(_, b)| a > b) {
                    out = Some((s, a));
                }
            }
            out.map(|(s, _)| s)
        };
        // session_previous_session / session_next_session by name.
        let step = |forward: bool| {
            let n = d.sessions.len();
            let i = d.sessions.iter().position(|(s, _)| *s == d.session)?;
            let j = if forward {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            };
            Some(d.sessions[j].0).filter(|s| *s != d.session)
        };
        let s_new = match d.mode {
            0 => newest(false),
            2 => newest(true),
            3 => step(false),
            4 => step(true),
            _ => None,
        };
        let cs_new = if s_new.is_none() && matches!(d.mode, 1 | 2) {
            newest(false)
        } else {
            None
        };
        let ids: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.attached == Some(d.session))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let use_s = s_new.or(cs_new.filter(|_| self.clients[&id].flags.no_detach_on_destroy));
            let Some(to) = use_s else { continue };
            let c = self.clients.get_mut(&id).expect("listed above");
            // c->session = NULL, c->last_session = NULL first.
            c.attached = None;
            c.last_session = None;
            if self.model.sessions.contains_key(&to) {
                self.switch_session(id, to);
            } else {
                self.clients.get_mut(&id).expect("listed above").attached = Some(to);
            }
        }
    }

    /// `server_destroy_pane` with remain-on-exit off: hang up the pane,
    /// take it out of its window; the window and an emptied session go
    /// with the last pane.
    pub fn destroy_pane(&mut self, p: PaneId) {
        self.hangup_pane(p);
        let panes = self.model.remove_pane(p);
        for pane in panes {
            self.close_pane_io(pane);
        }
    }

    /// `server_destroy_pane(wp, 1)` for a pane whose program went
    /// ([`Self::destroy_exited_pane`], remain-on-exit aside).
    pub(crate) fn pane_exited(&mut self, p: PaneId) {
        self.destroy_exited_pane(p, true);
    }

    /// `server_fire_pane_exit` (server-fn.c:35-56) once the status is in
    /// (`PANE_STATUSREADY`): kill-pane and friends fire nothing.
    pub(crate) fn fire_pane_exit(&mut self, name: &str, p: PaneId) {
        let Some(pane) = self.model.panes.get(&p) else {
            return;
        };
        let Some(status) = pane.status else { return };
        let w = pane.window;
        let mut ev = crate::events::Event::new(name)
            .target(crate::target::Found::from_pane(&self.model, p))
            .pane("pane", p)
            .window(&self.model, "window", w);
        ev = match status {
            crate::jobs::JobStatus::Exited(code) => ev.int("exit_status", code),
            crate::jobs::JobStatus::Signaled(sig) => ev.text("exit_signal", sig_name(sig)),
            crate::jobs::JobStatus::Unknown => ev,
        };
        ev = ev.int("exit_success", i64::from(status.success()));
        self.model.push_event(ev);
    }

    /// `respawn-pane` (spawn.c:312-345 with `SPAWN_RESPAWN|SPAWN_KILL`):
    /// close the pane's PTY (its process gets SIGHUP), reset its screen
    /// (`screen_reinit`: history and title stay) and start `ps` in it. The
    /// pane keeps its ID, place and terminal; the old process's exit no
    /// longer matches the pane, which now has the new pid.
    pub fn respawn_pane(&mut self, ps: &PaneSpawn<'_>) -> io::Result<()> {
        let p = ps.pane;
        self.hangup_pane(p);
        // spawn.c:343-346: output the control clients still had queued
        // from the old process goes.
        self.control_reset_pane(p);
        // spawn.c:328: the pane leaves its modes.
        self.reset_mode_all(p);
        if !self.model.panes.contains_key(&p) {
            return Ok(());
        }
        let fresh = self.spawn_pane(ps);
        let extended_keys = self.model.extended_keys();
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return fresh.map(|_| ());
        };
        // screen_reinit and a fresh input parser: alternate screen off,
        // scroll region, cursor home, visible lines cleared, modes back to
        // cursor+wrap, default attributes, default tabs (DECST8C,
        // `screen_reset_tabs`). input_free ends
        // synchronized output (input.c:922).
        pane.term.reset_input();
        pane.term.stop_sync();
        // grid_clear_lines: an erase, never into history (scroll-on-clear
        // is for the program's clears).
        pane.term.set_scroll_on_clear(false);
        // input_free: the old program's requests go.
        pane.requests.clear();
        // The cursor goes home first (screen.c:111-117; DECSTBM and DECOM
        // home it too, so they come before), then screen_alternate_off(s,
        // NULL, 0): no cursor restored, but its resize back to the pane's
        // size moves the homed one (a taller pane pulls history back above
        // it), and nothing homes it again; then saved_cx and saved_cy go
        // (screen.c:122-125). MODE_CRLF stays (screen.c:117): a pane once
        // empty keeps it.
        pane.term.feed(
            b"\x1b[r\x1b[?6l\x1b[H\x1b[?47l\x1b[m\x1b[2J\x1b[?7h\x1b[?25h\x1b[?1l\x1b>\x1b[4l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[?2031l",
        );
        let _ = pane.term.take_replies();
        // Default tab stops (DECST8C), past the scanner: it drops the
        // sequence as one tmux does not know from a program.
        pane.term.reset_tab_stops();
        // The mode bits the terminal does not hold: blinking and the very
        // visible cursor (the shape stays).
        pane.term.reinit_cursor_modes();
        pane.term.forget_alternate_saved();
        // screen_reinit (spawn.c:329) under the current `extended-keys`.
        pane.term.set_extended_keys(extended_keys);
        pane.term.reset_key_mode();
        if ps.empty {
            empty_pane_modes(&mut pane.term);
        }
        pane.compress.observe(&pane.term, Instant::now());
        // spawn.c:349, 587: the old status and the dead pane's line go
        // with the old process.
        pane.exited = false;
        pane.status = None;
        pane.dead_time = None;
        pane.changed = true;
        let fresh = match fresh {
            Ok(fresh) => fresh,
            Err(e) => {
                // spawn.c:478-488: a failed fork leaves the pane in place
                // with its new command and directory and no process (pid
                // -1): the old process's exit matches no pane, so the pane
                // stays, not dead, until a respawn or kill-pane.
                pane.pid = 0;
                pane.empty = false;
                if !ps.argv.is_empty() {
                    pane.argv.clone_from(&ps.argv);
                }
                pane.cwd.clone_from(&ps.cwd);
                return Err(e);
            }
        };
        pane.io = fresh.io;
        pane.empty = fresh.empty;
        // spawn.c:459-463: an empty respawn forks nothing and keeps the old
        // pid, so the old process's exit (server_child_exited matches by
        // pid, server.c:498) still destroys the pane.
        if !fresh.empty {
            pane.pid = fresh.pid;
            pane.tty = fresh.tty;
        }
        pane.argv = fresh.argv;
        pane.shell = fresh.shell;
        pane.cwd = fresh.cwd;
        Ok(())
    }

    /// `window_pane_wait_finish` (window.c:1486-1507): the `split-window
    /// -W` waiting on the pane goes on, an unattached client exiting with
    /// the process's status (128 + SIGHUP when it has none yet: the pane was
    /// killed).
    fn pane_wait_finish(&mut self, pane: &mut Pane) {
        let Some(step) = pane.wait_step.take() else {
            return;
        };
        let retval = match pane.status {
            Some(crate::jobs::JobStatus::Exited(code)) => code,
            Some(crate::jobs::JobStatus::Signaled(sig)) => 128 + sig,
            None | Some(crate::jobs::JobStatus::Unknown) => 128 + libc::SIGHUP,
        };
        self.cmdq_with_item(step, |server, item| {
            if item
                .client
                .is_some_and(|c| server.client_session(c).is_none())
            {
                item.status = retval;
            }
        });
        self.cmdq_continue(step);
    }

    pub(crate) fn close_pane_io(&mut self, mut pane: Pane) {
        // window_pane_destroy: window_pane_wait_finish and
        // spawn_editor_finish first.
        self.pane_wait_finish(&mut pane);
        if let Some(editor) = pane.editor.take() {
            self.editor_finish(editor, pane.status);
        }
        // window_pane_destroy: window_pane_clear_prompt frees the prompt's
        // owner, with no pane-prompt-closed (PANE_DESTROYED).
        if let Some(owner) = pane.prompt_owner.take() {
            owner.free(self);
        }
        if let Some(owner) = pane.mode_prompt_owner.take() {
            owner.free(self);
        }
        if let Some(pipe) = pane.pipe.take() {
            self.pipe_drop(pipe);
        }
        if let Some(io) = pane.io.take() {
            io.deregister(self.poll.registry());
            // window_pane_destroy closes the master; the kernel sends the
            // session leader SIGHUP. Sending it ourselves too covers a
            // child that has not set up its controlling terminal yet.
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_raw(pane.pid).expect("pane pid is positive"),
                rustix::process::Signal::HUP,
            );
            drop(io);
        }
    }

    /// Close the panes of windows a group synchronisation destroyed
    /// ([`crate::model::Model::group_sync1`]).
    pub(crate) fn close_dropped(&mut self) {
        for pane in std::mem::take(&mut self.model.dropped) {
            self.close_pane_io(pane);
        }
    }

    pub(crate) fn hangup_pane(&mut self, p: PaneId) {
        if let Some(pane) = self.model.panes.get_mut(&p)
            && let Some(io) = pane.io.take()
        {
            io.deregister(self.poll.registry());
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_raw(pane.pid).expect("pane pid is positive"),
                rustix::process::Signal::HUP,
            );
        }
    }

    /// Keys were queued for pane `p`: arm WRITABLE if they did not all go.
    pub fn pane_output_queued(&mut self, p: PaneId) {
        if let Some(io) = self.model.panes.get(&p).and_then(|x| x.io.as_ref()) {
            let _ = io.reregister(self.poll.registry(), Token(PANE_BASE + p.0 as usize));
        }
    }

    fn pane_writable(&mut self, p: PaneId) {
        if let Some(io) = self.model.panes.get_mut(&p).and_then(|x| x.io.as_mut()) {
            io.flush();
        }
        self.pane_output_queued(p);
    }

    /// `input_parse_buffer` (input.c:1044-1075): `bytes` from pane `p`'s
    /// program, or written to an empty pane (`display-message -I`), into
    /// its terminal, with what that sets off. `flooding`: more of a flood
    /// rather than the start of a burst.
    pub(crate) fn pane_input(&mut self, p: PaneId, bytes: &[u8], flooding: bool) {
        if bytes.is_empty() {
            return;
        }
        // DECRQSS / DECRQM 12 fall back to the pane's `cursor-style`
        // (input.c:1666-1680, 2590-2600).
        let cursor_style =
            u8::try_from(self.model.number_option(TreeId::Pane(p), "cursor-style")).unwrap_or(0);
        let input_buffer = self
            .model
            .number_option(TreeId::Server, "input-buffer-size");
        let extended_keys = self.model.extended_keys();
        let scroll_on_clear = self.model.number_option(TreeId::Pane(p), "scroll-on-clear") != 0;
        let redraw_pending = self.model.redraw_panes.contains(&p);
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        pane.term.set_cursor_style_option(cursor_style);
        pane.term.set_scroll_on_clear(scroll_on_clear);
        let before = (pane.term.title(), pane.term.pwd());
        let alternate = pane.term.alternate_on();
        pane.term
            .set_input_buffer_size(usize::try_from(input_buffer).unwrap_or(0));
        pane.term.set_extended_keys(extended_keys);
        pane.term.feed(bytes);
        // input.c:1065-1068: output to a pane not in a mode is written
        // through it, so its cursor moves arm the window's offset timer.
        let cursor_moved = pane.modes.is_empty();
        let passthrough = pane.term.take_passthrough();
        let renames = pane.term.take_renames();
        let now = Instant::now();
        pane.compress.observe(&pane.term, now);
        pane.output_at = Some(now);
        let entered_alternate = !alternate && pane.term.alternate_on();
        // input.c:2725-2740: a new title (OSC 0/2) or path
        // (OSC 7) redraws the status of the window's sessions.
        let retitled = (pane.term.title(), pane.term.pwd()) != before;
        let events = pane.term.take_events();
        let titled = events.iter().any(|e| matches!(e, TermEvent::Title(_)));
        pane.output_generation += 1;
        pane.changed = true;
        // input.c:1061: new input while in a mode.
        pane.unseen_changes |= !pane.modes.is_empty();
        let redraw = pane.term.take_redraw();
        let answers = pane.term.take_answers();
        let toggled_alternate = alternate != pane.term.alternate_on();
        if entered_alternate {
            pane.flush_resizes();
        }
        // input_parse_buffer -> window_update_activity on every
        // non-empty read (input.c:1053): W/t sorts and lsw -O
        // activity order by it.
        let window = pane.window;
        // input.c:1054-1056: pane-activity once a loop turn
        // (server_client_loop clears PANE_ACTIVITY), before what the
        // output itself fires.
        if self.activity_fired.insert(p) {
            self.fire_pane_often("pane-activity", p);
        }
        if cursor_moved {
            self.cursor_moved_in(window);
        }
        let stamp = self.model.touch();
        if let Some(w) = self.model.windows.get_mut(&window) {
            w.activity = stamp;
            w.activity_secs = crate::model::now_epoch();
        }
        self.model.alerts_queue(window, alerts::ACTIVITY);
        // screen_write_alternateon/off (screen-write.c:3162, 3191):
        // layout_fix_panes, so a reserved scrollbar comes and goes.
        if toggled_alternate && self.model.scrollbars(window) != crate::scrollbar::Scrollbars::Off {
            self.model.fix_panes(window);
            self.model.redraw(crate::model::Redraw::Window(window));
        }
        // ...and server_redraw_window_borders: a border style reading
        // #{alternate_on} is expanded again.
        if toggled_alternate {
            self.model
                .redraw(crate::model::Redraw::WindowBorders(window));
        }
        if !flooding {
            self.burst_started.insert(p);
        }
        if redraw {
            self.model.redraw_panes.insert(p);
        }
        if retitled || titled {
            self.model
                .redraw(crate::model::Redraw::WindowStatus(window));
        }
        for event in events {
            self.pane_term_event(p, window, event);
        }
        for name in renames {
            self.pane_rename(p, window, &name);
        }
        // An empty pane has no program to answer (pane_reply writes
        // nowhere).
        if !answers.is_empty() {
            self.pane_answers(p, answers);
        }
        self.control_output(p, bytes);
        self.pipe_write(p, bytes);
        self.pane_passthrough(p, passthrough, redraw_pending);
    }

    /// What pane `p`'s program did while its output was parsed, in order.
    fn pane_term_event(&mut self, p: PaneId, window: WindowId, event: TermEvent) {
        let now = crate::model::now_epoch();
        let commands = self.model.panes.get_mut(&p).map(|x| &mut x.shell_commands);
        match event {
            // input_fire_pane_title_changed (input.c:795-809): one event
            // per OSC 0/2 and per title stack pop.
            TermEvent::Title(title) => {
                let ev = crate::events::Event::new("pane-title-changed")
                    .target(crate::target::Found::from_pane(&self.model, p))
                    .pane("pane", p)
                    .window(&self.model, "window", window)
                    .text("new_title", title);
                self.model.push_event(ev);
            }
            // input_c0_dispatch BEL (input.c:1322-1326): pane-bell for
            // each, whatever monitor-bell says, then alerts_queue.
            TermEvent::Bells(n) => {
                for _ in 0..n {
                    self.model.fire_pane("pane-bell", p);
                }
                self.model.alerts_queue(window, alerts::BELL);
            }
            // input_osc_133 (input.c:3289-3328).
            TermEvent::Prompt => {
                if let Some(c) = commands {
                    c.last_prompt = Some(now);
                }
                self.model.fire_pane("pane-shell-prompt", p);
            }
            TermEvent::CommandStarted => {
                if let Some(c) = commands {
                    c.start(now);
                }
                self.fire_command_event("pane-command-started", p, window, now);
            }
            TermEvent::CommandFinished(status) => {
                if let Some(c) = commands {
                    c.finish(now, status);
                }
                self.fire_command_event("pane-command-finished", p, window, now);
            }
        }
    }

    /// `input_fire_command_event` (input.c:3224-3262): the pane event
    /// with the session and window index it is found in, and what OSC 133
    /// said of the command.
    fn fire_command_event(&mut self, name: &str, p: PaneId, window: WindowId, now: i64) {
        let Some(c) = self.model.panes.get(&p).map(|x| x.shell_commands) else {
            return;
        };
        let fs = crate::target::Found::from_pane(&self.model, p);
        let mut ev = crate::events::Event::new(name).target(fs);
        if let Some(s) = fs.session {
            ev = ev.session(&self.model, "session", s);
        }
        if let Some(i) = fs.index {
            ev = ev.int("window_index", i);
        }
        ev = ev.window(&self.model, "window", window).pane("pane", p);
        if let Some(status) = c.status {
            ev = ev.int("command_status", status);
        }
        if let Some(t) = c.started {
            ev = ev.int("command_start_time", t);
        }
        if let Some(t) = c.ended {
            ev = ev.int("command_end_time", t);
        }
        if let Some(d) = c.duration(now) {
            ev = ev.int("command_duration", d);
        }
        self.model.push_event(ev);
    }

    /// `server_client_check_pane_buffer`'s end (server-client.c:2113-2124),
    /// each loop turn: a pane is read unless only control clients that
    /// are behind would get its output, so its program blocks on its
    /// terminal instead of the server holding what it writes.
    fn pane_reads(&mut self) {
        let panes: Vec<(PaneId, WindowId)> = self
            .model
            .panes
            .iter()
            .filter(|(_, x)| x.io.is_some())
            .map(|(&p, x)| (p, x.window))
            .collect();
        for (p, w) in panes {
            let on = !self.control_holds_pane(p, w);
            let Some(io) = self.model.panes.get(&p).and_then(|x| x.io.as_ref()) else {
                continue;
            };
            if io.set_reading(on) {
                let _ = io.reregister(self.poll.registry(), Token(PANE_BASE + p.0 as usize));
                if on {
                    // What arrived while it was not read.
                    self.backlog.insert(p);
                }
            }
        }
    }

    /// `input_exit_rename` (input.c:2825-2855): `ESC k name ST` from pane
    /// `p` names its window `w`, if the pane's `allow-rename` lets it and
    /// the name is valid UTF-8 (printable ASCII). A name turns
    /// `automatic-rename` off on the window; an empty one unsets it there
    /// and empties the name unless the option is still on. Either way the
    /// name is cleaned as a program's (`window_set_name(w, name, 1)`).
    fn pane_rename(&mut self, p: PaneId, w: WindowId, name: &[u8]) {
        if self.model.number_option(TreeId::Pane(p), "allow-rename") == 0 {
            return;
        }
        let name = vtmux_core::rawstr::from_bytes(name);
        if !crate::names::check_name(&name) {
            return;
        }
        let tree = TreeId::Window(w);
        if name.is_empty() {
            self.model.options.unset(tree, "automatic-rename", None);
            if self.model.number_option(tree, "automatic-rename") == 0 {
                self.model.set_window_name(w, "");
            }
        } else {
            self.model
                .options
                .set_table_value(tree, "automatic-rename", Value::Flag(false));
            self.model
                .set_window_name(w, &crate::names::clean_untrusted(&name));
        }
        // server_redraw_window_borders, server_status_window.
        self.model.redraw(crate::model::Redraw::Window(w));
        self.model.redraw(crate::model::Redraw::WindowStatus(w));
    }

    /// Read a bounded amount from pane `p` (tmux reads once per libevent
    /// callback), so a pane flooding output cannot starve clients.
    /// `flooding`: the pane was in the backlog, so what it wrote is more of
    /// a flood rather than the start of a burst.
    fn pane_readable(&mut self, p: PaneId, flooding: bool) {
        const READS_PER_TURN: usize = 1;
        // Not read while only control clients that are behind would get
        // its output ([`Self::pane_reads`]).
        if self
            .model
            .panes
            .get(&p)
            .and_then(|x| x.io.as_ref())
            .is_some_and(|io| !io.reading())
        {
            return;
        }
        let mut closed = false;
        for turn in 0.. {
            if turn == READS_PER_TURN {
                self.backlog.insert(p);
                break;
            }
            let Some(io) = self.model.panes.get_mut(&p).and_then(|x| x.io.as_mut()) else {
                return;
            };
            match io.read() {
                ReadResult::Data(bytes) => self.pane_input(p, &bytes, flooding),
                ReadResult::WouldBlock => break,
                ReadResult::Closed => {
                    closed = true;
                    break;
                }
            }
        }
        self.pane_output_queued(p);
        if closed {
            // window_pane_error_callback: the PTY is gone. The pane goes
            // once the exit status is in (server_destroy_pane returns
            // early without PANE_STATUSREADY, server-fn.c:382), so its
            // pane-exited carries the status; until then the PTY is closed.
            // A pipe still writing its output destroys it once drained.
            if self.model.panes.get(&p).is_some_and(|x| x.status.is_some()) {
                if self.pane_destroy_ready(p) {
                    self.pane_exited(p);
                }
            } else if let Some(io) = self.model.panes.get_mut(&p).and_then(|x| x.io.take()) {
                io.deregister(self.poll.registry());
            }
        }
    }

    /// `input_dcs_dispatch` and `screen_write_rawstring`: the payloads of
    /// `DCS tmux; ... ST` that pane `p` wrote go to the terminals, as the
    /// pane's `allow-passthrough` says (0 off drops them, 1 on, 2 all).
    /// `tty_write` offers each to the clients in order
    /// (`screen_write_set_client_cb`, screen-write.c:131-173):
    /// - `all`: every client whose session has the pane's window, whatever
    ///   it is drawing (`TTY_CTX_INVISIBLE_PANES`);
    /// - `on`: the clients that show the window, unless they are in a full
    ///   redraw or frozen under a message (skipped); a pane with a redraw
    ///   pending ends the walk for every client after it (state -1).
    ///
    /// A pane in a mode has no `wp` on its screen-write context, so nothing
    /// is written.
    fn pane_passthrough(
        &mut self,
        p: PaneId,
        payloads: Vec<vtmux_vt::Passthrough>,
        redraw_pending: bool,
    ) {
        let Some(pane) = self.model.panes.get(&p) else {
            return;
        };
        let mode = self
            .model
            .number_option(TreeId::Pane(p), "allow-passthrough");
        if mode == 0 || payloads.is_empty() || !pane.modes.is_empty() {
            return;
        }
        let window = pane.window;
        let visible = self
            .model
            .windows
            .get(&window)
            .is_some_and(|w| w.is_visible(p));
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for payload in payloads {
            for &id in &ids {
                let c = &self.clients[&id];
                let Some(s) = c.attached else { continue };
                if c.control.is_some() || c.caps.is_none() {
                    continue;
                }
                if mode == 2 {
                    let has = self
                        .model
                        .sessions
                        .get(&s)
                        .is_some_and(|x| x.windows.values().any(|w| *w == window));
                    if !has {
                        continue;
                    }
                } else {
                    if c.status.redraw_window_pending() || c.status.is_frozen() {
                        continue;
                    }
                    if self.model.current_of(s).is_none_or(|(_, w, _)| w != window) || !visible {
                        continue;
                    }
                    if payload.redraw || redraw_pending {
                        break;
                    }
                }
                self.clients
                    .get_mut(&id)
                    .expect("client")
                    .passthrough
                    .extend_from_slice(&payload.data);
            }
        }
    }

    fn on_signals(&mut self) {
        let pending: Vec<i32> = self.signals.pending().collect();
        for sig in pending {
            match sig {
                SIGCHLD => self.reap_children(),
                // server_signal: SIGTERM/SIGINT exit (server.c:442-446).
                SIGTERM | SIGINT => self.begin_shutdown(),
                SIGUSR1 => self.recreate_socket(),
                // SIGUSR2 toggles tmux's debug log, which vtmux does not
                // have; the rest are caught and ignored like server_signal.
                _ => {}
            }
        }
    }

    /// SIGUSR1 (server.c:450-458): create the socket again, for one that
    /// was removed by accident, and serve on the new one. On failure the
    /// old listener stays.
    fn recreate_socket(&mut self) {
        let Ok(std_listener) = sys::daemon::bind_socket(&self.socket, self.config.default_socket)
        else {
            return;
        };
        let mut fresh = UnixListener::from_std(std_listener);
        let Some(old) = self.listener.as_mut() else {
            return;
        };
        let _ = self.poll.registry().deregister(old);
        if self
            .poll
            .registry()
            .register(&mut fresh, LISTENER, Interest::READABLE)
            .is_ok()
        {
            self.listener = Some(fresh);
        }
    }

    /// `server_child_signal`: reap every exited child; a pane whose process
    /// is gone and whose PTY is drained is destroyed.
    fn reap_children(&mut self) {
        loop {
            match self.reap_one() {
                Some((pid, status)) => {
                    if status.stopped() {
                        continue;
                    }
                    let raw = pid.as_raw_nonzero().get();
                    let job_status = match (status.exit_status(), status.terminating_signal()) {
                        (_, Some(sig)) => crate::jobs::JobStatus::Signaled(sig),
                        (code, None) => crate::jobs::JobStatus::Exited(code.unwrap_or(0)),
                    };
                    if self.job_died(raw, job_status) || self.popup_died(raw, job_status) {
                        continue;
                    }
                    let found = self
                        .model
                        .panes
                        .values_mut()
                        .find(|p| p.pid == raw)
                        .map(|p| {
                            p.exited = true;
                            p.status = Some(job_status);
                            p.id
                        });
                    if let Some(p) = found {
                        // server_child_exited: window_pane_wait_finish,
                        // spawn_editor_finish.
                        if let Some(mut pane) = self.model.panes.remove(&p) {
                            self.pane_wait_finish(&mut pane);
                            self.model.panes.insert(p, pane);
                        }
                        if let Some(pane) = self.model.panes.get_mut(&p) {
                            let status = pane.status;
                            if let Some(editor) = pane.editor.take() {
                                self.editor_finish(editor, status);
                            }
                        }
                        // Drain what is left, then the read side reports
                        // the close (EIO) and destroys the pane; a pipe
                        // still writing its output does once it is drained.
                        let flooding = self.backlog.contains(&p);
                        self.pane_readable(p, flooding);
                        if self.pane_destroy_ready(p) {
                            self.pane_exited(p);
                        }
                    }
                }
                None => return,
            }
        }
    }

    /// One exited child reaped, any but an upgrade's dry run while it
    /// runs: that one is the dry run's to reap, once it has killed the
    /// process group the child leads (`sys::upgrade::DryRun`), so it is
    /// left a zombie here and the dry run told it has ended.
    fn reap_one(&mut self) -> Option<(rustix::process::Pid, rustix::process::WaitStatus)> {
        use rustix::process::{Pid, WaitOptions, wait, waitpid};
        let Some(dry) = self.upgrade_dry_run_pid() else {
            return wait(WaitOptions::NOHANG).ok().flatten();
        };
        loop {
            let pid = sys::upgrade::next_exited()?;
            if pid == dry {
                self.upgrade_dry_run_ended();
                if self.upgrade_dry_run_pid().is_some() {
                    // Not reaped after all: leave the rest to the next turn.
                    return None;
                }
                return wait(WaitOptions::NOHANG).ok().flatten();
            }
            let pid = Pid::from_raw(pid)?;
            if let Ok(Some(reaped)) = waitpid(Some(pid), WaitOptions::NOHANG) {
                return Some(reaped);
            }
        }
    }

    /// `check_window_name` (names.c:54-104): for windows with
    /// `automatic-rename` on whose active pane changed, at most every
    /// `NAME_INTERVAL`, the expanded `automatic-rename-format` becomes the
    /// name (`window_set_name(w, name, 1)`).
    fn check_window_names(&mut self) {
        let now = Instant::now();
        let windows: Vec<_> = self.model.windows.keys().copied().collect();
        for w in windows {
            let Some(window) = self.model.windows.get(&w) else {
                continue;
            };
            if self
                .model
                .number_option(TreeId::Window(w), "automatic-rename")
                == 0
            {
                continue;
            }
            let active = window.active();
            let Some(pane) = self.model.panes.get(&active) else {
                continue;
            };
            if !pane.changed || pane.last_rename.is_some_and(|t| now - t < NAME_INTERVAL) {
                continue;
            }
            // format_window_name: FORMAT_WINDOW with no session or winlink.
            let found = crate::target::Found {
                window: Some(w),
                pane: Some(active),
                ..Default::default()
            };
            let template = self
                .model
                .options
                .get(TreeId::Window(w), "automatic-rename-format")
                .map(|e| e.value.to_text(e.def, None, false))
                .unwrap_or_default();
            let ctx = self.format_ctx(found, None);
            let name =
                crate::names::clean_untrusted(&vtmux_core::format::expand_str(&template, &ctx));
            if let Some(pane) = self.model.panes.get_mut(&active) {
                pane.changed = false;
                pane.last_rename = Some(now);
            }
            // window_set_name only on a change (names.c:94-99), which
            // fires window-renamed.
            if self.model.windows.get(&w).is_some_and(|x| x.name != name) {
                self.model.set_window_name(w, &name);
                // names.c:100.
                self.model.redraw(crate::model::Redraw::WindowStatus(w));
            }
        }
    }
}

/// A terminal's size as the server takes it (tty.c `tty_resize`,
/// 130-143): a dimension the terminal reports as 0 (a PTY nobody sized)
/// is 80 columns or 24 rows.
fn tty_size(cols: u32, rows: u32) -> (u32, u32) {
    (
        if cols == 0 { 80 } else { cols },
        if rows == 0 { 24 } else { rows },
    )
}

fn cmd_error_bytes(e: &CmdError) -> Vec<u8> {
    match e {
        CmdError::Edge(edge) => edge.to_bytes(),
        other => other.to_bytes(),
    }
}

/// Start the server on a socket the caller already bound
/// ([`sys::daemon::bind_socket`]) and run it until it exits.
#[must_use]
/// Run a server on `listener`; `starter` is the connection of the client
/// that started it, which is its first client ([`Server::adopt_starter`]).
pub fn run_server(
    listener: StdListener,
    socket: PathBuf,
    config: ServerConfig,
    starter: std::os::unix::net::UnixStream,
) -> i32 {
    // D27 decision 15: a `vtmux-persist` checkpoint comes back first.
    match restore::start(listener, socket, config) {
        Ok(mut server) => match server.adopt_starter(starter).and_then(|()| server.run()) {
            Ok(()) => 0,
            Err(_) => 1,
        },
        Err(_) => 1,
    }
}

/// `sig2name` (tmux.c:327-338): `sys_signame[signo]` where libc has it
/// (`HAVE_SYS_SIGNAME`: the BSDs and macOS, lower-case names), else the
/// number (glibc and musl have no `sys_signame`).
pub(crate) fn sig_name(sig: i32) -> String {
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        // The BSD signal numbering, which these systems share for 1-31.
        const NAMES: [&str; 32] = [
            "Signal 0", "hup", "int", "quit", "ill", "trap", "abrt", "emt", "fpe", "kill", "bus",
            "segv", "sys", "pipe", "alrm", "term", "urg", "stop", "tstp", "cont", "chld", "ttin",
            "ttou", "io", "xcpu", "xfsz", "vtalrm", "prof", "winch", "info", "usr1", "usr2",
        ];
        if let Some(name) = usize::try_from(sig)
            .ok()
            .filter(|&i| i > 0)
            .and_then(|i| NAMES.get(i))
        {
            return (*name).to_owned();
        }
    }
    sig.to_string()
}

/// Whether a pane no client shows keeps its screen view: it had output
/// (`output_at`, since it was last shown or released) less than
/// [`HIDDEN_VIEW_IDLE`] before `now`.
fn hidden_view_kept(output_at: Option<Instant>, now: Instant) -> bool {
    output_at.is_some_and(|t| now.saturating_duration_since(t) < HIDDEN_VIEW_IDLE)
}

fn int(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// The title the last OSC 0 or 2 in passthrough bytes `raw` sets (ended
/// by BEL or ST), if any.
fn osc_title(raw: &[u8]) -> Option<String> {
    let mut found = None;
    let mut at = 0;
    while let Some(i) = raw[at..].windows(2).position(|w| w == b"\x1b]") {
        let start = at + i + 2;
        at = start;
        let body = &raw[start..];
        let Some(text) = body
            .strip_prefix(b"0;")
            .or_else(|| body.strip_prefix(b"2;"))
        else {
            continue;
        };
        let end = text
            .iter()
            .enumerate()
            .find(|&(j, &b)| b == 0x07 || (b == 0x1b && text.get(j + 1) == Some(&b'\\')))
            .map(|(j, _)| j);
        if let Some(end) = end {
            found = Some(String::from_utf8_lossy(&text[..end]).into_owned());
        }
    }
    found
}

/// The command error for a terminal that cannot be opened: tmux's client
/// refuses a terminal its terminfo lacks before it connects
/// (`tty_term_read_list`, client.c:325-331), with no prefix; the server
/// refuses one without `clear` or `cup` (`tty_term_create`) as `open
/// terminal failed` (cmd-attach-session.c:141, cmd-new-session.c:208).
fn open_error(e: &vtmux_core::term_features::OpenError) -> EdgeError {
    match e {
        vtmux_core::term_features::OpenError::Missing(_) => EdgeError::Message(e.to_string()),
        vtmux_core::term_features::OpenError::Lacks(_) => {
            EdgeError::OpenTerminalFailed(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Server;
    use crate::mode::prompt::tests::setup;

    #[test]
    fn passthrough_titles_are_found() {
        use super::osc_title;
        assert_eq!(
            osc_title(b"X\x1b]52;c;aGk=\x07\x1b]2;INNER\x07\x1b[31m"),
            Some("INNER".into())
        );
        assert_eq!(osc_title(b"\x1b]0;a\x1b\\\x1b]2;b\x1b\\"), Some("b".into()));
        assert_eq!(osc_title(b"\x1b]2;cut"), None);
        assert_eq!(osc_title(b"\x1b]1;icon\x07"), None);
        assert_eq!(osc_title(b"plain"), None);
    }

    /// A client of another protocol version (`Speaks::Frozen`) is sent the
    /// frozen frames and nothing else (protocol module docs).
    #[test]
    fn a_frozen_client_is_sent_frozen_frames_only() {
        use std::io::Read as _;
        use vtmux_core::protocol::{ServerMsg, Speaks, decode};
        let (mut server, _, _, _, dir) = setup();
        let (ours, mut peer) = mio::net::UnixStream::pair().unwrap();
        let id = super::ClientId(server.next_client);
        server.next_client += 1;
        let mut client = super::Client::new(ours, 0);
        client.speaks = Speaks::Frozen;
        server.clients.insert(id, client);
        let sent = [
            ServerMsg::Ready,
            ServerMsg::Frame(b"frame".to_vec()),
            ServerMsg::Stdout(b"out".to_vec()),
            ServerMsg::WriteClose,
            ServerMsg::Suspend,
            ServerMsg::Flags { wait_exit: true },
            ServerMsg::Stderr(b"err".to_vec()),
        ];
        for m in &sent {
            server.send(id, m);
        }
        let mut wire = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = peer.read(&mut buf) {
            wire.extend_from_slice(&buf[..n]);
        }
        let mut got = Vec::new();
        let mut at = 0;
        while at < wire.len() {
            let (m, used) = decode::<ServerMsg>(&wire[at..]).unwrap();
            got.push(m);
            at += used;
        }
        let frozen: Vec<ServerMsg> = sent.into_iter().filter(ServerMsg::is_frozen).collect();
        assert_eq!(got, frozen);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An `UpgradeRequest` would run a binary as the server's owner, so a
    /// peer the access list denies or makes read-only is refused
    /// (server-acl.c `server_acl_check`, as `server_accept` does for any
    /// client); a writer with a relative binary is refused too, and one
    /// that can't run gets why.
    #[test]
    fn an_upgrade_request_needs_write_access_and_an_absolute_binary() {
        use crate::acl::Subject;
        use std::io::Read as _;
        use vtmux_core::protocol::{ClientMsg, ServerMsg, decode};
        let (mut server, _, _, _, dir) = setup();
        let uid = rustix::process::getuid().as_raw();
        let answer = |server: &mut super::Server, binary: &str| {
            let (ours, mut peer) = mio::net::UnixStream::pair().unwrap();
            let id = super::ClientId(server.next_client);
            server.next_client += 1;
            server.clients.insert(id, super::Client::new(ours, 0));
            let binary = std::path::PathBuf::from(binary);
            server.on_message(id, ClientMsg::UpgradeRequest { binary });
            let mut wire = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = peer.read(&mut buf) {
                wire.extend_from_slice(&buf[..n]);
            }
            match decode::<ServerMsg>(&wire).unwrap() {
                (ServerMsg::UpgradeResult { ok: false, message }, n) if n == wire.len() => message,
                other => panic!("{other:?}"),
            }
        };
        let missing = dir.join("missing");
        let missing = missing.to_str().unwrap();
        assert_eq!(
            answer(&mut server, missing),
            format!("{missing}: No such file or directory")
        );
        assert_eq!(
            answer(&mut server, "vtmux"),
            "binary must be an absolute path"
        );
        server.acl.set_read_only(Subject::User(uid), true);
        assert_eq!(answer(&mut server, missing), "access denied");
        server.acl.deny(Subject::User(uid));
        assert_eq!(answer(&mut server, missing), "access denied");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// What a pane's output sets off, in tmux's order (input.c
    /// `input_parse_buffer`): `pane-activity` first and once a loop turn,
    /// then each title, BEL and OSC 133 event as the bytes come, the
    /// command events with tmux's payload; the OSC 133 state follows.
    #[test]
    fn pane_output_fires_its_events_in_order() {
        use crate::events::{Queued, Value};
        let (mut server, p, s, _, dir) = setup();
        crate::server::snapshot::tests::ok(
            &mut server,
            &dir,
            &["set-hook", "-g", "pane-activity", "set -g @a 1"],
        );
        server.model.events.clear();
        server.render_and_exit();
        server.pane_input(
            p,
            b"\x1b]2;t\x07\x1b]133;A\x07$ \x1b]133;C\x07\x07\x07\x1b]133;D;3\x07",
            false,
        );
        server.pane_input(p, b"\x1b]133;C\x1b\\", false);
        let events: Vec<_> = std::mem::take(&mut server.model.events)
            .into_iter()
            .filter_map(|q| match q {
                Queued::Event(ev) => Some(ev),
                _ => None,
            })
            .collect();
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "pane-activity",
                "pane-title-changed",
                "pane-shell-prompt",
                "pane-command-started",
                "pane-bell",
                "pane-bell",
                "pane-command-finished",
                "pane-command-started",
            ]
        );
        let finished = &events[6];
        assert_eq!(finished.items.get("command_status"), Some(&Value::Int(3)));
        assert_eq!(finished.get_pane("pane"), Some(p));
        assert_eq!(finished.get_session("session").map(|x| x.0), Some(s));
        assert_eq!(finished.items.get("window_index"), Some(&Value::Int(0)));
        for key in ["command_start_time", "command_end_time", "command_duration"] {
            assert!(finished.items.contains_key(key), "{key}");
        }
        let started = &events[7];
        assert!(!started.items.contains_key("command_status"));
        assert!(!started.items.contains_key("command_end_time"));
        assert!(started.items.contains_key("command_duration"));
        let pane = &server.model.panes[&p];
        assert_eq!(pane.output_generation, 2);
        let c = pane.shell_commands;
        assert!(c.running && c.status.is_none() && c.ended.is_none());
        assert!(c.last_prompt.is_some() && c.started.is_some());
        // The next loop turn's output fires pane-activity again.
        server.render_and_exit();
        server.pane_input(p, b"x", false);
        assert!(
            server
                .model
                .events
                .iter()
                .any(|q| matches!(q, Queued::Event(ev) if ev.name == "pane-activity"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A read with a flood of BELs and titles fires `pane-bell` and
    /// `pane-title-changed` for every one, as tmux does while it parses
    /// (input.c `input_c0_dispatch`, `input_exit_osc`): none is dropped
    /// however many one read holds.
    #[test]
    fn a_flooding_read_fires_every_bell_and_title() {
        use crate::events::Queued;
        let (mut server, p, _, _, dir) = setup();
        server.model.events.clear();
        server.render_and_exit();
        server.pane_input(p, &b"\x1b]2;t\x07x\x07".repeat(8000), false);
        let count = |name: &str| {
            server
                .model
                .events
                .iter()
                .filter(|q| matches!(q, Queued::Event(ev) if ev.name == name))
                .count()
        };
        assert_eq!(
            (count("pane-bell"), count("pane-title-changed")),
            (8000, 8000)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A `pane-activity` nothing would run commands for is only counted
    /// (`options_hook_fired`), without an event; a hook command makes it
    /// an event again.
    #[test]
    fn pane_activity_with_no_hook_commands_is_only_counted() {
        use crate::events::Queued;
        use vtmux_core::options::TreeId;
        let (mut server, p, _, _, dir) = setup();
        let activity = |server: &Server| {
            server
                .model
                .events
                .iter()
                .filter(|q| matches!(q, Queued::Event(ev) if ev.name == "pane-activity"))
                .count()
        };
        server.model.events.clear();
        server.render_and_exit();
        server.pane_input(p, b"x", false);
        assert_eq!(activity(&server), 0);
        let fires = |server: &Server| {
            server
                .model
                .options
                .hook_fire_stats(TreeId::GlobalWindow, "pane-activity")
                .0
        };
        assert_eq!(fires(&server), 1);
        server.render_and_exit();
        server.pane_input(p, b"x", false);
        assert_eq!((activity(&server), fires(&server)), (0, 2));
        crate::server::snapshot::tests::ok(
            &mut server,
            &dir,
            &["set-hook", "-g", "pane-activity", "set -g @a 1"],
        );
        server.render_and_exit();
        server.model.events.clear();
        server.pane_input(p, b"x", false);
        assert_eq!(activity(&server), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Only a change a flood did not make lifts the pacing: a pane that
    /// started a burst (not in the backlog when read) or one to redraw
    /// whole, in the window shown; flood output alone does not.
    #[test]
    fn only_non_flood_changes_lift_the_pacing() {
        let (mut server, p, s, _, dir) = setup();
        let w = server.model.current_of(s).unwrap().1;
        server.model.redraw_panes.clear();
        server.backlog.insert(p);
        assert!(!server.unpaced_change_in(w), "a flood alone is paced");
        server.burst_started.insert(p);
        assert!(server.unpaced_change_in(w), "a burst (a key's echo)");
        server.burst_started.clear();
        server.model.redraw_panes.insert(p);
        assert!(server.unpaced_change_in(w), "a whole-pane redraw");
        server.model.redraw_panes.clear();
        let other = vtmux_core::ids::WindowId(w.0 + 1);
        server.burst_started.insert(p);
        assert!(!server.unpaced_change_in(other), "another window's pane");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A hidden pane keeps its view for `HIDDEN_VIEW_IDLE` after its last
    /// output, not a moment longer; with no output, not at all.
    #[test]
    fn hidden_view_kept_until_idle() {
        use super::{HIDDEN_VIEW_IDLE, hidden_view_kept};
        let t = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        assert!(!hidden_view_kept(None, t), "no output since shown");
        assert!(hidden_view_kept(Some(t), t));
        assert!(hidden_view_kept(
            Some(t),
            t + HIDDEN_VIEW_IDLE.saturating_sub(ms(1))
        ));
        assert!(!hidden_view_kept(Some(t), t + HIDDEN_VIEW_IDLE));
        assert!(hidden_view_kept(Some(t + ms(5)), t), "output after now");
    }

    /// A pane no client shows (no client is attached here) drops its
    /// screen view at the end of a turn with no output since it was last
    /// shown, but keeps it while synchronized updates (which read it)
    /// keep coming, so each is an incremental read, not a whole one; the
    /// server wakes to drop it once the output stops.
    #[test]
    fn hidden_pane_keeps_its_view_while_output_flows() {
        let (mut server, p, _, _, dir) = setup();
        let held = |server: &super::Server| server.model.panes[&p].term.holds_screen();
        let _ = server.model.panes[&p].term.screen();
        server.release_hidden_screens(std::time::Instant::now());
        assert!(!held(&server), "idle and hidden: released");
        assert_eq!(server.hidden_view_deadline(), None);

        server.pane_input(p, b"\x1b[?2026hone\x1b[?2026l", false);
        assert!(held(&server), "the update read the screen");
        let out = server.model.panes[&p].output_at.unwrap();
        server.release_hidden_screens(out);
        assert!(held(&server), "output flows: kept");
        let due = out + super::HIDDEN_VIEW_IDLE;
        assert_eq!(server.hidden_view_deadline(), Some(due));

        server.release_hidden_screens(due);
        assert!(!held(&server), "idle for HIDDEN_VIEW_IDLE: released");
        assert_eq!(server.hidden_view_deadline(), None, "no more wakeups");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The frame bytes the server wrote to the client end `peer` since the
    /// last call; every frame is acked, so the client never blocks.
    fn frames(
        server: &mut super::Server,
        id: super::ClientId,
        peer: &mut mio::net::UnixStream,
    ) -> Vec<u8> {
        use std::io::Read as _;
        use vtmux_core::protocol::{ServerMsg, decode};
        let mut wire = Vec::new();
        let mut buf = vec![0u8; 65536];
        while let Ok(n @ 1..) = peer.read(&mut buf) {
            wire.extend_from_slice(&buf[..n]);
        }
        let mut out = Vec::new();
        let mut at = 0;
        while at < wire.len() {
            let (msg, len) = decode::<ServerMsg>(&wire[at..]).unwrap();
            if let ServerMsg::Frame(b) = msg {
                out.extend(b);
            }
            at += len;
        }
        server.clients.get_mut(&id).unwrap().unacked.ack(u64::MAX);
        out
    }

    /// The characters a frame draws, without its control sequences.
    fn drawn_text(frame: &str) -> String {
        let mut text = String::new();
        let mut chars = frame.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.next();
                chars.find(|c| ('@'..='~').contains(c));
            } else {
                text.push(c);
            }
        }
        text
    }

    /// D26: with a pane blend on, the plain-frame skip still sends nothing
    /// for a turn with no change, and a focus change (two panes' roles
    /// swapped) is not skipped; the colours' arrival recomposes.
    #[test]
    fn pane_blend_keeps_the_frame_skip() {
        use vtmux_core::options::{TreeId, Value};
        let (mut server, p, s, _, dir) = setup();
        server.reset_mode(p);
        server
            .model
            .options
            .set_table_value(TreeId::GlobalSession, "status", Value::Choice(0));
        let run = |server: &mut super::Server, words: &[&str]| {
            let argv: Vec<Vec<u8>> = words.iter().map(|w| w.as_bytes().to_vec()).collect();
            let calls = vtmux_core::cmd::parse_argv_bytes(&argv).unwrap();
            let item = crate::commands::Item::new(None, dir.clone());
            let end = crate::cmdq::End::Quiet;
            server.cmdq_append(None, crate::cmdq::NewBatch::new(item, calls, end));
            server.cmdq_process();
        };
        run(
            &mut server,
            &[
                "set",
                "-g",
                "vtmux-pane-inactive-blend",
                "fg=bg/50,bg=#400000/50",
            ],
        );
        let (id, mut peer) = attach_test_client(&mut server, s, &dir);
        for _ in 0..3 {
            server.render_clients();
            frames(&mut server, id, &mut peer);
        }
        let (_, w, _) = server.model.current_of(s).unwrap();
        run(&mut server, &["split-window", "-h", "-t", "a", "cat"]);
        let q = *server.model.windows[&w]
            .panes
            .panes
            .iter()
            .find(|&&x| x != p)
            .unwrap();
        server.render_clients();
        let plain = frames(&mut server, id, &mut peer);
        assert!(!plain.is_empty(), "the split draws");
        server.render_clients();
        assert_eq!(frames(&mut server, id, &mut peer), b"", "settled");
        // The colours arrive: the inactive pane is recomposed, blended.
        server
            .on_client_input(id, b"\x1b]10;#ffffff\x07\x1b]11;#000000\x07")
            .unwrap();
        server.render_clients();
        let blended = String::from_utf8(frames(&mut server, id, &mut peer)).unwrap();
        assert_eq!(server.model.windows[&w].panes.panes.len(), 2);
        assert!(blended.contains("48;2;32;0;0m"), "blended: {blended:?}");
        let skipped = |server: &super::Server| server.clients[&id].window_cache.skipped.get();
        let before = skipped(&server);
        server.render_clients();
        assert_eq!(frames(&mut server, id, &mut peer), b"");
        assert_eq!(skipped(&server), before + 1, "a repeat is still skipped");
        let other = if server.model.windows[&w].active() == p {
            q
        } else {
            p
        };
        assert!(server.model.set_active(w, other));
        server.render_clients();
        let swapped = frames(&mut server, id, &mut peer);
        assert!(!swapped.is_empty(), "a focus change is not skipped");
        assert_eq!(skipped(&server), before + 1);
        server.render_clients();
        assert_eq!(frames(&mut server, id, &mut peer), b"", "nothing after it");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// D-033: the replies to the OSC 4 queries vtmux sends at attach for
    /// the pane blend are kept but answer no pane's OSC 4 request
    /// (`input_request_reply` would answer or drop it); the terminal's
    /// next reply for that entry answers the pane.
    #[test]
    fn blend_palette_replies_leave_a_pane_request_alone() {
        let (mut server, p, s, _, dir) = setup();
        let (id, _peer) = attach_test_client(&mut server, s, &dir);
        server.pane_answers(
            p,
            vec![vtmux_vt::Reply::Palette {
                index: 3,
                end: vtmux_vt::OscEnd::Bel,
            }],
        );
        let out = |server: &super::Server| server.model.panes[&p].requests.deadline();
        assert!(out(&server).is_some(), "the pane's request went out");
        let mut ours = Vec::new();
        for i in 0..16 {
            ours.extend_from_slice(
                format!("\x1b]4;{i};rgb:0{i:x}0{i:x}/0000/0000\x1b\\").as_bytes(),
            );
        }
        server.on_client_input(id, &ours).unwrap();
        assert!(out(&server).is_some(), "still waiting for its own reply");
        assert_eq!(
            server.clients[&id].reported.palette[15],
            Some((0x0f, 0, 0)),
            "kept for the blend"
        );
        server
            .on_client_input(id, b"\x1b]4;3;rgb:0303/0000/0000\x1b\\")
            .unwrap();
        assert_eq!(out(&server), None, "answered");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A 40x10 xterm-256color client attached to session `s`, and the peer
    /// end its frames arrive at.
    fn attach_test_client(
        server: &mut super::Server,
        s: vtmux_core::ids::SessionId,
        dir: &std::path::Path,
    ) -> (super::ClientId, mio::net::UnixStream) {
        use vtmux_core::protocol::{ClientFlags, Hello, TtyInfo};
        let (ours, peer) = mio::net::UnixStream::pair().unwrap();
        let id = super::ClientId(server.next_client);
        server.next_client += 1;
        let mut client = super::Client::new(ours, 0);
        client.hello = Some(Hello {
            version: vtmux_core::protocol::PROTOCOL_VERSION,
            invocation: vtmux_core::socket::Invocation::Vtmux,
            pid: 1,
            argv: Vec::new(),
            cwd: dir.to_path_buf(),
            env: vec![("COLORTERM".into(), "truecolor".into())],
            tty: Some(TtyInfo {
                name: "/dev/null".into(),
                term: "xterm-256color".into(),
                cols: 40,
                rows: 10,
                features: Vec::new(),
            }),
            flags: ClientFlags::default(),
            ext: vtmux_core::protocol::HelloExt::default(),
        });
        server.clients.insert(id, client);
        server.attach_client(id, s).unwrap();
        (id, peer)
    }

    /// The plain-frame skip (a turn where no composed row or the cursor
    /// changed sends nothing) does not swallow a change: a turn that only
    /// moves the pane's cursor still sends a frame, and it is a cursor
    /// move alone.
    #[test]
    fn plain_frame_skip_sends_nothing_unless_something_changed() {
        use vtmux_core::options::{TreeId, Value};
        use vtmux_core::protocol::{ClientFlags, Hello, TtyInfo};
        let (mut server, p, s, _, dir) = setup();
        server.reset_mode(p);
        // No status line: its clock and host name would change the frame.
        server
            .model
            .options
            .set_table_value(TreeId::GlobalSession, "status", Value::Choice(0));
        let (ours, mut peer) = mio::net::UnixStream::pair().unwrap();
        let id = super::ClientId(server.next_client);
        server.next_client += 1;
        let mut client = super::Client::new(ours, 0);
        client.hello = Some(Hello {
            version: vtmux_core::protocol::PROTOCOL_VERSION,
            invocation: vtmux_core::socket::Invocation::Vtmux,
            pid: 1,
            argv: Vec::new(),
            cwd: dir.clone(),
            env: Vec::new(),
            tty: Some(TtyInfo {
                name: "/dev/null".into(),
                term: "xterm-256color".into(),
                cols: 40,
                rows: 10,
                features: Vec::new(),
            }),
            flags: ClientFlags::default(),
            ext: vtmux_core::protocol::HelloExt::default(),
        });
        server.clients.insert(id, client);
        server.attach_client(id, s).unwrap();
        // The attach frames: startup, the first drawing, the modes.
        let sent: Vec<usize> = (0..4)
            .map(|_| {
                server.render_clients();
                frames(&mut server, id, &mut peer).len()
            })
            .collect();
        assert_eq!(sent.last(), Some(&0), "a turn with no change: {sent:?}");
        let skipped = |server: &super::Server| server.clients[&id].window_cache.skipped.get();
        // The no-change turn was skipped by draw_window itself, not just
        // diffed to nothing by the compositor.
        let before = skipped(&server);
        server.render_clients();
        assert_eq!(frames(&mut server, id, &mut peer), b"");
        assert_eq!(skipped(&server), before + 1, "draw_window skips a repeat");
        let pane = server.model.panes.get_mut(&p).unwrap();
        pane.term.feed(b"\x1b[3;5H");
        server.render_clients();
        let frame = String::from_utf8(frames(&mut server, id, &mut peer)).unwrap();
        assert!(frame.contains("\x1b[3;5H"), "the cursor moves: {frame:?}");
        // Nothing but control sequences: no cell is drawn again.
        let text = drawn_text(&frame);
        assert_eq!(text, "", "a cursor move only: {frame:?}");
        server.render_clients();
        assert_eq!(frames(&mut server, id, &mut peer), b"", "nothing after it");
        // A cell changes and the cursor ends where it was: a composed row
        // changed, so the frame is not skipped and carries the text.
        let before = skipped(&server);
        let pane = server.model.panes.get_mut(&p).unwrap();
        pane.term.feed(b"\x1b[1;1HZQ\x1b[3;5H");
        server.render_clients();
        let frame = String::from_utf8(frames(&mut server, id, &mut peer)).unwrap();
        assert_eq!(drawn_text(&frame), "ZQ", "the changed cells: {frame:?}");
        assert_eq!(skipped(&server), before, "a changed row is not skipped");
        server.render_clients();
        assert_eq!(frames(&mut server, id, &mut peer), b"", "nothing after it");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A plain frame's buffer is the frame before last: its window rows
    /// are copied again only where the composed row changed since, and the
    /// diff skips rows tagged alike. Rows changed one and two frames back
    /// must still reach the screen, and each frame draws only its change.
    #[test]
    fn plain_frames_copy_and_diff_only_changed_rows() {
        use vtmux_core::options::{TreeId, Value};
        use vtmux_core::protocol::{ClientFlags, Hello, TtyInfo};
        let (mut server, p, s, _, dir) = setup();
        server.reset_mode(p);
        server
            .model
            .options
            .set_table_value(TreeId::GlobalSession, "status", Value::Choice(0));
        let (ours, mut peer) = mio::net::UnixStream::pair().unwrap();
        let id = super::ClientId(server.next_client);
        server.next_client += 1;
        let mut client = super::Client::new(ours, 0);
        client.hello = Some(Hello {
            version: vtmux_core::protocol::PROTOCOL_VERSION,
            invocation: vtmux_core::socket::Invocation::Vtmux,
            pid: 1,
            argv: Vec::new(),
            cwd: dir.clone(),
            env: Vec::new(),
            tty: Some(TtyInfo {
                name: "/dev/null".into(),
                term: "xterm-256color".into(),
                cols: 40,
                rows: 10,
                features: Vec::new(),
            }),
            flags: ClientFlags::default(),
            ext: vtmux_core::protocol::HelloExt::default(),
        });
        server.clients.insert(id, client);
        server.attach_client(id, s).unwrap();
        for _ in 0..4 {
            server.render_clients();
            frames(&mut server, id, &mut peer);
        }
        let row_text = |server: &super::Server, y: usize| -> String {
            let (screen, _) = server.clients[&id].compositor.last().unwrap();
            let cols = usize::from(screen.cols);
            screen.cells[y * cols..(y + 1) * cols]
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>()
        };
        let mut want = [String::new(), String::new(), String::new()];
        for (i, letter) in ('A'..='H').enumerate() {
            let row = i % 3;
            want[row] = format!("{letter}{letter}");
            let pane = server.model.panes.get_mut(&p).unwrap();
            pane.term
                .feed(format!("\x1b[{};1H{letter}{letter}\x1b[9;1H", row + 1).as_bytes());
            server.render_clients();
            let frame = String::from_utf8(frames(&mut server, id, &mut peer)).unwrap();
            assert_eq!(
                drawn_text(&frame),
                format!("{letter}{letter}"),
                "frame {i}: {frame:?}"
            );
            for (y, want) in want.iter().enumerate() {
                let text = row_text(&server, y);
                assert!(
                    text.starts_with(want.as_str()),
                    "frame {i}, row {y}: {text:?}"
                );
            }
            let (_, tags) = server.clients[&id].compositor.last().unwrap();
            assert!(
                tags.iter().all(|&t| t != 0),
                "plain frames tag every row: {tags:?}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A server that waited for a job (`kill-server` with one running)
    /// stops listening only as it exits, and a client refused from then on
    /// starts a fresh server on the same socket at once. That server's
    /// `tmux` shim must outlive this one: the shim goes before the socket
    /// stops listening, never with the rest of the teardown.
    /// The starting client is the server's first client (proc.c
    /// `proc_fork_and_daemon`, server.c `server_start`): a client that
    /// connects before the starter has said anything, and leaves, does not
    /// leave the fresh server with no sessions and no clients, so it does
    /// not exit (`exit-empty`) under the starter. Without the starter it
    /// would: that was the race when the starter connected after the fork.
    #[test]
    fn a_client_that_comes_and_goes_first_leaves_the_starter_a_server() {
        use super::{ClientId, Server, ServerConfig};
        use crate::server::snapshot::tests::{listener, temp_dir};
        use std::os::unix::net::UnixStream;
        let dir = temp_dir("starter");
        let early_poller = |server: &mut Server, socket: &std::path::Path| {
            let early = UnixStream::connect(socket).unwrap();
            let id = ClientId(server.next_client);
            server.accept();
            assert!(server.clients.contains_key(&id));
            drop(early);
            server.client_readable(id);
        };
        for starter in [true, false] {
            let (l, socket) = listener(&dir, "s");
            l.set_nonblocking(true).unwrap();
            let mut server = Server::new(l, socket.clone(), ServerConfig::default()).unwrap();
            let (ours, _theirs) = UnixStream::pair().unwrap();
            if starter {
                server.adopt_starter(ours).unwrap();
            }
            early_poller(&mut server, &socket);
            assert_eq!(server.should_exit(), !starter, "starter {starter}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_fresh_server_keeps_its_shim_when_the_old_one_exits() {
        use super::{Server, ServerConfig};
        use crate::server::snapshot::tests::{listener, temp_dir};
        let dir = temp_dir("shim-exit");
        let (l, socket) = listener(&dir, "s");
        let mut old = Server::new(l, socket.clone(), ServerConfig::default()).unwrap();
        let link = dir.join(".s.bin").join("tmux");
        assert!(link.exists(), "{}", link.display());
        old.shutdown = true;
        old.exit_clients();
        assert!(old.listener.is_none());
        // Refused now: the next client starts a server.
        let (l, socket) = listener(&dir, "s");
        let fresh = Server::new(l, socket, ServerConfig::default()).unwrap();
        assert!(link.exists());
        drop(old);
        assert!(link.exists(), "the old server removed the fresh one's shim");
        drop(fresh);
        assert!(!link.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
