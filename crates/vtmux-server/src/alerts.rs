//! Window alerts (alerts.c): bell, activity and silence monitoring.
//!
//! tmux queues a window when its pane prints output or a BEL, or when the
//! silence timer fires, and checks the queue once per event-loop turn
//! (`alerts_callback`). A check sets the per-session winlink flags that
//! `#`, `!` and `~` in `#{window_flags}` show, and tells the server which
//! sessions need a bell or a status message (`alerts_set_message`).
//!
//! The state is in the [`Model`]; the pieces that need clients (the BEL,
//! the message, the `alert-*` hooks) are returned as [`Alert`]s and done
//! by the server.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use vtmux_core::ids::{SessionId, WindowId};
use vtmux_core::options::TreeId;

use crate::model::Model;

/// `WINDOW_BELL`/`WINLINK_BELL` and friends (tmux.h:1489-1495).
pub const BELL: u8 = 0x1;
pub const ACTIVITY: u8 = 0x2;
pub const SILENCE: u8 = 0x4;
pub const ALL: u8 = BELL | ACTIVITY | SILENCE;

/// A window's alert state (`w->flags & WINDOW_ALERTFLAGS`,
/// `w->alerts_queued`, `w->alerts_timer`).
#[derive(Debug, Default)]
pub struct WindowAlerts {
    pub flags: u8,
    queued: bool,
    /// When the silence timer fires.
    deadline: Option<Instant>,
}

/// Which alert fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bell,
    Activity,
    Silence,
}

impl Kind {
    const fn flag(self) -> u8 {
        match self {
            Kind::Bell => BELL,
            Kind::Activity => ACTIVITY,
            Kind::Silence => SILENCE,
        }
    }

    #[must_use]
    pub const fn hook(self) -> &'static str {
        match self {
            Kind::Bell => "alert-bell",
            Kind::Activity => "alert-activity",
            Kind::Silence => "alert-silence",
        }
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Kind::Bell => "Bell",
            Kind::Activity => "Activity",
            Kind::Silence => "Silence",
        }
    }

    const fn monitor(self) -> &'static str {
        match self {
            Kind::Bell => "monitor-bell",
            Kind::Activity => "monitor-activity",
            Kind::Silence => "monitor-silence",
        }
    }

    const fn action(self) -> &'static str {
        match self {
            Kind::Bell => "bell-action",
            Kind::Activity => "activity-action",
            Kind::Silence => "silence-action",
        }
    }

    #[must_use]
    pub const fn visual(self) -> &'static str {
        match self {
            Kind::Bell => "visual-bell",
            Kind::Activity => "visual-activity",
            Kind::Silence => "visual-silence",
        }
    }
}

/// One winlink an alert applies to (its `*-action` allows it): the server
/// fires the alert hook, and when `message` is set (the first such winlink
/// of its session, `SESSION_ALERTED`) tells the session's clients.
#[derive(Clone, Copy, Debug)]
pub struct Alert {
    pub kind: Kind,
    pub session: SessionId,
    pub index: u32,
    pub window: WindowId,
    pub message: bool,
}

impl Model {
    /// `alerts_reset`: the silence timer starts over and the silence flag
    /// goes.
    fn alerts_reset(&mut self, w: WindowId, now: Instant) {
        let secs = self.number_option(TreeId::Window(w), "monitor-silence");
        let Some(window) = self.windows.get_mut(&w) else {
            return;
        };
        window.alerts.flags &= !SILENCE;
        window.alerts.deadline = u64::try_from(secs)
            .ok()
            .filter(|s| *s != 0)
            .map(|s| now + Duration::from_secs(s));
    }

    /// `alerts_reset_all`, after `monitor-silence` changed.
    pub fn alerts_reset_all(&mut self) {
        let now = Instant::now();
        let all: Vec<_> = self.windows.keys().copied().collect();
        for w in all {
            self.alerts_reset(w, now);
        }
    }

    /// `alerts_enabled`: is any of `flags` monitored in window `w`.
    fn alerts_enabled(&self, w: WindowId, flags: u8) -> bool {
        [Kind::Bell, Kind::Activity, Kind::Silence]
            .into_iter()
            .any(|k| {
                flags & k.flag() != 0 && self.number_option(TreeId::Window(w), k.monitor()) != 0
            })
    }

    /// `alerts_queue`: note `flags` on window `w` and, when monitored,
    /// queue it for the next check.
    pub fn alerts_queue(&mut self, w: WindowId, flags: u8) {
        self.alerts_reset(w, Instant::now());
        let enabled = self.alerts_enabled(w, flags);
        let Some(window) = self.windows.get_mut(&w) else {
            return;
        };
        window.alerts.flags |= flags;
        if enabled && !window.alerts.queued {
            window.alerts.queued = true;
            self.alerts_list.push(w);
        }
    }

    /// When the next silence timer fires.
    #[must_use]
    pub fn alerts_deadline(&self) -> Option<Instant> {
        self.windows
            .values()
            .filter_map(|w| w.alerts.deadline)
            .min()
    }

    /// `alerts_timer` for every window whose timer has run out.
    pub fn alerts_timers(&mut self, now: Instant) {
        let due: Vec<_> = self
            .windows
            .iter()
            .filter(|(_, w)| w.alerts.deadline.is_some_and(|d| d <= now))
            .map(|(id, _)| *id)
            .collect();
        for w in due {
            self.alerts_queue(w, SILENCE);
        }
    }

    /// `alerts_action_applies`: `{bell,activity,silence}-action` of the
    /// winlink's session.
    fn alerts_action_applies(&self, s: SessionId, index: u32, kind: Kind) -> bool {
        let Some(session) = self.sessions.get(&s) else {
            return false;
        };
        match self.number_option(TreeId::Session(s), kind.action()) {
            1 => true,
            2 => session.current == index,
            3 => session.current != index,
            _ => false,
        }
    }

    /// `alerts_callback`: check every queued window.
    pub fn alerts_run(&mut self, attached: &BTreeSet<SessionId>) -> Vec<Alert> {
        let mut out = Vec::new();
        for w in std::mem::take(&mut self.alerts_list) {
            out.extend(self.alerts_check_all(w, attached));
            if let Some(window) = self.windows.get_mut(&w) {
                window.alerts.queued = false;
                window.alerts.flags &= !ALL;
            }
        }
        out
    }

    /// `alerts_check_session`, when a client attaches to `s`.
    pub fn alerts_check_session(
        &mut self,
        s: SessionId,
        attached: &BTreeSet<SessionId>,
    ) -> Vec<Alert> {
        let windows: Vec<_> = self
            .sessions
            .get(&s)
            .map(|x| x.windows.values().copied().collect())
            .unwrap_or_default();
        windows
            .into_iter()
            .flat_map(|w| self.alerts_check_all(w, attached))
            .collect()
    }

    fn alerts_check_all(&mut self, w: WindowId, attached: &BTreeSet<SessionId>) -> Vec<Alert> {
        let mut out = Vec::new();
        for kind in [Kind::Bell, Kind::Activity, Kind::Silence] {
            self.alerts_check(w, kind, attached, &mut out);
        }
        out
    }

    /// `alerts_check_bell`, `alerts_check_activity` and
    /// `alerts_check_silence`: they differ in that a bell may alert again
    /// while the winlink still has its bell flag.
    fn alerts_check(
        &mut self,
        w: WindowId,
        kind: Kind,
        attached: &BTreeSet<SessionId>,
        out: &mut Vec<Alert>,
    ) {
        let Some(window) = self.windows.get(&w) else {
            return;
        };
        if window.alerts.flags & kind.flag() == 0 {
            return;
        }
        let monitor = self.number_option(TreeId::Window(w), kind.monitor());
        if monitor == 0 {
            return;
        }
        let links: Vec<(SessionId, u32)> = self
            .sessions
            .values()
            .flat_map(|s| {
                s.windows
                    .iter()
                    .filter(|(_, x)| **x == w)
                    .map(|(i, _)| (s.id, *i))
            })
            .collect();
        let mut alerted = BTreeSet::new();
        for (s, index) in links {
            let session = self.sessions.get_mut(&s).expect("linked session");
            let flags = session.link_alerts.entry(index).or_default();
            if kind != Kind::Bell && *flags & kind.flag() != 0 {
                continue;
            }
            if session.current != index || !attached.contains(&s) {
                *flags |= kind.flag();
            }
            if !self.alerts_action_applies(s, index, kind) {
                continue;
            }
            out.push(Alert {
                kind,
                session: s,
                index,
                window: w,
                message: alerted.insert(s),
            });
        }
    }

    /// `winlink_clear_flags` (window.c:2466): the window and every link to
    /// it lose their alert flags.
    pub fn alerts_clear_window(&mut self, w: WindowId) {
        if let Some(window) = self.windows.get_mut(&w) {
            window.alerts.flags &= !ALL;
        }
        for s in self.sessions.values_mut() {
            let indices: Vec<u32> = s
                .windows
                .iter()
                .filter(|(_, x)| **x == w)
                .map(|(i, _)| *i)
                .collect();
            for i in indices {
                s.link_alerts.remove(&i);
            }
        }
    }

    /// Clear the alert flags of every window of session `s`
    /// (`kill-session -C`).
    pub fn alerts_clear_session(&mut self, s: SessionId) {
        let windows: Vec<_> = self
            .sessions
            .get(&s)
            .map(|x| x.windows.values().copied().collect())
            .unwrap_or_default();
        for w in windows {
            if let Some(window) = self.windows.get_mut(&w) {
                window.alerts.flags &= !ALL;
            }
        }
        if let Some(session) = self.sessions.get_mut(&s) {
            session.link_alerts.clear();
        }
    }
}
