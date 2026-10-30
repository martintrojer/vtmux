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
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, Mode, OFlags, flock};
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

/// `daemon(1, 0)` after the caller's fork, without `daemon(3)` (absent from
/// some platforms' libc crates): fork again, the intermediate process
/// `_exit`s, the child becomes the session leader with stdio on /dev/null.
/// Returns true in the surviving child.
fn daemonize() -> io::Result<bool> {
    // SAFETY: the process is single-threaded, so the child may run
    // arbitrary Rust code after fork.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            rustix::process::setsid()?;
            // SAFETY: plain libc calls on a NUL-terminated literal and the
            // standard descriptors.
            unsafe {
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null < 0 {
                    return Err(io::Error::last_os_error());
                }
                for fd in 0..=2 {
                    libc::dup2(null, fd);
                }
                if null > 2 {
                    libc::close(null);
                }
            }
            Ok(true)
        }
        // SAFETY: the intermediate process must not run destructors or
        // flush inherited buffers.
        _ => unsafe { libc::_exit(0) },
    }
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
            daemonize()?;
            Ok(Forked::Daemon)
        }
        child => {
            let pid = rustix::process::Pid::from_raw(child);
            let _ = rustix::process::waitpid(pid, WaitOptions::empty());
            Ok(Forked::Parent)
        }
    }
}

/// What `client_get_lock` (client.c:80-100) found.
pub enum Lock {
    /// We hold the server-start lock; dropping it removes the lock file.
    Held(StartLock),
    /// Another client was starting the server and has finished: connect
    /// again.
    Retry,
    /// The lock file could not be opened: start the server anyway.
    Unlocked,
}

/// The `<socket>.lock` file with an exclusive `flock`.
pub struct StartLock {
    path: PathBuf,
    owner: rustix::process::Pid,
    _fd: OwnedFd,
}

impl Drop for StartLock {
    fn drop(&mut self) {
        // server_start (server.c:223-227): unlink, then close (which
        // releases the flock). The daemon inherits a copy of the
        // descriptor and drops it without unlinking: the starting client
        // still owns the file until it has connected.
        if rustix::process::getpid() == self.owner {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `client_get_lock`: take the lock without blocking; if another client
/// holds it, wait for that client to release it and report [`Lock::Retry`].
#[must_use]
pub fn get_lock(path: &Path) -> Lock {
    let Ok(fd) = rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) else {
        return Lock::Unlocked;
    };
    // Any failure other than "held elsewhere" still continues as the
    // starter, as tmux does.
    if flock(&fd, FlockOperation::NonBlockingLockExclusive) == Err(rustix::io::Errno::WOULDBLOCK) {
        while flock(&fd, FlockOperation::LockExclusive) == Err(rustix::io::Errno::INTR) {}
        return Lock::Retry;
    }
    Lock::Held(StartLock {
        path: path.to_owned(),
        owner: rustix::process::getpid(),
        _fd: fd,
    })
}
