//! Control mode subscriptions (`refresh-client -B name:what:format`,
//! monitor.c with control.c `control_sub_change`): once a second each of a
//! control client's subscriptions expands its format for what it watches
//! (its session, one pane `%N` or window `@N`, or all its session's panes
//! `%*` or windows `@*`) and reports `%subscription-changed` for every value
//! that differs from the last one, the first value included
//! (`MONITOR_NOTIFY_INITIAL`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use vtmux_core::ids::{PaneId, SessionId, WindowId};

use crate::server::{ClientId, Server};
use crate::target::Found;

/// The timer interval (monitor.c `tv = { .tv_sec = 1 }`).
const INTERVAL: Duration = Duration::from_secs(1);

/// What a subscription watches (`enum monitor_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    Session,
    Pane(u32),
    AllPanes,
    Window(u32),
    AllWindows,
}

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

/// `monitor_parse` (monitor.c:585-626): `name:what:format`, `what` one of
/// empty, `%N`, `%*`, `@N`, `@*`.
fn parse(value: &str) -> Option<(&str, What, &str)> {
    let (name, rest) = value.split_once(':')?;
    let (what, format) = rest.split_once(':')?;
    let id = |s: &str| {
        // sscanf("%d") with `id >= 0`: digits after an optional sign.
        let end = s
            .char_indices()
            .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '+' || c == '-'))))
            .map_or(s.len(), |(i, _)| i);
        s[..end]
            .parse::<i64>()
            .ok()
            .and_then(|n| u32::try_from(n).ok())
    };
    let what = match what {
        "%*" => What::AllPanes,
        "@*" => What::AllWindows,
        "" => What::Session,
        w if w.starts_with('%') => What::Pane(id(&w[1..])?),
        w if w.starts_with('@') => What::Window(id(&w[1..])?),
        _ => return None,
    };
    Some((name, what, format))
}

impl Server {
    /// `cmd_refresh_client_update_subscription`: add (or replace) the
    /// subscription, or remove the one named `value` when it does not parse.
    pub(crate) fn control_subscribe(&mut self, id: ClientId, value: &str) {
        let Some(subs) = self.control_subs_mut(id) else {
            return;
        };
        let Some((name, what, format)) = parse(value) else {
            // monitor_remove: the timer stops once none are left.
            subs.items.remove(value);
            if subs.items.is_empty() {
                subs.due = None;
            }
            return;
        };
        subs.items.insert(
            name.to_owned(),
            Sub {
                what,
                format: format.to_owned(),
                last: None,
                each: BTreeMap::new(),
            },
        );
        subs.due.get_or_insert_with(|| Instant::now() + INTERVAL);
    }

    fn control_subs_mut(&mut self, id: ClientId) -> Option<&mut Subs> {
        Some(&mut self.clients.get_mut(&id)?.control.as_mut()?.subs)
    }

    /// The next subscription timer of any control client.
    pub(crate) fn subs_deadline(&self) -> Option<Instant> {
        self.clients
            .values()
            .filter_map(|c| c.control.as_ref()?.subs.deadline())
            .min()
    }

    /// `monitor_timer` for each control client whose timer is due.
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
    }

    /// `monitor_timer` (monitor.c:494-532): nothing without a session;
    /// session subscriptions first, then pane and window ones, then all
    /// panes, then all windows, each in name order.
    fn subs_check(&mut self, id: ClientId) {
        let Some(s) = self.client_session(id) else {
            return;
        };
        let Some(subs) = self.control_subs_mut(id) else {
            return;
        };
        let mut items = std::mem::take(&mut subs.items);
        let mut lines = Vec::new();
        for (name, sub) in items.iter_mut().filter(|(_, x)| x.what == What::Session) {
            let found = Found::from_session(&self.model, s);
            let found = Found {
                window: None,
                index: None,
                pane: None,
                ..found
            };
            let value = self.subs_expand(id, found, &sub.format);
            if sub.last.as_deref() != Some(value.as_str()) {
                lines.push(format!("%subscription-changed {name} {s} - - - : {value}"));
            }
            sub.last = Some(value);
        }
        for (name, sub) in &mut items {
            match sub.what {
                What::Pane(p) => self.subs_pane(id, s, name, sub, PaneId(p), &mut lines),
                What::Window(w) => self.subs_window(id, s, name, sub, WindowId(w), &mut lines),
                _ => {}
            }
        }
        if items.values().any(|x| x.what == What::AllPanes) {
            self.subs_all_panes(id, s, &mut items, &mut lines);
        }
        if items.values().any(|x| x.what == What::AllWindows) {
            self.subs_all_windows(id, s, &mut items, &mut lines);
        }
        if let Some(subs) = self.control_subs_mut(id) {
            subs.items = items;
        }
        for line in lines {
            self.control_notify_line(id, line.into_bytes());
        }
    }

    /// `monitor_create_formats`: the client, the session, the winlink and
    /// the pane, no jobs.
    fn subs_expand(&self, id: ClientId, found: Found, format: &str) -> String {
        let mut ctx = self.format_ctx(found, Some(id));
        ctx.jobs = None;
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

    /// `monitor_check_pane`: the pane, while its PTY is open, at each of
    /// its window's winlinks in the client's session.
    fn subs_pane(
        &self,
        id: ClientId,
        s: SessionId,
        name: &str,
        sub: &mut Sub,
        p: PaneId,
        lines: &mut Vec<String>,
    ) {
        let Some(pane) = self.model.panes.get(&p).filter(|x| x.io.is_some()) else {
            return;
        };
        let w = pane.window;
        for index in self.subs_links(s, w) {
            let found = Found {
                session: Some(s),
                index: Some(index),
                window: Some(w),
                pane: Some(p),
                new_index: None,
            };
            let value = self.subs_expand(id, found, &sub.format);
            report(
                sub,
                name,
                (s, w, index, Some(p)),
                (p.0, index),
                value,
                lines,
            );
        }
    }

    /// `monitor_check_window`: the window at each of its winlinks in the
    /// client's session.
    fn subs_window(
        &self,
        id: ClientId,
        s: SessionId,
        name: &str,
        sub: &mut Sub,
        w: WindowId,
        lines: &mut Vec<String>,
    ) {
        if !self.model.windows.contains_key(&w) {
            return;
        }
        for index in self.subs_links(s, w) {
            let value = self.subs_expand(id, window_found(s, index, w), &sub.format);
            report(sub, name, (s, w, index, None), (w.0, index), value, lines);
        }
    }

    /// `monitor_check_all_panes`: every pane of every window of the
    /// session; entries for panes no longer there are forgotten.
    fn subs_all_panes(
        &self,
        id: ClientId,
        s: SessionId,
        items: &mut BTreeMap<String, Sub>,
        lines: &mut Vec<String>,
    ) {
        let Some(session) = self.model.sessions.get(&s) else {
            return;
        };
        let mut seen = Vec::new();
        for (&index, &w) in &session.windows {
            let Some(window) = self.model.windows.get(&w) else {
                continue;
            };
            for &p in &window.panes.panes {
                seen.push((p.0, index));
                let found = Found {
                    session: Some(s),
                    index: Some(index),
                    window: Some(w),
                    pane: Some(p),
                    new_index: None,
                };
                for (name, sub) in items.iter_mut().filter(|(_, x)| x.what == What::AllPanes) {
                    let value = self.subs_expand(id, found, &sub.format);
                    report(
                        sub,
                        name,
                        (s, w, index, Some(p)),
                        (p.0, index),
                        value,
                        lines,
                    );
                }
            }
        }
        for sub in items.values_mut().filter(|x| x.what == What::AllPanes) {
            sub.each.retain(|k, _| seen.contains(k));
        }
    }

    /// `monitor_check_all_windows`: every winlink of the session.
    fn subs_all_windows(
        &self,
        id: ClientId,
        s: SessionId,
        items: &mut BTreeMap<String, Sub>,
        lines: &mut Vec<String>,
    ) {
        let Some(session) = self.model.sessions.get(&s) else {
            return;
        };
        let mut seen = Vec::new();
        for (&index, &w) in &session.windows {
            seen.push((w.0, index));
            for (name, sub) in items.iter_mut().filter(|(_, x)| x.what == What::AllWindows) {
                let value = self.subs_expand(id, window_found(s, index, w), &sub.format);
                report(sub, name, (s, w, index, None), (w.0, index), value, lines);
            }
        }
        for sub in items.values_mut().filter(|x| x.what == What::AllWindows) {
            sub.each.retain(|k, _| seen.contains(k));
        }
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

/// `monitor_check_value` and `control_sub_change`: the line for a value
/// that changed (or is the first), remembered under `key`.
fn report(
    sub: &mut Sub,
    name: &str,
    (s, w, index, p): (SessionId, WindowId, u32, Option<PaneId>),
    key: (u32, u32),
    value: String,
    lines: &mut Vec<String>,
) {
    if sub.each.get(&key) == Some(&value) {
        return;
    }
    let pane = p.map_or_else(|| "-".to_owned(), |p| p.to_string());
    lines.push(format!(
        "%subscription-changed {name} {s} {w} {index} {pane} : {value}"
    ));
    sub.each.insert(key, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        assert_eq!(parse("sw::#{x}"), Some(("sw", What::Session, "#{x}")));
        assert_eq!(parse("a:%3:f:g"), Some(("a", What::Pane(3), "f:g")));
        assert_eq!(parse("a:%*:f"), Some(("a", What::AllPanes, "f")));
        assert_eq!(parse("a:@2x:f"), Some(("a", What::Window(2), "f")));
        assert_eq!(parse("a:@*:f"), Some(("a", What::AllWindows, "f")));
        assert_eq!(parse("a:%-1:f"), None);
        assert_eq!(parse("a:x:f"), None);
        assert_eq!(parse("sw"), None);
        assert_eq!(parse("sw:"), None);
    }
}
