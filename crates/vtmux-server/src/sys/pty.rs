//! Spawning a pane's process on a new PTY (spawn.c `spawn_pane`,
//! 455-574).

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::process::Pid;

/// What to run in a new pane.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    /// The command words: empty runs a login shell, one word runs
    /// `<shell> -c <word>`, more are `execvp`ed directly (spawn.c:547-574).
    pub argv: Vec<String>,
    /// `default-shell`, already checked.
    pub shell: PathBuf,
    pub cwd: PathBuf,
    /// Fallback directory when `cwd` cannot be entered.
    pub home: Option<PathBuf>,
    /// The complete child environment (`PWD` is added here).
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// A running pane process.
#[derive(Debug)]
pub struct Spawned {
    /// The PTY master, non-blocking.
    pub master: OwnedFd,
    pub pid: Pid,
}

fn cstring(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// The argv[0] and argv tmux builds for the shell cases (spawn.c:556-574):
/// the shell's basename, with a leading `-` for a login shell.
fn shell_argv(shell: &Path, argv: &[String]) -> Vec<Vec<u8>> {
    let base = shell
        .file_name()
        .map_or_else(|| shell.as_os_str(), |b| b)
        .as_bytes()
        .to_vec();
    if let [command] = argv {
        vec![base, b"-c".to_vec(), command.as_bytes().to_vec()]
    } else {
        let mut login = b"-".to_vec();
        login.extend_from_slice(&base);
        vec![login]
    }
}

/// Fork a child on a new PTY of `cols` x `rows` and exec the command.
pub fn spawn(spec: &SpawnSpec) -> io::Result<Spawned> {
    // Everything the child needs is built before fork.
    let direct = spec.argv.len() > 1;
    let words: Vec<Vec<u8>> = if direct {
        spec.argv.iter().map(|a| a.as_bytes().to_vec()).collect()
    } else {
        shell_argv(&spec.shell, &spec.argv)
    };
    let argv: Vec<CString> = words
        .iter()
        .map(|w| cstring(w))
        .collect::<io::Result<_>>()?;
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let program = if direct {
        argv[0].clone()
    } else {
        cstring(spec.shell.as_os_str().as_bytes())?
    };
    let dirs: Vec<CString> = [
        Some(spec.cwd.as_path()),
        spec.home.as_deref(),
        Some(Path::new("/")),
    ]
    .into_iter()
    .flatten()
    .map(|d| cstring(d.as_os_str().as_bytes()))
    .collect::<io::Result<_>>()?;
    // The child's environment is exactly the pane's (environ_push), built
    // as an `envp` before the fork: `clearenv` and `setenv` are neither
    // portable nor async-signal-safe. The slot before the terminator is
    // `PWD`, which the child points at the directory it actually entered.
    let env: Vec<CString> = spec
        .env
        .iter()
        .filter(|(k, _)| !k.is_empty() && !k.contains('=') && k != "PWD")
        .map(|(k, v)| cstring(format!("{k}={v}").as_bytes()))
        .collect::<io::Result<_>>()?;
    let pwds: Vec<CString> = dirs
        .iter()
        .map(|d| cstring(&[b"PWD=", d.to_bytes()].concat()))
        .collect::<io::Result<_>>()?;
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
    let pwd_slot = envp.len();
    envp.push(std::ptr::null());
    envp.push(std::ptr::null());
    let candidates = exec_candidates(&program, &spec.env, direct)?;
    let fd_limit = fd_limit();
    let ws = libc::winsize {
        ws_row: spec.rows,
        ws_col: spec.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // spawn.c:454-456: block every signal across the fork so no handler
    // runs in the child before it resets them.
    // SAFETY: sigset_t is a plain C struct, all-zero is a valid value, and
    // sigfillset/sigprocmask only read and write the two sets.
    let old = unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&raw mut all);
        libc::sigprocmask(libc::SIG_BLOCK, &raw const all, &raw mut old);
        old
    };
    let mut master: libc::c_int = -1;
    // SAFETY: forkpty writes the master fd into `master`; `ws` outlives the
    // call. The server is single-threaded, so the child may run the code
    // below until exec.
    let pid = unsafe { forkpty(&mut master, &ws) };
    let fork_error = io::Error::last_os_error();
    if pid == 0 {
        // SAFETY: we are the child of a single-threaded process; every
        // pointer below refers to memory built before fork, and the
        // function never returns (exec or _exit).
        unsafe {
            child(&mut Child {
                dirs: &dirs,
                pwds: &pwds,
                envp: &mut envp,
                pwd_slot,
                argv: &argv_ptrs,
                candidates: &candidates,
                fd_limit,
            })
        }
    }
    // SAFETY: restores the mask saved above.
    unsafe {
        libc::sigprocmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
    if pid == -1 {
        return Err(fork_error);
    }
    // SAFETY: forkpty succeeded, so `master` is a fresh fd we now own.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK)?;
    Ok(Spawned {
        master,
        pid: Pid::from_raw(pid).expect("forkpty returned a positive pid"),
    })
}

/// `forkpty(3)`; the argument types differ between libcs.
///
/// # Safety
/// Same as `forkpty`: the caller is single-threaded or runs only
/// async-signal-safe code in the child.
unsafe fn forkpty(master: &mut libc::c_int, ws: &libc::winsize) -> libc::pid_t {
    #[cfg(target_vendor = "apple")]
    {
        let mut ws = *ws;
        // SAFETY: valid out-pointers; name and termios may be null.
        unsafe {
            libc::forkpty(
                master,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut ws,
            )
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    // SAFETY: valid out-pointers; name and termios may be null.
    unsafe {
        libc::forkpty(master, std::ptr::null_mut(), std::ptr::null(), ws)
    }
}

/// The paths `execvp` would try for a directly executed command: the
/// program itself when it holds a `/` (or for the shell cases), else
/// `$PATH` (the pane's, defaulting like `confstr(_CS_PATH)`) joined with it.
fn exec_candidates(
    program: &CString,
    env: &[(String, String)],
    direct: bool,
) -> io::Result<Vec<CString>> {
    let name = program.to_bytes();
    if !direct || name.contains(&b'/') {
        return Ok(vec![program.clone()]);
    }
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map_or("/usr/bin:/bin", |(_, v)| v.as_str());
    path.split(':')
        .map(|dir| {
            let dir = if dir.is_empty() { "." } else { dir };
            cstring(&[dir.as_bytes(), b"/", name].concat())
        })
        .collect()
}

/// One past the highest descriptor the child needs to close.
fn fd_limit() -> libc::c_int {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(1 << 16);
    libc::c_int::try_from(limit.min(1 << 16)).unwrap_or(1 << 16)
}

/// What the forked child needs, all built before the fork.
struct Child<'a> {
    dirs: &'a [CString],
    /// `PWD=<dir>` for each of `dirs`.
    pwds: &'a [CString],
    envp: &'a mut [*const libc::c_char],
    pwd_slot: usize,
    argv: &'a [*const libc::c_char],
    candidates: &'a [CString],
    fd_limit: libc::c_int,
}

/// The child after `forkpty` (spawn.c:500-574).
///
/// # Safety
/// Only call in the child of `fork` in a single-threaded parent; all
/// pointers must be valid NUL-terminated strings (and null-terminated
/// `argv`/`envp`).
unsafe fn child(c: &mut Child<'_>) -> ! {
    // SAFETY (whole body): libc calls on valid C strings in the forked
    // child; see the function contract.
    unsafe {
        // Working directory: the requested one, else home, else "/".
        for (d, pwd) in c.dirs.iter().zip(c.pwds) {
            if libc::chdir(d.as_ptr()) == 0 {
                c.envp[c.pwd_slot] = pwd.as_ptr();
                break;
            }
        }
        // VERASE is tmux's backspace (\177) and IUTF8 is on (spawn.c:524-538).
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &raw mut tio) != 0 {
            libc::_exit(1);
        }
        tio.c_cc[libc::VERASE] = 0o177;
        tio.c_iflag |= libc::IUTF8;
        if libc::tcsetattr(0, libc::TCSANOW, &raw const tio) != 0 {
            libc::_exit(1);
        }
        // Signals back to defaults, mask cleared (proc_clear_signals): Rust
        // ignores SIGPIPE and exec keeps ignored signals ignored.
        for sig in [
            libc::SIGPIPE,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGQUIT,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGCHLD,
            libc::SIGTERM,
            libc::SIGUSR1,
            libc::SIGUSR2,
            libc::SIGWINCH,
            libc::SIGCONT,
        ] {
            libc::signal(sig, libc::SIG_DFL);
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const set, std::ptr::null_mut());
        close_from(3, c.fd_limit);
        for path in c.candidates {
            libc::execve(path.as_ptr(), c.argv.as_ptr(), c.envp.as_ptr());
        }
        libc::_exit(1)
    }
}

/// `closefrom(first)`: `close_range` where the kernel has it, else a loop
/// up to the descriptor limit.
///
/// # Safety
/// Closes descriptors; only call in a forked child that owns none it needs.
unsafe fn close_from(first: libc::c_int, limit: libc::c_int) {
    #[cfg(target_os = "linux")]
    // SAFETY: close_range on a range of descriptors the child does not use.
    unsafe {
        if libc::close_range(first.cast_unsigned(), libc::c_uint::MAX, 0) == 0 {
            return;
        }
    }
    for fd in first..limit {
        // SAFETY: close on a descriptor the child does not use.
        unsafe {
            libc::close(fd);
        }
    }
}

/// `TIOCSWINSZ` on a pane's master.
pub fn resize(master: &OwnedFd, cols: u16, rows: u16) -> io::Result<()> {
    rustix::termios::tcsetwinsize(
        master,
        rustix::termios::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )?;
    Ok(())
}

/// `osdep_get_name` (osdep-linux.c:30-61): argv[0] of the PTY's foreground
/// process group, from /proc.
#[must_use]
pub fn foreground_name(master: &OwnedFd) -> Option<String> {
    let pgrp = rustix::termios::tcgetpgrp(master).ok()?;
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", pgrp.as_raw_nonzero())).ok()?;
    let first = cmdline.split(|b| *b == 0).next()?;
    Some(String::from_utf8_lossy(first).into_owned())
}

/// `osdep_get_cwd` (osdep-linux.c:64-89): the foreground process group's
/// working directory.
#[must_use]
pub fn foreground_cwd(master: &OwnedFd) -> Option<PathBuf> {
    let pgrp = rustix::termios::tcgetpgrp(master).ok()?;
    std::fs::read_link(format!("/proc/{}/cwd", pgrp.as_raw_nonzero())).ok()
}

/// `ttyname` of the PTY's slave side (`#{pane_tty}`).
#[must_use]
pub fn tty_name(master: &OwnedFd) -> Option<String> {
    let name = rustix::pty::ptsname(master, Vec::new()).ok()?;
    Some(
        OsStr::from_bytes(name.as_bytes())
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_argv_follows_spawn_c() {
        let sh = Path::new("/bin/sh");
        assert_eq!(shell_argv(sh, &[]), vec![b"-sh".to_vec()]);
        assert_eq!(
            shell_argv(sh, &["cat".into()]),
            vec![b"sh".to_vec(), b"-c".to_vec(), b"cat".to_vec()]
        );
    }
}
