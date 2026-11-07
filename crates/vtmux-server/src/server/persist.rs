//! `vtmux-persist` in the server (D27 phase 3,
//! docs/specs/2026-11-06-live-upgrade.md, decisions 8, 16 and 17): when
//! a checkpoint is written and what it holds. The files are
//! [`crate::persist`]'s.
//!
//! A checkpoint is written on a clean shutdown (`kill-server`, SIGTERM or
//! SIGINT, the last session gone with `exit-empty`) and every 60 s while
//! something changed since the last one. "Changed" is the checkpoint's own
//! content: [`fingerprint`] leaves out what moves with the clock alone
//! (the write time, timers kept as "due in"/"ago", activity stamps), and
//! the checkpoint holds nothing a read-only command changes (no message
//! log, no command counter), so an idle server writes nothing however
//! often tools poll it, and a field added to the snapshot is noticed
//! without anyone remembering to mark it dirty.
//!
//! What a checkpoint holds ([`Server::checkpoint_snapshot`], decision 17):
//! the structure the user built, never what was typed or shown.
//!
//! - Both modes: sessions, windows (names, layout, zoom, flags), panes
//!   (start command, cwd, geometry, dead status), options (with user
//!   options and hooks), key bindings, id counters. Never the message log
//!   (it records every command line: `send-keys` text), the environment
//!   (tokens), prompt history, the last search string, the command
//!   counter, hook and monitor run counts, config errors, or anything of
//!   the process: descriptors,
//!   clients (their sockets, popups, control queues and status messages),
//!   `pipe-pane` processes and `wait-for` channels.
//! - `layout` also leaves out every pane's screen, history and title (a
//!   program sets it, often to a command line or path), copy mode (its
//!   frozen grid is a screen) and paste buffers (copied screen text):
//!   [`Server::to_snapshot`] never exports terminal state or copy mode
//!   for a [`snap::Purpose::CheckpointLayout`] snapshot.
//! - `full` adds the screens and history (every pane's terminal state,
//!   in the vtmux row format even where Ghostty holds a page compressed,
//!   decision 23; copy mode), titles and paste buffers. Its history pages
//!   go to `chunks/`, a few per loop turn
//!   ([`crate::persist::Writer`]), so a first `full` checkpoint of
//!   hundreds of new pages never holds the loop for long; the shutdown
//!   checkpoint is written at once. Not yet incremental: building the
//!   snapshot exports every terminal on the loop, unchanged ones too
//!   (the fingerprint compares the whole export), 1.4 s at 10 panes x
//!   100k lines in a release build (`bench_checkpoint_ten_panes_of_100k_lines`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vtmux_core::options::TreeId;
use vtmux_snapshot as snap;

use super::Server;
use crate::persist::{Lock, MANIFEST, MANIFEST_PREV, Writer, Written};

/// What a checkpoint holds (`vtmux-persist`; module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Sessions, windows, layout, panes (dead on restore), options: no
    /// screen, title or buffer.
    Layout,
    /// [`Mode::Layout`] plus every pane's screens, history and title, and
    /// the paste buffers.
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

/// How long one loop turn writes chunks of a periodic checkpoint.
const STEP: Duration = Duration::from_millis(5);

/// What the checkpoint found at start leaves this server free to write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Kept {
    /// Nothing: checkpoints go to the directory.
    #[default]
    No,
    /// It could not be restored: nothing is written over it while its
    /// manifests are still there.
    WhileThere,
    /// A newer binary wrote it (decision 6): nothing is written to the
    /// directory for this server's lifetime, whatever happens to the
    /// files meanwhile.
    Always,
}

/// The checkpoint timer and what was last written.
#[derive(Debug)]
pub struct Persist {
    /// The next periodic checkpoint, while `vtmux-persist` is on.
    due: Option<Instant>,
    /// [`fingerprint`] of the last checkpoint written.
    last: Option<Vec<u8>>,
    /// The shutdown checkpoint is written: nothing after it may replace it
    /// with the emptied server.
    exited: bool,
    /// The checkpoint directory, found once ([`Server::persist_dir`]).
    pub(crate) dir: Option<PathBuf>,
    /// The lock held on a checkpoint directory.
    pub(super) lock: Option<(PathBuf, Lock)>,
    /// What the checkpoint found at start ([`super::restore`]) leaves
    /// this server free to write.
    pub(super) kept: Kept,
    /// Another server holds the lock: said once in the message log.
    refused: bool,
    /// A periodic checkpoint being written, and its fingerprint.
    writing: Option<(Writer, Vec<u8>)>,
    /// How long a loop turn writes its chunks ([`STEP`]).
    pub(crate) step: Duration,
}

impl Default for Persist {
    fn default() -> Self {
        Self {
            due: None,
            last: None,
            exited: false,
            dir: None,
            lock: None,
            kept: Kept::No,
            refused: false,
            writing: None,
            step: STEP,
        }
    }
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

    /// When the loop must wake for the persistence: now while a
    /// checkpoint is being written, else the next periodic one.
    pub(crate) fn persist_deadline(&self) -> Option<Instant> {
        if self.persist.writing.is_some() {
            return Some(Instant::now());
        }
        self.persist.due
    }

    /// The periodic checkpoint: armed while `vtmux-persist` is on, started
    /// when due if anything changed, and written a step per turn.
    pub(crate) fn persist_timers(&mut self, now: Instant) {
        if self.persist.exited || self.persist_mode().is_none() {
            self.persist.due = None;
            self.persist.writing = None;
            self.persist.lock = None;
            return;
        }
        self.persist_step();
        let due = *self.persist.due.get_or_insert(now + interval());
        if now >= due {
            self.persist.due = Some(now + interval());
            // One at a time: a write still going takes this turn's place.
            if self.persist.writing.is_none() {
                self.checkpoint_begin();
                self.persist_step();
            }
        }
    }

    /// One step of the checkpoint being written, if one is.
    fn persist_step(&mut self) {
        let Some((w, _)) = &mut self.persist.writing else {
            return;
        };
        match w.step(Some(self.persist.step), &mut |_| Ok(())) {
            Ok(false) => {}
            Ok(true) => {
                self.persist.last = self.persist.writing.take().map(|(_, print)| print);
            }
            Err(e) => {
                self.persist.writing = None;
                self.persist_error(&e);
            }
        }
    }

    /// The checkpoint of a clean shutdown, before the panes are hung up;
    /// once, and all at once.
    pub(crate) fn checkpoint_exit(&mut self) {
        if std::mem::replace(&mut self.persist.exited, true) {
            return;
        }
        self.persist.due = None;
        self.checkpoint();
    }

    /// Write a checkpoint now, all at once, if `vtmux-persist` is on and
    /// the server changed since the last one ([`Server::checkpoint_to`]).
    pub(crate) fn checkpoint(&mut self) -> Option<Written> {
        let (dir, mode) = self.checkpoint_target()?;
        self.checkpoint_to(&dir, mode)
    }

    /// Write a checkpoint of `mode` into `dir` now, all at once, if this
    /// server holds the directory's lock and changed since the last one; a
    /// periodic one still being written is dropped for it (the chunks it
    /// wrote count as on disk). A failure goes to the message log
    /// (`show-messages`); the next one tries again.
    pub(crate) fn checkpoint_to(&mut self, dir: &Path, mode: Mode) -> Option<Written> {
        self.persist.writing = None;
        let (mut w, print) = self.checkpoint_writer(dir, mode)?;
        match w.step(None, &mut |_| Ok(())) {
            Ok(_) => {
                self.persist.last = Some(print);
                Some(w.written().clone())
            }
            Err(e) => {
                self.persist_error(&e);
                None
            }
        }
    }

    /// Start the periodic checkpoint ([`Server::persist_step`] writes it).
    fn checkpoint_begin(&mut self) {
        self.persist.writing = self
            .checkpoint_target()
            .and_then(|(dir, mode)| self.checkpoint_writer(&dir, mode));
    }

    /// The directory and mode a checkpoint goes to, while `vtmux-persist`
    /// is on.
    fn checkpoint_target(&mut self) -> Option<(PathBuf, Mode)> {
        let mode = self.persist_mode()?;
        let Some(dir) = self.persist_dir() else {
            self.add_message("vtmux-persist: no state directory (no home)".to_owned());
            return None;
        };
        match self.persist.kept {
            // Before this server first writes there (no lock held yet): a
            // newer binary's checkpoint is left alone, as at the start
            // (`super::restore::written_by_newer`).
            Kept::No => {
                if self.persist.lock.is_none()
                    && let Some(why) = super::restore::written_by_newer(&dir)
                {
                    self.persist.kept = Kept::Always;
                    self.add_message(super::restore::left_alone(&dir, &why));
                    return None;
                }
            }
            // A newer binary's checkpoint: the directory is left alone
            // (`super::restore`, which said so).
            Kept::Always => return None,
            // The checkpoint a restore refused stays as it was until the
            // user moves it away (`super::restore`, which said so).
            Kept::WhileThere => {
                if [MANIFEST, MANIFEST_PREV]
                    .iter()
                    .any(|m| dir.join(m).exists())
                {
                    return None;
                }
                self.persist.kept = Kept::No;
            }
        }
        Some((dir, mode))
    }

    /// A writer for a checkpoint of `mode` into `dir`, if one is wanted:
    /// this server holding the directory's lock, and the server changed.
    fn checkpoint_writer(&mut self, dir: &Path, mode: Mode) -> Option<(Writer, Vec<u8>)> {
        if !self.persist_lock(dir) {
            return None;
        }
        let s = self.checkpoint_snapshot(mode);
        let print = fingerprint(&s);
        if self.persist.last.as_ref() == Some(&print) {
            return None;
        }
        match Writer::new(dir, &s) {
            Ok(w) => Some((w, print)),
            Err(e) => {
                self.persist_error(&e);
                None
            }
        }
    }

    /// Hold the lock on `dir`, taking it if needed. Another server holding
    /// it is said in the message log once, and nothing is written while
    /// it does.
    fn persist_lock(&mut self, dir: &Path) -> bool {
        if self.persist.lock.as_ref().is_some_and(|(d, _)| d == dir) {
            return true;
        }
        self.persist.lock = None;
        match Lock::take(dir) {
            Ok(l) => {
                self.persist.lock = Some((dir.to_path_buf(), l));
                self.persist.refused = false;
                true
            }
            Err(e) => {
                if !std::mem::replace(&mut self.persist.refused, true) {
                    let why = if e.kind() == std::io::ErrorKind::WouldBlock {
                        "in use by another server, not written".to_owned()
                    } else {
                        e.to_string()
                    };
                    self.add_message(format!("vtmux-persist: {}: {why}", dir.display()));
                }
                false
            }
        }
    }

    fn persist_error(&mut self, e: &std::io::Error) {
        let dir = self
            .persist
            .lock
            .as_ref()
            .map(|(d, _)| d.display().to_string())
            .unwrap_or_default();
        self.add_message(format!("vtmux-persist: {dir}: {e}"));
    }

    /// This server's checkpoint directory ([`crate::persist::state_dir`] of
    /// its socket's absolute path), from the process's own
    /// `XDG_STATE_HOME` and `HOME` (else the passwd home); found once.
    pub(crate) fn persist_dir(&mut self) -> Option<PathBuf> {
        if self.persist.dir.is_none() {
            let home = std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from)
                .or_else(|| crate::sys::user::current_user().map(|p| p.home));
            let socket = std::path::absolute(&self.socket).ok()?;
            self.persist.dir = crate::persist::state_dir(
                &socket,
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                home.as_deref(),
            );
        }
        self.persist.dir.clone()
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
        // The process: gone with it.
        s.descriptors.clear();
        s.clients.clear();
        s.server.next_client = 0;
        s.server.wait_channels.clear();
        // What was typed or ran: the message log records every command
        // line (cmd-queue.c `cmdq_add_message`), the environment holds
        // tokens, prompt history and search strings are typed text.
        let x = &mut s.server;
        x.messages.clear();
        x.message_next = 0;
        x.environment.clear();
        x.command_history.clear();
        x.search_history.clear();
        x.config_causes.clear();
        x.cmd_number = 0;
        // Run-time counters every command moves (`after-*` hooks fire
        // for each), and what monitors last saw (formatted values).
        x.hook_fires.clear();
        for m in &mut x.monitors {
            m.fire_count = 0;
            m.fire_time = 0;
            m.last = None;
            m.each.clear();
            m.due_in_ms = None;
        }
        for x in &mut s.sessions {
            x.environment.clear();
        }
        for w in &mut s.windows {
            // `window-size latest`: a client, gone.
            w.latest_client = None;
        }
        for p in &mut s.panes {
            p.pipe = None;
            p.search = None;
            p.environment.clear();
            // A restored pane is dead, its scrollbar hidden.
            p.scrollbar_visible = false;
            p.scrollbar_hover = false;
            p.scrollbar_hide_in_ms = None;
        }
        s.meta.requires.retain(|r| r != snap::SCROLLBAR_FEATURE);
        if mode == Mode::Layout {
            // Copied screen text, and titles programs set.
            s.server.buffers.clear();
            s.server.buffer_next_index = 0;
            s.server.buffer_next_order = 0;
            for p in &mut s.panes {
                p.title.clear();
            }
        }
        s
    }
}

/// `s` as a checkpoint compares with the last: its manifest encoding with
/// the write time, the clock-relative timers and the activity stamps
/// (which pane output alone moves) cleared. History pages count by name.
fn fingerprint(s: &snap::Snapshot) -> Vec<u8> {
    let mut f = s.clone();
    f.meta.written_us = 0;
    f.server.activity = 0;
    for x in &mut f.sessions {
        x.lock_in_ms = None;
        x.activity = 0;
        x.activity_secs = 0;
    }
    for w in &mut f.windows {
        w.silence_in_ms = None;
        w.activity = 0;
        w.activity_secs = 0;
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
