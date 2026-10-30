//! Resolving `-t` targets against the model (cmd-find.c
//! `cmd_find_target`, 943-1274). The text was already split by
//! [`TargetSpec::resolve`](vtmux_core::cmd::TargetSpec::resolve) for the
//! kind the command wants; this module looks the parts up.

use vtmux_core::cmd::target::{Part, Resolved, TargetParts};
use vtmux_core::cmd::{FindFlags, TargetKind, TargetSpec, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, SessionId, WindowId};

use crate::model::{Model, Side};

/// A resolved target (`struct cmd_find_state`). Every field is `None` when
/// a `CANFAIL` lookup failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub session: Option<SessionId>,
    /// The window's index in `session` (tmux's winlink).
    pub index: Option<u32>,
    pub window: Option<WindowId>,
    pub pane: Option<PaneId>,
}

impl Found {
    /// A session's current window and active pane (`cmd_find_from_session`).
    #[must_use]
    pub fn from_session(model: &Model, s: SessionId) -> Self {
        let Some((index, window, pane)) = model.current_of(s) else {
            return Self::default();
        };
        Self {
            session: Some(s),
            index: Some(index),
            window: Some(window),
            pane: Some(pane),
        }
    }

    fn valid(&self) -> bool {
        self.session.is_some() && self.window.is_some() && self.pane.is_some()
    }
}

/// `cmd_find_best_session`: the most recently active session, optionally
/// among those linking window `w`.
#[must_use]
pub fn best_session(model: &Model, with: Option<WindowId>) -> Option<SessionId> {
    model
        .sessions_by_name()
        .filter(|s| model.current_of(s.id).is_some())
        .filter(|s| with.is_none_or(|w| s.windows.values().any(|x| *x == w)))
        // timercmp(>) keeps the first of equals; activity stamps are unique.
        .max_by_key(|s| s.activity)
        .map(|s| s.id)
}

/// Window `w` in its best session (`cmd_find_best_session_with_window` +
/// `cmd_find_best_winlink_with_window`).
fn best_with_window(model: &Model, w: WindowId) -> Option<Found> {
    let s = best_session(model, Some(w))?;
    let session = &model.sessions[&s];
    let index = if session.windows.get(&session.current) == Some(&w) {
        session.current
    } else {
        *session.windows.iter().find(|(_, x)| **x == w)?.0
    };
    Some(Found {
        session: Some(s),
        index: Some(index),
        window: Some(w),
        pane: model.windows.get(&w).map(crate::model::Window::active),
    })
}

/// Where the command runs from: the client's attached session, the pane
/// it was started in (`$TMUX_PANE`), or the best session
/// (`cmd_find_from_client`, cmd-find.c:879-937).
#[derive(Clone, Copy, Debug, Default)]
pub struct Origin {
    pub attached: Option<SessionId>,
    pub inside_pane: Option<PaneId>,
}

#[must_use]
pub fn current(model: &Model, origin: Origin) -> Option<Found> {
    if let Some(s) = origin.attached {
        let f = Found::from_session(model, s);
        return f.valid().then_some(f);
    }
    if let Some(p) = origin.inside_pane
        && let Some(pane) = model.panes.get(&p)
        && let Some(mut f) = best_with_window(model, pane.window)
    {
        f.pane = model
            .windows
            .get(&pane.window)
            .map(crate::model::Window::active);
        return Some(f);
    }
    best_session(model, None).map(|s| Found::from_session(model, s))
}

/// fnmatch(3) with no flags: `*`, `?`, `[...]` (with `!`/`^` negation and
/// ranges) and backslash escapes.
#[must_use]
pub fn fnmatch(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob(&p, &t)
}

fn glob(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => (0..=t.len()).any(|i| glob(&p[1..], &t[i..])),
        Some('?') => !t.is_empty() && glob(&p[1..], &t[1..]),
        Some('[') => {
            let Some(&c) = t.first() else { return false };
            let mut i = 1;
            let negate = matches!(p.get(1), Some('!' | '^'));
            if negate {
                i += 1;
            }
            let mut matched = false;
            let mut first = true;
            while i < p.len() && (first || p[i] != ']') {
                first = false;
                let lo = p[i];
                if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|c| *c != ']') {
                    matched |= (lo..=p[i + 2]).contains(&c);
                    i += 3;
                } else {
                    matched |= lo == c;
                    i += 1;
                }
            }
            if i >= p.len() {
                // No closing bracket: a literal '['.
                return c == '[' && glob(&p[1..], &t[1..]);
            }
            matched != negate && glob(&p[i + 1..], &t[1..])
        }
        Some('\\') if p.len() > 1 => t.first() == Some(&p[1]) && glob(&p[2..], &t[1..]),
        Some(c) => t.first() == Some(c) && glob(&p[1..], &t[1..]),
    }
}

/// `cmd_find_get_session` (cmd-find.c:410-466). Client names are not
/// matched (no named clients in M0).
fn find_session(model: &Model, part: &Part) -> Option<SessionId> {
    let name = part.text.as_str();
    if name.starts_with('$') {
        let id: SessionId = name.parse().ok()?;
        return model.sessions.contains_key(&id).then_some(id);
    }
    if let Some(s) = model.session_named(name) {
        return Some(s.id);
    }
    if part.exact {
        return None;
    }
    let unique = |pred: &dyn Fn(&str) -> bool| {
        let mut hits = model.sessions_by_name().filter(|s| pred(&s.name));
        let first = hits.next()?;
        hits.next().is_none().then_some(first.id)
    };
    let prefix: Vec<_> = model
        .sessions_by_name()
        .filter(|s| s.name.starts_with(name))
        .collect();
    match prefix.as_slice() {
        [one] => return Some(one.id),
        [] => {}
        _ => return None,
    }
    unique(&|n| fnmatch(name, n))
}

/// `cmd_find_get_window_with_session` (cmd-find.c:499-640).
fn find_window_in(model: &Model, s: SessionId, part: &Part) -> Option<Found> {
    let session = model.sessions.get(&s)?;
    let window = part.text.as_str();
    let at = |index: u32| {
        session.windows.get(&index).map(|w| Found {
            session: Some(s),
            index: Some(index),
            window: Some(*w),
            pane: model.windows.get(w).map(crate::model::Window::active),
        })
    };
    if window.starts_with('@') {
        let id: WindowId = window.parse().ok()?;
        let index = *session.windows.iter().find(|(_, w)| **w == id)?.0;
        return at(index);
    }
    let exact = part.exact;
    if !exact && (window.starts_with('+') || window.starts_with('-')) {
        let n = if window.len() > 1 {
            usize::try_from(strtonum(&window[1..], 1, i64::from(i32::MAX)).ok()?).ok()?
        } else {
            1
        };
        // winlink_next_by_number / previous: wrap around the indices.
        let indices: Vec<u32> = session.windows.keys().copied().collect();
        let pos = indices.iter().position(|i| *i == session.current)?;
        let len = indices.len();
        let target = if window.starts_with('+') {
            (pos + n) % len
        } else {
            (pos + len - n % len) % len
        };
        return at(indices[target]);
    }
    if !exact {
        match window {
            "!" => return None, // no last-window stack in M0
            "^" => return at(*session.windows.keys().next()?),
            "$" => return at(*session.windows.keys().next_back()?),
            _ => {}
        }
    }
    if let Ok(idx) = strtonum(window, 0, i64::from(i32::MAX))
        && let Some(f) = at(u32::try_from(idx).ok()?)
    {
        return Some(f);
    }
    let named = |pred: &dyn Fn(&str) -> bool| -> Result<Option<Found>, ()> {
        let mut hit = None;
        for (index, w) in &session.windows {
            if model.windows.get(w).is_some_and(|x| pred(&x.name)) {
                if hit.is_some() {
                    return Err(());
                }
                hit = at(*index);
            }
        }
        Ok(hit)
    };
    match named(&|n| n == window) {
        Ok(Some(f)) => return Some(f),
        Err(()) => return None,
        Ok(None) => {}
    }
    if exact {
        return None;
    }
    for pred in [
        &(|n: &str| n.starts_with(window)) as &dyn Fn(&str) -> bool,
        &|n: &str| fnmatch(window, n),
    ] {
        match named(pred) {
            Ok(Some(f)) => return Some(f),
            Err(()) => return None,
            Ok(None) => {}
        }
    }
    None
}

/// `cmd_find_get_window` (cmd-find.c:469-497).
fn find_window(model: &Model, current: Found, part: &Part, only: bool) -> Option<Found> {
    if part.text.starts_with('@') {
        let id: WindowId = part.text.parse().ok()?;
        model.windows.get(&id)?;
        return best_with_window(model, id);
    }
    if let Some(s) = current.session
        && let Some(f) = find_window_in(model, s, part)
    {
        return Some(f);
    }
    if !only && let Some(s) = find_session(model, part) {
        return Some(Found::from_session(model, s));
    }
    None
}

/// `cmd_find_get_pane_with_window` (cmd-find.c:579-660).
fn find_pane_in(model: &Model, w: WindowId, part: &str) -> Option<PaneId> {
    let window = model.windows.get(&w)?;
    if part.starts_with('%') {
        let id: PaneId = part.parse().ok()?;
        return (model.panes.get(&id)?.window == w).then_some(id);
    }
    let active = window.active();
    let side = match part {
        "!" => return window.panes.last.first().copied(),
        "{up-of}" => Some(Side::Up),
        "{down-of}" => Some(Side::Down),
        "{left-of}" => Some(Side::Left),
        "{right-of}" => Some(Side::Right),
        _ => None,
    };
    if let Some(side) = side {
        return model.neighbour(active, side);
    }
    let panes = &window.panes.panes;
    if part.starts_with('+') || part.starts_with('-') {
        let n = if part.len() > 1 {
            usize::try_from(strtonum(&part[1..], 1, i64::from(i32::MAX)).ok()?).ok()?
        } else {
            1
        };
        // window_pane_next_by_number / previous_by_number: wrap round.
        let pos = panes.iter().position(|p| *p == active)?;
        let len = panes.len();
        let target = if part.starts_with('+') {
            (pos + n % len) % len
        } else {
            (pos + len - n % len) % len
        };
        return Some(panes[target]);
    }
    // window_pane_at_index counts from pane-base-index.
    if let Ok(idx) = strtonum(part, 0, i64::from(i32::MAX)) {
        let base = i64::from(model.pane_base_index(w));
        if let Some(p) = idx
            .checked_sub(base)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| panes.get(i))
        {
            return Some(*p);
        }
    }
    model.pane_at_position(w, part)
}

/// `cmd_find_get_pane` (cmd-find.c:643-676).
fn find_pane(model: &Model, current: Found, part: &Part, only: bool) -> Option<Found> {
    let text = part.text.as_str();
    if text.starts_with('%') {
        let id: PaneId = text.parse().ok()?;
        let pane = model.panes.get(&id)?;
        let mut f = best_with_window(model, pane.window)?;
        f.pane = Some(id);
        return Some(f);
    }
    if let Some(w) = current.window
        && let Some(p) = find_pane_in(model, w, text)
    {
        return Some(Found {
            pane: Some(p),
            ..current
        });
    }
    if !only && let Some(mut f) = find_window(model, current, part, false) {
        f.pane = f
            .window
            .and_then(|w| model.windows.get(&w))
            .map(crate::model::Window::active);
        return Some(f);
    }
    None
}

/// `cmd_find_target`: resolve `spec` (absent = current) for `kind`.
///
/// With `can_fail`, any failure is silent and yields an empty [`Found`].
pub fn resolve(
    model: &Model,
    origin: Origin,
    spec: Option<&TargetSpec>,
    kind: TargetKind,
    flags: FindFlags,
) -> Result<Found, EdgeError> {
    let result = resolve_inner(model, origin, spec, kind);
    match result {
        Err(_) if flags.can_fail => Ok(Found::default()),
        other => other,
    }
}

fn resolve_inner(
    model: &Model,
    origin: Origin,
    spec: Option<&TargetSpec>,
    kind: TargetKind,
) -> Result<Found, EdgeError> {
    let current = current(model, origin).ok_or(EdgeError::NoCurrentTarget)?;
    let Some(spec) = spec else {
        return Ok(current);
    };
    let parts = match spec.resolve(kind) {
        Resolved::Current => return Ok(current),
        Resolved::ClientWindow => {
            let s = origin.attached.ok_or(EdgeError::NoCurrentClient)?;
            return Ok(Found::from_session(model, s));
        }
        Resolved::Mouse => return Err(EdgeError::NotImplemented("mouse target".into())),
        Resolved::Marked => return Err(EdgeError::NotImplemented("marked target".into())),
        Resolved::Parts(p) => p,
    };
    find_parts(model, current, &parts)
}

fn find_parts(model: &Model, current: Found, parts: &TargetParts) -> Result<Found, EdgeError> {
    let no_session = |p: &Part| EdgeError::SessionNotFound(p.text.clone());
    let no_window = |p: &Part| EdgeError::WindowNotFound(p.text.clone());
    let no_pane = |p: &Part| EdgeError::PaneNotFound(p.text.clone());
    if let Some(sp) = &parts.session {
        let s = find_session(model, sp).ok_or_else(|| no_session(sp))?;
        return match (&parts.window, &parts.pane) {
            (None, None) => Ok(Found::from_session(model, s)),
            (Some(wp), None) => find_window_in(model, s, wp).ok_or_else(|| no_window(wp)),
            (None, Some(pp)) => {
                let base = Found::from_session(model, s);
                let w = base.window.ok_or_else(|| no_pane(pp))?;
                let p = find_pane_in(model, w, &pp.text).ok_or_else(|| no_pane(pp))?;
                Ok(Found {
                    pane: Some(p),
                    ..base
                })
            }
            (Some(wp), Some(pp)) => {
                let f = find_window_in(model, s, wp).ok_or_else(|| no_window(wp))?;
                let w = f.window.ok_or_else(|| no_window(wp))?;
                let p = find_pane_in(model, w, &pp.text).ok_or_else(|| no_pane(pp))?;
                Ok(Found { pane: Some(p), ..f })
            }
        };
    }
    match (&parts.window, &parts.pane) {
        (Some(wp), Some(pp)) => {
            let f =
                find_window(model, current, wp, parts.window_only).ok_or_else(|| no_window(wp))?;
            let w = f.window.ok_or_else(|| no_window(wp))?;
            let p = find_pane_in(model, w, &pp.text).ok_or_else(|| no_pane(pp))?;
            Ok(Found { pane: Some(p), ..f })
        }
        (Some(wp), None) => {
            find_window(model, current, wp, parts.window_only).ok_or_else(|| no_window(wp))
        }
        (None, Some(pp)) => {
            find_pane(model, current, pp, parts.pane_only).ok_or_else(|| no_pane(pp))
        }
        (None, None) => Ok(current),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_basics() {
        assert!(fnmatch("a*", "abc"));
        assert!(fnmatch("a?c", "abc"));
        assert!(fnmatch("[ab]c", "bc"));
        assert!(fnmatch("[!a]c", "bc"));
        assert!(fnmatch("[a-c]x", "bx"));
        assert!(!fnmatch("a*d", "abc"));
        assert!(fnmatch("\\*", "*"));
        assert!(fnmatch("[", "["));
    }
}
