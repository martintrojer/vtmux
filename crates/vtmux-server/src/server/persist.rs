//! `vtmux-persist` in the server (D27 phase 3,
//! docs/specs/2026-11-06-live-upgrade.md, decisions 8, 16 and 17): when
//! a checkpoint is written and what it holds. The files are
//! [`crate::persist`]'s.
//!
//! A checkpoint is written on a clean shutdown (`kill-server`, SIGTERM or
//! SIGINT, the last session gone with `exit-empty`) and every 60 s while
//! something changed since the last one. "Changed" is the checkpoint's own
//! content: [`fingerprint`] leaves out what moves with the clock alone
//! (the write time, timers kept as "due in"/"ago"), so an idle server
//! writes nothing, and a field added to the snapshot is noticed without
//! anyone remembering to mark it dirty.
//!
//! What a checkpoint holds ([`Server::checkpoint_snapshot`]) is the
//! upgrade snapshot without what can't outlive the process: descriptors,
//! clients (their sockets, popups, control queues and status messages),
//! `pipe-pane` processes and the clients blocked in `wait-for`. With
//! `layout` it also holds no screen (decision 17): [`Server::to_snapshot`]
//! leaves out pane terminal state and copy mode's frozen grid for a
//! [`snap::Purpose::CheckpointLayout`] snapshot, so a layout checkpoint
//! never pays for exporting them. `full` keeps both, and their history
//! pages go to `chunks/`.
//!
//! Full mode needs nothing here beyond what [`Server::to_snapshot`]
//! exports: once pane terminal state fills `Pane::term` (`lu_pane_state`),
//! a `full` checkpoint carries each pane's screens and history, and only
//! history pages not yet in `chunks/` are written.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vtmux_core::options::TreeId;
use vtmux_snapshot as snap;

use super::Server;

/// What a checkpoint holds (`vtmux-persist`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Sessions, windows, layout, panes (dead on restore), options,
    /// buffers: no screen.
    Layout,
    /// [`Mode::Layout`] plus every pane's screens and history.
    Full,
}

/// How often a changed server is checkpointed (decision 16). A debug
/// build takes `VTMUX_PERSIST_INTERVAL_MS` instead, for tests.
#[must_use]
pub fn interval() -> Duration {
    static INTERVAL: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *INTERVAL.get_or_init(|| {
        let fake = cfg!(debug_assertions)
            .then(|| std::env::var("VTMUX_PERSIST_INTERVAL_MS").ok())
            .flatten()
            .and_then(|v| v.parse().ok());
        Duration::from_millis(fake.unwrap_or(60_000))
    })
}

/// The checkpoint timer and what was last written.
#[derive(Debug, Default)]
pub struct Persist {
    /// The next periodic checkpoint, while `vtmux-persist` is on.
    due: Option<Instant>,
    /// [`fingerprint`] of the last checkpoint written.
    last: Option<Vec<u8>>,
    /// The shutdown checkpoint is written: nothing after it may replace it
    /// with the emptied server.
    exited: bool,
}

impl Server {
    /// `vtmux-persist`, `None` when `off`.
    pub(crate) fn persist_mode(&self) -> Option<Mode> {
        match self.model.number_option(TreeId::Server, "vtmux-persist") {
            1 => Some(Mode::Layout),
            2 => Some(Mode::Full),
            _ => None,
        }
    }

    /// When the loop must wake for the next periodic checkpoint.
    pub(crate) fn persist_deadline(&self) -> Option<Instant> {
        self.persist.due
    }

    /// The periodic checkpoint: armed while `vtmux-persist` is on, written
    /// when due if anything changed.
    pub(crate) fn persist_timers(&mut self, now: Instant) {
        if self.persist.exited || self.persist_mode().is_none() {
            self.persist.due = None;
            return;
        }
        let due = *self.persist.due.get_or_insert(now + interval());
        if now >= due {
            self.persist.due = Some(now + interval());
            self.checkpoint();
        }
    }

    /// The checkpoint of a clean shutdown, before the panes are hung up;
    /// once.
    pub(crate) fn checkpoint_exit(&mut self) {
        if std::mem::replace(&mut self.persist.exited, true) {
            return;
        }
        self.persist.due = None;
        self.checkpoint();
    }

    /// Write a checkpoint if `vtmux-persist` is on and the server changed
    /// since the last one. A failure goes to the message log
    /// (`show-messages`); the next one tries again.
    pub(crate) fn checkpoint(&mut self) -> Option<crate::persist::Written> {
        let mode = self.persist_mode()?;
        let Some(dir) = self.persist_dir() else {
            self.add_message("vtmux-persist: no state directory (no home)".to_owned());
            return None;
        };
        self.checkpoint_to(&dir, mode)
    }

    /// [`Server::checkpoint`] of `mode` into `dir`.
    pub(crate) fn checkpoint_to(
        &mut self,
        dir: &std::path::Path,
        mode: Mode,
    ) -> Option<crate::persist::Written> {
        let s = self.checkpoint_snapshot(mode);
        let print = fingerprint(&s);
        if self.persist.last.as_ref() == Some(&print) {
            return None;
        }
        match crate::persist::write(dir, &s, &mut |_| Ok(())) {
            Ok(w) => {
                self.persist.last = Some(print);
                Some(w)
            }
            Err(e) => {
                self.add_message(format!("vtmux-persist: {}: {e}", dir.display()));
                None
            }
        }
    }

    /// This server's checkpoint directory, from the process's own
    /// `XDG_STATE_HOME` and `HOME` (else the passwd home).
    pub(crate) fn persist_dir(&self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::sys::user::current_user().map(|p| p.home));
        crate::persist::state_dir(
            &self.socket,
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            home.as_deref(),
        )
    }

    /// The snapshot a checkpoint of `mode` writes (module docs).
    #[must_use]
    pub fn checkpoint_snapshot(&self, mode: Mode) -> snap::Snapshot {
        let written_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        let meta = snap::Meta {
            purpose: match mode {
                Mode::Layout => snap::Purpose::CheckpointLayout,
                Mode::Full => snap::Purpose::CheckpointFull,
            },
            writer_version: vtmux_core::version::display(),
            binary: crate::shim::own_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            written_us,
            ..snap::Meta::default()
        };
        let mut s = self.to_snapshot(meta);
        s.descriptors.clear();
        s.clients.clear();
        for p in &mut s.panes {
            p.pipe = None;
        }
        for c in &mut s.server.wait_channels {
            c.waiters.clear();
            c.lockers.clear();
        }
        s
    }
}

/// `s` as a checkpoint compares with the last: its manifest encoding with
/// the write time and the clock-relative timers cleared. History pages
/// count by name.
fn fingerprint(s: &snap::Snapshot) -> Vec<u8> {
    let mut f = s.clone();
    f.meta.written_us = 0;
    for m in &mut f.server.monitors {
        m.due_in_ms = None;
    }
    for x in &mut f.sessions {
        x.lock_in_ms = None;
    }
    for w in &mut f.windows {
        w.silence_in_ms = None;
    }
    for p in &mut f.panes {
        p.output_ago_ms = None;
        p.resize_in_ms = None;
        p.rename_ago_ms = None;
        for m in &mut p.modes {
            if let snap::PaneMode::Copy(c) | snap::PaneMode::View(c) = m {
                c.refresh_in_ms = None;
            }
        }
    }
    snap::encode_manifest(&f)
}

#[cfg(test)]
mod tests;
