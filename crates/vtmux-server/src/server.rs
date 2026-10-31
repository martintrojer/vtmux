//! The server: one `mio` loop on one thread (D9) over the listening
//! socket, client connections, pane PTYs and signals.
//!
//! A client connects, sends `Hello` (argv, cwd, environment, tty), the
//! server parses and runs the argv as one batch, relays the batch's output
//! as `Stdout`/`Stderr` frames and ends with `Exit`. A client attached by
//! its command stays connected and is handed to the attach hooks
//! ([`Server::attach_client`], [`Server::on_client_input`]).

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use signal_hook::consts::{SIGCHLD, SIGCONT, SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2, SIGWINCH};
use signal_hook_mio::v1_0::Signals;
use vtmux_core::cmd::{self, CmdError, CommandCall, ConfigEnv};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::layout::{Geometry, Layout as CoreLayout, WindowPanes};
use vtmux_core::options::{Defaults, OptionValue, OptionsStore, TreeId, Value};
use vtmux_core::protocol::{self, ClientMsg, Hello, ProtocolError, ServerMsg};
use vtmux_vt::PaneTerm;

use crate::alerts;
use crate::commands::{self, After, Item, Output};
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

/// `NAME_INTERVAL` (tmux.h:119): automatic-rename runs at most this often.
const NAME_INTERVAL: Duration = Duration::from_millis(500);

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
}

pub(crate) struct Client {
    stream: UnixStream,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    /// Frames sent but not yet written to the client's terminal.
    unacked: Unacked,
    /// Exit has been queued; close once the output is flushed.
    closing: bool,
    pub(crate) hello: Option<Hello>,
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
    /// The status lines as drawn, the message shown over them, and their
    /// redraw flag and timers (`c->status`, `c->message_string`).
    pub(crate) status: crate::render::status::StatusLine,
    /// Frame bytes sent to the client (`c->written`).
    pub(crate) written: usize,
    pasting: bool,
    decoder: crate::input::Decoder,
    caps: Option<crate::render::Caps>,
    /// The client's terminal once attached (`tty_open`): its features and
    /// capabilities after `terminal-features` and `terminal-overrides`.
    term: Option<vtmux_core::term_features::Term>,
    pub(crate) compositor: crate::render::Compositor,
}

/// Frame bytes the client has not yet written to its terminal: in
/// `outbuf`, the socket, or the client's own buffer. This is the queue
/// `tty_block_maybe` measures (tty.c:219-245, `tty->out`); `outbuf` alone
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

/// A batch whose client is blocked (`CMDQ_WAITING`): the item so far and
/// the commands after the blocking one.
struct Parked {
    item: Item,
    rest: Vec<CommandCall>,
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
}

pub struct Server {
    pub model: Model,
    pub info: ServerInfo,
    pub global_env: Environ,
    /// `kill-server` ran: exit once replies are out.
    pub shutdown: bool,
    /// Nesting of `source-file` (cmd-source-file.c depth limit).
    pub source_depth: usize,
    /// Errors from config files, shown to the first client
    /// (cfg.c `cfg_add_cause`).
    pub causes: Vec<String>,
    socket: PathBuf,
    poll: Poll,
    listener: UnixListener,
    signals: Signals,
    pub(crate) clients: BTreeMap<ClientId, Client>,
    next_client: usize,
    /// The last command number handed out (cmd-queue.c `cmdq_next`'s
    /// static `number`): every fired command takes one.
    pub(crate) cmd_number: u32,
    config: ServerConfig,
    config_loaded: bool,
    /// Panes that still had output when their read budget ran out. mio is
    /// edge-triggered, so they are read again on the next turn instead of
    /// waiting for an event that will not come.
    backlog: std::collections::BTreeSet<PaneId>,
    /// `wait-for` channels.
    pub waits: WaitChannels,
    /// Blocked batches, one per client at most.
    parked: BTreeMap<ClientId, Parked>,
    /// Clients woken this turn (`cmdq_continue`); their batches resume at
    /// the end of the turn, as tmux resumes them in its next `cmdq_next`.
    woken: Vec<ClientId>,
    /// Every key table (`key_tables`), the 3.8 defaults loaded at start.
    pub key_bindings: vtmux_core::keys::KeyBindings,
    /// Keys `send-keys -K` handed to a client, handled after the command
    /// (`server_client_handle_key_after`).
    pub(crate) injected_keys: Vec<(ClientId, vtmux_core::keys::Key)>,
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
    /// vtmux never runs `#()` while expanding (D9), which matches
    /// `FORMAT_NOJOBS`.
    fn expand(&self, template: &str) -> String {
        let origin = self.client.as_ref().map(|c| c.0).unwrap_or_default();
        let found = crate::target::current(self.model, origin).unwrap_or_default();
        let mut ctx = Ctx::new(self.model, self.info, self.env, found);
        if let Some((_, client, clients)) = &self.client {
            ctx.client.clone_from(client);
            ctx.clients = std::rc::Rc::clone(clients);
        }
        vtmux_core::format::expand_str(template, &ctx)
    }
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
        let mut signals = Signals::new([
            SIGCHLD, SIGTERM, SIGINT, SIGHUP, SIGUSR1, SIGUSR2, SIGWINCH, SIGCONT,
        ])?;
        poll.registry()
            .register(&mut signals, SIGNALS, Interest::READABLE)?;
        let mut global_env = Environ::from_pairs(config.environ.clone());
        if let Some(cwd) = &config.cwd {
            global_env.set("PWD", &cwd.to_string_lossy(), false);
        }
        let defaults = Defaults::from_env(
            default_shell(&config.environ)
                .to_string_lossy()
                .into_owned(),
            global_env.get("VISUAL").map(str::to_owned),
            global_env.get("EDITOR").map(str::to_owned),
        );
        let mut model = Model::default();
        model.options = OptionsStore::new(&defaults);
        Ok(Self {
            model,
            info: ServerInfo::new(socket.to_string_lossy().into_owned()),
            global_env,
            shutdown: false,
            source_depth: 0,
            causes: Vec::new(),
            socket,
            poll,
            listener,
            signals,
            clients: BTreeMap::new(),
            next_client: 0,
            cmd_number: 0,
            config,
            config_loaded: false,
            backlog: std::collections::BTreeSet::new(),
            waits: WaitChannels::default(),
            parked: BTreeMap::new(),
            woken: Vec::new(),
            key_bindings: vtmux_core::keys::KeyBindings::with_defaults(),
            injected_keys: Vec::new(),
        })
    }

    /// Run until the server exits.
    pub fn run(&mut self) -> io::Result<()> {
        let mut events = Events::with_capacity(256);
        loop {
            let timeout = self.next_timeout();
            match self.poll.poll(&mut events, timeout) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
            for event in &events {
                match event.token() {
                    LISTENER => self.accept(),
                    SIGNALS => self.on_signals(),
                    Token(t) if t >= CLIENT_BASE => {
                        let id = ClientId(t - CLIENT_BASE);
                        if event.is_readable() || event.is_read_closed() {
                            self.client_readable(id);
                        }
                        if event.is_writable() {
                            self.flush_client(id);
                        }
                    }
                    Token(t) => {
                        let p = PaneId(u32::try_from(t - PANE_BASE).unwrap_or(u32::MAX));
                        if event.is_writable() {
                            self.pane_writable(p);
                        }
                        if event.is_readable() || event.is_read_closed() {
                            self.pane_readable(p);
                        }
                    }
                }
            }
            for p in std::mem::take(&mut self.backlog) {
                self.pane_readable(p);
            }
            self.expire_repeat(Instant::now());
            self.status_timers(Instant::now());
            self.model.alerts_timers(Instant::now());
            self.run_alerts();
            let ids: Vec<_> = self.clients.keys().copied().collect();
            for id in ids {
                let _ = self.on_client_input(id, &[]);
            }
            self.resume_woken();
            self.check_window_names();
            self.check_window_resizes();
            self.apply_redraws();
            self.render_clients();
            self.model.redraw_panes.clear();
            self.control_tick();
            if self.wants_flush() {
                // server_loop / server_send_exit: release every waiter.
                self.woken.extend(self.waits.flush());
                self.resume_woken();
            }
            if self.should_exit() {
                self.exit_clients();
                return Ok(());
            }
        }
    }

    /// The poll timeout: the sooner of the loop's own needs and the next
    /// silence timer (`alerts_reset`'s `event_add`).
    fn next_timeout(&self) -> Option<Duration> {
        let own = self.own_timeout();
        let silence = self
            .model
            .alerts_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()));
        match (own, silence) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn own_timeout(&self) -> Option<Duration> {
        if !self.backlog.is_empty() || !self.woken.is_empty() {
            return Some(Duration::ZERO);
        }
        if let Some(deadline) = self
            .clients
            .values()
            .flat_map(|c| {
                [
                    c.decoder.deadline(),
                    c.keys.repeat_deadline,
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
    /// `server_loop` with `exit-empty` on and no sessions (server.c:268-306).
    fn wants_flush(&self) -> bool {
        self.shutdown || (self.exit_empty() && self.model.sessions.is_empty())
    }

    fn exit_empty(&self) -> bool {
        // A client that left before saying Hello (`-CC` without a tty)
        // still counts: tmux exits once it has no sessions and no clients.
        (self.config_loaded || self.next_client > 0)
            && matches!(
                self.model
                    .options
                    .get(TreeId::Server, "exit-empty")
                    .map(|e| &e.value),
                Some(OptionValue::Single(Value::Flag(true)))
            )
    }

    /// `server_loop` (server.c:268-306): exit when told to, or when there
    /// are no sessions left (`exit-empty`) and no client is waiting.
    fn should_exit(&self) -> bool {
        if self.shutdown {
            return self.clients.values().all(|c| c.outbuf.is_empty());
        }
        self.exit_empty()
            && self.model.sessions.is_empty()
            && self
                .clients
                .values()
                .all(|c| c.closing && c.outbuf.is_empty())
    }

    fn exit_clients(&mut self) {
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.flush_client(id);
        }
        let panes: Vec<_> = self.model.panes.keys().copied().collect();
        for p in panes {
            self.hangup_pane(p);
        }
        // tmux leaves the socket file: a later client gets ECONNREFUSED and
        // prints `no server running on` (client.c:291-295).
    }

    // ------------------------------------------------------------ clients

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let id = ClientId(self.next_client);
                    self.next_client += 1;
                    let activity = self.model.touch();
                    if self
                        .poll
                        .registry()
                        .register(&mut stream, Token(CLIENT_BASE + id.0), Interest::READABLE)
                        .is_err()
                    {
                        continue;
                    }
                    self.clients.insert(
                        id,
                        Client {
                            stream,
                            inbuf: Vec::new(),
                            outbuf: Vec::new(),
                            unacked: Unacked::default(),
                            closing: false,
                            hello: None,
                            attached: None,
                            control: None,
                            activity,
                            activity_secs: now_epoch(),
                            created_secs: now_epoch(),
                            focused: true,
                            keys: crate::key_dispatch::ClientKeys::default(),
                            last_session: None,
                            status: crate::render::status::StatusLine::default(),
                            written: 0,
                            pasting: false,
                            decoder: crate::input::Decoder::default(),
                            caps: None,
                            term: None,
                            compositor: crate::render::Compositor::default(),
                        },
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
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
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            self.drop_client(id);
        }
    }

    /// `server_client_lost`: the client's blocked commands never run
    /// (`cmd_wait_for_client_lost`, then `cmdq_next` drops the dead
    /// client's queue).
    fn drop_client(&mut self, id: ClientId) {
        if let Some(mut c) = self.clients.remove(&id) {
            let _ = self.poll.registry().deregister(&mut c.stream);
        }
        self.waits.client_lost(id);
        self.parked.remove(&id);
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
    fn array_option(&self, tree: TreeId, name: &str) -> Vec<String> {
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

    /// `cmdq_continue`: the client's blocked batch resumes at the end of
    /// this turn.
    pub fn wake(&mut self, id: ClientId) {
        self.woken.push(id);
    }

    fn resume_woken(&mut self) {
        while !self.woken.is_empty() {
            for id in std::mem::take(&mut self.woken) {
                let Some(p) = self.parked.remove(&id) else {
                    continue;
                };
                let control_line = p.item.control == Some(1);
                self.run_batch(id, p.item, p.rest);
                if control_line && !self.parked.contains_key(&id) {
                    self.control_read(id);
                }
            }
        }
    }

    /// Whether `id` has a blocked batch.
    pub(crate) fn is_parked(&self, id: ClientId) -> bool {
        self.parked.contains_key(&id)
    }

    /// Run a client's batch; park it if a command blocks, else hand its
    /// results back.
    pub(crate) fn run_batch(&mut self, id: ClientId, mut item: Item, mut calls: Vec<CommandCall>) {
        match commands::run(self, &mut item, &calls) {
            Some(at) => {
                let rest = calls.split_off(at);
                if item.control.is_none() {
                    // What ran so far reaches the client now, as tmux
                    // writes each command's output as it runs.
                    self.send_output(id, &mut item);
                }
                self.parked.insert(id, Parked { item, rest });
            }
            None => match item.control {
                Some(0) => self.control_command_done(id, item),
                Some(_) => {
                    self.control_take_output(id, &mut item);
                    self.control_flush(id);
                }
                None => self.deliver(id, item),
            },
        }
    }

    /// Queue a message; write what the socket takes now.
    pub(crate) fn send(&mut self, id: ClientId, msg: &ServerMsg) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
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
                c.outbuf.extend_from_slice(&bytes);
            }
        }
        self.flush_client(id);
    }

    fn flush_client(&mut self, id: ClientId) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        while !c.outbuf.is_empty() {
            match c.stream.write(&c.outbuf) {
                Ok(0) => break,
                Ok(n) => {
                    c.outbuf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    c.outbuf.clear();
                    c.closing = true;
                    break;
                }
            }
        }
        let interest = if c.outbuf.is_empty() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        let _ = self
            .poll
            .registry()
            .reregister(&mut c.stream, Token(CLIENT_BASE + id.0), interest);
        if c.closing && c.outbuf.is_empty() {
            self.drop_client(id);
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
            ClientMsg::Resize { cols, rows } => self.resize_client(id, cols, rows),
            ClientMsg::Stdin(bytes) => self.control_stdin(id, &bytes),
            ClientMsg::StdinEof => self.control_stdin_eof(id),
            ClientMsg::Detach => self.detach_client(id),
            ClientMsg::FrameAck { seq } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.unacked.ack(seq);
                }
            }
        }
    }

    fn fail_client(&mut self, id: ClientId, e: &EdgeError) {
        let mut line = e.to_bytes();
        line.push(b'\n');
        self.send(id, &ServerMsg::Stderr(line));
        self.finish_client(id, 1);
    }

    pub(crate) fn finish_client(&mut self, id: ClientId, code: i32) {
        self.send(
            id,
            &ServerMsg::Exit {
                code,
                message: None,
            },
        );
        if let Some(c) = self.clients.get_mut(&id) {
            c.closing = true;
        }
        self.flush_client(id);
    }

    fn on_hello(&mut self, id: ClientId, hello: Hello) {
        if let Err(e) = hello.check_version() {
            self.fail_client(id, &e);
            return;
        }
        let origin = Origin {
            attached: None,
            inside_pane: hello
                .env
                .iter()
                .find(|(k, _)| k == "TMUX_PANE")
                .and_then(|(_, v)| v.parse().ok()),
        };
        let mut item = Item::new(Some(id), hello.cwd.clone());
        item.env.clone_from(&hello.env);
        item.tty.clone_from(&hello.tty);
        item.utf8 = hello.flags.utf8;
        item.origin = origin;
        let argv = hello.argv.clone();
        let control = hello.flags.control != vtmux_core::protocol::ControlMode::None;
        let cwd = hello.cwd.clone();
        if let Some(c) = self.clients.get_mut(&id) {
            c.hello = Some(hello);
        }
        // The first client loads the configuration before its own command
        // runs (server-client.c:3052-3060, cfg.c start_cfg).
        if !self.config_loaded {
            self.load_config(id, origin, &cwd);
        }
        if control {
            // The argv command runs with guard flags 0 (cmd-queue.c:594).
            self.control_start(id);
            item.control = Some(0);
        }
        // Config causes wait for an attached session (cfg.c
        // cfg_show_causes): a command client never sees them.
        // No command: new-session (server_client_default_command).
        let argv = if argv.is_empty() {
            vec!["new-session".to_owned()]
        } else {
            argv
        };
        match cmd::parse_argv(&argv) {
            Ok(calls) => self.run_batch(id, item, calls),
            Err(e) => {
                item.error(self, cmd_error_bytes(&e));
                self.run_batch(id, item, Vec::new());
            }
        }
    }

    fn send_output(&mut self, id: ClientId, item: &mut Item) {
        for out in std::mem::take(&mut item.output) {
            let msg = match out {
                Output::Stdout(b) => ServerMsg::Stdout(b),
                Output::Stderr(b) => ServerMsg::Stderr(b),
            };
            self.send(id, &msg);
        }
    }

    fn deliver(&mut self, id: ClientId, mut item: Item) {
        self.send_output(id, &mut item);
        if item.after == After::Exit || self.shutdown {
            self.finish_client(id, item.status);
        }
    }

    // ------------------------------------------------------- attach hooks

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
            .ok_or_else(|| {
                EdgeError::OpenTerminalFailed(
                    "unsupported terminal (xterm*, xterm-ghostty, screen, screen-256color, tmux-256color)".into(),
                )
            })?;
        client.caps = Some(caps);
        client.term = Some(term);
        client.compositor.invalidate();
        client.attached = Some(s);
        // server_client_set_session: session_update_activity (server-client.c:468).
        self.model.touch_session(s);
        // Caps::startup ends with a DECRQM query (tty.c tty_send_requests).
        client.decoder.query_sent(Instant::now());
        self.send(id, &ServerMsg::Ready);
        // tty.c tty_start_tty / tty_stop_tty; reference PTY probe pins the
        // set of modes, not their ordering.
        let mut startup = caps.startup();
        // tty_start_tty (tty.c:563-564): ENFCS asks the terminal for focus
        // reports when `focus-events` is on.
        if self.model.number_option(TreeId::Server, "focus-events") != 0 {
            startup.extend_from_slice(b"\x1b[?1004h");
        }
        self.send(id, &ServerMsg::Frame(startup));
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
        // server_client_set_session: status_timer_start, then
        // server_redraw_client.
        self.status_timer_start(id);
        self.redraw_client(id);
        Ok(())
    }

    /// The sessions with a client (`s->attached != 0`).
    fn attached_sessions(&self) -> std::collections::BTreeSet<SessionId> {
        self.clients.values().filter_map(|c| c.attached).collect()
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
            // TODO(hooks): events_fire_winlink(a.kind.hook(), wl) once
            // hooks run; alert-activity, alert-bell and alert-silence are
            // in the options table and can be set, but nothing runs them.
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

    /// `MSG_RESIZE` (server-client.c:2712-2730): the client's terminal
    /// takes its new size and every window is fitted again
    /// (`recalculate_sizes`); the loop's check pass applies it.
    pub(crate) fn resize_client(&mut self, id: ClientId, cols: u32, rows: u32) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if let Some(tty) = client.hello.as_mut().and_then(|h| h.tty.as_mut()) {
            tty.cols = cols.max(1);
            tty.rows = rows.max(1);
        }
        client.compositor.invalidate();
        // server-client.c:2729: server_redraw_client.
        client.status.request_full_redraw();
        self.recalculate_sizes();
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

    /// `recalculate_sizes` (resize.c:426-466): [`Self::recalculate_window`]
    /// for every window.
    pub fn recalculate_sizes(&mut self) {
        let windows: Vec<_> = self.model.windows.keys().copied().collect();
        for w in windows {
            self.recalculate_window(w);
        }
    }

    /// `recalculate_size(w, 0)` (resize.c:359-424) with `latest` semantics:
    /// the last terminal client attached to a session that links `w` sizes
    /// it, less its status lines; no such client leaves it alone. The size
    /// is only recorded (`WINDOW_RESIZE`) when it differs from the one
    /// already pending, else from the window's own;
    /// [`Self::check_window_resizes`] applies it.
    pub fn recalculate_window(&mut self, w: WindowId) {
        let size = self
            .clients
            .values()
            .rev()
            .filter(|c| c.control.is_none())
            .filter(|c| {
                c.attached
                    .and_then(|s| self.model.sessions.get(&s))
                    .is_some_and(|s| s.windows.values().any(|x| *x == w))
            })
            .find_map(|c| {
                let tty = c.hello.as_ref()?.tty.as_ref()?;
                let lines = self.status_line_size(c);
                Some((tty.cols.max(1), tty.rows.saturating_sub(lines).max(1)))
            });
        let Some(size) = size else {
            return;
        };
        let Some(window) = self.model.windows.get_mut(&w) else {
            return;
        };
        if window.pending_size.unwrap_or((window.sx(), window.sy())) != size {
            window.pending_size = Some(size);
        }
    }

    /// `server_client_check_window_resize` (server-client.c:1953-1970),
    /// run every loop before drawing: a window with a pending size takes
    /// it once it is the current window of a session with a client
    /// attached. A background window keeps its size (and its panes get no
    /// SIGWINCH) until then.
    fn check_window_resizes(&mut self) {
        let current: Vec<WindowId> = self
            .clients
            .values()
            .filter_map(|c| c.attached)
            .filter_map(|s| self.model.current_of(s))
            .map(|(_, w, _)| w)
            .collect();
        for w in current {
            if let Some((sx, sy)) = self.model.windows.get(&w).and_then(|x| x.pending_size) {
                self.model.resize_window(w, sx, sy);
            }
        }
    }

    pub fn detach_session_clients(&mut self, session: SessionId) {
        let ids: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| (c.attached == Some(session)).then_some(id))
            .collect();
        for id in ids {
            self.detach_client(id);
        }
    }

    pub fn detach_client(&mut self, id: ClientId) {
        if self.is_control(id) {
            self.control_detach(id);
            return;
        }
        let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
            return;
        };
        let message = self
            .model
            .sessions
            .get(&s)
            .map(|s| format!("detached (from session {})", s.name));
        self.exit_attached(id, 0, message);
    }

    fn exit_attached(&mut self, id: ClientId, code: i32, message: Option<String>) {
        if let Some(caps) = self.clients.get(&id).and_then(|c| c.caps) {
            self.send(id, &ServerMsg::Frame(caps.teardown()));
        }
        self.send(id, &ServerMsg::Exit { code, message });
        if let Some(c) = self.clients.get_mut(&id) {
            c.attached = None;
            c.closing = true;
        }
        // server_client_set_session(c, NULL) then recalculate_sizes
        // (server-client.c:2733-2738): the clients left size the windows.
        self.recalculate_sizes();
        self.flush_client(id);
    }

    /// Input from an attached terminal client: decode it into keys
    /// (tty-keys.c) and hand each to the key tables
    /// ([`Server::handle_key`], `server_client_key_callback`).
    pub fn on_client_input(&mut self, id: ClientId, bytes: &[u8]) -> Result<(), EdgeError> {
        let Some(s) = self
            .clients
            .get(&id)
            .filter(|c| c.control.is_none())
            .and_then(|c| c.attached)
        else {
            return Ok(());
        };
        self.model.current_of(s).ok_or(EdgeError::NoCurrentTarget)?;
        let c = self
            .clients
            .get_mut(&id)
            .ok_or(EdgeError::NoCurrentClient)?;
        c.decoder.feed(bytes);
        let now = Instant::now();
        let events = c.decoder.drain(now);
        if !events.is_empty() {
            // The repeat timer fires before keys that arrive after it.
            self.expire_repeat(now);
        }
        for event in events {
            let Some(c) = self.clients.get_mut(&id).filter(|c| c.attached.is_some()) else {
                break;
            };
            match event {
                crate::input::Event::Ignore => {}
                crate::input::Event::Sync(sync) => {
                    // tty_keys_sync: only a supported reply changes anything.
                    if let (true, Some(term)) = (sync, &mut c.term) {
                        term.sync_supported();
                        c.caps = Some(crate::render::Caps::from_term(term));
                    }
                }
                crate::input::Event::Paste(start) => {
                    c.pasting = start;
                    // server_client_is_bracket_paste: the markers reach a
                    // pane that asked for bracketed paste.
                    if let Some(p) = self.client_pane(id)
                        && self.model.panes[&p].term.bracketed_paste()
                    {
                        self.write_pane(p, if start { b"\x1b[200~" } else { b"\x1b[201~" });
                    }
                }
                crate::input::Event::Focus(focused) => {
                    // tty_keys_next (tty-keys.c:1030-1039), then the key
                    // callback's activity update; focus events are not
                    // keys and reach no table (server-client.c:1523).
                    c.focused = focused;
                    self.touch_client(id);
                    self.update_focus(self.client_window(id));
                }
                crate::input::Event::Key(key) => {
                    // server_client_handle_key0: a message goes with the
                    // key, or swallows it with -N.
                    if c.status.key() {
                        continue;
                    }
                    let pasting = c.pasting;
                    self.touch_client(id);
                    if (pasting || !self.handle_key(id, key.to_core()))
                        && let Some(p) = self.client_pane(id)
                    {
                        self.forward_key(p, &key);
                    }
                }
            }
        }
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
        }
    }

    /// `session_set_current` and `window_set_active_pane` run
    /// `window_update_focus` for the windows they change when `focus-events`
    /// is on; looking at every window after a command finds the same changes.
    pub(crate) fn update_focus_all(&mut self) {
        if self.model.number_option(TreeId::Server, "focus-events") == 0 {
            return;
        }
        let windows: Vec<_> = self.model.windows.keys().copied().collect();
        for w in windows {
            self.update_focus(Some(w));
        }
    }

    /// The window a client is looking at.
    pub(crate) fn client_window(&self, id: ClientId) -> Option<WindowId> {
        let s = self.clients.get(&id)?.attached?;
        Some(self.model.current_of(s)?.1)
    }

    /// `window_update_focus` (window.c:707-713, 716-752): the window's
    /// active pane is focused when a focused client looks at its window;
    /// a change is reported to a pane that asked for it (`?1004h`).
    pub(crate) fn update_focus(&mut self, w: Option<WindowId>) {
        let Some(window) = w.and_then(|w| self.model.windows.get(&w)) else {
            return;
        };
        let p = window.active();
        let looking = self.clients.values().any(|c| {
            c.focused
                && c.attached
                    .and_then(|s| self.model.current_of(s))
                    .is_some_and(|(_, cw, _)| Some(cw) == w)
        });
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        let focused = looking && !pane.exited;
        if focused == pane.focused || pane.exited {
            return;
        }
        pane.focused = focused;
        if pane.term.focus_events() {
            if let Some(io) = pane.io.as_mut() {
                io.write(if focused { b"\x1b[I" } else { b"\x1b[O" });
            }
            self.pane_output_queued(p);
        }
    }

    /// The pane a client's keys go to (`cmd_find_from_client`).
    pub(crate) fn client_pane(&self, id: ClientId) -> Option<PaneId> {
        let s = self.clients.get(&id)?.attached?;
        Some(self.model.current_of(s)?.2)
    }

    /// `window_pane_key` with no mode: the key encoded for the pane.
    pub(crate) fn forward_key(&mut self, p: PaneId, key: &crate::keys::Key) {
        let Some(pane) = self.model.panes.get(&p) else {
            return;
        };
        let modes = crate::keys::KeyModes {
            app_cursor: pane.term.app_cursor(),
        };
        self.write_pane(p, &crate::keys::encode(key, modes));
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

    fn render_clients(&mut self) {
        let attached: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| c.control.is_none())
            .filter_map(|(&id, c)| c.attached.map(|s| (id, s)))
            .collect();
        for (id, s) in attached {
            if self.shutdown {
                self.exit_attached(id, 1, Some("server exited".into()));
                continue;
            }
            if self.model.current_of(s).is_none() {
                self.exit_attached(id, 0, Some("exited".into()));
                continue;
            }
            let client = &self.clients[&id];
            let Some(tty) = client.hello.as_ref().and_then(|h| h.tty.as_ref()) else {
                continue;
            };
            let Some(caps) = client.caps else { continue };
            let cols = u16::try_from(tty.cols).unwrap_or(u16::MAX).max(1);
            let rows = u16::try_from(tty.rows).unwrap_or(u16::MAX).max(1);
            let queued = client.unacked.bytes;
            let lines = u16::try_from(self.status_line_size(client)).unwrap_or(u16::MAX);
            if !self
                .clients
                .get_mut(&id)
                .expect("client")
                .compositor
                .ready(queued, cols, rows)
            {
                continue;
            }
            let mut screen = vtmux_vt::Snapshot {
                cols,
                rows,
                cells: vec![vtmux_vt::Cell::default(); usize::from(cols) * usize::from(rows)],
                wrapped: Vec::new(),
                cursor: None,
                cursor_visible: true,
            };
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
            let client = self.clients.get_mut(&id).expect("client");
            let held = client
                .status
                .hold_window()
                .then(|| client.compositor.shown_rows(cols, rows, scene_y, area))
                .flatten()
                .map(<[vtmux_vt::Cell]>::to_vec);
            self.draw_window(s, caps, &mut screen, scene_y, area, held.as_deref());
            self.draw_status(id, s, caps, &mut screen, lines, top);
            let frame = self
                .clients
                .get_mut(&id)
                .expect("client")
                .compositor
                .frame(screen, caps);
            if !frame.is_empty() {
                self.send(id, &ServerMsg::Frame(frame));
            }
        }
    }

    /// Session `s`'s current window composed into rows
    /// `y..y + rows` of `screen`, with the active pane's cursor. `held` is
    /// what the terminal shows in those rows while pane output is frozen:
    /// pane content cells keep it.
    fn draw_window(
        &self,
        s: SessionId,
        caps: crate::render::Caps,
        screen: &mut vtmux_vt::Snapshot,
        y: u16,
        rows: u16,
        held: Option<&[vtmux_vt::Cell]>,
    ) {
        let source = crate::render::window::Source {
            model: &self.model,
            info: &self.info,
            env: &self.global_env,
        };
        let Some((scene, draw)) = source.window(s, screen.cols, rows, caps) else {
            return;
        };
        let theme = source.theme(caps);
        let cols = usize::from(screen.cols);
        let mut window = vtmux_vt::Snapshot {
            rows,
            cells: vec![vtmux_vt::Cell::default(); cols * usize::from(rows)],
            ..screen.clone()
        };
        crate::render::compose::compose(&scene, &draw, &theme, &mut window);
        crate::render::window::cursor(&mut window, &draw, &self.model, rows);
        if let Some(held) = held {
            for (i, (cell, shown)) in window.cells.iter_mut().zip(held).enumerate() {
                if let Some(crate::render::scene::SceneCell::Pane { pane, .. }) = scene.cells.get(i)
                    && !self.model.redraw_panes.contains(pane)
                {
                    cell.clone_from(shown);
                }
            }
        }
        let at = usize::from(y) * cols;
        screen.cells[at..at + window.cells.len()].clone_from_slice(&window.cells);
        screen.cursor = window.cursor.map(|(x, cy)| (x, cy + y));
        screen.cursor_visible = window.cursor_visible;
    }

    /// The client's status lines, or its message, over `screen`
    /// ([`crate::render::status::draw`]), expanded in the client's format
    /// tree when they need drawing again.
    fn draw_status(
        &mut self,
        id: ClientId,
        s: SessionId,
        caps: crate::render::Caps,
        screen: &mut vtmux_vt::Snapshot,
        lines: u16,
        top: bool,
    ) {
        let Some(mut status) = self
            .clients
            .get_mut(&id)
            .map(|c| std::mem::take(&mut c.status))
        else {
            return;
        };
        {
            let source = crate::render::window::Source {
                model: &self.model,
                info: &self.info,
                env: &self.global_env,
            };
            let mut ctx =
                self.format_ctx(crate::target::Found::from_session(&self.model, s), Some(id));
            status.draw(&source, &mut ctx, s, caps, screen, lines, top);
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
                        Redraw::Window(w) => {
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
            let full = matches!(r, Redraw::Session(_) | Redraw::Window(_));
            for id in ids {
                if full {
                    self.redraw_client(id);
                } else {
                    self.status_redraw_client(id);
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
    pub fn client_views(&self) -> std::rc::Rc<[ClientView]> {
        self.clients
            .iter()
            .filter(|(_, c)| c.attached.is_some())
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
            // sets it for a terminal.
            width: if started {
                tty.map_or(80, |t| t.cols)
            } else {
                80
            },
            height: started.then(|| tty.map_or(24, |t| t.rows)),
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
            colours: c.caps.map(|caps| caps.colours),
        }
    }

    /// `cmd_find_best_client` (cmd-find.c:112-130): the most recently active
    /// client attached to `s`, or to any session when `s` has none.
    pub fn best_client(&self, s: Option<SessionId>) -> Option<ClientId> {
        let s = s.filter(|&s| self.clients.values().any(|c| c.attached == Some(s)));
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
            let hit = self.client_name(id) == target
                || (!tty.is_empty()
                    && (tty == target || tty.strip_prefix("/dev/") == Some(target)));
            hit.then_some(id)
        })
    }

    /// A format context over `found` with `client` as `ft->c`.
    pub fn format_ctx(&self, found: crate::target::Found, client: Option<ClientId>) -> Ctx<'_> {
        let mut ctx = Ctx::new(&self.model, &self.info, &self.global_env, found);
        ctx.clients = self.client_views();
        ctx.client = client.and_then(|id| Some(self.client_view(id, self.clients.get(&id)?)));
        ctx
    }

    /// Most recent attached client (`cmd_find_current_client`, M0 one client).
    pub fn current_client(&self) -> Option<ClientId> {
        self.clients
            .iter()
            .rev()
            .find_map(|(&id, c)| c.attached.map(|_| id))
    }

    // ------------------------------------------------------------- config

    /// cfg.c `start_cfg` / `load_cfg`: each file runs with no client but
    /// is parsed for `first`, the client that started the server (`pi.c`,
    /// attached or not), so `%if` conditions see it; errors become causes
    /// `file:line: message`.
    fn load_config(&mut self, first: ClientId, origin: Origin, cwd: &Path) {
        self.config_loaded = true;
        let files = self.config.config_files.clone();
        let quiet = self.config.config_quiet;
        let mut item = Item::new(None, cwd.to_owned());
        let opts = commands::source_file::LoadOpts {
            quiet,
            parse_client: Some((first, origin)),
            ..Default::default()
        };
        for file in files {
            commands::source_file::load(self, &mut item, &file, opts);
        }
    }

    // --------------------------------------------------------------- panes

    /// Create a session with one window and one pane.
    pub fn create_session(&mut self, spec: NewSession) -> io::Result<SessionId> {
        let id = self.model.session_ids.allocate();
        let name = spec.name.unwrap_or_else(|| {
            // session_create (session.c:138-146): the ID, skipping names
            // already taken.
            let mut n = id.0;
            loop {
                let name = n.to_string();
                if self.model.session_named(&name).is_none() {
                    break name;
                }
                n += 1;
            }
        });
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
        })?;
        self.insert_window(window_id, pane, spec.window_name, cols, rows);
        // spawn_window with idx -1: the first free index from base-index
        // (spawn.c:183-185); the session is new, so that is base-index.
        let index = u32::try_from(
            self.model
                .number_option(TreeId::GlobalSession, "base-index"),
        )
        .unwrap_or(0);
        let activity = self.model.touch();
        let before = self.control_links();
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
        self.control_links_changed(&before);
        Ok(id)
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
        self.model.panes.insert(pane_id, pane);
        let activity = self.model.touch();
        self.model.windows.insert(
            window_id,
            Window {
                id: window_id,
                name,
                activity,
                layout: CoreLayout::new(cols, rows, pane_id),
                panes: WindowPanes::new(pane_id),
                zoom: None,
                last_layout: None,
                old_layout: None,
                pending_size: None,
                alerts: crate::alerts::WindowAlerts::default(),
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
        let before = self.control_links();
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
        let (cols, rows) = self.default_window_size(s);
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
            })
            .map_err(|e| format!("fork failed: {e}"))?;
        self.insert_window(window_id, pane, nw.name.clone(), cols, rows);
        let session = self.model.sessions.get_mut(&s).expect("target session");
        session.windows.insert(index, window_id);
        if !session.windows.contains_key(&session.current) {
            session.current = index;
        }
        if !detached {
            self.model.set_current(s, index);
        }
        self.control_links_changed(&before);
        Ok(index)
    }

    /// `default_window_size` (resize.c:291-343) with `window-size latest`:
    /// a terminal client attached to the session, less its status lines,
    /// else the session's `default-size`, else 80x24; clamped to
    /// `WINDOW_MINIMUM..=WINDOW_MAXIMUM`.
    fn default_window_size(&self, s: SessionId) -> (u32, u32) {
        const WINDOW_MAXIMUM: u32 = 10_000;
        let client = self
            .clients
            .values()
            .rev()
            .filter(|c| c.control.is_none() && c.attached == Some(s))
            .find_map(|c| Some((c.hello.as_ref()?.tty.as_ref()?, self.status_line_size(c))));
        let (sx, sy) = if let Some((tty, lines)) = client {
            (tty.cols, tty.rows.saturating_sub(lines))
        } else {
            let size = self
                .model
                .options
                .get(TreeId::Session(s), "default-size")
                .map(|e| e.value.to_text(e.def, None, false))
                .unwrap_or_default();
            size.split_once('x')
                .and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)))
                .unwrap_or((80, 24))
        };
        (sx.clamp(1, WINDOW_MAXIMUM), sy.clamp(1, WINDOW_MAXIMUM))
    }

    /// `server_kill_window` (server-fn.c:239-262) with the PTYs closed and
    /// control clients told.
    pub fn kill_window(&mut self, w: WindowId, renumber: bool) {
        let before = self.control_links();
        let panes = self.model.kill_window(w, renumber);
        self.control_links_changed(&before);
        for pane in panes {
            self.close_pane_io(pane);
        }
        self.recalculate_sizes();
    }

    /// `spawn_pane` (spawn.c:216-617) for a new pane: the child environment
    /// (`environ_for_session` plus the spawn additions), the shell from
    /// `default-shell`, the PTY sized to `rect`, and a terminal whose title
    /// starts as the host name (window.c:1479-1480). The caller links the
    /// returned pane into the model.
    pub fn spawn_pane(&mut self, ps: &PaneSpawn<'_>) -> io::Result<Pane> {
        let mut child = self.global_env.clone();
        ps.session_env.copy_into(&mut child);
        // environ_for_session (environ.c:243-250): TERM from default-terminal.
        let term = self
            .model
            .options
            .get(TreeId::Server, "default-terminal")
            .map_or_else(
                || "tmux-256color".to_owned(),
                |e| e.value.to_text(e.def, None, false),
            );
        child.set("TERM", &term, false);
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
        if child.get("PATH").is_none() {
            child.set("PATH", "/usr/bin:/bin:/usr/sbin:/sbin", false);
        }
        // The shell and history budget come from the session's options,
        // not the starting client's shell.
        let session_tree = TreeId::Session(ps.session);
        let configured_shell = self
            .model
            .options
            .get(session_tree, "default-shell")
            .map_or_else(
                || "/bin/sh".to_owned(),
                |e| e.value.to_text(e.def, None, false),
            );
        let shell = if let Some(kept) = &ps.shell {
            kept.clone()
        } else if check_shell(Path::new(&configured_shell)) {
            PathBuf::from(configured_shell)
        } else {
            PathBuf::from("/bin/sh")
        };
        child.set("SHELL", &shell.to_string_lossy(), false);

        let cols = u16::try_from(ps.rect.sx).unwrap_or(u16::MAX).max(1);
        let rows = u16::try_from(ps.rect.sy).unwrap_or(u16::MAX).max(1);
        let spawned = sys::pty::spawn(&sys::pty::SpawnSpec {
            argv: ps.argv.clone(),
            shell: shell.clone(),
            cwd: ps.cwd.clone(),
            home: find_home(&self.global_env),
            env: child.for_process(),
            cols,
            rows,
        })?;
        let io = PaneIo::new(spawned.master);
        let history_limit =
            usize::try_from(self.model.number_option(session_tree, "history-limit")).unwrap_or(0);
        let mut term = PaneTerm::new(cols, rows, history_limit);
        let host = &self.info.host;
        if !host.chars().any(char::is_control) {
            term.feed(format!("\x1b]2;{host}\x1b\\").as_bytes());
        }
        io.register(self.poll.registry(), Token(PANE_BASE + ps.pane.0 as usize))?;
        Ok(Pane {
            id: ps.pane,
            window: ps.window,
            term,
            io: Some(io),
            pid: spawned.pid.as_raw_nonzero().get(),
            argv: ps.argv.clone(),
            shell,
            cwd: ps.cwd.clone(),
            exited: false,
            changed: false,
            last_rename: None,
            rect: ps.rect,
            history_limit,
            input_off: false,
            focused: false,
            active_point: 0,
        })
    }

    /// `server_destroy_session` + `session_destroy`: its panes' processes
    /// get SIGHUP when their PTYs close.
    pub fn destroy_session(&mut self, s: SessionId) {
        let before = self.control_links();
        let panes = self.model.remove_session(s);
        self.control_links_changed(&before);
        for pane in panes {
            self.close_pane_io(pane);
        }
    }

    /// `server_destroy_pane` with remain-on-exit off: hang up the pane,
    /// take it out of its window; the window and an emptied session go
    /// with the last pane.
    pub fn destroy_pane(&mut self, p: PaneId) {
        self.hangup_pane(p);
        let before = self.control_links();
        let panes = self.model.remove_pane(p);
        self.control_links_changed(&before);
        for pane in panes {
            self.close_pane_io(pane);
        }
    }

    /// `respawn-pane` (spawn.c:312-345 with `SPAWN_RESPAWN|SPAWN_KILL`):
    /// close the pane's PTY (its process gets SIGHUP), reset its screen
    /// (`screen_reinit`: history and title stay) and start `ps` in it. The
    /// pane keeps its ID, place and terminal; the old process's exit no
    /// longer matches the pane, which now has the new pid.
    pub fn respawn_pane(&mut self, ps: &PaneSpawn<'_>) -> io::Result<()> {
        let p = ps.pane;
        self.hangup_pane(p);
        let fresh = self.spawn_pane(ps)?;
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return Ok(());
        };
        // screen_reinit and a fresh input parser: alternate screen off,
        // scroll region, cursor home, visible lines cleared, modes back to
        // cursor+wrap, default attributes, default tabs.
        pane.term.feed(
            b"\x1b[?1049l\x1b[r\x1b[m\x1b[2J\x1b[H\x1b[?7h\x1b[?25h\x1b[?6l\x1b[?1l\x1b>\x1b[4l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l",
        );
        let _ = pane.term.take_replies();
        pane.io = fresh.io;
        pane.pid = fresh.pid;
        pane.argv = fresh.argv;
        pane.shell = fresh.shell;
        pane.cwd = fresh.cwd;
        pane.exited = false;
        pane.changed = true;
        Ok(())
    }

    pub(crate) fn close_pane_io(&self, mut pane: Pane) {
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

    fn hangup_pane(&mut self, p: PaneId) {
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

    /// Read a bounded amount from pane `p` (tmux reads once per libevent
    /// callback), so a pane flooding output cannot starve clients.
    fn pane_readable(&mut self, p: PaneId) {
        const READS_PER_TURN: usize = 1;
        let mut closed = false;
        for turn in 0.. {
            if turn == READS_PER_TURN {
                self.backlog.insert(p);
                break;
            }
            let Some(pane) = self.model.panes.get_mut(&p) else {
                return;
            };
            let Some(io) = pane.io.as_mut() else { return };
            match io.read() {
                ReadResult::Data(bytes) => {
                    let before = (pane.term.title(), pane.term.pwd());
                    pane.term.feed(&bytes);
                    // input.c:2725-2740: a new title (OSC 0/2) or path
                    // (OSC 7) redraws the status of the window's sessions.
                    let retitled = (pane.term.title(), pane.term.pwd()) != before;
                    pane.changed = true;
                    let belled = pane.term.take_bells() != 0;
                    let redraw = pane.term.take_redraw();
                    let replies = pane.term.take_replies();
                    if !replies.is_empty() {
                        io.write(&replies);
                    }
                    // input_parse_buffer -> window_update_activity on every
                    // non-empty read (input.c:1053): W/t sorts and lsw -O
                    // activity order by it.
                    let window = pane.window;
                    let stamp = self.model.touch();
                    if let Some(w) = self.model.windows.get_mut(&window) {
                        w.activity = stamp;
                    }
                    self.model.alerts_queue(window, alerts::ACTIVITY);
                    if redraw {
                        self.model.redraw_panes.insert(p);
                    }
                    if retitled {
                        self.model
                            .redraw(crate::model::Redraw::WindowStatus(window));
                    }
                    if belled {
                        // input_c0_dispatch BEL: alerts_queue(WINDOW_BELL).
                        self.model.alerts_queue(window, alerts::BELL);
                    }
                    self.control_output(p, &bytes);
                }
                ReadResult::WouldBlock => break,
                ReadResult::Closed => {
                    closed = true;
                    break;
                }
            }
        }
        self.pane_output_queued(p);
        if closed {
            // window_pane_error_callback: the PTY is gone; destroy the pane
            // (server_destroy_pane, remain-on-exit off).
            self.destroy_pane(p);
        }
    }

    fn on_signals(&mut self) {
        let pending: Vec<i32> = self.signals.pending().collect();
        for sig in pending {
            match sig {
                SIGCHLD => self.reap_children(),
                // server_signal: SIGTERM/SIGINT exit (server.c:442-446).
                SIGTERM | SIGINT => self.shutdown = true,
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
        let _ = self.poll.registry().deregister(&mut self.listener);
        if self
            .poll
            .registry()
            .register(&mut fresh, LISTENER, Interest::READABLE)
            .is_ok()
        {
            self.listener = fresh;
        }
    }

    /// `server_child_signal`: reap every exited child; a pane whose process
    /// is gone and whose PTY is drained is destroyed.
    fn reap_children(&mut self) {
        loop {
            match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => {
                    if status.stopped() {
                        continue;
                    }
                    let raw = pid.as_raw_nonzero().get();
                    let found = self
                        .model
                        .panes
                        .values_mut()
                        .find(|p| p.pid == raw)
                        .map(|p| {
                            p.exited = true;
                            p.id
                        });
                    if let Some(p) = found {
                        // Drain what is left, then the read side reports
                        // the close (EIO) and destroys the pane.
                        self.pane_readable(p);
                        let drained = self
                            .model
                            .panes
                            .get(&p)
                            .and_then(|x| x.io.as_ref())
                            .is_none_or(|io| io.readable_bytes() == 0);
                        if drained && self.model.panes.contains_key(&p) {
                            self.destroy_pane(p);
                        }
                    }
                }
                Ok(None) | Err(_) => return,
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
            if let Some(window) = self.model.windows.get_mut(&w)
                && window.name != name
            {
                window.name.clone_from(&name);
                self.control_window_renamed(w, &name);
                // names.c:100.
                self.model.redraw(crate::model::Redraw::WindowStatus(w));
            }
        }
    }
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
pub fn run_server(listener: StdListener, socket: PathBuf, config: ServerConfig) -> i32 {
    match Server::new(listener, socket, config) {
        Ok(mut server) => match server.run() {
            Ok(()) => 0,
            Err(_) => 1,
        },
        Err(_) => 1,
    }
}
