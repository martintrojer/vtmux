//! OSC 7501, the Program Status Protocol (spec D34): what libghostty-vt
//! reports when a pane's program says what it is doing. The events reach
//! the server in byte order with the pane's other [`crate::TermEvent`]s;
//! the server keeps the records.

use libghostty_vt::terminal::{ProgramStatusKind, ProgramStatusReport, ProgramStatusState};

/// A program's state, as the protocol names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    /// Remove the record with `id` and every record beneath it.
    Clear,
}

/// What a blocked program needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProgramKind {
    None,
    Permission,
    Question,
    Auth,
}

/// One report, owned. Text is already decoded and free of control
/// characters, but is still untrusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramStatusEvent {
    pub state: ProgramState,
    pub kind: ProgramKind,
    /// 0 to 100; `None` when the program sent none.
    pub progress: Option<u8>,
    /// The record's id; empty for the root record.
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

impl ProgramStatusEvent {
    /// `state=clear` with an empty id: every record goes (RIS).
    #[must_use]
    pub fn clear_all() -> Self {
        Self {
            state: ProgramState::Clear,
            kind: ProgramKind::None,
            progress: None,
            id: String::new(),
            app: String::new(),
            title: String::new(),
            message: String::new(),
        }
    }

    pub(crate) fn from_report(r: &ProgramStatusReport<'_>) -> Self {
        Self {
            state: match r.state {
                ProgramStatusState::Idle => ProgramState::Idle,
                ProgramStatusState::Working => ProgramState::Working,
                ProgramStatusState::Done => ProgramState::Done,
                ProgramStatusState::Blocked => ProgramState::Blocked,
                ProgramStatusState::Error => ProgramState::Error,
                ProgramStatusState::Clear => ProgramState::Clear,
            },
            kind: match r.kind {
                ProgramStatusKind::None => ProgramKind::None,
                ProgramStatusKind::Permission => ProgramKind::Permission,
                ProgramStatusKind::Question => ProgramKind::Question,
                ProgramStatusKind::Auth => ProgramKind::Auth,
            },
            progress: r.progress,
            id: r.id.to_owned(),
            app: r.app.to_owned(),
            title: r.title.to_owned(),
            message: r.message.to_owned(),
        }
    }
}
