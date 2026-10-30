//! The [`FormatContext`] over the model (format.c `format_defaults` and the
//! `format_table` callbacks).

use vtmux_core::format::{FormatContext, Tm};
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
            target.pane = Some(w.active);
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
        let current = self
            .session()
            .is_some_and(|s| Some(s.current) == self.target.index);
        Some(match name {
            "window_id" => w.id.to_string(),
            "window_index" => self.target.index?.to_string(),
            "window_name" => w.name.clone(),
            "window_panes" => w.layout.panes().count().to_string(),
            "window_width" => w.cols.to_string(),
            "window_height" => w.rows.to_string(),
            "window_active" => u8::from(current).to_string(),
            "window_flags" => if current { "*" } else { "" }.to_owned(),
            _ => return None,
        })
    }

    fn pane_value(&self, name: &str) -> Option<String> {
        let p = self.pane()?;
        let io = p.io.as_ref();
        Some(match name {
            "pane_id" => p.id.to_string(),
            "pane_index" => self.model.pane_index(p.id)?.to_string(),
            "pane_pid" => p.pid.to_string(),
            "pane_width" => p.term.cols().to_string(),
            "pane_height" => p.term.rows().to_string(),
            "pane_title" => p.term.title(),
            "pane_active" => u8::from(self.window().is_some_and(|w| w.active == p.id)).to_string(),
            "pane_dead" => u8::from(p.exited && io.is_none()).to_string(),
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
            "alternate_on" => u8::from(p.term.alternate_on()).to_string(),
            "history_size" => p.term.history_size().to_string(),
            _ => return None,
        })
    }
}

impl FormatContext for Ctx<'_> {
    fn get(&self, name: &str) -> Option<String> {
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
            // format_find falls back to the session, then global,
            // environment (format.c:4665-4675).
            .or_else(|| {
                self.session()?
                    .env
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            })
            .or_else(|| self.global_env.get(name).map(str::to_owned))
    }

    fn time(&self, name: &str) -> Option<i64> {
        match name {
            "session_created" | "session_activity" => self.session().map(|s| s.created),
            _ => None,
        }
    }

    fn local_time(&self, epoch: i64) -> Option<Tm> {
        sys::user::local_time(epoch)
    }
}
