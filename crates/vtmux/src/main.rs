#![forbid(unsafe_code)]
//! The `vtmux` binary: parse global options, find or start the server,
//! send the command, relay its output and exit with its status.

mod cli;

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vtmux_core::cmd;
use vtmux_core::error::EdgeError;
use vtmux_core::protocol::{
    self, ClientFlags, ClientMsg, Hello, PROTOCOL_VERSION, ServerMsg, TtyInfo,
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
        term: std::env::var("TERM").unwrap_or_default(),
        cols: ws.map_or(80, |w| u32::from(w.ws_col)),
        rows: ws.map_or(24, |w| u32::from(w.ws_row)),
        features: features.to_vec(),
    })
}

/// `connect(2)` the socket, with C's errno semantics for a long path.
fn connect(path: &Path) -> io::Result<UnixStream> {
    if path.as_os_str().as_bytes().len() >= 108 {
        return Err(io::Error::from_raw_os_error(ENAMETOOLONG));
    }
    UnixStream::connect(path)
}

// Linux errno values (asm-generic/errno*.h).
const ENOENT: i32 = 2;
const ENAMETOOLONG: i32 = 36;
const ECONNREFUSED: i32 = 111;

fn connect_error(path: &Path, e: io::Error) -> EdgeError {
    if e.raw_os_error() == Some(ECONNREFUSED) {
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
/// the command starts a server, bind the socket (reporting
/// `error creating`), fork the daemon and connect to it.
fn connect_or_start(
    path: &Path,
    start: bool,
    default_socket: bool,
    config: impl FnOnce() -> ServerConfig,
) -> Result<UnixStream, EdgeError> {
    let err = match connect(path) {
        Ok(s) => return Ok(s),
        Err(e) => e,
    };
    let absent = matches!(err.raw_os_error(), Some(ECONNREFUSED | ENOENT));
    if !absent || !start {
        return Err(connect_error(path, err));
    }
    let listener =
        daemon::bind_socket(path, default_socket).map_err(|io| EdgeError::CreateFailed {
            path: path.to_owned(),
            io,
        })?;
    let config = config();
    match daemon::fork_daemon() {
        Ok(Forked::Daemon) => {
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

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
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

    let cli = match cli::parse(&args, &progname) {
        Ok(cli) => cli,
        Err(cli::Early::Version) => {
            // D21.
            match invocation {
                Invocation::Tmux => println!("tmux 3.8"),
                Invocation::Vtmux => println!("vtmux {} (tmux 3.8)", env!("CARGO_PKG_VERSION")),
            }
            return ExitCode::SUCCESS;
        }
        Err(cli::Early::Help) => {
            print!("{}", cli::usage(&progname));
            return ExitCode::SUCCESS;
        }
        Err(cli::Early::Usage(msg)) => {
            let mut err = io::stderr().lock();
            if let Some(m) = msg {
                let _ = writeln!(err, "{m}");
            }
            let _ = err.write_all(cli::usage(&progname).as_bytes());
            return ExitCode::from(1);
        }
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
        || cmd::parse_argv(&cli.command)
            .is_ok_and(|calls| calls.iter().any(|c| c.entry.starts_server));
    let start = start && !cli.no_start;

    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
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
    };
    let mut stream = match connect_or_start(&path, start, default_socket, config) {
        Ok(s) => s,
        Err(e) => return edge_exit(&e),
    };

    let hello = Hello {
        version: PROTOCOL_VERSION,
        invocation,
        argv: cli.command.clone(),
        cwd: cwd.clone(),
        env: env.clone(),
        tty: tty_info(&cli.features),
        flags: ClientFlags {
            control: cli.control,
            utf8,
            no_start: cli.no_start,
        },
    };
    if protocol::write_msg(&mut stream, &ClientMsg::Hello(hello)).is_err() {
        write_err(b"server exited unexpectedly");
        return ExitCode::from(1);
    }
    relay(&mut stream)
}

/// Print what the server sends until `Exit`.
fn relay(stream: &mut UnixStream) -> ExitCode {
    loop {
        match protocol::read_msg::<ServerMsg>(stream) {
            Ok(Some(ServerMsg::Stdout(b) | ServerMsg::Frame(b))) => {
                let mut out = io::stdout().lock();
                let _ = out.write_all(&b);
                let _ = out.flush();
            }
            Ok(Some(ServerMsg::Stderr(b))) => {
                let _ = io::stderr().lock().write_all(&b);
            }
            Ok(Some(ServerMsg::Exit { code, message })) => {
                if let Some(m) = message {
                    write_err(m.as_bytes());
                }
                return ExitCode::from(u8::try_from(code).unwrap_or(1));
            }
            Ok(Some(ServerMsg::Ready)) => {}
            Ok(None) | Err(_) => {
                // client.c: CLIENT_EXIT_LOST_SERVER.
                write_err(b"server exited unexpectedly");
                return ExitCode::from(1);
            }
        }
    }
}
