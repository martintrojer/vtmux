//! The edge error type: every variant's `Display` is the exact stderr line
//! tmux 3.8 prints (without the trailing newline).
//!
//! Each string was taken from the reference tmux 3.8 (`$T -f /dev/null -S $D/s`,
//! no `$TMUX` set, server started with `new -d -s a`):
//!
//! - `$T -S $D/stale ls` after `kill-server` -> `no server running on $D/s`
//! - `$T -S $D/none ls` (no socket file)     -> `error connecting to $D/none (No such file or directory)`
//! - `$T -S /nonexistent/s new -d`           -> `error creating /nonexistent/s (No such file or directory)`
//! - `has -t nope`                           -> `can't find session: nope`
//! - `new -d -s a` (twice)                   -> `duplicate session: a`
//! - `nosuch`                                -> `unknown command: nosuch`
//! - `displ`                                 -> `ambiguous command: displ, could be: display-menu, display-message, display-popup, display-panes`
//! - `display-message -Z`                    -> `command display-message: unknown flag -Z`
//! - `show-options a b`                      -> `command show-options: too many arguments (need at most 1)`
//! - `set-option`                            -> `command set-option: too few arguments (need at least 1)`
//! - `show-options nope`                     -> `invalid option: nope`
//!
//! Sources: client.c:291-297 (connect/create), arguments.c:331-340 (arity),
//! cmd.c (unknown/ambiguous), client.c:659-668 (protocol version).

use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::socket::{SocketError, path_line};

/// What C's `strerror` would print for `err`: Rust appends ` (os error N)`
/// to OS errors, tmux does not.
#[must_use]
pub fn strerror(err: &io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(at) if err.raw_os_error().is_some() => text[..at].to_owned(),
        _ => text,
    }
}

#[derive(Debug)]
pub enum EdgeError {
    /// The socket exists but nothing accepts (`ECONNREFUSED`).
    NoServer {
        path: PathBuf,
    },
    /// Any other connect failure.
    ConnectFailed {
        path: PathBuf,
        io: io::Error,
    },
    /// Starting a server failed to create the socket (e.g. missing directory).
    CreateFailed {
        path: PathBuf,
        io: io::Error,
    },
    SessionNotFound(String),
    DuplicateSession(String),
    UnknownCommand(String),
    AmbiguousCommand {
        name: String,
        candidates: Vec<String>,
    },
    UnknownFlag {
        command: String,
        flag: char,
    },
    TooManyArgs {
        command: String,
        max: usize,
    },
    TooFewArgs {
        command: String,
        min: usize,
    },
    InvalidOption(String),
    VersionMismatch {
        client: u32,
        server: u32,
    },
    Socket(SocketError),
}

impl fmt::Display for EdgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoServer { path } => write!(f, "no server running on {}", path.display()),
            Self::ConnectFailed { path, io } => {
                write!(
                    f,
                    "error connecting to {} ({})",
                    path.display(),
                    strerror(io)
                )
            }
            Self::CreateFailed { path, io } => {
                write!(f, "error creating {} ({})", path.display(), strerror(io))
            }
            Self::SessionNotFound(s) => write!(f, "can't find session: {s}"),
            Self::DuplicateSession(s) => write!(f, "duplicate session: {s}"),
            Self::UnknownCommand(s) => write!(f, "unknown command: {s}"),
            Self::AmbiguousCommand { name, candidates } => {
                write!(
                    f,
                    "ambiguous command: {name}, could be: {}",
                    candidates.join(", ")
                )
            }
            Self::UnknownFlag { command, flag } => {
                write!(f, "command {command}: unknown flag -{flag}")
            }
            Self::TooManyArgs { command, max } => {
                write!(
                    f,
                    "command {command}: too many arguments (need at most {max})"
                )
            }
            Self::TooFewArgs { command, min } => {
                write!(
                    f,
                    "command {command}: too few arguments (need at least {min})"
                )
            }
            Self::InvalidOption(s) => write!(f, "invalid option: {s}"),
            Self::VersionMismatch { client, server } => {
                write!(
                    f,
                    "protocol version mismatch (client {client}, server {server})"
                )
            }
            Self::Socket(e) => e.fmt(f),
        }
    }
}

impl EdgeError {
    /// The stderr line (no newline) with path bytes exactly as given; see
    /// [`SocketError::to_bytes`]. Use this for stderr, `Display` for logs.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::NoServer { path } => path_line("no server running on ", path, ""),
            Self::ConnectFailed { path, io } => path_line(
                "error connecting to ",
                path,
                &format!(" ({})", strerror(io)),
            ),
            Self::CreateFailed { path, io } => {
                path_line("error creating ", path, &format!(" ({})", strerror(io)))
            }
            Self::Socket(e) => e.to_bytes(),
            _ => self.to_string().into_bytes(),
        }
    }
}

impl std::error::Error for EdgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ConnectFailed { io, .. } | Self::CreateFailed { io, .. } => Some(io),
            Self::Socket(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SocketError> for EdgeError {
    fn from(e: SocketError) -> Self {
        Self::Socket(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(e: &EdgeError) -> String {
        e.to_string()
    }

    #[test]
    fn display_is_the_tmux_line() {
        let enoent = || io::Error::from_raw_os_error(2);
        let p = || PathBuf::from("/tmp/tmux-1000/x");
        assert_eq!(
            s(&EdgeError::NoServer { path: p() }),
            "no server running on /tmp/tmux-1000/x"
        );
        assert_eq!(
            s(&EdgeError::ConnectFailed {
                path: p(),
                io: enoent()
            }),
            "error connecting to /tmp/tmux-1000/x (No such file or directory)"
        );
        assert_eq!(
            s(&EdgeError::CreateFailed {
                path: p(),
                io: enoent()
            }),
            "error creating /tmp/tmux-1000/x (No such file or directory)"
        );
        assert_eq!(
            s(&EdgeError::SessionNotFound("nope".into())),
            "can't find session: nope"
        );
        assert_eq!(
            s(&EdgeError::DuplicateSession("a".into())),
            "duplicate session: a"
        );
        assert_eq!(
            s(&EdgeError::UnknownCommand("nosuchcmd".into())),
            "unknown command: nosuchcmd"
        );
        assert_eq!(
            s(&EdgeError::AmbiguousCommand {
                name: "displ".into(),
                candidates: [
                    "display-menu",
                    "display-message",
                    "display-popup",
                    "display-panes"
                ]
                .map(String::from)
                .to_vec(),
            }),
            "ambiguous command: displ, could be: display-menu, display-message, display-popup, display-panes"
        );
        assert_eq!(
            s(&EdgeError::UnknownFlag {
                command: "display-message".into(),
                flag: 'Z'
            }),
            "command display-message: unknown flag -Z"
        );
        assert_eq!(
            s(&EdgeError::TooManyArgs {
                command: "show-options".into(),
                max: 1
            }),
            "command show-options: too many arguments (need at most 1)"
        );
        assert_eq!(
            s(&EdgeError::TooFewArgs {
                command: "set-option".into(),
                min: 1
            }),
            "command set-option: too few arguments (need at least 1)"
        );
        assert_eq!(
            s(&EdgeError::InvalidOption("nope".into())),
            "invalid option: nope"
        );
        assert_eq!(
            s(&EdgeError::VersionMismatch {
                client: 1,
                server: 2
            }),
            "protocol version mismatch (client 1, server 2)"
        );
    }

    #[test]
    fn socket_errors_pass_through() {
        let e = EdgeError::from(SocketError::NotADirectory("/tmp/tmux-1000".into()));
        assert_eq!(s(&e), "/tmp/tmux-1000 is not a directory");
    }

    /// Reference: `env -u TMUX $T -f /dev/null -S "$D/bad-\xff" ls` prints
    /// `error connecting to $D/bad-\xff (No such file or directory)` with the
    /// 0xff byte intact (client.c:291-297 writes the `char *` with `%s`).
    #[test]
    fn non_utf8_paths_keep_their_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(b"/tmp/bad-\xff".to_vec()));
        let enoent = || io::Error::from_raw_os_error(2);
        let cases = [
            (
                EdgeError::ConnectFailed {
                    path: path.clone(),
                    io: enoent(),
                },
                &b"error connecting to /tmp/bad-\xff (No such file or directory)"[..],
            ),
            (
                EdgeError::CreateFailed {
                    path: path.clone(),
                    io: enoent(),
                },
                b"error creating /tmp/bad-\xff (No such file or directory)",
            ),
            (
                EdgeError::NoServer { path: path.clone() },
                b"no server running on /tmp/bad-\xff",
            ),
            (
                SocketError::NotADirectory(path.clone()).into(),
                b"/tmp/bad-\xff is not a directory",
            ),
            (
                SocketError::UnsafePermissions(path.clone()).into(),
                b"directory /tmp/bad-\xff has unsafe permissions",
            ),
            (
                SocketError::CreateDir {
                    dir: path.clone(),
                    cause: "Permission denied".into(),
                }
                .into(),
                b"couldn't create directory /tmp/bad-\xff (Permission denied)",
            ),
            (
                SocketError::ReadDir {
                    dir: path,
                    cause: "Permission denied".into(),
                }
                .into(),
                b"couldn't read directory /tmp/bad-\xff (Permission denied)",
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.to_bytes(), want, "{err}");
        }
    }

    #[test]
    fn to_bytes_matches_display_for_utf8() {
        let e = EdgeError::ConnectFailed {
            path: "/tmp/x".into(),
            io: io::Error::from_raw_os_error(2),
        };
        assert_eq!(e.to_bytes(), e.to_string().into_bytes());
        let e = EdgeError::UnknownCommand("x".into());
        assert_eq!(e.to_bytes(), b"unknown command: x");
    }

    #[test]
    fn strerror_drops_the_rust_suffix() {
        assert_eq!(
            strerror(&io::Error::from_raw_os_error(13)),
            "Permission denied"
        );
        assert_eq!(strerror(&io::Error::other("boom")), "boom");
    }
}
