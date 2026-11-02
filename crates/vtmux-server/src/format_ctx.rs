//! The [`FormatContext`] over the model (format.c `format_defaults` and the
//! `format_table` callbacks).

use std::rc::Rc;

use vtmux_core::format::{FormatContext, LoopItem, LoopKeys, LoopOption, OptionScope, OptionsLoop};
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

/// Server-wide values formats can read.
#[derive(Debug)]
pub struct ServerInfo {
    pub socket_path: String,
    pub pid: u32,
    pub host: String,
    /// `#{current_file}`: the file the running command was parsed from
    /// (cfg.c `load_cfg`: `cmdq_add_format(state, "current_file", ...)`,
    /// merged into every format tree of the item by `cmdq_merge_formats`).
    /// Set by the queue while such a command fires.
    pub current_file: Option<String>,
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
            current_file: None,
        }
    }
}

/// An attached client as formats see it (`#{L:...}`, `ft->c`).
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// `#{client_colours}`; `None` before the terminal is open
    /// (format.c `format_cb_client_colours` needs `TTY_STARTED`).
    pub colours: Option<u32>,
    /// Frame bytes written (`c->written`).
    pub written: usize,
    /// The flags commands change (`c->flags`).
    pub flags: crate::client_flags::ClientFlags,
}

impl ClientView {
    /// A control client without `new-layouts` (`LAYOUT_CUSTOM_OLD_FORMAT`).
    #[must_use]
    pub fn old_layouts(&self) -> bool {
        self.control && !self.flags.new_layouts
    }

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

/// One expansion's view: the model plus the target (`format_defaults(ft,
/// c, s, wl, wp)`, which fills a missing window or pane from the session).
pub struct Ctx<'a> {
    pub model: &'a Model,
    pub info: &'a ServerInfo,
    pub global_env: &'a Environ,
    pub options: &'a OptionsStore,
    pub target: Found,
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
}

impl<'a> Ctx<'a> {
    #[must_use]
    pub fn new(
        model: &'a Model,
        info: &'a ServerInfo,
        global_env: &'a Environ,
        target: Found,
    ) -> Self {
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
            extra: Vec::new(),
            clients: Rc::from([]),
            client: None,
            buffer: None,
            jobs: None,
        }
    }

    /// A loop child: the same server view and client over another target.
    fn child(&self, target: Found, client: Option<ClientView>) -> Ctx<'a> {
        let mut c = Ctx::new(self.model, self.info, self.global_env, target);
        c.options = self.options;
        c.clients = Rc::clone(&self.clients);
        c.client = client;
        // format_create(c, item, FORMAT_NONE, ft->flags): the client, the flags.
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
    /// no pixel sizes (0, as tmux without them), discard count or theme yet.
    fn client_value(&self, name: &str) -> Option<String> {
        let c = self.client.as_ref()?;
        let flag = |b: bool| u8::from(b).to_string();
        Some(match name {
            "client_name" => c.name.clone(),
            "client_termfeatures" => c.termfeatures.clone(),
            "client_colours" => c.colours?.to_string(),
            "client_session" => self.model.sessions.get(&c.session?)?.name.clone(),
            "client_last_session" => self.model.sessions.get(&c.last_session?)?.name.clone(),
            "client_width" => c.width.to_string(),
            "client_height" => c.height?.to_string(),
            "client_cell_width" | "client_cell_height" => {
                c.height?;
                "0".to_owned()
            }
            "client_termname" => c.term_name.clone(),
            "client_termtype" => String::new(),
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
            "client_mode_format" => {
                "#[fg=themelightgrey]#{t/p:client_activity}: session #[default]#{session_name}"
                    .to_owned()
            }
            _ => return None,
        })
    }

    fn attached_to(&self, s: SessionId) -> impl Iterator<Item = &ClientView> {
        self.clients.iter().filter(move |c| c.session == Some(s))
    }

    fn session_value(&self, name: &str) -> Option<String> {
        let s = self.session()?;
        let flag = |b: bool| u8::from(b).to_string();
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "session_windows" => s.windows.len().to_string(),
            "session_created" => s.created.to_string(),
            "session_activity" => s.activity_secs.to_string(),
            "session_last_attached" => s.last_attached.map(|t| t.to_string()).unwrap_or_default(),
            // No marked pane yet (server_check_marked), no session groups.
            "session_marked" | "session_grouped" => "0".to_owned(),
            "session_group" => String::new(),
            // s->attached: the clients attached to it (format.c
            // format_cb_session_attached, recalculate_sizes).
            "session_attached" => self.attached_to(s.id).count().to_string(),
            // format_cb_session_many_attached.
            "session_many_attached" => flag(self.attached_to(s.id).count() > 1),
            // format_cb_session_attached_list: their names, in client order.
            "session_attached_list" => self
                .attached_to(s.id)
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
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
            "window_marked_flag" => "0".to_owned(),
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
            "window_active" => flag(wl.is_some_and(|(s, i)| s.current == i)),
            "window_last_flag" => flag(wl.is_some_and(|(s, i)| s.last.first() == Some(&i))),
            "window_start_flag" => {
                flag(wl.is_some_and(|(s, i)| s.windows.keys().next() == Some(&i)))
            }
            "window_end_flag" => {
                flag(wl.is_some_and(|(s, i)| s.windows.keys().next_back() == Some(&i)))
            }
            // window_printable_flags (window.c:1359-1386) for the flags
            // vtmux has (no marked pane or modal windows); `window_flags`
            // doubles the `#` of the activity flag.
            "window_flags" | "window_raw_flags" => {
                let (s, i) = wl?;
                let alerts = s.link_alerts.get(&i).copied().unwrap_or(0);
                let mut f = String::new();
                if alerts & alerts::ACTIVITY != 0 {
                    f.push_str(if name == "window_flags" { "##" } else { "#" });
                }
                if alerts & alerts::BELL != 0 {
                    f.push('!');
                }
                if alerts & alerts::SILENCE != 0 {
                    f.push('~');
                }
                if s.current == i {
                    f.push('*');
                }
                if s.last.first() == Some(&i) {
                    f.push('-');
                }
                if w.zoomed() {
                    f.push('Z');
                }
                f
            }
            // format_cb_window_linked: the window has more than one winlink
            // (a window linked twice in one session counts).
            "window_linked" => {
                wl?;
                flag(self.model.window_links(w.id).len() > 1)
            }
            // format_cb_window_linked_sessions: sessions linking it (no
            // session groups yet, so each session counts once).
            "window_linked_sessions" => {
                wl?;
                self.model.sessions_with(w.id).count().to_string()
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
            "window_layout" if self.client.as_ref().is_some_and(ClientView::old_layouts) => {
                w.full_layout().dump_v1()
            }
            "window_visible_layout"
                if self.client.as_ref().is_some_and(ClientView::old_layouts) =>
            {
                w.layout.dump_v1()
            }
            "window_layout" => w.full_layout().dump_v2(&self.layout_panes(w))?,
            "window_visible_layout" => w.layout.dump_v2(&self.layout_panes(w))?,
            _ => return None,
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
            "pane_pid" => p.pid.to_string(),
            "pane_width" => r.sx.to_string(),
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
            "pane_active" => flag(w.active() == p.id),
            "pane_last" => flag(w.panes.last.first() == Some(&p.id)),
            // window_pane_printable_flags (window.c:1389-1409) for the
            // flags vtmux has: active, last, zoomed, floating (no
            // float-over-zoom or modal panes).
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
                if w.is_floating(p.id) {
                    f.push('F');
                }
                f
            }
            "pane_floating_flag" => flag(w.is_floating(p.id)),
            "pane_zoomed_flag" => flag(w.zoom.as_ref().is_some_and(|z| z.pane == p.id)),
            "pane_input_off" => flag(p.input_off),
            // format_cb_pane_synchronized: the window's option.
            "pane_synchronized" => flag(
                self.model
                    .number_option(TreeId::Window(w.id), "synchronize-panes")
                    != 0,
            ),
            // No marked pane and no pipe-pane yet.
            "pane_marked" | "pane_pipe" => "0".to_owned(),
            "pane_unseen_changes" => flag(p.unseen_changes),
            "pane_dead" => flag(p.exited && io.is_none()),
            "pane_start_command" => crate::names::stringify_argv(&p.argv),
            // format_cb_start_path (format.c:943-955).
            "pane_start_path" => p.cwd.to_string_lossy().into_owned(),
            "pane_tty" => io
                .and_then(|io| sys::pty::tty_name(&io.owned().try_clone_to_owned().ok()?))
                .unwrap_or_default(),
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
            "alternate_on" => flag(p.term.alternate_on()),
            "history_size" => p.term.history_size().to_string(),
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
            "copy_cursor_hyperlink" => String::new(),
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
        let server = match name {
            "socket_path" => Some(self.info.socket_path.clone()),
            "pid" => Some(self.info.pid.to_string()),
            "uid" => Some(rustix::process::getuid().as_raw().to_string()),
            "user" => sys::user::current_user().map(|p| p.name),
            "version" => Some("3.8".to_owned()),
            "vtmux_version" => Some(env!("CARGO_PKG_VERSION").to_owned()),
            "current_file" => self.info.current_file.clone(),
            "host" => Some(self.info.host.clone()),
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
            // environment (format.c:4665-4675).
            .or_else(|| {
                let entry = self
                    .session()
                    .and_then(|s| s.env.find(name))
                    .or_else(|| self.global_env.find(name))?;
                entry.value.clone()
            })
    }

    /// Not for `t` lookups: format.c:4666 skips the environment then.
    fn get_without_environ(&self, name: &str) -> Option<String> {
        self.find_before_environ(name)
    }

    fn time(&self, name: &str) -> Option<i64> {
        match name {
            "session_created" => self.session().map(|s| s.created),
            "session_activity" => self.session().map(|s| s.activity_secs),
            "session_last_attached" => self.session().and_then(|s| s.last_attached),
            "window_activity" => self.window().map(|w| w.activity_secs),
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
