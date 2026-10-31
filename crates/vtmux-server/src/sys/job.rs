//! Forking a job (job.c `job_run`, 70-232): `<shell> -c <command>` with
//! its stdin and stdout on one end of a socket pair, stderr on it too or on
//! `/dev/null`.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::process::Pid;

/// What to run.
#[derive(Clone, Debug)]
pub struct JobSpec {
    /// The shell (`_PATH_BSHELL` unless `JOB_DEFAULTSHELL`).
    pub shell: PathBuf,
    pub command: String,
    /// Directory to start in; `None` stays in the server's.
    pub cwd: Option<PathBuf>,
    /// Fallback directory when `cwd` cannot be entered.
    pub home: Option<PathBuf>,
    /// The complete environment (`PWD` is set here when `cwd` is given).
    pub env: Vec<(String, String)>,
    /// `JOB_SHOWSTDERR`: stderr goes into the output too.
    pub show_stderr: bool,
}

/// A running job: its end of the socket pair (non-blocking) and pid.
#[derive(Debug)]
pub struct Spawned {
    pub fd: OwnedFd,
    pub pid: Pid,
}

fn cstring(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Fork and exec the job.
pub fn spawn(spec: &JobSpec) -> io::Result<Spawned> {
    let shell = cstring(spec.shell.as_os_str().as_bytes())?;
    let base = spec
        .shell
        .file_name()
        .map_or_else(|| spec.shell.as_os_str(), |b| b)
        .as_bytes();
    let argv0 = cstring(base)?;
    let dash_c = cstring(b"-c")?;
    let command = cstring(spec.command.as_bytes())?;
    let argv = [
        argv0.as_ptr(),
        dash_c.as_ptr(),
        command.as_ptr(),
        std::ptr::null(),
    ];
    // job.c:140-149: the directory, else home, else "/", PWD following.
    let dirs: Vec<CString> = match &spec.cwd {
        Some(cwd) => [
            Some(cwd.as_path()),
            spec.home.as_deref(),
            Some(Path::new("/")),
        ]
        .into_iter()
        .flatten()
        .map(|d| cstring(d.as_os_str().as_bytes()))
        .collect::<io::Result<_>>()?,
        None => Vec::new(),
    };
    let pwds: Vec<CString> = dirs
        .iter()
        .map(|d| cstring(&[b"PWD=", d.to_bytes()].concat()))
        .collect::<io::Result<_>>()?;
    let env: Vec<CString> = spec
        .env
        .iter()
        .filter(|(k, _)| !k.is_empty() && !k.contains('=') && (spec.cwd.is_none() || k != "PWD"))
        .map(|(k, v)| cstring(format!("{k}={v}").as_bytes()))
        .collect::<io::Result<_>>()?;
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
    let pwd_slot = envp.len();
    envp.push(std::ptr::null());
    envp.push(std::ptr::null());
    let devnull = cstring(b"/dev/null")?;
    let fd_limit = super::pty::fd_limit();

    let (ours, theirs) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )?;
    let child_fd = theirs.as_raw_fd();
    let show_stderr = spec.show_stderr;

    // job.c:108-109: every signal blocked across the fork.
    // SAFETY: sigset_t is a plain C struct, all-zero is a valid value, and
    // sigfillset/sigprocmask only read and write the two sets.
    let old = unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&raw mut all);
        libc::sigprocmask(libc::SIG_BLOCK, &raw const all, &raw mut old);
        old
    };
    // SAFETY: the server is single-threaded; the child runs only
    // async-signal-safe calls on memory built before the fork.
    let pid = unsafe { libc::fork() };
    let fork_error = io::Error::last_os_error();
    if pid == 0 {
        // SAFETY (whole block): the forked child of a single-threaded
        // process, valid C strings, never returns.
        unsafe {
            super::pty::clear_signals();
            for (d, pwd) in dirs.iter().zip(&pwds) {
                if libc::chdir(d.as_ptr()) == 0 {
                    envp[pwd_slot] = pwd.as_ptr();
                    break;
                }
            }
            if libc::dup2(child_fd, 0) == -1 || libc::dup2(child_fd, 1) == -1 {
                libc::_exit(1);
            }
            if show_stderr {
                if libc::dup2(child_fd, 2) == -1 {
                    libc::_exit(1);
                }
            } else {
                let null = libc::open(devnull.as_ptr(), libc::O_RDWR);
                if null == -1 || libc::dup2(null, 2) == -1 {
                    libc::_exit(1);
                }
            }
            super::pty::close_from(3, fd_limit);
            libc::execve(shell.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(1)
        }
    }
    // SAFETY: restores the mask saved above.
    unsafe {
        libc::sigprocmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
    drop(theirs);
    if pid == -1 {
        return Err(fork_error);
    }
    rustix::fs::fcntl_setfl(&ours, rustix::fs::OFlags::NONBLOCK)?;
    // job_write_callback (job.c:331-334): nothing to write, so the job's
    // stdin sees EOF at once.
    let _ = rustix::net::shutdown(&ours, rustix::net::Shutdown::Write);
    Ok(Spawned {
        fd: ours,
        pid: Pid::from_raw(pid).expect("fork returned a positive pid"),
    })
}
