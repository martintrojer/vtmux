//! The system side of a live upgrade (D27, docs/specs/2026-11-06-live-upgrade.md,
//! *Live upgrade*): the anonymous memory file the snapshot travels in
//! (decision 19), the dry run (decision 7), `execve` in place keeping the
//! descriptors the snapshot names (decision 2), and taking them back by
//! number in the new image.
//!
//! The new image is `<binary> --vtmux-resume <fd>` ([`RESUME_FLAG`]); a
//! switch back after a failed resume adds [`SWITCHED_BACK_FLAG`], so the
//! image it lands in does not switch again. The dry run is
//! `<binary> --vtmux-check-snapshot <fd>` ([`CHECK_FLAG`]).

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
use vtmux_snapshot::Role;

/// The new image's flag: resume from the snapshot in the descriptor that
/// follows.
pub const RESUME_FLAG: &str = "--vtmux-resume";

/// After [`RESUME_FLAG`] `<fd>`: this image is the one a failed resume
/// switched back to; if it fails too it must not switch again.
pub const SWITCHED_BACK_FLAG: &str = "--vtmux-switched-back";

/// The dry run's flag: decode and verify the snapshot in the descriptor
/// that follows, exit 0 or non-zero with a message on stderr.
pub const CHECK_FLAG: &str = "--vtmux-check-snapshot";

/// How long the dry run may take before it counts as failed.
const DRY_RUN_LIMIT: Duration = Duration::from_secs(30);

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

/// `fd`'s close-on-exec cleared; the previous flags, to put back.
fn inheritable(fd: BorrowedFd<'_>) -> io::Result<FdFlags> {
    let flags = fcntl_getfd(fd)?;
    fcntl_setfd(fd, flags - FdFlags::CLOEXEC)?;
    Ok(flags)
}

/// The dry run (live-upgrade spec, sequence step 4): run
/// `<binary> --vtmux-check-snapshot <fd>` on `snapshot` and wait for it,
/// at most [`DRY_RUN_LIMIT`]. `Ok(Err(message))` when the binary refuses
/// the snapshot: its stderr, or how it ended.
///
/// # Errors
/// The binary can't be started or waited for.
pub fn dry_run(binary: &Path, snapshot: BorrowedFd<'_>) -> io::Result<Result<(), String>> {
    let flags = inheritable(snapshot)?;
    let child = Command::new(binary)
        .arg(CHECK_FLAG)
        .arg(snapshot.as_raw_fd().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    fcntl_setfd(snapshot, flags)?;
    let mut child = child?;
    let end = Instant::now() + DRY_RUN_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Err(format!(
                "{} {CHECK_FLAG} took over {} s",
                binary.display(),
                DRY_RUN_LIMIT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    if status.success() {
        return Ok(Ok(()));
    }
    let mut message = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut message);
    }
    let message = message.trim_end();
    Ok(Err(if message.is_empty() {
        format!("{} {CHECK_FLAG} failed: {status}", binary.display())
    } else {
        message.to_owned()
    }))
}

/// The arguments of the new image: `argv0 --vtmux-resume <fd>`, and
/// [`SWITCHED_BACK_FLAG`] for a switch back.
fn resume_argv(argv0: &OsString, fd: RawFd, back: bool) -> io::Result<Vec<CString>> {
    let mut words = vec![
        argv0.as_bytes().to_vec(),
        RESUME_FLAG.into(),
        fd.to_string().into_bytes(),
    ];
    if back {
        words.push(SWITCHED_BACK_FLAG.into());
    }
    words
        .into_iter()
        .map(|w| CString::new(w).map_err(io::Error::other))
        .collect()
}

/// Replace this process with `binary` (decision 2: same pid, so the
/// panes stay its children): `execve(binary, [argv0, "--vtmux-resume",
/// <snapshot>], environ)` with `snapshot` and every descriptor of `keep`
/// left open across it (close-on-exec cleared). `argv0` is this process's
/// own, so `ps` still names it as before. Returns only on failure, with
/// every descriptor's close-on-exec flag as it was; `snapshot` is closed.
#[must_use]
pub fn exec_in_place(binary: &Path, snapshot: OwnedFd, keep: &[(Role, RawFd)]) -> io::Error {
    let e = exec_resume(binary, snapshot.as_fd(), keep, false);
    drop(snapshot);
    e
}

/// [`exec_in_place`] for the new image that failed to resume: back to
/// `binary` (the image before) on the same `snapshot`, which stays open
/// either way, marked [`SWITCHED_BACK_FLAG`].
#[must_use]
pub fn switch_back(binary: &Path, snapshot: BorrowedFd<'_>, keep: &[(Role, RawFd)]) -> io::Error {
    exec_resume(binary, snapshot, keep, true)
}

fn exec_resume(
    binary: &Path,
    snapshot: BorrowedFd<'_>,
    keep: &[(Role, RawFd)],
    back: bool,
) -> io::Error {
    let argv0 = std::env::args_os()
        .next()
        .unwrap_or_else(|| binary.as_os_str().to_owned());
    let (path, argv) = match (
        CString::new(binary.as_os_str().as_bytes().to_vec()),
        resume_argv(&argv0, snapshot.as_raw_fd(), back),
    ) {
        (Ok(path), Ok(argv)) => (path, argv),
        (Err(e), _) => return io::Error::other(e),
        (_, Err(e)) => return e,
    };
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    let mut restore = Vec::with_capacity(keep.len() + 1);
    let fds = std::iter::once(snapshot.as_raw_fd()).chain(keep.iter().map(|&(_, fd)| fd));
    for fd in fds {
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
    // SAFETY: `path` and every `argv` word are NUL-terminated strings that
    // outlive the call, `ptrs` is NULL-terminated, and `execv` passes this
    // process's `environ` on. It returns only on failure.
    unsafe { libc::execv(path.as_ptr(), ptrs.as_ptr()) };
    let e = io::Error::last_os_error();
    put_back(&restore);
    e
}

fn put_back(restore: &[(BorrowedFd<'_>, FdFlags)]) {
    for &(fd, flags) in restore {
        let _ = fcntl_setfd(fd, flags);
    }
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

    #[test]
    fn the_resume_arguments_name_the_descriptor_and_a_switch_back() {
        let argv0 = OsString::from("tmux");
        let words = |back| {
            resume_argv(&argv0, 7, back)
                .unwrap()
                .into_iter()
                .map(|c| c.into_string().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(words(false), ["tmux", RESUME_FLAG, "7"]);
        assert_eq!(words(true), ["tmux", RESUME_FLAG, "7", SWITCHED_BACK_FLAG]);
    }

    #[test]
    fn a_failed_exec_leaves_every_flag_as_it_was() {
        let d = dir();
        let snapshot = memfile(&d.0, b"x").unwrap();
        let kept = memfile(&d.0, b"y").unwrap();
        let missing = d.0.join("no-such-binary");
        let e = exec_in_place(
            &missing,
            snapshot,
            &[(Role::Listener(()), kept.as_raw_fd())],
        );
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(fcntl_getfd(&kept).unwrap().contains(FdFlags::CLOEXEC));
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
