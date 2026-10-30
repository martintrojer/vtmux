//! Starting the server the way tmux does (server.c `server_create_socket`,
//! `server_start`; proc.c `proc_fork_and_daemon`).
//!
//! The listening socket is created and bound by the client, before it
//! forks, so a bind failure is reported to the caller as
//! `error creating <path> (<strerror>)` and the client exits 1. Only then
//! does the client fork; the child becomes a daemon (`daemon(1, 0)`: cwd
//! kept, stdio on /dev/null, new session) and runs the server on the
//! inherited socket, while the client connects to it as any client would.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

use rustix::fs::Mode;
use rustix::process::{WaitOptions, umask};

/// `server_create_socket` (server.c:106-150): unlink, bind with a umask
/// that leaves `rw` for the owner (and the group for a default `-L`
/// socket), restore the umask, listen.
pub fn bind_socket(path: &Path, default_socket: bool) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    // S_IXUSR|S_IXGRP|S_IRWXO for the default socket, S_IXUSR|S_IRWXG|S_IRWXO
    // otherwise.
    let mask = if default_socket { 0o117 } else { 0o177 };
    let old = umask(Mode::from_raw_mode(mask));
    let listener = UnixListener::bind(path);
    umask(old);
    let listener = listener?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Which side of [`fork_daemon`] we are on.
pub enum Forked {
    /// The original process; the daemon is running.
    Parent,
    /// The daemon: detached from the terminal, stdio on /dev/null.
    Daemon,
}

/// `proc_fork_and_daemon` (proc.c:374-395): fork; the child calls
/// `daemon(1, 0)`. The parent reaps the intermediate child so it does not
/// linger as a zombie.
pub fn fork_daemon() -> io::Result<Forked> {
    // SAFETY: the client is single-threaded when it starts a server, so the
    // child may run arbitrary Rust code after fork.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            // SAFETY: plain libc call; daemon(3) forks again, the
            // intermediate process _exits, and this process continues as
            // the session leader with stdio redirected to /dev/null.
            if unsafe { libc::daemon(1, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Forked::Daemon)
        }
        child => {
            let pid = rustix::process::Pid::from_raw(child);
            let _ = rustix::process::waitpid(pid, WaitOptions::empty());
            Ok(Forked::Parent)
        }
    }
}
