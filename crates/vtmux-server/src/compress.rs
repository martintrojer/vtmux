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
//! flushed, or when its next output is likeliest); after [`STARVED`]
//! without such a turn it runs anyway. A turn woken for a step alone runs only the step
//! and polls again: the rest of a turn costs more than a step (a full
//! render pass, about 1.2 ms for one attached client, against 0.3 ms). After each step the loop rests [`REST_FACTOR`]
//! times as long as the step took (and at least [`STEP_GAP`]), blocked in
//! poll, so output arriving meanwhile is handled at once: a busy pane's
//! output meets a step in progress about one time in `REST_FACTOR + 1`.
//! A pass takes longer, but it only runs while the history is idle.
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
pub const QUIET: Duration = Duration::from_millis(5);

/// How long a due step waits for a quiet turn before it runs anyway.
pub const STARVED: Duration = Duration::from_millis(250);

/// The least rest between two steps: Ghostty's own renderer waits 1 ms
/// before a pending pass's next step (renderer/Thread.zig `step_interval`).
pub const STEP_GAP: Duration = Duration::from_millis(1);

/// One pane's compression schedule.
#[derive(Debug, Default)]
pub struct PaneCompress {
    /// The activity last seen; `None` before the first look.
    activity: Option<CompressionActivity>,
    /// When the next step may run; `None` while there is nothing to do.
    due: Option<Instant>,
    /// Ghostty can't compress on this target: never due again.
    unsupported: bool,
}

impl PaneCompress {
    /// Look at `term` after something that may have changed its history:
    /// a new activity restarts the idle delay from `now`. With no history
    /// to compress the pane is not due at all.
    pub fn observe(&mut self, term: &PaneTerm, now: Instant) {
        let activity = term.compression_activity();
        if self.unsupported || self.activity == Some(activity) {
            return;
        }
        self.activity = Some(activity);
        self.due = term.has_history().then(|| now + IDLE);
    }

    /// When the pane next wants a step.
    #[must_use]
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Run one step if the pane is due at `now`. A finished pass leaves
    /// the pane idle until its activity changes; stepping again would only
    /// start a new pass over the same pages. Unsupported leaves it idle
    /// for good.
    pub fn step(&mut self, term: &mut PaneTerm, now: Instant) -> Option<Compression> {
        if self.due.is_none_or(|d| d > now) {
            return None;
        }
        let result = term.compress_step();
        if result == Compression::Pending {
            self.due = Some(now);
        } else {
            self.due = None;
            self.activity = Some(term.compression_activity());
            self.unsupported = result == Compression::Unsupported;
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

    /// The soonest a pane is due, rest and quiet aside.
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
    /// [`QUIET`], or the step is [`STARVED`], the next due pane after the
    /// last one stepped runs one step.
    pub(crate) fn compress_turn(&mut self, now: Instant, idle: bool) {
        let reads = vtmux_vt::history_reads();
        if reads != self.compress.reads {
            self.compress.reads = reads;
            for p in self.model.panes.values_mut() {
                p.compress.observe(&p.term, now);
            }
        }
        let starved = self.compress_due().is_some_and(|due| now >= due + STARVED);
        if !idle {
            self.compress.busy_at = Some(now);
        }
        if self.compress.rest_until.is_some_and(|r| r > now) && !starved {
            return;
        }
        let quiet = idle && self.compress.busy_at.is_none_or(|b| now >= b + QUIET);
        if !quiet && !starved {
            return;
        }
        let panes = self
            .model
            .panes
            .iter()
            .map(|(&id, p)| (id, p.compress.due()));
        let Some(id) = next_due(panes, self.compress.last, now) else {
            return;
        };
        self.compress.last = Some(id);
        if let Some(p) = self.model.panes.get_mut(&id) {
            let start = Instant::now();
            p.compress.step(&mut p.term, now);
            let took = start.elapsed();
            self.compress.rest_until = Some(now + took + (took * REST_FACTOR).max(STEP_GAP));
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
        assert!(server.model.panes[&p].term.compressed_pages() >= pages);

        // copy-mode copies the history out, restoring every page.
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
        let t1 = now + Duration::from_secs(60);
        server.compress_turn(t1, true);
        assert_eq!(server.compress_deadline(), Some(t1 + IDLE));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A busy turn (one that handled events) doesn't step, nor does an
    /// idle one less than QUIET after it, unless the step waited STARVED.
    #[test]
    fn busy_turns_step_only_when_starved() {
        let (mut server, p, _, _, dir) = crate::mode::prompt::tests::setup();
        server.reset_mode_all(p);
        let t0 = Instant::now();
        let pane = server.model.panes.get_mut(&p).unwrap();
        fill(&mut pane.term, 3000);
        pane.compress.observe(&pane.term, t0);
        let due = t0 + IDLE;
        server.compress_turn(due, false);
        // Idle, but not QUIET since the busy turn.
        server.compress_turn(due + Duration::from_millis(1), true);
        assert_eq!(server.compress_deadline(), Some(due + QUIET));
        server.compress_turn(
            due + STARVED.checked_sub(Duration::from_millis(1)).unwrap(),
            false,
        );
        assert_eq!(server.model.panes[&p].term.compressed_pages(), 0);
        server.compress_turn(due + STARVED, false);
        assert_eq!(server.model.panes[&p].term.compressed_pages(), 1);
        // Read again: due IDLE later; a busy turn then holds the step off
        // until the loop has been QUIET.
        let t1 = due + Duration::from_secs(60);
        let _ = capture(&server.model.panes[&p].term, all());
        server.compress_turn(t1, true);
        assert_eq!(server.compress_due(), Some(t1 + IDLE));
        let t = t1 + IDLE;
        server.compress_turn(t, false);
        let rest = server.compress.rest_until;
        server.compress_turn(t + Duration::from_millis(1), true);
        assert_eq!(server.compress.rest_until, rest, "no step yet");
        server.compress_turn(t + QUIET, true);
        assert!(server.compress.rest_until > rest, "stepped");
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
