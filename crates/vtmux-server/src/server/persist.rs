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
//! A `full` checkpoint is compared first on a [`Precheck`]: the
//! checkpoint without its terminals, and each pane's output count,
//! history size and terminal state outside its rows. Building it exports
//! no history, so an idle server with a long one does not hold the loop
//! each interval to find nothing changed (8 ms at 10 panes x 100k lines
//! in a release build, where the export takes 1.7 s). Only when it differs is every
//! terminal exported and the whole checkpoint compared.
//!
//! `vtmux-persist` set from `full` to `layout` writes a `layout`
//! checkpoint at the next loop turn (or the shutdown checkpoint, if that
//! comes first) whether anything changed or not, with
//! `manifest.prev` a copy of it and the chunks no manifest names deleted
//! ([`Server::persist_timers`], [`crate::persist::Writer::replacing_prev`]),
//! so no screen or history stays on disk until the next change.
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
//!   checkpoint is written at once. Not yet incremental: once anything
//!   changed, building the snapshot exports every terminal on the loop,
//!   unchanged ones too, 1.4 s at 10 panes x 100k lines in a release
//!   build (`bench_checkpoint_ten_panes_of_100k_lines`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vtmux_core::options::TreeId;
use vtmux_snapshot as snap;

use super::Server;
use crate::persist::{Known, Lock, MANIFEST, MANIFEST_PREV, Writer, Written};
use crate::snapshot_term::Form;

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

/// How soon a periodic checkpoint refused the directory's lock tries
/// again, for [`crate::persist::LOCK_WAIT`] ([`Server::persist_lock`]).
const LOCK_RETRY: Duration = Duration::from_millis(2);

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

impl Kept {
    /// As the snapshot carries it (`snap::Server::persist_hold`), so the
    /// image a live upgrade starts keeps it.
    pub(super) fn hold(self) -> u32 {
        match self {
            Self::No => 0,
            Self::WhileThere => snap::PERSIST_HOLD_WHILE_THERE,
            Self::Always => snap::PERSIST_HOLD_ALWAYS,
        }
    }

    /// [`Kept::hold`] read back; a value this build does not know holds
    /// the directory for good.
    pub(super) fn from_hold(hold: u32) -> Self {
        match hold {
            0 => Self::No,
            snap::PERSIST_HOLD_WHILE_THERE => Self::WhileThere,
            _ => Self::Always,
        }
    }
}

/// What a checkpoint is compared on.
#[derive(Debug)]
struct Print {
    /// [`fingerprint`] of the checkpoint.
    whole: Vec<u8>,
    /// [`Precheck`] of the server it was built from, for a `full` one.
    pre: Option<Precheck>,
}

/// A `full` checkpoint's stand-in for the export of its terminals (module
/// docs): whatever changes a terminal's rows moves its output count, its
/// history size, its geometry or its state outside the rows, or else
/// something the rest of the checkpoint holds (a pane respawned or dead,
/// a title, an option, copy mode).
#[derive(Debug, PartialEq)]
struct Precheck {
    /// [`fingerprint`] of the checkpoint without its terminals.
    rest: Vec<u8>,
    /// Each pane's output count, history size and terminal state outside
    /// its pages.
    terms: Vec<(u32, u64, usize, vtmux_vt::TermState)>,
}

/// The checkpoint timer and what was last written.
#[derive(Debug)]
pub struct Persist {
    /// The next periodic checkpoint, while `vtmux-persist` is on.
    due: Option<Instant>,
    /// What the last checkpoint written is compared on.
    last: Option<Print>,
    /// What this process knows of the checkpoint directory: what it
    /// wrote there, or read back (a restore's read too).
    pub(super) known: Known,
    /// The shutdown checkpoint is written: nothing after it may replace it
    /// with the emptied server.
    exited: bool,
    /// `vtmux-persist` was `full` and no `layout` checkpoint has replaced
    /// what it may have written ([`Server::persist_timers`]).
    full_may_be_on_disk: bool,
    /// Why the checkpoint was not restored at start, until the starting
    /// client is told ([`Server::tell_starter`]).
    pub(super) refusal: Option<String>,
    /// The checkpoint directory, found once ([`Server::persist_dir`]).
    pub(crate) dir: Option<PathBuf>,
    /// The lock held on a checkpoint directory.
    pub(super) lock: Option<(PathBuf, Lock)>,
    /// What the checkpoint found at start ([`super::restore`]) leaves
    /// this server free to write.
    pub(super) kept: Kept,
    /// Another server holds the lock: said once in the message log.
    refused: bool,
    /// The first of the periodic checkpoint's refused attempts at the
    /// lock being retried ([`Server::persist_lock`]).
    lock_retry: Option<Instant>,
    /// A periodic checkpoint being written, and its fingerprint.
    writing: Option<(Writer, Print)>,
    /// How long a loop turn writes its chunks ([`STEP`]).
    pub(crate) step: Duration,
    /// A cold restore's `vtmux-persist-respawn auto` pass, due once the
    /// config has run ([`crate::restored`]).
    pub(crate) auto_pass: crate::restored::AutoPass,
    /// Checkpoint snapshots built ([`Server::checkpoint_snapshot`]).
    #[cfg(test)]
    pub(super) built: u32,
}

impl Default for Persist {
    fn default() -> Self {
        Self {
            due: None,
            last: None,
            known: Known::default(),
            exited: false,
            full_may_be_on_disk: false,
            refusal: None,
            dir: None,
            lock: None,
            kept: Kept::No,
            refused: false,
            lock_retry: None,
            writing: None,
            step: STEP,
            auto_pass: crate::restored::AutoPass::Done,
            #[cfg(test)]
            built: 0,
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
        let mode = self.persist_mode();
        if mode == Some(Mode::Full) {
            self.persist.full_may_be_on_disk = true;
        }
        if self.persist.exited || mode.is_none() {
            self.persist.due = None;
            self.persist.writing = None;
            self.persist.lock = None;
            self.persist.lock_retry = None;
            return;
        }
        // `full` set to `layout`: the screens and history of the full
        // checkpoints must not stay on disk until something changes.
        if mode == Some(Mode::Layout) && std::mem::take(&mut self.persist.full_may_be_on_disk) {
            self.checkpoint_layout_now();
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
                if let Some((w, print)) = self.persist.writing.take() {
                    self.persist.known = w.into_known();
                    self.persist.last = Some(print);
                }
            }
            Err(e) => {
                self.persist_drop_writer();
                self.persist_error(&e);
            }
        }
    }

    /// A restore refused at start ([`super::restore::start`]) while the
    /// server has no session by the time its starting client's command
    /// ends: `start-server` or `attach` with nothing to start, where the
    /// server exits at once (`exit-empty`) and takes the reason in
    /// `show-messages` with it. The client gets the reason on its stderr,
    /// once, and exits 1 where its command had succeeded (tmux has no
    /// restore to refuse; this is vtmux's own). Returns the exit status
    /// to send client `id`, `code` if it is not told.
    pub(crate) fn tell_starter(&mut self, id: super::ClientId, code: i32) -> i32 {
        // The starter is the first client (`Server::adopt_starter`).
        if id != super::ClientId(0) || !self.model.sessions.is_empty() {
            return code;
        }
        let Some(why) = self.persist.refusal.take() else {
            return code;
        };
        self.send(
            id,
            &vtmux_core::protocol::ServerMsg::Stderr(format!("{why}\n").into_bytes()),
        );
        code.max(1)
    }

    /// The checkpoint of a clean shutdown, before the panes are hung up;
    /// once, and all at once. `full` set to `layout` with no loop turn
    /// since (`set -s vtmux-persist layout ; kill-server`) prunes here,
    /// as that turn would have ([`Server::persist_timers`]).
    pub(crate) fn checkpoint_exit(&mut self) {
        if std::mem::replace(&mut self.persist.exited, true) {
            return;
        }
        self.persist.due = None;
        if self.persist_mode() == Some(Mode::Layout)
            && std::mem::take(&mut self.persist.full_may_be_on_disk)
        {
            self.checkpoint_layout_now();
        } else {
            self.checkpoint();
        }
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
        self.checkpoint_run(dir, mode, false)
    }

    /// The `layout` checkpoint at once when `vtmux-persist` goes from
    /// `full` to `layout`, changed or not, with `manifest.prev` a copy of
    /// it and the chunks no manifest names deleted
    /// ([`Writer::replacing_prev`]): the full checkpoints' screens and
    /// history would otherwise stay in `manifest.prev` and `chunks/` until
    /// the next change, a minute or an hour away.
    fn checkpoint_layout_now(&mut self) {
        if let Some((dir, mode)) = self.checkpoint_target() {
            self.checkpoint_run(&dir, mode, true);
        }
    }

    /// [`Server::checkpoint_to`]; `prune`: written whether changed or not,
    /// and not keeping the previous checkpoint ([`Writer::replacing_prev`]).
    fn checkpoint_run(&mut self, dir: &Path, mode: Mode, prune: bool) -> Option<Written> {
        self.persist_drop_writer();
        let (mut w, print) = self.checkpoint_writer(dir, mode, prune, true)?;
        let stepped = w.step(None, &mut |_| Ok(()));
        let written = w.written().clone();
        self.persist.known = w.into_known();
        match stepped {
            Ok(_) => {
                self.persist.last = Some(print);
                Some(written)
            }
            Err(e) => {
                self.persist_error(&e);
                None
            }
        }
    }

    /// Drop the periodic checkpoint being written, keeping what it
    /// learnt of the directory ([`Writer::into_known`]).
    fn persist_drop_writer(&mut self) {
        if let Some((w, _)) = self.persist.writing.take() {
            self.persist.known = w.into_known();
        }
    }

    /// Start the periodic checkpoint ([`Server::persist_step`] writes it).
    fn checkpoint_begin(&mut self) {
        self.persist.writing = self
            .checkpoint_target()
            .and_then(|(dir, mode)| self.checkpoint_writer(&dir, mode, false, false));
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
    /// this server holding the directory's lock (`wait`: see
    /// [`Server::persist_lock`]), and the server changed (or `prune`: see
    /// [`Server::checkpoint_run`]).
    fn checkpoint_writer(
        &mut self,
        dir: &Path,
        mode: Mode,
        prune: bool,
        wait: bool,
    ) -> Option<(Writer, Print)> {
        if !self.persist_lock(dir, wait) {
            return None;
        }
        let pre = (mode == Mode::Full).then(|| self.precheck());
        if !prune
            && let (Some(pre), Some(last)) = (&pre, &self.persist.last)
            && last.pre.as_ref() == Some(pre)
        {
            return None;
        }
        let s = self.checkpoint_snapshot(mode);
        #[cfg(test)]
        {
            self.persist.built += 1;
        }
        let print = Print {
            whole: fingerprint(&s),
            pre,
        };
        if !prune
            && let Some(last) = &mut self.persist.last
            && last.whole == print.whole
        {
            // Unchanged after all (a terminal's state moved outside what
            // a checkpoint holds): compared on this precheck from now on.
            last.pre = print.pre;
            return None;
        }
        match Writer::new(dir, &s, &self.persist.known) {
            Ok(w) => Some((if prune { w.replacing_prev() } else { w }, print)),
            Err(e) => {
                self.persist_error(&e);
                None
            }
        }
    }

    /// Hold the lock on `dir`, taking it if needed. Another server holding
    /// it is said in the message log once, and nothing is written while
    /// it does.
    ///
    /// With `wait` (a checkpoint written at once, once: the shutdown's,
    /// or `full` set to `layout`) this waits up to
    /// [`crate::persist::LOCK_WAIT`] for it ([`Lock::take`]): there is no
    /// later turn to retry on. Without (the periodic one) the loop never sleeps
    /// on it: a refused attempt is retried [`LOCK_RETRY`] later, from the
    /// loop, for as long, so a moment's holder (`cargo xtask reap`'s probe)
    /// neither costs the checkpoint nor gets reported; after that it is
    /// another server's until the next interval.
    fn persist_lock(&mut self, dir: &Path, wait: bool) -> bool {
        if self.persist.lock.as_ref().is_some_and(|(d, _)| d == dir) {
            return true;
        }
        self.persist.lock = None;
        let taken = if wait {
            Lock::take(dir)
        } else {
            Lock::try_take(dir)
        };
        match taken {
            Ok(l) => {
                // Without the lock another server may have written there.
                self.persist.known = Known::default();
                self.persist.lock = Some((dir.to_path_buf(), l));
                self.persist.refused = false;
                self.persist.lock_retry = None;
                true
            }
            Err(e)
                if !wait
                    && e.kind() == std::io::ErrorKind::WouldBlock
                    && self.persist_lock_retry() =>
            {
                false
            }
            Err(e) => {
                self.persist.lock_retry = None;
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

    /// A periodic checkpoint refused the lock: due again [`LOCK_RETRY`]
    /// from now while its refusals are younger than
    /// [`crate::persist::LOCK_WAIT`] (true); false once they are not.
    fn persist_lock_retry(&mut self) -> bool {
        let now = Instant::now();
        let since = *self.persist.lock_retry.get_or_insert(now);
        if now.duration_since(since) >= crate::persist::LOCK_WAIT {
            return false;
        }
        self.persist.due = Some(now + LOCK_RETRY);
        true
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

    /// The [`Precheck`] of a `full` checkpoint now.
    fn precheck(&self) -> Precheck {
        Precheck {
            rest: fingerprint(&self.checkpoint_snapshot_with(Mode::Full, None)),
            terms: self
                .model
                .panes
                .iter()
                .map(|(id, p)| {
                    (
                        id.0,
                        p.output_generation,
                        p.term.history_size(),
                        p.term.export_state_head(),
                    )
                })
                .collect(),
        }
    }

    /// The snapshot a checkpoint of `mode` writes (module docs).
    #[must_use]
    pub fn checkpoint_snapshot(&self, mode: Mode) -> snap::Snapshot {
        let form = match mode {
            Mode::Layout => None,
            Mode::Full => Some(Form::Rows),
        };
        self.checkpoint_snapshot_with(mode, form)
    }

    /// [`Server::checkpoint_snapshot`] with the terminals in `form`, or
    /// none.
    fn checkpoint_snapshot_with(&self, mode: Mode, form: Option<Form>) -> snap::Snapshot {
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
        let mut s = self.to_snapshot_with(meta, form);
        // The process: gone with it.
        s.descriptors.clear();
        s.clients.clear();
        s.server.next_client = 0;
        s.server.wait_channels.clear();
        s.server.persist_hold = 0;
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
            // Typed text, for a program that is gone.
            p.input_queued.clear();
            // A restored pane is dead, its scrollbar hidden.
            p.scrollbar_visible = false;
            p.scrollbar_hover = false;
            p.scrollbar_hide_in_ms = None;
            // Nor does it hold a mode 2048 grant (D31): its program is gone.
            if let Some(t) = &mut p.term {
                t.inband_resize = false;
                // Nor a kitty graphics grant (D33).
                t.graphics_primary = false;
                t.graphics_alternate = false;
            }
            p.graphics_owner_primary = None;
            p.graphics_owner_alternate = None;
        }
        s.meta.requires.retain(|r| {
            r != snap::SCROLLBAR_FEATURE
                && r != snap::INBAND_RESIZE_FEATURE
                && r != snap::GRAPHICS_GRANT_FEATURE
        });
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
