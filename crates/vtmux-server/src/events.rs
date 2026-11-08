//! The event layer (events.c, events-payload.c): something happened (a
//! session was created, a window renamed, a command ran), described by a
//! name, a target and a payload of named values. Three kinds of sink hear
//! each event, in this order (server.c:218-219 builds control's before the
//! hooks'; `wait-for -E` adds its own later): control-mode notifications
//! ([`crate::control`]), hooks (hooks.c) and `wait-for -E` waiters.
//!
//! The model and the commands queue events on [`Model::events`] as they
//! change things, so the order is the order tmux fires them in;
//! [`Server::drain_events`] hands them to the sinks after each command and
//! at the points of the loop that change state. Names and values are taken
//! when the event is made, as tmux's payload holds references to objects
//! that may be gone by the time a hook's commands run.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use vtmux_core::cmd::{CommandCall, CommandList, ConfigEnv};
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::options::{OptionValue, TreeId, Value as OptValue};

use crate::cmdq::{End, NewBatch, StepId, StepState};
use crate::commands::Item;
use crate::model::{Model, now_epoch};
use crate::server::{ClientId, GlobalEnv, Server};
use crate::target::{self, Found};

/// One payload value (`struct event_payload_item`), with the names of the
/// objects as they were when the event fired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Int(i64),
    Client(ClientId, String),
    Session(SessionId, String),
    Window(WindowId, String),
    Pane(PaneId),
}

impl Value {
    /// `event_payload_item_print`.
    #[must_use]
    pub fn print(&self) -> String {
        match self {
            Self::Text(s) | Self::Client(_, s) => s.clone(),
            Self::Int(n) => n.to_string(),
            Self::Session(s, _) => s.to_string(),
            Self::Window(w, _) => w.to_string(),
            Self::Pane(p) => p.to_string(),
        }
    }
}

/// One event: its name, payload (by name, in byte order like tmux's RB
/// tree) and target (`event_payload_set_target`).
#[derive(Clone, Debug, Default)]
pub struct Event {
    pub name: String,
    pub items: BTreeMap<String, Value>,
    pub target: Option<Found>,
    /// `_cmdq_item`: the command an `after-*` or `command-error` event is
    /// for; its hooks go in right after it.
    pub step: Option<StepId>,
    /// That command's name (`#{command}` in its hooks' formats).
    pub command: Option<&'static str>,
    /// The sessions that linked the event's window when it fired: control
    /// mode says `%window-*` to their clients and `%unlinked-window-*` to
    /// the rest (control-notify.c:128-203).
    pub linked: BTreeSet<SessionId>,
    /// For window-layout-changed: `#{window_raw_flags}` of the window in
    /// each session linking it when it fired, as control-notify.c:62-104
    /// expands them (a later select-window in the same command does not
    /// change them).
    pub flags: BTreeMap<SessionId, String>,
    /// `_hooks_monitor`: a monitor hook's event, fired by the monitor of
    /// the hook in this tree. Only that hook runs for it.
    pub monitor: Option<TreeId>,
    /// A `window-layout-changed`'s window layouts when it fired.
    pub layouts: Option<Rc<Layouts>>,
}

/// A window's layouts as `#{window_layout}` and `#{window_visible_layout}`
/// dump them, in both forms. tmux's control sink formats `%layout-change`
/// as the event fires (control-notify.c `control_window_layout_changed_cb`),
/// so a command that unzooms (window.c `window_unzoom` fires one) and then
/// rearranges or zooms again reports the unzoomed layout first.
#[derive(Clone, Debug)]
pub struct Layouts {
    pub layout: Option<String>,
    pub visible: Option<String>,
    pub layout_v1: String,
    pub visible_v1: String,
    /// The `Z` of `#{window_raw_flags}`.
    pub zoomed: bool,
}

impl Event {
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn target(mut self, f: Found) -> Self {
        self.target = Some(f);
        self
    }

    #[must_use]
    pub fn text(mut self, key: &str, value: impl Into<String>) -> Self {
        self.items.insert(key.to_owned(), Value::Text(value.into()));
        self
    }

    #[must_use]
    pub fn int(mut self, key: &str, value: impl Into<i64>) -> Self {
        self.items.insert(key.to_owned(), Value::Int(value.into()));
        self
    }

    #[must_use]
    pub fn client(mut self, key: &str, id: ClientId, name: String) -> Self {
        self.items.insert(key.to_owned(), Value::Client(id, name));
        self
    }

    #[must_use]
    pub fn session_named(mut self, key: &str, s: SessionId, name: String) -> Self {
        self.items.insert(key.to_owned(), Value::Session(s, name));
        self
    }

    #[must_use]
    pub fn session(self, model: &Model, key: &str, s: SessionId) -> Self {
        let name = model
            .sessions
            .get(&s)
            .map(|x| x.name.clone())
            .unwrap_or_default();
        self.session_named(key, s, name)
    }

    #[must_use]
    pub fn window_named(mut self, key: &str, w: WindowId, name: String) -> Self {
        self.items.insert(key.to_owned(), Value::Window(w, name));
        self
    }

    #[must_use]
    pub fn window(self, model: &Model, key: &str, w: WindowId) -> Self {
        let name = model
            .windows
            .get(&w)
            .map(|x| x.name.clone())
            .unwrap_or_default();
        self.window_named(key, w, name)
    }

    #[must_use]
    pub fn pane(mut self, key: &str, p: PaneId) -> Self {
        self.items.insert(key.to_owned(), Value::Pane(p));
        self
    }

    /// The sessions linking `w` now.
    #[must_use]
    pub fn linked_now(mut self, model: &Model, w: WindowId) -> Self {
        self.linked = model.sessions_with(w).map(|s| s.id).collect();
        self
    }

    #[must_use]
    pub fn get_window(&self, key: &str) -> Option<WindowId> {
        match self.items.get(key)? {
            Value::Window(w, _) => Some(*w),
            _ => None,
        }
    }

    #[must_use]
    pub fn get_session(&self, key: &str) -> Option<(SessionId, &str)> {
        match self.items.get(key)? {
            Value::Session(s, n) => Some((*s, n)),
            _ => None,
        }
    }

    #[must_use]
    pub fn get_client(&self, key: &str) -> Option<(ClientId, &str)> {
        match self.items.get(key)? {
            Value::Client(c, n) => Some((*c, n)),
            _ => None,
        }
    }

    #[must_use]
    pub fn get_pane(&self, key: &str) -> Option<PaneId> {
        match self.items.get(key)? {
            Value::Pane(p) => Some(*p),
            _ => None,
        }
    }

    #[must_use]
    pub fn get_text(&self, key: &str) -> Option<&str> {
        match self.items.get(key)? {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// `event_payload_add_formats`: each value as `<prefix><name>`, and a
    /// session's or window's name as `<prefix><name>_name`.
    #[must_use]
    pub fn formats(&self, prefix: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (key, v) in &self.items {
            out.push((format!("{prefix}{key}"), v.print()));
            if let Value::Session(_, n) | Value::Window(_, n) = v {
                out.push((format!("{prefix}{key}_name"), n.clone()));
            }
        }
        out
    }
}

/// The standard events (events.c `events_fire_*`), made against the model
/// as it is now and queued.
impl Model {
    /// Queue an event for the sinks. A target that is not valid now is
    /// dropped: hooks.c:116-119 checks it when the event fires and starts
    /// such a hook from nothing (`cmd_find_from_nothing`).
    pub fn push_event(&mut self, mut ev: Event) {
        if ev.target.is_some_and(|t| !t.still_valid(self)) {
            ev.target = None;
        }
        self.events.push(Queued::Event(ev));
    }

    /// Queue `window_pane_update_focus` for `p`: it runs in order with the
    /// events around it, as tmux runs it where the change happens.
    pub fn push_focus_check(&mut self, p: PaneId) {
        self.events.push(Queued::Focus(p));
    }

    /// Queue `tty_update_window_offset` for window `w`: the views of the
    /// clients showing it are worked out again
    /// ([`crate::server::Server::update_window_offset`]).
    pub fn update_offset(&mut self, w: WindowId) {
        self.events.push(Queued::Offset(w));
    }

    /// `focus-events`, which gates the focus checks in
    /// `window_set_active_pane` and `session_set_current`.
    #[must_use]
    pub fn focus_events(&self) -> bool {
        self.number_option(TreeId::Server, "focus-events") != 0
    }

    /// `events_fire_session`: the target only while the session is alive.
    pub fn fire_session(&mut self, name: &str, s: SessionId, alive: bool) {
        let mut ev = Event::new(name).session(self, "session", s);
        if alive {
            ev = ev.target(Found::from_session(self, s));
        }
        self.push_event(ev);
    }

    /// `events_fire_window`.
    pub fn fire_window(&mut self, name: &str, w: WindowId) {
        let ev = Event::new(name)
            .target(Found::from_window(self, w))
            .window(self, "window", w)
            .linked_now(self, w);
        self.push_event(ev);
    }

    /// `events_fire_pane`.
    pub fn fire_pane(&mut self, name: &str, p: PaneId) {
        let Some(w) = self.panes.get(&p).map(|x| x.window) else {
            return;
        };
        let ev = Event::new(name)
            .target(Found::from_pane(self, p))
            .pane("pane", p)
            .window(self, "window", w);
        self.push_event(ev);
    }

    /// `events_fire_winlink` for session `s`'s window `w` at `index`.
    pub fn fire_winlink(&mut self, name: &str, s: SessionId, index: u32, w: WindowId) {
        let ev = Event::new(name)
            .target(Found::from_winlink(self, s, index))
            .session(self, "session", s)
            .window(self, "window", w)
            .int("window_index", index)
            .linked_now(self, w);
        self.push_event(ev);
    }

    /// `window_set_name` (window.c:103-117, 431-440): the name, and
    /// window-renamed with the old and new names.
    pub fn set_window_name(&mut self, w: WindowId, name: &str) {
        let Some(window) = self.windows.get_mut(&w) else {
            return;
        };
        let old = std::mem::replace(&mut window.name, name.to_owned());
        let ev = Event::new("window-renamed")
            .target(Found::from_window(self, w))
            .window(self, "window", w)
            .text("old_name", old)
            .text("new_name", name)
            .linked_now(self, w);
        self.push_event(ev);
    }

    /// `window-layout-changed`, with the layouts as they are now.
    pub fn layout_changed(&mut self, w: WindowId) {
        let Some(window) = self.windows.get(&w) else {
            self.fire_window("window-layout-changed", w);
            return;
        };
        let mut panes = window.panes.clone();
        panes.base_index = self.pane_base_index(w);
        let layouts = Layouts {
            layout: match &window.zoom {
                Some(z) => z.saved.dump_v2_saved(&panes, z.pane),
                None => window.layout.dump_v2(&panes),
            },
            visible: window.layout.dump_v2(&panes),
            layout_v1: window.full_layout().dump_v1(),
            visible_v1: window.layout.dump_v1(),
            zoomed: window.zoomed(),
        };
        let mut ev = Event::new("window-layout-changed")
            .target(Found::from_window(self, w))
            .window(self, "window", w)
            .linked_now(self, w);
        ev.layouts = Some(Rc::new(layouts));
        ev.flags = ev
            .linked
            .iter()
            .filter_map(|&s| Some((s, self.printable_flags(s, self.index_of(s, w)?, false))))
            .collect();
        self.push_event(ev);
    }
}

/// What [`Model::events`] holds: an event, or a pane whose focus is to be
/// looked at then (it needs the clients, which the model has not got).
#[derive(Clone, Debug)]
pub enum Queued {
    Event(Event),
    Focus(PaneId),
    /// `tty_update_window_offset` for a window.
    Offset(WindowId),
    /// A session went: its clients move on before its `session-closed`
    /// ([`crate::server::Server::session_destroyed`]).
    Destroyed(Destroyed),
}

/// What `server_destroy_session` (server-fn.c:486-527) reads when a session
/// goes, kept for the server, which has the clients.
#[derive(Clone, Debug)]
pub struct Destroyed {
    pub session: SessionId,
    /// The session's `detach-on-destroy` (off, on, no-detached, previous,
    /// next).
    pub mode: i64,
    /// Every session then, the destroyed one included, in name order with
    /// its activity stamp.
    pub sessions: Vec<(SessionId, u64)>,
}

/// A `wait-for -E` waiter (cmd-wait-for.c `struct wait_event_item`).
#[derive(Debug)]
pub struct EventWait {
    pub name: String,
    pub step: StepId,
    pub client: ClientId,
    pub filter: Option<String>,
    pub verbose: bool,
}

impl Server {
    /// Run the `tty_update_window_offset`s queued so far now, the rest of
    /// the queue left as it was: tmux works the views out where the change
    /// happens (window.c:781 in `window_set_active_pane`), before what the
    /// command does next moves the panes again.
    pub(crate) fn flush_offsets(&mut self) {
        let mut windows = Vec::new();
        self.model.events.retain(|q| match q {
            Queued::Offset(w) => {
                windows.push(*w);
                false
            }
            _ => true,
        });
        for w in windows {
            self.update_window_offset(w);
        }
    }

    /// Hand every queued event to the sinks, in order.
    pub(crate) fn drain_events(&mut self) {
        loop {
            for (deleted, name) in self.model.paste.take_changes() {
                let ev = if deleted {
                    "paste-buffer-deleted"
                } else {
                    "paste-buffer-changed"
                };
                self.model
                    .push_event(Event::new(ev).text("paste_buffer", name));
            }
            if self.model.events.is_empty() {
                return;
            }
            let mut queue: std::collections::VecDeque<Queued> =
                std::mem::take(&mut self.model.events).into();
            while let Some(q) = queue.pop_front() {
                match q {
                    Queued::Event(ev) => self.fire_event(&ev),
                    Queued::Focus(p) => {
                        if let Some(ev) = self.pane_focus(p) {
                            self.fire_event(&ev);
                        }
                    }
                    Queued::Offset(w) => self.update_window_offset(w),
                    Queued::Destroyed(d) => {
                        // The client-session-changed events this fires
                        // come before the session's session-closed.
                        self.session_destroyed(&d);
                        for e in std::mem::take(&mut self.model.events).into_iter().rev() {
                            queue.push_front(e);
                        }
                    }
                }
            }
        }
    }

    /// `events_fire`: control, then hooks, then the waiters there were
    /// when it fired.
    /// Every payload names its event (events.c:99).
    pub(crate) fn fire_event(&mut self, ev: &Event) {
        let mut ev = ev.clone();
        ev.items
            .insert("event".to_owned(), Value::Text(ev.name.clone()));
        self.control_event(&ev);
        self.hooks_event(&ev);
        self.waits_event(&ev);
    }

    /// `events_fire_pane` for an event a pane fires on every loop turn it
    /// writes (`pane-activity`), made cheap when only the firing is
    /// counted: outside a command, with no `wait-for -E` waiting for it and
    /// no command in its hook, only what `hooks_insert` does then
    /// (`options_hook_fired`) is done, without the event.
    pub(crate) fn fire_pane_often(&mut self, name: &str, p: PaneId) {
        if !self.cmdq.firing() && !self.event_waits.iter().any(|w| w.name == name) {
            let fs = self.hook_target(Some(Found::from_pane(&self.model, p)));
            let Some(tree) = self.hook_tree(name, fs) else {
                return;
            };
            let idle = self
                .model
                .options
                .tree(tree)
                .get_only(name)
                .is_some_and(|e| match &e.value {
                    OptionValue::Array(items) => {
                        !items.values().any(|v| matches!(v, OptValue::Command(_)))
                    }
                    OptionValue::Single(_) => false,
                });
            if idle {
                self.model.options.hook_fired(tree, name, now_epoch());
                return;
            }
        }
        self.model.fire_pane(name, p);
    }

    /// `events_fire_client`'s target: `cmd_find_from_client`.
    pub(crate) fn client_found(&self, c: ClientId) -> Found {
        match self.clients.get(&c).and_then(|c| c.attached) {
            Some(s) => Found::from_session(&self.model, s),
            None => target::current(&self.model, target::Origin::default()).unwrap_or_default(),
        }
    }

    /// `events_fire_client`.
    pub(crate) fn fire_client(&mut self, name: &str, c: ClientId) {
        let ev = self.client_event(name, c);
        self.model.push_event(ev);
    }

    /// The payload `events_fire_client` and its friends start from.
    pub(crate) fn client_event(&self, name: &str, c: ClientId) -> Event {
        let fs = self.client_found(c);
        let mut ev = Event::new(name)
            .target(fs)
            .client("client", c, self.client_name(c));
        if let Some(s) = fs.session {
            ev = ev.session(&self.model, "session", s);
        }
        if let Some(w) = fs.window {
            ev = ev.window(&self.model, "window", w);
        }
        if let Some(i) = fs.index {
            ev = ev.int("window_index", i);
        }
        if let Some(p) = fs.pane {
            ev = ev.pane("pane", p);
        }
        ev
    }

    /// [`Self::client_event`] for a client on its way out: its session is
    /// the one it left.
    pub(crate) fn lost_client_event(&self, name: &str, c: ClientId) -> Event {
        let left = self
            .clients
            .get(&c)
            .and_then(|x| x.attached.or(x.left_session))
            .filter(|s| self.model.sessions.contains_key(s));
        let fs = left.map_or_else(
            || self.client_found(c),
            |s| Found::from_session(&self.model, s),
        );
        let mut ev = Event::new(name)
            .target(fs)
            .client("client", c, self.client_name(c));
        if let Some(s) = fs.session {
            ev = ev.session(&self.model, "session", s);
        }
        if let Some(w) = fs.window {
            ev = ev.window(&self.model, "window", w);
        }
        if let Some(i) = fs.index {
            ev = ev.int("window_index", i);
        }
        if let Some(p) = fs.pane {
            ev = ev.pane("pane", p);
        }
        ev
    }

    /// `server_client_fire_session_changed` (server-client.c:395-419).
    pub(crate) fn fire_session_changed(&mut self, c: ClientId, old: Option<SessionId>) {
        let mut ev = self.client_event("client-session-changed", c);
        if let Some((s, _)) = ev.get_session("session") {
            ev = ev.session(&self.model, "new_session", s);
        }
        if let Some(o) = old {
            ev = ev.session(&self.model, "old_session", o);
        }
        self.model.push_event(ev);
    }

    // ---------------------------------------------------------- hooks

    /// `hooks_event_cb` (hooks.c:231-248), and `hooks_monitor_hook_cb`
    /// (hooks.c:354-363) for a monitor hook's own event.
    fn hooks_event(&mut self, ev: &Event) {
        if let Some(tree) = ev.monitor {
            self.hooks_monitor_insert(tree, ev);
            return;
        }
        let hook = ev.name.starts_with('@') && self.model.options.is_hook_event(&ev.name)
            || vtmux_core::options::find(&ev.name).is_some_and(|d| d.hook);
        if !hook {
            return;
        }
        if let Some(step) = ev.step {
            self.hooks_insert(Some(step), ev, || ev.formats("hook_"));
            return;
        }
        if !self.cmdq.global_no_hooks() {
            self.hooks_insert(None, ev, || ev.formats("hook_"));
        }
    }

    /// `set-hook -R` (`hooks_run`, hooks.c:304-320): hook `name` now, for
    /// the command's target, right after it.
    pub(crate) fn hooks_run(&mut self, step: Option<StepId>, name: &str, target: Found) {
        let ev = Event::new(name).target(target);
        self.hooks_insert(step, &ev, Vec::new);
    }

    /// `hooks_insert` (hooks.c:100-197): the hook's commands, each array
    /// item a list of its own, after `after` or at the end of the global
    /// queue, run from the event's target with hooks off and the hook
    /// formats (`payload`, made only when there is something to run: a
    /// pane's output fires events every loop turn).
    fn hooks_insert(
        &mut self,
        after: Option<StepId>,
        ev: &Event,
        payload: impl FnOnce() -> Vec<(String, String)>,
    ) {
        let fs = self.hook_target(ev.target);
        let Some((tree, lists)) = self.hook_commands(&ev.name, fs) else {
            return;
        };
        self.hooks_queue(after, ev, fs, tree, lists, payload);
    }

    /// `hooks_insert_event(cmdq_running(NULL), name, ep, hm->oo, 1)` for a
    /// monitor hook: the option in the monitor's own tree, its text
    /// expanded with the payload's `hook_*` formats before it is parsed
    /// (`hooks_parse`); a parse error is only logged. Nothing runs from a
    /// command that has hooks off.
    fn hooks_monitor_insert(&mut self, tree: TreeId, ev: &Event) {
        let fs = self.hook_target(ev.target);
        let Some(entry) = self.model.options.tree(tree).get_only(&ev.name) else {
            return;
        };
        let text = match &entry.value {
            OptionValue::Single(v) => v.to_text(entry.def, false),
            OptionValue::Array(_) => return,
        };
        let payload = ev.formats("hook_");
        let mut ctx = self.format_ctx(fs.unwrap_or_default(), None);
        ctx.extra
            .extend(payload.iter().map(|(k, v)| (k.clone().into(), v.clone())));
        ctx.extra.push(("hook".into(), ev.name.clone()));
        let expanded = vtmux_core::format::expand_str(&text, &ctx);
        drop(ctx);
        let mut env = GlobalEnv::new(&mut self.global_env, &self.model, &self.info);
        let lists: Vec<CommandList> =
            vtmux_core::cmd::parse_string(&expanded, &mut env as &mut dyn ConfigEnv)
                .ok()
                .into_iter()
                .collect();
        self.hooks_queue(None, ev, fs, tree, lists, || payload);
    }

    /// The rest of `hooks_insert`: count the firing, then queue `lists`
    /// with the hook's state.
    fn hooks_queue(
        &mut self,
        after: Option<StepId>,
        ev: &Event,
        fs: Option<Found>,
        tree: TreeId,
        lists: Vec<CommandList>,
        payload: impl FnOnce() -> Vec<(String, String)>,
    ) {
        self.model.options.hook_fired(tree, &ev.name, now_epoch());
        if lists.is_empty() {
            return;
        }
        // format_create(c, item): `command`, then the payload, then `hook`;
        // a later value replaces an earlier one.
        let mut formats: BTreeMap<String, String> = BTreeMap::new();
        if let Some(c) = ev.command {
            formats.insert("command".into(), c.into());
        }
        formats.extend(payload());
        formats.insert("hook".into(), ev.name.clone());
        let state = Rc::new(StepState {
            current: std::cell::Cell::new(fs),
            no_hooks: true,
            formats: formats.into_iter().collect(),
        });
        let groups: crate::cmdq::Groups = lists.into_iter().map(|l| (None, l.calls)).collect();
        if let Some(a) = after {
            self.cmdq_insert_with_state(a, groups, Some(&state));
            return;
        }
        let cwd = self.client_cwd(None, fs.and_then(|f| f.session));
        let mut item = Item::new(None, cwd);
        item.borrow_client = true;
        let mut batch = NewBatch::new(item, Vec::new(), End::Quiet);
        batch.groups = groups;
        batch.state = Some(state);
        self.cmdq_append(None, batch);
    }

    /// `event_payload_get_target` then `cmd_find_from_nothing` when that is
    /// not valid (hooks.c:116-119).
    fn hook_target(&self, t: Option<Found>) -> Option<Found> {
        let m = &self.model;
        if let Some(t) = t {
            if t.still_valid(m) {
                return Some(t);
            }
            // Valid when it fired ([`Model::push_event`]) and gone since
            // (a pane killed by the command that fired it): tmux keeps
            // the state, and its commands find their current target from
            // the client or from nothing when they run (cmd-find.c:995-1006).
            if t.valid() {
                return Some(t);
            }
            if let Some(p) = t.pane.filter(|p| m.panes.contains_key(p)) {
                let f = Found::from_pane(m, p);
                if f.still_valid(m) {
                    return Some(f);
                }
            }
            if let (Some(s), Some(w)) = (t.session, t.window)
                && let Some(i) = m.index_of(s, w)
            {
                return Some(Found::from_winlink(m, s, i));
            }
            if let Some(s) = t.session.filter(|s| m.sessions.contains_key(s)) {
                return Some(Found::from_session(m, s));
            }
        }
        target::best_session(m, None, None).map(|s| Found::from_session(m, s))
    }

    /// The tree of the hook's option (hooks.c:121-140): the session's (or
    /// the global session options), else the pane's, else the window's.
    fn hook_tree(&self, name: &str, fs: Option<Found>) -> Option<TreeId> {
        let store = &self.model.options;
        let session_tree = fs
            .and_then(|f| f.session)
            .map_or(TreeId::GlobalSession, TreeId::Session);
        [
            Some(session_tree),
            fs.and_then(|f| f.pane).map(TreeId::Pane),
            fs.and_then(|f| f.window).map(TreeId::Window),
        ]
        .into_iter()
        .flatten()
        .find_map(|t| store.holder(t, name))
    }

    /// The hook's option ([`Self::hook_tree`]): its tree and its command
    /// lists.
    fn hook_commands(
        &mut self,
        name: &str,
        fs: Option<Found>,
    ) -> Option<(TreeId, Vec<CommandList>)> {
        let tree = self.hook_tree(name, fs)?;
        let entry = self.model.options.tree(tree).get_only(name)?;
        let lists = match &entry.value {
            OptionValue::Array(items) => items
                .values()
                .filter_map(|v| match v {
                    OptValue::Command(list) => Some(list.clone()),
                    _ => None,
                })
                .collect(),
            // A user hook is parsed when it runs; a parse error is only
            // logged (hooks.c:150-160).
            OptionValue::Single(v) => {
                let text = v.to_text(entry.def, false);
                let mut env = GlobalEnv::new(&mut self.global_env, &self.model, &self.info);
                vtmux_core::cmd::parse_string(&text, &mut env as &mut dyn ConfigEnv)
                    .ok()
                    .into_iter()
                    .collect()
            }
        };
        Some((tree, lists))
    }

    /// `cmdq_insert_hook` (cmd-queue.c:358-415): `name` for the command
    /// that just ran in `step`, with its arguments and flags as the
    /// payload.
    pub(crate) fn cmd_hook(
        &mut self,
        name: &str,
        call: &CommandCall,
        step: Option<StepId>,
        target: Option<Found>,
    ) {
        let mut ev = Event::new(name).text("arguments", call.print_args());
        ev.target = target;
        ev.step = step;
        ev.command = Some(call.entry.name);
        let args = &call.args;
        for (i, v) in args.positionals.iter().enumerate() {
            ev = ev.text(&format!("argument_{i}"), v.text());
        }
        for (flag, values) in &args.flags {
            let given: Vec<&str> = values.iter().filter_map(|v| v.as_deref()).collect();
            ev = ev.text(
                &format!("flag_{flag}"),
                given.last().copied().unwrap_or("1"),
            );
            for (i, v) in given.iter().enumerate() {
                ev = ev.text(&format!("flag_{flag}_{i}"), *v);
            }
        }
        self.fire_event(&ev);
    }

    // ------------------------------------------------------ wait-for -E

    /// `cmd_wait_for_event_cb` (cmd-wait-for.c:224-252) for the waiters
    /// on this event.
    fn waits_event(&mut self, ev: &Event) {
        let waiting: Vec<usize> = (0..self.event_waits.len())
            .filter(|&i| self.event_waits[i].name == ev.name)
            .collect();
        let mut done = Vec::new();
        for i in waiting {
            let (step, client, verbose, filter) = {
                let w = &self.event_waits[i];
                (w.step, w.client, w.verbose, w.filter.clone())
            };
            if verbose {
                let lines: Vec<String> = ev
                    .formats("")
                    .into_iter()
                    .filter(|(k, _)| ev.items.contains_key(k))
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                self.cmdq_with_item(step, |_, item| {
                    for l in &lines {
                        item.print(l);
                    }
                });
            }
            if let Some(f) = filter {
                let mut ctx = self.format_ctx(Found::default(), Some(client));
                // event_payload_add_formats adds the payload to the tree,
                // where a format_table name (window_name, session_name) is
                // shadowed by the table's callback, which has no window or
                // session here (format.c format_find).
                ctx.extra.extend(
                    ev.formats("")
                        .into_iter()
                        .filter(|(k, _)| !crate::format_ctx::in_format_table(k))
                        .map(|(k, v)| (k.into(), v)),
                );
                // cmd_wait_for_event_cb (cmd-wait-for.c:238-239): FORMAT_NOJOBS.
                if !vtmux_core::format::is_true(&vtmux_core::format::expand_str_nojobs(&f, &ctx)) {
                    continue;
                }
            }
            done.push(i);
        }
        for i in done.into_iter().rev() {
            let w = self.event_waits.remove(i);
            self.cmdq_continue(w.step);
        }
    }
}
