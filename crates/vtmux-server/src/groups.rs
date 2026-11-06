//! Session groups (session.c `session_group_*`): sessions that share one
//! window list. Each member keeps its own current window and last-window
//! stack; whenever one member's windows change, every other member's
//! winlinks are made again from it (`session_group_synchronize1`), which
//! fires `window-linked` for each and `window-unlinked` for the ones that
//! went, as tmux does.
//!
//! Groups are kept by name (tmux's `session_groups` RB tree); members in
//! the order they joined (`sg->sessions`, a tail queue).

use std::collections::BTreeMap;

use vtmux_core::ids::{SessionId, WindowId};

use crate::events::Event;
use crate::model::{Model, Redraw};
use crate::target::Found;

/// Every session group: name -> members in the order they joined.
#[derive(Debug, Default)]
pub struct Groups {
    by_name: BTreeMap<String, Vec<SessionId>>,
}

impl Groups {
    /// Every group and its members in order, by name.
    pub fn all(&self) -> impl Iterator<Item = (&str, &[SessionId])> {
        self.by_name.iter().map(|(n, m)| (n.as_str(), m.as_slice()))
    }

    /// Groups from [`Self::all`] (a snapshot's, D27).
    #[must_use]
    pub fn from_parts(groups: impl IntoIterator<Item = (String, Vec<SessionId>)>) -> Self {
        Self {
            by_name: groups.into_iter().collect(),
        }
    }

    /// `session_group_contains`: the name of `s`'s group.
    #[must_use]
    pub fn name_of(&self, s: SessionId) -> Option<&str> {
        self.by_name
            .iter()
            .find(|(_, m)| m.contains(&s))
            .map(|(n, _)| n.as_str())
    }

    /// The members of `s`'s group, `None` when it has none.
    #[must_use]
    pub fn members_of(&self, s: SessionId) -> Option<&[SessionId]> {
        self.by_name
            .values()
            .find(|m| m.contains(&s))
            .map(Vec::as_slice)
    }

    /// The members of the group called `name` (`session_group_find`).
    #[must_use]
    pub fn named(&self, name: &str) -> Option<&[SessionId]> {
        self.by_name.get(name).map(Vec::as_slice)
    }

    /// Each group's first member (format.c
    /// `format_cb_window_linked_sessions`).
    pub fn firsts(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.by_name.values().filter_map(|m| m.first().copied())
    }

    /// `s` itself, or every member of its group (the `*_session_group`
    /// helpers of server-fn.c).
    #[must_use]
    pub fn with_peers(&self, s: SessionId) -> Vec<SessionId> {
        self.members_of(s).map_or_else(|| vec![s], <[_]>::to_vec)
    }
}

impl Model {
    /// `session_group_new` + `session_group_add`: put `s` in the group
    /// called `name` (made if missing) unless it is in one already, and fire
    /// `session-added-to-group`.
    pub fn group_add(&mut self, name: &str, s: SessionId) {
        if self.groups.name_of(s).is_some() {
            return;
        }
        let members = self.groups.by_name.entry(name.to_owned()).or_default();
        members.push(s);
        let size = members.len();
        self.fire_group("session-added-to-group", name, size, s, true);
    }

    /// `session_group_remove` (from `session_destroy`): fire
    /// `session-removed-from-group` with the size before, then drop the
    /// group once empty.
    pub(crate) fn group_remove(&mut self, s: SessionId) {
        let Some(name) = self.groups.name_of(s).map(str::to_owned) else {
            return;
        };
        let size = self.groups.by_name[&name].len();
        self.fire_group("session-removed-from-group", &name, size, s, false);
        let members = self.groups.by_name.get_mut(&name).expect("found above");
        members.retain(|x| *x != s);
        if members.is_empty() {
            self.groups.by_name.remove(&name);
        }
    }

    /// `session_group_fire` (session.c:572-588): the session is the target
    /// only while it is alive.
    fn fire_group(&mut self, event: &str, name: &str, size: usize, s: SessionId, alive: bool) {
        let mut ev = Event::new(event)
            .session(self, "session", s)
            .text("group", name)
            .int("group_size", i64::try_from(size).unwrap_or(i64::MAX));
        if alive {
            ev = ev.target(Found::from_session(self, s));
        }
        self.push_event(ev);
    }

    /// `session_group_attached_count`-style helper: the members of `s`'s
    /// group, or just `s`.
    #[must_use]
    pub fn group_peers(&self, s: SessionId) -> Vec<SessionId> {
        self.groups.with_peers(s)
    }

    /// `server_redraw_session_group`.
    pub fn redraw_group(&mut self, s: SessionId) {
        for x in self.group_peers(s) {
            self.redraw(Redraw::Session(x));
        }
    }

    /// `server_status_session_group`.
    pub fn status_group(&mut self, s: SessionId) {
        for x in self.group_peers(s) {
            self.redraw(Redraw::SessionStatus(x));
        }
    }

    /// `session_group_synchronize_to`: make `s`'s windows those of the
    /// first other member of its group.
    pub fn group_sync_to(&mut self, s: SessionId) {
        let Some(target) = self
            .groups
            .members_of(s)
            .and_then(|m| m.iter().copied().find(|x| *x != s))
        else {
            return;
        };
        self.group_sync1(target, s);
    }

    /// `session_group_synchronize_from`: make every other member's windows
    /// those of `target`.
    pub fn group_sync_from(&mut self, target: SessionId) {
        let Some(members) = self.groups.members_of(target).map(<[_]>::to_vec) else {
            return;
        };
        for s in members {
            if s != target {
                self.group_sync1(target, s);
            }
        }
    }

    /// `session_group_synchronize1` (session.c:686-742): destroy `s`'s
    /// winlinks and make them again from `target`'s, keeping the current
    /// window and the last-window stack by index. A window that loses its
    /// last link goes (unless held by a kill in progress); its panes wait
    /// in [`Model::dropped`] for the server to close.
    #[allow(clippy::missing_panics_doc)] // members are live sessions
    pub fn group_sync1(&mut self, target: SessionId, s: SessionId) {
        let Some(t) = self.sessions.get(&target) else {
            return;
        };
        // An empty session is about to be destroyed.
        if t.windows.is_empty() || !self.sessions.contains_key(&s) {
            return;
        }
        let from: Vec<(u32, WindowId)> = t.windows.iter().map(|(i, w)| (*i, *w)).collect();
        let from_alerts = t.link_alerts.clone();
        let target_current = t.current;

        // The current window vanished: move to the last, else previous,
        // else next one now.
        let session = &self.sessions[&s];
        let had_current = session.windows.contains_key(&session.current);
        if had_current
            && !from.iter().any(|(i, _)| *i == session.current)
            && self.last_window(s) != Some(true)
            && self.previous_window(s, false) != Some(true)
        {
            self.next_window(s, false);
        }

        let session = self.sessions.get_mut(&s).expect("checked above");
        let old_windows = std::mem::take(&mut session.windows);
        let old_tree = std::mem::take(&mut session.link_tree);
        session.link_alerts.clear();
        session.link_seq.clear();
        for (i, w) in &from {
            let stamp = self.next_link_stamp();
            let session = self.sessions.get_mut(&s).expect("checked above");
            session.windows.insert(*i, *w);
            session.link_tree.insert(*i);
            session.link_seq.insert(*i, stamp);
            if let Some(f) = from_alerts.get(i).filter(|f| **f != 0) {
                session.link_alerts.insert(*i, *f);
            }
            self.fire_winlink("window-linked", s, *i, *w);
        }

        let session = self.sessions.get_mut(&s).expect("checked above");
        let first = session.windows.keys().next().copied();
        let keep = if had_current {
            session.current
        } else {
            target_current
        };
        session.current = if session.windows.contains_key(&keep) {
            keep
        } else {
            first.expect("not empty")
        };
        let windows = &session.windows;
        session.last.retain(|i| windows.contains_key(i));

        // The old winlinks go root first; a window no longer in the
        // session is unlinked, and destroyed with its last link.
        let mut remaining: Vec<(u32, WindowId)> =
            old_windows.iter().map(|(i, w)| (*i, *w)).collect();
        for index in old_tree.root_order() {
            let Some(&w) = old_windows.get(&index) else {
                continue;
            };
            remaining.retain(|(i, _)| *i != index);
            if self.index_of(s, w).is_some() {
                continue;
            }
            let ev = Event::new("window-unlinked")
                .target(Found {
                    session: Some(s),
                    index: Some(index),
                    window: Some(w),
                    pane: self.windows.get(&w).and_then(|x| x.panes.active),
                    new_index: None,
                })
                .session(self, "session", s)
                .window(self, "window", w)
                .int("window_index", index)
                .linked_now(self, w);
            self.push_event(ev);
            if !remaining.iter().any(|(_, x)| *x == w) {
                let gone = self.drop_if_unlinked(w);
                self.dropped.extend(gone);
            }
        }
    }

    /// `server_destroy_session_group` (server-fn.c:439-455): `s`, or every
    /// member of its group, destroyed. Returns the panes that went.
    pub fn remove_session_group(&mut self, s: SessionId) -> Vec<crate::model::Pane> {
        let mut gone = Vec::new();
        for x in self.group_peers(s) {
            gone.extend(self.remove_session(x));
        }
        gone
    }

    /// `session_is_linked` (session.c:378-390): the window has links other
    /// than one per member of `s`'s group (or than `s`'s one).
    #[must_use]
    pub fn linked_outside(&self, s: SessionId, w: WindowId) -> bool {
        let links = self.window_links(w).len();
        let count = self.groups.members_of(s).map_or(1, <[_]>::len);
        links != count
    }

    /// `format_cb_window_linked_sessions`: the groups whose first member
    /// links `w`, then the ungrouped sessions linking it.
    #[must_use]
    pub fn linked_sessions(&self, w: WindowId) -> usize {
        let groups = self
            .groups
            .firsts()
            .filter(|s| self.index_of(*s, w).is_some())
            .count();
        let single = self
            .sessions_with(w)
            .filter(|s| self.groups.name_of(s.id).is_none())
            .count();
        groups + single
    }

    /// True when `a` and `b` are different sessions of one group (the
    /// `sessions are grouped` checks of server-fn.c:296-300 and
    /// cmd-swap-window.c:59-64).
    #[must_use]
    pub fn grouped_together(&self, a: SessionId, b: SessionId) -> bool {
        a != b && self.groups.members_of(a).is_some_and(|m| m.contains(&b))
    }
}
