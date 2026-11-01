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

    let (ours, theirs) = cloexec_socketpair()?;
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

/// A Unix stream socket pair, both ends close-on-exec. `SOCK_CLOEXEC` does not
/// exist on macOS, so the flag is set with fcntl afterwards (the child dup2s
/// its end onto 0/1/2, which clears it there).
fn cloexec_socketpair() -> io::Result<(rustix::fd::OwnedFd, rustix::fd::OwnedFd)> {
    let (a, b) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::empty(),
        None,
    )?;
    for fd in [&a, &b] {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }
    Ok((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Run `command` to completion: its output and wait status.
    fn run(
        command: &str,
        cwd: Option<&str>,
        show_stderr: bool,
    ) -> (String, rustix::process::WaitStatus) {
        let spawned = spawn(&JobSpec {
            shell: PathBuf::from("/bin/sh"),
            command: command.to_owned(),
            cwd: cwd.map(PathBuf::from),
            home: Some(PathBuf::from("/")),
            env: vec![
                ("FOO".into(), "bar".into()),
                ("PWD".into(), "/stale".into()),
            ],
            show_stderr,
        })
        .expect("spawn");
        let mut file = std::fs::File::from(spawned.fd);
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("read: {e}"),
            }
        }
        let (_, status) =
            rustix::process::waitpid(Some(spawned.pid), rustix::process::WaitOptions::empty())
                .expect("waitpid")
                .expect("status");
        (String::from_utf8(out).expect("utf-8"), status)
    }

    #[test]
    fn job_runs_with_env_cwd_and_status() {
        // job.c:140-149: chdir and PWD; a missing directory falls back to
        // home. stdin is the socket, shut for writing: `cat` sees EOF.
        let (out, status) = run(
            "echo $FOO; pwd; echo $PWD; cat; exit 3",
            Some("/tmp"),
            false,
        );
        assert_eq!(out, "bar\n/tmp\n/tmp\n");
        assert_eq!(status.exit_status(), Some(3));
        let (out, _) = run("pwd", Some("/nonexistent/dir"), false);
        assert_eq!(out, "/\n");
        // No cwd: the environment's PWD is passed as given (the shell
        // itself may replace a stale one, so look at the raw environment).
        #[cfg(target_os = "linux")]
        let (out, _) = run("tr '\\0' '\\n' </proc/$$/environ | grep ^PWD=", None, false);
        #[cfg(target_os = "linux")]
        assert_eq!(out, "PWD=/stale\n");
    }

    #[test]
    fn job_stderr_only_with_show_stderr() {
        let (out, _) = run("echo out; echo err >&2", None, false);
        assert_eq!(out, "out\n");
        let (out, _) = run("echo out; echo err >&2", None, true);
        assert_eq!(out, "out\nerr\n");
        let (_, status) = run("kill -9 $$", None, false);
        assert_eq!(status.terminating_signal(), Some(9));
    }
}
