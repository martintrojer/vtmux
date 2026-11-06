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
/// `read`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Block {
    start: u64,
    end: u64,
    read: Instant,
}

/// What goes out next, in order (`cs->all_blocks`): a line waits behind
/// the pane output queued before it.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Line(Vec<u8>),
    /// The oldest block of the pane's lane.
    Output(PaneId),
}

/// One piece for the connection, from [`Queue::next`].
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    /// A line, with its newline.
    Line(Vec<u8>),
    /// Raw pane bytes and the age of the oldest block they came from.
    Output {
        pane: PaneId,
        data: Vec<u8>,
        age: Duration,
    },
}

/// A control client's unsent output (`struct control_state`'s blocks).
#[derive(Debug, Default)]
pub struct Queue {
    entries: VecDeque<Entry>,
    /// Each pane's blocks, oldest first (`cp->blocks`).
    lanes: BTreeMap<PaneId, VecDeque<Block>>,
    /// Bytes of the lines held (`cs->queued_reply_bytes`).
    line_bytes: usize,
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
    /// Everything held, in the order it goes out, the pane bytes read
    /// from `streams`.
    #[must_use]
    pub fn held(&self, streams: &Streams) -> Vec<Held> {
        let mut at: BTreeMap<PaneId, usize> = BTreeMap::new();
        self.entries
            .iter()
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

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of lines held behind pane output.
    #[must_use]
    pub fn line_bytes(&self) -> usize {
        self.line_bytes
    }

    /// Whether any pane output is held: a line must wait behind it.
    #[must_use]
    pub fn has_output(&self) -> bool {
        !self.lanes.is_empty()
    }

    /// A line (ending in its newline) behind what is queued.
    pub fn push_line(&mut self, line: Vec<u8>) {
        self.line_bytes += line.len();
        self.entries.push_back(Entry::Line(line));
    }

    /// Pane `p`'s bytes `[start, end)`, read at `read`: a block of its own,
    /// so its age is its own (`control_check_age` looks at the oldest).
    pub fn push_output(&mut self, p: PaneId, start: u64, end: u64, read: Instant) {
        if start == end {
            return;
        }
        let lane = self.lanes.entry(p).or_default();
        lane.push_back(Block { start, end, read });
        self.entries.push_back(Entry::Output(p));
    }

    /// `control_discard_pane`: pane `p`'s output goes.
    pub fn discard_pane(&mut self, p: PaneId) {
        if self.lanes.remove(&p).is_some() {
            self.entries.retain(|e| *e != Entry::Output(p));
        }
    }

    /// `control_discard`: every pane's output goes; the lines stay.
    pub fn discard_output(&mut self) {
        self.lanes.clear();
        self.entries.retain(|e| matches!(e, Entry::Line(_)));
    }

    /// `control_discard`, then the lines held, in order, for the exit.
    pub fn take_lines(&mut self) -> Vec<u8> {
        self.discard_output();
        self.line_bytes = 0;
        self.entries
            .drain(..)
            .flat_map(|e| match e {
                Entry::Line(line) => line,
                Entry::Output(_) => Vec::new(),
            })
            .collect()
    }

    /// Everything goes (`control_discard_all`).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The pane whose output goes next, if output is next.
    #[must_use]
    pub fn front_output(&self) -> Option<PaneId> {
        match self.entries.front()? {
            Entry::Output(p) => Some(*p),
            Entry::Line(_) => None,
        }
    }

    /// How many panes have output held (`cs->pending_count`).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.lanes.len()
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

    /// The next piece (`control_write_pending`): a line, or up to `limit`
    /// bytes of the pane whose output is first, across its blocks queued
    /// one after another.
    pub fn next(&mut self, limit: usize, streams: &Streams, now: Instant) -> Option<Piece> {
        match self.entries.front()? {
            Entry::Line(_) => {
                let Some(Entry::Line(line)) = self.entries.pop_front() else {
                    unreachable!("front is a line");
                };
                self.line_bytes -= line.len();
                Some(Piece::Line(line))
            }
            &Entry::Output(pane) => {
                let mut data = Vec::new();
                let mut age = None;
                while data.len() < limit && self.entries.front() == Some(&Entry::Output(pane)) {
                    let lane = self.lanes.get_mut(&pane).expect("an entry has its lane");
                    let block = lane.front_mut().expect("an entry has its block");
                    age.get_or_insert_with(|| now.saturating_duration_since(block.read));
                    let left = usize::try_from(block.end - block.start).unwrap_or(usize::MAX);
                    let n = left.min(limit - data.len());
                    data.extend(streams.read(pane, block.start, n));
                    block.start += n as u64;
                    if block.start == block.end {
                        lane.pop_front();
                        if lane.is_empty() {
                            self.lanes.remove(&pane);
                        }
                        self.entries.pop_front();
                    }
                }
                Some(Piece::Output {
                    pane,
                    data,
                    age: age.unwrap_or_default(),
                })
            }
        }
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

    fn drain(q: &mut Queue, s: &Streams, limit: usize) -> Vec<Piece> {
        let now = Instant::now();
        std::iter::from_fn(|| q.next(limit, s, now)).collect()
    }

    fn out(pane: PaneId, data: &[u8]) -> (PaneId, Vec<u8>) {
        (pane, data.to_vec())
    }

    fn outputs(pieces: &[Piece]) -> Vec<(PaneId, Vec<u8>)> {
        pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Output { pane, data, .. } => Some((*pane, data.clone())),
                Piece::Line(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_line_waits_behind_the_output_queued_before_it() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        let now = Instant::now();
        let (a, b) = s.append(P, b"hello");
        q.push_output(P, a, b, now);
        q.push_line(b"%sessions-changed\n".to_vec());
        let (a, b) = s.append(P, b"world");
        q.push_output(P, a, b, now);
        assert!(q.has_output());
        let pieces = drain(&mut q, &s, 100);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[1], Piece::Line(b"%sessions-changed\n".to_vec()));
        assert_eq!(outputs(&pieces), [out(P, b"hello"), out(P, b"world")]);
        assert!(q.is_empty() && q.line_bytes() == 0);
    }

    #[test]
    fn output_is_cut_at_the_limit_across_a_pane_s_blocks() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        let now = Instant::now();
        for chunk in [&b"abcdef"[..], b"ghij"] {
            let (a, b) = s.append(P, chunk);
            q.push_output(P, a, b, now);
        }
        let (a, b) = s.append(Q, b"xy");
        q.push_output(Q, a, b, now);
        let pieces = drain(&mut q, &s, 4);
        assert_eq!(
            outputs(&pieces),
            [
                out(P, b"abcd"),
                out(P, b"efgh"),
                out(P, b"ij"),
                out(Q, b"xy")
            ]
        );
    }

    #[test]
    fn trim_keeps_what_the_slowest_client_is_owed() {
        let mut s = Streams::default();
        let (mut fast, mut slow) = (Queue::default(), Queue::default());
        let now = Instant::now();
        let (a, b) = s.append(P, b"0123456789");
        fast.push_output(P, a, b, now);
        slow.push_output(P, a, b, now);
        drain(&mut fast, &s, 100);
        let first = slow.next(3, &s, now).unwrap();
        assert_eq!(outputs(&[first]), [out(P, b"012")]);
        let mut owed = BTreeMap::new();
        for (p, o) in fast.owed().chain(slow.owed()) {
            let e = owed.entry(p).or_insert(o);
            *e = (*e).min(o);
        }
        s.trim(&owed);
        assert_eq!(s.len(), 7);
        assert_eq!(outputs(&drain(&mut slow, &s, 100)), [out(P, b"3456789")]);
        s.trim(&BTreeMap::new());
        assert!(s.is_empty());
    }

    #[test]
    fn discarding_a_pane_keeps_the_lines_and_other_panes() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        let now = Instant::now();
        let (a, b) = s.append(P, b"p");
        q.push_output(P, a, b, now);
        q.push_line(b"l\n".to_vec());
        let (a, b) = s.append(Q, b"q");
        q.push_output(Q, a, b, now);
        q.discard_pane(P);
        assert_eq!(q.oldest().map(|(p, _)| p).collect::<Vec<_>>(), [Q]);
        q.discard_output();
        assert!(!q.has_output());
        assert_eq!(drain(&mut q, &s, 100), [Piece::Line(b"l\n".to_vec())]);
    }

    #[test]
    fn take_lines_drops_the_output_and_keeps_the_lines_in_order() {
        let mut s = Streams::default();
        let mut q = Queue::default();
        let now = Instant::now();
        q.push_line(b"a\n".to_vec());
        let (a, b) = s.append(P, b"out");
        q.push_output(P, a, b, now);
        q.push_line(b"b\n".to_vec());
        assert_eq!(q.take_lines(), b"a\nb\n");
        assert!(q.is_empty() && q.line_bytes() == 0 && q.pending() == 0);
    }

    #[test]
    fn write_limit_is_a_third_of_the_room_with_a_floor() {
        assert_eq!(write_limit(0, 1), BUFFER_HIGH / 3);
        assert_eq!(write_limit(0, 2), BUFFER_HIGH / 6);
        assert_eq!(write_limit(BUFFER_HIGH, 1), WRITE_MINIMUM);
    }
}
