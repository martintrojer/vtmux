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
//! The preflight and quiesce (steps 1-2) are `upgrade.rs`'s, and so are
//! the attached clients' tokens and notices (step 6) and their re-attach
//! in the image that serves next. Pane terminal state is not carried
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
use crate::snapshot_term::Form;
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

/// What this binary reads beyond [`snap::KNOWN_FEATURES`]: history pages
/// in its own Ghostty form (decision 23).
fn features() -> snap::Features {
    crate::snapshot_term::reader_features()
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
        // Attached clients that can't re-attach (decision 25).
        server.detach_unfollowing();
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
/// again. `socket` is the `-S` it was started with, if any: only `ps`
/// reads it (the listener comes in the snapshot), and a switch back
/// passes it on.
#[must_use]
pub fn resume(snapshot_fd: RawFd, switched_back: bool, socket: Option<&Path>) -> i32 {
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
            // The stream and its decoded records (as large as the history
            // in Ghostty's form, twice over) are not held while serving.
            drop((kept, exe, file, decoded, bytes));
            sys::release_free_memory();
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
            socket,
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
/// ends up serving: `Err` with why when that is not the new one; `Ok`
/// with the warnings `server` gathered (the clients it detached or that
/// did not come back, decision 25), one a line. With a server, the answer
/// waits for the clients told to re-attach
/// ([`Server::upgrade_answer_later`]); without one, a handshake gets the
/// frozen `UpgradeResult` on `handshake` at once.
fn answer(
    server: Option<&mut Server>,
    requester: Option<&snap::Requester>,
    handshake: Option<OwnedFd>,
    outcome: Result<(), String>,
) {
    let Some(requester) = requester else {
        return;
    };
    match server {
        Some(server) => server.upgrade_answer_later(super::upgrade::LaterAnswer {
            requester: requester.clone(),
            handshake,
            outcome,
        }),
        None => {
            if let (snap::Requester::Handshake(_), Some(fd)) = (requester, handshake) {
                super::upgrade::write_result(fd, outcome.map(|()| String::new()));
            }
        }
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

    /// Sequence steps 3 and 4 (live-upgrade spec): the snapshot into an
    /// anonymous memory file, and the dry run of `binary` on it started,
    /// for the loop to wait on while it serves ([`Check`],
    /// `upgrade.rs`). The tokens attached clients re-attach with are made
    /// here, so the dry run rebuilds their records too. The preflight and
    /// quiesce before it are `upgrade.rs`'s.
    ///
    /// # Errors
    /// Why the upgrade is refused: the snapshot can't be written, or the
    /// binary can't be started.
    pub(crate) fn upgrade_check(
        &mut self,
        binary: &Path,
        requester: Option<snap::Requester>,
    ) -> Result<Check, String> {
        // Decision 23: history in Ghostty's form only to a binary of this
        // patch stack, and then all of it, compressed first.
        let form = if same_pin(binary) {
            Form::SamePin
        } else {
            Form::Rows
        };
        let (snapshot, file) = self.upgrade_snapshot(form, requester)?;
        self.check_on(binary, &snapshot, &file, form, 1)
    }

    /// The dry run of `binary` on `snapshot` (in `file`, of `form`)
    /// started, the `round`th of this upgrade.
    fn check_on(
        &mut self,
        binary: &Path,
        snapshot: &snap::Snapshot,
        file: &OwnedFd,
        form: Form,
        round: u8,
    ) -> Result<Check, String> {
        match sys::DryRun::start(binary, file.as_fd(), dry_run_limit(snapshot)) {
            Ok(run) => Ok(Check {
                run,
                form,
                requires: snapshot.meta.requires.clone(),
                round,
            }),
            Err(e) => {
                self.upgrade_tokens_void();
                Err(format!(
                    "{}: {}",
                    binary.display(),
                    vtmux_core::error::strerror(&e)
                ))
            }
        }
    }

    /// Once the dry run has passed (`passed`) and the work in flight is
    /// done again: sequence step 3 once more, then steps 5, 6 and 7. The
    /// server served on while the dry run ran, so the snapshot the exec
    /// gets is taken now, with everything since in it (the one checked is
    /// what the binary was shown it can read). One that requires a
    /// feature the checked one did not (its first history page in
    /// Ghostty's form, an auto-hide scrollbar shown) gets a dry run of its
    /// own instead ([`Exec::Again`]), up to [`CHECK_ROUNDS`] in all: the
    /// binary was never shown it reads that. Then `execve(binary)` with
    /// the descriptors the snapshot names kept open. Returns only if the
    /// server is still this image, with why. `requester` goes in the
    /// snapshot (a handshake's descriptor is kept across the exec too), so
    /// the image that ends up serving tells it how the upgrade went
    /// ([`answer`]): only a failure that returns here is this image's to
    /// report.
    pub(crate) fn exec_upgrade(
        &mut self,
        binary: &Path,
        requester: Option<snap::Requester>,
        passed: &Passed,
    ) -> Exec {
        let handshake = handshake_fd(requester.as_ref());
        let (snapshot, file) = match self.upgrade_snapshot(passed.form, requester) {
            Ok(taken) => taken,
            Err(message) => return Exec::Failed(message),
        };
        if let Some(need) = snapshot
            .meta
            .requires
            .iter()
            .find(|r| !passed.requires.contains(r))
        {
            if passed.round >= CHECK_ROUNDS {
                self.upgrade_tokens_void();
                return Exec::Failed(format!(
                    "the server changed during {CHECK_ROUNDS} dry runs: its snapshot now \
                     requires {need}, which no dry run checked: run vtmux-upgrade again"
                ));
            }
            return match self.check_on(binary, &snapshot, &file, passed.form, passed.round + 1) {
                Ok(check) => Exec::Again(check),
                Err(message) => Exec::Failed(message),
            };
        }
        // The editors the snapshot does not carry (an editor pane resumes
        // as a plain modal pane): their 0600 temp files would outlive the
        // exec (no destructor runs), so they go now, the last moment before
        // it. Not at `to_snapshot`'s export: that runs for each dry run and
        // for persistence checkpoints too, with the server serving on.
        for editor in self.model.panes.values().filter_map(|p| p.editor.as_ref()) {
            let _ = std::fs::remove_file(&editor.path);
        }
        // 6. Attached clients told to re-execute and re-attach.
        self.upgrade_notify(binary);
        let keep: Vec<RawFd> = snapshot
            .descriptors
            .iter()
            .map(|d| d.fd)
            .chain(snapshot.meta.binary_fd)
            .chain(handshake)
            .collect();
        let socket = passed.checked.takes_socket.then_some(self.socket.as_path());
        let e = (self.upgrade.exec)(binary, file, &keep, socket);
        Exec::Failed(format!(
            "{}: {}",
            binary.display(),
            vtmux_core::error::strerror(&e)
        ))
    }

    /// Step 3: the snapshot of an upgrade onto a binary that reads `form`,
    /// naming `requester`, in an anonymous memory file. Attached clients
    /// that follow upgrades get their tokens first (those that have one
    /// keep it). Refused, the tokens not yet sent are void.
    fn upgrade_snapshot(
        &mut self,
        form: Form,
        requester: Option<snap::Requester>,
    ) -> Result<(snap::Snapshot, OwnedFd), String> {
        let exe = std::env::current_exe()
            .map(|p| sys::exe_path(&p))
            .unwrap_or_default();
        let written_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        let binary_fd = self.exe.as_ref().map(AsRawFd::as_raw_fd);
        // The tokens attached clients re-attach with go in the snapshot.
        self.upgrade_tokens();
        if form == Form::SamePin {
            self.compress_all_history();
        }
        let snapshot = self.to_snapshot_with(
            snap::Meta {
                purpose: snap::Purpose::Upgrade,
                writer_version: vtmux_core::version::display(),
                binary: exe.to_string_lossy().into_owned(),
                ghostty_patches: String::new(),
                written_us,
                requires: Vec::new(),
                binary_fd,
                requester,
            },
            Some(form),
        );
        let bytes = snap::encode(&snapshot);
        let dir = self.socket.parent().unwrap_or(Path::new("/tmp"));
        match sys::memfile(dir, &bytes) {
            Ok(file) => Ok((snapshot, file)),
            Err(e) => {
                self.upgrade_tokens_void();
                Err(format!("snapshot: {}", vtmux_core::error::strerror(&e)))
            }
        }
    }

    /// Every pane's and popup's history compressed now, as far as Ghostty
    /// compresses it ([`vtmux_vt::compress_all_of`], the terminals shared
    /// out over threads), so that it all travels in Ghostty's form: 10
    /// panes x 100k lines of 200 columns took 0.17 s on the 2026-11
    /// devserver (0.85 s on one thread), where exporting them as rows took
    /// about 1.5 s and each of the two imports (the dry run's and the new
    /// image's) as long again (`lu_upgrade_perf`). If the upgrade fails,
    /// the history stays compressed, as the compressor would have left it.
    fn compress_all_history(&mut self) {
        let panes = self.model.panes.values_mut().map(|p| &mut p.term);
        let popups = self
            .clients
            .values_mut()
            .filter_map(|c| Some(&mut c.popup.as_mut()?.term));
        vtmux_vt::compress_all_of(panes.chain(popups));
    }
}

/// A dry run in flight (sequence step 4) on the snapshot
/// [`Server::upgrade_check`] took, which the loop serves around.
#[derive(Debug)]
pub(crate) struct Check {
    pub(crate) run: sys::DryRun,
    /// The form its history went in, which the exec's snapshot keeps.
    form: Form,
    /// The features the checked snapshot requires.
    requires: Vec<String>,
    /// Which dry run of the upgrade it is, from 1.
    round: u8,
}

impl Check {
    /// It has passed, saying `checked`.
    pub(crate) fn passed(self, checked: sys::Checked) -> Passed {
        Passed {
            checked,
            form: self.form,
            requires: self.requires,
            round: self.round,
        }
    }
}

/// A dry run that passed, for [`Server::exec_upgrade`].
#[derive(Debug)]
pub(crate) struct Passed {
    checked: sys::Checked,
    form: Form,
    requires: Vec<String>,
    round: u8,
}

/// How many dry runs an upgrade may take, when the snapshot after each
/// requires a feature the one before did not ([`Server::exec_upgrade`]).
/// There are two such features (Ghostty's history pages, the auto-hide
/// scrollbar), so three is one more than ever needed.
pub(crate) const CHECK_ROUNDS: u8 = 3;

/// What [`Server::exec_upgrade`] came to, when the exec did not happen.
#[derive(Debug)]
pub(crate) enum Exec {
    /// Refused, or the `execve` returned: why.
    Failed(String),
    /// The snapshot needs a dry run of its own: running.
    Again(Check),
}

/// Whether `binary` reads history in this binary's Ghostty form: it
/// reports the same patch-stack hash (decision 23, `--vtmux-patch-stack`).
fn same_pin(binary: &Path) -> bool {
    sys::patch_stack(binary, sys::PATCH_STACK_LIMIT).as_deref()
        == Some(crate::snapshot_term::PATCH_STACK)
}

/// Rows of history a dry run of `s` decodes and imports: history pages in
/// the vtmux row format (pages in Ghostty's form are only checked, by
/// checksum and layout), copy mode's frozen grids among them.
fn rows_to_import(s: &snap::Snapshot) -> u64 {
    let copies: u64 = s
        .panes
        .iter()
        .flat_map(|p| &p.modes)
        .filter_map(|m| match m {
            snap::PaneMode::Copy(c) | snap::PaneMode::View(c) => Some(&c.history),
            snap::PaneMode::Clock(_) | snap::PaneMode::Closed(()) => None,
        })
        .flatten()
        .map(|page| u64::from(page.rows))
        .sum();
    let terms = s.panes.iter().filter_map(|p| p.term.as_ref()).chain(
        s.clients
            .iter()
            .filter_map(|c| c.popup.as_ref()?.term.as_ref()),
    );
    terms
        .flat_map(|t| std::iter::once(&t.primary).chain(&t.alternate))
        .flat_map(|screen| &screen.history)
        .map(|page| u64::from(page.rows))
        .sum::<u64>()
        + copies
}

/// History rows per second of dry run allowed on top of
/// [`sys::DRY_RUN_LIMIT`]: at least twice what a dry run took on the
/// 2026-11 devserver, about 0.4 s per 100k rows.
const DRY_RUN_ROWS_PER_SEC: u64 = 100_000;

/// How long the dry run of `s` may take: [`sys::DRY_RUN_LIMIT`] (or the
/// test hook's, `upgrade::test_wait`) and a second for every
/// [`DRY_RUN_ROWS_PER_SEC`] rows of history it imports, so an
/// upgrade is never refused for the size of its history (an upgrade to
/// another patch stack carries it all as rows), while a binary that hangs
/// is still given up on.
pub(super) fn dry_run_limit(s: &snap::Snapshot) -> std::time::Duration {
    let rows = rows_to_import(s);
    super::upgrade::test_wait().unwrap_or(sys::DRY_RUN_LIMIT)
        + std::time::Duration::from_millis(rows.saturating_mul(1000) / DRY_RUN_ROWS_PER_SEC)
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

    /// The dry run's limit grows with the history it imports as rows, a
    /// second per 100k rows over the 3 s, so no upgrade is refused for its
    /// size; pages in Ghostty's form, only checked, add nothing.
    #[test]
    fn the_dry_run_limit_grows_with_the_rows_it_imports() {
        let page = |rows: u32, ghostty: bool| snap::HistoryPage {
            rows: if ghostty { 0 } else { rows },
            ghostty: if ghostty { vec![1; 64] } else { Vec::new() },
            ..snap::HistoryPage::default()
        };
        let term = |history: Vec<snap::HistoryPage>| snap::TermState {
            primary: snap::term::Screen {
                history,
                ..snap::term::Screen::default()
            },
            ..snap::TermState::default()
        };
        let mut s = snap::Snapshot::default();
        assert_eq!(dry_run_limit(&s), sys::DRY_RUN_LIMIT);
        s.panes = (0..10)
            .map(|id| snap::Pane {
                id,
                term: Some(term(vec![page(0, true); 400])),
                ..snap::Pane::default()
            })
            .collect();
        assert_eq!(dry_run_limit(&s), sys::DRY_RUN_LIMIT);
        // 10 panes x 100k rows, the alternate screen's too.
        for p in &mut s.panes {
            let t = p.term.as_mut().unwrap();
            t.primary.history = vec![page(1000, false); 90];
            t.alternate = Some(term(vec![page(1000, false); 10]).primary);
        }
        assert_eq!(
            dry_run_limit(&s),
            sys::DRY_RUN_LIMIT + std::time::Duration::from_secs(10)
        );
        // Copy mode's frozen grid always goes as rows, however its pane's
        // history goes: 200k rows over two panes.
        for p in &mut s.panes[..2] {
            p.modes = vec![snap::PaneMode::Copy(snap::CopyMode {
                history: vec![page(1000, false); 100],
                ..snap::CopyMode::default()
            })];
        }
        assert_eq!(
            dry_run_limit(&s),
            sys::DRY_RUN_LIMIT + std::time::Duration::from_secs(12)
        );
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

    /// Decision 23: a stream holding history pages in this build's Ghostty
    /// form (what the compressor leaves idle history in) is one this
    /// build's dry run and resume read: an upgrade onto itself of a server
    /// with compressed history goes through.
    #[test]
    fn this_builds_ghostty_pages_are_read() {
        let s = snap::Snapshot {
            meta: snap::Meta {
                requires: vec![crate::snapshot_term::ghostty_pages_requirement()],
                ..snap::Meta::default()
            },
            ..snap::Snapshot::default()
        };
        assert_eq!(decode(&snap::encode(&s)).err(), None);
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
