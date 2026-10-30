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
    let env: Vec<(CString, CString)> = spec
        .env
        .iter()
        .filter(|(k, _)| !k.is_empty() && !k.contains('=') && k != "PWD")
        .map(|(k, v)| Ok((cstring(k.as_bytes())?, cstring(v.as_bytes())?)))
        .collect::<io::Result<_>>()?;
    let pwd_key = cstring(b"PWD")?;
    let ws = libc::winsize {
        ws_row: spec.rows,
        ws_col: spec.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let mut master: libc::c_int = -1;
    // SAFETY: forkpty writes the master fd into `master`; name and termios
    // are null (allowed), `ws` outlives the call. The server is
    // single-threaded, so the child may run the code below until exec.
    let pid = unsafe {
        libc::forkpty(
            &raw mut master,
            std::ptr::null_mut(),
            std::ptr::null(),
            &raw const ws,
        )
    };
    if pid == -1 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: we are the child of a single-threaded process; every
        // pointer below refers to memory built before fork, and the
        // function never returns (exec or _exit).
        unsafe { child(&dirs, &env, &pwd_key, &program, &argv_ptrs, direct) }
    }
    // SAFETY: forkpty succeeded, so `master` is a fresh fd we now own.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK)?;
    Ok(Spawned {
        master,
        pid: Pid::from_raw(pid).expect("forkpty returned a positive pid"),
    })
}

/// The child after `forkpty` (spawn.c:500-574).
///
/// # Safety
/// Only call in the child of `fork` in a single-threaded parent; all
/// pointers must be valid NUL-terminated strings (and a null-terminated
/// `argv`).
unsafe fn child(
    dirs: &[CString],
    env: &[(CString, CString)],
    pwd_key: &CString,
    program: &CString,
    argv: &[*const libc::c_char],
    direct: bool,
) -> ! {
    // SAFETY (whole body): libc calls on valid C strings in the forked
    // child; see the function contract.
    unsafe {
        // Working directory: the requested one, else home, else "/".
        let mut actual = None;
        for d in dirs {
            if libc::chdir(d.as_ptr()) == 0 {
                actual = Some(d);
                break;
            }
        }
        // The environment: exactly the pane's (environ_push).
        libc::clearenv();
        for (k, v) in env {
            libc::setenv(k.as_ptr(), v.as_ptr(), 1);
        }
        if let Some(d) = actual {
            libc::setenv(pwd_key.as_ptr(), d.as_ptr(), 1);
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
        // closefrom(STDERR_FILENO + 1).
        libc::close_range(3, libc::c_uint::MAX, 0);
        if direct {
            libc::execvp(program.as_ptr(), argv.as_ptr());
        } else {
            libc::execv(program.as_ptr(), argv.as_ptr());
        }
        libc::_exit(1)
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
