//! Control-client output with backpressure (control.c blocks and offsets,
//! window.c `window_pane_get_new_data`, server-client.c
//! `server_client_check_pane_buffer`).
//!
//! Pane output is not copied per client. Each pane's bytes stay in one
//! [`Streams`] entry until every control client owed them has been sent
//! them; a client keeps, per pane, the byte ranges it is owed
//! ([`Block`]s, with the time each was read) and, in one [`Queue`], the
//! order its lines and pane output go out in. Output is formatted only as
//! the client's connection takes it, a few kilobytes at a time
//! ([`BUFFER_HIGH`]), so a slow reader costs the server the pane bytes it
//! has not been sent, not their escaped copies; and while every client
//! that would get a pane's output is behind, the pane is not read at all
//! ([`BUFFER_LOW`]), so its program blocks on its terminal.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use vtmux_core::ids::PaneId;

/// `CONTROL_BUFFER_LOW`: a client with at least this much unwritten is
/// behind, and a pane whose clients all are is not read.
pub const BUFFER_LOW: usize = 512;
/// `CONTROL_BUFFER_HIGH`: pane output is formatted for a client until it
/// has this much unwritten.
pub const BUFFER_HIGH: usize = 8192;
/// `CONTROL_WRITE_MINIMUM`: the least pane output put in one line.
pub const WRITE_MINIMUM: usize = 32;
/// `CONTROL_MAXIMUM_REPLY_BUFFER`: lines (replies, notifications) held
/// for a client that is not reading, before it is let go.
pub const MAXIMUM_REPLY_BUFFER: usize = 64 * 1024 * 1024;

/// `CONTROL_MAXIMUM_AGE`: a client without `pause-after` whose oldest
/// unsent output is this old is let go ("too far behind"). A debug build
/// takes `VTMUX_CONTROL_MAXIMUM_AGE` (milliseconds) instead, for tests.
#[must_use]
pub fn maximum_age() -> Duration {
    static AGE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *AGE.get_or_init(|| {
        let fake = cfg!(debug_assertions)
            .then(|| std::env::var("VTMUX_CONTROL_MAXIMUM_AGE").ok())
            .flatten()
            .and_then(|v| v.parse().ok());
        Duration::from_millis(fake.unwrap_or(300_000))
    })
}

/// Pane bytes some control client is still owed, by absolute offset in
/// the pane's output since its stream started.
#[derive(Debug, Default)]
pub struct Streams {
    panes: BTreeMap<PaneId, Stream>,
}

#[derive(Debug, Default)]
struct Stream {
    /// Offset of `data[0]`.
    start: u64,
    data: VecDeque<u8>,
}

impl Stream {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

impl Streams {
    /// Keep `bytes` from pane `p`; their offsets.
    pub fn append(&mut self, p: PaneId, bytes: &[u8]) -> (u64, u64) {
        let s = self.panes.entry(p).or_default();
        let start = s.end();
        s.data.extend(bytes);
        (start, s.end())
    }

    /// Up to `len` bytes of pane `p` from offset `from`.
    fn read(&self, p: PaneId, from: u64, len: usize) -> Vec<u8> {
        let Some(s) = self.panes.get(&p) else {
            return Vec::new();
        };
        let skip = usize::try_from(from.saturating_sub(s.start))
            .unwrap_or(usize::MAX)
            .min(s.data.len());
        let end = skip.saturating_add(len).min(s.data.len());
        s.data.range(skip..end).copied().collect()
    }

    /// `server_client_check_pane_buffer`'s drain: keep each pane's bytes
    /// from the lowest offset still owed (`owed`); a pane owed nothing
    /// goes.
    pub fn trim(&mut self, owed: &BTreeMap<PaneId, u64>) {
        self.panes.retain(|p, s| {
            let Some(&from) = owed.get(p) else {
                return false;
            };
            let n = usize::try_from(from.saturating_sub(s.start))
                .unwrap_or(usize::MAX)
                .min(s.data.len());
            s.data.drain(..n);
            s.start += n as u64;
            true
        });
    }

    /// Bytes held, over every pane.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panes.values().map(|s| s.data.len()).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `struct control_block` with a size: pane bytes `[start, end)` read at
/// `read`, `start` moving up as they are sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Block {
    /// Where the block stands in [`Queue::all`].
    seq: u64,
    start: u64,
    end: u64,
    read: Instant,
}

/// What `cs->all_blocks` holds: a line waits behind the pane output
/// queued before it.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Line(Vec<u8>),
    /// A block of the pane's lane that still has bytes to send.
    Output(PaneId),
}

/// A control client's unsent output (`struct control_state`'s blocks).
///
/// Lines and pane output are kept in the order they were queued
/// (`cs->all_blocks`), but pane output is sent as tmux sends it
/// (`control_write_callback`): the panes with output held take turns, each
/// up to its share of the room in a round ([`Self::next_pane`],
/// [`Self::take_output`]); a line goes out as soon as nothing queued before
/// it is left to send ([`Self::take_line`]).
#[derive(Debug, Default)]
pub struct Queue {
    /// `cs->all_blocks`: lines, and the blocks of every pane that still
    /// have bytes to send, by the order they were queued in.
    all: BTreeMap<u64, Entry>,
    next_seq: u64,
    /// Each pane's blocks, oldest first (`cp->blocks`).
    lanes: BTreeMap<PaneId, VecDeque<Block>>,
    /// `cs->pending_list`: the panes with output queued, in the order
    /// their first block was queued. A pane stays in it after its blocks
    /// were discarded, until its turn finds nothing to send.
    pending: VecDeque<PaneId>,
    /// Bytes of the lines held (`cs->queued_reply_bytes`).
    line_bytes: usize,
    /// Where the current `control_write_callback` is.
    round: Round,
}

/// One call of `control_write_callback`: the panes still to be served in
/// the round it is in, and the pane being served.
#[derive(Debug, Default)]
struct Round {
    /// The share of one pane (`limit`), the same for the whole round.
    limit: usize,
    /// The panes yet to have their turn.
    todo: VecDeque<PaneId>,
    /// The pane being served and the bytes it has had this turn (`used`).
    serving: Option<(PaneId, usize)>,
}

/// One thing a queue holds, as a snapshot carries it (D27): a line, or a
/// block of pane output with its bytes and when they were read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    Line(Vec<u8>),
    Output {
        pane: PaneId,
        data: Vec<u8>,
        read: Instant,
    },
}

impl Queue {
    /// Everything held, in the order it was queued, the pane bytes read
    /// from `streams`.
    #[must_use]
    pub fn held(&self, streams: &Streams) -> Vec<Held> {
        let mut at: BTreeMap<PaneId, usize> = BTreeMap::new();
        self.all
            .values()
            .filter_map(|e| match e {
                Entry::Line(line) => Some(Held::Line(line.clone())),
                Entry::Output(p) => {
                    let i = at.entry(*p).or_default();
                    let block = self.lanes.get(p)?.get(*i)?;
                    *i += 1;
                    let len = usize::try_from(block.end - block.start).unwrap_or(usize::MAX);
                    Some(Held::Output {
                        pane: *p,
                        data: streams.read(*p, block.start, len),
                        read: block.read,
                    })
                }
            })
            .collect()
    }

    /// A queue of what [`Self::held`] gave, its pane bytes kept in
    /// `streams` again.
    #[must_use]
    pub fn from_held(held: Vec<Held>, streams: &mut Streams) -> Self {
        let mut q = Self::default();
        for h in held {
            match h {
                Held::Line(line) => q.push_line(line),
                Held::Output { pane, data, read } => {
                    let (start, end) = streams.append(pane, &data);
                    q.push_output(pane, start, end, read);
                }
            }
        }
        q
    }

    /// Whether `cs->all_blocks` is empty: a line may go straight to the
    /// connection.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    /// Bytes of lines held behind pane output.
    #[must_use]
    pub fn line_bytes(&self) -> usize {
        self.line_bytes
    }

    fn push(&mut self, entry: Entry) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.all.insert(seq, entry);
        seq
    }

    /// A line (ending in its newline) behind what is queued.
    pub fn push_line(&mut self, line: Vec<u8>) {
        self.line_bytes += line.len();
        self.push(Entry::Line(line));
    }

    /// Pane `p`'s bytes `[start, end)`, read at `read`: a block of its own,
    /// so its age is its own (`control_check_age` looks at the oldest).
    pub fn push_output(&mut self, p: PaneId, start: u64, end: u64, read: Instant) {
        if start == end {
            return;
        }
        let seq = self.push(Entry::Output(p));
        self.lanes.entry(p).or_default().push_back(Block {
            seq,
            start,
            end,
            read,
        });
        if !self.pending.contains(&p) {
            self.pending.push_back(p);
        }
    }

    /// `control_discard_pane`: pane `p`'s blocks go. The pane stays
    /// pending until its turn comes.
    pub fn discard_pane(&mut self, p: PaneId) {
        if let Some(lane) = self.lanes.remove(&p) {
            for block in lane {
                self.all.remove(&block.seq);
            }
        }
    }

    /// `control_discard`: every pane's output goes; the lines stay.
    pub fn discard_output(&mut self) {
        for p in self.lanes.keys().copied().collect::<Vec<_>>() {
            self.discard_pane(p);
        }
    }

    /// `control_reset_offsets`: every pane's output goes and none is
    /// pending.
    pub fn reset_panes(&mut self) {
        self.discard_output();
        self.pending.clear();
        self.round = Round::default();
    }

    /// `control_discard`, then the lines held, in order, for the exit.
    pub fn take_lines(&mut self) -> Vec<Vec<u8>> {
        self.discard_output();
        self.line_bytes = 0;
        std::mem::take(&mut self.all)
            .into_values()
            .filter_map(|e| match e {
                Entry::Line(line) => Some(line),
                Entry::Output(_) => None,
            })
            .collect()
    }

    /// Everything goes (`control_discard_all`).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The line at the head of `cs->all_blocks`, if the head is a line
    /// (`control_flush_all_blocks`): nothing queued before it is left to
    /// send.
    pub fn take_line(&mut self) -> Option<Vec<u8>> {
        let (&seq, entry) = self.all.first_key_value()?;
        if !matches!(entry, Entry::Line(_)) {
            return None;
        }
        let Some(Entry::Line(line)) = self.all.remove(&seq) else {
            unreachable!("the head is a line");
        };
        self.line_bytes = self.line_bytes.saturating_sub(line.len());
        Some(line)
    }

    /// How many panes are pending (`cs->pending_count`).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The panes with output held and when the oldest of each was read.
    pub fn oldest(&self) -> impl Iterator<Item = (PaneId, Instant)> + '_ {
        self.lanes
            .iter()
            .filter_map(|(&p, lane)| Some((p, lane.front()?.read)))
    }

    /// When pane `p`'s oldest output held was read.
    #[must_use]
    pub fn oldest_of(&self, p: PaneId) -> Option<Instant> {
        Some(self.lanes.get(&p)?.front()?.read)
    }

    /// The lowest offset owed in each pane.
    pub fn owed(&self) -> impl Iterator<Item = (PaneId, u64)> + '_ {
        self.lanes
            .iter()
            .filter_map(|(&p, lane)| Some((p, lane.front()?.start)))
    }

    /// A new `control_write_callback`: the panes go round from the first
    /// pending one again.
    pub fn begin(&mut self) {
        self.round = Round::default();
    }

    /// The pane whose turn it is, for a connection with `unwritten` bytes
    /// unwritten; none when the room is used (`unwritten` at
    /// [`BUFFER_HIGH`], checked as a turn would start) or nothing is
    /// pending. A round gives every pending pane the same share of the
    /// room left when it starts ([`write_limit`]); a new one starts when
    /// the last has been through every pane.
    pub fn next_pane(&mut self, unwritten: usize) -> Option<PaneId> {
        if let Some((p, _)) = self.round.serving {
            return Some(p);
        }
        if unwritten >= BUFFER_HIGH {
            return None;
        }
        if self.round.todo.is_empty() {
            if self.pending.is_empty() {
                return None;
            }
            self.round.limit = write_limit(unwritten, self.pending.len());
            self.round.todo.clone_from(&self.pending);
        }
        let p = self.round.todo.pop_front()?;
        self.round.serving = Some((p, 0));
        Some(p)
    }

    /// The end of the turn of the pane being served
    /// (`control_write_pending` returning): it stays pending while it has
    /// blocks.
    pub fn end_turn(&mut self) {
        let Some((p, _)) = self.round.serving.take() else {
            return;
        };
        if !self.lanes.contains_key(&p) {
            self.pending.retain(|x| *x != p);
        }
    }

    /// Up to the rest of the share of the pane being served
    /// (`control_write_pending`) from its blocks, one after another, and
    /// the age of the oldest block they came from. It stops where a line is
    /// next to go, so the line goes between this and the rest. `None`
    /// ends the turn: the share is used, or the pane has nothing more.
    pub fn take_output(
        &mut self,
        streams: &Streams,
        now: Instant,
    ) -> Option<(PaneId, Vec<u8>, Duration)> {
        let (pane, mut used) = self.round.serving?;
        let limit = self.round.limit;
        let mut data = Vec::new();
        let mut age = None;
        while used != limit {
            let Some(lane) = self.lanes.get_mut(&pane) else {
                break;
            };
            let block = lane.front_mut().expect("a lane has a block");
            age.get_or_insert_with(|| now.saturating_duration_since(block.read));
            let left = usize::try_from(block.end - block.start).unwrap_or(usize::MAX);
            let n = left.min(limit - used);
            data.extend(streams.read(pane, block.start, n));
            block.start += n as u64;
            used += n;
            if block.start == block.end {
                let seq = block.seq;
                lane.pop_front();
                if lane.is_empty() {
                    self.lanes.remove(&pane);
                }
                self.all.remove(&seq);
                // A line now at the head goes out first.
                if matches!(self.all.first_key_value(), Some((_, Entry::Line(_)))) {
                    break;
                }
            }
        }
        self.round.serving = Some((pane, used));
        if data.is_empty() {
            return None;
        }
        Some((pane, data, age.unwrap_or_default()))
    }
}

/// How much pane output goes in the next line for a client with `unwritten`
/// bytes unwritten and `pending` panes owed: the room below
/// [`BUFFER_HIGH`] shared between the panes, a third of it (an escaped
/// byte takes up to four), at least [`WRITE_MINIMUM`] (control.c
/// `control_write_callback`).
#[must_use]
pub fn write_limit(unwritten: usize, pending: usize) -> usize {
    (BUFFER_HIGH.saturating_sub(unwritten) / pending.max(1) / 3).max(WRITE_MINIMUM)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: PaneId = PaneId(1);
    const Q: PaneId = PaneId(2);

    /// What one `control_write_callback` writes with `unwritten` bytes
    /// already held and `room` more taken before it is full: lines
    /// whole, pane output as the (pane, bytes) pieces.
    #[derive(Debug, PartialEq, Eq)]
    enum Sent {
        Line(Vec<u8>),
        /// The pane and its bytes as runs (`p*3 q*1`).
        Output(PaneId, String),
    }

    /// `data` as runs of equal bytes: `[b'a'; 3]` is `a*3`.
    fn runs(data: &[u8]) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let n = data[i..].iter().take_while(|&&b| b == data[i]).count();
            out.push(format!("{}*{n}", char::from(data[i])));
            i += n;
        }
        out.join(" ")
    }

    fn drain(q: &mut Queue, s: &Streams, unwritten: usize) -> Vec<Sent> {
        let now = Instant::now();
        let mut out = Vec::new();
        q.begin();
        loop {
            if let Some(line) = q.take_line() {
                out.push(Sent::Line(line));
                continue;
            }
            if q.next_pane(unwritten).is_none() {
                return out;
            }
            match q.take_output(s, now) {
                Some((p, data, _)) => out.push(Sent::Output(p, runs(&data))),
                None => q.end_turn(),
            }
        }
    }

    fn out(pane: PaneId, data: &[u8]) -> Sent {
        Sent::Output(pane, runs(data))
    }

    fn push(q: &mut Queue, streams: &mut Streams, pane: PaneId, data: &[u8]) {
        let (start, end) = streams.append(pane, data);
        q.push_output(pane, start, end, Instant::now());
    }

    #[test]
    fn a_line_waits_behind_the_output_queued_before_it() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        push(&mut q, &mut s, P, b"hello");
        q.push_line(b"%sessions-changed\n".to_vec());
        push(&mut q, &mut s, P, b"world");
        assert!(q.take_line().is_none());
        assert_eq!(
            drain(&mut q, &s, 0),
            [
                out(P, b"hello"),
                Sent::Line(b"%sessions-changed\n".to_vec()),
                out(P, b"world")
            ]
        );
        assert!(q.is_empty() && q.line_bytes() == 0 && q.pending() == 0);
    }

    /// `control_write_pending`: a pane's share is cut across its blocks,
    /// and a line goes out when the blocks before it are done, in the
    /// middle of the pane's turn (control.c:889-905).
    #[test]
    fn a_share_runs_across_blocks_and_stops_for_a_line() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        push(&mut q, &mut s, P, &[b'a'; 1000]);
        q.push_line(b"l\n".to_vec());
        push(&mut q, &mut s, P, &[b'b'; 2000]);
        // BUFFER_HIGH / 1 pane / 3 = 2730 bytes a turn.
        assert_eq!(
            drain(&mut q, &s, 0),
            [
                out(P, &[b'a'; 1000]),
                Sent::Line(b"l\n".to_vec()),
                out(P, &[b'b'; 1730]),
                // The next round.
                out(P, &[b'b'; 270]),
            ]
        );
    }

    /// `control_write_callback`: the panes with output take turns, each up
    /// to the same share, whatever the order the output was read in; the
    /// lines wait for what was queued before them.
    #[test]
    fn panes_take_turns() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        // 2 panes: BUFFER_HIGH / 2 / 3 = 1365 bytes each a round.
        push(&mut q, &mut s, P, &[b'p'; 3000]);
        push(&mut q, &mut s, Q, &[b'q'; 1000]);
        push(&mut q, &mut s, P, &[b'P'; 10]);
        q.push_line(b"l\n".to_vec());
        assert_eq!(
            drain(&mut q, &s, 0),
            [
                out(P, &[b'p'; 1365]),
                out(Q, &[b'q'; 1000]),
                // Q is through: P has the room of one pane.
                Sent::Output(P, "p*1635 P*10".to_owned()),
                Sent::Line(b"l\n".to_vec()),
            ]
        );
    }

    /// A call stops starting turns once the room is used, and the next
    /// begins at the first pane again.
    #[test]
    fn no_turn_starts_without_room() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        push(&mut q, &mut s, P, b"p");
        assert_eq!(drain(&mut q, &s, BUFFER_HIGH).len(), 0);
        assert_eq!(drain(&mut q, &s, 0), [out(P, b"p")]);
    }

    #[test]
    fn trim_keeps_what_the_slowest_client_is_owed() {
        let mut s = Streams::default();
        let (mut fast, mut slow) = (Queue::default(), Queue::default());
        let now = Instant::now();
        let (a, b) = s.append(P, b"0123456789");
        fast.push_output(P, a, b, now);
        slow.push_output(P, a, b, now);
        drain(&mut fast, &s, 0);
        // Part of the block: 3 bytes, by a share cut short.
        slow.begin();
        slow.next_pane(0).unwrap();
        slow.round.limit = 3;
        assert_eq!(slow.take_output(&s, now).unwrap().1, b"012");
        let mut owed = BTreeMap::new();
        for (p, o) in fast.owed().chain(slow.owed()) {
            let e = owed.entry(p).or_insert(o);
            *e = (*e).min(o);
        }
        s.trim(&owed);
        assert_eq!(s.len(), 7);
        assert_eq!(drain(&mut slow, &s, 0), [out(P, b"3456789")]);
        s.trim(&BTreeMap::new());
        assert!(s.is_empty());
    }

    /// `control_discard_pane` frees the blocks and leaves the pane in the
    /// pending list; its next turn finds nothing and takes it out.
    #[test]
    fn discarding_a_pane_keeps_the_lines_and_other_panes() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        push(&mut q, &mut s, P, b"p");
        q.push_line(b"l\n".to_vec());
        push(&mut q, &mut s, Q, b"q");
        q.discard_pane(P);
        assert_eq!(q.oldest().map(|(p, _)| p).collect::<Vec<_>>(), [Q]);
        assert_eq!(q.pending(), 2);
        assert_eq!(
            drain(&mut q, &s, 0),
            [Sent::Line(b"l\n".to_vec()), out(Q, b"q")]
        );
        assert_eq!(q.pending(), 0);
        push(&mut q, &mut s, Q, b"q");
        q.discard_output();
        assert!(q.is_empty() && q.oldest().next().is_none());
    }

    #[test]
    fn take_lines_drops_the_output_and_keeps_the_lines_in_order() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        q.push_line(b"a\n".to_vec());
        push(&mut q, &mut s, P, b"out");
        q.push_line(b"b\n".to_vec());
        assert_eq!(q.take_lines(), [b"a\n".to_vec(), b"b\n".to_vec()]);
        assert!(q.is_empty() && q.line_bytes() == 0 && q.oldest().next().is_none());
    }

    #[test]
    fn held_is_in_the_order_queued_and_comes_back() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        push(&mut q, &mut s, P, b"one");
        q.push_line(b"l\n".to_vec());
        push(&mut q, &mut s, Q, b"two");
        push(&mut q, &mut s, P, b"three");
        let held = q.held(&s);
        assert_eq!(held.len(), 4);
        assert!(matches!(&held[1], Held::Line(l) if l == b"l\n"));
        assert!(
            matches!(&held[3], Held::Output { pane, data, .. } if *pane == P && data == b"three")
        );
        let mut s2 = Streams::default();
        let mut back = Queue::from_held(held, &mut s2);
        assert_eq!(
            drain(&mut back, &s2, 0),
            [
                out(P, b"one"),
                Sent::Line(b"l\n".to_vec()),
                out(P, b"three"),
                out(Q, b"two"),
            ]
        );
    }

    #[test]
    fn write_limit_is_a_third_of_the_room_with_a_floor() {
        assert_eq!(write_limit(0, 1), BUFFER_HIGH / 3);
        assert_eq!(write_limit(0, 2), BUFFER_HIGH / 6);
        assert_eq!(write_limit(BUFFER_HIGH, 1), WRITE_MINIMUM);
    }
}
