//! The system side of a live upgrade (D27, docs/specs/2026-11-06-live-upgrade.md,
//! *Live upgrade*): the anonymous memory file the snapshot travels in
//! (decision 19), the dry run (decision 7), `execve` in place keeping the
//! descriptors the snapshot names (decision 2), and taking them back by
//! number in the new image.
//!
//! The new image is `<binary> --vtmux-resume <fd>` ([`RESUME_FLAG`]); a
//! switch back after a failed resume adds [`SWITCHED_BACK_FLAG`], so the
//! image it lands in does not switch again. The dry run is
//! `<binary> --vtmux-check-snapshot <fd>` ([`CHECK_FLAG`]). A binary whose
//! dry run says it takes one ([`TAKES_SOCKET`]) is started as `<binary> -S
//! <socket> --vtmux-resume <fd>` (`argv[0]` the binary exec'd, cleaned
//! lexically: no `.` or `..`; a symlink is kept as given, not resolved), so
//! `ps` (the oracle runner, `cargo xtask reap`) tells which server it is by
//! the `-S` socket and the resume flag, whatever the binary is called; an
//! image of a binary from before that parses only the words above, so it
//! gets them alone and `ps` shows nothing of whose it is.

use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};

/// The new image's flag: resume from the snapshot in the descriptor that
/// follows.
pub const RESUME_FLAG: &str = "--vtmux-resume";

/// After [`RESUME_FLAG`] `<fd>`: this image is the one a failed resume
/// switched back to; if it fails too it must not switch again.
pub const SWITCHED_BACK_FLAG: &str = "--vtmux-switched-back";

/// The dry run's flag: decode and verify the snapshot in the descriptor
/// that follows, exit 0 or non-zero with a message on stderr.
pub const CHECK_FLAG: &str = "--vtmux-check-snapshot";

/// The flag a binary answers with its Ghostty patch-stack hash (decision
/// 23: `vtmux_vt::PATCH_STACK`) on stdout and exits 0: the old image asks
/// the new one before it writes the snapshot ([`patch_stack`]), and sends
/// history in Ghostty's form only to a binary that gives its own hash. A
/// binary from before the flag prints its usage and exits 1, so it is sent
/// rows.
pub const PATCH_STACK_FLAG: &str = "--vtmux-patch-stack";

/// How long [`patch_stack`] waits for the answer.
pub const PATCH_STACK_LIMIT: Duration = Duration::from_secs(1);

/// What a dry run that passes writes on stderr when its binary takes `-S
/// <socket>` before [`RESUME_FLAG`]. Binaries from before it write
/// nothing, and the image running the dry run ignores the stderr of one
/// that passes, so either side may be the older.
pub const TAKES_SOCKET: &str = "vtmux-resume takes -S\n";

/// What a dry run that passes writes on stderr when its binary, re-attaching
/// a client, takes the terminal raw with its own settings in
/// `VTMUX_REATTACH_TTY` (`vtmux_core::protocol::REATTACH_TTY_ENV`): the
/// clients told to follow are then told to leave it raw across their exec
/// (`ServerMsg::UpgradeRaw`). A binary from before it writes nothing, and
/// its clients restore the terminal before the exec, as they always did.
pub const TAKES_RAW_TTY: &str = "vtmux-reattach takes the terminal raw\n";

/// What a dry run that read and rebuilt the snapshot writes on stderr, a
/// line of its own: this, the hash of the snapshot's body its header
/// records (`vtmux_snapshot::digest`) in lowercase hex, and a newline
/// ([`acknowledgement`]). A dry run passes only with that exact line: an
/// exit status of 0 alone is any program's (`/bin/echo`, `true`, a
/// script), and the exec that follows would replace the server with it.
/// A vtmux from before it writes no such line; it passes on exit 0 only
/// if its `-V` names one of the releases before it ([`LEGACY_VERSIONS`]),
/// so a downgrade to one still works.
pub const CHECKED: &str = "vtmux-snapshot-checked ";

/// What `<binary> -V` (run as `vtmux`, so a binary named `tmux` answers
/// as vtmux too) prints, exactly, for a vtmux from before [`CHECKED`]:
/// 3.8-beta1, 3.8-beta2, and the builds between 3.8-beta2 and the bump
/// to 3.8-beta3, which report 3.8-beta2.
/// `/bin/echo -V` prints `-V`, a script nothing.
pub const LEGACY_VERSIONS: &[&str] = &["vtmux 3.8-beta1", "vtmux 3.8-beta2"];

/// How long [`legacy_vtmux`] waits for `-V`: long enough for a loaded
/// laptop, or macOS assessing a binary it has not seen run. The wait
/// blocks the server's loop (it runs inside [`DryRun::poll`]): only a
/// dry run that exited 0 without the acknowledgement pays it, a release
/// before it answering in milliseconds, so the server freezes for the
/// whole 5 s only for a program that is no vtmux, exits 0 on
/// `--vtmux-check-snapshot` and then hangs on `-V`.
const VERSION_LIMIT: Duration = Duration::from_secs(5);

/// The line a dry run of the stream whose body hashes to `digest` answers
/// with ([`CHECKED`]).
#[must_use]
pub fn acknowledgement(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut line = String::from(CHECKED);
    for b in digest {
        let _ = write!(line, "{b:02x}");
    }
    line.push('\n');
    line
}

/// A dry run that passed: what the image it checked takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// `-S <socket>` before [`RESUME_FLAG`] ([`TAKES_SOCKET`]).
    pub takes_socket: bool,
    /// A re-attaching client's terminal left raw ([`TAKES_RAW_TTY`]).
    pub takes_raw_tty: bool,
}

/// The environment variable a switch back passes why the resume failed in,
/// for the image it lands in to log ([`take_failure_note`]). Not an
/// argument: an image from before it was added must still parse its
/// command line.
pub const FAILED_ENV: &str = "VTMUX_UPGRADE_FAILED";

/// How long an upgrade's dry run may take before it counts as failed (a
/// second more per 100k rows of history it imports, `server/resume.rs`).
/// The server goes on serving meanwhile (`server/upgrade.rs`), but its
/// requester waits, so a binary that hangs is given up on soon. A dry
/// run rebuilds the server from the snapshot and exits; it takes
/// milliseconds.
pub const DRY_RUN_LIMIT: Duration = Duration::from_secs(3);

/// [`DRY_RUN_LIMIT`] for the dry run of a switch back by path (macOS),
/// whose image has no server running yet and whose alternative is the
/// rebuild without terminal state.
const SWITCH_BACK_LIMIT: Duration = Duration::from_secs(30);

/// At most this much of a dry run's stderr is kept for the message.
const MESSAGE_LIMIT: usize = 64 * 1024;

/// `bytes` in an anonymous file, its offset at the end: a `memfd` on
/// Linux, elsewhere a file created in `dir` (the socket directory, private
/// to the user) and unlinked at once (decision 19). Close-on-exec is set:
/// [`exec_in_place`] and [`dry_run`] clear it for the image they start.
///
/// # Errors
/// The file can't be created or written.
pub fn memfile(dir: &Path, bytes: &[u8]) -> io::Result<OwnedFd> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let fd = {
        let _ = dir;
        rustix::fs::memfd_create(c"vtmux-snapshot", rustix::fs::MemfdFlags::CLOEXEC)?
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let fd = unlinked_file(dir)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)?;
    Ok(file.into())
}

/// A new file in `dir`, mode 0600, already unlinked.
#[cfg_attr(
    all(any(target_os = "linux", target_os = "android"), not(test)),
    expect(dead_code, reason = "macOS's memfile; tested on Linux too")
)]
fn unlinked_file(dir: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt as _;
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    loop {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(".vtmux-snapshot.{}.{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(&path)?;
                return Ok(file.into());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// The whole content of a snapshot file, read from its start whatever its
/// offset (the writer, the dry run and the new image share one open file).
///
/// # Errors
/// The read fails.
pub fn read_all(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let file = std::fs::File::from(fd.try_clone_to_owned()?);
    let mut out = Vec::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        match file.read_at(&mut buf, out.len() as u64) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Descriptors already taken by [`inherited`].
static TAKEN: Mutex<BTreeSet<RawFd>> = Mutex::new(BTreeSet::new());

/// Take ownership of descriptor `fd`, inherited across `execve` from the
/// image before (the snapshot's descriptor table, or the snapshot itself),
/// and set close-on-exec on it again. Call it only at startup, before the
/// process has opened anything of its own: a number this process opened
/// itself has an owner already. Standard input, output and error are
/// refused, as is a number taken before.
///
/// # Errors
/// `fd` is not open, is 0-2, or was taken already.
pub fn inherited(fd: RawFd) -> io::Result<OwnedFd> {
    if fd <= 2 {
        return Err(io::Error::other(format!(
            "descriptor {fd} is not inherited"
        )));
    }
    let mut taken = TAKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !taken.insert(fd) {
        return Err(io::Error::other(format!("descriptor {fd} taken twice")));
    }
    // SAFETY: `fcntl(F_GETFD)` on any number is harmless; it only reports
    // whether the descriptor is open.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        taken.remove(&fd);
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is open (just checked), was kept open across `execve`
    // for this image (the caller's contract: startup, before anything here
    // opened a descriptor of its own), and `TAKEN` guarantees it is owned
    // once.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    fcntl_setfd(&owned, FdFlags::CLOEXEC)?;
    Ok(owned)
}

/// `nap` as a `poll` timeout, whole seconds included.
fn timespec(nap: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: i64::try_from(nap.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(nap.subsec_nanos()),
    }
}

/// `fd`'s close-on-exec cleared; the previous flags, to put back.
fn inheritable(fd: BorrowedFd<'_>) -> io::Result<FdFlags> {
    let flags = fcntl_getfd(fd)?;
    fcntl_setfd(fd, flags - FdFlags::CLOEXEC)?;
    Ok(flags)
}

/// The `execve` arguments of the dry run, built before the fork so the
/// child allocates nothing.
struct PreparedExec {
    path: CString,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    /// Owners of what `argv` and `envp` point into.
    _strings: Vec<CString>,
}

// SAFETY: the raw pointers point into `_strings`' heap buffers, which a
// move leaves where they are, and nothing writes through them.
unsafe impl Send for PreparedExec {}
// SAFETY: as above; the struct is read-only after `new`.
unsafe impl Sync for PreparedExec {}

impl PreparedExec {
    /// `binary --vtmux-check-snapshot <fd>` in this process's environment.
    fn new(binary: &Path, fd: RawFd) -> io::Result<Self> {
        let path = CString::new(binary.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let mut strings = vec![
            path.clone(),
            CString::new(CHECK_FLAG).map_err(io::Error::other)?,
            CString::new(fd.to_string()).map_err(io::Error::other)?,
        ];
        let words = strings.len();
        strings.extend(environ(None));
        let mut argv: Vec<*const libc::c_char> =
            strings[..words].iter().map(|s| s.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<*const libc::c_char> =
            strings[words..].iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());
        Ok(Self {
            path,
            argv,
            envp,
            _strings: strings,
        })
    }

    /// `execve`; returns only the error it failed with.
    fn exec(&self) -> io::Error {
        // SAFETY: `path`, `argv` and `envp` are NULL-terminated and point
        // at NUL-terminated strings that live as long as `self`.
        unsafe { libc::execve(self.path.as_ptr(), self.argv.as_ptr(), self.envp.as_ptr()) };
        io::Error::last_os_error()
    }
}

/// The dry run (live-upgrade spec, sequence step 4): run
/// `<binary> --vtmux-check-snapshot <fd>` on `snapshot` and wait for it,
/// at most `limit` ([`DryRun`], waited for here). `Ok(Err(message))` when
/// the binary refuses the snapshot: its stderr, or how it ended.
///
/// # Errors
/// The binary can't be started (not found, not executable, not an
/// executable format: the `execve` error, on every host) or waited for.
pub fn dry_run(
    binary: &Path,
    snapshot: BorrowedFd<'_>,
    limit: Duration,
) -> io::Result<Result<Checked, String>> {
    let mut run = DryRun::start(binary, snapshot, limit)?;
    loop {
        if let Some(outcome) = run.poll()? {
            return Ok(outcome);
        }
        run.nap(Duration::from_millis(10));
    }
}

/// A dry run started and not yet waited for: the server loop goes on
/// serving panes and clients while it runs (`server/upgrade.rs`) and
/// [`DryRun::poll`]s it. Dropped before it has ended, its process group
/// is killed and the child reaped.
///
/// The child leads a process group of its own, and once it has ended, or
/// at the limit, the whole group is killed: nothing it started outlives
/// it holding the snapshot or its stderr. Its stderr is read as it comes,
/// so a child that writes much, or a descendant that keeps the pipe open,
/// can't hold the wait past the child's end. The child is reaped only
/// after its group is killed (its pid, the group's id, can't have been
/// reused before), so whoever else reaps this process's children must
/// leave it to [`DryRun::poll`] ([`next_exited`]).
#[derive(Debug)]
pub struct DryRun {
    binary: std::path::PathBuf,
    child: std::process::Child,
    pid: libc::pid_t,
    err: Option<std::process::ChildStderr>,
    said: Vec<u8>,
    end: Instant,
    limit: Duration,
    /// The [`acknowledgement`] of the snapshot it was given, `None` when
    /// that is no stream (nothing passes then).
    expect: Option<String>,
    /// How it went, once it has ended and been reaped.
    outcome: Option<Result<Checked, String>>,
}

impl DryRun {
    /// Start `<binary> --vtmux-check-snapshot <fd>` on `snapshot`, to end
    /// within `limit`.
    ///
    /// # Errors
    /// The binary can't be started, as [`dry_run`].
    pub fn start(binary: &Path, snapshot: BorrowedFd<'_>, limit: Duration) -> io::Result<Self> {
        use std::os::unix::process::CommandExt as _;
        // Before the flag is cleared: an error here must not leave the
        // snapshot inheritable.
        let exec = PreparedExec::new(binary, snapshot.as_raw_fd())?;
        let expect = expected_acknowledgement(snapshot)?;
        let flags = inheritable(snapshot)?;
        let mut command = Command::new(binary);
        command
            .arg(CHECK_FLAG)
            .arg(snapshot.as_raw_fd().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // The child execs by `execve` itself, as the real exec does
        // (`exec_resume`): `Command` with a `pre_exec` hook falls back to
        // `fork` + `execvp`, and glibc's `execvp` runs a file the kernel
        // refuses (ENOEXEC) as a shell script. What that prints and exits
        // with is the shell's (bash: "cannot execute binary file", 126;
        // dash: nothing, 127), so the refusal differed by host.
        // `Command` passes the signal mask on, and a switch back runs this
        // with the handled signals blocked (`exec_resume`): the child
        // starts with none blocked, as a pane does (`pty::clear_signals`).
        // SAFETY: the closure runs in the forked child before exec and only
        // calls setpgid, sigemptyset, pthread_sigmask on a local set, and
        // `PreparedExec::exec` (async-signal-safe, allocation-free).
        unsafe {
            command.pre_exec(move || {
                libc::setpgid(0, 0);
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut set);
                libc::pthread_sigmask(libc::SIG_SETMASK, &raw const set, std::ptr::null_mut());
                Err(exec.exec())
            });
        }
        let child = command.spawn();
        fcntl_setfd(snapshot, flags)?;
        let mut child = child?;
        let pid = libc::pid_t::try_from(child.id()).map_err(io::Error::other)?;
        // Also here, so the group exists before the first kill whichever
        // side runs first (EACCES once the child has exec'd, which is
        // fine).
        // SAFETY: setpgid on our own child's pid.
        unsafe { libc::setpgid(pid, pid) };
        let err = child.stderr.take();
        if let Some(e) = &err {
            rustix::io::ioctl_fionbio(e, true)?;
        }
        Ok(Self {
            binary: binary.to_path_buf(),
            child,
            pid,
            err,
            said: Vec::new(),
            end: Instant::now() + limit,
            limit,
            expect,
            outcome: None,
        })
    }

    /// The child's pid while it is not reaped, `None` after.
    #[must_use]
    pub fn pid(&self) -> Option<i32> {
        self.outcome.is_none().then_some(self.pid)
    }

    /// When it is given up on.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.end
    }

    /// Without waiting: `None` while the child runs within its limit;
    /// once it has ended, or at the limit, its group killed and the child
    /// reaped, how it went (the same answer every call after).
    ///
    /// # Errors
    /// It can't be waited for.
    pub fn poll(&mut self) -> io::Result<Option<Result<Checked, String>>> {
        if let Some(outcome) = &self.outcome {
            return Ok(Some(outcome.clone()));
        }
        if let Some(e) = &mut self.err
            && !drain(e, &mut self.said)
        {
            self.err = None;
        }
        let ended = exited(self.pid)?;
        if !ended && Instant::now() < self.end {
            return Ok(None);
        }
        let outcome = self.finish(ended)?;
        self.outcome = Some(outcome.clone());
        Ok(Some(outcome))
    }

    /// Wait at most `nap` for its stderr to have something or close (or
    /// just `nap` once that is closed).
    pub fn nap(&self, nap: Duration) {
        let nap = nap.min(self.end.saturating_duration_since(Instant::now()));
        match &self.err {
            Some(e) => {
                let mut fds = [rustix::event::PollFd::new(e, rustix::event::PollFlags::IN)];
                let _ = rustix::event::poll(&mut fds, Some(&timespec(nap)));
            }
            None => std::thread::sleep(nap),
        }
    }

    /// Kill the group, reap the child; how it went (`ended`: by itself,
    /// not at the limit).
    fn finish(&mut self, ended: bool) -> io::Result<Result<Checked, String>> {
        self.kill();
        let status = self.child.wait()?;
        if !ended {
            return Ok(Err(format!(
                "{} {CHECK_FLAG} took over {} s",
                self.binary.display(),
                self.limit.as_secs()
            )));
        }
        if let Some(e) = &mut self.err {
            drain(e, &mut self.said);
        }
        if status.success() {
            let line = |want: &str| {
                let want = want.as_bytes();
                self.said
                    .split_inclusive(|&b| b == b'\n')
                    .any(|l| l == want)
            };
            // Only for a binary that does not acknowledge (rare: another
            // program, or a downgrade), so the loop waits on its `-V`
            // ([`VERSION_LIMIT`]) only then.
            if !self.expect.as_deref().is_some_and(line) && !legacy_vtmux(&self.binary) {
                return Ok(Err(format!(
                    "{} is not a vtmux that can take over: its {CHECK_FLAG} exited 0 \
                     without saying it read the snapshot, and its -V names no vtmux \
                     release from before that",
                    self.binary.display()
                )));
            }
            return Ok(Ok(Checked {
                takes_socket: line(TAKES_SOCKET),
                takes_raw_tty: line(TAKES_RAW_TTY),
            }));
        }
        let message = String::from_utf8_lossy(&self.said);
        let message = message.trim_end();
        Ok(Err(if message.is_empty() {
            format!("{} {CHECK_FLAG} failed: {status}", self.binary.display())
        } else {
            message.to_owned()
        }))
    }

    /// SIGKILL to the child's process group.
    fn kill(&self) {
        // Only before the child is reaped (`exited` leaves it a zombie), so
        // its pid, and the group's id, can't have been reused.
        // SAFETY: kill(2) of our child's process group.
        unsafe { libc::kill(-self.pid, libc::SIGKILL) };
    }
}

impl Drop for DryRun {
    fn drop(&mut self) {
        if self.outcome.is_none() {
            self.kill();
            let _ = self.child.wait();
        }
    }
}

/// The [`acknowledgement`] a dry run of the stream in `snapshot` must
/// answer with, read from its header; `None` when it has none. The file
/// is left at offset 0, so a dry run that reads it from its descriptor
/// (a script's `/dev/fd/<n>`, which macOS opens as a `dup`) reads it
/// whole; the images read it by offset (`read_all`).
fn expected_acknowledgement(snapshot: BorrowedFd<'_>) -> io::Result<Option<String>> {
    let mut head = [0; vtmux_snapshot::HEADER_LEN];
    let mut got = 0;
    while got < head.len() {
        match rustix::io::pread(snapshot, &mut head[got..], got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    rustix::fs::seek(snapshot, rustix::fs::SeekFrom::Start(0))?;
    Ok(vtmux_snapshot::digest(&head[..got]).map(|d| acknowledgement(&d)))
}

/// A child of this process that a signal has stopped, and the signal;
/// the stop is taken, a child that has ended is left alone (the server
/// reaps by [`next_exited`] while a [`DryRun`] is in flight). `None` when
/// none has stopped.
#[must_use]
pub fn next_stopped() -> Option<(i32, i32)> {
    // SAFETY: an all-zero `siginfo_t` is a valid value for waitid to
    // fill in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: waitid on any child with a valid out pointer.
        let r = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &raw mut info,
                libc::WSTOPPED | libc::WNOHANG,
            )
        };
        if r == 0 {
            // SAFETY: waitid filled `info` in; si_pid is 0 when no child
            // has stopped, si_status the stopping signal otherwise.
            let (pid, sig) = unsafe { (info.si_pid(), info.si_status()) };
            return (pid != 0).then_some((pid, sig));
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// A child of this process that has ended and is not reaped yet, left so
/// (`WNOWAIT`): the server reaps by it while a [`DryRun`] is in flight,
/// so it reaps every child but that one, which [`DryRun::poll`] reaps
/// after killing its group. `None` when no child has ended.
#[must_use]
pub fn next_exited() -> Option<i32> {
    // SAFETY: an all-zero `siginfo_t` is a valid value for waitid to
    // fill in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: waitid on any child with a valid out pointer.
        let r = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r == 0 {
            // SAFETY: waitid filled `info` in; si_pid is 0 when no child
            // has changed state.
            let pid = unsafe { info.si_pid() };
            return (pid != 0).then_some(pid);
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// The Ghostty patch-stack hash `binary` reports
/// (`<binary> --vtmux-patch-stack`, [`PATCH_STACK_FLAG`]), waiting at most
/// `limit`; `None` when it can't be started, exits non-zero, prints no
/// hash or takes longer (it is killed then). The answer is read once the
/// binary has exited, from what it wrote: a descendant that keeps its
/// stdout open can't hold the wait.
#[must_use]
pub fn patch_stack(binary: &Path, limit: Duration) -> Option<String> {
    let said = answer(Command::new(binary).arg(PATCH_STACK_FLAG), limit)?;
    let hash = said.trim_end_matches('\n');
    (!hash.is_empty() && hash.len() <= 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_owned())
}

/// Whether `binary` is a vtmux from before the dry run's acknowledgement
/// ([`CHECKED`]): its `-V`, run as `vtmux` within [`VERSION_LIMIT`], is
/// exactly one of [`LEGACY_VERSIONS`] and a newline.
fn legacy_vtmux(binary: &Path) -> bool {
    use std::os::unix::process::CommandExt as _;
    answer(Command::new(binary).arg0("vtmux").arg("-V"), VERSION_LIMIT).is_some_and(|said| {
        said.strip_suffix('\n')
            .is_some_and(|v| LEGACY_VERSIONS.contains(&v))
    })
}

/// What `command` writes on stdout (its first 128 bytes) if it exits 0
/// within `limit`; `None` when it can't be started, exits non-zero, writes
/// what is not UTF-8 or takes longer (it is killed then). Read once it has
/// exited: a descendant that keeps its stdout open can't hold the wait.
fn answer(command: &mut Command, limit: Duration) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let end = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(1)),
            Ok(None) | Err(_) => break None,
        }
    };
    if !status.is_some_and(|s| s.success()) {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    rustix::io::ioctl_fionbio(&out, true).ok()?;
    let mut said = [0; 128];
    let mut n = 0;
    while n < said.len() {
        match out.read(&mut said[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    std::str::from_utf8(&said[..n]).ok().map(str::to_owned)
}

/// Give the heap's free memory back to the system where the allocator
/// keeps it (glibc: `malloc_trim`), after the new image has dropped the
/// stream and its decoded records: tens of thousands of history pages'
/// copies, freed between allocations that stay, which glibc would
/// otherwise hold on to (about 110 MB at 10 x 100k lines,
/// `lu_upgrade_perf`). Elsewhere (macOS) the allocator returns it itself.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only walks the allocator's own free lists.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Read what non-blocking `pipe` has into `said` (up to
/// [`MESSAGE_LIMIT`]); whether it is still open.
fn drain(pipe: &mut std::process::ChildStderr, said: &mut Vec<u8>) -> bool {
    let mut buf = [0; 4096];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => {
                let room = MESSAGE_LIMIT.saturating_sub(said.len());
                said.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

/// Whether child `pid` has ended, leaving it unreaped (`WNOWAIT`), so its
/// process group can still be killed by its id.
fn exited(pid: libc::pid_t) -> io::Result<bool> {
    // SAFETY: an all-zero `siginfo_t` is a valid value for waitid to
    // fill in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: waitid on our own child with a valid out pointer.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                libc::id_t::try_from(pid).map_err(io::Error::other)?,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r == 0 {
            // SAFETY: waitid filled `info` in; si_pid is 0 when the child
            // has not changed state.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// The arguments of the new image: `argv0 --vtmux-resume <fd>`, and
/// [`SWITCHED_BACK_FLAG`] for a switch back; `-S <socket>` after `argv0`
/// for an image that takes it ([`TAKES_SOCKET`]).
fn resume_argv(
    argv0: &OsString,
    fd: RawFd,
    back: bool,
    socket: Option<&Path>,
) -> io::Result<Vec<CString>> {
    let mut words = vec![argv0.as_bytes().to_vec()];
    if let Some(socket) = socket {
        words.push(b"-S".to_vec());
        words.push(socket.as_os_str().as_bytes().to_vec());
    }
    words.push(RESUME_FLAG.into());
    words.push(fd.to_string().into_bytes());
    if back {
        words.push(SWITCHED_BACK_FLAG.into());
    }
    words
        .into_iter()
        .map(|w| CString::new(w).map_err(io::Error::other))
        .collect()
}

/// Replace this process with `binary` (decision 2: same pid, so the
/// panes stay its children): `execve(binary, [binary, "--vtmux-resume",
/// <snapshot>], environ)` with `snapshot` and every descriptor of `keep`
/// left open across it (close-on-exec cleared). `argv0` is `binary`, as
/// given (the path the server resolved for the exec), so `ps` names the
/// image that runs, not the one before it, and `socket`, given for a binary
/// whose dry run said it takes it, goes before the flag as `-S <socket>`,
/// so `ps` still tells which server it is. The signals the server handles
/// are blocked across it (`signal::block_handled`): the new image unblocks
/// them once it has handlers, and one that comes meanwhile waits instead
/// of killing the process. Returns only on failure, with every
/// descriptor's close-on-exec flag and the signal mask as they were;
/// `snapshot` is closed.
#[must_use]
pub fn exec_in_place(
    binary: &Path,
    snapshot: OwnedFd,
    keep: &[RawFd],
    socket: Option<&Path>,
) -> io::Error {
    let e = exec_resume(Target::Path(binary), snapshot.as_fd(), keep, None, socket);
    drop(snapshot);
    e
}

/// [`exec_in_place`] for the new image that failed to resume: back to the
/// image before on the same `snapshot`, which stays open either way,
/// marked [`SWITCHED_BACK_FLAG`]. `exe` is that image's executable, kept
/// open from its start ([`own_exe`]): it is exec'd whatever its path
/// holds by now (a relinked build, a binary from before the feature).
/// Without it, or when it can't be exec'd, `path` is, but only once its
/// dry run on `snapshot` passes. `why` (the resume's failure) reaches
/// that image in [`FAILED_ENV`]. `socket` is the `-S` this image was
/// started with: the image that wrote the snapshot gave it, so takes it.
#[must_use]
pub fn switch_back(
    exe: Option<BorrowedFd<'_>>,
    path: Option<&Path>,
    snapshot: BorrowedFd<'_>,
    keep: &[RawFd],
    why: &str,
    socket: Option<&Path>,
) -> io::Error {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let e = match exe {
        Some(fd) => exec_resume(Target::Fd(fd, path), snapshot, keep, Some(why), socket),
        None => io::Error::other("no image to switch back to"),
    };
    // No `fexecve` here, so no executable is kept open (`own_exe`).
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let e = {
        let _ = exe;
        io::Error::other("no image to switch back to")
    };
    let Some(path) = path else { return e };
    match dry_run(path, snapshot, SWITCH_BACK_LIMIT) {
        Ok(Ok(c)) => exec_resume(
            Target::Path(path),
            snapshot,
            keep,
            Some(why),
            socket.filter(|_| c.takes_socket),
        ),
        Ok(Err(message)) => io::Error::other(message),
        Err(e) => e,
    }
}

/// What [`exec_resume`] runs.
enum Target<'a> {
    Path(&'a Path),
    /// An open executable (`fexecve`, which macOS lacks), and the path it
    /// was started as, when known (for `argv[0]`).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Fd(BorrowedFd<'a>, Option<&'a Path>),
}

/// This image's executable, open for a switch back to it from the image
/// it upgrades to ([`switch_back`]): `/proc/self/exe` on Linux names the
/// running file even after it was replaced or removed. Close-on-exec: an
/// upgrade keeps it open by listing it ([`exec_in_place`]'s `keep`).
/// `None` where `fexecve` is missing (macOS), whose switch back goes by
/// path.
#[must_use]
pub fn own_exe() -> Option<OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return None;
    }
    rustix::fs::open(
        "/proc/self/exe",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
}

/// This process's environment as `execve` takes it, with [`FAILED_ENV`]
/// set to `note` (and only then).
fn environ(note: Option<&str>) -> Vec<CString> {
    std::env::vars_os()
        .filter(|(k, _)| k != FAILED_ENV)
        .chain(note.map(|n| (FAILED_ENV.into(), n.replace('\0', " ").into())))
        .filter_map(|(k, v)| {
            let mut kv = k.into_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            CString::new(kv).ok()
        })
        .collect()
}

/// [`FAILED_ENV`], taken out of the environment so that nothing this
/// image starts or execs inherits it: why the image that switched back
/// to this one failed to resume.
#[must_use]
pub fn take_failure_note() -> Option<String> {
    take_env(FAILED_ENV).map(|note| note.to_string_lossy().into_owned())
}

/// Environment variable `name`, taken out of the environment so that
/// nothing this process starts or execs inherits it. Call it only at the
/// start of `main`, before any thread is started.
#[must_use]
pub fn take_env(name: &str) -> Option<std::ffi::OsString> {
    let value = std::env::var_os(name)?;
    // SAFETY: called by the process at its start, before it has started
    // a thread, so nothing reads the environment concurrently.
    unsafe { std::env::remove_var(name) };
    Some(value)
}

/// `execve` of `target` resuming from `snapshot`; `back` is a switch back
/// (`Some(why)`).
fn exec_resume(
    target: Target<'_>,
    snapshot: BorrowedFd<'_>,
    keep: &[RawFd],
    back: Option<&str>,
    socket: Option<&Path>,
) -> io::Error {
    // `ps`, `pkill -f` and plugins read argv[0] as the program the
    // process runs, so it is the path exec'd, not this image's own.
    let argv0 = match target {
        Target::Path(p) => p.as_os_str().to_owned(),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Target::Fd(_, Some(p)) => p.as_os_str().to_owned(),
        // The image's path is not known: keep this process's own.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Target::Fd(_, None) => std::env::args_os().next().unwrap_or_else(|| "vtmux".into()),
    };
    let argv = match resume_argv(&argv0, snapshot.as_raw_fd(), back.is_some(), socket) {
        Ok(argv) => argv,
        Err(e) => return e,
    };
    let path = match target {
        Target::Path(p) => match CString::new(p.as_os_str().as_bytes().to_vec()) {
            Ok(path) => Some(path),
            Err(e) => return io::Error::other(e),
        },
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Target::Fd(..) => None,
    };
    let env = environ(back);
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|a| a.as_ptr()).collect();
    envp.push(std::ptr::null());

    let mut restore = Vec::with_capacity(keep.len() + 1);
    for fd in std::iter::once(snapshot.as_raw_fd()).chain(keep.iter().copied()) {
        // SAFETY: the caller's descriptors are open for the duration of
        // this call (the snapshot's table lists the server's own); a
        // borrow only reads and sets the descriptor flags.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        match inheritable(fd) {
            Ok(flags) => restore.push((fd, flags)),
            Err(e) => {
                put_back(&restore);
                return e;
            }
        }
    }
    let mask = super::signal::block_handled();
    match (&path, target) {
        // SAFETY: `path`, every `argv` word and every `envp` entry are
        // NUL-terminated strings that outlive the call, and `ptrs` and
        // `envp` are NULL-terminated. It returns only on failure.
        (Some(path), _) => unsafe {
            libc::execve(path.as_ptr(), ptrs.as_ptr(), envp.as_ptr());
        },
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: as above; `fd` is an open descriptor borrowed for the
        // call.
        (None, Target::Fd(fd, _)) => unsafe {
            libc::fexecve(fd.as_raw_fd(), ptrs.as_ptr(), envp.as_ptr());
        },
        _ => {
            super::signal::restore_mask(mask);
            put_back(&restore);
            return io::Error::from(io::ErrorKind::Unsupported);
        }
    }
    let e = io::Error::last_os_error();
    super::signal::restore_mask(mask);
    put_back(&restore);
    e
}

fn put_back(restore: &[(BorrowedFd<'_>, FdFlags)]) {
    for &(fd, flags) in restore {
        let _ = fcntl_setfd(fd, flags);
    }
}

/// Every descriptor from 3 up that is open and not taken by [`inherited`]
/// yet, taken now: what an image that can't read its snapshot holds on
/// to (the last resort), so no pane's PTY closes. Call it only where
/// [`inherited`] may be called.
#[must_use]
pub fn inherit_rest() -> Vec<OwnedFd> {
    let limit = super::pty::fd_limit();
    (3..limit)
        .filter(|&fd| is_open(fd))
        .filter_map(|fd| inherited(fd).ok())
        .collect()
}

/// Whether descriptor `fd` is open.
#[must_use]
pub fn is_open(fd: RawFd) -> bool {
    // SAFETY: `fcntl(F_GETFD)` on any number only reports whether it is
    // open.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// The listening socket among `fds` and the path it is bound to: a Unix
/// socket that is listening, bound to a path. Linux reports listening
/// (`SO_ACCEPTCONN`); macOS lacks that, but only the socket that was bound
/// can `listen` (again: it only resets the backlog), an accepted or
/// connected one can't (XNU `uipc_listen`).
#[must_use]
pub fn find_listener(fds: &[OwnedFd]) -> Option<(usize, std::path::PathBuf)> {
    fds.iter().enumerate().find_map(|(i, fd)| {
        #[cfg(not(target_vendor = "apple"))]
        let listening = rustix::net::sockopt::socket_acceptconn(fd).unwrap_or(false);
        #[cfg(target_vendor = "apple")]
        let listening = rustix::net::listen(fd, libc::SOMAXCONN).is_ok();
        if !listening {
            return None;
        }
        let addr = rustix::net::getsockname(fd).ok()?;
        let unix = rustix::net::SocketAddrUnix::try_from(addr).ok()?;
        let path = unix.path_bytes()?;
        Some((i, OsString::from_vec(path.to_vec()).into()))
    })
}

/// The path `own_exe` names, without the ` (deleted)` Linux adds to
/// `/proc/self/exe` once the file was replaced: where a switch back looks
/// for the image before.
#[must_use]
pub fn exe_path(exe: &Path) -> std::path::PathBuf {
    let bytes = exe.as_os_str().as_bytes();
    let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    OsString::from_vec(bytes.to_vec()).into()
}

/// The parent of process `pid`, `None` when it can't be told (gone, or
/// a host with neither way below).
#[must_use]
pub fn parent_pid(pid: i32) -> Option<i32> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: an all-zero `proc_bsdinfo` is a valid value for
        // proc_pidinfo to fill in.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: the buffer is `size` bytes of a `proc_bsdinfo`, what
        // PROC_PIDTBSDINFO writes.
        let n = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (n == size).then(|| i32::try_from(info.pbi_ppid).ok())?
    }
    #[cfg(not(target_os = "macos"))]
    {
        // `pid (comm) state ppid ...`; comm may hold spaces and parens.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(1)?.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile_lite::Dir {
        tempfile_lite::Dir::new()
    }

    /// A private directory removed on drop (the crate has no `tempfile`).
    mod tempfile_lite {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p =
                    std::env::temp_dir().join(format!("vtmux-upgrade-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn a_memfile_reads_back_whole_from_any_offset_and_leaves_nothing() {
        let d = dir();
        let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let fd = memfile(&d.0, &bytes).unwrap();
        assert_eq!(read_all(fd.as_fd()).unwrap(), bytes);
        assert!(fcntl_getfd(&fd).unwrap().contains(FdFlags::CLOEXEC));
        assert_eq!(std::fs::read_dir(&d.0).unwrap().count(), 0);
    }

    #[test]
    fn the_unlinked_file_of_macos_is_private_and_gone_from_the_directory() {
        use std::os::unix::fs::MetadataExt as _;
        let d = dir();
        let fd = unlinked_file(&d.0).unwrap();
        let file = std::fs::File::from(fd);
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(file.metadata().unwrap().nlink(), 0);
        assert_eq!(std::fs::read_dir(&d.0).unwrap().count(), 0);
        assert!(fcntl_getfd(&file).unwrap().contains(FdFlags::CLOEXEC));
    }

    /// An executable `sh` script in `d`.
    fn script(d: &tempfile_lite::Dir, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = d.0.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A dry run passes only with the acknowledgement of the snapshot it
    /// was given, a line of its own: exit 0 alone, `TAKES_SOCKET` alone,
    /// another stream's digest or the line run into other output are
    /// refused, unless the binary's `-V` (run as `vtmux`) is exactly a
    /// release from before the acknowledgement. The snapshot is left at
    /// offset 0 for a script that reads its descriptor.
    #[test]
    fn a_dry_run_passes_only_with_the_snapshots_acknowledgement() {
        let d = dir();
        let stream = vtmux_snapshot::encode(&vtmux_snapshot::Snapshot::default());
        let other = vtmux_snapshot::encode(&vtmux_snapshot::Snapshot {
            sessions: vec![vtmux_snapshot::Session::default()],
            ..vtmux_snapshot::Snapshot::default()
        });
        let snapshot = memfile(&d.0, &stream).unwrap();
        let ack = acknowledgement(&vtmux_snapshot::digest(&stream).unwrap());
        let wrong = acknowledgement(&vtmux_snapshot::digest(&other).unwrap());
        assert_ne!(ack, wrong);
        assert_eq!(ack.len(), CHECKED.len() + 64 + 1);
        let says = |name: &str, what: &str| {
            let path = d.0.join(format!("{name}.txt"));
            std::fs::write(&path, what).unwrap();
            script(&d, name, &format!("cat '{}' >&2", path.display()))
        };
        let refused = [
            script(&d, "zero", "exit 0"),
            says("old", TAKES_SOCKET),
            says("wrong", &wrong),
            says("run-in", &format!("x{ack}")),
            says("unended", ack.trim_end()),
        ];
        for binary in &refused {
            let message = dry_run(binary, snapshot.as_fd(), DRY_RUN_LIMIT)
                .unwrap()
                .unwrap_err();
            assert!(
                message.ends_with("names no vtmux release from before that"),
                "{}: {message}",
                binary.display()
            );
        }
        let passes = says("new", &format!("{TAKES_SOCKET}{TAKES_RAW_TTY}{ack}"));
        assert_eq!(
            dry_run(&passes, snapshot.as_fd(), DRY_RUN_LIMIT).unwrap(),
            Ok(Checked {
                takes_socket: true,
                takes_raw_tty: true
            })
        );
        let cooked = says("cooked", &format!("{TAKES_SOCKET}{ack}"));
        assert_eq!(
            dry_run(&cooked, snapshot.as_fd(), DRY_RUN_LIMIT).unwrap(),
            Ok(Checked {
                takes_socket: true,
                takes_raw_tty: false
            })
        );
        let no_socket = says("bare", &ack);
        assert_eq!(
            dry_run(&no_socket, snapshot.as_fd(), DRY_RUN_LIMIT).unwrap(),
            Ok(Checked::default())
        );
        // A vtmux from before the acknowledgement: its `-V` says which;
        // nothing else will do. (Run as `vtmux`, which a script, given its
        // path as `$0`, can't see; the real 3.8-beta2 named `tmux` was
        // checked by hand.)
        let legacy = |name: &str, version: &str| {
            script(
                &d,
                name,
                &format!(
                    "case \"$1\" in\n\
                     -V) echo '{version}'; exit 0;;\n\
                     --vtmux-check-snapshot) printf '{}' >&2; exit 0;;\n\
                     esac\nexit 1",
                    TAKES_SOCKET.trim_end().to_owned() + "\\n"
                ),
            )
        };
        for (name, version, passes) in [
            ("tmux", "vtmux 3.8-beta2", true),
            ("beta1", "vtmux 3.8-beta1", true),
            ("beta3", "vtmux 3.8-beta3", false),
            ("spaced", "vtmux 3.8-beta2 ", false),
            ("bare", "3.8-beta2", false),
            ("tmux-like", "tmux 3.8", false),
        ] {
            let got = dry_run(&legacy(name, version), snapshot.as_fd(), DRY_RUN_LIMIT).unwrap();
            if passes {
                assert_eq!(
                    got,
                    Ok(Checked {
                        takes_socket: true,
                        takes_raw_tty: false
                    }),
                    "{name}"
                );
            } else {
                assert!(got.is_err(), "{name}: {got:?}");
            }
        }
        // Read through its descriptor, as a script does: the whole stream.
        let reads = script(
            &d,
            "reads",
            "cat /dev/fd/$2 | wc -c | tr -d ' ' >&2; exit 1",
        );
        assert_eq!(
            dry_run(&reads, snapshot.as_fd(), DRY_RUN_LIMIT).unwrap(),
            Err(stream.len().to_string())
        );
    }

    /// Decision 23: the hash a binary prints for `--vtmux-patch-stack`,
    /// and nothing for one that refuses the flag (a binary from before
    /// it), prints no hash, can't run, or does not answer within the limit
    /// (killed then, the wait bounded); a descendant holding its stdout
    /// does not hold the answer.
    #[test]
    fn a_binary_tells_its_patch_stack_or_nothing() {
        let d = dir();
        let says = script(
            &d,
            "says",
            "[ \"$1\" = --vtmux-patch-stack ] && echo 82eda04cc524487c",
        );
        assert_eq!(
            patch_stack(&says, PATCH_STACK_LIMIT).as_deref(),
            Some("82eda04cc524487c")
        );
        let holds = script(&d, "holds", "sleep 5 &\necho 0123abcd");
        assert_eq!(
            patch_stack(&holds, PATCH_STACK_LIMIT).as_deref(),
            Some("0123abcd")
        );
        let old = script(&d, "old", "echo 'unknown option -- -' >&2; exit 1");
        assert_eq!(patch_stack(&old, PATCH_STACK_LIMIT), None);
        let words = script(&d, "words", "echo usage: vtmux");
        assert_eq!(patch_stack(&words, PATCH_STACK_LIMIT), None);
        let garbage = d.0.join("garbage");
        std::fs::write(&garbage, b"\x7fnot a program").unwrap();
        assert_eq!(patch_stack(&garbage, PATCH_STACK_LIMIT), None);
        assert_eq!(patch_stack(&d.0.join("missing"), PATCH_STACK_LIMIT), None);
        let hangs = script(&d, "hangs", "exec sleep 30");
        let start = Instant::now();
        assert_eq!(patch_stack(&hangs, Duration::from_millis(200)), None);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_resume_arguments_name_the_descriptor_and_a_switch_back() {
        let argv0 = OsString::from("tmux");
        let words = |back, socket: Option<&str>| {
            resume_argv(&argv0, 7, back, socket.map(Path::new))
                .unwrap()
                .into_iter()
                .map(|c| c.into_string().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(words(false, None), ["tmux", RESUME_FLAG, "7"]);
        assert_eq!(
            words(true, None),
            ["tmux", RESUME_FLAG, "7", SWITCHED_BACK_FLAG]
        );
        assert_eq!(
            words(false, Some("/tmp/vto-1/s")),
            ["tmux", "-S", "/tmp/vto-1/s", RESUME_FLAG, "7"]
        );
    }

    /// A dry run that fails to start (a NUL in the path) returns before
    /// anything is spawned, and the snapshot is as close-on-exec as it was.
    #[test]
    fn a_dry_run_that_cannot_start_leaves_the_snapshot_close_on_exec() {
        let d = dir();
        let snapshot = memfile(&d.0, b"x").unwrap();
        let bad = Path::new("no\0such");
        assert!(DryRun::start(bad, snapshot.as_fd(), DRY_RUN_LIMIT).is_err());
        assert!(fcntl_getfd(&snapshot).unwrap().contains(FdFlags::CLOEXEC));
    }

    /// A nap of a second or more polls for all of it.
    #[test]
    fn a_nap_keeps_its_whole_seconds() {
        let t = timespec(Duration::from_millis(2500));
        assert_eq!((t.tv_sec, t.tv_nsec), (2, 500_000_000));
        let t = timespec(Duration::from_millis(10));
        assert_eq!((t.tv_sec, t.tv_nsec), (0, 10_000_000));
    }

    #[test]
    fn a_failed_exec_leaves_every_flag_as_it_was() {
        let d = dir();
        let snapshot = memfile(&d.0, b"x").unwrap();
        let kept = memfile(&d.0, b"y").unwrap();
        let missing = d.0.join("no-such-binary");
        let e = exec_in_place(&missing, snapshot, &[kept.as_raw_fd()], None);
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(fcntl_getfd(&kept).unwrap().contains(FdFlags::CLOEXEC));
        // The handled signals were blocked across the attempt and are not
        // any more.
        assert!(!super::super::signal::blocked(libc::SIGHUP));
    }

    /// A dry run started while the handled signals are blocked (the new
    /// image's switch back) runs with none blocked: `Command` clears the
    /// mask in the child. The script would survive its own SIGHUP and
    /// exit 0 if the mask carried over.
    #[test]
    fn a_dry_run_starts_with_no_signal_blocked() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = dir();
        let script = d.0.join("hup");
        std::fs::write(&script, "#!/bin/sh\nkill -HUP $$\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let snapshot = memfile(&d.0, b"x").unwrap();
        let mask = super::super::signal::block_handled();
        let end = Instant::now() + Duration::from_secs(20);
        let got = loop {
            match dry_run(&script, snapshot.as_fd(), DRY_RUN_LIMIT) {
                // Another test thread's fork can hold the script open for
                // writing a moment (ETXTBSY).
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => break other,
            }
        };
        super::super::signal::restore_mask(mask);
        let message = got.unwrap().unwrap_err();
        assert!(message.contains("signal: 1"), "{message}");
    }

    /// A file the kernel will not exec (no `#!`, not an executable
    /// format) fails the spawn with the `execve` error: never the shell
    /// fallback of `execvp`, whose exit code and message differ by host
    /// (bash 126 with a message, dash 127 and none).
    #[test]
    fn a_dry_run_of_a_file_that_is_no_executable_is_an_exec_error() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = dir();
        let garbage = d.0.join("garbage");
        std::fs::write(&garbage, [0x7f, 0x00, 0xff, 0x13, 0x37, 0x00]).unwrap();
        std::fs::set_permissions(&garbage, std::fs::Permissions::from_mode(0o755)).unwrap();
        let snapshot = memfile(&d.0, b"x").unwrap();
        let e = dry_run(&garbage, snapshot.as_fd(), DRY_RUN_LIMIT).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOEXEC), "{e}");
    }

    #[test]
    fn the_parent_of_this_process_is_known() {
        let me = i32::try_from(std::process::id()).unwrap();
        let parent = i32::try_from(std::os::unix::process::parent_id()).unwrap();
        assert_eq!(parent_pid(me), Some(parent));
        assert_eq!(parent_pid(i32::MAX), None);
    }

    #[test]
    fn standard_descriptors_and_closed_ones_are_not_inherited() {
        assert!(inherited(0).is_err());
        assert!(inherited(2).is_err());
        assert!(inherited(1_000_000).is_err());
    }

    #[test]
    fn a_deleted_executable_is_found_by_its_name() {
        assert_eq!(
            exe_path(Path::new("/x/vtmux (deleted)")),
            Path::new("/x/vtmux")
        );
        assert_eq!(exe_path(Path::new("/x/vtmux")), Path::new("/x/vtmux"));
    }
}
