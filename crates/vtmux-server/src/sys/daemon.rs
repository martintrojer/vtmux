//! Starting the server the way tmux does (server.c `server_create_socket`,
//! `server_start`; proc.c `proc_fork_and_daemon`).
//!
//! The listening socket is created and bound by the client, before it
//! forks, so a bind failure is reported to the caller as
//! `error creating <path> (<strerror>)` and the client exits 1. This differs
//! from tmux 3.8, where the forked daemon binds (server.c `server_start`,
//! :220-221) and sends the error to its starting client as its exit message:
//! the text and the exit status are the same, but here no daemon exists when
//! the bind fails. Only after the bind does the client fork; the child
//! becomes a daemon (`daemon(1, 0)`: cwd kept, stdio on /dev/null, new
//! session) and runs the server on the inherited socket. The client is its
//! first client, on a socketpair made before the fork, as tmux's is
//! (`proc_fork_and_daemon`), not a later connection: another client
//! connecting first can't leave the fresh server empty, so exiting, before
//! the starter's command runs.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
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

/// In the daemon, before the server starts: close every descriptor but
/// stdio, the listener, which ends up as fd 3, and the starting client's
/// end of the socketpair (`proc_fork_and_daemon`), fd 4, both
/// close-on-exec. A starting client can hold descriptors it never meant to
/// pass on: where `pipe2` is missing (macOS) a pipe gets `FD_CLOEXEC` only
/// after it is made, so a client spawned by another thread in that window
/// inherits it, and a server keeping it open forever leaves the pipe's
/// reader waiting for EOF (cli.rs `concurrent_starts_share_one_server` hung
/// on macOS). tmux keeps them.
#[must_use]
pub fn close_inherited(listener: UnixListener, starter: UnixStream) -> (UnixListener, UnixStream) {
    use std::os::fd::{FromRawFd, IntoRawFd};
    const LISTENER_FD: libc::c_int = 3;
    const STARTER_FD: libc::c_int = 4;
    let lfd = listener.into_raw_fd();
    let sfd = starter.into_raw_fd();
    // SAFETY: the daemon is single-threaded and owns `lfd` and `sfd`; each
    // is first moved above the two targets (so a dup2 to one target never
    // closes the other descriptor), then dup2'd into place; close_from(5)
    // leaves stdio, the listener and the starter, and nothing else in the
    // process holds a descriptor it still uses.
    unsafe {
        let high = libc::c_int::max(lfd, sfd).max(STARTER_FD) + 1;
        let lhigh = libc::fcntl(lfd, libc::F_DUPFD, high);
        let shigh = libc::fcntl(sfd, libc::F_DUPFD, high);
        if lhigh == -1 || shigh == -1 {
            return (UnixListener::from_raw_fd(lfd), UnixStream::from_raw_fd(sfd));
        }
        libc::close(lfd);
        libc::close(sfd);
        libc::dup2(lhigh, LISTENER_FD);
        libc::dup2(shigh, STARTER_FD);
        super::pty::close_from(STARTER_FD + 1, super::pty::fd_limit());
        let listener = UnixListener::from_raw_fd(LISTENER_FD);
        let starter = UnixStream::from_raw_fd(STARTER_FD);
        // dup2 clears close-on-exec: set it again, so nothing the server
        // starts inherits the sockets; only a live upgrade clears it, for
        // the descriptors it keeps (sys::upgrade).
        let _ = rustix::io::fcntl_setfd(&listener, rustix::io::FdFlags::CLOEXEC);
        let _ = rustix::io::fcntl_setfd(&starter, rustix::io::FdFlags::CLOEXEC);
        (listener, starter)
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
