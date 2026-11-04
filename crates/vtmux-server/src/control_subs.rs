//! Monitors (monitor.c): once a second each subscription expands its format
//! for what it watches (a session, one pane `%N` or window `@N`, or all
//! the session's panes `%*` or windows `@*`) and reports every value that
//! differs from the last one. Two kinds of set use them:
//!
//! - a control client's subscriptions (`refresh-client -B`, control.c
//!   `control_sub_change`): the first value is reported too
//!   (`MONITOR_NOTIFY_INITIAL`), as `%subscription-changed`;
//! - monitor hooks (`set-hook -B`, hooks.c `hooks_monitor_cb`): a change
//!   fires the hook's own event, whose only sink runs the hook's commands
//!   with the payload's formats expanded into them (`hooks_monitor_hook_cb`,
//!   `hooks_parse`). What each hook watches lives in the option store
//!   ([`vtmux_core::options::monitor`]); the last values live here.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::options::TreeId;
pub use vtmux_core::options::monitor::MonitorWhat as What;
use vtmux_core::options::monitor::parse_monitor;

use crate::events::Event;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// The timer interval (monitor.c `tv = { .tv_sec = 1 }`).
const INTERVAL: Duration = Duration::from_secs(1);

/// One subscription and its last values, by (object, winlink index).
#[derive(Debug)]
struct Sub {
    what: What,
    format: String,
    /// The session subscription's last value.
    last: Option<String>,
    /// Pane or window subscriptions: the last value per (id, index).
    each: BTreeMap<(u32, u32), String>,
}

impl Sub {
    fn new(what: What, format: &str) -> Self {
        Self {
            what,
            format: format.to_owned(),
            last: None,
            each: BTreeMap::new(),
        }
    }
}

/// A control client's subscriptions (`cs->subs`), by name.
#[derive(Debug, Default)]
pub struct Subs {
    items: BTreeMap<String, Sub>,
    /// When the timer fires next; `None` while it is not running.
    due: Option<Instant>,
}

impl Subs {
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.due
    }
}

/// The monitor hooks' last values and timers (`hm->set` of each hook), by
/// the store's monitor serial.
#[derive(Debug, Default)]
pub struct HookMonitors {
    sets: HashMap<u64, HookSet>,
}

#[derive(Debug)]
struct HookSet {
    sub: Sub,
    due: Instant,
}

impl HookMonitors {
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.sets.values().map(|s| s.due).min()
    }
}

/// A value that changed (`struct monitor_change`): where, the new value
/// and the last one (`None` for a first value).
struct Change {
    s: SessionId,
    at: Option<(WindowId, u32)>,
    pane: Option<PaneId>,
    value: String,
    last: Option<String>,
}

/// `monitor_check_value`: the change `value` is from what `last` held, if
/// it is one to report; `last` takes the value either way. `initial`:
/// `MONITOR_NOTIFY_INITIAL`; `only_true`: `MONITOR_NOTIFY_TRUE`.
fn check(
    last: Option<&mut String>,
    value: String,
    initial: bool,
    only_true: bool,
) -> (Option<(String, Option<String>)>, String) {
    let truthy = !only_true || vtmux_core::format::is_true(&value);
    match last {
        None => {
            let report = (initial && truthy).then(|| (value.clone(), None));
            (report, value)
        }
        Some(old) if *old == value => (None, value),
        Some(old) => {
            let before = std::mem::replace(old, value.clone());
            (truthy.then(|| (value.clone(), Some(before))), value)
        }
    }
}

/// How a set reports: `MONITOR_NOTIFY_INITIAL`, `MONITOR_NOTIFY_TRUE`.
#[derive(Clone, Copy)]
struct Mode {
    initial: bool,
    only_true: bool,
}

impl Server {
    /// `cmd_refresh_client_update_subscription`: add (or replace) the
    /// subscription, or remove the one named `value` when it does not parse.
    pub(crate) fn control_subscribe(&mut self, id: ClientId, value: &str) {
        let Some(subs) = self.control_subs_mut(id) else {
            return;
        };
        let Some((name, what, format)) = parse_monitor(value) else {
            // monitor_remove: the timer stops once none are left.
            subs.items.remove(value);
            if subs.items.is_empty() {
                subs.due = None;
            }
            return;
        };
        subs.items.insert(name.to_owned(), Sub::new(what, format));
        subs.due.get_or_insert_with(|| Instant::now() + INTERVAL);
    }

    fn control_subs_mut(&mut self, id: ClientId) -> Option<&mut Subs> {
        Some(&mut self.clients.get_mut(&id)?.control.as_mut()?.subs)
    }

    /// The next subscription timer of any control client or monitor hook.
    pub(crate) fn subs_deadline(&self) -> Option<Instant> {
        self.clients
            .values()
            .filter_map(|c| c.control.as_ref()?.subs.deadline())
            .chain(self.hook_monitors.deadline())
            .min()
    }

    /// `monitor_timer` for each control client and monitor hook whose
    /// timer is due.
    pub(crate) fn subs_timers(&mut self, now: Instant) {
        let due: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| {
                c.control
                    .as_ref()
                    .and_then(|cs| cs.subs.due)
                    .is_some_and(|d| d <= now)
            })
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            if let Some(subs) = self.control_subs_mut(id) {
                subs.due = Some(now + INTERVAL);
            }
            self.subs_check(id);
        }
        self.hook_monitor_timers(now);
    }

    /// `monitor_timer` (monitor.c:494-532) for a control client: nothing
    /// without a session; session subscriptions first, then pane and
    /// window ones, then all panes, then all windows, each in name order.
    fn subs_check(&mut self, id: ClientId) {
        let Some(s) = self.client_session(id) else {
            return;
        };
        let Some(subs) = self.control_subs_mut(id) else {
            return;
        };
        let mut items = std::mem::take(&mut subs.items);
        let mode = Mode {
            initial: true,
            only_true: false,
        };
        let mut lines = Vec::new();
        for kind in [
            Pass::Session,
            Pass::Single,
            Pass::AllPanes,
            Pass::AllWindows,
        ] {
            let any = items.values().any(|x| kind.takes(x.what));
            if !any {
                continue;
            }
            let mut changes: Vec<(String, Change)> = Vec::new();
            for (name, sub) in items.iter_mut().filter(|(_, x)| kind.takes(x.what)) {
                for c in self.monitor_check(Some(id), s, sub, mode) {
                    changes.push((name.clone(), c));
                }
            }
            // All-panes and all-windows passes go object by object, each
            // object through every subscription.
            if matches!(kind, Pass::AllPanes | Pass::AllWindows) {
                changes.sort_by_key(|(_, c)| {
                    (
                        c.at.map(|(_, i)| i),
                        c.pane.and_then(|p| self.model.pane_index(p)),
                    )
                });
            }
            for (name, c) in changes {
                lines.push(subscription_line(&name, &c));
            }
        }
        if let Some(subs) = self.control_subs_mut(id) {
            subs.items = items;
        }
        for line in lines {
            self.control_notify_line(id, line.into_bytes());
        }
    }

    /// The values `sub` sees now in session `s`, checked against its last
    /// ones: the changes to report, in the order monitor.c finds them.
    fn monitor_check(
        &self,
        client: Option<ClientId>,
        s: SessionId,
        sub: &mut Sub,
        mode: Mode,
    ) -> Vec<Change> {
        let mut out = Vec::new();
        let mut each = |sub: &mut Sub, key: (u32, u32), found: Found, change: Change| {
            let value = self.subs_expand(client, found, &sub.format);
            let (report, kept) = check(sub.each.get_mut(&key), value, mode.initial, mode.only_true);
            sub.each.entry(key).or_insert(kept);
            if let Some((value, last)) = report {
                out.push(Change {
                    value,
                    last,
                    ..change
                });
            }
        };
        let blank = |at, pane| Change {
            s,
            at,
            pane,
            value: String::new(),
            last: None,
        };
        match sub.what {
            What::Session => {
                let found = Found {
                    window: None,
                    index: None,
                    pane: None,
                    ..Found::from_session(&self.model, s)
                };
                let value = self.subs_expand(client, found, &sub.format);
                let (report, kept) = check(sub.last.as_mut(), value, mode.initial, mode.only_true);
                sub.last = Some(kept);
                if let Some((value, last)) = report {
                    out.push(Change {
                        value,
                        last,
                        ..blank(None, None)
                    });
                }
            }
            // monitor_check_pane: the pane, while its PTY is open, at each
            // of its window's winlinks in the session.
            What::Pane(p) => {
                let p = PaneId(p);
                let Some(pane) = self.model.panes.get(&p).filter(|x| x.io.is_some()) else {
                    return out;
                };
                let w = pane.window;
                for index in self.subs_links(s, w) {
                    let found = pane_found(s, index, w, p);
                    each(sub, (p.0, index), found, blank(Some((w, index)), Some(p)));
                }
            }
            What::Window(w) => {
                let w = WindowId(w);
                if !self.model.windows.contains_key(&w) {
                    return out;
                }
                for index in self.subs_links(s, w) {
                    let found = window_found(s, index, w);
                    each(sub, (w.0, index), found, blank(Some((w, index)), None));
                }
            }
            // monitor_check_all_panes: every pane of every window of the
            // session; entries for panes no longer there are forgotten.
            What::AllPanes => {
                let Some(session) = self.model.sessions.get(&s) else {
                    return out;
                };
                let mut seen = Vec::new();
                for (&index, &w) in &session.windows {
                    let Some(window) = self.model.windows.get(&w) else {
                        continue;
                    };
                    for &p in &window.panes.panes {
                        seen.push((p.0, index));
                        let found = pane_found(s, index, w, p);
                        each(sub, (p.0, index), found, blank(Some((w, index)), Some(p)));
                    }
                }
                sub.each.retain(|k, _| seen.contains(k));
            }
            // monitor_check_all_windows: every winlink of the session.
            What::AllWindows => {
                let Some(session) = self.model.sessions.get(&s) else {
                    return out;
                };
                let mut seen = Vec::new();
                for (&index, &w) in &session.windows {
                    seen.push((w.0, index));
                    let found = window_found(s, index, w);
                    each(sub, (w.0, index), found, blank(Some((w, index)), None));
                }
                sub.each.retain(|k, _| seen.contains(k));
            }
        }
        out
    }

    /// `monitor_create_formats`: the client, the session, the winlink and
    /// the pane, no jobs.
    fn subs_expand(&self, id: Option<ClientId>, found: Found, format: &str) -> String {
        let mut ctx = self.format_ctx(found, id);
        ctx.jobs = None;
        ctx.old_layouts = false;
        vtmux_core::format::expand_str_nojobs(format, &ctx)
    }

    /// The winlinks of `w` in session `s`, in the window's link order.
    fn subs_links(&self, s: SessionId, w: WindowId) -> Vec<u32> {
        self.model
            .window_links(w)
            .into_iter()
            .filter(|(ls, _)| ls.id == s)
            .map(|(_, i)| i)
            .collect()
    }

    // ------------------------------------------------------ monitor hooks

    /// Keep the hook monitors' state in step with the option store: a
    /// monitor set (or set again) starts with no values and its timer a
    /// second away (`monitor_add`); one gone is forgotten.
    pub(crate) fn hook_monitors_sync(&mut self) {
        let now = Instant::now();
        let live: Vec<(u64, What, String)> = self
            .model
            .options
            .monitors()
            .into_iter()
            .map(|(_, _, m)| (m.serial, m.what, m.format.clone()))
            .collect();
        let sets = &mut self.hook_monitors.sets;
        sets.retain(|serial, _| live.iter().any(|(s, ..)| s == serial));
        for (serial, what, format) in live {
            sets.entry(serial).or_insert_with(|| HookSet {
                sub: Sub::new(what, &format),
                due: now + INTERVAL,
            });
        }
    }

    /// `monitor_timer` for each monitor hook that is due, oldest first.
    fn hook_monitor_timers(&mut self, now: Instant) {
        self.hook_monitors_sync();
        let hooks: Vec<_> = self
            .model
            .options
            .monitors()
            .into_iter()
            .map(|(t, n, m)| (t, n.to_owned(), m.clone()))
            .collect();
        for (tree, name, m) in hooks {
            let Some(set) = self.hook_monitors.sets.get_mut(&m.serial) else {
                continue;
            };
            if set.due > now {
                continue;
            }
            set.due = now + INTERVAL;
            // monitor_get_session: the hook's session while it lives, or
            // the first session for a global hook.
            let s = match m.session {
                Some(s) => self.model.sessions.contains_key(&s).then_some(s),
                None => self.model.by_name.values().next().copied(),
            };
            let Some(s) = s else {
                continue;
            };
            let mut sub = std::mem::replace(&mut set.sub, Sub::new(m.what, &m.format));
            let mode = Mode {
                initial: false,
                only_true: m.notify_true,
            };
            let changes = self.monitor_check(None, s, &mut sub, mode);
            if let Some(set) = self.hook_monitors.sets.get_mut(&m.serial) {
                set.sub = sub;
            }
            for c in changes {
                self.hook_monitor_fire(tree, &name, &c);
            }
        }
    }

    /// `monitor_report` and `hooks_monitor_cb` (hooks.c:365-417): count the
    /// firing, then fire the hook's event with the change as its payload.
    /// Only the hook's own sink hears it (`hooks_event_cb` skips it, and a
    /// `wait-for -E` on the name sees it with `_hooks_monitor` hidden).
    fn hook_monitor_fire(&mut self, tree: TreeId, name: &str, c: &Change) {
        self.model
            .options
            .monitor_fired(tree, name, crate::model::now_epoch());
        let m = &self.model;
        let target = match (c.at, c.pane) {
            (Some((w, index)), Some(p)) => pane_found(c.s, index, w, p),
            (Some((_, index)), None) => Found::from_winlink(m, c.s, index),
            _ => Found::from_session(m, c.s),
        };
        let mut ev = Event::new(name)
            .target(target)
            .text("value", c.value.clone())
            .text("last", c.last.clone().unwrap_or_default())
            .session(m, "session", c.s);
        if let Some((w, index)) = c.at {
            ev = ev.window(m, "window", w).int("window_index", index);
        }
        if let Some(p) = c.pane {
            ev = ev.pane("pane", p);
        }
        ev.monitor = Some(tree);
        self.fire_event(&ev);
    }
}

/// The passes of `monitor_timer`, in order.
#[derive(Clone, Copy)]
enum Pass {
    Session,
    Single,
    AllPanes,
    AllWindows,
}

impl Pass {
    fn takes(self, what: What) -> bool {
        matches!(
            (self, what),
            (Self::Session, What::Session)
                | (Self::Single, What::Pane(_) | What::Window(_))
                | (Self::AllPanes, What::AllPanes)
                | (Self::AllWindows, What::AllWindows)
        )
    }
}

/// `control_sub_change` (control.c:963-986).
fn subscription_line(name: &str, c: &Change) -> String {
    let s = c.s;
    let value = &c.value;
    match (c.at, c.pane) {
        (Some((w, index)), Some(p)) => {
            format!("%subscription-changed {name} {s} {w} {index} {p} : {value}")
        }
        (Some((w, index)), None) => {
            format!("%subscription-changed {name} {s} {w} {index} - : {value}")
        }
        _ => format!("%subscription-changed {name} {s} - - - : {value}"),
    }
}

/// A pane at a winlink.
fn pane_found(s: SessionId, index: u32, w: WindowId, p: PaneId) -> Found {
    Found {
        session: Some(s),
        index: Some(index),
        window: Some(w),
        pane: Some(p),
        new_index: None,
    }
}

/// A window at a winlink with no pane (`monitor_create_formats(c, s, wl,
/// NULL)`: `format_defaults` then takes the window's active pane).
fn window_found(s: SessionId, index: u32, w: WindowId) -> Found {
    Found {
        session: Some(s),
        index: Some(index),
        window: Some(w),
        pane: None,
        new_index: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_reports_by_mode() {
        let s = |x: &str| x.to_owned();
        // A first value: only with MONITOR_NOTIFY_INITIAL.
        assert_eq!(check(None, s("a"), true, false).0, Some((s("a"), None)));
        assert_eq!(check(None, s("a"), false, false).0, None);
        // Unchanged: nothing.
        let mut last = s("a");
        assert_eq!(check(Some(&mut last), s("a"), false, false).0, None);
        // Changed: the new value and the last one.
        assert_eq!(
            check(Some(&mut last), s("b"), false, false).0,
            Some((s("b"), Some(s("a"))))
        );
        assert_eq!(last, "b");
        // MONITOR_NOTIFY_TRUE: a change to a false value is remembered but
        // not reported.
        assert_eq!(check(Some(&mut last), s("0"), false, true).0, None);
        assert_eq!(last, "0");
        assert_eq!(
            check(Some(&mut last), s("1"), false, true).0,
            Some((s("1"), Some(s("0"))))
        );
    }
}
