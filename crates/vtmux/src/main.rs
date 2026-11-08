#![forbid(unsafe_code)]
//! The `vtmux` binary: parse global options, find or start the server,
//! send the command, relay its output and exit with its status.

mod cli;
mod files;

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vtmux_client::attach::Reattach;
use vtmux_core::cmd;
use vtmux_core::error::{EdgeError, strerror};
use vtmux_core::protocol::{
    self, ClientFlags, ClientMsg, ControlMode, Hello, HelloExt, PROTOCOL_VERSION, REATTACH_FLAG,
    REATTACH_TOKEN_ENV, ServerMsg, TtyInfo,
};
use vtmux_core::socket::{self, Invocation, RealEnv, RealFs};
use vtmux_server::ServerConfig;
use vtmux_server::sys::daemon::{self, Forked};

/// tmux's default config search list (Makefile.am:14, the `TMUX_CONF` the
/// reference was built with).
const DEFAULT_CONFIG: &str =
    "/etc/tmux.conf:~/.tmux.conf:$XDG_CONFIG_HOME/tmux/tmux.conf:~/.config/tmux/tmux.conf";

fn write_err(bytes: &[u8]) {
    let mut err = io::stderr().lock();
    let _ = err.write_all(bytes);
    let _ = err.write_all(b"\n");
}

fn edge_exit(e: &EdgeError) -> ExitCode {
    write_err(&e.to_bytes());
    ExitCode::from(1)
}

/// `find_home` (tmux.c:366-384).
fn find_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| vtmux_server::sys::user::current_user().map(|p| p.home))
}

/// `find_cwd` (tmux.c:341-363): `$PWD` when it names the real working
/// directory (keeps symlinks), else `getcwd`.
fn find_cwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    if let Some(pwd) = std::env::var_os("PWD").filter(|p| !p.is_empty())
        && let (Ok(a), Ok(b)) = (std::fs::canonicalize(&pwd), std::fs::canonicalize(&cwd))
        && a == b
    {
        return Some(PathBuf::from(pwd));
    }
    Some(cwd)
}

/// `expand_paths(TMUX_CONF, ..., no_realpath)` (tmux.c:122-202).
fn default_config_files() -> Vec<PathBuf> {
    let home = find_home();
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in DEFAULT_CONFIG.split(':') {
        let path = if let Some(rest) = entry.strip_prefix("~/") {
            let Some(home) = &home else { continue };
            home.join(rest)
        } else if let Some(var) = entry.strip_prefix('$') {
            let (name, rest) = var.split_once('/').unwrap_or((var, ""));
            let Some(value) = std::env::var_os(name) else {
                continue;
            };
            let mut p = value;
            p.push("/");
            p.push(rest);
            PathBuf::from(p)
        } else {
            PathBuf::from(entry)
        };
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// tmux.c:503-523.
fn detect_utf8(forced: bool) -> bool {
    if forced || std::env::var_os("TMUX").is_some() {
        return true;
    }
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_default()
        .to_ascii_uppercase();
    locale.contains("UTF-8") || locale.contains("UTF8")
}

fn tty_info(features: &[String]) -> Option<TtyInfo> {
    let stdin = io::stdin();
    if !rustix::termios::isatty(&stdin) {
        return None;
    }
    let ws = rustix::termios::tcgetwinsize(&stdin).ok();
    Some(TtyInfo {
        name: rustix::termios::ttyname(&stdin, Vec::new())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        term: std::env::var("TERM").unwrap_or_default(),
        cols: ws.map_or(80, |w| u32::from(w.ws_col)),
        rows: ws.map_or(24, |w| u32::from(w.ws_row)),
        features: features.to_vec(),
    })
}

/// The terminal's size in pixels (`ws_xpixel`, `ws_ypixel`), 0 when it
/// doesn't say or there is no terminal: tmux reads it at identify (tty.c
/// `tty_resize`).
fn tty_pixels() -> (u32, u32) {
    let stdin = io::stdin();
    if !rustix::termios::isatty(&stdin) {
        return (0, 0);
    }
    rustix::termios::tcgetwinsize(&stdin)
        .map_or((0, 0), |w| (u32::from(w.ws_xpixel), u32::from(w.ws_ypixel)))
}

/// The environment as UTF-8 pairs; others are skipped.
fn client_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// `connect(2)` the socket, with C's errno semantics for a long path.
fn connect(path: &Path) -> io::Result<UnixStream> {
    if path.as_os_str().as_bytes().len() >= 108 {
        return Err(io::Error::from_raw_os_error(ENAMETOOLONG));
    }
    UnixStream::connect(path)
}

// errno values from the platform, never hard-coded: ECONNREFUSED is 111 on
// Linux but 61 on macOS.
const ENAMETOOLONG: i32 = rustix::io::Errno::NAMETOOLONG.raw_os_error();

fn connect_error(path: &Path, e: io::Error) -> EdgeError {
    if e.kind() == io::ErrorKind::ConnectionRefused {
        EdgeError::NoServer {
            path: path.to_owned(),
        }
    } else {
        EdgeError::ConnectFailed {
            path: path.to_owned(),
            io: e,
        }
    }
}

/// `client_connect` (client.c:104-182): connect; if nothing is there and
/// the command starts a server, take the `<socket>.lock` flock (waiting for
/// a concurrent starter to finish), connect again, and only then bind the
/// socket (reporting `error creating`), fork the daemon and connect to it.
/// The lock is held until the connection is made.
fn connect_or_start(
    path: &Path,
    start: bool,
    default_socket: bool,
    config: impl FnOnce() -> ServerConfig,
) -> Result<UnixStream, EdgeError> {
    let mut lock: Option<daemon::Lock> = None;
    loop {
        let err = match connect(path) {
            Ok(s) => return Ok(s),
            Err(e) => e,
        };
        let absent = matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
        );
        if !absent || !start {
            return Err(connect_error(path, err));
        }
        if lock.is_none() {
            let mut lock_path = path.as_os_str().to_owned();
            lock_path.push(".lock");
            match daemon::get_lock(Path::new(&lock_path)) {
                daemon::Lock::Retry => continue,
                got => lock = Some(got),
            }
            // Always retry once: another client may have started the
            // server and released the lock between connect and flock.
            continue;
        }
        break;
    }
    let listener =
        daemon::bind_socket(path, default_socket).map_err(|io| EdgeError::CreateFailed {
            path: path.to_owned(),
            io,
        })?;
    let config = config();
    match daemon::fork_daemon() {
        Ok(Forked::Daemon) => {
            // server_start closes the lock descriptor (server.c:223-227).
            drop(lock);
            let listener = daemon::close_inherited(listener);
            let code = vtmux_server::run_server(listener, path.to_owned(), config);
            std::process::exit(code);
        }
        Ok(Forked::Parent) => {
            drop(listener);
            connect(path).map_err(|e| connect_error(path, e))
        }
        Err(io) => Err(EdgeError::CreateFailed {
            path: path.to_owned(),
            io,
        }),
    }
}

/// The global options (tmux.c `main`'s getopt loop), or how to exit:
/// `-V`, `-h` and usage errors end here.
fn parse_cli(
    args: &[OsString],
    progname: &str,
    invocation: Invocation,
) -> Result<cli::Cli, ExitCode> {
    match cli::parse(args, progname) {
        Ok(cli) => Ok(cli),
        Err(cli::Early::Version) => {
            // D21, D25.
            match invocation {
                Invocation::Tmux => println!("tmux {}", vtmux_core::version::TMUX_VERSION),
                Invocation::Vtmux => println!("vtmux {}", vtmux_core::version::display()),
            }
            Err(ExitCode::SUCCESS)
        }
        Err(cli::Early::Help) => {
            print!("{}", cli::usage(progname));
            Err(ExitCode::SUCCESS)
        }
        Err(cli::Early::Usage(msg)) => {
            let mut err = io::stderr().lock();
            if let Some(m) = msg {
                let _ = writeln!(err, "{m}");
            }
            let _ = err.write_all(cli::usage(progname).as_bytes());
            Err(ExitCode::from(1))
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // Taken out at once, whatever the command line: nothing this process
    // starts (a server, a pane, a lock command) inherits the token.
    let token = vtmux_server::sys::upgrade::take_env(REATTACH_TOKEN_ENV);
    upgrade_image(args.get(1..).unwrap_or_default())
        .or_else(|| match args.as_slice() {
            [argv0, flag, socket] if flag == REATTACH_FLAG => Some(reattach_main(
                argv0.clone(),
                token.as_deref(),
                Path::new(socket),
            )),
            _ => None,
        })
        .unwrap_or_else(|| client_main(args))
}

/// The client: find or start the server, send the command, relay.
fn client_main(mut args: Vec<OsString>) -> ExitCode {
    let argv0 = if args.is_empty() {
        OsString::from("vtmux")
    } else {
        args.remove(0)
    };
    let invocation = Invocation::from_argv0(&argv0);
    let progname = Path::new(&argv0)
        .file_name()
        .map_or_else(|| "vtmux".to_owned(), |n| n.to_string_lossy().into_owned());
    let progname = progname.trim_start_matches('-').to_owned();

    let cli = match parse_cli(&args, &progname, invocation) {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    if cli.shell_command.is_some() || cli.no_fork {
        write_err(b"not implemented in vtmux yet: -c and -D");
        return ExitCode::from(1);
    }
    let utf8 = detect_utf8(cli.utf8);

    let default_socket = cli.socket.is_none()
        && (cli.label.is_some()
            || std::env::var_os("TMUX").is_none_or(|t| t.is_empty() || t.as_bytes()[0] == b','));
    let path = match socket::socket_path(
        invocation,
        cli.label.as_deref(),
        cli.socket.as_deref(),
        &RealEnv,
        &RealFs,
    ) {
        Ok(p) => p,
        Err(e) => return edge_exit(&EdgeError::from(e)),
    };

    // client_main (client.c:248-272): the command is parsed here only to
    // learn whether it starts a server; errors are the server's to report.
    let start = cli.command.is_empty()
        || cmd::parse_argv_bytes(&cli.command)
            .is_ok_and(|calls| calls.iter().any(|c| c.entry.starts_server));
    let start = start && !cli.no_start;

    let env = client_env();
    let cwd = find_cwd()
        .or_else(find_home)
        .unwrap_or_else(|| PathBuf::from("/"));
    let config = || ServerConfig {
        config_quiet: cli.config_files.is_empty(),
        config_files: if cli.config_files.is_empty() {
            default_config_files()
        } else {
            cli.config_files.clone()
        },
        environ: env.clone(),
        cwd: Some(cwd.clone()),
        default_socket,
        dry_run: false,
        binary: start_binary(&argv0),
    };
    let mut stream = match connect_or_start(&path, start, default_socket, config) {
        Ok(s) => s,
        Err(e) => return edge_exit(&e),
    };
    if let Err(code) = check_control_tty(cli.control) {
        return code;
    }
    let hello = Hello {
        version: PROTOCOL_VERSION,
        invocation,
        pid: std::process::id(),
        argv: cli.command.clone(),
        cwd: cwd.clone(),
        env,
        tty: tty_info(&cli.features),
        flags: ClientFlags {
            control: cli.control,
            utf8,
            no_start: cli.no_start,
        },
        // An attached client re-executes on an upgrade (decision 4).
        ext: HelloExt {
            follows_upgrade: cli.control == ControlMode::None,
            reattach: None,
            pixels: tty_pixels(),
        },
    };
    if protocol::write_msg(&mut stream, &ClientMsg::Hello(hello)).is_err() {
        write_err(b"server exited unexpectedly");
        return ExitCode::from(1);
    }
    let how = Reattach {
        argv0: argv0.clone(),
        socket: path.clone(),
        sent_size: None,
    };
    match (cli.control, lone_upgrade(&cli.command)) {
        (ControlMode::None, Some(up)) => upgrade_relay(
            &mut stream,
            &path,
            up.flag.map(|b| cwd.join(b)),
            &argv0,
            &how,
        ),
        (ControlMode::None, None) => relay(&mut stream, None, &how),
        _ => control_relay(&mut stream),
    }
}

/// The frozen re-attach command line (`<binary> --vtmux-reattach
/// <socket>`, the token in `VTMUX_REATTACH_TOKEN`, protocol module docs)
/// that an attached client of this or another build runs when an upgrade
/// tells it to (live-upgrade spec, *Clients*): connect to the same socket,
/// never starting a server, and present the token, which binds this
/// process to the client record it had; then go on as that attached
/// client. A token the server no longer knows is told to attach again.
/// The terminal is the shell's until the server starts it again: the old
/// image restored it before the notice, so every way out here leaves it
/// as a detach does.
fn reattach_main(argv0: OsString, token: Option<&std::ffi::OsStr>, socket: &Path) -> ExitCode {
    let Some(token) = token
        .and_then(|t| t.to_str())
        .filter(|t| protocol::is_upgrade_token(t))
    else {
        write_err(b"can't re-attach after the server's upgrade: no valid token");
        println!("[{}]", protocol::UPGRADED_DETACH);
        return ExitCode::from(1);
    };
    let mut stream = match connect(socket) {
        Ok(s) => s,
        Err(e) => return edge_exit(&connect_error(socket, e)),
    };
    let tty = tty_info(&[]);
    let sent_size = tty.as_ref().map(|t| (t.cols, t.rows));
    let hello = Hello {
        version: PROTOCOL_VERSION,
        invocation: Invocation::from_argv0(&argv0),
        pid: std::process::id(),
        argv: Vec::new(),
        cwd: find_cwd()
            .or_else(find_home)
            .unwrap_or_else(|| PathBuf::from("/")),
        env: client_env(),
        tty,
        flags: ClientFlags {
            control: ControlMode::None,
            utf8: detect_utf8(false),
            no_start: true,
        },
        ext: HelloExt {
            follows_upgrade: true,
            reattach: Some(token.to_owned()),
            pixels: tty_pixels(),
        },
    };
    if protocol::write_msg(&mut stream, &ClientMsg::Hello(hello)).is_err() {
        write_err(b"server exited unexpectedly");
        return ExitCode::from(1);
    }
    let how = Reattach {
        argv0,
        socket: socket.to_owned(),
        sent_size,
    };
    match protocol::read_msg::<ServerMsg>(&mut stream) {
        // Told to attach again: said as a detached client says it.
        Ok(Some(ServerMsg::Exit {
            code,
            message: Some(m),
            ..
        })) => {
            println!("[{m}]");
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        Ok(Some(first)) => relay(&mut stream, Some(first), &how),
        Ok(None) | Err(_) => relay(&mut stream, None, &how),
    }
}

/// The images a live upgrade starts (vtmux-server `sys::upgrade`): the new
/// server, `[-S <socket>] --vtmux-resume <fd> [--vtmux-switched-back]`,
/// the dry run, `--vtmux-check-snapshot <fd>`, which says on stderr that
/// this binary takes the `-S` (`TAKES_SOCKET`), and the old image's
/// question before it, `--vtmux-patch-stack`. `None` for any other command
/// line.
fn upgrade_image(args: &[OsString]) -> Option<ExitCode> {
    use vtmux_server::server::resume;
    use vtmux_server::sys::upgrade::{
        CHECK_FLAG, PATCH_STACK_FLAG, RESUME_FLAG, SWITCHED_BACK_FLAG, TAKES_SOCKET,
    };
    let (socket, args) = match args {
        [s, path, rest @ ..] if s == "-S" && rest.first().is_some_and(|f| f == RESUME_FLAG) => {
            (Some(Path::new(path)), rest)
        }
        _ => (None, args),
    };
    let words: Vec<&str> = args.iter().map(|a| a.to_str()).collect::<Option<_>>()?;
    let fd = |w: &str| w.parse::<i32>().ok();
    let resume = |n: &str, back| {
        Some(ExitCode::from(
            u8::try_from(resume::resume(fd(n)?, back, socket)).unwrap_or(1),
        ))
    };
    match words.as_slice() {
        [flag] if *flag == PATCH_STACK_FLAG => {
            println!("{}", vtmux_server::snapshot_term::PATCH_STACK);
            Some(ExitCode::SUCCESS)
        }
        [flag, n] if *flag == RESUME_FLAG => resume(n, false),
        [flag, n, back] if *flag == RESUME_FLAG && *back == SWITCHED_BACK_FLAG => resume(n, true),
        [flag, n] if *flag == CHECK_FLAG => Some(match resume::check_snapshot(fd(n)?) {
            Ok(()) => {
                write_err(TAKES_SOCKET.as_bytes());
                ExitCode::SUCCESS
            }
            Err(message) => {
                write_err(message.as_bytes());
                ExitCode::from(1)
            }
        }),
        _ => None,
    }
}

/// The path this binary was started as, for a server it starts
/// (live-upgrade spec, decision 22).
fn start_binary(argv0: &std::ffi::OsStr) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let path = std::env::var("PATH").ok();
    vtmux_server::server::upgrade::start_binary(argv0, path.as_deref(), &cwd)
        .or_else(|| std::env::current_exe().ok())
}

/// A command line that is `vtmux-upgrade` alone.
struct LoneUpgrade {
    /// Its `-b`.
    flag: Option<PathBuf>,
}

fn lone_upgrade(command: &[Vec<u8>]) -> Option<LoneUpgrade> {
    let calls = cmd::parse_argv_bytes(command).ok()?;
    let [call] = calls.as_slice() else {
        return None;
    };
    (call.entry.name == "vtmux-upgrade").then(|| LoneUpgrade {
        flag: call.args.value('b').map(PathBuf::from),
    })
}

/// `vtmux-upgrade` on the command line (live-upgrade spec, *Command*): a
/// server of this protocol runs it as any command. One of another version
/// answers the `Hello` with the version mismatch; then the frozen
/// `UpgradeRequest` asks it to move onto `flag` (`-b`, from the client's
/// directory) or, without one, onto this binary, the one the mismatch
/// hint means.
fn upgrade_relay(
    stream: &mut UnixStream,
    socket: &Path,
    flag: Option<PathBuf>,
    argv0: &std::ffi::OsStr,
    how: &Reattach,
) -> ExitCode {
    let mismatch = match protocol::read_msg::<ServerMsg>(stream) {
        Ok(Some(ServerMsg::Stderr(b))) if b.starts_with(b"protocol version mismatch") => b,
        Ok(Some(first)) => return relay(stream, Some(first), how),
        Ok(None) | Err(_) => return relay(stream, None, how),
    };
    let Some(binary) =
        flag.or_else(|| std::env::current_exe().ok().or_else(|| start_binary(argv0)))
    else {
        let _ = io::stderr().lock().write_all(&mismatch);
        return ExitCode::from(1);
    };
    let answer = connect(socket).ok().and_then(|mut s| {
        protocol::write_msg(&mut s, &ClientMsg::UpgradeRequest { binary }).ok()?;
        loop {
            match protocol::read_msg::<ServerMsg>(&mut s) {
                Ok(Some(ServerMsg::UpgradeResult { ok, message })) => return Some((ok, message)),
                Ok(Some(_)) | Err(protocol::ProtocolError::UnknownTag(_)) => {}
                Ok(None) | Err(_) => return None,
            }
        }
    });
    // No answer: a server from before the handshake (decision 14), and
    // the mismatch is all there is to say.
    let Some((ok, message)) = answer else {
        let _ = io::stderr().lock().write_all(&mismatch);
        return ExitCode::from(1);
    };
    if !message.is_empty() {
        write_err(message.as_bytes());
    }
    ExitCode::from(u8::from(!ok))
}

/// client.c:343-348: `-CC` puts stdin's terminal in raw mode, after the
/// server is up. vtmux otherwise runs `-CC` as `-C` (iTerm2 `-CC` is out of
/// M0 scope: no DCS wrapping, output on stdout).
fn check_control_tty(mode: ControlMode) -> Result<(), ExitCode> {
    if mode != ControlMode::ControlControl {
        return Ok(());
    }
    rustix::termios::tcgetattr(io::stdin()).map_err(|e| {
        write_err(format!("tcgetattr failed: {}", strerror(&io::Error::from(e))).as_bytes());
        ExitCode::from(1)
    })?;
    Ok(())
}

/// Copy stdin to the server as `Stdin` messages, then `StdinEof`. Once
/// the server is gone, or `after` is set (the client has printed `%exit`),
/// what is read goes to `rest` instead, `None` at the end of the input,
/// for [`wait_exit`].
fn pump_stdin(
    mut stream: UnixStream,
    after: &std::sync::atomic::AtomicBool,
    rest: &std::sync::mpsc::Sender<Option<Vec<u8>>>,
) {
    let mut buf = [0u8; 16 * 1024];
    let mut stdin = io::stdin().lock();
    let mut server = true;
    loop {
        let read = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(buf[..n].to_vec()),
        };
        let eof = read.is_none();
        if server && !after.load(std::sync::atomic::Ordering::SeqCst) {
            let msg = read.clone().map_or(ClientMsg::StdinEof, ClientMsg::Stdin);
            server = protocol::write_msg(&mut stream, &msg).is_ok();
            if server {
                if eof {
                    return;
                }
                continue;
            }
        }
        if rest.send(read).is_err() || eof {
            return;
        }
    }
}

/// `control_wait_exit` (control.c:745-785), for a control client with the
/// `wait-exit` flag: after `%exit`, read the input until an empty line or
/// its end. Lines already read in one go count one at a time.
fn wait_exit(rest: &std::sync::mpsc::Receiver<Option<Vec<u8>>>) {
    let mut line = Vec::new();
    while let Ok(Some(chunk)) = rest.recv() {
        for b in chunk {
            if b != b'\n' {
                line.push(b);
            } else if line.is_empty() {
                return;
            } else {
                line.clear();
            }
        }
    }
}

/// A control client (`-C`): stdin goes to the server, the server's
/// protocol stream to stdout; on exit print `%exit [reason]`
/// (client.c:417-424), then wait for the input if `wait-exit` is set.
fn control_relay(stream: &mut UnixStream) -> ExitCode {
    let after = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rest) = std::sync::mpsc::channel();
    if let Ok(writer) = stream.try_clone() {
        let after = std::sync::Arc::clone(&after);
        std::thread::spawn(move || pump_stdin(writer, &after, &tx));
    }
    let mut wait = false;
    let (code, reason) = loop {
        match protocol::read_msg::<ServerMsg>(stream) {
            Ok(Some(ServerMsg::Stdout(b) | ServerMsg::Frame(b))) => {
                let mut out = io::stdout().lock();
                let _ = out.write_all(&b);
                let _ = out.flush();
            }
            Ok(Some(ServerMsg::Stderr(b))) => {
                let _ = io::stderr().lock().write_all(&b);
            }
            Ok(Some(ServerMsg::Exit { code, message, .. })) => break (code, message),
            // client.c:403-407: detach-client -E replaces the client.
            Ok(Some(ServerMsg::Exec { command, shell })) => {
                return ExitCode::from(
                    u8::try_from(vtmux_client::attach::exec(&shell, &command)).unwrap_or(1),
                );
            }
            Ok(Some(ServerMsg::Flags { wait_exit })) => wait = wait_exit,
            // A control client's file I/O is the server's (file.c:321). It
            // stays connected across an upgrade, and only a requester gets
            // a result (live-upgrade spec, decision 26). An unknown tag is a
            // frame a later server added (new frozen frames take new tags,
            // protocol module docs): `read_msg` has read all of it, so the
            // stream is still in step.
            Ok(Some(
                ServerMsg::Ready
                | ServerMsg::ReadOpen { .. }
                | ServerMsg::WriteOpen { .. }
                | ServerMsg::WriteData(_)
                | ServerMsg::WriteClose
                | ServerMsg::Suspend
                | ServerMsg::Lock { .. }
                | ServerMsg::UpgradeNotice { .. }
                | ServerMsg::UpgradeResult { .. },
            ))
            | Err(protocol::ProtocolError::UnknownTag(_)) => {}
            Ok(None) | Err(_) => break (1, Some("server exited unexpectedly".to_owned())),
        }
    };
    after.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut out = io::stdout().lock();
    let _ = match reason {
        Some(r) => writeln!(out, "%exit {r}"),
        None => writeln!(out, "%exit"),
    };
    let _ = out.flush();
    drop(out);
    if wait {
        wait_exit(&rest);
    }
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Print what the server sends until `Exit`, `first` (already read) then
/// the rest, doing the file I/O it asks for (file.c, client side). Once
/// attached, the client comes back after an upgrade as `how` says.
fn relay(stream: &mut UnixStream, mut first: Option<ServerMsg>, how: &Reattach) -> ExitCode {
    let mut writing: Option<files::Write> = None;
    loop {
        let next = match first.take() {
            Some(m) => Ok(Some(m)),
            None => protocol::read_msg::<ServerMsg>(stream),
        };
        match next {
            Ok(Some(ServerMsg::Stdout(b) | ServerMsg::Frame(b))) => {
                let mut out = io::stdout().lock();
                let _ = out.write_all(&b);
                let _ = out.flush();
            }
            Ok(Some(ServerMsg::Stderr(b))) => {
                let _ = io::stderr().lock().write_all(&b);
            }
            Ok(Some(ServerMsg::Exit { code, message, .. })) => {
                if let Some(m) = message {
                    write_err(m.as_bytes());
                }
                return ExitCode::from(u8::try_from(code).unwrap_or(1));
            }
            Ok(Some(ServerMsg::Exec { command, shell })) => {
                return ExitCode::from(
                    u8::try_from(vtmux_client::attach::exec(&shell, &command)).unwrap_or(1),
                );
            }
            Ok(Some(ServerMsg::Ready)) => {
                // suspend-client stops the client with SIGTSTP.
                vtmux_server::sys::signal::default_stop_on_tstp();
                return ExitCode::from(
                    u8::try_from(vtmux_client::attach::attach(stream, how)).unwrap_or(1),
                );
            }
            Ok(Some(ServerMsg::WriteOpen { path, append })) => {
                writing = Some(files::Write::open(&path, append));
            }
            Ok(Some(ServerMsg::WriteData(b))) => {
                if let Some(w) = writing.as_mut() {
                    w.write(&b);
                }
            }
            Ok(Some(ServerMsg::WriteClose)) => {
                let error = writing.take().map_or(libc_ebadf(), files::Write::close);
                let _ = protocol::write_msg(stream, &ClientMsg::FileDone { error });
            }
            Ok(Some(ServerMsg::ReadOpen { path })) => files::read(stream, path.as_deref()),
            // Only an attached client is suspended or locked, and only a
            // control client waits to exit.
            // Only an attached client is told of an upgrade, and only a
            // requester (which sends no Hello) gets a result.
            Ok(Some(
                ServerMsg::Suspend
                | ServerMsg::Lock { .. }
                | ServerMsg::Flags { .. }
                | ServerMsg::UpgradeNotice { .. }
                | ServerMsg::UpgradeResult { .. },
            )) => {}
            Ok(None) | Err(_) => {
                // client.c: CLIENT_EXIT_LOST_SERVER.
                write_err(b"server exited unexpectedly");
                return ExitCode::from(1);
            }
        }
    }
}

fn libc_ebadf() -> i32 {
    rustix::io::Errno::BADF.raw_os_error()
}
