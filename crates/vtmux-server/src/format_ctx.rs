//! The [`FormatContext`] over the model (format.c `format_defaults` and the
//! `format_table` callbacks).

use vtmux_core::format::FormatContext;
use vtmux_core::layout::PaneStatus;
use vtmux_core::options::{OptionsStore, Target};

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
        }
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

    fn session_value(&self, name: &str) -> Option<String> {
        let s = self.session()?;
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "session_windows" => s.windows.len().to_string(),
            "session_created" => s.created.to_string(),
            "session_attached" | "session_grouped" => "0".to_owned(),
            "session_group" | "session_attached_list" => String::new(),
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
            "session_created" | "session_activity" => self.session().map(|s| s.created),
            _ => None,
        }
    }
}
