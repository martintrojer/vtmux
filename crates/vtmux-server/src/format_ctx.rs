//! The [`FormatContext`] over the model (format.c `format_defaults` and the
//! `format_table` callbacks).

use std::rc::Rc;

use vtmux_core::format::{FormatContext, LoopItem, LoopKeys, LoopOption, OptionScope, OptionsLoop};
use vtmux_core::ids::{SessionId, WindowId};
use vtmux_core::layout::PaneStatus;
use vtmux_core::options::{OptionValue, OptionsStore, Target, TreeId};

use crate::environ::Environ;
use crate::model::Model;
use crate::names::parse_window_name;
use crate::sys;
use crate::target::Found;

/// Server-wide values formats can read.
#[derive(Debug)]
pub struct ServerInfo {
    pub socket_path: String,
    pub pid: u32,
    pub host: String,
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
}

impl ClientView {
    /// `server_client_get_flags` (server-client.c:3163-3199) for the flags
    /// vtmux clients have: every listed client is attached.
    fn flags(&self) -> String {
        let mut flags = vec!["attached"];
        if self.focused {
            flags.push("focused");
        }
        if self.control {
            flags.push("control-mode");
        }
        if self.utf8 {
            flags.push("UTF-8");
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
    pub extra: Vec<(&'static str, String)>,
    /// The attached clients, in connection order (`#{L:...}`).
    pub clients: Rc<[ClientView]>,
    /// The client the formats are for (`ft->c`): its session marks the active item
    /// of `#{S:all,active}`.
    pub client: Option<ClientView>,
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
            target.pane = Some(w.active());
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
        }
    }

    /// A loop child: the same server view and client over another target.
    fn child(&self, target: Found, client: Option<ClientView>) -> Ctx<'a> {
        let mut c = Ctx::new(self.model, self.info, self.global_env, target);
        c.options = self.options;
        c.clients = Rc::clone(&self.clients);
        c.client = client;
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
    /// no pixel sizes (0, as tmux without them), discard count, theme or
    /// read-only clients yet.
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
            "client_readonly" => flag(false),
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
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "session_windows" => s.windows.len().to_string(),
            "session_created" => s.created.to_string(),
            "session_activity" => s.activity_secs.to_string(),
            "session_grouped" => "0".to_owned(),
            "session_group" => String::new(),
            // s->attached: the clients attached to it (format.c
            // format_cb_session_attached, recalculate_sizes).
            "session_attached" => self.attached_to(s.id).count().to_string(),
            // format_cb_session_attached_list: their names, in client order.
            "session_attached_list" => self
                .attached_to(s.id)
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
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
            // window_count_panes(w, 1).
            "window_panes" => w.panes.panes.len().to_string(),
            "window_width" => w.sx().to_string(),
            "window_height" => w.sy().to_string(),
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
            // vtmux has: current, last and zoomed (no alerts or marks yet).
            "window_flags" | "window_raw_flags" => {
                let (s, i) = wl?;
                let mut f = String::new();
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
            // format_cb_window_layout: the saved layout while zoomed.
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
            "pane_floating_flag" => flag(w.is_floating(p.id)),
            "pane_zoomed_flag" => flag(w.zoom.as_ref().is_some_and(|z| z.pane == p.id)),
            "pane_input_off" => flag(p.input_off),
            "pane_dead" => flag(p.exited && io.is_none()),
            "pane_start_command" => crate::names::stringify_argv(&p.argv),
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
    }
}

impl FormatContext for Ctx<'_> {
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
            "client_activity" => self.client.as_ref().map(|c| c.activity_secs),
            "client_created" => self.client.as_ref().map(|c| c.created_secs),
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
                        ctx: Box::new(self.child(
                            Found {
                                session: Some(s.id),
                                index: Some(index),
                                window: Some(w),
                                ..Found::default()
                            },
                            self.client.clone(),
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
                        ctx: Box::new(self.child(
                            Found {
                                pane: Some(p),
                                ..self.target
                            },
                            self.client.clone(),
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
                    ctx: Box::new(self.child(target, Some(c.clone()))),
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
