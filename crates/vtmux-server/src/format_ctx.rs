//! The [`FormatContext`] over the model (format.c `format_defaults` and the
//! `format_table` callbacks).

use std::rc::Rc;

use vtmux_core::format::{
    ClientQuery, FormatContext, LoopItem, LoopKeys, LoopOption, OptionScope, OptionsLoop,
    SearchFlags,
};
use vtmux_core::ids::{SessionId, WindowId};
use vtmux_core::layout::PaneStatus;
use vtmux_core::options::{OptionValue, OptionsStore, Target, TreeId};

use crate::alerts;
use crate::environ::Environ;
use crate::format_jobs::{JobHandle, TAG_PANE, TAG_WINDOW};
use crate::model::Model;
use crate::names::parse_window_name;
use crate::server::ClientId;
use crate::sys;
use crate::target::Found;
use vtmux_vt::MouseTracking;

/// Server-wide values formats can read.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub socket_path: String,
    pub pid: u32,
    pub host: String,
    /// `#{config_files}`: the startup config paths, joined with commas
    /// (format.c `format_cb_config_files`).
    pub config_files: String,
    /// `#{current_file}`: the file the running command was parsed from
    /// (cfg.c `load_cfg`: `cmdq_add_format(state, "current_file", ...)`,
    /// merged into every format tree of the item by `cmdq_merge_formats`).
    /// Set by the queue while such a command fires.
    pub current_file: Option<String>,
    /// `start_time` (tmux.c): when the server started, epoch seconds.
    pub start_secs: i64,
}

impl ServerInfo {
    #[must_use]
    pub fn new(socket_path: String) -> Self {
        let host = rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned();
        Self {
            socket_path,
            pid: std::process::id(),
            host,
            config_files: String::new(),
            current_file: None,
            start_secs: crate::model::now_epoch(),
        }
    }
}

/// An attached client as formats see it (`#{L:...}`, `ft->c`).
#[derive(Clone, Debug)]
pub struct ClientView {
    /// Connection order (tmux's `clients` list is in creation order).
    pub order: u32,
    /// `client-PID` or the tty (server-client.c:3016-3019).
    pub name: String,
    pub session: Option<SessionId>,
    /// `c->activity_time` as a server-wide stamp: creation, then input.
    pub activity: u64,
    /// `c->activity_time` and `c->creation_time`, epoch seconds.
    pub activity_secs: i64,
    pub created_secs: i64,
    /// `c->tty.sx`/`sy`; the height only once the terminal started
    /// (`TTY_STARTED`: a terminal client, not control mode).
    pub width: u32,
    pub height: Option<u32>,
    /// `c->tty.xpixel`/`ypixel`: a cell's size in pixels, 0 if unknown.
    pub cell_pixels: (u32, u32),
    /// `c->term_name` (`unknown` when the client sent none).
    pub term_name: String,
    /// `c->ttyname`; `None` without a tty.
    pub tty: Option<String>,
    pub control: bool,
    /// `CLIENT_FOCUSED`: the terminal reported focus (tty-keys.c).
    pub focused: bool,
    pub utf8: bool,
    pub pid: u32,
    /// The key table the client is in, and its default table.
    pub key_table: String,
    pub default_key_table: String,
    pub last_session: Option<SessionId>,
    /// `#{client_termfeatures}` (`tty_get_features(c->term_features)`).
    pub termfeatures: String,
    /// `#{client_termtype}`: the terminal's XTVERSION text.
    pub termtype: String,
    /// `#{client_colours}`; `None` before the terminal is open
    /// (format.c `format_cb_client_colours` needs `TTY_STARTED`).
    pub colours: Option<u32>,
    /// `#{client_theme}`: the theme the terminal reported.
    pub theme: Option<vtmux_core::colour::Theme>,
    /// Frame bytes written (`c->written`).
    pub written: usize,
    /// The flags commands change (`c->flags`).
    pub flags: crate::client_flags::ClientFlags,
    /// `c->tty.term` while the client can be asked about it (format.c:6307:
    /// not before the terminal is open, nor once it is dead or suspended).
    pub term: Option<Rc<vtmux_core::term_features::Term>>,
    /// `c->environ`: what the client sent.
    pub environ: Rc<[(String, String)]>,
    /// The part of its current window the client shows (`tty->oox` ...).
    pub view: crate::view::View,
}

impl ClientView {
    /// `server_client_get_flags` (server-client.c:3163-3199) for the flags
    /// vtmux clients have: every listed client is attached.
    fn flags(&self) -> String {
        let mut flags = vec!["attached".to_owned()];
        if self.focused {
            flags.push("focused".to_owned());
        }
        if self.control {
            flags.push("control-mode".to_owned());
        }
        flags.extend(self.flags.listed_before_read_only());
        if self.flags.read_only {
            flags.push("read-only".to_owned());
        }
        if self.utf8 {
            flags.push("UTF-8".to_owned());
        }
        flags.join(",")
    }
}

/// What a format tree describes (`ft->type`, `FORMAT_TYPE_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Unknown,
    Session,
    Window,
    Pane,
}

/// One expansion's view: the model plus the target (`format_defaults(ft,
/// c, s, wl, wp)`, which fills a missing window or pane from the session).
pub struct Ctx<'a> {
    pub model: &'a Model,
    pub info: &'a ServerInfo,
    pub global_env: &'a Environ,
    pub options: &'a OptionsStore,
    pub target: Found,
    /// What the tree describes (`ft->type`), read by `#{session_format}`,
    /// `#{window_format}` and `#{pane_format}`.
    pub kind: Kind,
    /// Extra variables (`line` for `list-*`).
    pub extra: Vec<(std::borrow::Cow<'static, str>, String)>,
    /// The attached clients, in connection order (`#{L:...}`).
    pub clients: Rc<[ClientView]>,
    /// The client the formats are for (`ft->c`): its session marks the active item
    /// of `#{S:all,active}`.
    pub client: Option<ClientView>,
    /// The paste buffer the buffer variables describe (`ft->pb`); the top
    /// automatic buffer when unset (`format_defaults`, format.c:7032).
    pub buffer: Option<String>,
    /// The `#()` job cache this expansion reaches (`ft->client`, `ft->tag`,
    /// `ft->flags`); without one `#()` is empty.
    pub jobs: Option<JobHandle>,
    /// The client the tree was created for (`ft->client`, not `ft->c`) is a
    /// control client without `new-layouts`, so layouts dump as v1
    /// (format.c `format_cb_window_layout`, `LAYOUT_CUSTOM_OLD_FORMAT`).
    pub old_layouts: bool,
}

impl<'a> Ctx<'a> {
    #[must_use]
    pub fn new(
        model: &'a Model,
        info: &'a ServerInfo,
        global_env: &'a Environ,
        target: Found,
    ) -> Self {
        // format_defaults (format.c:7006-7013): the type is the deepest part
        // given, before the rest is filled in.
        let kind = if target.pane.is_some() {
            Kind::Pane
        } else if target.index.is_some() || target.window.is_some() {
            Kind::Window
        } else if target.session.is_some() {
            Kind::Session
        } else {
            Kind::Unknown
        };
        // format_defaults (format.c:7015-7020): fill window and pane from the
        // session.
        let mut target = target;
        if target.window.is_none()
            && let Some(s) = target.session
        {
            let f = Found::from_session(model, s);
            target.index = f.index;
            target.window = f.window;
        }
        if target.pane.is_none()
            && let Some(w) = target.window.and_then(|w| model.windows.get(&w))
        {
            target.pane = w.panes.active;
        }
        Self {
            model,
            info,
            global_env,
            options: &model.options,
            target,
            kind,
            extra: Vec::new(),
            clients: Rc::from([]),
            client: None,
            buffer: None,
            jobs: None,
            old_layouts: false,
        }
    }

    /// A loop child: the same server view and client over another target.
    fn child(&self, target: Found, client: Option<ClientView>) -> Ctx<'a> {
        let mut c = Ctx::new(self.model, self.info, self.global_env, target);
        c.options = self.options;
        c.clients = Rc::clone(&self.clients);
        c.client = client;
        // format_create(c, item, FORMAT_NONE, ft->flags): the client, the flags.
        c.old_layouts = self.old_layouts;
        c.jobs = self.jobs.clone().map(|j| j.with_tag(0));
        c
    }

    /// A loop child with its own tag (`FORMAT_WINDOW|id`, `FORMAT_PANE|id`).
    fn tagged_child(&self, target: Found, client: Option<ClientView>, tag: u32) -> Ctx<'a> {
        let mut c = self.child(target, client);
        c.jobs = c.jobs.map(|j| j.with_tag(tag));
        c
    }

    /// A window's own `@options`, for `next_@x`/`prev_@x`.
    fn window_user_options(&self, w: WindowId) -> Vec<(String, String)> {
        self.options
            .tree(TreeId::Window(w))
            .iter()
            .filter(|(name, _)| name.starts_with('@'))
            .map(|(name, e)| (name.to_owned(), e.value.to_text(e.def, None, true)))
            .collect()
    }

    /// The marked pane's winlink is session `s`'s window at `index`.
    fn marked_winlink(&self, s: SessionId, index: u32) -> bool {
        self.model
            .marked()
            .is_some_and(|m| m.session == Some(s) && m.index == Some(index))
    }

    fn session(&self) -> Option<&crate::model::Session> {
        self.model.sessions.get(&self.target.session?)
    }

    fn window(&self) -> Option<&crate::model::Window> {
        self.model.windows.get(&self.target.window?)
    }

    fn pane(&self) -> Option<&crate::model::Pane> {
        self.model.panes.get(&self.target.pane?)
    }

    /// The client variables (format.c `format_cb_client_*`, 1540-1830).
    /// `client_termfeatures` is the terminal-features work's; vtmux has
    /// no discard count or theme yet.
    fn client_value(&self, name: &str) -> Option<String> {
        let c = self.client.as_ref()?;
        let flag = |b: bool| u8::from(b).to_string();
        Some(match name {
            "client_name" => c.name.clone(),
            "client_termfeatures" => c.termfeatures.clone(),
            "client_colours" => c.colours?.to_string(),
            "client_theme" => c.theme?.name().to_owned(),
            "client_session" => self.model.sessions.get(&c.session?)?.name.clone(),
            "client_last_session" => self.model.sessions.get(&c.last_session?)?.name.clone(),
            // format_cb_window_bigger, format_cb_window_offset_x/_y
            // (format.c:3142-3363): the client's view of its window.
            "window_bigger" => flag(c.view.bigger),
            "window_offset_x" => c.view.bigger.then(|| c.view.ox.to_string())?,
            "window_offset_y" => c.view.bigger.then(|| c.view.oy.to_string())?,
            "client_width" => c.width.to_string(),
            "client_height" => c.height?.to_string(),
            // format_cb_client_cell_width/_height: once the terminal started.
            "client_cell_width" => {
                c.height?;
                c.cell_pixels.0.to_string()
            }
            "client_cell_height" => {
                c.height?;
                c.cell_pixels.1.to_string()
            }
            "client_termname" => c.term_name.clone(),
            "client_termtype" => c.termtype.clone(),
            "client_tty" => c.tty.clone()?,
            "client_flags" => c.flags(),
            "client_control_mode" => flag(c.control),
            "client_readonly" => flag(c.flags.read_only),
            "client_utf8" => flag(c.utf8),
            "client_pid" => c.pid.to_string(),
            "client_key_table" => c.key_table.clone(),
            "client_prefix" => flag(c.key_table != c.default_key_table),
            "client_discarded" => "0".to_owned(),
            "client_written" => c.written.to_string(),
            "client_activity" => c.activity_secs.to_string(),
            "client_created" => c.created_secs.to_string(),
            // The server accepts only its own user (no server-access), so
            // the peer's uid is the server's.
            "client_uid" => rustix::process::getuid().as_raw().to_string(),
            "client_user" => sys::user::current_user()?.name,
            _ => return None,
        })
    }

    fn attached_to(&self, s: SessionId) -> impl Iterator<Item = &ClientView> {
        self.clients.iter().filter(move |c| c.session == Some(s))
    }

    /// The clients attached to a member of `s`'s group, `None` when it is
    /// in no group.
    fn group_clients(&self, s: SessionId) -> Option<impl Iterator<Item = &ClientView>> {
        let members = self.model.groups.members_of(s)?;
        Some(
            self.clients
                .iter()
                .filter(move |c| c.session.is_some_and(|x| members.contains(&x))),
        )
    }

    fn session_value(&self, name: &str) -> Option<String> {
        let s = self.session()?;
        let flag = |b: bool| u8::from(b).to_string();
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "session_windows" => s.windows.len().to_string(),
            // format_cb_last_window_index: the highest index.
            "last_window_index" => s.windows.keys().next_back()?.to_string(),
            // format_cb_active_window_index.
            "active_window_index" => s.current.to_string(),
            // format_cb_session_stack: the current window, then `s->lastw`.
            "session_stack" => std::iter::once(&s.current)
                .chain(&s.last)
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
            "session_created" => s.created.to_string(),
            "session_activity" => s.activity_secs.to_string(),
            "session_last_attached" => s.last_attached.map(|t| t.to_string()).unwrap_or_default(),
            // format_cb_session_marked.
            "session_marked" => flag(self.model.marked().is_some_and(|m| m.session == Some(s.id))),
            // format.c format_cb_session_grouped and friends: empty (not
            // set) for a session in no group.
            "session_grouped" => flag(self.model.groups.name_of(s.id).is_some()),
            "session_group" => self.model.groups.name_of(s.id)?.to_owned(),
            "session_group_size" => self.model.groups.members_of(s.id)?.len().to_string(),
            "session_group_list" => self
                .model
                .groups
                .members_of(s.id)?
                .iter()
                .filter_map(|m| self.model.sessions.get(m))
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
            // session_group_attached_count: the clients attached to any
            // member.
            "session_group_attached" => self.group_clients(s.id)?.count().to_string(),
            "session_group_many_attached" => flag(self.group_clients(s.id)?.count() > 1),
            // format_cb_session_group_attached_list: client order.
            "session_group_attached_list" => self
                .group_clients(s.id)?
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
            // s->attached: the clients attached to it (format.c
            // format_cb_session_attached, recalculate_sizes).
            "session_attached" => self.attached_to(s.id).count().to_string(),
            // format_cb_session_active: the client's session is this one.
            "session_active" => flag(self.client.as_ref()?.session == Some(s.id)),
            // format_cb_session_many_attached.
            "session_many_attached" => flag(self.attached_to(s.id).count() > 1),
            // format_cb_session_attached_list: their names, in client order.
            // format_cb_session_attached_list: none without a client.
            "session_attached_list" => {
                let names = self
                    .attached_to(s.id)
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                if names.is_empty() {
                    return None;
                }
                names
            }
            // format_cb_session_alert: each flag once, in the order the
            // windows (by index) first show it.
            "session_alert" => {
                let mut seen = 0;
                let mut f = String::new();
                for flags in s.link_alerts.values() {
                    for (bit, c) in [
                        (alerts::ACTIVITY, '#'),
                        (alerts::BELL, '!'),
                        (alerts::SILENCE, '~'),
                    ] {
                        if !seen & flags & bit != 0 {
                            f.push(c);
                            seen |= bit;
                        }
                    }
                }
                f
            }
            // format_cb_session_alerts: `idx` plus its flags, per window.
            "session_alerts" => s
                .link_alerts
                .iter()
                .filter(|(_, f)| **f != 0)
                .map(|(i, f)| {
                    let mut t = i.to_string();
                    for (bit, c) in [
                        (alerts::ACTIVITY, '#'),
                        (alerts::BELL, '!'),
                        (alerts::SILENCE, '~'),
                    ] {
                        if f & bit != 0 {
                            t.push(c);
                        }
                    }
                    t
                })
                .collect::<Vec<_>>()
                .join(","),
            "session_activity_flag" => {
                flag(s.link_alerts.values().any(|f| f & alerts::ACTIVITY != 0))
            }
            "session_bell_flag" => flag(s.link_alerts.values().any(|f| f & alerts::BELL != 0)),
            "session_silence_flag" => {
                flag(s.link_alerts.values().any(|f| f & alerts::SILENCE != 0))
            }
            "session_path" => s.cwd.to_string_lossy().into_owned(),
            _ => return None,
        })
    }

    #[allow(clippy::too_many_lines)] // one arm per variable
    fn window_value(&self, name: &str) -> Option<String> {
        let w = self.window()?;
        // The winlink callbacks (format.c format_cb_window_active and
        // friends) need a session and index; without one they are empty.
        let wl = self.session().zip(self.target.index);
        let flag = |b: bool| u8::from(b).to_string();
        Some(match name {
            "window_id" => w.id.to_string(),
            "window_index" => wl?.1.to_string(),
            "window_name" => w.name.clone(),
            "window_activity" => w.activity_secs.to_string(),
            // format_cb_window_marked_flag: the marked winlink.
            "window_marked_flag" => flag(wl.is_some_and(|(s, i)| self.marked_winlink(s.id, i))),
            // window_count_panes(w, 1).
            "window_panes" => w.panes.panes.len().to_string(),
            "window_width" => w.sx().to_string(),
            "window_height" => w.sy().to_string(),
            // format.c:3213-3226, 3415-3428: empty unless window-size manual.
            "window_manual_width" | "window_manual_height" => {
                let manual = self
                    .model
                    .number_option(TreeId::Window(w.id), "window-size")
                    == 2;
                match (manual, name) {
                    (false, _) => String::new(),
                    (true, "window_manual_width") => w.manual_size.0.to_string(),
                    (true, _) => w.manual_size.1.to_string(),
                }
            }
            "window_zoomed_flag" => flag(w.zoomed()),
            // format_cb_window_modal_pane (format.c:3318-3325).
            "window_modal_pane" => w.panes.modal?.to_string(),
            "window_active" => flag(wl.is_some_and(|(s, i)| s.current == i)),
            "window_last_flag" => flag(wl.is_some_and(|(s, i)| s.last.first() == Some(&i))),
            "window_start_flag" => {
                flag(wl.is_some_and(|(s, i)| s.windows.keys().next() == Some(&i)))
            }
            "window_end_flag" => {
                flag(wl.is_some_and(|(s, i)| s.windows.keys().next_back() == Some(&i)))
            }
            // window_printable_flags (window.c:1359-1386); `window_flags`
            // doubles the `#` of the activity flag.
            "window_flags" | "window_raw_flags" => {
                let (s, i) = wl?;
                self.model.printable_flags(s.id, i, name == "window_flags")
            }
            // format_cb_window_linked: the window has more than one winlink
            // (a window linked twice in one session counts).
            "window_linked" => {
                wl?;
                flag(self.model.window_links(w.id).len() > 1)
            }
            // format_cb_window_linked_sessions: groups whose first member
            // links it, then ungrouped sessions linking it.
            "window_linked_sessions" => {
                wl?;
                self.model.linked_sessions(w.id).to_string()
            }
            // format_cb_window_linked_sessions_list: one name per winlink,
            // in the order they were attached to the window.
            "window_linked_sessions_list" => {
                wl?;
                self.model
                    .window_links(w.id)
                    .iter()
                    .map(|(s, _)| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            }
            "window_activity_flag" => flag(wl.is_some_and(|(s, i)| {
                s.link_alerts.get(&i).copied().unwrap_or(0) & alerts::ACTIVITY != 0
            })),
            "window_bell_flag" => flag(wl.is_some_and(|(s, i)| {
                s.link_alerts.get(&i).copied().unwrap_or(0) & alerts::BELL != 0
            })),
            "window_silence_flag" => flag(wl.is_some_and(|(s, i)| {
                s.link_alerts.get(&i).copied().unwrap_or(0) & alerts::SILENCE != 0
            })),
            // format_cb_window_layout: the saved layout while zoomed.
            // format.c:859-896: a control client without new-layouts sees
            // the v1 form.
            "window_layout" if self.old_layouts => w.full_layout().dump_v1(),
            "window_visible_layout" if self.old_layouts => w.layout.dump_v1(),
            "window_layout" => match &w.zoom {
                Some(z) => z.saved.dump_v2_saved(&self.layout_panes(w), z.pane)?,
                None => w.layout.dump_v2(&self.layout_panes(w))?,
            },
            "window_visible_layout" => w.layout.dump_v2(&self.layout_panes(w))?,
            _ => return self.winlink_value(w, wl, name),
        })
    }

    /// The winlink callbacks that walk the session's stack, the window's
    /// winlinks or the clients; empty without a winlink.
    fn winlink_value(
        &self,
        w: &crate::model::Window,
        wl: Option<(&crate::model::Session, u32)>,
        name: &str,
    ) -> Option<String> {
        let (s, i) = wl?;
        Some(match name {
            // format_cb_window_stack_index: the 1-based place in
            // `s->lastw`, 0 when not there.
            "window_stack_index" => s
                .last
                .iter()
                .position(|&x| x == i)
                .map_or(0, |p| p + 1)
                .to_string(),
            // format_cb_window_active_sessions(_list): the winlinks of the
            // window that are their session's current one.
            "window_active_sessions" => self.active_sessions(w.id).count().to_string(),
            "window_active_sessions_list" => {
                let names: Vec<_> = self
                    .active_sessions(w.id)
                    .map(|s| s.name.as_str())
                    .collect();
                if names.is_empty() {
                    return None;
                }
                names.join(",")
            }
            // format_cb_window_active_clients(_list): clients whose
            // session's current window is this window.
            "window_active_clients" => self.active_clients(w.id).count().to_string(),
            "window_active_clients_list" => {
                let names: Vec<_> = self.active_clients(w.id).map(|c| c.name.as_str()).collect();
                if names.is_empty() {
                    return None;
                }
                names.join(",")
            }
            _ => return None,
        })
    }

    /// The sessions whose current winlink shows `w`, in the window's
    /// winlink order.
    fn active_sessions(&self, w: WindowId) -> impl Iterator<Item = &crate::model::Session> {
        self.model
            .window_links(w)
            .into_iter()
            .filter(|(s, i)| s.current == *i)
            .map(|(s, _)| s)
    }

    /// The clients whose session's current window is `w`, in client order.
    fn active_clients(&self, w: WindowId) -> impl Iterator<Item = &ClientView> {
        self.clients.iter().filter(move |c| {
            c.session
                .and_then(|s| self.model.sessions.get(&s))
                .and_then(|s| s.windows.get(&s.current))
                == Some(&w)
        })
    }

    /// The window's pane state with its `pane-base-index`, for `"i"`.
    fn layout_panes(&self, w: &crate::model::Window) -> vtmux_core::layout::WindowPanes {
        let mut panes = w.panes.clone();
        panes.base_index = self.model.pane_base_index(w.id);
        panes
    }

    #[allow(clippy::too_many_lines)] // one arm per variable
    fn pane_value(&self, name: &str) -> Option<String> {
        let p = self.pane()?;
        let w = self.model.windows.get(&p.window)?;
        let io = p.io.as_ref();
        let flag = |b: bool| u8::from(b).to_string();
        let r = p.rect;
        let (wsx, wsy) = (i64::from(w.sx()), i64::from(w.sy()));
        let (xoff, yoff) = (i64::from(r.xoff), i64::from(r.yoff));
        let (sx, sy) = (i64::from(r.sx), i64::from(r.sy));
        let status = self.model.pane_status(w.id);
        Some(match name {
            "pane_id" => p.id.to_string(),
            "pane_index" => self.model.pane_index(p.id)?.to_string(),
            // format_cb_pane_pid: nothing once the PTY is closed, or for an
            // empty pane.
            "pane_pid" => {
                io?;
                p.pid.to_string()
            }
            "pane_width" => r.sx.to_string(),
            // format_cb_pane_unzoomed_width / _height (format.c:2704-2776):
            // the pane's cell in the full layout, the row of a pane status
            // line not counted, nor the columns of an `on` scrollbar (the
            // option alone while zoomed, which may hide the bar).
            "pane_unzoomed_width" => {
                let root = &w.full_layout().root;
                let sx = root.get(&self.model.pane_cell(w, p.id)?).g().sx;
                let reserve = if w.zoomed() {
                    !p.term.alternate_on()
                        && self.model.scrollbars(w.id) == crate::scrollbar::Scrollbars::On
                } else {
                    self.model.scrollbar_reserve(p.id)
                };
                if reserve {
                    let style = self.model.scrollbar_style(p.id);
                    let total = style.width + style.pad;
                    let min = vtmux_core::layout::PANE_MINIMUM;
                    if i64::from(sx) - i64::from(total) < i64::from(min) {
                        min.to_string()
                    } else {
                        (sx - total).to_string()
                    }
                } else {
                    sx.to_string()
                }
            }
            "pane_unzoomed_height" => {
                let layout = w.full_layout();
                let path = self.model.pane_cell(w, p.id)?;
                let g = layout.root.get(&path).g();
                let border = layout.cell_needs_border(&path) && g.sy > 1;
                (g.sy - u32::from(border)).to_string()
            }
            "pane_height" => r.sy.to_string(),
            "pane_left" | "pane_x" => r.xoff.to_string(),
            "pane_top" | "pane_y" => r.yoff.to_string(),
            "pane_right" => (xoff + sx - 1).to_string(),
            "pane_bottom" => (yoff + sy - 1).to_string(),
            // format_cb_pane_at_* (format.c:1235-1275, 2220-2242).
            "pane_at_left" => flag(xoff == 0),
            "pane_at_right" => flag(xoff + sx == wsx),
            "pane_at_top" => flag(if status == PaneStatus::Top {
                yoff == 1
            } else {
                yoff == 0
            }),
            "pane_at_bottom" => flag(if status == PaneStatus::Bottom {
                yoff + sy == wsy - 1
            } else {
                yoff + sy == wsy
            }),
            "pane_title" => p.term.title(),
            // format_cb_pane_fg / _bg: tty_default_colours.
            "pane_fg" => crate::term_queries::default_colours(self.model, p.id, self)
                .0
                .to_string(),
            "pane_bg" => crate::term_queries::default_colours(self.model, p.id, self)
                .1
                .to_string(),
            "pane_active" => flag(w.active() == p.id),
            "pane_last" => flag(w.panes.last.first() == Some(&p.id)),
            // window_pane_printable_flags (window.c:1389-1409).
            "pane_flags" => {
                let mut f = String::new();
                if w.active() == p.id {
                    f.push('*');
                }
                if w.panes.last.first() == Some(&p.id) {
                    f.push('-');
                }
                if w.zoom.as_ref().is_some_and(|z| z.pane == p.id) {
                    f.push('Z');
                }
                if w.is_floating_now(p.id) {
                    f.push('F');
                }
                if p.over_zoom {
                    f.push('A');
                }
                if w.panes.modal == Some(p.id) {
                    f.push('O');
                }
                f
            }
            // format_cb_pane_floating_flag: the pane's cell in the visible
            // layout (a pane hidden by a zoom has none).
            "pane_floating_flag" => flag(w.is_floating_now(p.id)),
            "pane_modal_flag" => flag(w.panes.modal == Some(p.id)),
            // format_cb_pane_z.
            "pane_z" => self.model.pane_zindex(p.id)?.to_string(),
            "pane_zoomed_flag" => flag(w.zoom.as_ref().is_some_and(|z| z.pane == p.id)),
            "pane_input_off" => flag(p.input_off),
            "pane_synchronized" => flag(self.model.pane_synchronized(p.id)),
            // format_cb_pane_marked, format_cb_pane_marked_set.
            "pane_marked" => flag(self.model.marked().is_some_and(|m| m.pane == Some(p.id))),
            "pane_marked_set" => flag(self.model.marked().is_some()),
            // format_cb_pane_pipe, format_cb_pane_pipe_pid (format.c:2587-2608).
            "pane_pipe" => flag(p.pipe.is_some()),
            "pane_pipe_pid" => p.pipe.as_ref()?.pid.to_string(),
            "pane_unseen_changes" => flag(p.unseen_changes),
            // format_cb_pane_dead* (format.c:2255-2311): the PTY closed
            // and the status in; the status or signal as it ended; when
            // remain-on-exit kept it.
            // A pane whose pipe is still writing its output is not
            // destroyed yet (tmux keeps its fd until server_destroy_pane).
            "pane_dead" => flag(p.status.is_some() && io.is_none() && p.pipe.is_none()),
            "pane_dead_status" => match p.status? {
                crate::jobs::JobStatus::Exited(code) => code.to_string(),
                crate::jobs::JobStatus::Signaled(_) | crate::jobs::JobStatus::Unknown => {
                    return None;
                }
            },
            "pane_dead_signal" => match p.status? {
                crate::jobs::JobStatus::Signaled(sig) => crate::server::sig_name(sig),
                crate::jobs::JobStatus::Exited(_) | crate::jobs::JobStatus::Unknown => {
                    return None;
                }
            },
            "pane_dead_time" => p.dead_time?.to_string(),
            // format_cb_pane_output_generation (format.c:2329-2340).
            "pane_output_generation" => p.output_generation.to_string(),
            // format_cb_pane_last_prompt_time and the pane_command_*
            // callbacks (format.c:2342-2426): nothing until OSC 133 says.
            "pane_last_prompt_time" => p.shell_commands.last_prompt?.to_string(),
            "pane_command_start_time" => p.shell_commands.started?.to_string(),
            "pane_command_end_time" => p.shell_commands.ended?.to_string(),
            "pane_command_running" => flag(p.shell_commands.running),
            "pane_command_duration" => p
                .shell_commands
                .duration(crate::model::now_epoch())?
                .to_string(),
            "pane_command_status" => p.shell_commands.status?.to_string(),
            "pane_start_command" => crate::names::stringify_argv(&p.argv),
            // format_cb_start_command_list: each word single-quoted for sh.
            "pane_start_command_list" => p
                .argv
                .iter()
                .map(|w| vtmux_core::format::quote_shell_single(w))
                .collect::<Vec<_>>()
                .join(" "),
            // format_cb_start_path (format.c:943-955).
            // format_cb_pane_path: what OSC 7 set, else empty.
            "pane_path" => p.term.pwd(),
            "pane_start_path" => p.cwd.to_string_lossy().into_owned(),
            "pane_tty" => p.tty.clone(),
            "pane_current_command" => {
                // format_cb_current_command (format.c:958-978).
                let cmd = io
                    .and_then(|io| {
                        sys::pty::foreground_name(&io.owned().try_clone_to_owned().ok()?)
                    })
                    .filter(|c| !c.is_empty())
                    .or_else(|| {
                        let argv = crate::names::stringify_argv(&p.argv);
                        (!argv.is_empty()).then_some(argv)
                    })
                    .unwrap_or_else(|| p.shell.to_string_lossy().into_owned());
                parse_window_name(&cmd)
            }
            "pane_current_path" => io
                .and_then(|io| sys::pty::foreground_cwd(&io.owned().try_clone_to_owned().ok()?))
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "cursor_x" => p.term.cursor().0.to_string(),
            "cursor_y" => p.term.cursor().1.to_string(),
            // format_cb_cursor_character: the cell under the cursor, nothing
            // on the second half of a wide character; a blank or a cell past
            // the line's end is the default cell's space.
            "cursor_character" => {
                let (cx, cy) = p.term.cursor();
                let row = p.term.row(p.term.history_size() + usize::from(cy));
                match row.cells.get(usize::from(cx)) {
                    Some(c) if c.width == 0 => return None,
                    Some(c) if !c.text.is_empty() => c.text.to_string(),
                    _ => " ".to_owned(),
                }
            }
            "alternate_on" => flag(p.term.alternate_on()),
            // format_cb_alternate_saved_x, _y (format.c:1463-1478): what
            // ?1049h saved, UINT_MAX before any (screen.c screen_reinit).
            "alternate_saved_x" => p
                .term
                .alternate_saved()
                .map_or(u32::MAX, |(x, _)| u32::from(x))
                .to_string(),
            "alternate_saved_y" => p
                .term
                .alternate_saved()
                .map_or(u32::MAX, |(_, y)| u32::from(y))
                .to_string(),
            // format_cb_synchronized_output_flag (format.c:2143-2150).
            "synchronized_output_flag" => flag(p.term.sync_held().is_some()),
            // format_cb_mouse_*_flag (format.c:1995-2080), from tmux's own
            // mode bits.
            "mouse_any_flag" => flag(p.term.mouse_modes().any()),
            "mouse_standard_flag" => flag(p.term.mouse_modes().tracking == MouseTracking::Standard),
            "mouse_button_flag" => flag(p.term.mouse_modes().tracking == MouseTracking::Button),
            "mouse_all_flag" => flag(p.term.mouse_modes().tracking == MouseTracking::All),
            "mouse_sgr_flag" => flag(p.term.mouse_modes().sgr),
            "mouse_utf8_flag" => flag(p.term.mouse_modes().utf8),
            // format.c:1482-1491, 1837-1992, 2131-2140, 3441-3450. Those
            // that read `wp->screen` see a mode's own screen while the pane
            // is in one (`screen_init`: no bracketed paste, no blinking, the
            // default shape).
            "bracket_paste_flag" => flag(Self::pane_screen(p).bracketed_paste),
            "cursor_blinking" => flag(Self::pane_screen(p).cursor_blinking),
            "cursor_very_visible" => flag(Self::pane_screen(p).cursor_very_visible),
            "cursor_shape" => match Self::pane_screen(p).cursor_shape {
                vtmux_vt::CursorShape::Default => "default",
                vtmux_vt::CursorShape::Block => "block",
                vtmux_vt::CursorShape::Underline => "underline",
                vtmux_vt::CursorShape::Bar => "bar",
            }
            .to_owned(),
            "cursor_flag" => flag(p.term.screen_modes().cursor_visible),
            "insert_flag" => flag(p.term.screen_modes().insert),
            "keypad_cursor_flag" => flag(p.term.screen_modes().app_cursor),
            "keypad_flag" => flag(p.term.screen_modes().app_keypad),
            "origin_flag" => flag(p.term.screen_modes().origin),
            "wrap_flag" => flag(p.term.screen_modes().wrap),
            "pane_private_modes" => private_modes(p.term.screen_modes(), p.term.mouse_modes()),
            // format.c:2833-2848, 1050-1077 (empty without tab stops).
            "scroll_region_upper" => p.term.scroll_region().0.to_string(),
            "scroll_region_lower" => p.term.scroll_region().1.to_string(),
            "pane_tabs" => {
                let tabs = p.term.tab_stops();
                if tabs.is_empty() {
                    return None;
                }
                tabs.iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            }
            // format_cb_cursor_colour (format.c:1295-1306): what OSC 12 set,
            // else `cursor-colour` (`screen_set_default_cursor`). Copy and
            // view mode's screen takes the global window option
            // (window-copy.c:590-591), other modes' none (`screen_init`).
            "cursor_colour" => match p.modes.first().map(crate::mode::PaneMode::name) {
                Some("copy-mode" | "view-mode") => {
                    cursor_colour_option(&self.option_text(TreeId::GlobalWindow, "cursor-colour"))
                }
                Some(_) => "none".to_owned(),
                None => match p.term.cursor_colour() {
                    Some((red, green, blue)) => {
                        vtmux_core::colour::Colour::Rgb(red, green, blue).to_string()
                    }
                    None => {
                        cursor_colour_option(&self.option_text(TreeId::Pane(p.id), "cursor-colour"))
                    }
                },
            },
            "pane_key_mode" => match self.model.pane_key_mode(p.id)? {
                vtmux_vt::KeyMode::Standard => "VT10x",
                vtmux_vt::KeyMode::Extended1 => "Ext 1",
                vtmux_vt::KeyMode::Extended2 => "Ext 2",
            }
            .to_owned(),
            "history_size" => p.term.history_size().to_string(),
            "history_bytes" => vtmux_vt::history_bytes(&p.term).to_string(),
            // format_cb_history_all_bytes (format.c:1023-1047), on
            // history_bytes' model (compat/DIVERGENCES.md D-001).
            "history_all_bytes" => {
                use vtmux_vt::capture::{CELL_ENTRY, EXTD_ENTRY, GRID_LINE};
                let counts = vtmux_vt::history_counts(&p.term);
                format!(
                    "{},{},{},{},{},{}",
                    counts.lines,
                    counts.lines * GRID_LINE,
                    counts.cells,
                    counts.cells * CELL_ENTRY,
                    counts.extended,
                    counts.extended * EXTD_ENTRY
                )
            }
            "history_limit" => p.history_limit.to_string(),
            "pane_in_mode" => p.modes.len().to_string(),
            "pane_mode" => crate::mode::PaneMode::name(p.modes.first()?).to_owned(),
            // format_cb_pane_search_string: the copy mode's own search
            // while it is shown, else the pane's last.
            "pane_search_string" => p
                .modes
                .first()
                .and_then(crate::mode::Mode::as_copy)
                .map_or(p.search.as_ref(), |m| m.pane_search.as_ref())
                .map(|(s, _)| s.clone())
                .unwrap_or_default(),
            _ => return self.copy_value(p, name),
        })
    }

    /// The variables copy mode adds while it is the pane's shown mode
    /// (`window_copy_formats`, window-copy.c:1106-1180).
    fn copy_value(&self, p: &crate::model::Pane, name: &str) -> Option<String> {
        let m = p.modes.first()?.as_copy()?;
        let line_numbers = self
            .model
            .number_option(TreeId::Window(p.window), "copy-mode-line-numbers");
        let (cx, cy) = m.cursor();
        let (position, limit) = m.position(line_numbers);
        Some(match name {
            "scroll_position" => m.scroll_position().to_string(),
            "copy_position" => position.to_string(),
            "copy_position_limit" => limit.to_string(),
            "copy_line_numbers" => u8::from(m.line_numbers_active(line_numbers)).to_string(),
            "copy_cursor_x" => cx.to_string(),
            "copy_cursor_y" => cy.to_string(),
            "top_line_time" => m.top_line_time().to_string(),
            "copy_cursor_line" => m.cursor_line(),
            "copy_cursor_word" => {
                let separators = self
                    .options
                    .get(TreeId::GlobalSession, "word-separators")
                    .map(|e| e.value.to_text(e.def, None, false))
                    .unwrap_or_default();
                m.cursor_word(&separators)
            }
            "selection_mode" => m.selection_mode().name().to_owned(),
            "rectangle_toggle" => u8::from(m.rectangle()).to_string(),
            "selection_active" => u8::from(m.selection().is_some_and(|s| s.active)).to_string(),
            "selection_present" => u8::from(m.selection().is_some_and(|s| s.present)).to_string(),
            "selection_start_x" => m.selection()?.start.0.to_string(),
            "selection_start_y" => m.selection()?.start.1.to_string(),
            "selection_end_x" => m.selection()?.end.0.to_string(),
            "selection_end_y" => m.selection()?.end.1.to_string(),
            "search_present" => u8::from(m.search_info().0).to_string(),
            "search_timed_out" => u8::from(m.search_info().1).to_string(),
            // Not set while the count is unknown (window-copy.c:1169-1172).
            "search_count" => m.search_info().2?.0.to_string(),
            "search_count_partial" => u8::from(m.search_info().2?.1).to_string(),
            "search_match" => m.search_match(),
            "refresh_active" => u8::from(m.refresh_active()).to_string(),
            // window_copy_cursor_hyperlink_cb: the view's cell.
            "copy_cursor_hyperlink" => m
                .hyperlink_at(cx, cy)
                .map(|uri| vtmux_vt::vis_link(&uri).into_owned())
                .unwrap_or_default(),
            _ => return None,
        })
    }
}

impl Ctx<'_> {
    /// Everything `format_find` knows before it falls back to the environment
    /// (format.c:4623-4663): options, extra entries, then the built-in variables.
    fn find_before_environ(&self, name: &str) -> Option<String> {
        // format.c format_find: options precede built-ins and extra entries.
        if let Some(value) = self.options.format_value(
            name,
            &Target {
                session: self.target.session,
                window: self.target.window,
                pane: self.target.pane,
            },
        ) {
            return Some(value);
        }
        if let Some((_, v)) = self.extra.iter().find(|(k, _)| *k == name) {
            return Some(v.clone());
        }
        self.builtin(name)
    }

    /// `format_each` (format.c:4341-4380), what `display-message -a` lists:
    /// each `format_table` entry with a value, in the table's order, then
    /// the tree's own entries (the command's formats, a hook's, copy
    /// mode's) in name order.
    #[must_use]
    pub fn each(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = FORMAT_TABLE
            .iter()
            .filter_map(|&name| {
                let mut value = self.builtin(name)?;
                // A time entry's callback gives the session's zero time,
                // which format_each prints (format.c:4358-4361) and
                // expansion leaves empty.
                if name == "session_last_attached" && value.is_empty() {
                    "0".clone_into(&mut value);
                }
                Some((name.to_owned(), value))
            })
            .collect();
        let mut tree: std::collections::BTreeMap<String, String> = self
            .extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        if let Some(p) = self.pane() {
            for &name in COPY_FORMATS {
                // format_add_cb entries are empty when their callback
                // gives nothing.
                let cb = matches!(
                    name,
                    "search_match"
                        | "copy_cursor_word"
                        | "copy_cursor_line"
                        | "copy_cursor_hyperlink"
                );
                match self.copy_value(p, name) {
                    Some(v) => {
                        tree.insert(name.to_owned(), v);
                    }
                    None if cb && p.modes.first().and_then(|m| m.as_copy()).is_some() => {
                        tree.insert(name.to_owned(), String::new());
                    }
                    None => {}
                }
            }
        }
        out.extend(tree);
        out
    }

    /// The built-in variables (`format_table` and copy mode's), without
    /// options or the tree's entries.
    fn builtin(&self, name: &str) -> Option<String> {
        let server = match name {
            "socket_path" => Some(self.info.socket_path.clone()),
            "pid" => Some(self.info.pid.to_string()),
            "start_time" => Some(self.info.start_secs.to_string()),
            "uid" => Some(rustix::process::getuid().as_raw().to_string()),
            "user" => sys::user::current_user().map(|p| p.name),
            "version" => Some(vtmux_core::version::TMUX_VERSION.to_owned()),
            "vtmux_version" => Some(vtmux_core::version::display()),
            "config_files" => Some(self.info.config_files.clone()),
            "current_file" => self.info.current_file.clone(),
            "host" => Some(self.info.host.clone()),
            "server_sessions" => Some(self.model.sessions.len().to_string()),
            "next_session_id" => Some(self.model.session_ids.peek().to_string()),
            // Built without sixel (format.c `format_cb_sixel_support`).
            "sixel_support" => Some("0".to_owned()),
            "buffer_mode_format" => Some(crate::mode::window_buffer::DEFAULT_FORMAT.to_owned()),
            "client_mode_format" => Some(crate::mode::window_client::DEFAULT_FORMAT.to_owned()),
            "tree_mode_format" => Some(crate::mode::window_tree::DEFAULT_FORMAT.to_owned()),
            "session_format" => Some(u8::from(self.kind == Kind::Session).to_string()),
            "window_format" => Some(u8::from(self.kind == Kind::Window).to_string()),
            "pane_format" => Some(u8::from(self.kind == Kind::Pane).to_string()),
            "host_short" => Some(
                self.info
                    .host
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            ),
            _ => None,
        };
        server
            .or_else(|| self.client_value(name))
            .or_else(|| self.session_value(name))
            .or_else(|| self.window_value(name))
            .or_else(|| self.pane_value(name))
            .or_else(|| self.buffer_value(name))
    }

    /// The modes of the screen a pane shows (`wp->screen`): its own, or a
    /// mode's (`screen_init`) while it is in one.
    fn pane_screen(p: &crate::model::Pane) -> vtmux_vt::ScreenModes {
        let m = p.term.screen_modes();
        if p.modes.is_empty() {
            return m;
        }
        vtmux_vt::ScreenModes {
            bracketed_paste: false,
            cursor_blinking: false,
            cursor_blinking_set: false,
            cursor_very_visible: false,
            cursor_shape: vtmux_vt::CursorShape::Default,
            ..m
        }
    }

    /// An option's value as `show-options` gives it, inherited.
    fn option_text(&self, tree: TreeId, name: &str) -> String {
        self.options
            .get(tree, name)
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default()
    }

    /// The buffer variables (format.c:1493-1535).
    fn buffer_value(&self, name: &str) -> Option<String> {
        let paste = &self.model.paste;
        let b = match &self.buffer {
            Some(n) => paste.get(n),
            None => paste.top(),
        }?;
        Some(match name {
            "buffer_name" => b.name.clone(),
            "buffer_sample" => b.sample(),
            "buffer_full" => b.full(),
            "buffer_size" => b.data.len().to_string(),
            "buffer_created" => b.created.to_string(),
            _ => return None,
        })
    }
}

impl FormatContext for Ctx<'_> {
    fn job(&self, raw: &str, expanded: &str) -> String {
        self.jobs
            .as_ref()
            .map_or_else(String::new, |j| j.get(raw, expanded))
    }

    fn get(&self, name: &str) -> Option<String> {
        self.find_before_environ(name)
            // format_find falls back to the session, then global,
            // environment (format.c:4665-4675), but not for a format_table
            // name, whose callback's answer stands even when it is none
            // (format.c:4641-4649).
            .or_else(|| {
                if in_format_table(name) {
                    return None;
                }
                let entry = self
                    .session()
                    .and_then(|s| s.env.find(name))
                    .or_else(|| self.global_env.find(name))?;
                entry.value.clone()
            })
    }

    /// `format_window_name` (format.c:5295).
    fn window_name_exists(&self, name: &str) -> Option<bool> {
        let s = self.session()?;
        Some(
            s.windows
                .values()
                .any(|w| self.model.windows.get(w).is_some_and(|w| w.name == name)),
        )
    }

    /// `#{I/c:...}`, `#{I/f:...}` and `#{I/e:...}` (format.c:6304-6327).
    fn client_query(&self, query: ClientQuery, name: &str) -> Option<String> {
        let c = self.client.as_ref()?;
        let term = c.term.as_ref()?;
        let flag = |b: bool| u8::from(b).to_string();
        Some(match query {
            ClientQuery::Termcap => flag(term.has_name(name)),
            ClientQuery::Feature => flag(term.feature_present(name)),
            ClientQuery::Environ => c
                .environ
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default(),
        })
    }

    /// `format_cycle`'s `FORMAT_STATUS` test and clock: only a status line's
    /// expansion (its job handle says so) animates.
    fn cycle_clock(&self, animate: bool) -> Option<u64> {
        self.jobs.as_ref()?.cycle_clock(animate)
    }

    /// `format_session_name` (format.c:5214).
    fn session_name_exists(&self, name: &str) -> bool {
        self.model.session_named(name).is_some()
    }

    /// Not for `t` lookups: format.c:4666 skips the environment then.
    fn get_without_environ(&self, name: &str) -> Option<String> {
        self.find_before_environ(name)
    }

    /// `window_pane_search` (window.c:2123-2168): the visible rows of the
    /// pane, trailing whitespace stripped, against `*pattern*` (fnmatch)
    /// or the ERE; a regex that does not compile matches nothing.
    fn search_pane(&self, pattern: &str, flags: SearchFlags) -> Option<u32> {
        let term = &self.pane()?.term;
        let regex = if flags.regex {
            Some(vtmux_core::regex::Regex::new(pattern, flags.ignore_case).ok()?)
        } else {
            None
        };
        let glob = format!("*{pattern}*");
        let hsize = term.history_size();
        (0..usize::from(term.rows()))
            .position(|y| {
                let text = term.row(hsize + y).text();
                let line = text.trim_end_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
                match &regex {
                    Some(r) => r.is_match(line).unwrap_or(false),
                    None => vtmux_core::fnmatch::matches(&glob, line, flags.ignore_case),
                }
            })
            .and_then(|y| u32::try_from(y + 1).ok())
    }

    fn time(&self, name: &str) -> Option<i64> {
        match name {
            // format_cb_start_time.
            "start_time" => Some(self.info.start_secs),
            "session_created" => self.session().map(|s| s.created),
            "session_activity" => self.session().map(|s| s.activity_secs),
            "session_last_attached" => self.session().and_then(|s| s.last_attached),
            "window_activity" => self.window().map(|w| w.activity_secs),
            "pane_dead_time" => self.pane().and_then(|p| p.dead_time),
            "pane_last_prompt_time" => self.pane().and_then(|p| p.shell_commands.last_prompt),
            "pane_command_start_time" => self.pane().and_then(|p| p.shell_commands.started),
            "pane_command_end_time" => self.pane().and_then(|p| p.shell_commands.ended),
            "client_activity" => self.client.as_ref().map(|c| c.activity_secs),
            "client_created" => self.client.as_ref().map(|c| c.created_secs),
            "buffer_created" => {
                let paste = &self.model.paste;
                match &self.buffer {
                    Some(n) => paste.get(n),
                    None => paste.top(),
                }
                .map(|b| b.created)
            }
            _ => None,
        }
    }

    /// `format_loop_sessions` (format.c:5231): `format_defaults(c, s, NULL, NULL)`.
    fn sessions(&self) -> Vec<LoopItem<'_>> {
        let current = self.client.as_ref().and_then(|c| c.session);
        self.model
            .sessions_by_name()
            .map(|s| LoopItem {
                ctx: Box::new(self.child(
                    Found {
                        session: Some(s.id),
                        ..Found::default()
                    },
                    self.client.clone(),
                )),
                keys: LoopKeys {
                    active: current == Some(s.id),
                    id: s.id.0,
                    name: s.name.clone(),
                    activity: s.activity,
                    ..LoopKeys::default()
                },
            })
            .collect()
    }

    /// `format_loop_windows` (format.c:5350): the target session's winlinks.
    fn windows(&self) -> Option<Vec<LoopItem<'_>>> {
        let s = self.session()?;
        Some(
            s.windows
                .iter()
                .filter_map(|(&index, &w)| {
                    let window = self.model.windows.get(&w)?;
                    Some(LoopItem {
                        ctx: Box::new(self.tagged_child(
                            Found {
                                session: Some(s.id),
                                index: Some(index),
                                window: Some(w),
                                ..Found::default()
                            },
                            self.client.clone(),
                            TAG_WINDOW | w.0,
                        )),
                        keys: LoopKeys {
                            active: s.current == index,
                            id: w.0,
                            index,
                            name: window.name.clone(),
                            activity: window.activity,
                            user_options: self.window_user_options(w),
                            ..LoopKeys::default()
                        },
                    })
                })
                .collect(),
        )
    }

    /// `format_loop_panes` (format.c:5432): the target window's panes, with the
    /// parent's session and winlink.
    fn panes(&self) -> Option<Vec<LoopItem<'_>>> {
        let w = self.window()?;
        Some(
            w.panes
                .panes
                .iter()
                .filter_map(|&p| {
                    let pane = self.model.panes.get(&p)?;
                    Some(LoopItem {
                        ctx: Box::new(self.tagged_child(
                            Found {
                                pane: Some(p),
                                ..self.target
                            },
                            self.client.clone(),
                            TAG_PANE | p.0,
                        )),
                        keys: LoopKeys {
                            active: w.active() == p,
                            id: p.0,
                            index: self.model.pane_index(p).unwrap_or(0),
                            name: pane.term.title(),
                            activity: pane.active_point,
                            z: self.model.pane_zindex(p).unwrap_or(0),
                            ..LoopKeys::default()
                        },
                    })
                })
                .collect(),
        )
    }

    /// `format_loop_clients` (format.c:5762): `format_defaults(c, ft->s, ft->wl,
    /// ft->wp)`; with no session in the target the client's own is used.
    fn clients(&self) -> Vec<LoopItem<'_>> {
        self.clients
            .iter()
            .map(|c| {
                let target = if self.target.session.is_none()
                    && let Some(s) = c.session
                {
                    Found {
                        session: Some(s),
                        ..Found::default()
                    }
                } else {
                    self.target
                };
                LoopItem {
                    ctx: {
                        // format_create(c, ...): the looped client's own jobs.
                        let mut child = self.child(target, Some(c.clone()));
                        child.jobs = child
                            .jobs
                            .map(|j| j.with_client(Some(ClientId(c.order as usize))));
                        Box::new(child)
                    },
                    keys: LoopKeys {
                        id: c.order,
                        name: c.name.clone(),
                        activity: c.activity,
                        ..LoopKeys::default()
                    },
                }
            })
            .collect()
    }

    /// `format_loop_options` (format.c:5603): one tree's own entries.
    fn options(&self, scope: OptionScope) -> Option<OptionsLoop<'_>> {
        let tree = match scope {
            OptionScope::Server => TreeId::Server,
            OptionScope::GlobalSession => TreeId::GlobalSession,
            OptionScope::GlobalWindow => TreeId::GlobalWindow,
            OptionScope::Session => TreeId::Session(self.target.session?),
            OptionScope::Window => TreeId::Window(self.target.window?),
            OptionScope::Pane => TreeId::Pane(self.target.pane?),
        };
        let entries = self
            .options
            .tree(tree)
            .iter()
            .map(|(name, e)| LoopOption {
                name: name.to_owned(),
                value: e.value.to_text(e.def, None, false),
                array: e.is_array().then(|| match &e.value {
                    OptionValue::Array(items) => items
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_text(e.def, false)))
                        .collect(),
                    OptionValue::Single(_) => Vec::new(),
                }),
                hook: e.def.is_some_and(|d| d.hook),
                user: e.def.is_none(),
            })
            .collect();
        Some(OptionsLoop {
            ctx: Box::new(self.child(self.target, self.client.clone())),
            entries,
        })
    }
}

/// `format_cb_pane_private_modes` (format.c:2155-2206): the DEC private
/// modes set, by number in tmux's table order; the cursor blinking only
/// when the program set it (`MODE_CURSOR_BLINKING_SET`), not when it comes
/// with a DECSCUSR style.
fn private_modes(m: vtmux_vt::ScreenModes, mouse: vtmux_vt::MouseModes) -> String {
    let table = [
        (m.app_cursor, 1),
        (m.origin, 6),
        (m.wrap, 7),
        (m.cursor_blinking && m.cursor_blinking_set, 12),
        (m.cursor_visible, 25),
        (mouse.tracking == MouseTracking::Standard, 1000),
        (mouse.tracking == MouseTracking::Button, 1002),
        (mouse.tracking == MouseTracking::All, 1003),
        (m.focus, 1004),
        (mouse.utf8, 1005),
        (mouse.sgr, 1006),
        (m.bracketed_paste, 2004),
        (m.sync, 2026),
        (m.theme_updates, 2031),
    ];
    table
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// `colour_tostring` of `cursor-colour` as `screen_set_default_cursor`
/// reads it (`style_apply` over `style_parse_colour`, style.c:476-497):
/// empty is none (-1), a colour it cannot parse leaves the default cell's.
fn cursor_colour_option(text: &str) -> String {
    if text.is_empty() {
        return "none".to_owned();
    }
    vtmux_core::colour::Colour::parse(text)
        .unwrap_or(vtmux_core::colour::Colour::Default)
        .to_string()
}

/// Whether `name` is in tmux's `format_table`: `format_find` asks the
/// table before the tree, so a tree entry of that name is never seen
/// (format.c:4641-4649).
pub(crate) fn in_format_table(name: &str) -> bool {
    FORMAT_TABLE.binary_search(&name).is_ok()
}

/// The names in tmux's `format_table` (format.c:3592-4170), in its
/// (sorted) order: what `format_each` walks first.
const FORMAT_TABLE: &[&str] = &[
    "active_window_index",
    "alternate_on",
    "alternate_saved_x",
    "alternate_saved_y",
    "bracket_paste_flag",
    "buffer_created",
    "buffer_full",
    "buffer_mode_format",
    "buffer_name",
    "buffer_sample",
    "buffer_size",
    "client_activity",
    "client_cell_height",
    "client_cell_width",
    "client_colours",
    "client_control_mode",
    "client_created",
    "client_discarded",
    "client_flags",
    "client_height",
    "client_key_table",
    "client_last_session",
    "client_mode_format",
    "client_name",
    "client_pid",
    "client_prefix",
    "client_readonly",
    "client_session",
    "client_termfeatures",
    "client_termname",
    "client_termtype",
    "client_theme",
    "client_tty",
    "client_uid",
    "client_user",
    "client_utf8",
    "client_width",
    "client_written",
    "config_files",
    "cursor_blinking",
    "cursor_character",
    "cursor_colour",
    "cursor_flag",
    "cursor_shape",
    "cursor_very_visible",
    "cursor_x",
    "cursor_y",
    "history_added",
    "history_all_bytes",
    "history_bytes",
    "history_collected",
    "history_generation",
    "history_limit",
    "history_size",
    "host",
    "host_short",
    "insert_flag",
    "keypad_cursor_flag",
    "keypad_flag",
    "last_window_index",
    "mouse_all_flag",
    "mouse_any_flag",
    "mouse_button_flag",
    "mouse_hyperlink",
    "mouse_line",
    "mouse_pane",
    "mouse_sgr_flag",
    "mouse_standard_flag",
    "mouse_status_line",
    "mouse_status_range",
    "mouse_utf8_flag",
    "mouse_word",
    "mouse_x",
    "mouse_y",
    "next_session_id",
    "origin_flag",
    "pane_active",
    "pane_at_bottom",
    "pane_at_left",
    "pane_at_right",
    "pane_at_top",
    "pane_bg",
    "pane_bottom",
    "pane_command_duration",
    "pane_command_end_time",
    "pane_command_running",
    "pane_command_start_time",
    "pane_command_status",
    "pane_current_command",
    "pane_current_path",
    "pane_dead",
    "pane_dead_signal",
    "pane_dead_status",
    "pane_dead_time",
    "pane_fg",
    "pane_flags",
    "pane_floating_flag",
    "pane_format",
    "pane_height",
    "pane_id",
    "pane_in_mode",
    "pane_index",
    "pane_input_off",
    "pane_key_mode",
    "pane_last",
    "pane_last_output_time",
    "pane_last_prompt_time",
    "pane_left",
    "pane_marked",
    "pane_marked_set",
    "pane_modal_flag",
    "pane_mode",
    "pane_output_generation",
    "pane_path",
    "pane_pb_progress",
    "pane_pb_state",
    "pane_pid",
    "pane_pipe",
    "pane_pipe_pid",
    "pane_private_modes",
    "pane_right",
    "pane_search_string",
    "pane_start_command",
    "pane_start_command_list",
    "pane_start_path",
    "pane_synchronized",
    "pane_tabs",
    "pane_title",
    "pane_top",
    "pane_tty",
    "pane_unseen_changes",
    "pane_unzoomed_height",
    "pane_unzoomed_width",
    "pane_width",
    "pane_x",
    "pane_y",
    "pane_z",
    "pane_zoomed_flag",
    "pid",
    "scroll_region_lower",
    "scroll_region_upper",
    "server_sessions",
    "session_active",
    "session_activity",
    "session_activity_flag",
    "session_alert",
    "session_alerts",
    "session_attached",
    "session_attached_list",
    "session_bell_flag",
    "session_created",
    "session_format",
    "session_group",
    "session_group_attached",
    "session_group_attached_list",
    "session_group_list",
    "session_group_many_attached",
    "session_group_size",
    "session_grouped",
    "session_id",
    "session_last_attached",
    "session_many_attached",
    "session_marked",
    "session_name",
    "session_path",
    "session_silence_flag",
    "session_stack",
    "session_windows",
    "sixel_support",
    "socket_path",
    "start_time",
    "synchronized_output_flag",
    "tree_mode_format",
    "uid",
    "user",
    "version",
    "window_active",
    "window_active_clients",
    "window_active_clients_list",
    "window_active_sessions",
    "window_active_sessions_list",
    "window_activity",
    "window_activity_flag",
    "window_bell_flag",
    "window_bigger",
    "window_cell_height",
    "window_cell_width",
    "window_end_flag",
    "window_flags",
    "window_format",
    "window_height",
    "window_id",
    "window_index",
    "window_last_flag",
    "window_layout",
    "window_linked",
    "window_linked_sessions",
    "window_linked_sessions_list",
    "window_manual_height",
    "window_manual_width",
    "window_marked_flag",
    "window_modal_pane",
    "window_name",
    "window_offset_x",
    "window_offset_y",
    "window_panes",
    "window_raw_flags",
    "window_silence_flag",
    "window_stack_index",
    "window_start_flag",
    "window_visible_layout",
    "window_width",
    "window_zoomed_flag",
    "wrap_flag",
];

/// The entries `window_copy_formats` adds to a tree (window-copy.c:1105-1178).
const COPY_FORMATS: &[&str] = &[
    "top_line_time",
    "scroll_position",
    "copy_position",
    "copy_position_limit",
    "copy_line_numbers",
    "refresh_active",
    "rectangle_toggle",
    "copy_cursor_x",
    "copy_cursor_y",
    "selection_start_x",
    "selection_start_y",
    "selection_end_x",
    "selection_end_y",
    "selection_active",
    "selection_present",
    "selection_mode",
    "search_present",
    "search_timed_out",
    "search_count",
    "search_count_partial",
    "search_match",
    "copy_cursor_word",
    "copy_cursor_line",
    "copy_cursor_hyperlink",
];

#[cfg(test)]
mod table_tests {
    /// `get` looks names up in [`super::FORMAT_TABLE`] by binary search, as
    /// tmux's `format_table_get` does (`bsearch`).
    #[test]
    fn format_table_is_sorted() {
        assert!(super::FORMAT_TABLE.windows(2).all(|w| w[0] < w[1]));
    }
}
