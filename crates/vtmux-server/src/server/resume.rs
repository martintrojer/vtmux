//! Both ends of the `execve` in a live upgrade (D27,
//! docs/specs/2026-11-06-live-upgrade.md, *Live upgrade*):
//!
//! - [`Server::exec_upgrade`], the old image: the snapshot into an
//!   anonymous memory file (sequence step 3), the dry run (step 4), the
//!   descriptors it names kept open (step 5) and `execve` (step 7).
//! - [`resume`], the new image (`--vtmux-resume <fd>`): decode the
//!   snapshot, take the descriptors back by number, rebuild the server,
//!   reap any pane that exited during the exec, and run. A resume that
//!   fails switches back to the binary that wrote the snapshot (decision
//!   7); if that fails too, the server is rebuilt without pane terminal
//!   state, and failing that it keeps every descriptor open rather than
//!   exit: panes never die because of an upgrade.
//! - [`check_snapshot`], the dry run (`--vtmux-check-snapshot <fd>`):
//!   everything [`resume`] checks before it touches a descriptor.
//!
//! Not here yet: the preflight and quiesce (steps 1-2) and telling
//! attached clients to re-execute (step 6). An attached client of the
//! same build stays connected through the exec, since its socket is kept;
//! the server redraws it from scratch. Pane terminal state is not carried
//! yet (`snapshot.rs`, `restore_term`): panes come back with blank
//! terminals of their size, their PTY, process and pid kept.

use std::io;
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};

use vtmux_snapshot as snap;

use super::{Server, ServerConfig};
use crate::sys::upgrade as sys;

/// A test hook (live-upgrade spec, *Tests*, failure paths): in the
/// server's environment, `1` makes a resume fail in the image an upgrade
/// execs (not in the one it switches back to); `always` makes the first
/// restore fail in both, so the last resort runs.
const FAIL_HOOK: &str = "VTMUX_TEST_RESUME_FAIL";

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
/// `fd` decodes fully and describes a server this binary can rebuild.
/// Nothing else is done with it: no descriptor it names is touched.
///
/// # Errors
/// Why the snapshot can't be resumed from, as a message for the
/// requester.
pub fn check_snapshot(fd: RawFd) -> Result<(), String> {
    let fd = sys::inherited(fd).map_err(|e| format!("snapshot descriptor {fd}: {e}"))?;
    let bytes = sys::read_all(fd.as_fd()).map_err(|e| format!("reading the snapshot: {e}"))?;
    decode(&bytes).map(|_| ())
}

/// The binary a stream was written by, read from its header record alone,
/// for a switch back when the rest does not decode.
fn writer_binary(bytes: &[u8]) -> Option<PathBuf> {
    let body = bytes.get(snap::HEADER_LEN..)?;
    if snap::version(bytes).ok()? > snap::FORMAT_VERSION {
        return None;
    }
    let cx = snap::wire::Ctx::for_input(snap::version(bytes).ok()?, body.len());
    let mut r = snap::wire::Reader::new(body);
    while let Ok(Some((tag, field))) = r.field() {
        if tag == 1 {
            let meta = <snap::Meta as snap::wire::Value>::get(field, &cx).ok()?;
            return Some(PathBuf::from(meta.binary)).filter(|p| p.is_absolute());
        }
    }
    None
}

/// The socket was the default `-L` one (`ServerConfig::default_socket`),
/// read from the group bits it was bound with (`bind_socket`).
fn default_socket(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o060 != 0)
}

/// The server `s` describes, on duplicates of the descriptors in `kept`
/// (so a failure closes nothing a switch back needs).
fn restore(s: &snap::Snapshot, kept: &[(snap::Role, OwnedFd)]) -> io::Result<Server> {
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
    };
    let mut server = Server::from_snapshot(listener, config, s, &mut |role| dup(role))?;
    // Panes whose process exited while the image changed.
    server.reap_children();
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

fn hook_fails(switched_back: bool) -> bool {
    match std::env::var(FAIL_HOOK).as_deref() {
        Ok("1") => !switched_back,
        Ok("always") => true,
        _ => false,
    }
}

/// Run the server until it exits.
fn run(mut server: Server) -> i32 {
    match server.run() {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// The new image (`--vtmux-resume <fd>`): resume the server from the
/// snapshot in inherited descriptor `snapshot_fd` and run it; returns its
/// exit status. `switched_back` is set in the image a failed resume
/// switched back to ([`sys::SWITCHED_BACK_FLAG`]), which does not switch
/// again. Returns at once (1) only when there is no snapshot to read.
#[must_use]
pub fn resume(snapshot_fd: RawFd, switched_back: bool) -> i32 {
    let Ok(file) = sys::inherited(snapshot_fd) else {
        return 1;
    };
    let Ok(bytes) = sys::read_all(file.as_fd()) else {
        park();
    };
    let decoded = decode(&bytes);
    // Every descriptor the snapshot names, taken once for every attempt.
    let mut kept: Vec<(snap::Role, OwnedFd)> = Vec::new();
    if let Ok(s) = &decoded {
        for d in &s.descriptors {
            if let Ok(fd) = sys::inherited(d.fd) {
                kept.push((d.role.clone(), fd));
            }
        }
    }
    let first = match &decoded {
        Ok(_) if hook_fails(switched_back) => None,
        Ok(s) => restore(s, &kept).ok(),
        Err(_) => None,
    };
    if let Some(server) = first {
        drop(kept);
        drop(file);
        return run(server);
    }
    // Decision 7: back to the binary that wrote the snapshot, on the same
    // descriptors.
    let previous = match &decoded {
        Ok(s) => Some(PathBuf::from(&s.meta.binary)).filter(|p| p.is_absolute()),
        Err(_) => writer_binary(&bytes),
    };
    if !switched_back && let Some(binary) = previous {
        let keep: Vec<(snap::Role, RawFd)> = kept
            .iter()
            .map(|(r, fd)| (r.clone(), fd.as_raw_fd()))
            .collect();
        let _ = sys::switch_back(&binary, file.as_fd(), &keep);
    }
    // The last resort: serve the panes with no terminal state.
    if let Ok(s) = &decoded
        && let Ok(server) = restore(&without_terminals(s), &kept)
    {
        drop(kept);
        drop(file);
        return run(server);
    }
    for (_, fd) in kept {
        let _ = fd.into_raw_fd();
    }
    let _ = file.into_raw_fd();
    park();
}

/// Keep every descriptor open and do nothing, for ever: what is left when
/// no image can rebuild the server. Pane programs block on output but
/// live; SIGTERM still ends this process.
fn park() -> ! {
    loop {
        std::thread::park();
    }
}

impl Server {
    /// Sequence steps 3, 4, 5 and 7 (live-upgrade spec): the snapshot
    /// into an anonymous memory file, the dry run of `binary` on it, then
    /// `execve(binary)` with the descriptors it names kept open. Returns
    /// only if the server is still this image, with why.
    pub(crate) fn exec_upgrade(&mut self, binary: &Path) -> String {
        let fail =
            |e: &io::Error| format!("{}: {}", binary.display(), vtmux_core::error::strerror(e));
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.flush_client(id);
        }
        let exe = std::env::current_exe()
            .map(|p| sys::exe_path(&p))
            .unwrap_or_default();
        let written_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        let snapshot = self.to_snapshot(snap::Meta {
            purpose: snap::Purpose::Upgrade,
            writer_version: vtmux_core::version::display(),
            binary: exe.to_string_lossy().into_owned(),
            ghostty_patches: String::new(),
            written_us,
            requires: Vec::new(),
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
        let keep: Vec<(snap::Role, RawFd)> = snapshot
            .descriptors
            .iter()
            .map(|d| (d.role.clone(), d.fd))
            .collect();
        fail(&sys::exec_in_place(binary, file, &keep))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_writer_binary_is_read_from_a_stream_whose_rest_is_damaged() {
        let s = snap::Snapshot {
            meta: snap::Meta {
                binary: "/opt/vtmux/old".into(),
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
        assert_eq!(writer_binary(&bytes), Some(PathBuf::from("/opt/vtmux/old")));
        // A flipped byte fails the hash: no decode, but the header record
        // still names the binary.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(decode(&bytes).is_err());
        assert_eq!(writer_binary(&bytes), Some(PathBuf::from("/opt/vtmux/old")));
        assert_eq!(writer_binary(b"garbage"), None);
    }

    #[test]
    fn a_relative_writer_binary_is_not_switched_back_to() {
        let s = snap::Snapshot {
            meta: snap::Meta {
                binary: "vtmux".into(),
                ..snap::Meta::default()
            },
            ..snap::Snapshot::default()
        };
        assert_eq!(writer_binary(&snap::encode(&s)), None);
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
