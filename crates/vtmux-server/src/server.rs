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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use signal_hook::consts::{SIGCHLD, SIGCONT, SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2, SIGWINCH};
use signal_hook_mio::v1_0::Signals;
use vtmux_core::cmd::{self, CmdError, ConfigEnv};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, SessionId};
use vtmux_core::protocol::{self, ClientMsg, Hello, ProtocolError, ServerMsg};
use vtmux_vt::PaneTerm;

use crate::commands::{self, After, Item, Output};
use crate::environ::{Environ, session_environ};
use crate::format_ctx::ServerInfo;
use crate::model::{Layout, Model, Pane, Session, Window};
use crate::names::parse_window_name;
use crate::pane_io::{PaneIo, ReadResult};
use crate::sys;
use crate::target::Origin;

const LISTENER: Token = Token(0);
const SIGNALS: Token = Token(1);
/// Client tokens start here; pane tokens are `PANE_BASE + pane id`.
const CLIENT_BASE: usize = 1 << 32;
const PANE_BASE: usize = 16;

/// `history-limit` default (options-table.c).
const HISTORY_LIMIT: usize = 2000;
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

struct Client {
    stream: UnixStream,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    /// Exit has been queued; close once the output is flushed.
    closing: bool,
    hello: Option<Hello>,
    attached: Option<SessionId>,
    prefix: bool,
    pasting: bool,
    decoder: crate::input::Decoder,
    caps: Option<crate::render::Caps>,
    compositor: crate::render::Compositor,
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
    clients: BTreeMap<ClientId, Client>,
    next_client: usize,
    /// The global `default-shell` (tmux.c `getshell`).
    shell: PathBuf,
    config: ServerConfig,
    config_loaded: bool,
    /// Panes that still had output when their read budget ran out. mio is
    /// edge-triggered, so they are read again on the next turn instead of
    /// waiting for an event that will not come.
    backlog: std::collections::BTreeSet<PaneId>,
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

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// `find_home` (tmux.c:366-384).
fn find_home(env: &Environ) -> Option<PathBuf> {
    env.get("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| sys::user::current_user().map(|p| p.home))
}

/// The server's [`ConfigEnv`]: `$VAR` from the global environment, `~`
/// and `~user` through the passwd database (cmd-parse.y
/// `yylex_token_variable`, `yylex_token_tilde`), assignments into the
/// global environment.
pub struct GlobalEnv<'a>(pub &'a mut Environ);

impl ConfigEnv for GlobalEnv<'_> {
    fn var(&self, name: &str) -> Option<String> {
        self.0.get(name).map(str::to_owned)
    }

    fn home(&self, user: Option<&str>) -> Option<String> {
        let path = match user {
            None => find_home(self.0)?,
            Some(u) => sys::user::user_named(u)?.home,
        };
        Some(path.to_string_lossy().into_owned())
    }

    fn set(&mut self, name: &str, value: &str, hidden: bool) {
        self.0.set(name, value, hidden);
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
        Ok(Self {
            model: Model::default(),
            info: ServerInfo::new(socket.to_string_lossy().into_owned()),
            shell: default_shell(&config.environ),
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
            config,
            config_loaded: false,
            backlog: std::collections::BTreeSet::new(),
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
            let ids: Vec<_> = self.clients.keys().copied().collect();
            for id in ids {
                let _ = self.on_client_input(id, &[]);
            }
            self.check_window_names();
            self.render_clients();
            if self.should_exit() {
                self.exit_clients();
                return Ok(());
            }
        }
    }

    fn next_timeout(&self) -> Option<Duration> {
        if !self.backlog.is_empty() {
            return Some(Duration::ZERO);
        }
        if let Some(deadline) = self
            .clients
            .values()
            .filter_map(|c| c.decoder.deadline())
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

    /// `server_loop` (server.c:268-306): exit when told to, or when there
    /// are no sessions left (`exit-empty`) and no client is waiting.
    fn should_exit(&self) -> bool {
        if self.shutdown {
            return self.clients.values().all(|c| c.outbuf.is_empty());
        }
        self.config_loaded
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
                            closing: false,
                            hello: None,
                            attached: None,
                            prefix: false,
                            pasting: false,
                            decoder: crate::input::Decoder::default(),
                            caps: None,
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

    fn drop_client(&mut self, id: ClientId) {
        if let Some(mut c) = self.clients.remove(&id) {
            let _ = self.poll.registry().deregister(&mut c.stream);
        }
    }

    /// Queue a message; write what the socket takes now.
    fn send(&mut self, id: ClientId, msg: &ServerMsg) {
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
            ClientMsg::Stdin(_) | ClientMsg::StdinEof => {}
            ClientMsg::Detach => self.detach_client(id),
        }
    }

    fn fail_client(&mut self, id: ClientId, e: &EdgeError) {
        let mut line = e.to_bytes();
        line.push(b'\n');
        self.send(id, &ServerMsg::Stderr(line));
        self.finish_client(id, 1);
    }

    fn finish_client(&mut self, id: ClientId, code: i32) {
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
        // The first client loads the configuration before its own command
        // runs (server-client.c:3052-3060, cfg.c start_cfg).
        if !self.config_loaded {
            self.load_config(&hello.cwd);
        }
        let mut item = Item::new(Some(id), hello.cwd.clone());
        item.env.clone_from(&hello.env);
        item.tty.clone_from(&hello.tty);
        item.utf8 = hello.flags.utf8;
        item.origin = Origin {
            attached: None,
            inside_pane: hello
                .env
                .iter()
                .find(|(k, _)| k == "TMUX_PANE")
                .and_then(|(_, v)| v.parse().ok()),
        };
        let argv = hello.argv.clone();
        if let Some(c) = self.clients.get_mut(&id) {
            c.hello = Some(hello);
        }
        // Config causes wait for an attached session (cfg.c
        // cfg_show_causes): a command client never sees them.
        if argv.is_empty() {
            // No command: new-session (server_client_default_command).
            match cmd::parse_argv(&["new-session".to_owned()]) {
                Ok(calls) => commands::run(self, &mut item, &calls),
                Err(e) => item.error(self, e.to_bytes()),
            }
        } else {
            match cmd::parse_argv(&argv) {
                Ok(calls) => commands::run(self, &mut item, &calls),
                Err(e) => item.error(self, cmd_error_bytes(&e)),
            }
        }
        self.deliver(id, item);
    }

    fn deliver(&mut self, id: ClientId, item: Item) {
        for out in item.output {
            let msg = match out {
                Output::Stdout(b) => ServerMsg::Stdout(b),
                Output::Stderr(b) => ServerMsg::Stderr(b),
            };
            self.send(id, &msg);
        }
        if item.after == After::Exit || self.shutdown {
            self.finish_client(id, item.status);
        }
    }

    // ------------------------------------------------------- attach hooks

    /// `cmd_attach_session`: Ready precedes encoded terminal output.
    pub fn attach_client(&mut self, id: ClientId, s: SessionId) -> Result<(), EdgeError> {
        self.model.current_of(s).ok_or(EdgeError::NoCurrentTarget)?;
        let client = self
            .clients
            .get_mut(&id)
            .ok_or(EdgeError::NoCurrentClient)?;
        let caps = client
            .hello
            .as_ref()
            .and_then(crate::render::Caps::from_hello)
            .ok_or_else(|| {
                EdgeError::OpenTerminalFailed(
                    "unsupported terminal (M0: xterm, tmux-256color, screen)".into(),
                )
            })?;
        client.caps = Some(caps);
        client.compositor.invalidate();
        client.attached = Some(s);
        client.prefix = false;
        self.send(id, &ServerMsg::Ready);
        // tty.c tty_start_tty / tty_stop_tty; reference PTY probe pins the
        // set of modes, not their ordering.
        self.send(id, &ServerMsg::Frame(caps.startup()));
        if let Some(tty) = self
            .clients
            .get(&id)
            .and_then(|c| c.hello.as_ref())
            .and_then(|h| h.tty.clone())
        {
            self.resize_client(id, tty.cols, tty.rows);
        }
        Ok(())
    }

    /// `recalculate_sizes` latest semantics (M0 one attached client): status
    /// costs one row; both emulator and kernel PTY get the same dimensions.
    fn resize_client(&mut self, id: ClientId, cols: u32, rows: u32) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if let Some(tty) = client.hello.as_mut().and_then(|h| h.tty.as_mut()) {
            tty.cols = cols.max(1);
            tty.rows = rows.max(1);
        }
        client.compositor.invalidate();
        let Some(session) = client.attached else {
            return;
        };
        let Some((_, window, pane)) = self.model.current_of(session) else {
            return;
        };
        let cols = u16::try_from(cols).unwrap_or(u16::MAX).max(1);
        let rows = u16::try_from(rows.saturating_sub(1))
            .unwrap_or(u16::MAX)
            .max(1);
        if let Some(window) = self.model.windows.get_mut(&window) {
            window.cols = cols;
            window.rows = rows;
        }
        if let Some(pane) = self.model.panes.get_mut(&pane)
            && (pane.term.cols(), pane.term.rows()) != (cols, rows)
        {
            pane.term.resize(cols, rows);
            if let Some(io) = &pane.io {
                let _ = rustix::termios::tcsetwinsize(
                    io.fd(),
                    rustix::termios::Winsize {
                        ws_col: cols,
                        ws_row: rows,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    },
                );
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
        self.flush_client(id);
    }

    /// M0 prefix table: C-b d detaches; other prefixed keys are ignored.
    pub fn on_client_input(&mut self, id: ClientId, bytes: &[u8]) -> Result<(), EdgeError> {
        let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
            return Ok(());
        };
        let (_, _, p) = self.model.current_of(s).ok_or(EdgeError::NoCurrentTarget)?;
        let c = self
            .clients
            .get_mut(&id)
            .ok_or(EdgeError::NoCurrentClient)?;
        c.decoder.feed(bytes);
        let events = c.decoder.drain(Instant::now());
        let term = &self.model.panes[&p].term;
        let modes = crate::keys::KeyModes {
            app_cursor: term.app_cursor(),
        };
        let bracketed = term.bracketed_paste();
        let mut forward = Vec::new();
        for event in events {
            let c = self
                .clients
                .get_mut(&id)
                .ok_or(EdgeError::NoCurrentClient)?;
            match event {
                crate::input::Event::Ignore => {}
                crate::input::Event::Sync(sync) => {
                    if let Some(caps) = &mut c.caps {
                        caps.sync = sync;
                    }
                }
                crate::input::Event::Paste(start) => {
                    c.pasting = start;
                    if bracketed {
                        forward.extend_from_slice(if start { b"\x1b[200~" } else { b"\x1b[201~" });
                    }
                }
                crate::input::Event::Key(key) => {
                    if !c.pasting && c.prefix {
                        c.prefix = false;
                        if key.base == crate::keys::Base::Char('d') && !key.ctrl && !key.meta {
                            self.detach_client(id);
                            return Ok(());
                        }
                    } else if !c.pasting
                        && key.base == crate::keys::Base::Char('b')
                        && key.ctrl
                        && !key.meta
                    {
                        c.prefix = true;
                    } else {
                        forward.extend(crate::keys::encode(&key, modes));
                    }
                }
            }
        }
        if let Some(io) = self.model.panes.get_mut(&p).and_then(|p| p.io.as_mut()) {
            io.write(&forward);
        }
        self.pane_output_queued(p);
        Ok(())
    }

    fn render_clients(&mut self) {
        let attached: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| c.attached.map(|s| (id, s)))
            .collect();
        for (id, s) in attached {
            if self.shutdown {
                self.exit_attached(id, 1, Some("server exited".into()));
                continue;
            }
            let Some((_, _, p)) = self.model.current_of(s) else {
                self.exit_attached(id, 0, Some("exited".into()));
                continue;
            };
            let pane = &self.model.panes[&p];
            let client = &self.clients[&id];
            let Some(tty) = client.hello.as_ref().and_then(|h| h.tty.as_ref()) else {
                continue;
            };
            let Some(caps) = client.caps else { continue };
            let cols = u16::try_from(tty.cols).unwrap_or(u16::MAX).max(1);
            let rows = u16::try_from(tty.rows).unwrap_or(u16::MAX).max(1);
            let mut screen = vtmux_vt::Snapshot {
                cols,
                rows,
                cells: vec![vtmux_vt::Cell::default(); usize::from(cols) * usize::from(rows)],
                cursor: None,
                cursor_visible: true,
            };
            let snap = vtmux_vt::snapshot(&pane.term);
            for y in 0..snap.rows.min(rows.saturating_sub(1)) {
                for x in 0..snap.cols.min(cols) {
                    screen.cells[usize::from(y) * usize::from(cols) + usize::from(x)] =
                        snap.cell(x, y).expect("pane cell").clone();
                }
            }
            screen.cursor = snap
                .cursor
                .filter(|&(x, y)| x < cols && y < rows.saturating_sub(1));
            screen.cursor_visible = snap.cursor_visible && screen.cursor.is_some();
            let found = crate::target::Found::from_session(&self.model, s);
            let ctx = crate::format_ctx::Ctx::new(&self.model, &self.info, &self.global_env, found);
            let left = vtmux_core::format::expand_str("[#{session_name}] ", &ctx);
            let right = vtmux_core::format::trim::trim_left(
                &vtmux_core::format::expand_time("\"#{=21:pane_title}\" %H:%M %d-%b-%y", &ctx),
                40,
            );
            let session = &self.model.sessions[&s];
            let windows: Vec<_> = session
                .windows
                .iter()
                .map(|(&index, w)| {
                    (
                        format!(
                            "{index}:{}{}",
                            self.model.windows[w].name,
                            if index == session.current { "*" } else { " " }
                        ),
                        index == session.current,
                    )
                })
                .collect();
            crate::render::status(&mut screen, &left, &windows, &right, caps);
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

    /// Most recent attached client (`cmd_find_current_client`, M0 one client).
    pub fn current_client(&self) -> Option<ClientId> {
        self.clients
            .iter()
            .rev()
            .find_map(|(&id, c)| c.attached.map(|_| id))
    }

    // ------------------------------------------------------------- config

    /// cfg.c `start_cfg` / `load_cfg`: each file runs with no client;
    /// errors become causes `file:line: message`.
    fn load_config(&mut self, cwd: &Path) {
        self.config_loaded = true;
        let files = self.config.config_files.clone();
        let quiet = self.config.config_quiet;
        let mut item = Item::new(None, cwd.to_owned());
        let opts = commands::source_file::LoadOpts {
            quiet,
            ..Default::default()
        };
        for file in files {
            commands::source_file::load(self, &mut item, &file, opts);
        }
    }

    // --------------------------------------------------------------- panes

    /// Create a session with one window and one pane.
    #[allow(clippy::too_many_lines)] // spawn_window + spawn_pane, in order
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
        let mut env = if spec.update_env {
            session_environ(&spec.client_env)
        } else {
            Environ::default()
        };
        for e in &spec.extra_env {
            if let Some((k, v)) = e.split_once('=') {
                env.set(k, v, false);
            }
        }
        let window_id = self.model.window_ids.allocate();
        let pane_id = self.model.pane_ids.allocate();

        // environ_for_session (environ.c:253-286) + spawn_pane additions.
        let mut child = self.global_env.clone();
        env.copy_into(&mut child);
        child.set("TERM", "tmux-256color", false);
        child.set("TERM_PROGRAM", "tmux", false);
        child.set("TERM_PROGRAM_VERSION", "3.8", false);
        child.set("COLORTERM", "truecolor", false);
        child.set(
            "TMUX",
            &format!("{},{},{}", self.socket.display(), std::process::id(), id.0),
            false,
        );
        child.set("TMUX_PANE", &pane_id.to_string(), false);
        if let Some((_, path)) = spec.client_env.iter().find(|(k, _)| k == "PATH") {
            child.set("PATH", path, false);
        }
        if child.get("PATH").is_none() {
            child.set("PATH", "/usr/bin:/bin:/usr/sbin:/sbin", false);
        }
        let shell = if check_shell(&self.shell) {
            self.shell.clone()
        } else {
            PathBuf::from("/bin/sh")
        };
        child.set("SHELL", &shell.to_string_lossy(), false);

        let spawned = sys::pty::spawn(&sys::pty::SpawnSpec {
            argv: spec.argv.clone(),
            shell: shell.clone(),
            cwd: spec.cwd.clone(),
            home: find_home(&self.global_env),
            env: child.for_process(),
            cols: spec.cols,
            rows: spec.rows,
        })?;
        let io = PaneIo::new(spawned.master);
        // window_pane_create (window.c:1479-1480): the title starts as the
        // host name.
        let mut term = PaneTerm::new(spec.cols, spec.rows, HISTORY_LIMIT);
        let host = &self.info.host;
        if !host.chars().any(char::is_control) {
            term.feed(format!("\x1b]2;{host}\x1b\\").as_bytes());
        }
        io.register(self.poll.registry(), Token(PANE_BASE + pane_id.0 as usize))?;

        // default_window_name (names.c:107-121).
        let (window_name, auto_rename) = if let Some(n) = spec.window_name {
            (n, false)
        } else {
            let cmd = crate::names::stringify_argv(&spec.argv);
            let from = if cmd.is_empty() {
                shell.to_string_lossy().into_owned()
            } else {
                cmd
            };
            (parse_window_name(&from), true)
        };
        self.model.panes.insert(
            pane_id,
            Pane {
                id: pane_id,
                window: window_id,
                term,
                io: Some(io),
                pid: spawned.pid.as_raw_nonzero().get(),
                argv: spec.argv,
                shell,
                cwd: spec.cwd.clone(),
                exited: false,
                changed: false,
                last_rename: None,
            },
        );
        self.model.windows.insert(
            window_id,
            Window {
                id: window_id,
                name: window_name,
                auto_rename,
                layout: Layout {
                    tiled: vec![pane_id],
                    floating: Vec::new(),
                },
                active: pane_id,
                cols: spec.cols,
                rows: spec.rows,
            },
        );
        let activity = self.model.touch();
        self.model.insert_session(Session {
            id,
            name,
            created: now_epoch(),
            activity,
            cwd: spec.cwd,
            windows: BTreeMap::from([(0, window_id)]),
            current: 0,
            env: env.for_process(),
        });
        Ok(id)
    }

    /// `server_destroy_session` + `session_destroy`: its panes' processes
    /// get SIGHUP when their PTYs close.
    pub fn destroy_session(&mut self, s: SessionId) {
        for pane in self.model.remove_session(s) {
            self.close_pane_io(pane);
        }
    }

    fn close_pane_io(&self, mut pane: Pane) {
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
                    pane.term.feed(&bytes);
                    pane.changed = true;
                    let replies = pane.term.take_replies();
                    if !replies.is_empty() {
                        io.write(&replies);
                    }
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
            self.hangup_pane(p);
            if let Some(pane) = self.model.remove_pane(p) {
                self.close_pane_io(pane);
            }
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
                            self.hangup_pane(p);
                            if let Some(pane) = self.model.remove_pane(p) {
                                self.close_pane_io(pane);
                            }
                        }
                    }
                }
                Ok(None) | Err(_) => return,
            }
        }
    }

    /// `check_window_name` (names.c:54-104) with the default
    /// `automatic-rename-format` (`#{?pane_in_mode,[tmux],#{pane_current_command}}#{?pane_dead,[dead],}`).
    fn check_window_names(&mut self) {
        let now = Instant::now();
        let windows: Vec<_> = self.model.windows.keys().copied().collect();
        for w in windows {
            let Some(window) = self.model.windows.get(&w) else {
                continue;
            };
            if !window.auto_rename {
                continue;
            }
            let active = window.active;
            let Some(pane) = self.model.panes.get(&active) else {
                continue;
            };
            if !pane.changed || pane.last_rename.is_some_and(|t| now - t < NAME_INTERVAL) {
                continue;
            }
            let found = crate::target::Found {
                session: self.model.sessions_with(w).next().map(|s| s.id),
                index: None,
                window: Some(w),
                pane: Some(active),
            };
            let ctx = crate::format_ctx::Ctx::new(&self.model, &self.info, &self.global_env, found);
            let name = vtmux_core::format::expand_str("#{pane_current_command}", &ctx);
            if let Some(pane) = self.model.panes.get_mut(&active) {
                pane.changed = false;
                pane.last_rename = Some(now);
            }
            if let Some(window) = self.model.windows.get_mut(&w) {
                window.name = name;
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
