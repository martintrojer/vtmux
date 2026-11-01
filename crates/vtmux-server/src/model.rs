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
use vtmux_core::layout::{Cell, Geometry, Layout, NamedLayout, PaneStatus, WindowPanes};
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
    /// `s->activity_time` in epoch seconds (`#{session_activity}`).
    pub activity_secs: i64,
    pub cwd: PathBuf,
    /// Window index -> window (tmux's winlinks).
    pub windows: BTreeMap<u32, WindowId>,
    /// The current window's index.
    pub current: u32,
    /// Previously current windows by index, most recent first (`s->lastw`):
    /// `last-window`, the `!` target and the `-` flag read its head.
    pub last: Vec<u32>,
    /// The session's own environment (update-environment, `-e`).
    pub env: crate::environ::Environ,
    /// Alert flags of the winlinks (`wl->flags & WINLINK_ALERTFLAGS`),
    /// by window index: [`crate::alerts::BELL`] and friends. Absent is 0.
    pub link_alerts: BTreeMap<u32, u8>,
    /// When each winlink was attached to its window, by window index
    /// ([`Model::next_link_stamp`]): tmux appends a link to its window's
    /// `winlinks` list (`winlink_set_window`), and
    /// `#{window_linked_sessions_list}` walks that list. Absent is the
    /// oldest (a window's first link).
    pub link_seq: BTreeMap<u32, u64>,
    /// The shape of tmux's winlinks RB tree for this session, kept through
    /// the same inserts and removes: `session_destroy` unlinks windows
    /// root first (session.c:221-224), which the hooks and control mode
    /// show.
    pub link_tree: rb::Tree,
}

impl Session {
    /// Bring [`Self::link_tree`] up to date after one winlink came or went
    /// (removes, then inserts, in index order).
    pub fn sync_link_tree(&mut self) {
        let tree_keys = self.link_tree.keys();
        for k in &tree_keys {
            if !self.windows.contains_key(k) {
                self.link_tree.remove(*k);
            }
        }
        for k in self.windows.keys() {
            if !tree_keys.contains(k) {
                self.link_tree.insert(*k);
            }
        }
    }
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
    /// `w->activity_time` as an ordering stamp (window.c
    /// `window_update_activity`): creation, selection and pane output.
    pub activity: u64,
    /// The visible layout (`w->layout_root`); one cell while zoomed.
    pub layout: Layout,
    /// Pane order, active pane, last-used stack, z-order.
    pub panes: WindowPanes,
    pub zoom: Option<Zoom>,
    /// `w->lastlayout`: the named layout last applied (select-layout with
    /// no name and next-layout/previous-layout start from it).
    pub last_layout: Option<NamedLayout>,
    /// `w->old_layout`: the layout string before the last select-layout,
    /// for `select-layout -o`.
    pub old_layout: Option<String>,
    /// `WINDOW_RESIZE` with `new_sx`/`new_sy`: the size `recalculate_sizes`
    /// chose, applied once the window is current in an attached session.
    pub pending_size: Option<(u32, u32)>,
    /// `w->manual_sx`/`manual_sy`: the size `window-size manual` keeps;
    /// the creation size until resize-window sets it.
    pub manual_size: (u32, u32),
    /// `w->latest`: the client that last used the window, which sizes it
    /// under `window-size latest` when several clients see it.
    pub latest: Option<crate::server::ClientId>,
    /// The window's alert state (`w->flags`, `alerts_queued`,
    /// `alerts_timer`).
    pub alerts: crate::alerts::WindowAlerts,
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
#[allow(clippy::struct_excessive_bools)] // tmux's PANE_* flags
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
    /// Its wait status once reaped (`wp->status`, `PANE_STATUSREADY`).
    pub status: Option<crate::jobs::JobStatus>,
    /// Output arrived since the last automatic-rename check (tmux's
    /// `PANE_CHANGED`).
    pub changed: bool,
    /// `PANE_UNSEENCHANGES`: output arrived while the pane was in a mode.
    pub unseen_changes: bool,
    pub last_rename: Option<Instant>,
    /// `wp->xoff`, `wp->yoff`, `wp->sx`, `wp->sy` as `layout_fix_panes` set
    /// them.
    pub rect: Geometry,
    /// `grid->hlimit`: the session's `history-limit` when the pane was made,
    /// then as [`Model::update_history`] last set it.
    pub history_limit: usize,
    /// `PANE_INPUTOFF` (select-pane -d / -e).
    pub input_off: bool,
    /// `PANE_FOCUSED`: the pane was last told it has the focus.
    pub focused: bool,
    /// `wp->active_point`: when the pane last became active, for picking the
    /// most recent of several neighbours (`window_pane_choose_best`).
    pub active_point: u64,
    /// `wp->resize_queue`: size changes not yet told to the program, each
    /// `(from, to)`.
    pub resizes: Vec<((u32, u32), (u32, u32))>,
    /// `wp->resize_timer`: until when no queued size is sent.
    pub resize_timer: Option<Instant>,
    /// `wp->modes`: the pane's modes, the one shown first.
    pub modes: Vec<crate::mode::Mode>,
    /// The last copy mode search and whether it was a regular expression
    /// (`wp->searchstr`, `wp->searchregex`): `#{pane_search_string}`, and
    /// what the next copy mode's search-again looks for.
    pub search: Option<(String, bool)>,
    /// `wp->prompt`: a prompt drawn over the pane's last line
    /// (`command-prompt -P`), and the command it answers to
    /// (`wp->prompt_data`).
    pub prompt: Option<crate::prompt::OpenPrompt>,
    pub(crate) prompt_owner: Option<crate::open_prompt::Owner>,
    /// `mtd->prompt`: the shown mode's own prompt, drawn on the mode's
    /// screen, and the mode's callbacks for it (`mtd->prompt_data`).
    pub mode_prompt: Option<crate::mode::prompt::ModePrompt>,
    pub(crate) mode_prompt_owner: Option<crate::open_prompt::Owner>,
}

/// A redraw tmux asks for (server-fn.c); the server applies them to its
/// clients before the next frame. The `*Status` kinds redraw only the
/// status (`server_status_*`); the others the whole client
/// (`server_redraw_*`), which also draws a pane area frozen under a
/// message again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redraw {
    /// `server_redraw_window`: clients whose current window this is.
    Window(WindowId),
    /// `server_status_window`: clients of every session that has it.
    WindowStatus(WindowId),
    /// `server_redraw_session`.
    Session(SessionId),
    /// `server_status_session`.
    SessionStatus(SessionId),
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
    /// The paste buffers (paste.c).
    pub paste: crate::paste::PasteStore,
    activity: u64,
    /// `next_active_point` (window.c:63).
    active_points: u64,
    /// Source of [`Session::link_seq`].
    link_stamp: u64,
    /// Windows queued for the next alert check (`alerts_list`).
    pub(crate) alerts_list: Vec<WindowId>,
    /// Status redraws asked for since the last frame.
    pub(crate) redraws: Vec<Redraw>,
    /// `PANE_REDRAW`: panes drawn whole at the next frame, even where pane
    /// output is frozen under a message; cleared once every client drew
    /// (server-client.c:1929-1940).
    pub(crate) redraw_panes: std::collections::BTreeSet<PaneId>,
    /// The server's own info and environment, which a drawing made inside
    /// the model (`display-panes-format`) expands formats with. Set by
    /// the server; `#{current_file}` is empty there.
    pub(crate) format_base:
        Option<std::rc::Rc<(crate::format_ctx::ServerInfo, crate::environ::Environ)>>,
    /// Events fired since the sinks last heard ([`crate::events`]).
    pub events: Vec<crate::events::Queued>,
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
            paste: crate::paste::PasteStore::default(),
            activity: 0,
            active_points: 0,
            link_stamp: 0,
            alerts_list: Vec::new(),
            redraws: Vec::new(),
            redraw_panes: std::collections::BTreeSet::new(),
            format_base: None,
            events: Vec::new(),
        }
    }
}

impl Model {
    /// Ask for a status redraw ([`Redraw`]).
    pub fn redraw(&mut self, r: Redraw) {
        if !self.redraws.contains(&r) {
            self.redraws.push(r);
        }
    }

    /// The next activity stamp.
    pub fn touch(&mut self) -> u64 {
        self.activity += 1;
        self.activity
    }

    /// `session_update_activity(s, NULL)`: the session is active now.
    pub fn touch_session(&mut self, s: SessionId) {
        let stamp = self.touch();
        if let Some(session) = self.sessions.get_mut(&s) {
            session.activity = stamp;
            session.activity_secs = now_epoch();
        }
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
        Some((session.current, w, self.windows.get(&w)?.panes.active?))
    }

    /// Sessions that link window `w`.
    pub fn sessions_with(&self, w: WindowId) -> impl Iterator<Item = &Session> {
        self.sessions
            .values()
            .filter(move |s| s.windows.values().any(|x| *x == w))
    }

    /// The next [`Session::link_seq`] stamp.
    pub fn next_link_stamp(&mut self) -> u64 {
        self.link_stamp += 1;
        self.link_stamp
    }

    /// Every winlink of window `w` in the order of tmux's `w->winlinks`
    /// list (oldest attachment first).
    #[must_use]
    pub fn window_links(&self, w: WindowId) -> Vec<(&Session, u32)> {
        let mut links: Vec<_> = self
            .sessions
            .values()
            .flat_map(|s| {
                s.windows
                    .iter()
                    .filter(move |(_, x)| **x == w)
                    .map(move |(i, _)| (s, *i, s.link_seq.get(i).copied().unwrap_or(0)))
            })
            .collect();
        links.sort_by_key(|(_, _, stamp)| *stamp);
        links.into_iter().map(|(s, i, _)| (s, i)).collect()
    }

    /// `session_update_history` for every session (options.c:1464-1466,
    /// `options_push_changes("history-limit")`): each session in name order
    /// gives the panes of its windows its `history-limit`, so a window
    /// linked into several sessions ends with the last one's.
    pub fn update_history(&mut self) {
        let limits: Vec<(Vec<WindowId>, usize)> = self
            .sessions_by_name()
            .map(|s| {
                let limit =
                    usize::try_from(self.number_option(TreeId::Session(s.id), "history-limit"))
                        .unwrap_or(0);
                (s.windows.values().copied().collect(), limit)
            })
            .collect();
        for (windows, limit) in limits {
            for w in windows {
                let Some(window) = self.windows.get(&w) else {
                    continue;
                };
                for id in window.panes.panes.clone() {
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.history_limit = limit;
                        p.term.set_history_limit(limit);
                    }
                }
            }
        }
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

    /// `status` (`status_update_cache`, status.c:87-96): how many status
    /// lines `tree`'s session has, 0 when off.
    #[must_use]
    pub fn status_lines(&self, tree: TreeId) -> u32 {
        u32::try_from(self.number_option(tree, "status")).unwrap_or(0)
    }

    /// `status-position top` (`statusat` 0, status.c:92).
    #[must_use]
    pub fn status_top(&self, tree: TreeId) -> bool {
        self.number_option(tree, "status-position") == 0
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

    /// `window_pane_zindex` (window.c:1293): floating panes above count, plus one
    /// for a tiled pane.
    #[must_use]
    pub fn pane_zindex(&self, p: PaneId) -> Option<u32> {
        let w = self.windows.get(&self.panes.get(&p)?.window)?;
        let mut i = 0;
        for &q in &w.panes.z_order {
            if q == p {
                return Some(i + u32::from(!w.is_floating(p)));
            }
            i += u32::from(w.is_floating(q));
        }
        None
    }

    #[must_use]
    pub fn pane_base_index(&self, w: WindowId) -> u32 {
        u32::try_from(self.number_option(TreeId::Window(w), "pane-base-index")).unwrap_or(0)
    }

    /// `window_get_pane_status` (window.c:2918): the `*-floating` values
    /// count as off for tiled geometry, and so does everything under the
    /// focus ring (D23), whose edges carry the status lines instead of a
    /// reserved row.
    #[must_use]
    pub fn pane_status(&self, w: WindowId) -> PaneStatus {
        if self.number_option(TreeId::Window(w), "vtmux-focus-ring") != 0 {
            return PaneStatus::Off;
        }
        self.border_status(w)
    }

    /// `pane-border-status` for tiled panes as drawn: [`Self::pane_status`]
    /// without the focus ring.
    #[must_use]
    pub fn border_status(&self, w: WindowId) -> PaneStatus {
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
        let old = window.panes.active;
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
        // window.c:776-786: the old pane's focus, the new one's, then
        // window-pane-changed.
        if self.focus_events() {
            if let Some(o) = old {
                self.push_focus_check(o);
            }
            self.push_focus_check(p);
        }
        self.redraw(Redraw::Window(w));
        self.fire_pane_changed(w, p, old);
        true
    }

    /// `window_fire_pane_changed` (window.c:119-134).
    pub fn fire_pane_changed(&mut self, w: WindowId, p: PaneId, old: Option<PaneId>) {
        let mut ev = crate::events::Event::new("window-pane-changed")
            .target(crate::target::Found::from_pane(self, p))
            .window(self, "window", w)
            .pane("pane", p)
            .pane("new_pane", p);
        if let Some(o) = old {
            ev = ev.pane("old_pane", o);
        }
        self.push_event(ev);
    }

    /// `window_fire_pane_moved` (window.c:136-156).
    pub fn fire_pane_moved(
        &mut self,
        p: PaneId,
        old: (WindowId, Option<u32>),
        new: (WindowId, Option<u32>),
    ) {
        let mut ev = crate::events::Event::new("pane-moved")
            .target(crate::target::Found::from_pane(self, p))
            .pane("pane", p)
            .window(self, "window", new.0)
            .window(self, "old_window", old.0)
            .window(self, "new_window", new.0);
        if let Some(i) = old.1 {
            ev = ev.int("old_window_index", i);
        }
        if let Some(i) = new.1 {
            ev = ev.int("window_index", i).int("new_window_index", i);
        }
        self.push_event(ev);
    }

    /// Stamp `p` as just made active (`w->active->active_point =
    /// next_active_point++`) when something other than [`Self::set_active`]
    /// changed the active pane (a parsed layout's `"a"`).
    pub fn stamp_active(&mut self, p: PaneId) {
        self.active_points += 1;
        if let Some(pane) = self.panes.get_mut(&p) {
            pane.active_point = self.active_points;
            pane.changed = true;
        }
    }

    /// Set `PANE_CHANGED` on the active pane of `w` if its
    /// `automatic-rename` is on, so the next name check looks at it.
    pub fn mark_for_rename(&mut self, w: WindowId) {
        if self.number_option(TreeId::Window(w), "automatic-rename") == 0 {
            return;
        }
        if let Some(p) = self.windows.get(&w).map(Window::active)
            && let Some(pane) = self.panes.get_mut(&p)
        {
            pane.changed = true;
        }
    }

    /// `layout_fix_panes` + `window_pane_resize`: copy each cell's
    /// rectangle (less the pane status line) to its pane, resizing the
    /// terminal and the PTY of every pane whose size changed.
    pub fn fix_panes(&mut self, w: WindowId) {
        self.fix_panes_except(w, None);
    }

    /// [`Self::fix_panes`] leaving `skip` alone (`layout_fix_panes(w, wp)`,
    /// for a pane spawned zoomed, which keeps the window's size).
    pub fn fix_panes_except(&mut self, w: WindowId, skip: Option<PaneId>) {
        let status = self.pane_status(w);
        let Some(window) = self.windows.get_mut(&w) else {
            return;
        };
        window.layout.opts.status = status;
        if let Some(z) = &mut window.zoom {
            z.saved.opts.status = status;
        }
        let hidden = window.zoom.as_ref().map(|z| z.pane).filter(|z| {
            // window_pane_get_pane_status (window.c:2936-2940): the
            // zoomed pane of a mode that hides it has no status line.
            self.panes.get(z).is_some_and(|x| {
                x.modes
                    .first()
                    .is_some_and(crate::mode::Mode::hides_pane_status)
            })
        });
        for p in window.panes.panes.iter().filter(|p| Some(**p) != skip) {
            // Panes hidden by a zoom keep their last rectangle.
            let Some(mut rect) = window.layout.pane_rect(*p) else {
                continue;
            };
            if hidden == Some(*p)
                && let Some(path) = window.layout.root.find_pane(*p)
            {
                rect = *window.layout.root.get(&path).g();
            }
            if let Some(pane) = self.panes.get_mut(p) {
                pane.set_rect(rect);
            }
        }
        self.panes_resized(w);
    }

    pub fn insert_session(&mut self, session: Session) {
        self.options.add_session(session.id);
        self.by_name.insert(session.name.clone(), session.id);
        self.sessions.insert(session.id, session);
    }

    /// Remove a session and every window no other session links; returns
    /// the panes that went with them (their PTYs close when dropped).
    pub fn remove_session(&mut self, id: SessionId) -> Vec<Pane> {
        if !self.sessions.contains_key(&id) {
            return Vec::new();
        }
        // session_destroy (session.c:195-229): session-closed, then
        // window-unlinked for each winlink in the order the RB tree's root
        // is removed.
        self.fire_session("session-closed", id, false);
        let order = self.sessions[&id].link_tree.root_order();
        for index in order {
            let w = self.sessions[&id].windows[&index];
            let ev = crate::events::Event::new("window-unlinked")
                .session(self, "session", id)
                .window(self, "window", w)
                .int("window_index", index)
                .linked_now(self, w);
            self.push_event(ev);
        }
        let session = self.sessions.remove(&id).expect("checked above");
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

    /// Drop window `w` and its panes (not its links): `window-closed`
    /// as its last reference goes (window.c:525-530).
    fn remove_window(&mut self, w: WindowId) -> Vec<Pane> {
        if !self.windows.contains_key(&w) {
            return Vec::new();
        }
        self.fire_window("window-closed", w);
        let window = self.windows.remove(&w).expect("checked above");
        self.options.remove_window(w);
        let mut gone = Vec::new();
        for p in window.panes.panes {
            self.options.remove_pane(p);
            gone.extend(self.panes.remove(&p));
        }
        gone
    }

    /// `server_kill_window` (server-fn.c:239-262): unlink window `w` from
    /// every session that has it, in name order (`session_detach`); a
    /// session left without windows is destroyed, and with `renumber` each
    /// session is renumbered if its `renumber-windows` is on. Returns the
    /// panes that went (their PTYs close when dropped).
    pub fn kill_window(&mut self, w: WindowId, renumber: bool) -> Vec<Pane> {
        let linked: Vec<SessionId> = self
            .sessions_by_name()
            .filter(|s| s.windows.values().any(|x| *x == w))
            .map(|s| s.id)
            .collect();
        let mut gone = Vec::new();
        for s in linked {
            // server-fn.c:255: server_redraw_session_group.
            self.redraw(Redraw::Session(s));
            self.unzoom(w);
            while let Some(index) = self.index_of(s, w) {
                if self.detach_window(s, index) {
                    gone.extend(self.remove_session(s));
                    break;
                }
            }
            if renumber {
                self.renumber_if_set(s);
            }
        }
        gone.extend(self.remove_window(w));
        gone
    }

    /// The first index at which session `s` links window `w`
    /// (`winlink_find_by_window`).
    #[must_use]
    pub fn index_of(&self, s: SessionId, w: WindowId) -> Option<u32> {
        let session = self.sessions.get(&s)?;
        session
            .windows
            .iter()
            .find(|(_, x)| **x == w)
            .map(|(i, _)| *i)
    }

    /// Remove the link at `index` from session `s` and destroy the window
    /// if nothing links it any more (`winlink_remove` dropping the last
    /// reference). The session's current window is not changed.
    pub fn unlink_index(&mut self, s: SessionId, index: u32) -> Vec<Pane> {
        let Some(session) = self.sessions.get_mut(&s) else {
            return Vec::new();
        };
        let Some(&w) = session.windows.get(&index) else {
            return Vec::new();
        };
        self.fire_winlink("window-unlinked", s, index, w);
        let session = self.sessions.get_mut(&s).expect("checked above");
        session.last.retain(|i| *i != index);
        session.link_alerts.remove(&index);
        session.link_seq.remove(&index);
        session.windows.remove(&index);
        session.sync_link_tree();
        if self.sessions_with(w).next().is_none() {
            self.remove_window(w)
        } else {
            Vec::new()
        }
    }

    /// `session_detach` (session.c:338-358): unlink `index` from `s`,
    /// moving the current window away from it first (last, else previous,
    /// else next). True when it is the session's only window: the caller
    /// destroys the session instead.
    pub fn detach_window(&mut self, s: SessionId, index: u32) -> bool {
        let Some(session) = self.sessions.get(&s) else {
            return false;
        };
        if session.windows.len() == 1 && session.windows.contains_key(&index) {
            return true;
        }
        if session.current == index
            && self.last_window(s) != Some(true)
            && self.previous_window(s, false) != Some(true)
        {
            self.next_window(s, false);
        }
        if let Some(&w) = self.sessions.get(&s).and_then(|x| x.windows.get(&index)) {
            self.fire_winlink("window-unlinked", s, index, w);
        }
        if let Some(session) = self.sessions.get_mut(&s) {
            session.last.retain(|i| *i != index);
            session.link_alerts.remove(&index);
            session.link_seq.remove(&index);
            session.windows.remove(&index);
            session.sync_link_tree();
        }
        false
    }

    /// Destroy window `w` if no session links it any more (the last
    /// `winlink_remove` dropping its reference).
    pub fn drop_if_unlinked(&mut self, w: WindowId) -> Vec<Pane> {
        if self.sessions_with(w).next().is_none() {
            self.remove_window(w)
        } else {
            Vec::new()
        }
    }

    /// `session_set_current` (session.c:520-545): `None` when there is no
    /// window at `index` (-1), `Some(false)` when it already is current
    /// (1), `Some(true)` when it became current (0). The old current goes
    /// to the head of the last-window stack and the window's activity is
    /// stamped.
    pub fn set_current(&mut self, s: SessionId, index: u32) -> Option<bool> {
        let session = self.sessions.get(&s)?;
        let w = *session.windows.get(&index)?;
        if session.current == index {
            return Some(false);
        }
        let stamp = self.touch();
        let session = self.sessions.get_mut(&s).expect("checked above");
        let old = session.current;
        let old_w = session.windows.get(&old).copied();
        session.last.retain(|i| *i != index && *i != old);
        if session.windows.contains_key(&old) {
            session.last.insert(0, old);
        }
        session.current = index;
        if let Some(window) = self.windows.get_mut(&w) {
            window.activity = stamp;
        }
        // session_set_current: winlink_clear_flags, then
        // window_update_activity.
        // session.c:516-520: the old window's focus, then the new one's.
        if self.focus_events() {
            let actives: Vec<PaneId> = old_w
                .into_iter()
                .chain([w])
                .filter_map(|x| self.windows.get(&x).map(Window::active))
                .collect();
            for p in actives {
                self.push_focus_check(p);
            }
        }
        self.alerts_clear_window(w);
        self.alerts_queue(w, crate::alerts::ACTIVITY);
        self.fire_window_changed(s, index, old_w.map(|ow| (ow, old)));
        Some(true)
    }

    /// `session_fire_window_changed` (session.c:479-498).
    pub fn fire_window_changed(&mut self, s: SessionId, index: u32, old: Option<(WindowId, u32)>) {
        let Some(&w) = self.sessions.get(&s).and_then(|x| x.windows.get(&index)) else {
            return;
        };
        let mut ev = crate::events::Event::new("session-window-changed")
            .target(crate::target::Found::from_winlink(self, s, index))
            .session(self, "session", s)
            .window(self, "window", w)
            .window(self, "new_window", w)
            .int("window_index", index)
            .int("new_window_index", index);
        if let Some((ow, oi)) = old {
            ev = ev
                .window(self, "old_window", ow)
                .int("old_window_index", oi);
        }
        self.push_event(ev);
    }

    /// `session_last` (session.c:500-512), same results as
    /// [`Self::set_current`].
    pub fn last_window(&mut self, s: SessionId) -> Option<bool> {
        let session = self.sessions.get(&s)?;
        let head = *session.last.first()?;
        if head == session.current {
            return Some(false);
        }
        self.set_current(s, head)
    }

    /// `session_next` (session.c:424-440), wrapping to the first window.
    /// With `alert` only windows with an alert flag count.
    pub fn next_window(&mut self, s: SessionId, alert: bool) -> Option<bool> {
        let session = self.sessions.get(&s)?;
        let counts = |i: &&u32| !alert || session.link_alerts.get(i).is_some_and(|f| *f != 0);
        let next = session
            .windows
            .range(session.current.saturating_add(1)..)
            .map(|(i, _)| i)
            .find(counts)
            .filter(|_| session.current != u32::MAX)
            .or_else(|| session.windows.keys().find(counts))
            .copied()?;
        self.set_current(s, next)
    }

    /// `session_previous` (session.c:454-470), wrapping to the last window.
    pub fn previous_window(&mut self, s: SessionId, alert: bool) -> Option<bool> {
        let session = self.sessions.get(&s)?;
        let counts = |i: &&u32| !alert || session.link_alerts.get(i).is_some_and(|f| *f != 0);
        let prev = session
            .windows
            .range(..session.current)
            .map(|(i, _)| i)
            .rev()
            .find(counts)
            .or_else(|| session.windows.keys().rev().find(counts))
            .copied()?;
        self.set_current(s, prev)
    }

    /// `server_renumber_session` (server-fn.c:265-277): renumber when the
    /// session's `renumber-windows` is on.
    pub fn renumber_if_set(&mut self, s: SessionId) {
        if self.number_option(TreeId::Session(s), "renumber-windows") != 0 {
            self.renumber(s);
        }
    }

    /// `session_renumber_windows` (session.c:745-810): consecutive indices
    /// from `base-index`, keeping the current window and the last-window
    /// stack on the same windows; left alone when they would not fit below
    /// `INT_MAX`.
    pub fn renumber(&mut self, s: SessionId) {
        let base = self.number_option(TreeId::Session(s), "base-index");
        let Some(session) = self.sessions.get_mut(&s) else {
            return;
        };
        let n = i64::try_from(session.windows.len()).unwrap_or(i64::MAX);
        if n > i64::from(i32::MAX) - base + 1 {
            return;
        }
        let base = u32::try_from(base).unwrap_or(0);
        let moved: BTreeMap<u32, u32> = session
            .windows
            .keys()
            .zip(base..)
            .map(|(old, new)| (*old, new))
            .collect();
        session.windows = std::mem::take(&mut session.windows)
            .into_iter()
            .map(|(old, w)| (moved[&old], w))
            .collect();
        // RB_INIT, then each winlink added in order.
        session.link_tree = rb::Tree::default();
        session.sync_link_tree();
        session.current = moved.get(&session.current).copied().unwrap_or(base);
        for i in &mut session.last {
            if let Some(new) = moved.get(i) {
                *i = *new;
            }
        }
        session.link_alerts = std::mem::take(&mut session.link_alerts)
            .into_iter()
            .filter_map(|(old, f)| moved.get(&old).map(|new| (*new, f)))
            .collect();
        // Every winlink is made anew and appended to its window's list.
        let keys: Vec<u32> = session.windows.keys().copied().collect();
        session.link_seq.clear();
        for i in keys {
            self.link_stamp += 1;
            let stamp = self.link_stamp;
            self.sessions
                .get_mut(&s)
                .expect("session")
                .link_seq
                .insert(i, stamp);
        }
    }

    /// `winlink_shuffle_up` (window.c:2481-2510): make room for a window
    /// after (or, with `before`, at) `index` by moving the windows up to
    /// the next free index along by one. `None` when there is no room
    /// below `INT_MAX`.
    pub fn shuffle_up(&mut self, s: SessionId, index: u32, before: bool) -> Option<u32> {
        const INT_MAX: u32 = i32::MAX as u32;
        if index >= INT_MAX {
            return None;
        }
        let session = self.sessions.get_mut(&s)?;
        let idx = if before { index } else { index + 1 };
        let last = (idx..INT_MAX).find(|i| !session.windows.contains_key(i))?;
        let shift = |i: u32| if (idx..last).contains(&i) { i + 1 } else { i };
        session.windows = std::mem::take(&mut session.windows)
            .into_iter()
            .map(|(i, w)| (shift(i), w))
            .collect();
        // window.c:2502-2507: from the top down, each RB_REMOVE then
        // RB_INSERT one up.
        for i in (idx..last).rev() {
            session.link_tree.remove(i);
            session.link_tree.insert(i + 1);
        }
        session.current = shift(session.current);
        for i in &mut session.last {
            *i = shift(*i);
        }
        session.link_alerts = std::mem::take(&mut session.link_alerts)
            .into_iter()
            .map(|(i, f)| (shift(i), f))
            .collect();
        session.link_seq = std::mem::take(&mut session.link_seq)
            .into_iter()
            .map(|(i, f)| (shift(i), f))
            .collect();
        Some(idx)
    }

    /// `session_attach` (session.c:323-337) of window `w` to session `s` at
    /// `index`, or with `None` at the first free index from `base-index`
    /// (`-1 - base-index`). Errors are tmux's causes: `same index: N` when
    /// the window is already there (server-fn.c:307-309) and `index in use:
    /// N`. The session's current window is not changed.
    pub fn link_window(
        &mut self,
        s: SessionId,
        w: WindowId,
        index: Option<u32>,
    ) -> Result<u32, String> {
        let session = self
            .sessions
            .get(&s)
            .ok_or_else(|| "no session".to_owned())?;
        let index = if let Some(i) = index {
            if let Some(x) = session.windows.get(&i) {
                return Err(if *x == w {
                    format!("same index: {i}")
                } else {
                    format!("index in use: {i}")
                });
            }
            i
        } else {
            let base = self.number_option(TreeId::Session(s), "base-index");
            let base = u32::try_from(base).unwrap_or(0);
            self.next_free_index(s, base)
                .ok_or_else(|| format!("index in use: {}", -1 - i64::from(base)))?
        };
        let stamp = self.next_link_stamp();
        let session = self.sessions.get_mut(&s).expect("checked above");
        session.windows.insert(index, w);
        session.link_seq.insert(index, stamp);
        session.sync_link_tree();
        self.fire_winlink("window-linked", s, index, w);
        Ok(index)
    }

    /// `winlink_next_index` (window.c:251-265): the first free index from
    /// `start`, wrapping from `INT_MAX` to 0.
    #[must_use]
    pub fn next_free_index(&self, s: SessionId, start: u32) -> Option<u32> {
        const INT_MAX: u32 = i32::MAX as u32;
        let session = self.sessions.get(&s)?;
        let mut i = start;
        loop {
            if !session.windows.contains_key(&i) {
                return Some(i);
            }
            i = if i >= INT_MAX { 0 } else { i + 1 };
            if i == start {
                return None;
            }
        }
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
            return self.kill_window(wid, true);
        }
        // window_push_zoom(w, 0, ...) / window_pop_zoom: unzoom around the
        // removal; the flag is only set for float-over-zoom panes, which
        // vtmux does not have, so the window stays unzoomed.
        self.unzoom(wid);
        let window = self.windows.get_mut(&wid).expect("checked above");
        // layout_close_pane (layout.c:1548), then window_remove_pane ->
        // window_lost_pane, which fires window-pane-changed for a new
        // active pane (window.c:1215-1219).
        window.layout.close_pane(id);
        let was_active = window.panes.active == Some(id);
        window.panes.remove(id);
        let next = window.panes.active;
        self.fix_panes(wid);
        self.layout_changed(wid);
        if was_active && let Some(n) = next {
            self.fire_pane_changed(wid, n, Some(id));
            // window.c:1219.
            self.push_focus_check(n);
        }
        self.options.remove_pane(id);
        let pane = self.panes.remove(&id);
        // server-fn.c:435 / 567: server_redraw_window.
        self.redraw(Redraw::Window(wid));
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
        // window.c:1014-1015.
        self.fire_window("window-zoomed", w);
        self.layout_changed(w);
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
        window.pending_size = None;
        let old = (window.sx(), window.sy());
        window.layout.resize_window(sx, sy);
        self.fix_panes(w);
        // resize.c:80.
        self.redraw(Redraw::Window(w));
        if let Some(p) = zoomed {
            self.zoom(w, p);
        }
        // resize.c:80-82, resize_fire_window_resized (resize.c:27-40).
        self.layout_changed(w);
        let Some(window) = self.windows.get(&w) else {
            return;
        };
        let (nx, ny) = (window.sx(), window.sy());
        let ev = crate::events::Event::new("window-resized")
            .target(crate::target::Found::from_window(self, w))
            .window(self, "window", w)
            .int("width", nx)
            .int("height", ny)
            .int("old_width", old.0)
            .int("old_height", old.1);
        self.push_event(ev);
    }

    /// `window_push_zoom` (window.c:1108-1124): unzoom, and say whether
    /// [`Self::pop_zoom`] should zoom again (`WINDOW_WASZOOMED`): `flag`
    /// and either `always` or the window was zoomed.
    pub fn push_zoom(&mut self, w: WindowId, always: bool, flag: bool) -> bool {
        let zoomed = self.windows.get(&w).is_some_and(Window::zoomed);
        self.unzoom(w);
        flag && (always || zoomed)
    }

    /// `window_pop_zoom` (window.c:1126-1145): zoom the active pane again
    /// if the matching push asked for it. vtmux has no float-over-zoom
    /// panes, so the active pane is the one zoomed.
    pub fn pop_zoom(&mut self, w: WindowId, was_zoomed: bool) -> bool {
        if !was_zoomed {
            return false;
        }
        let Some(active) = self.windows.get(&w).map(Window::active) else {
            return false;
        };
        self.zoom(w, active)
    }

    /// The pane order and cells of `rotate-window` (cmd-rotate-window.c:
    /// 52-110) for a window already unzoomed: the last pane (`down`) or
    /// the first moves to the other end of the list and every pane takes
    /// the layout cell its new list position had before, so the cells keep
    /// their places. The pane that lands in the active pane's cell becomes
    /// active.
    pub fn rotate_panes(&mut self, w: WindowId, down: bool) {
        let Some(window) = self.windows.get_mut(&w) else {
            return;
        };
        let old = window.panes.panes.clone();
        let mut new = old.clone();
        if down {
            new.rotate_right(1);
        } else {
            new.rotate_left(1);
        }
        // Cell k keeps its place and goes to the pane now at position k.
        let paths: Vec<_> = old
            .iter()
            .map(|p| window.layout.root.find_pane(*p))
            .collect();
        for (path, pane) in paths.into_iter().zip(&new) {
            if let Some(path) = path
                && let Cell::Pane(leaf) | Cell::Floating(leaf) = window.layout.root.get_mut(&path)
            {
                leaf.pane = *pane;
            }
        }
        let active = window.active();
        window.panes.panes.clone_from(&new);
        let at = new.iter().position(|p| *p == active).unwrap_or(0);
        let pick = if down {
            new[..at].last().or(new.last())
        } else {
            new.get(at + 1).or(new.first())
        };
        let pick = pick.copied().unwrap_or(active);
        self.fix_panes(w);
        self.set_active(w, pick);
    }

    /// The window half of `spawn_window` with `SPAWN_RESPAWN`
    /// (spawn.c:106-128): every pane but the first goes (returned, for
    /// their PTYs to close), the first fills the window as the only,
    /// active pane in a fresh layout. An existing zoom is dropped first;
    /// tmux frees the layout under it and crashes on the next read (D-008).
    pub fn reset_window_panes(&mut self, w: WindowId) -> Option<(PaneId, Vec<Pane>)> {
        self.unzoom(w);
        let window = self.windows.get_mut(&w)?;
        let first = *window.panes.panes.first()?;
        let gone: Vec<PaneId> = window.panes.panes.drain(1..).collect();
        window.panes.last.clear();
        window.panes.z_order = vec![first];
        window.panes.active = Some(first);
        let mut layout = Layout::new(window.layout.sx, window.layout.sy, first);
        layout.opts = window.layout.opts;
        window.layout = layout;
        let mut removed = Vec::new();
        for p in gone {
            self.options.remove_pane(p);
            removed.extend(self.panes.remove(&p));
        }
        self.stamp_active(first);
        self.fix_panes(w);
        Some((first, removed))
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
        // window_unzoom(w, 1) (window.c:1072-1075).
        self.fire_window("window-unzoomed", w);
        self.layout_changed(w);
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

/// Give the cell holding `from` to `to` (`lc->wp = ...`); false when
/// `from` has no cell.
pub fn relabel_cell(layout: &mut Layout, from: PaneId, to: PaneId) -> bool {
    let Some(path) = layout.root.find_pane(from) else {
        return false;
    };
    match layout.root.get_mut(&path) {
        Cell::Pane(leaf) | Cell::Floating(leaf) => leaf.pane = to,
        Cell::Node(_) => return false,
    }
    true
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
    /// terminal reflows at once; the program hears of a new size later,
    /// from the queue [`Self::send_resizes`] works through.
    pub fn set_rect(&mut self, rect: Geometry) {
        let from = (self.rect.sx, self.rect.sy);
        self.rect = rect;
        if from == (rect.sx, rect.sy) {
            return;
        }
        self.resizes.push((from, (rect.sx, rect.sy)));
        self.term.resize(clamp16(rect.sx), clamp16(rect.sy));
        // window_pane_resize: the shown mode follows.
        let size = (usize::from(self.term.cols()), usize::from(self.term.rows()));
        match self.modes.first_mut() {
            Some(crate::mode::Mode::Copy(m)) => m.resize(size.0, size.1),
            Some(crate::mode::Mode::Clock(m)) => m.resize(size.0, size.1),
            _ => {}
        }
    }

    /// `server_client_check_pane_resize` (server-client.c:1984-2041): once
    /// the timer has run out, one size goes to the program and the timer
    /// starts again (250 ms). Several queued sizes collapse to the last,
    /// unless they end where they started: then the one before the last is
    /// sent and the last stays queued for 10 ms later, so the program still
    /// sees a change and redraws.
    pub fn send_resizes(&mut self, now: Instant) {
        if self.io.is_none() || self.resizes.is_empty() {
            return;
        }
        if self.resize_timer.is_some_and(|t| t > now) {
            return;
        }
        let first = self.resizes[0];
        let last = self.resizes[self.resizes.len() - 1];
        let mut delay = std::time::Duration::from_millis(250);
        let to = if self.resizes.len() == 1 {
            self.resizes.clear();
            first.1
        } else if last.1 != first.0 {
            self.resizes.clear();
            last.1
        } else {
            let to = self.resizes[self.resizes.len() - 2].1;
            self.resizes.clear();
            self.resizes.push(last);
            delay = std::time::Duration::from_millis(10);
            to
        };
        self.send_size(to);
        self.resize_timer = Some(now + delay);
    }

    /// `screen_write_alternateon` (screen-write.c:3157-3166): entering the
    /// alternate screen sends the pane's size at once if any was queued.
    pub fn flush_resizes(&mut self) {
        if self.resizes.is_empty() {
            return;
        }
        self.resizes.clear();
        self.resize_timer = None;
        self.send_size((self.rect.sx, self.rect.sy));
    }

    /// When the queue next wants a turn of the loop.
    #[must_use]
    pub fn resize_deadline(&self) -> Option<Instant> {
        if self.io.is_none() || self.resizes.is_empty() {
            return None;
        }
        Some(self.resize_timer.unwrap_or_else(Instant::now))
    }

    /// `window_pane_send_resize`: `TIOCSWINSZ`; the kernel sends the
    /// foreground process group SIGWINCH when the size differs.
    fn send_size(&self, (sx, sy): (u32, u32)) {
        if let Some(io) = &self.io {
            let _ = io.resize(clamp16(sx), clamp16(sy));
        }
    }
}

fn clamp16(n: u32) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Seconds since the epoch.
pub(crate) fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Just enough of OpenBSD's `RB_*` (sys/tree.h) to know which key is the
/// root as nodes come and go.
pub mod rb {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Colour {
        Red,
        Black,
    }

    #[derive(Clone, Debug)]
    struct Node {
        key: u32,
        colour: Colour,
        left: Option<usize>,
        right: Option<usize>,
        parent: Option<usize>,
    }

    /// Nodes live in an arena; a removed one stays there unlinked.
    #[derive(Clone, Debug, Default)]
    pub struct Tree {
        nodes: Vec<Node>,
        root: Option<usize>,
    }

    impl Tree {
        fn root_key(&self) -> Option<u32> {
            self.root.map(|r| self.nodes[r].key)
        }

        /// The keys, in order.
        #[must_use]
        pub fn keys(&self) -> Vec<u32> {
            let mut out = Vec::new();
            let mut stack = Vec::new();
            let mut at = self.root;
            while at.is_some() || !stack.is_empty() {
                while let Some(a) = at {
                    stack.push(a);
                    at = self.nodes[a].left;
                }
                let a = stack.pop().expect("not empty");
                out.push(self.nodes[a].key);
                at = self.nodes[a].right;
            }
            out
        }

        /// The keys in the order removing the root until empty takes them
        /// (`while (!RB_EMPTY) remove(RB_ROOT)`).
        #[must_use]
        pub fn root_order(&self) -> Vec<u32> {
            let mut t = self.clone();
            let mut out = Vec::new();
            while let Some(k) = t.root_key() {
                out.push(k);
                t.remove(k);
            }
            out
        }

        fn colour(&self, n: Option<usize>) -> Colour {
            n.map_or(Colour::Black, |n| self.nodes[n].colour)
        }

        fn rotate_left(&mut self, x: usize) {
            let y = self.nodes[x].right.expect("right child");
            let yl = self.nodes[y].left;
            self.nodes[x].right = yl;
            if let Some(l) = yl {
                self.nodes[l].parent = Some(x);
            }
            let p = self.nodes[x].parent;
            self.nodes[y].parent = p;
            match p {
                None => self.root = Some(y),
                Some(p) if self.nodes[p].left == Some(x) => self.nodes[p].left = Some(y),
                Some(p) => self.nodes[p].right = Some(y),
            }
            self.nodes[y].left = Some(x);
            self.nodes[x].parent = Some(y);
        }

        fn rotate_right(&mut self, x: usize) {
            let y = self.nodes[x].left.expect("left child");
            let yr = self.nodes[y].right;
            self.nodes[x].left = yr;
            if let Some(r) = yr {
                self.nodes[r].parent = Some(x);
            }
            let p = self.nodes[x].parent;
            self.nodes[y].parent = p;
            match p {
                None => self.root = Some(y),
                Some(p) if self.nodes[p].right == Some(x) => self.nodes[p].right = Some(y),
                Some(p) => self.nodes[p].left = Some(y),
            }
            self.nodes[y].right = Some(x);
            self.nodes[x].parent = Some(y);
        }

        /// `RB_INSERT` + `RB_INSERT_COLOR`.
        pub fn insert(&mut self, key: u32) {
            let n = self.nodes.len();
            let mut parent = None;
            let mut at = self.root;
            while let Some(a) = at {
                parent = Some(a);
                at = if key < self.nodes[a].key {
                    self.nodes[a].left
                } else {
                    self.nodes[a].right
                };
            }
            self.nodes.push(Node {
                key,
                colour: Colour::Red,
                left: None,
                right: None,
                parent,
            });
            match parent {
                None => self.root = Some(n),
                Some(p) if key < self.nodes[p].key => self.nodes[p].left = Some(n),
                Some(p) => self.nodes[p].right = Some(n),
            }
            let mut elm = n;
            while let Some(p) = self.nodes[elm]
                .parent
                .filter(|&p| self.nodes[p].colour == Colour::Red)
            {
                let gp = self.nodes[p].parent.expect("red parent has a parent");
                if self.nodes[gp].left == Some(p) {
                    let uncle = self.nodes[gp].right;
                    if self.colour(uncle) == Colour::Red {
                        let u = uncle.expect("red");
                        self.nodes[u].colour = Colour::Black;
                        self.nodes[p].colour = Colour::Black;
                        self.nodes[gp].colour = Colour::Red;
                        elm = gp;
                        continue;
                    }
                    let mut p = p;
                    if self.nodes[p].right == Some(elm) {
                        self.rotate_left(p);
                        std::mem::swap(&mut p, &mut elm);
                    }
                    self.nodes[p].colour = Colour::Black;
                    self.nodes[gp].colour = Colour::Red;
                    self.rotate_right(gp);
                } else {
                    let uncle = self.nodes[gp].left;
                    if self.colour(uncle) == Colour::Red {
                        let u = uncle.expect("red");
                        self.nodes[u].colour = Colour::Black;
                        self.nodes[p].colour = Colour::Black;
                        self.nodes[gp].colour = Colour::Red;
                        elm = gp;
                        continue;
                    }
                    let mut p = p;
                    if self.nodes[p].left == Some(elm) {
                        self.rotate_right(p);
                        std::mem::swap(&mut p, &mut elm);
                    }
                    self.nodes[p].colour = Colour::Black;
                    self.nodes[gp].colour = Colour::Red;
                    self.rotate_left(gp);
                }
            }
            let r = self.root.expect("not empty");
            self.nodes[r].colour = Colour::Black;
        }

        fn find(&self, key: u32) -> Option<usize> {
            let mut at = self.root;
            while let Some(a) = at {
                if key == self.nodes[a].key {
                    return Some(a);
                }
                at = if key < self.nodes[a].key {
                    self.nodes[a].left
                } else {
                    self.nodes[a].right
                };
            }
            None
        }

        fn set_child(&mut self, parent: Option<usize>, old: usize, new: Option<usize>) {
            match parent {
                None => self.root = new,
                Some(p) if self.nodes[p].left == Some(old) => self.nodes[p].left = new,
                Some(p) => self.nodes[p].right = new,
            }
        }

        /// `RB_REMOVE` + `RB_REMOVE_COLOR` (the sys/tree.h algorithm: a
        /// node with two children is replaced by its in-order successor).
        pub fn remove(&mut self, key: u32) {
            let Some(elm) = self.find(key) else { return };
            let (child, parent, colour);
            if self.nodes[elm].left.is_none() || self.nodes[elm].right.is_none() {
                child = self.nodes[elm].left.or(self.nodes[elm].right);
                parent = self.nodes[elm].parent;
                colour = self.nodes[elm].colour;
                if let Some(c) = child {
                    self.nodes[c].parent = parent;
                }
                self.set_child(parent, elm, child);
            } else {
                let mut next = self.nodes[elm].right.expect("two children");
                while let Some(l) = self.nodes[next].left {
                    next = l;
                }
                child = self.nodes[next].right;
                let mut par = self.nodes[next].parent;
                colour = self.nodes[next].colour;
                if let Some(c) = child {
                    self.nodes[c].parent = par;
                }
                self.set_child(par, next, child);
                if par == Some(elm) {
                    par = Some(next);
                }
                // next takes elm's place.
                let (l, r, p, c) = {
                    let e = &self.nodes[elm];
                    (e.left, e.right, e.parent, e.colour)
                };
                self.nodes[next].left = l;
                self.nodes[next].right = r;
                self.nodes[next].parent = p;
                self.nodes[next].colour = c;
                self.set_child(p, elm, Some(next));
                if let Some(l) = l {
                    self.nodes[l].parent = Some(next);
                }
                if let Some(r) = r {
                    self.nodes[r].parent = Some(next);
                }
                parent = par;
            }
            if colour == Colour::Black {
                self.remove_colour(parent, child);
            }
        }

        fn remove_colour(&mut self, mut parent: Option<usize>, mut elm: Option<usize>) {
            while (elm.is_none() || self.colour(elm) == Colour::Black) && elm != self.root {
                let Some(p) = parent else { break };
                if self.nodes[p].left == elm {
                    let mut tmp = self.nodes[p].right.expect("sibling");
                    if self.nodes[tmp].colour == Colour::Red {
                        self.nodes[tmp].colour = Colour::Black;
                        self.nodes[p].colour = Colour::Red;
                        self.rotate_left(p);
                        tmp = self.nodes[p].right.expect("sibling");
                    }
                    if self.colour(self.nodes[tmp].left) == Colour::Black
                        && self.colour(self.nodes[tmp].right) == Colour::Black
                    {
                        self.nodes[tmp].colour = Colour::Red;
                        elm = Some(p);
                        parent = self.nodes[p].parent;
                    } else {
                        if self.colour(self.nodes[tmp].right) == Colour::Black {
                            if let Some(l) = self.nodes[tmp].left {
                                self.nodes[l].colour = Colour::Black;
                            }
                            self.nodes[tmp].colour = Colour::Red;
                            self.rotate_right(tmp);
                            tmp = self.nodes[p].right.expect("sibling");
                        }
                        self.nodes[tmp].colour = self.nodes[p].colour;
                        self.nodes[p].colour = Colour::Black;
                        if let Some(r) = self.nodes[tmp].right {
                            self.nodes[r].colour = Colour::Black;
                        }
                        self.rotate_left(p);
                        elm = self.root;
                        break;
                    }
                } else {
                    let mut tmp = self.nodes[p].left.expect("sibling");
                    if self.nodes[tmp].colour == Colour::Red {
                        self.nodes[tmp].colour = Colour::Black;
                        self.nodes[p].colour = Colour::Red;
                        self.rotate_right(p);
                        tmp = self.nodes[p].left.expect("sibling");
                    }
                    if self.colour(self.nodes[tmp].left) == Colour::Black
                        && self.colour(self.nodes[tmp].right) == Colour::Black
                    {
                        self.nodes[tmp].colour = Colour::Red;
                        elm = Some(p);
                        parent = self.nodes[p].parent;
                    } else {
                        if self.colour(self.nodes[tmp].left) == Colour::Black {
                            if let Some(r) = self.nodes[tmp].right {
                                self.nodes[r].colour = Colour::Black;
                            }
                            self.nodes[tmp].colour = Colour::Red;
                            self.rotate_left(tmp);
                            tmp = self.nodes[p].left.expect("sibling");
                        }
                        self.nodes[tmp].colour = self.nodes[p].colour;
                        self.nodes[p].colour = Colour::Black;
                        if let Some(l) = self.nodes[tmp].left {
                            self.nodes[l].colour = Colour::Black;
                        }
                        self.rotate_right(p);
                        elm = self.root;
                        break;
                    }
                }
            }
            if let Some(e) = elm {
                self.nodes[e].colour = Colour::Black;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        /// Reference (tmux 3.8, kill-session with a window-unlinked hook
        /// appending `#{hook_window_index}` to an option).
        #[test]
        fn root_order_matches_reference() {
            let order = |n: u32| {
                let mut t = super::Tree::default();
                for k in 0..n {
                    t.insert(k);
                }
                t.root_order()
            };
            assert_eq!(order(2), [0, 1]);
            assert_eq!(order(3), [1, 2, 0]);
            assert_eq!(order(5), [1, 2, 3, 4, 0]);
            assert_eq!(order(8), [3, 4, 5, 6, 1, 2, 7, 0]);
            assert_eq!(order(10), [3, 4, 5, 6, 7, 8, 1, 2, 9, 0]);
            assert_eq!(
                order(16),
                [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 1, 2, 15, 0]
            );
            // Three windows, the last moved away: 1 then 0.
            let mut t = super::Tree::default();
            for k in 0..3 {
                t.insert(k);
            }
            t.remove(2);
            assert_eq!(t.root_order(), [1, 0]);
            assert_eq!(t.keys(), [0, 1]);
        }
    }
}
