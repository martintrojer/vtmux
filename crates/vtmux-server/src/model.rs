//! Sessions, windows and panes in ID-keyed arenas.
//!
//! Edge-visible orderings are reproduced on purpose (spec, *Rust at the
//! core, tmux at the edge*): sessions are listed in name order (tmux keeps
//! them in an RB tree keyed by name, session.c `session_cmp`), windows by
//! their index in the session, and IDs come from monotonic allocators that
//! never reuse a number while the server lives (session.c:30, window.c:61).
//!
//! A window's geometry is a [`vtmux_core::layout::Layout`] (D15: tiled tree
//! plus floating cells) and its pane order, active pane, last-used stack and
//! z-order are a [`WindowPanes`]; panes carry the rectangle the layout last
//! gave them (`layout_fix_panes`), which is what formats and the PTY see.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use vtmux_core::ids::{IdAllocator, PaneId, SessionId, WindowId};
use vtmux_core::layout::{Geometry, Layout, PaneStatus, WindowPanes};
use vtmux_core::options::{Defaults, OptionValue, OptionsStore, TreeId, Value};
use vtmux_vt::PaneTerm;

use crate::pane_io::PaneIo;

#[derive(Debug)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    /// Seconds since the epoch (`#{session_created}`).
    pub created: i64,
    /// Ordering for "most recently active" (cmd-find.c
    /// `cmd_find_session_better`): a server-wide counter bumped on activity.
    pub activity: u64,
    pub cwd: PathBuf,
    /// Window index -> window (tmux's winlinks).
    pub windows: BTreeMap<u32, WindowId>,
    /// The current window's index.
    pub current: u32,
    /// The session's own environment (update-environment, `-e`).
    pub env: crate::environ::Environ,
}

/// A zoomed window: the full layout is kept aside while the zoomed pane
/// fills the window (`w->saved_layout_root`, window.c `window_zoom`).
#[derive(Debug)]
pub struct Zoom {
    pub pane: PaneId,
    pub saved: Layout,
}

#[derive(Debug)]
pub struct Window {
    pub id: WindowId,
    pub name: String,
    /// `automatic-rename`: off once a name is given.
    pub auto_rename: bool,
    /// The visible layout (`w->layout_root`); one cell while zoomed.
    pub layout: Layout,
    /// Pane order, active pane, last-used stack, z-order.
    pub panes: WindowPanes,
    pub zoom: Option<Zoom>,
}

impl Window {
    /// The active pane. A window always has one while it exists.
    #[must_use]
    pub fn active(&self) -> PaneId {
        self.panes.active.expect("a window has an active pane")
    }

    /// The full layout (`saved_layout_root` while zoomed), which
    /// `#{window_layout}` prints.
    #[must_use]
    pub fn full_layout(&self) -> &Layout {
        self.zoom.as_ref().map_or(&self.layout, |z| &z.saved)
    }

    #[must_use]
    pub fn zoomed(&self) -> bool {
        self.zoom.is_some()
    }

    /// `window_pane_is_visible`: every pane, or only the zoomed one.
    #[must_use]
    pub fn is_visible(&self, p: PaneId) -> bool {
        self.zoom.as_ref().is_none_or(|z| z.pane == p)
    }

    /// `window_pane_is_floating` (with the saved layout while zoomed).
    #[must_use]
    pub fn is_floating(&self, p: PaneId) -> bool {
        let root = &self.full_layout().root;
        root.find_pane(p)
            .is_some_and(|at| root.get(&at).is_floating())
    }

    #[must_use]
    pub fn sx(&self) -> u32 {
        self.layout.sx
    }

    #[must_use]
    pub fn sy(&self) -> u32 {
        self.layout.sy
    }
}

#[derive(Debug)]
pub struct Pane {
    pub id: PaneId,
    pub window: WindowId,
    pub term: PaneTerm,
    /// The PTY and its process; `None` once the PTY is closed.
    pub io: Option<PaneIo>,
    pub pid: i32,
    /// The command it was started with (`#{pane_start_command}`).
    pub argv: Vec<String>,
    pub shell: PathBuf,
    pub cwd: PathBuf,
    /// The process has been reaped.
    pub exited: bool,
    /// Output arrived since the last automatic-rename check (tmux's
    /// `PANE_CHANGED`).
    pub changed: bool,
    pub last_rename: Option<Instant>,
    /// `wp->xoff`, `wp->yoff`, `wp->sx`, `wp->sy` as `layout_fix_panes` set
    /// them.
    pub rect: Geometry,
    /// `PANE_INPUTOFF` (select-pane -d / -e).
    pub input_off: bool,
    /// `wp->active_point`: when the pane last became active, for picking the
    /// most recent of several neighbours (`window_pane_choose_best`).
    pub active_point: u64,
}

/// The server's sessions, windows and panes.
#[derive(Debug)]
pub struct Model {
    pub options: OptionsStore,
    pub sessions: BTreeMap<SessionId, Session>,
    /// Name -> ID: the listing order (tmux sorts sessions by name).
    pub by_name: BTreeMap<String, SessionId>,
    pub windows: BTreeMap<WindowId, Window>,
    pub panes: BTreeMap<PaneId, Pane>,
    pub session_ids: IdAllocator<SessionId>,
    pub window_ids: IdAllocator<WindowId>,
    pub pane_ids: IdAllocator<PaneId>,
    activity: u64,
    /// `next_active_point` (window.c:63).
    active_points: u64,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            options: OptionsStore::new(&Defaults::default()),
            sessions: BTreeMap::new(),
            by_name: BTreeMap::new(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            session_ids: IdAllocator::new(),
            window_ids: IdAllocator::new(),
            pane_ids: IdAllocator::new(),
            activity: 0,
            active_points: 0,
        }
    }
}

impl Model {
    /// The next activity stamp.
    pub fn touch(&mut self) -> u64 {
        self.activity += 1;
        self.activity
    }

    /// Sessions in name order.
    pub fn sessions_by_name(&self) -> impl Iterator<Item = &Session> {
        self.by_name.values().map(|id| &self.sessions[id])
    }

    #[must_use]
    pub fn session_named(&self, name: &str) -> Option<&Session> {
        self.by_name.get(name).map(|id| &self.sessions[id])
    }

    /// The session's current window and its active pane.
    #[must_use]
    pub fn current_of(&self, s: SessionId) -> Option<(u32, WindowId, PaneId)> {
        let session = self.sessions.get(&s)?;
        let w = *session.windows.get(&session.current)?;
        Some((session.current, w, self.windows.get(&w)?.active()))
    }

    /// Sessions that link window `w`.
    pub fn sessions_with(&self, w: WindowId) -> impl Iterator<Item = &Session> {
        self.sessions
            .values()
            .filter(move |s| s.windows.values().any(|x| *x == w))
    }

    /// A number-valued option as seen from `tree`.
    #[must_use]
    pub fn number_option(&self, tree: TreeId, name: &str) -> i64 {
        match self.options.get(tree, name).map(|e| &e.value) {
            Some(OptionValue::Single(Value::Number(n))) => *n,
            Some(OptionValue::Single(Value::Choice(n))) => i64::try_from(*n).unwrap_or(0),
            Some(OptionValue::Single(Value::Flag(b))) => i64::from(*b),
            _ => 0,
        }
    }

    /// `#{pane_index}`: position in the window's pane list plus
    /// `pane-base-index` (`window_pane_index`).
    #[must_use]
    pub fn pane_index(&self, p: PaneId) -> Option<u32> {
        let pane = self.panes.get(&p)?;
        let w = self.windows.get(&pane.window)?;
        let at = w.panes.panes.iter().position(|x| *x == p)?;
        Some(self.pane_base_index(pane.window) + u32::try_from(at).ok()?)
    }

    #[must_use]
    pub fn pane_base_index(&self, w: WindowId) -> u32 {
        u32::try_from(self.number_option(TreeId::Window(w), "pane-base-index")).unwrap_or(0)
    }

    /// `window_get_pane_status` (window.c:2918): the `*-floating` values
    /// count as off for tiled geometry.
    #[must_use]
    pub fn pane_status(&self, w: WindowId) -> PaneStatus {
        match self.number_option(TreeId::Window(w), "pane-border-status") {
            1 => PaneStatus::Top,
            2 => PaneStatus::Bottom,
            _ => PaneStatus::Off,
        }
    }

    /// `window_set_active_pane` (window.c:755-791) without the zoom part:
    /// stamps the active point. False when `p` already is active.
    pub fn set_active(&mut self, w: WindowId, p: PaneId) -> bool {
        let Some(window) = self.windows.get(&w) else {
            return false;
        };
        if window.panes.active == Some(p) {
            return false;
        }
        // window.c:767-768: selecting a hidden pane unzooms.
        if !window.is_visible(p) {
            self.unzoom(w);
        }
        let window = self.windows.get_mut(&w).expect("checked above");
        let floating = window.is_floating(p);
        if !window.panes.select_raising(p, floating) {
            return false;
        }
        self.active_points += 1;
        if let Some(pane) = self.panes.get_mut(&p) {
            pane.active_point = self.active_points;
            pane.changed = true;
        }
        true
    }

    /// `layout_fix_panes` + `window_pane_resize`: copy each cell's
    /// rectangle (less the pane status line) to its pane, resizing the
    /// terminal and the PTY of every pane whose size changed.
    pub fn fix_panes(&mut self, w: WindowId) {
        let status = self.pane_status(w);
        let Some(window) = self.windows.get_mut(&w) else {
            return;
        };
        window.layout.opts.status = status;
        if let Some(z) = &mut window.zoom {
            z.saved.opts.status = status;
        }
        for p in &window.panes.panes {
            // Panes hidden by a zoom keep their last rectangle.
            let Some(rect) = window.layout.pane_rect(*p) else {
                continue;
            };
            if let Some(pane) = self.panes.get_mut(p) {
                pane.set_rect(rect);
            }
        }
    }

    pub fn insert_session(&mut self, session: Session) {
        self.options.add_session(session.id);
        self.by_name.insert(session.name.clone(), session.id);
        self.sessions.insert(session.id, session);
    }

    /// Remove a session and every window no other session links; returns
    /// the panes that went with them (their PTYs close when dropped).
    pub fn remove_session(&mut self, id: SessionId) -> Vec<Pane> {
        let Some(session) = self.sessions.remove(&id) else {
            return Vec::new();
        };
        self.options.remove_session(id);
        self.by_name.remove(&session.name);
        let mut gone = Vec::new();
        for w in session.windows.values() {
            if self.sessions_with(*w).next().is_none() {
                gone.extend(self.remove_window(*w));
            }
        }
        gone
    }

    /// Drop window `w` and its panes (not its links).
    fn remove_window(&mut self, w: WindowId) -> Vec<Pane> {
        let Some(window) = self.windows.remove(&w) else {
            return Vec::new();
        };
        self.options.remove_window(w);
        let mut gone = Vec::new();
        for p in window.panes.panes {
            self.options.remove_pane(p);
            gone.extend(self.panes.remove(&p));
        }
        gone
    }

    /// Unlink window `w` from every session and destroy it; an emptied
    /// session is destroyed (`server_kill_window`, server-fn.c:239-270).
    pub fn kill_window(&mut self, w: WindowId) -> Vec<Pane> {
        let mut gone = self.remove_window(w);
        let mut dead = Vec::new();
        for s in self.sessions.values_mut() {
            s.windows.retain(|_, x| *x != w);
            if s.windows.is_empty() {
                dead.push(s.id);
            } else if !s.windows.contains_key(&s.current) {
                // session_detach picks the last window (session.c
                // session_detach -> session_next_session... M0 has one).
                s.current = *s.windows.keys().next().expect("non-empty");
            }
        }
        for s in dead {
            gone.extend(self.remove_session(s));
        }
        gone
    }

    /// `server_destroy_pane` / `server_kill_pane`: remove a pane from its
    /// window's layout and pane list; the last pane takes the window with it.
    /// Returns the removed panes (just `id` unless the window went).
    pub fn remove_pane(&mut self, id: PaneId) -> Vec<Pane> {
        let Some(wid) = self.panes.get(&id).map(|p| p.window) else {
            return Vec::new();
        };
        let Some(window) = self.windows.get(&wid) else {
            return self.panes.remove(&id).into_iter().collect();
        };
        if window.panes.panes.len() == 1 {
            return self.kill_window(wid);
        }
        // window_push_zoom(w, 0, ...) / window_pop_zoom: unzoom around the
        // removal; the flag is only set for float-over-zoom panes, which
        // vtmux does not have, so the window stays unzoomed.
        self.unzoom(wid);
        let window = self.windows.get_mut(&wid).expect("checked above");
        window.layout.close_pane(id);
        window.panes.remove(id);
        self.options.remove_pane(id);
        let pane = self.panes.remove(&id);
        self.fix_panes(wid);
        pane.into_iter().collect()
    }

    /// `window_zoom` (window.c:971-1020): false when already zoomed or the
    /// window has one pane.
    pub fn zoom(&mut self, w: WindowId, p: PaneId) -> bool {
        let Some(window) = self.windows.get(&w) else {
            return false;
        };
        if window.zoomed() || window.panes.panes.len() == 1 {
            return false;
        }
        if window.active() != p {
            self.set_active(w, p);
        }
        let window = self.windows.get_mut(&w).expect("checked above");
        let mut single = Layout::new(window.layout.sx, window.layout.sy, p);
        single.opts = window.layout.opts;
        let saved = std::mem::replace(&mut window.layout, single);
        window.zoom = Some(Zoom { pane: p, saved });
        self.fix_panes(w);
        true
    }

    /// `resize_window` (resize.c:27-69): unzoom, resize the layout (the
    /// window is no smaller than it), give the panes their new rectangles
    /// (terminal and PTY), and zoom again.
    pub fn resize_window(&mut self, w: WindowId, sx: u32, sy: u32) {
        let zoomed = self
            .windows
            .get(&w)
            .and_then(|win| win.zoom.as_ref().map(|z| z.pane));
        self.unzoom(w);
        let Some(window) = self.windows.get_mut(&w) else {
            return;
        };
        window.layout.resize_window(sx, sy);
        self.fix_panes(w);
        if let Some(p) = zoomed {
            self.zoom(w, p);
        }
    }

    /// `window_unzoom` (window.c:1022-1080): false when not zoomed.
    pub fn unzoom(&mut self, w: WindowId) -> bool {
        let Some(window) = self.windows.get_mut(&w) else {
            return false;
        };
        let Some(z) = window.zoom.take() else {
            return false;
        };
        window.layout = z.saved;
        self.fix_panes(w);
        true
    }
}

/// A direction for [`Model::neighbour`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Up,
    Down,
    Left,
    Right,
}

impl Model {
    /// `window_pane_find_up/down/left/right` (window.c:2220-2440): the panes
    /// whose edge touches `p`'s on that side (wrapping round the window),
    /// overlapping it along the edge; the most recently active wins
    /// (`window_pane_choose_best`). Scrollbars are not modelled, so the
    /// full-size offset is the pane's own.
    #[must_use]
    pub fn neighbour(&self, p: PaneId, side: Side) -> Option<PaneId> {
        let pane = self.panes.get(&p)?;
        let w = self.windows.get(&pane.window)?;
        let status = self.pane_status(w.id);
        let (wsx, wsy) = (to_i64(w.sx()), to_i64(w.sy()));
        let r = Rect::of(pane);
        // The edge the neighbour's opposite side must sit on, and the span
        // it must overlap.
        let (edge, lo, hi) = match side {
            Side::Up => {
                let edge = match (status, r.y) {
                    (PaneStatus::Top, 1) | (PaneStatus::Off, 0) => wsy + 1,
                    (PaneStatus::Bottom, 0) => wsy,
                    _ => r.y,
                };
                (edge, r.x, r.x + r.w)
            }
            Side::Down => {
                let mut edge = r.y + r.h + 1;
                match status {
                    PaneStatus::Top if edge >= wsy => edge = 1,
                    PaneStatus::Bottom if edge >= wsy - 1 => edge = 0,
                    PaneStatus::Off if edge >= wsy => edge = 0,
                    _ => {}
                }
                (edge, r.x, r.x + r.w)
            }
            Side::Left => {
                let edge = if r.x == 0 { wsx + 1 } else { r.x };
                (edge, r.y, r.y + r.h)
            }
            Side::Right => {
                let mut edge = r.x + r.w + 1;
                if edge >= wsx {
                    edge = 0;
                }
                (edge, r.y, r.y + r.h)
            }
        };
        w.panes
            .panes
            .iter()
            .filter(|&&q| q != p)
            .filter_map(|q| self.panes.get(q))
            .filter(|q| {
                let n = Rect::of(q);
                let (at, start, len) = match side {
                    Side::Up => (n.y + n.h + 1, n.x, n.w),
                    Side::Down => (n.y, n.x, n.w),
                    Side::Left => (n.x + n.w + 1, n.y, n.h),
                    Side::Right => (n.x, n.y, n.h),
                };
                let end = start + len - 1;
                at == edge
                    && ((start < lo && end > hi)
                        || (start >= lo && start <= hi)
                        || (end >= lo && end <= hi))
            })
            // The first of equal active points, as the C loop's `>` keeps.
            .fold(None::<&Pane>, |best, q| match best {
                Some(b) if q.active_point <= b.active_point => Some(b),
                _ => Some(q),
            })
            .map(|q| q.id)
    }

    /// `window_find_string` (window.c:930-969): `top`, `bottom-left` and the
    /// other position names as the pane at that point of the window.
    #[must_use]
    pub fn pane_at_position(&self, w: WindowId, name: &str) -> Option<PaneId> {
        let window = self.windows.get(&w)?;
        let (sx, sy) = (window.sx(), window.sy());
        let (mut x, mut y) = (sx / 2, sy / 2);
        let (mut top, mut bottom) = (0, sy.saturating_sub(1));
        match self.pane_status(w) {
            PaneStatus::Top => top += 1,
            PaneStatus::Bottom => bottom = bottom.saturating_sub(1),
            PaneStatus::Off => {}
        }
        let right = sx.saturating_sub(1);
        match name.to_ascii_lowercase().as_str() {
            "top" => y = top,
            "bottom" => y = bottom,
            "left" => x = 0,
            "right" => x = right,
            "top-left" => (x, y) = (0, top),
            "top-right" => (x, y) = (right, top),
            "bottom-left" => (x, y) = (0, bottom),
            "bottom-right" => (x, y) = (right, bottom),
            _ => return None,
        }
        self.pane_at(w, to_i64(x), to_i64(y))
    }

    /// `window_get_active_at` (window.c:849-928) without modal panes:
    /// floating panes first (with their border), then the pane status line
    /// of a tiled pane, then any visible pane in z-order, tiled ones up to
    /// and including their right and bottom border.
    #[must_use]
    pub fn pane_at(&self, w: WindowId, x: i64, y: i64) -> Option<PaneId> {
        let window = self.windows.get(&w)?;
        let status = self.pane_status(w);
        let stack: Vec<&Pane> = window
            .panes
            .z_order
            .iter()
            .filter_map(|p| self.panes.get(p))
            .collect();
        let floating = |p: &Pane| window.is_floating(p.id);
        let visible = |p: &Pane| window.is_visible(p.id);
        let in_border = |r: Rect| x >= r.x - 1 && x <= r.x + r.w && y >= r.y - 1 && y <= r.y + r.h;
        if let Some(p) = stack
            .iter()
            .find(|p| floating(p) && visible(p) && in_border(Rect::of(p)))
        {
            return Some(p.id);
        }
        if status == PaneStatus::Top
            && let Some(p) = stack.iter().find(|p| {
                let r = Rect::of(p);
                visible(p) && !floating(p) && x >= r.x && x <= r.x + r.w && y == r.y - 1
            })
        {
            return Some(p.id);
        }
        stack
            .iter()
            .find(|p| {
                if !visible(p) {
                    return false;
                }
                let r = Rect::of(p);
                if floating(p) {
                    return in_border(r);
                }
                let top = if status == PaneStatus::Top {
                    r.y - 1
                } else {
                    r.y
                };
                x >= r.x && x <= r.x + r.w && y >= top && y <= r.y + r.h
            })
            .map(|p| p.id)
    }
}

fn to_i64(v: u32) -> i64 {
    i64::from(v)
}

/// A pane's rectangle in signed arithmetic, as the C compares it.
#[derive(Clone, Copy)]
struct Rect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Rect {
    fn of(p: &Pane) -> Self {
        Self {
            x: i64::from(p.rect.xoff),
            y: i64::from(p.rect.yoff),
            w: i64::from(p.rect.sx),
            h: i64::from(p.rect.sy),
        }
    }
}

impl Pane {
    /// `window_pane_resize` (window.c:1702-1740) plus the pane offsets: the
    /// terminal reflows and the PTY gets `TIOCSWINSZ` when the size changed.
    pub fn set_rect(&mut self, rect: Geometry) {
        let resized = (self.rect.sx, self.rect.sy) != (rect.sx, rect.sy);
        self.rect = rect;
        if !resized {
            return;
        }
        let cols = u16::try_from(rect.sx).unwrap_or(u16::MAX);
        let rows = u16::try_from(rect.sy).unwrap_or(u16::MAX);
        self.term.resize(cols, rows);
        if let Some(io) = &self.io {
            let _ = io.resize(cols, rows);
        }
    }
}
