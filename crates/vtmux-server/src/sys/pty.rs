//! Spawning a process on a new PTY: a pane's (spawn.c `spawn_pane`,
//! 455-574) or a PTY job's (job.c `job_run` with `JOB_PTY`, 70-232).

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::process::Pid;

/// What runs on the PTY.
#[derive(Clone, Debug)]
pub enum Program {
    /// A pane's command words: empty runs a login shell, one word runs
    /// `<shell> -c <word>`, more are `execvp`ed directly (spawn.c:547-574).
    /// The terminal gets [`SpawnSpec::chars`] and `IUTF8`.
    Pane(Vec<String>),
    /// A job's shell command: `<shell> -c <command>`, argv[0] the shell's
    /// basename (job.c:204-208). The terminal is as `forkpty` made it.
    Command(String),
    /// A job's argument vector, `execvp`ed as it is (job.c:209-212).
    Argv(Vec<String>),
}

/// What to run on a new PTY.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub program: Program,
    /// `default-shell`, already checked.
    pub shell: PathBuf,
    pub cwd: PathBuf,
    /// Fallback directory when `cwd` cannot be entered.
    pub home: Option<PathBuf>,
    /// The complete child environment (`PWD` is added here).
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    /// The size in pixels (`ws_xpixel`, `ws_ypixel`): a pane's cells times
    /// its window's cell size (spawn.c:451-452), 0 for a popup (job.c:
    /// 114-115).
    pub pixels: (u16, u16),
    /// A pane terminal's control characters; a job's are `forkpty`'s.
    pub chars: PaneChars,
}

/// A pane terminal's control characters (spawn.c:520-532).
#[derive(Clone, Debug)]
pub struct PaneChars {
    /// The session's (`s->tio`): the `c_cc` of the terminal that made it
    /// ([`control_chars`]), used when it is this platform's size.
    pub session: Option<Vec<u8>>,
    /// VERASE: the `backspace` key when it is below 0x7f, else `^?`.
    pub erase: u8,
}

impl Default for PaneChars {
    fn default() -> Self {
        Self {
            session: None,
            erase: 0o177,
        }
    }
}

/// The control characters (`c_cc`) of the terminal on `fd`, as the
/// platform lays them out; `None` when it is not a terminal.
pub fn control_chars(fd: impl std::os::fd::AsFd) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;
    // SAFETY: tcgetattr writes a termios into the zeroed local, which is
    // a plain C struct; the descriptor is borrowed for the call.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        (libc::tcgetattr(fd.as_fd().as_raw_fd(), &raw mut tio) == 0).then(|| tio.c_cc.to_vec())
    }
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
        vec![
            base,
            b"-c".to_vec(),
            vtmux_core::rawstr::to_bytes(command).into_owned(),
        ]
    } else {
        let mut login = b"-".to_vec();
        login.extend_from_slice(&base);
        vec![login]
    }
}

/// The argv to exec, whether it is `execvp`ed (`direct`), and whether the
/// terminal takes a pane's settings.
fn program_words(program: &Program, shell: &Path) -> (bool, Vec<Vec<u8>>, bool) {
    let bytes = |words: &[String]| -> Vec<Vec<u8>> {
        words
            .iter()
            .map(|a| vtmux_core::rawstr::to_bytes(a).into_owned())
            .collect()
    };
    match program {
        Program::Pane(argv) if argv.len() > 1 => (true, bytes(argv), true),
        Program::Pane(argv) => (false, shell_argv(shell, argv), true),
        Program::Command(command) => (
            false,
            shell_argv(shell, std::slice::from_ref(command)),
            false,
        ),
        Program::Argv(argv) => (true, bytes(argv), false),
    }
}

/// Fork a child on a new PTY of `cols` x `rows` and exec the command.
pub fn spawn(spec: &SpawnSpec) -> io::Result<Spawned> {
    // Everything the child needs is built before fork.
    let (direct, words, pane) = program_words(&spec.program, &spec.shell);
    if words.is_empty() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
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
        .map(|(k, v)| cstring(&vtmux_core::rawstr::to_bytes(&format!("{k}={v}"))))
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
    // execvp's ENOEXEC fallback argv: `/bin/sh <file> args...` (glibc
    // maybe_script_execute passes _PATH_BSHELL as argv[0]), slot 1 filled in
    // with the candidate path by the child.
    let sh = cstring(b"/bin/sh")?;
    let mut sh_argv: Vec<*const libc::c_char> = vec![sh.as_ptr(), std::ptr::null()];
    sh_argv.extend(argv_ptrs.iter().skip(1));
    let fd_limit = fd_limit();
    let ws = libc::winsize {
        ws_row: spec.rows,
        ws_col: spec.cols,
        ws_xpixel: spec.pixels.0,
        ws_ypixel: spec.pixels.1,
    };

    // spawn.c:454-456: block every signal across the fork so no handler
    // runs in the child before it resets them. pthread_sigmask, not
    // sigprocmask: XNU's sigprocmask sets the mask of every thread in the
    // process (`block_procsigmask`), which in a threaded test binary
    // leaks the block into the others.
    // SAFETY: sigset_t is a plain C struct, all-zero is a valid value, and
    // sigfillset/pthread_sigmask only read and write the two sets.
    let old = unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&raw mut all);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const all, &raw mut old);
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
                sh_argv: &mut sh_argv,
                direct,
                pane,
                chars: (
                    spec.chars
                        .session
                        .as_deref()
                        .filter(|cc| cc.len() == libc::NCCS),
                    spec.chars.erase,
                ),
                fd_limit,
            })
        }
    }
    // SAFETY: restores the mask saved above.
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
    if pid == -1 {
        return Err(fork_error);
    }
    // SAFETY: forkpty succeeded, so `master` is a fresh fd we now own.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK)?;
    // forkpty makes the master without close-on-exec. With it, nothing
    // the server starts inherits a pane's terminal (a dry run, a
    // `Command`), and only a live upgrade clears it, for the descriptors
    // it keeps (sys::upgrade).
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
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

/// The search path `execvp` takes when there is no `PATH`: macOS's and
/// FreeBSD's `_PATH_DEFPATH` (paths.h, gen/exec.c), musl's literal
/// (src/process/execvp.c), glibc's `CS_PATH` (posix/execvpe.c), which a
/// distribution may change and glibc itself reads from
/// [`exec_default_path`]. Other targets (the other BSDs, whose
/// `_PATH_DEFPATH` lists more) get glibc's upstream value.
#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
const EXEC_DEFAULT_PATH: &str = "/usr/bin:/bin";
#[cfg(all(target_env = "musl", not(target_os = "freebsd")))]
const EXEC_DEFAULT_PATH: &str = "/usr/local/bin:/bin:/usr/bin";
#[cfg(not(any(target_vendor = "apple", target_os = "freebsd", target_env = "musl")))]
const EXEC_DEFAULT_PATH: &str = "/bin:/usr/bin";

/// glibc's `execvpe` searches its build's `CS_PATH` (posix/execvpe.c),
/// which a distribution sets (EL9's is `/usr/bin`) and `confstr(_CS_PATH)`
/// returns (posix/confstr.c); [`EXEC_DEFAULT_PATH`] if that fails. Only
/// glibc: macOS's, FreeBSD's and musl's `confstr(_CS_PATH)` is not what
/// their `execvp` searches.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn exec_default_path() -> &'static str {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        // SAFETY: a null buffer of length 0 asks for the size only.
        let len = unsafe { libc::confstr(libc::_CS_PATH, std::ptr::null_mut(), 0) };
        if len == 0 {
            return EXEC_DEFAULT_PATH.to_owned();
        }
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` holds `len` bytes, the size confstr asked for
        // (its NUL included).
        let got = unsafe { libc::confstr(libc::_CS_PATH, buf.as_mut_ptr().cast(), len) };
        match std::ffi::CStr::from_bytes_until_nul(&buf) {
            Ok(s) if got == len => s.to_str().unwrap_or(EXEC_DEFAULT_PATH).to_owned(),
            _ => EXEC_DEFAULT_PATH.to_owned(),
        }
    })
}

/// [`EXEC_DEFAULT_PATH`]: this libc's `execvp` takes it as it is.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn exec_default_path() -> &'static str {
    EXEC_DEFAULT_PATH
}

/// The paths `execvp` would try for a directly executed command: the
/// program itself when it holds a `/` (or for the shell cases), else
/// `$PATH` (the pane's, else [`exec_default_path`]) joined with it.
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
        .map_or(exec_default_path(), |(_, v)| v.as_str());
    path.split(':')
        .map(|dir| {
            // glibc execvpe tries the bare name for an empty PATH component
            // (leading, trailing or `::`), not `./name`; the difference shows
            // in a script's $0 and /proc/<pid>/cmdline.
            if dir.is_empty() {
                Ok(program.clone())
            } else {
                cstring(&[dir.as_bytes(), b"/", name].concat())
            }
        })
        .collect()
}

/// One past the highest descriptor the child needs to close.
pub(super) fn fd_limit() -> libc::c_int {
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
    /// `["/bin/sh", <file>, argv[1..]..., null]` for the ENOEXEC fallback.
    sh_argv: &'a mut [*const libc::c_char],
    /// A multi-word command, run as `execvp` does (spawn.c:552).
    direct: bool,
    /// A pane's terminal settings (spawn.c:524-538); a job keeps
    /// `forkpty`'s.
    pane: bool,
    /// The session's `c_cc`, of this platform's size, and VERASE.
    chars: (Option<&'a [u8]>, u8),
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
        // The session's control characters, VERASE the backspace key, and
        // IUTF8 on (spawn.c:520-538).
        if c.pane {
            let mut tio: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &raw mut tio) != 0 {
                libc::_exit(1);
            }
            let (session, erase) = c.chars;
            if let Some(cc) = session {
                tio.c_cc.copy_from_slice(cc);
            }
            tio.c_cc[libc::VERASE] = erase;
            tio.c_iflag |= libc::IUTF8;
            if libc::tcsetattr(0, libc::TCSANOW, &raw const tio) != 0 {
                libc::_exit(1);
            }
        }
        clear_signals();
        close_from(3, c.fd_limit);
        for path in c.candidates {
            libc::execve(path.as_ptr(), c.argv.as_ptr(), c.envp.as_ptr());
            let mut errno = last_errno();
            if c.direct && errno == libc::ENOEXEC {
                // execvp (glibc, macOS): a file the kernel cannot execute
                // is run as `/bin/sh <file> args...`.
                c.sh_argv[1] = path.as_ptr();
                libc::execve(c"/bin/sh".as_ptr(), c.sh_argv.as_ptr(), c.envp.as_ptr());
                errno = last_errno();
            }
            // execvp keeps searching on these (EACCES is remembered, which
            // only matters for the error it returns); anything else ends it.
            if !matches!(
                errno,
                libc::EACCES
                    | libc::ENOENT
                    | libc::ESTALE
                    | libc::ENOTDIR
                    | libc::ENODEV
                    | libc::ETIMEDOUT
            ) {
                break;
            }
        }
        libc::_exit(1)
    }
}

/// Signals back to defaults, mask cleared (`proc_clear_signals`): Rust
/// ignores SIGPIPE and exec keeps ignored signals ignored.
///
/// # Safety
/// Only call in a forked child before exec.
pub(super) unsafe fn clear_signals() {
    // SAFETY: signal(2) to SIG_DFL and pthread_sigmask on a local set.
    unsafe {
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
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const set, std::ptr::null_mut());
    }
}

fn last_errno() -> libc::c_int {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `closefrom(first)`: `close_range` where the kernel has it, the open
/// descriptors from `proc_pidinfo(PROC_PIDLISTFDS)` on macOS (tmux's
/// compat/closefrom.c), else a loop up to the descriptor limit.
///
/// # Safety
/// Closes descriptors; only call in a forked child that owns none it needs.
pub(super) unsafe fn close_from(first: libc::c_int, limit: libc::c_int) {
    #[cfg(target_os = "linux")]
    // SAFETY: close_range on a range of descriptors the child does not use.
    unsafe {
        if libc::close_range(first.cast_unsigned(), libc::c_uint::MAX, 0) == 0 {
            return;
        }
    }
    #[cfg(target_os = "macos")]
    {
        // Closing 65536 descriptors one by one takes 10-15 ms there, which
        // delays every pane's and job's exec (and so its exit). The list
        // goes in a fixed buffer: no allocation after fork. A full buffer
        // may be truncated, so it falls back to the loop.
        const MAX: usize = 1024;
        let mut fds = [libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        }; MAX];
        let size = libc::c_int::try_from(size_of_val(&fds)).unwrap_or(0);
        // SAFETY: the buffer holds `size` bytes of proc_fdinfo.
        let got = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                size,
            )
        };
        if got > 0 && got < size {
            let n = usize::try_from(got).unwrap_or(0) / size_of::<libc::proc_fdinfo>();
            for info in &fds[..n] {
                if info.proc_fd >= first {
                    // SAFETY: close on a descriptor the child does not use.
                    unsafe {
                        libc::close(info.proc_fd);
                    }
                }
            }
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

/// `TIOCSWINSZ` on a pane's master (window.c `window_pane_send_resize`):
/// `cols` x `rows`, `pixels` wide and high.
pub fn resize(
    master: impl std::os::fd::AsFd,
    cols: u16,
    rows: u16,
    (xpixel, ypixel): (u16, u16),
) -> io::Result<()> {
    rustix::termios::tcsetwinsize(
        master,
        rustix::termios::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: xpixel,
            ws_ypixel: ypixel,
        },
    )?;
    Ok(())
}

/// The PTY's foreground process group (`tcgetpgrp`), `None` when it has
/// none.
#[must_use]
pub fn foreground_pgid(master: impl std::os::fd::AsFd) -> Option<i32> {
    Some(
        rustix::termios::tcgetpgrp(master)
            .ok()?
            .as_raw_nonzero()
            .get(),
    )
}

/// Whether process group `pgid` still has a process, one not yet reaped
/// included (`kill(-pgid, 0)`: another user's group answers `EPERM`).
#[must_use]
pub fn group_alive(pgid: i32) -> bool {
    let Some(leader) = Pid::from_raw(pgid) else {
        return false;
    };
    match rustix::process::test_kill_process_group(leader) {
        Ok(()) => true,
        Err(e) => e != rustix::io::Errno::SRCH,
    }
}

/// `osdep_get_name` (osdep-linux.c:30-61): argv[0] of the PTY's foreground
/// process group, from /proc.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn foreground_name(master: &OwnedFd) -> Option<String> {
    let pgrp = rustix::termios::tcgetpgrp(master).ok()?;
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", pgrp.as_raw_nonzero())).ok()?;
    let first = cmdline.split(|b| *b == 0).next()?;
    Some(vtmux_core::rawstr::from_bytes(first))
}

/// `osdep_get_name` (osdep-darwin.c:37-69): the foreground process group's
/// `p_comm` (the executable's name, at most 16 bytes, so `/bin/sh` is
/// `bash`), from `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)`.
#[cfg(target_os = "macos")]
#[must_use]
pub fn foreground_name(master: &OwnedFd) -> Option<String> {
    let pgrp = rustix::termios::tcgetpgrp(master).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    // SAFETY: the buffer is a zeroed proc_bsdshortinfo of the size passed.
    let got = unsafe {
        libc::proc_pidinfo(
            pgrp.as_raw_nonzero().get(),
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if got != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled all `size` bytes.
    let info = unsafe { info.assume_init() };
    let comm: Vec<u8> = info
        .pbsi_comm
        .iter()
        .map(|&c| c.cast_unsigned())
        .take_while(|&b| b != 0)
        .collect();
    (!comm.is_empty()).then(|| vtmux_core::rawstr::from_bytes(&comm))
}

/// `osdep_get_cwd` (osdep-linux.c:64-89): the foreground process group's
/// working directory or, when that can't be read (the group's leader has
/// exited, or is another user's, as under `sudo`), the session leader's,
/// found through `TIOCGSID`.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn foreground_cwd(master: &OwnedFd) -> Option<PathBuf> {
    let cwd = |pid: rustix::process::Pid| {
        std::fs::read_link(format!("/proc/{}/cwd", pid.as_raw_nonzero())).ok()
    };
    let pgrp = rustix::termios::tcgetpgrp(master).ok()?;
    cwd(pgrp).or_else(|| cwd(rustix::termios::tcgetsid(master).ok()?))
}

/// `osdep_get_cwd` (osdep-darwin.c:71-91): the foreground process group's
/// working directory, from `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
#[cfg(target_os = "macos")]
#[must_use]
pub fn foreground_cwd(master: &OwnedFd) -> Option<PathBuf> {
    let pgrp = rustix::termios::tcgetpgrp(master).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = libc::c_int::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: the buffer is a zeroed proc_vnodepathinfo of the size passed.
    let got = unsafe {
        libc::proc_pidinfo(
            pgrp.as_raw_nonzero().get(),
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if got != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled all `size` bytes.
    let info = unsafe { info.assume_init() };
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|&c| c.cast_unsigned())
        .take_while(|&b| b != 0)
        .collect();
    Some(PathBuf::from(OsStr::from_bytes(&path)))
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

    /// With no `PATH`, the search is `execvp`'s own: glibc's `/bin` first,
    /// macOS's and FreeBSD's `/usr/bin` first, musl's `/usr/local/bin`
    /// first.
    #[test]
    fn no_path_searches_as_execvp_does() {
        let program = CString::new("prog").unwrap();
        let got = exec_candidates(&program, &[], true).unwrap();
        let want: &[&str] = if cfg!(any(target_vendor = "apple", target_os = "freebsd")) {
            &["/usr/bin/prog", "/bin/prog"]
        } else if cfg!(target_env = "musl") {
            &["/usr/local/bin/prog", "/bin/prog", "/usr/bin/prog"]
        } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
            return;
        } else {
            &["/bin/prog", "/usr/bin/prog"]
        };
        let got: Vec<_> = got.iter().map(|c| c.to_str().unwrap()).collect();
        assert_eq!(got, want);
    }

    /// glibc's `execvp` searches the `CS_PATH` its build was given, which
    /// `getconf PATH` prints (EL9: `/usr/bin`, upstream `/bin:/usr/bin`).
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn no_path_searches_glibcs_cs_path() {
        let out = std::process::Command::new("getconf")
            .arg("PATH")
            .output()
            .expect("getconf");
        assert!(out.status.success());
        let cs_path = String::from_utf8(out.stdout).unwrap();
        let want: Vec<String> = cs_path
            .trim_end()
            .split(':')
            .map(|d| format!("{d}/prog"))
            .collect();
        let program = CString::new("prog").unwrap();
        let got = exec_candidates(&program, &[], true).unwrap();
        let got: Vec<_> = got.iter().map(|c| c.to_str().unwrap()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn shell_argv_follows_spawn_c() {
        let sh = Path::new("/bin/sh");
        assert_eq!(shell_argv(sh, &[]), vec![b"-sh".to_vec()]);
        assert_eq!(
            shell_argv(sh, &["cat".into()]),
            vec![b"sh".to_vec(), b"-c".to_vec(), b"cat".to_vec()]
        );
    }

    /// Everything a PTY child writes until it closes the terminal.
    fn output(spec: &SpawnSpec) -> String {
        let spawned = spawn(spec).expect("spawn");
        let mut file = std::fs::File::from(spawned.master);
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match std::io::Read::read(&mut file, &mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                // EIO once the child is gone.
                Err(_) => break,
            }
        }
        let _ = rustix::process::waitpid(Some(spawned.pid), rustix::process::WaitOptions::empty());
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn job_programs_follow_job_c() {
        // job.c:204-212: `<shell> -c cmd` with the shell's basename as $0,
        // or argv execvp'd; the PTY has the size asked for.
        let spec = |program| SpawnSpec {
            program,
            shell: PathBuf::from("/bin/sh"),
            cwd: PathBuf::from("/"),
            home: None,
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            cols: 18,
            rows: 4,
            pixels: (0, 0),
            chars: PaneChars::default(),
        };
        let out = output(&spec(Program::Command("echo $0; stty size; pwd".into())));
        assert_eq!(out, "sh\r\n4 18\r\n/\r\n");
        let out = output(&spec(Program::Argv(vec![
            "sh".into(),
            "-c".into(),
            "echo $0 $1".into(),
            "a".into(),
            "b".into(),
        ])));
        assert_eq!(out, "a b\r\n");
    }

    /// The PTY starts at the size in pixels asked for, and a resize sets
    /// it with the cells (spawn.c:451-452, `window_pane_send_resize`).
    #[test]
    fn the_pty_carries_its_size_in_pixels() {
        let spawned = spawn(&SpawnSpec {
            program: Program::Argv(vec!["sleep".into(), "100".into()]),
            shell: PathBuf::from("/bin/sh"),
            cwd: PathBuf::from("/"),
            home: None,
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            cols: 18,
            rows: 4,
            pixels: (180, 80),
            chars: PaneChars::default(),
        })
        .expect("spawn");
        let size = || {
            let ws = rustix::termios::tcgetwinsize(&spawned.master).unwrap();
            (ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel)
        };
        assert_eq!(size(), (18, 4, 180, 80));
        resize(&spawned.master, 20, 5, (320, 160)).unwrap();
        assert_eq!(size(), (20, 5, 320, 160));
        let _ = rustix::process::kill_process(spawned.pid, rustix::process::Signal::KILL);
        let _ = rustix::process::waitpid(Some(spawned.pid), rustix::process::WaitOptions::empty());
    }

    /// The master is close-on-exec, so nothing else the server starts
    /// inherits a pane's terminal; and a pane started while the server
    /// has the handled signals blocked (a live upgrade's new image, until
    /// its handlers are in) starts with none blocked (`clear_signals`).
    #[test]
    fn a_pane_has_a_private_master_and_no_signal_blocked() {
        let spec = SpawnSpec {
            program: Program::Argv(vec![
                "sh".into(),
                "-c".into(),
                "kill -HUP $$; echo blocked".into(),
            ]),
            shell: PathBuf::from("/bin/sh"),
            cwd: PathBuf::from("/"),
            home: None,
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            cols: 20,
            rows: 4,
            pixels: (0, 0),
            chars: PaneChars::default(),
        };
        let spawned = spawn(&spec).expect("spawn");
        let flags = rustix::io::fcntl_getfd(&spawned.master).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        let _ = rustix::process::kill_process(spawned.pid, rustix::process::Signal::KILL);
        let _ = rustix::process::waitpid(Some(spawned.pid), rustix::process::WaitOptions::empty());

        let mask = super::super::signal::block_handled();
        let out = output(&spec);
        super::super::signal::restore_mask(mask);
        assert_eq!(out, "");
    }
}
