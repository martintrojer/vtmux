//! Idle scrollback compression (docs/specs/2026-10-31-history-memory.md).
//!
//! libghostty-vt compresses cold history pages when asked and creates no
//! thread or timer, so the server drives it. Each pane keeps the
//! [`CompressionActivity`] it last saw; whenever that changes (output,
//! resize, a new `history-limit`, or vtmux reading the history, which
//! restores pages without Ghostty noticing), the pane is due [`IDLE`]
//! later. Once due, the loop runs one incremental step per turn,
//! round-robin across due panes, until the pane's pass is complete; then
//! it is idle until its activity changes again. A pane with no history to
//! compress is never due, so an idle server has no compression wakeups.
//! A step runs only in a loop turn that woke for nothing else (poll timed
//! out) at least [`QUIET`] after the last turn that had work, so it never
//! follows straight on from a pane's output (before its frame is even
//! flushed, or when its next output is likeliest). A turn woken for a step
//! alone runs only the step and polls again: the rest of a turn costs more
//! than a step (a full render pass, about 1.2 ms for one attached client,
//! against 0.3 ms). After each step the loop rests [`REST_FACTOR`] times as
//! long as the step took (and at least [`STEP_GAP`]), blocked in poll, so
//! output arriving meanwhile is handled at once: a busy pane's output meets
//! a step in progress about one time in `REST_FACTOR + 1`.
//!
//! Starvation override: a pane whose history has kept changing (gaps
//! under [`IDLE`]) or kept wanting a pass (held off by a busy loop) for
//! [`STARVE`] is starved. Its steps also run at the end of busy turns,
//! after the turn's frames are written, one step per turn, and in quiet
//! turns without waiting out `IDLE`. The rest is shared: with `n` panes
//! starved it is `REST_FACTOR / n` times the step (`n` up to
//! [`MAX_SHARE`]), so each starved pane gets the steps a lone one would,
//! and twenty panes streaming at once keep up as one does. Ghostty walks the
//! oldest pages first, farthest from the active area. A pane stays starved
//! until its pass is done and its history quiet for `IDLE`, so each new
//! page of a streaming pane is compressed soon after it scrolls off, and
//! its memory stays near what an idle pane's settles to.
//!
//! Every function takes the time as an argument: tests drive it with a
//! clock of their own.

use std::time::{Duration, Instant};

use vtmux_core::ids::PaneId;
use vtmux_vt::{Compression, CompressionActivity, PaneTerm};

use crate::server::Server;

/// How long a pane's activity must stay unchanged before its history is
/// compressed.
pub const IDLE: Duration = Duration::from_secs(5);

/// Rest after a step, as a multiple of the step's own time. At 1 the
/// busy pane's p95 output latency rose 21% (spec scenario) and 270%
/// (narrow truecolour, 2.5-4.5 ms steps) while other panes compressed.
pub const REST_FACTOR: u32 = 100;

/// How long the loop must have had nothing to do before a step runs.
pub const QUIET: Duration = Duration::from_millis(50);

/// The least rest between two steps: Ghostty's own renderer waits 1 ms
/// before a pending pass's next step (renderer/Thread.zig `step_interval`).
pub const STEP_GAP: Duration = Duration::from_millis(1);

/// How long a pane's history must keep changing (or a pass be held off)
/// before its steps run in busy turns too. A shell someone types in for
/// less than this, or with pauses of [`IDLE`], never meets a step in a busy
/// turn; at 1000 lines/s in a 120-column pane it is about 26 pages (10 MB)
/// before the first one.
pub const STARVE: Duration = Duration::from_secs(10);

/// The most starved panes the rest is shared by: a rest stays at least
/// `REST_FACTOR / MAX_SHARE` (4) times the step, so compression never
/// takes more than a fifth of the loop however many panes stream.
pub const MAX_SHARE: u32 = 25;

/// The rest after a step that took `took`, with `starved` panes starved
/// at the time: [`REST_FACTOR`] times the step, shared by the starved
/// panes (each gets the steps a lone pane gets), at least [`STEP_GAP`].
/// Each step meets the same rest whatever turn ran it, so a busy pane's
/// output still meets at most one step in progress.
#[must_use]
pub fn rest(took: Duration, starved: usize) -> Duration {
    let share = u32::try_from(starved).map_or(MAX_SHARE, |n| n.clamp(1, MAX_SHARE));
    (took * REST_FACTOR / share).max(STEP_GAP)
}

/// One pane's compression schedule.
#[derive(Debug, Default)]
pub struct PaneCompress {
    /// The activity last seen; `None` before the first look.
    activity: Option<CompressionActivity>,
    /// When the next step may run; `None` while there is nothing to do.
    due: Option<Instant>,
    /// Ghostty can't compress on this target: never due again.
    unsupported: bool,
    /// Scroll progress at the last completed pass (including pruning).
    completed_scroll: Option<u64>,
    /// A resize restores history even without a full page of new output.
    size: Option<(u16, u16)>,
    /// When the history last changed (activity or size).
    changed_at: Option<Instant>,
    /// Since when it has kept changing with gaps under [`IDLE`], or kept
    /// wanting a pass.
    busy_since: Option<Instant>,
}

impl PaneCompress {
    /// Look at `term` after something that may have changed its history:
    /// a new activity postpones a pending pass. A completed pass waits
    /// for one page of new scrollback, or an explicit history read/resize.
    /// With no history to compress the pane is not due at all.
    pub fn observe(&mut self, term: &PaneTerm, now: Instant) {
        let activity = term.compression_activity();
        let size = (term.cols(), term.rows());
        let resized = self.size != Some(size);
        self.size = Some(size);
        if self.unsupported || (self.activity == Some(activity) && !resized) {
            return;
        }
        // A new run starts only from rest: quiet for IDLE with no pass
        // pending (one held off by a busy loop keeps its run).
        if self.due.is_none() && self.changed_at.is_none_or(|c| now >= c + IDLE) {
            self.busy_since = Some(now);
        }
        self.changed_at = Some(now);
        let read = self
            .activity
            .is_some_and(|old| activity.history_was_read(old));
        self.activity = Some(activity);
        let (scroll, page_rows) = term.compression_progress();
        let new_page = self
            .completed_scroll
            .is_none_or(|last| scroll < last || scroll.saturating_sub(last) >= page_rows as u64);
        if self.due.is_some() || read || resized || new_page {
            self.due = term.has_history().then(|| now + IDLE);
        }
    }

    /// The pane's terminal was just rebuilt from a snapshot (live upgrade,
    /// restore): history imported as rows is resident, so it is due at
    /// once rather than after [`IDLE`] (decision 23), and its steps run in
    /// the loop's quiet turns as any pane's do (starved after [`STARVE`]
    /// if output keeps coming). Pages that came across compressed are
    /// skipped by the pass. Output meanwhile postpones the pass as usual.
    pub fn restored(&mut self, term: &PaneTerm, now: Instant) {
        *self = Self {
            activity: Some(term.compression_activity()),
            size: Some((term.cols(), term.rows())),
            due: term.has_history().then_some(now),
            busy_since: Some(now),
            ..Self::default()
        };
    }

    /// When the pane next wants a step in a quiet turn: once its history
    /// has been idle for [`IDLE`], or once it is starved.
    #[must_use]
    pub fn due(&self) -> Option<Instant> {
        self.due.map(|d| self.starved_at().map_or(d, |s| d.min(s)))
    }

    /// From when the pane is starved and steps in busy turns too: [`STARVE`]
    /// after its history started changing, from rest (no pass pending and
    /// no change for [`IDLE`]). A pane that went quiet but whose pass a busy
    /// loop holds off stays in its run.
    #[must_use]
    pub fn starved_at(&self) -> Option<Instant> {
        self.due?;
        self.busy_since.map(|b| b + STARVE)
    }

    /// Run one step if the pane is due at `now`. A finished pass leaves
    /// the pane idle until its activity changes; stepping again would only
    /// start a new pass over the same pages. Unsupported leaves it idle
    /// for good.
    pub fn step(&mut self, term: &mut PaneTerm, now: Instant) -> Option<Compression> {
        if self.due().is_none_or(|d| d > now) {
            return None;
        }
        let result = term.compress_step();
        if result == Compression::Pending {
            self.due = Some(now);
        } else {
            self.due = None;
            self.activity = Some(term.compression_activity());
            self.unsupported = result == Compression::Unsupported;
            self.completed_scroll = Some(term.compression_progress().0);
        }
        Some(result)
    }
}

/// The pane after `last` (in id order, wrapping) that is due at `now`.
pub fn next_due(
    mut panes: impl Iterator<Item = (PaneId, Option<Instant>)> + Clone,
    last: Option<PaneId>,
    now: Instant,
) -> Option<PaneId> {
    let due = |&(_, d): &(PaneId, Option<Instant>)| d.is_some_and(|d| d <= now);
    let after = panes
        .clone()
        .filter(due)
        .find(|&(p, _)| last.is_none_or(|l| p > l));
    after.or_else(|| panes.find(due)).map(|(p, _)| p)
}

/// The server's side: which pane stepped last, the history reads already
/// looked at, when the next step may run, and when the loop last had work.
#[derive(Debug, Default)]
pub struct Rotation {
    last: Option<PaneId>,
    reads: u64,
    rest_until: Option<Instant>,
    busy_at: Option<Instant>,
}

impl Server {
    /// The soonest a pane wants a compression step, for the poll timeout.
    pub(crate) fn compress_deadline(&self) -> Option<Instant> {
        let due = self.compress_due()?;
        let quiet = self.compress.busy_at.map(|b| b + QUIET);
        Some(
            [self.compress.rest_until, quiet]
                .into_iter()
                .flatten()
                .fold(due, Instant::max),
        )
    }

    /// How many panes are starved at `now`: the share of [`rest`].
    fn starved(&self, now: Instant) -> usize {
        self.model
            .panes
            .values()
            .filter(|p| p.compress.starved_at().is_some_and(|s| s <= now))
            .count()
    }

    /// The soonest a pane is due, rest and quiet aside (a starved pane at
    /// once: a quiet turn steps it without waiting out [`IDLE`]).
    fn compress_due(&self) -> Option<Instant> {
        self.model
            .panes
            .values()
            .filter_map(|p| p.compress.due())
            .min()
    }

    /// One loop turn's compression: panes whose history vtmux read since
    /// the last turn (capture-pane, copy mode) are looked at again, then,
    /// if the turn was `idle` (woke for nothing else) and the loop has been
    /// [`QUIET`], the next due pane after the last one stepped runs one step.
    /// A busy turn (called at its end, after rendering) steps only a
    /// starved pane ([`STARVE`]). The rest after the step is shared by the
    /// panes starved before it ([`rest`]).
    pub(crate) fn compress_turn(&mut self, now: Instant, idle: bool) {
        let reads = vtmux_vt::history_reads();
        if reads != self.compress.reads {
            self.compress.reads = reads;
            for p in self.model.panes.values_mut() {
                p.compress.observe(&p.term, now);
            }
        }
        let wants: fn(&PaneCompress) -> Option<Instant> = if idle {
            if self.compress.busy_at.is_some_and(|b| now < b + QUIET) {
                return;
            }
            PaneCompress::due
        } else {
            self.compress.busy_at = Some(now);
            PaneCompress::starved_at
        };
        if self.compress.rest_until.is_some_and(|r| r > now) {
            return;
        }
        let panes = self
            .model
            .panes
            .iter()
            .map(|(&id, p)| (id, wants(&p.compress)));
        let Some(id) = next_due(panes, self.compress.last, now) else {
            return;
        };
        self.compress.last = Some(id);
        let starved = self.starved(now);
        if let Some(p) = self.model.panes.get_mut(&id) {
            let start = Instant::now();
            p.compress.step(&mut p.term, now);
            let took = start.elapsed();
            self.compress.rest_until = Some(now + took + rest(took, starved));
        }
    }
}

/// Feed `lines` numbered, styled lines, enough history for several pages.
#[cfg(test)]
pub(crate) fn fill(term: &mut PaneTerm, lines: usize) {
    use std::fmt::Write as _;
    let mut s = String::new();
    for i in 0..lines {
        let _ = write!(s, "\x1b[3{}mline {i} some log text\x1b[m\r\n", i % 8);
    }
    term.feed(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtmux_vt::{CaptureOpts, LineRef, capture};

    fn all() -> CaptureOpts {
        CaptureOpts {
            escapes: true,
            start: LineRef::Start,
            end: LineRef::End,
            ..CaptureOpts::default()
        }
    }

    /// Steps the pane to completion at `now`, returning how many it took.
    fn drain(c: &mut PaneCompress, t: &mut PaneTerm, now: Instant) -> usize {
        let mut n = 0;
        while let Some(r) = c.step(t, now) {
            n += 1;
            if r != Compression::Pending {
                assert_eq!(r, Compression::Complete);
                break;
            }
            assert!(n < 10_000, "compression never completed");
        }
        n
    }

    /// A pane rebuilt from a snapshot's rows has its history resident:
    /// `restored` makes it due at once, not after IDLE, and the pass
    /// compresses it again (live upgrade spec, decision 23).
    #[test]
    fn a_restored_pane_is_due_at_once() {
        use crate::snapshot_term::{Form, term_from_snapshot, term_to_snapshot};
        let t0 = Instant::now();
        let mut t = PaneTerm::new(80, 24, 10_000);
        fill(&mut t, 5000);
        let mut c = PaneCompress::default();
        c.observe(&t, t0);
        drain(&mut c, &mut t, t0 + IDLE);
        if t.compressed_pages() == 0 {
            return; // Ghostty can't compress on this target.
        }
        let state = term_to_snapshot(&t, Form::Rows);
        let mut back = term_from_snapshot(&state, vtmux_vt::PATCH_STACK).unwrap();
        assert_eq!(back.compressed_pages(), 0, "rows come back resident");
        let mut c = PaneCompress::default();
        c.restored(&back, t0);
        assert_eq!(c.due(), Some(t0));
        assert!(drain(&mut c, &mut back, t0) > 0);
        assert_eq!(back.compressed_pages(), t.compressed_pages());
        assert_eq!(capture(&back, all()), capture(&t, all()));
        // A pane with no history has nothing to do.
        let mut c = PaneCompress::default();
        c.restored(&PaneTerm::new(80, 24, 10_000), t0);
        assert_eq!(c.due(), None);
    }

    /// Arm on output, wait out the idle delay, compress, disarm on
    /// Complete; re-arm after vtmux reads the history and after more
    /// output, each time from that moment.
    #[test]
    fn arms_disarms_and_rearms_on_an_injected_clock() {
        let t0 = Instant::now();
        let mut t = PaneTerm::new(80, 24, 10_000);
        let mut c = PaneCompress::default();
        fill(&mut t, 5000);
        c.observe(&t, t0);
        assert_eq!(c.due(), Some(t0 + IDLE));

        // Output before the deadline postpones it.
        let t1 = t0 + Duration::from_secs(3);
        assert_eq!(c.step(&mut t, t1), None);
        fill(&mut t, 10);
        c.observe(&t, t1);
        assert_eq!(c.due(), Some(t1 + IDLE));
        assert_eq!(c.step(&mut t, t0 + IDLE), None, "not due yet");

        // Due: steps run until the pass completes, then nothing is due.
        let t2 = t1 + IDLE;
        assert!(drain(&mut c, &mut t, t2) > 1);
        assert_eq!(c.due(), None);
        assert!(t.compressed_pages() > 0);
        c.observe(&t, t2 + IDLE);
        assert_eq!(c.due(), None, "nothing changed: no new pass");

        // Reading the history restores pages without changing Ghostty's
        // token: the read re-arms.
        let t3 = t2 + Duration::from_secs(60);
        let _ = capture(&t, all());
        c.observe(&t, t3);
        assert_eq!(c.due(), Some(t3 + IDLE));
        assert!(drain(&mut c, &mut t, t3 + IDLE) > 0);
        assert_eq!(c.due(), None);

        // Visible rows only: no history read, no re-arm.
        let _ = capture(&t, CaptureOpts::default());
        c.observe(&t, t3 + IDLE);
        assert_eq!(c.due(), None);

        // A resize reflows (and restores) every page.
        t.resize(100, 24);
        let t4 = t3 + Duration::from_secs(100);
        c.observe(&t, t4);
        assert_eq!(c.due(), Some(t4 + IDLE));
    }

    /// Nothing to compress, nothing due: a pane without history, or
    /// without a full page of it.
    #[test]
    fn trickle_does_not_restart_a_pass() {
        let now = Instant::now();
        let mut t = PaneTerm::new(120, 40, 100_000);
        let mut c = PaneCompress::default();
        fill(&mut t, 8000);
        c.observe(&t, now);
        drain(&mut c, &mut t, now + IDLE);
        for i in 1..20 {
            fill(&mut t, 1);
            c.observe(&t, now + Duration::from_secs(i * 6));
            assert_eq!(c.due(), None, "one new line is not a page");
        }
        fill(&mut t, 1000);
        c.observe(&t, now + Duration::from_secs(120));
        assert!(c.due().is_some(), "new full pages do arm compression");
    }

    #[test]
    fn no_history_no_deadline() {
        let now = Instant::now();
        let mut none = PaneTerm::new(80, 24, 0);
        let mut c = PaneCompress::default();
        fill(&mut none, 500);
        c.observe(&none, now);
        assert_eq!(c.due(), None);

        let mut short = PaneTerm::new(80, 24, 2000);
        let mut c = PaneCompress::default();
        fill(&mut short, 30);
        c.observe(&short, now);
        assert_eq!(c.due(), None);
        assert_eq!(c.step(&mut short, now + IDLE), None);
    }

    /// capture-pane -S - -E - (with and without -e) reads the same bytes
    /// before compression, with every page compressed, and after.
    #[test]
    fn capture_is_byte_identical_across_compression() {
        let now = Instant::now();
        let mut t = PaneTerm::new(80, 24, 10_000);
        fill(&mut t, 8000);
        t.feed("wide 中文字 \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\r\n".as_bytes());
        fill(&mut t, 3000);
        let plain = CaptureOpts {
            escapes: false,
            ..all()
        };
        let before = (capture(&t, all()), capture(&t, plain));
        let mut c = PaneCompress::default();
        c.observe(&t, now);
        drain(&mut c, &mut t, now + IDLE);
        let compressed = t.compressed_pages();
        assert!(compressed > 5, "{compressed} pages compressed");
        let after = (capture(&t, all()), capture(&t, plain));
        assert!(before.0 == after.0, "capture -e changed");
        assert!(before.1 == after.1, "capture changed");
    }

    /// Through the server: a fresh pane has no deadline (no poll wakeup);
    /// history arms it, a turn runs one step for one pane, and copy mode
    /// reading the history re-arms it.
    #[test]
    fn server_turns_step_and_rearm_after_copy_mode() {
        let (mut server, p, _, _, dir) = crate::mode::prompt::tests::setup();
        server.reset_mode_all(p);
        let _ = vtmux_vt::history_reads();
        server.compress_turn(Instant::now(), true);
        assert_eq!(server.compress_deadline(), None, "nothing to compress");

        let t0 = Instant::now();
        let pane = server.model.panes.get_mut(&p).unwrap();
        fill(&mut pane.term, 3000);
        pane.compress.observe(&pane.term, t0);
        assert_eq!(server.compress_deadline(), Some(t0 + IDLE));
        server.compress_turn(t0, true);
        assert_eq!(server.model.panes[&p].term.compressed_pages(), 0);
        // One step, then a rest of at least STEP_GAP before the next.
        server.compress_turn(t0 + IDLE, true);
        let pages = server.model.panes[&p].term.compressed_pages();
        assert_eq!(pages, 1);
        let next = server.compress_deadline().unwrap();
        assert!(next >= t0 + IDLE + STEP_GAP);
        server.compress_turn(next.checked_sub(Duration::from_micros(1)).unwrap(), true);
        assert_eq!(server.model.panes[&p].term.compressed_pages(), pages);
        let mut turns = 0;
        let mut now = next;
        while let Some(next) = server.compress_deadline() {
            now = next;
            server.compress_turn(now, true);
            turns += 1;
            assert!(turns < 10_000);
        }
        let compressed = server.model.panes[&p].term.compressed_pages();
        assert!(compressed >= pages);

        // copy-mode reads the history (from its own copy of the pages, so
        // the pane's stay compressed), which re-arms the deadline.
        server.model.set_copy_mode(p, false, |m| {
            crate::mode::CopyMode::new(
                &m.panes[&p].term,
                (20, 5),
                false,
                (false, false),
                false,
                None,
            )
        });
        assert_eq!(server.model.panes[&p].term.compressed_pages(), compressed);
        let t1 = now + Duration::from_secs(60);
        server.compress_turn(t1, true);
        assert_eq!(server.compress_deadline(), Some(t1 + IDLE));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Starvation counts from the start of a run of history changes with
    /// gaps under IDLE; it survives completed passes while the run goes on,
    /// and an IDLE gap ends it.
    #[test]
    fn starvation_follows_continuous_activity() {
        let t0 = Instant::now();
        let mut t = PaneTerm::new(120, 40, 100_000);
        let mut c = PaneCompress::default();
        let page = t.compression_progress().1;
        fill(&mut t, 3 * page);
        c.observe(&t, t0);
        assert_eq!(c.due(), Some(t0 + IDLE));
        assert_eq!(c.starved_at(), Some(t0 + STARVE));
        // Output every second postpones IDLE but not STARVE.
        let mut now = t0;
        for _ in 0..STARVE.as_secs() - 1 {
            now += Duration::from_secs(1);
            fill(&mut t, 10);
            c.observe(&t, now);
            assert_eq!(c.step(&mut t, now), None, "not starved yet");
        }
        assert_eq!(c.due(), Some(t0 + STARVE), "starved before idle");
        now = t0 + STARVE;
        assert!(drain(&mut c, &mut t, now) > 1);
        assert_eq!((c.due(), c.starved_at()), (None, None));
        // Still the same run: the next page is due at once.
        now += Duration::from_secs(1);
        fill(&mut t, page);
        c.observe(&t, now);
        assert_eq!(c.due(), Some(t0 + STARVE));
        assert!(c.step(&mut t, now).is_some());
        // A gap with a pass pending keeps the run (a busy loop held it off).
        now += IDLE;
        fill(&mut t, 10);
        c.observe(&t, now);
        assert_eq!(c.starved_at(), Some(t0 + STARVE));
        // A gap with no pass pending ends it: the next starves STARVE later.
        drain(&mut c, &mut t, now);
        now += IDLE;
        fill(&mut t, page);
        c.observe(&t, now);
        assert_eq!(c.due(), Some(now + IDLE));
        assert_eq!(c.starved_at(), Some(now + STARVE));
    }

    /// A busy turn steps only a starved pane, one step per turn under the
    /// same rest, and before STARVE never; quiet turns still need QUIET.
    /// A streaming pane's pass completes in busy turns, and its next page
    /// is compressed without another STARVE wait.
    #[test]
    fn busy_turns_step_only_starved_panes() {
        let (mut server, p, _, _, dir) = crate::mode::prompt::tests::setup();
        server.reset_mode_all(p);
        let compressed_pages = |s: &Server| s.model.panes[&p].term.compressed_pages();
        let t0 = Instant::now();
        let pane = server.model.panes.get_mut(&p).unwrap();
        // Room for several pages: the default limit keeps less than one.
        pane.term.set_history_limit(100_000);
        let page_rows = pane.term.compression_progress().1;
        fill(&mut pane.term, 3 * page_rows);
        pane.compress.observe(&pane.term, t0);
        let due = t0 + IDLE;
        server.compress_turn(due, false);
        // Idle, but not QUIET since the busy turn.
        server.compress_turn(due + Duration::from_millis(1), true);
        assert_eq!(server.compress_deadline(), Some(due + QUIET));
        assert_eq!(compressed_pages(&server), 0);
        // The loop stays busy (the pane is silent): no step before STARVE.
        let mut now = due;
        while now + Duration::from_millis(10) < t0 + STARVE {
            now += Duration::from_millis(10);
            server.compress_turn(now, false);
        }
        assert_eq!(
            compressed_pages(&server),
            0,
            "busy turn stepped before STARVE"
        );
        now = t0 + STARVE;
        server.compress_turn(now, false);
        assert_eq!(compressed_pages(&server), 1);
        let rest = server.compress.rest_until.unwrap();
        assert!(rest >= now + STEP_GAP);
        server.compress_turn(rest.checked_sub(Duration::from_micros(1)).unwrap(), false);
        assert_eq!(compressed_pages(&server), 1, "stepped during the rest");
        // Output keeps arriving: busy turns still finish the pass.
        let mut turns = 0;
        while server.model.panes[&p].compress.due.is_some() {
            now = server.compress.rest_until.unwrap();
            let pane = server.model.panes.get_mut(&p).unwrap();
            fill(&mut pane.term, 1);
            pane.compress.observe(&pane.term, now);
            server.compress_turn(now, false);
            turns += 1;
            assert!(turns < 10_000);
        }
        assert!(compressed_pages(&server) > 1);
        // A new page of the same run: the next busy turn steps it.
        now += Duration::from_millis(100);
        let pane = server.model.panes.get_mut(&p).unwrap();
        fill(&mut pane.term, page_rows);
        pane.compress.observe(&pane.term, now);
        let rest = server.compress.rest_until;
        server.compress_turn(now, false);
        assert!(server.compress.rest_until > rest, "stepped");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The rest is `REST_FACTOR` times the step for none or one starved
    /// pane (the single-pane schedule), divided among more, never below
    /// `REST_FACTOR / MAX_SHARE` times the step, nor `STEP_GAP`.
    #[test]
    fn rest_is_shared_by_starved_panes() {
        let ms = Duration::from_millis;
        let us = Duration::from_micros;
        assert_eq!(rest(ms(1), 0), ms(100));
        assert_eq!(rest(ms(1), 1), ms(100));
        assert_eq!(rest(ms(1), 2), ms(50));
        assert_eq!(rest(ms(1), 20), ms(5));
        assert_eq!(rest(us(250), 20), us(1250));
        assert_eq!(rest(ms(1), MAX_SHARE as usize), ms(4));
        assert_eq!(rest(ms(1), 1000), ms(4), "capped at MAX_SHARE");
        assert_eq!(rest(ms(1), usize::MAX), ms(4));
        assert_eq!(rest(us(5), 20), STEP_GAP);
        assert_eq!(rest(Duration::ZERO, 1), STEP_GAP);
    }

    /// Panes count toward the share once starved, not while merely due,
    /// and every starved pane is stepped in busy turns, round-robin: the
    /// passes of several streaming panes all complete.
    #[test]
    fn starved_panes_share_the_rest() {
        let (mut server, p, _, _, dir) = crate::mode::prompt::tests::setup();
        server.reset_mode_all(p);
        let mut ids = vec![p];
        for name in ["b", "c"] {
            let s = server
                .create_session(crate::server::NewSession {
                    name: Some(name.into()),
                    window_name: Some("w".into()),
                    argv: vec!["cat".into(), "-".into()],
                    cwd: dir.clone(),
                    cols: 20,
                    rows: 5,
                    client_env: Vec::new(),
                    extra_env: Vec::new(),
                    update_env: false,
                    default_size: None,
                    cell_pixels: (0, 0),
                    group: None,
                    control_chars: None,
                })
                .unwrap();
            ids.push(server.model.current_of(s).unwrap().2);
        }
        let t0 = Instant::now();
        for (i, id) in ids.iter().enumerate() {
            let pane = server.model.panes.get_mut(id).unwrap();
            pane.term.set_history_limit(100_000);
            let page_rows = pane.term.compression_progress().1;
            fill(&mut pane.term, 3 * page_rows);
            // The third pane starts its run later: due, not yet starved.
            let at = if i == 2 { t0 + IDLE } else { t0 };
            pane.compress.observe(&pane.term, at);
        }
        assert_eq!(
            server.starved(t0 + STARVE.saturating_sub(Duration::from_millis(1))),
            0
        );
        assert_eq!(server.starved(t0 + STARVE), 2);
        assert_eq!(server.starved(t0 + IDLE + STARVE), 3);
        let mut now = t0 + IDLE + STARVE;
        let mut turns = 0;
        while ids
            .iter()
            .any(|id| server.model.panes[id].compress.due.is_some())
        {
            server.compress_turn(now, false);
            now = server.compress.rest_until.unwrap().max(now);
            turns += 1;
            assert!(turns < 10_000);
        }
        for id in &ids {
            assert!(server.model.panes[id].term.compressed_pages() > 1, "{id:?}");
        }
        assert_eq!(server.starved(now), 0, "passes done: none starved");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn steps_go_round_robin_across_due_panes() {
        let now = Instant::now();
        let later = Some(now + IDLE);
        let due = Some(now);
        let panes = [
            (PaneId(1), due),
            (PaneId(2), later),
            (PaneId(3), due),
            (PaneId(4), None),
            (PaneId(5), due),
        ];
        let next = |last| next_due(panes.iter().copied(), last, now);
        assert_eq!(next(None), Some(PaneId(1)));
        assert_eq!(next(Some(PaneId(1))), Some(PaneId(3)));
        assert_eq!(next(Some(PaneId(3))), Some(PaneId(5)));
        assert_eq!(next(Some(PaneId(5))), Some(PaneId(1)));
        // A pane gone since the last step.
        assert_eq!(next(Some(PaneId(4))), Some(PaneId(5)));
        assert_eq!(next_due(panes[1..2].iter().copied(), None, now), None);
    }
}
