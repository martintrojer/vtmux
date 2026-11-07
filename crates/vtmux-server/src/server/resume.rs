//! Both ends of the `execve` in a live upgrade (D27,
//! docs/specs/2026-11-06-live-upgrade.md, *Live upgrade*):
//!
//! - [`Server::exec_upgrade`], the old image: the snapshot into an
//!   anonymous memory file (sequence step 3), the dry run (step 4), the
//!   descriptors it names kept open (step 5) and `execve` (step 7), with
//!   the signals the server handles blocked across it.
//! - [`resume`], the new image (`--vtmux-resume <fd>`): decode the
//!   snapshot, take the descriptors back by number, rebuild the server,
//!   reap any pane that exited during the exec, let the signals in and
//!   run. A resume that fails (an error or a panic) switches back to the
//!   image that wrote the snapshot (decision 7), by the executable it kept
//!   open, not by path; if that fails too, the server is rebuilt without
//!   pane terminal state, and failing that it serves with no session while
//!   holding every descriptor open rather than exit: panes never die
//!   because of an upgrade. Whichever image ends up serving tells the
//!   requester the snapshot names how it went ([`answer`]).
//! - [`check_snapshot`], the dry run (`--vtmux-check-snapshot <fd>`): the
//!   decode and the whole rebuild [`resume`] does, on placeholder
//!   descriptors.
//!
//! The preflight and quiesce (steps 1-2) are `upgrade.rs`'s. Not here
//! yet: telling attached clients to re-execute (step 6). An attached client of the
//! same build stays connected through the exec, since its socket is kept;
//! the server redraws it from scratch. Pane terminal state is not carried
//! yet (`snapshot.rs`, `restore_term`): panes come back with blank
//! terminals of their size, their PTY, process and pid kept.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener as StdListener;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use mio::unix::SourceFd;
use mio::{Interest, Token};
use vtmux_snapshot as snap;

use super::{Server, ServerConfig};
use crate::sys::upgrade as sys;

/// A test hook (live-upgrade spec, *Tests*, failure paths), read from the
/// server's environment:
///
/// - `1`: the first restore fails in the image an upgrade execs (not in
///   the one it switches back to);
/// - `always`: the first restore fails in both, so the last resort runs;
/// - `panic`: the first restore panics in the image an upgrade execs;
/// - `check`: the rebuild fails in the dry run, and only there.
const FAIL_HOOK: &str = "VTMUX_TEST_RESUME_FAIL";

fn hook() -> Option<String> {
    std::env::var(FAIL_HOOK).ok()
}

/// What this binary reads beyond [`snap::KNOWN_FEATURES`].
fn features() -> snap::Features {
    snap::Features::default()
}

/// A stream decoded and checked as [`Server::from_snapshot`] would check
/// it.
fn decode(bytes: &[u8]) -> Result<snap::Snapshot, String> {
    let s = snap::decode_for(bytes, &features(), |_| None).map_err(|e| e.to_string())?;
    super::snapshot::validate(&s).map_err(|e| e.to_string())?;
    Ok(s)
}

/// The dry run (sequence step 4): the snapshot in inherited descriptor
/// `fd` decodes fully and rebuilds into a server, on placeholders for the
/// descriptors it names ([`check_bytes`]). No descriptor it names is
/// touched, and nothing is left behind.
///
/// # Errors
/// Why the snapshot can't be resumed from, as a message for the
/// requester.
pub fn check_snapshot(fd: RawFd) -> Result<(), String> {
    // A panic is reported as the message, not printed on the stderr the
    // message is read from.
    std::panic::set_hook(Box::new(|_| {}));
    let fd = sys::inherited(fd).map_err(|e| format!("snapshot descriptor {fd}: {e}"))?;
    let bytes = sys::read_all(fd.as_fd()).map_err(|e| format!("reading the snapshot: {e}"))?;
    check_bytes(&bytes)
}

/// [`check_snapshot`] on the stream's bytes: decode, then the rebuild
/// [`resume`] runs, every descriptor the snapshot names replaced by a
/// fresh socket of this process's own.
fn check_bytes(bytes: &[u8]) -> Result<(), String> {
    let s = decode(bytes)?;
    if matches!(
        s.meta.purpose,
        snap::Purpose::CheckpointLayout | snap::Purpose::CheckpointFull
    ) {
        // A `vtmux-persist` checkpoint a starting server is about to
        // restore (`super::restore`): it names no descriptor.
        return guarded(|| {
            if hook().as_deref() == Some("check") {
                return Err(io::Error::other("test hook: the rebuild fails"));
            }
            super::restore::check(&s)
        })
        .map(drop);
    }
    let kept = s
        .descriptors
        .iter()
        .map(|d| {
            let (ours, _) = std::os::unix::net::UnixStream::pair()?;
            Ok((d.role.clone(), OwnedFd::from(ours)))
        })
        .collect::<io::Result<Vec<_>>>()
        .map_err(|e| format!("placeholder descriptors: {e}"))?;
    guarded(|| {
        if hook().as_deref() == Some("check") {
            return Err(io::Error::other("test hook: the rebuild fails"));
        }
        restore(&s, &kept, true)
    })
    .map(drop)
}

/// `rebuild()`, a panic in it turned into an error: a failed rebuild in
/// the new image leads to the switch back, not to an exit that would close
/// every pane's PTY.
pub(super) fn guarded(rebuild: impl FnOnce() -> io::Result<Server>) -> Result<Server, String> {
    match catch_unwind(AssertUnwindSafe(rebuild)) {
        Ok(Ok(server)) => Ok(server),
        Ok(Err(e)) => Err(e.to_string()),
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            Err(format!("rebuilding the server panicked: {what}"))
        }
    }
}

/// The header record of a stream, read alone, for a switch back when the
/// rest does not decode.
fn writer_meta(bytes: &[u8]) -> Option<snap::Meta> {
    let body = bytes.get(snap::HEADER_LEN..)?;
    if snap::version(bytes).ok()? > snap::FORMAT_VERSION {
        return None;
    }
    let cx = snap::wire::Ctx::for_input(snap::version(bytes).ok()?, body.len());
    let mut r = snap::wire::Reader::new(body);
    while let Ok(Some((tag, field))) = r.field() {
        if tag == 1 {
            return <snap::Meta as snap::wire::Value>::get(field, &cx).ok();
        }
    }
    None
}

/// The binary a snapshot was written by, when its path is absolute.
fn writer_binary(meta: &snap::Meta) -> Option<PathBuf> {
    Some(PathBuf::from(&meta.binary)).filter(|p| p.is_absolute())
}

/// The socket was the default `-L` one (`ServerConfig::default_socket`),
/// read from the group bits it was bound with (`bind_socket`).
fn default_socket(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o060 != 0)
}

/// The server `s` describes, on duplicates of the descriptors in `kept`
/// (so a failure closes nothing a switch back needs). A `dry_run` builds
/// it only to drop it ([`ServerConfig::dry_run`]) and reaps nothing.
fn restore(
    s: &snap::Snapshot,
    kept: &[(snap::Role, OwnedFd)],
    dry_run: bool,
) -> io::Result<Server> {
    let dup = |role: &snap::Role| {
        kept.iter()
            .find(|(r, _)| r == role)
            .and_then(|(_, fd)| fd.try_clone().ok())
    };
    let listener = StdListener::from(
        dup(&snap::Role::Listener(()))
            .ok_or_else(|| io::Error::other("no listening socket kept"))?,
    );
    listener.set_nonblocking(true)?;
    let environ: Vec<(String, String)> = s
        .server
        .environment
        .iter()
        .filter_map(|v| Some((v.name.clone(), v.value.clone()?)))
        .collect();
    let config = ServerConfig {
        config_files: s.server.config_files.iter().map(PathBuf::from).collect(),
        config_quiet: true,
        cwd: environ
            .iter()
            .find(|(k, _)| k == "PWD")
            .map(|(_, v)| PathBuf::from(v)),
        environ,
        default_socket: default_socket(Path::new(&s.server.socket_path)),
        dry_run,
        // The snapshot carries it (`Server::start_binary`).
        binary: None,
    };
    let mut server = Server::from_snapshot(listener, config, s, &mut |role| dup(role))?;
    if !dry_run {
        // Panes whose process exited while the image changed.
        server.reap_children();
    }
    Ok(server)
}

/// `s` without what a restore can lose and still serve every pane: pane
/// and popup terminal state, and modes (the last resort, decision 7).
fn without_terminals(s: &snap::Snapshot) -> snap::Snapshot {
    let mut s = s.clone();
    for p in &mut s.panes {
        p.term = None;
        p.modes.clear();
    }
    for c in &mut s.clients {
        if let Some(p) = &mut c.popup {
            p.term = None;
        }
    }
    s
}

/// Run the server until it exits. The signals blocked across the exec
/// (`sys::upgrade::exec_in_place`) come in now, handled.
fn run(mut server: Server) -> i32 {
    crate::sys::signal::unblock_handled();
    match server.run() {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// The new image (`--vtmux-resume <fd>`): resume the server from the
/// snapshot in inherited descriptor `snapshot_fd` and run it; returns its
/// exit status. `switched_back` is set in the image a failed resume
/// switched back to ([`sys::SWITCHED_BACK_FLAG`]), which does not switch
/// again.
#[must_use]
pub fn resume(snapshot_fd: RawFd, switched_back: bool) -> i32 {
    // Why the image that switched back to this one failed (it is taken out
    // of the environment either way).
    let note = sys::take_failure_note().filter(|_| switched_back);
    let file = sys::inherited(snapshot_fd).ok();
    let bytes = file
        .as_ref()
        .and_then(|f| sys::read_all(f.as_fd()).ok())
        .unwrap_or_default();
    let decoded = decode(&bytes);
    let meta = match &decoded {
        Ok(s) => Some(s.meta.clone()),
        Err(_) => writer_meta(&bytes),
    };
    // Every descriptor the snapshot names, taken once for every attempt,
    // and the executable of the image before, for the switch back.
    let kept = decoded.as_ref().map(adopt).unwrap_or_default();
    // Whoever asked over the frozen handshake, to be told how it went.
    let requester = meta.as_ref().and_then(|m| m.requester.clone());
    let mut handshake = handshake_fd(requester.as_ref()).and_then(|fd| sys::inherited(fd).ok());
    // (An image switched back to does not switch again; the descriptor
    // did not survive its exec anyway.)
    let exe = meta
        .as_ref()
        .and_then(|m| m.binary_fd)
        .filter(|_| !switched_back)
        .and_then(|fd| sys::inherited(fd).ok());
    let first = match &decoded {
        Ok(s) => guarded(|| match hook().as_deref() {
            Some("1") if !switched_back => Err(io::Error::other("test hook")),
            Some("always") => Err(io::Error::other("test hook")),
            Some("panic") if !switched_back => panic!("test hook"),
            _ => restore(s, &kept, false),
        }),
        Err(e) => Err(e.clone()),
    };
    let me = this_image();
    let mut why = match (first, &note) {
        (Ok(mut server), _) => {
            let outcome = match &note {
                Some(note) => {
                    let message = format!("live upgrade failed: {note}; switched back to {me}");
                    server.add_message(message.clone());
                    Err(message)
                }
                None => Ok(()),
            };
            answer(Some(&mut server), requester.as_ref(), handshake, outcome);
            drop((kept, exe, file));
            return run(server);
        }
        (Err(why), Some(note)) => {
            format!("{note}; switched back to {me}, which failed too: {why}")
        }
        (Err(why), None) => format!("{me}: {why}"),
    };
    // Decision 7: back to the image that wrote the snapshot, on the same
    // descriptors.
    if !switched_back && let Some(file) = &file {
        let keep: Vec<RawFd> = kept
            .iter()
            .map(|(_, fd)| fd.as_raw_fd())
            .chain(handshake.as_ref().map(AsRawFd::as_raw_fd))
            .collect();
        let path = meta.as_ref().and_then(writer_binary);
        let e = sys::switch_back(
            exe.as_ref().map(AsFd::as_fd),
            path.as_deref(),
            file.as_fd(),
            &keep,
            &why,
        );
        let image = path.map_or_else(|| "the image before".into(), |p| p.display().to_string());
        why = format!("{why}; switching back to {image}: {e}");
    }
    drop(exe);
    // The last resort: serve the panes with no terminal state.
    if let Ok(s) = &decoded
        && let Ok(mut server) = guarded(|| restore(&without_terminals(s), &kept, false))
    {
        let message =
            format!("live upgrade failed: {why}; panes restored without their terminal state");
        server.add_message(message.clone());
        answer(
            Some(&mut server),
            requester.as_ref(),
            handshake.take(),
            Err(message),
        );
        drop((kept, file));
        return run(server);
    }
    drop(file);
    answer(
        None,
        requester.as_ref(),
        handshake.take(),
        Err(format!("live upgrade failed: {why}; no session restored")),
    );
    let held = kept
        .into_iter()
        .map(|(_, fd)| fd)
        .chain(sys::inherit_rest())
        .collect();
    serve_held(held, &why)
}

/// Every descriptor in `s`'s table that is open, taken by this image.
fn adopt(s: &snap::Snapshot) -> Vec<(snap::Role, OwnedFd)> {
    s.descriptors
        .iter()
        .filter_map(|d| Some((d.role.clone(), sys::inherited(d.fd).ok()?)))
        .collect()
}

/// The descriptor of a requester over the frozen handshake, which an
/// upgrade keeps across `execve` outside the descriptor table.
fn handshake_fd(requester: Option<&snap::Requester>) -> Option<RawFd> {
    match requester? {
        snap::Requester::Handshake(fd) => Some(*fd),
        snap::Requester::Client(_) => None,
    }
}

/// Tell whoever asked for the upgrade how it went, from the image that
/// ends up serving: `Err` with why when that is not the new one. A
/// handshake gets the frozen `UpgradeResult` on `handshake` (written at
/// once, the descriptor closed after); a client is told by `server`
/// ([`Server::upgrade_answer`]).
fn answer(
    server: Option<&mut Server>,
    requester: Option<&snap::Requester>,
    handshake: Option<OwnedFd>,
    outcome: Result<(), String>,
) {
    match (requester, server) {
        (Some(snap::Requester::Handshake(_)), _) => {
            if let Some(fd) = handshake {
                let (ok, message) = match outcome {
                    Ok(()) => (true, String::new()),
                    Err(m) => (false, m),
                };
                let mut stream = std::os::unix::net::UnixStream::from(fd);
                // The requester is waiting for this one small frame, so
                // the socket has room; a requester that went is no matter.
                let _ = vtmux_core::protocol::write_msg(
                    &mut stream,
                    &vtmux_core::protocol::ServerMsg::UpgradeResult { ok, message },
                );
            }
        }
        (Some(snap::Requester::Client(id)), Some(server)) => {
            server.upgrade_answer(super::ClientId(*id as usize), outcome);
        }
        _ => {}
    }
}

/// This image's executable, for the messages a failed resume logs.
fn this_image() -> String {
    std::env::current_exe().map_or_else(
        |_| "this image".into(),
        |p| sys::exe_path(&p).display().to_string(),
    )
}

/// The very last resort, when no image could rebuild the server: a server
/// with no session on the listening socket found among `held`, holding
/// every pane PTY open (`Server::held`) so that no pane process loses its
/// terminal. The other sockets (clients, `pipe-pane`) are closed: with no
/// session to show, an attached client is better told its server went
/// than left frozen. The PTYs are read and what comes is discarded
/// ([`Server::hold`]), so a program printing does not block on a full
/// terminal. It answers commands (`ls`, `show-messages`, which
/// says what happened, `kill-server`) and handles signals as any server.
/// With no listening socket left, the process only holds the descriptors,
/// ignoring every handled signal but SIGTERM.
fn serve_held(mut held: Vec<OwnedFd>, why: &str) -> i32 {
    if let Some((i, socket)) = sys::find_listener(&held) {
        let listener = StdListener::from(held.remove(i));
        held.retain(|fd| rustix::net::getsockname(fd).is_err());
        let config = ServerConfig {
            config_files: Vec::new(),
            config_quiet: true,
            environ: std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
            cwd: std::env::current_dir().ok(),
            default_socket: default_socket(&socket),
            dry_run: false,
            binary: None,
        };
        if listener.set_nonblocking(true).is_ok()
            && let Ok(mut server) = Server::new(listener, socket, config)
        {
            server.config_loaded = true;
            server.cfg_finished = true;
            server.add_message(format!(
                "live upgrade failed: {why}; no session restored, {} descriptors held open",
                held.len()
            ));
            server.hold(held);
            return run(server);
        }
    }
    crate::sys::signal::ignore_handled_but_term();
    loop {
        std::thread::park();
    }
}

impl Server {
    /// Hold `held` open (the last resort, [`serve_held`]), reading the
    /// terminals among them, the pane and popup PTY masters, for
    /// [`Self::held_readable`] to discard what their programs write.
    fn hold(&mut self, held: Vec<OwnedFd>) {
        for (i, fd) in held.iter().enumerate() {
            let registered = rustix::termios::isatty(fd)
                && rustix::fs::fcntl_setfl(fd, rustix::fs::OFlags::NONBLOCK).is_ok()
                && self
                    .poll
                    .registry()
                    .register(
                        &mut SourceFd(&fd.as_raw_fd()),
                        Token(super::HELD_BASE + i),
                        Interest::READABLE,
                    )
                    .is_ok();
            if registered {
                // Edge-triggered: what is there already is read now.
                self.held_backlog.insert(i);
            }
        }
        self.held = held;
    }

    /// Held descriptor `i` is readable: read and discard, at most
    /// `READS` reads a turn (the rest the next, through the backlog). At
    /// the end of the terminal (its program's side closed) it leaves the
    /// poll set, still held.
    pub(super) fn held_readable(&mut self, i: usize) {
        const READS: usize = 64;
        let Some(fd) = self.held.get(i) else { return };
        let mut buf = [0; 1 << 14];
        for _ in 0..READS {
            match rustix::io::read(fd, &mut buf) {
                Ok(1..) | Err(rustix::io::Errno::INTR) => {}
                Err(rustix::io::Errno::AGAIN) => return,
                // EOF, or EIO once the slave side is closed (Linux).
                Ok(0) | Err(_) => {
                    let _ = self
                        .poll
                        .registry()
                        .deregister(&mut SourceFd(&fd.as_raw_fd()));
                    return;
                }
            }
        }
        self.held_backlog.insert(i);
    }

    /// Sequence steps 3, 4, 5 and 7 (live-upgrade spec): the snapshot
    /// into an anonymous memory file, the dry run of `binary` on it, then
    /// `execve(binary)` with the descriptors it names kept open. Returns
    /// only if the server is still this image, with why. The preflight and
    /// quiesce before it are `upgrade.rs`'s. `requester` goes in the
    /// snapshot (a handshake's descriptor is kept across the exec too), so
    /// the image that ends up serving tells it how the upgrade went
    /// ([`answer`]): only a failure that returns here is this image's to
    /// report.
    pub(crate) fn exec_upgrade(
        &mut self,
        binary: &Path,
        requester: Option<snap::Requester>,
    ) -> String {
        let fail =
            |e: &io::Error| format!("{}: {}", binary.display(), vtmux_core::error::strerror(e));
        let exe = std::env::current_exe()
            .map(|p| sys::exe_path(&p))
            .unwrap_or_default();
        let written_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        let binary_fd = self.exe.as_ref().map(AsRawFd::as_raw_fd);
        let handshake = handshake_fd(requester.as_ref());
        let snapshot = self.to_snapshot(snap::Meta {
            purpose: snap::Purpose::Upgrade,
            writer_version: vtmux_core::version::display(),
            binary: exe.to_string_lossy().into_owned(),
            ghostty_patches: String::new(),
            written_us,
            requires: Vec::new(),
            binary_fd,
            requester,
        });
        let bytes = snap::encode(&snapshot);
        let dir = self.socket.parent().unwrap_or(Path::new("/tmp"));
        let file = match sys::memfile(dir, &bytes) {
            Ok(f) => f,
            Err(e) => return format!("snapshot: {}", vtmux_core::error::strerror(&e)),
        };
        match sys::dry_run(binary, file.as_fd()) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return message,
            Err(e) => return fail(&e),
        }
        let keep: Vec<RawFd> = snapshot
            .descriptors
            .iter()
            .map(|d| d.fd)
            .chain(binary_fd)
            .chain(handshake)
            .collect();
        fail(&(self.upgrade.exec)(binary, file, &keep))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The writer's binary and executable descriptor, from a stream
    /// whose rest is damaged.
    fn writer(bytes: &[u8]) -> Option<(Option<PathBuf>, Option<i32>)> {
        writer_meta(bytes).map(|m| (writer_binary(&m), m.binary_fd))
    }

    #[test]
    fn the_writer_binary_is_read_from_a_stream_whose_rest_is_damaged() {
        let s = snap::Snapshot {
            meta: snap::Meta {
                binary: "/opt/vtmux/old".into(),
                binary_fd: Some(9),
                ..snap::Meta::default()
            },
            // Records after the header one, for the damage to land in.
            sessions: vec![snap::Session {
                name: "s".into(),
                ..snap::Session::default()
            }],
            ..snap::Snapshot::default()
        };
        let mut bytes = snap::encode(&s);
        let want = Some((Some(PathBuf::from("/opt/vtmux/old")), Some(9)));
        assert_eq!(writer(&bytes), want);
        // A flipped byte fails the hash: no decode, but the header record
        // still names the binary.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(decode(&bytes).is_err());
        assert_eq!(writer(&bytes), want);
        assert_eq!(writer(b"garbage"), None);
    }

    #[test]
    fn a_relative_writer_binary_is_not_switched_back_to() {
        let meta = snap::Meta {
            binary: "vtmux".into(),
            ..snap::Meta::default()
        };
        assert_eq!(writer_binary(&meta), None);
    }

    /// A private directory under /tmp, removed on drop.
    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dir(name: &str) -> Dir {
        let d = PathBuf::from(format!("/tmp/vtr-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    /// The dry run rebuilds the server, not only decodes it: a snapshot
    /// that decodes and validates but names no listening socket can't be
    /// resumed from, and the dry run says so. A rebuild that works leaves
    /// the running server's socket and shim as they were.
    #[test]
    fn the_dry_run_rebuilds_the_server_and_leaves_nothing_behind() {
        let d = dir("check");
        let socket = d.0.join("s");
        // Not `bind_socket`: its umask is the process's, and a test thread
        // creating a directory meanwhile would make it 0600 (EACCES).
        let l = StdListener::bind(&socket).unwrap();
        l.set_nonblocking(true).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = Server::new(l, socket.clone(), ServerConfig::default()).unwrap();
        let shim = server.shim.as_ref().map(|s| s.dir().join("tmux"));
        let taken = server.to_snapshot(snap::Meta::default());
        assert!(
            taken
                .descriptors
                .iter()
                .any(|d| d.role == snap::Role::Listener(()))
        );

        assert_eq!(check_bytes(&snap::encode(&taken)), Ok(()));
        assert!(socket.exists());
        if let Some(link) = &shim {
            assert!(std::fs::symlink_metadata(link).is_ok(), "shim removed");
        }

        let mut no_listener = taken.clone();
        no_listener.descriptors.clear();
        assert_eq!(
            check_bytes(&snap::encode(&no_listener)),
            Err("no listening socket kept".to_owned())
        );
        drop(server);
    }

    /// A panic in a rebuild is an error with its message, not an unwind
    /// out of the image.
    #[test]
    fn a_panicking_rebuild_is_an_error() {
        let got = guarded(|| panic!("boom"));
        assert_eq!(
            got.err(),
            Some("rebuilding the server panicked: boom".to_owned())
        );
    }

    #[test]
    fn the_last_resort_drops_terminal_state_and_modes_only() {
        let s = snap::Snapshot {
            panes: vec![snap::Pane {
                id: 3,
                term: Some(snap::TermState::default()),
                modes: vec![snap::PaneMode::Closed(())],
                ..snap::Pane::default()
            }],
            ..snap::Snapshot::default()
        };
        let bare = without_terminals(&s);
        assert_eq!(bare.panes[0].id, 3);
        assert!(bare.panes[0].term.is_none() && bare.panes[0].modes.is_empty());
    }
}
