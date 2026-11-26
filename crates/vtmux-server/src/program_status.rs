//! OSC 7501 program status records (spec D34): what a pane's programs
//! reported, kept by `id`. The events arrive in byte order from
//! [`vtmux_vt::TermEvent::ProgramStatus`]; this is the server's store and
//! the rules that end a record.

use std::collections::BTreeMap;

use vtmux_vt::{ProgramKind, ProgramState, ProgramStatusEvent};

/// Records a pane keeps; the least recently updated goes first.
pub const MAX_RECORDS: usize = 256;

/// One program's last report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub state: ProgramState,
    pub kind: ProgramKind,
    pub progress: Option<u8>,
    pub app: String,
    pub title: String,
    pub message: String,
    /// Update order: the LRU and the tie-break.
    pub seq: u64,
}

/// What the formats show: the winning record, its `app` inherited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub state: ProgramState,
    pub kind: ProgramKind,
    pub progress: Option<u8>,
    pub app: String,
    pub title: String,
    pub message: String,
}

/// What a change did to the summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    /// Anything the formats show.
    pub any: bool,
    /// The state itself (the hook fires).
    pub state: bool,
}

/// A pane's records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramStatus {
    records: BTreeMap<String, Record>,
    next_seq: u64,
    /// Unix seconds when the summary state last changed; none without a
    /// summary.
    state_time: Option<u64>,
    /// The pane was focused at the last look, so a focus gain is an edge.
    pub focused: bool,
}

#[must_use]
pub fn state_name(s: ProgramState) -> &'static str {
    match s {
        ProgramState::Idle => "idle",
        ProgramState::Working => "working",
        ProgramState::Done => "done",
        ProgramState::Blocked => "blocked",
        ProgramState::Error => "error",
        ProgramState::Clear => "clear",
    }
}

#[must_use]
pub fn kind_name(k: ProgramKind) -> &'static str {
    match k {
        ProgramKind::None => "none",
        ProgramKind::Permission => "permission",
        ProgramKind::Question => "question",
        ProgramKind::Auth => "auth",
    }
}

#[must_use]
pub fn state_code(s: ProgramState) -> u8 {
    match s {
        ProgramState::Idle => 0,
        ProgramState::Working => 1,
        ProgramState::Done => 2,
        ProgramState::Blocked => 3,
        ProgramState::Error => 4,
        ProgramState::Clear => 5,
    }
}

#[must_use]
pub fn state_from_code(c: u8) -> Option<ProgramState> {
    Some(match c {
        0 => ProgramState::Idle,
        1 => ProgramState::Working,
        2 => ProgramState::Done,
        3 => ProgramState::Blocked,
        4 => ProgramState::Error,
        _ => return None,
    })
}

#[must_use]
pub fn kind_code(k: ProgramKind) -> u8 {
    match k {
        ProgramKind::None => 0,
        ProgramKind::Permission => 1,
        ProgramKind::Question => 2,
        ProgramKind::Auth => 3,
    }
}

#[must_use]
pub fn kind_from_code(c: u8) -> ProgramKind {
    match c {
        1 => ProgramKind::Permission,
        2 => ProgramKind::Question,
        3 => ProgramKind::Auth,
        _ => ProgramKind::None,
    }
}

fn priority(s: ProgramState) -> u8 {
    match s {
        ProgramState::Blocked => 5,
        ProgramState::Error => 4,
        ProgramState::Working => 3,
        ProgramState::Done => 2,
        ProgramState::Idle | ProgramState::Clear => 1,
    }
}

/// The state that wins among `states` (`blocked > error > working > done >
/// idle`), none for no states.
#[must_use]
pub fn rollup(states: impl IntoIterator<Item = ProgramState>) -> Option<ProgramState> {
    states.into_iter().max_by_key(|s| priority(*s))
}

/// Control and invisible formatting characters (bidi controls, zero-width
/// and joiners, the BOM) dropped from text that reaches formats.
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
        )
}

/// `text` without invisible characters.
#[must_use]
pub fn sanitize(text: &str) -> String {
    text.chars().filter(|c| !invisible(*c)).collect()
}

fn is_under(id: &str, prefix: &str) -> bool {
    id == prefix
        || id
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

impl ProgramStatus {
    /// Rebuilt from a snapshot.
    #[must_use]
    pub fn restored(
        next_seq: u64,
        state_time: Option<u64>,
        records: impl IntoIterator<Item = (String, Record)>,
    ) -> Self {
        let records: BTreeMap<_, _> = records.into_iter().collect();
        let next_seq = records.values().map(|r| r.seq).fold(next_seq, u64::max);
        Self {
            state_time: if records.is_empty() { None } else { state_time },
            records,
            next_seq,
            focused: false,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The records, by id.
    pub fn records(&self) -> impl Iterator<Item = (&String, &Record)> {
        self.records.iter()
    }

    /// The summary's state alone.
    #[must_use]
    pub fn summary_state(&self) -> Option<ProgramState> {
        self.winner().map(|(_, r)| r.state)
    }

    /// The winning record's `id`.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.winner().map(|(id, _)| id.as_str())
    }

    /// The root record's (`id=""`) state alone.
    #[must_use]
    pub fn root_state(&self) -> Option<ProgramState> {
        self.records.get("").map(|r| r.state)
    }

    /// Unix seconds when the summary state last changed.
    #[must_use]
    pub fn state_time(&self) -> Option<u64> {
        self.state_time
    }

    /// Note a change of the summary state at `now` (none while there is
    /// no summary). Call when [`Changed::state`] is set.
    pub fn stamp(&mut self, now: u64) {
        self.state_time = (!self.records.is_empty()).then_some(now);
    }

    fn winner(&self) -> Option<(&String, &Record)> {
        self.records
            .iter()
            .max_by_key(|(_, r)| (priority(r.state), r.seq))
    }

    /// The nearest non-empty `app`: the record's own, its ancestors', the
    /// root's.
    fn app_of(&self, id: &str, own: &Record) -> String {
        if !own.app.is_empty() {
            return own.app.clone();
        }
        let mut cur = id;
        while !cur.is_empty() {
            cur = cur.rfind('/').map_or("", |i| &cur[..i]);
            if let Some(r) = self.records.get(cur)
                && !r.app.is_empty()
            {
                return r.app.clone();
            }
        }
        String::new()
    }

    /// The record the formats show.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        let (id, r) = self.winner()?;
        Some(Summary {
            state: r.state,
            kind: r.kind,
            progress: r.progress,
            app: self.app_of(id, r),
            title: r.title.clone(),
            message: r.message.clone(),
        })
    }

    fn change(&mut self, f: impl FnOnce(&mut Self)) -> Changed {
        let before = self.summary();
        f(self);
        let after = self.summary();
        Changed {
            any: before != after,
            state: before.as_ref().map(|s| s.state) != after.as_ref().map(|s| s.state),
        }
    }

    /// One report, in order.
    pub fn apply(&mut self, ev: &ProgramStatusEvent) -> Changed {
        self.change(|s| {
            if ev.state == ProgramState::Clear {
                if ev.id.is_empty() {
                    s.records.clear();
                } else {
                    s.records.retain(|id, _| !is_under(id, &ev.id));
                }
                return;
            }
            if !s.records.contains_key(&ev.id) && s.records.len() >= MAX_RECORDS {
                let oldest = s
                    .records
                    .iter()
                    .min_by_key(|(_, r)| r.seq)
                    .map(|(id, _)| id.clone());
                if let Some(id) = oldest {
                    s.records.remove(&id);
                }
            }
            s.next_seq += 1;
            s.records.insert(
                ev.id.clone(),
                Record {
                    state: ev.state,
                    kind: ev.kind,
                    progress: ev.progress,
                    app: sanitize(&ev.app),
                    title: sanitize(&ev.title),
                    message: sanitize(&ev.message),
                    seq: s.next_seq,
                },
            );
        })
    }

    fn drop_states(&mut self, gone: &[ProgramState]) -> Changed {
        self.change(|s| s.records.retain(|_, r| !gone.contains(&r.state)))
    }

    /// `OSC 133;A`: the program is back at its prompt.
    pub fn on_prompt_start(&mut self) -> Changed {
        self.drop_states(&[
            ProgramState::Working,
            ProgramState::Blocked,
            ProgramState::Idle,
        ])
    }

    /// The pane's process ended.
    pub fn on_process_exit(&mut self) -> Changed {
        self.on_prompt_start()
    }

    /// The pane was focused: `done` and `error` are seen.
    pub fn on_focus(&mut self) -> Changed {
        self.drop_states(&[ProgramState::Done, ProgramState::Error])
    }

    /// A respawn: nothing of the old program stays.
    pub fn reset(&mut self) -> Changed {
        let focused = self.focused;
        let c = self.change(|s| *s = Self::default());
        self.focused = focused;
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, state: ProgramState) -> ProgramStatusEvent {
        ProgramStatusEvent {
            state,
            kind: ProgramKind::None,
            progress: None,
            id: id.into(),
            app: String::new(),
            title: String::new(),
            message: String::new(),
        }
    }

    #[test]
    fn report_replaces_and_clear_removes_the_subtree() {
        let mut s = ProgramStatus::default();
        let mut e = ev("a", ProgramState::Working);
        e.title = "one".into();
        s.apply(&e);
        e.title = "two".into();
        s.apply(&e);
        assert_eq!(s.records().count(), 1);
        assert_eq!(s.summary().unwrap().title, "two");
        s.apply(&ev("a/b", ProgramState::Idle));
        s.apply(&ev("ab", ProgramState::Idle));
        let c = s.apply(&ev("a", ProgramState::Clear));
        assert!(c.any);
        assert_eq!(
            s.records().map(|(i, _)| i.as_str()).collect::<Vec<_>>(),
            ["ab"]
        );
        s.apply(&ev("", ProgramState::Clear));
        assert!(s.is_empty());
    }

    #[test]
    fn lru_evicts_the_least_recently_updated() {
        let mut s = ProgramStatus::default();
        for i in 0..MAX_RECORDS {
            s.apply(&ev(&i.to_string(), ProgramState::Idle));
        }
        s.apply(&ev("0", ProgramState::Idle));
        s.apply(&ev("new", ProgramState::Idle));
        assert_eq!(s.records().count(), MAX_RECORDS);
        assert!(s.records().any(|(i, _)| i == "0"));
        assert!(!s.records().any(|(i, _)| i == "1"));
    }

    #[test]
    fn summary_priority_ties_and_app_inheritance() {
        let mut s = ProgramStatus::default();
        let mut root = ev("", ProgramState::Idle);
        root.app = "claude".into();
        s.apply(&root);
        s.apply(&ev("x", ProgramState::Done));
        s.apply(&ev("y", ProgramState::Done));
        s.apply(&ev("w", ProgramState::Working));
        assert_eq!(s.summary().unwrap().state, ProgramState::Working);
        s.apply(&ev("x/q", ProgramState::Error));
        s.apply(&ev("b", ProgramState::Blocked));
        let sum = s.summary().unwrap();
        assert_eq!(sum.state, ProgramState::Blocked);
        assert_eq!(sum.app, "claude");
        s.on_prompt_start();
        // done x, done y, error x/q remain; error wins.
        assert_eq!(s.summary().unwrap().state, ProgramState::Error);
        s.on_focus();
        assert!(s.is_empty());
    }

    #[test]
    fn id_root_state_stamp_and_rollup() {
        let mut s = ProgramStatus::default();
        assert!(s.id().is_none() && s.root_state().is_none() && s.state_time().is_none());
        assert!(s.apply(&ev("", ProgramState::Idle)).state);
        s.stamp(10);
        assert_eq!(s.state_time(), Some(10));
        let c = s.apply(&ev("build", ProgramState::Working));
        assert!(c.state);
        s.stamp(11);
        assert_eq!(s.id(), Some("build"));
        assert_eq!(s.root_state(), Some(ProgramState::Idle));
        let mut e = ev("build", ProgramState::Working);
        e.progress = Some(40);
        assert!(!s.apply(&e).state);
        s.apply(&ev("", ProgramState::Clear));
        s.stamp(12);
        assert_eq!(s.state_time(), None);
        assert_eq!(
            rollup([
                ProgramState::Done,
                ProgramState::Blocked,
                ProgramState::Idle
            ]),
            Some(ProgramState::Blocked)
        );
        assert_eq!(rollup([]), None);
    }

    #[test]
    fn ties_go_to_the_latest() {
        let mut s = ProgramStatus::default();
        let mut a = ev("a", ProgramState::Done);
        a.title = "a".into();
        let mut b = ev("b", ProgramState::Done);
        b.title = "b".into();
        s.apply(&a);
        s.apply(&b);
        assert_eq!(s.summary().unwrap().title, "b");
        s.apply(&a);
        assert_eq!(s.summary().unwrap().title, "a");
    }

    #[test]
    fn exit_clears_what_a_prompt_does() {
        let mut s = ProgramStatus::default();
        s.apply(&ev("a", ProgramState::Working));
        s.apply(&ev("b", ProgramState::Done));
        let c = s.on_process_exit();
        assert!(c.state);
        assert_eq!(s.summary().unwrap().state, ProgramState::Done);
    }

    #[test]
    fn text_loses_invisible_characters() {
        let mut s = ProgramStatus::default();
        let mut e = ev("a", ProgramState::Working);
        e.title = "a\u{202E}b\u{200B}c\u{7}d".into();
        e.message = "\u{2066}ok\u{FEFF}".into();
        s.apply(&e);
        let sum = s.summary().unwrap();
        assert_eq!((sum.title.as_str(), sum.message.as_str()), ("abcd", "ok"));
    }
}
