//! Switch mode (window-switch.c, `switch-mode`): every session (`-s`) or
//! every window of every session (`-w`), by name, filtered by a fuzzy
//! search typed into a prompt on the screen's last row. Enter runs the
//! command (default `switch-client -Zt '%%'`) on the selected row with
//! `%%` its target; Escape leaves. `-Z` zooms the pane while the mode is
//! open, `-k` kills it when the mode goes.
//!
//! The rows are the format (`-F`, default
//! [`DEFAULT_FORMAT`]) expanded for each session or window; the selected
//! row is in `mode-style`, the matched columns in
//! `switch-mode-match-style`.

use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::keys::{Key, MouseKind, MouseLocation};

use super::Mode;
use crate::open_prompt::{Line, Owner, Reply};
use crate::prompt::Kind;
use crate::prompt_history::PromptType;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `WINDOW_SWITCH_DEFAULT_COMMAND`.
pub const DEFAULT_COMMAND: &str = "switch-client -Zt '%%'";

/// `WINDOW_SWITCH_DEFAULT_FORMAT`.
pub const DEFAULT_FORMAT: &str = "#{?window_format,\
#{window_name} \
#[dim]#{session_name}:#{window_index}#{window_flags}#[default] \
#[dim]#{pane_current_command}#[default] \
#[dim]#{?#{!=:#{pane_title},#{host_short}},#{pane_title},}#[default],\
#{session_name} \
#[dim]#{session_windows} windows#[default] \
#{?session_attached,attached,#[dim]detached#[default]} \
#[dim]#{window_name}#[default]}";

/// What a row lists (`enum window_switch_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Session(SessionId),
    /// A session's window at an index.
    Window(SessionId, u32),
}

/// One row (`struct window_switch_itemdata`).
#[derive(Clone, Debug)]
pub struct Row {
    pub target: Target,
    /// The expanded format.
    pub text: String,
    /// The matched columns, if the filter matched anything.
    pub matched: Vec<usize>,
    score: u32,
    order: usize,
}

/// One switch mode on a pane's stack (`struct window_switch_modedata`).
#[derive(Clone, Debug)]
pub struct SwitchMode {
    pub key_mode: vtmux_vt::KeyMode,
    /// `-w`: windows; else sessions.
    pub windows: bool,
    pub format: String,
    pub command: String,
    pub filter: String,
    /// `data->matches`, in order.
    pub rows: Vec<Row>,
    pub current: usize,
    pub offset: usize,
    /// The screen size.
    pub sx: usize,
    pub sy: usize,
    /// `-Z` zoomed the window (`data->zoomed == 0`): unzoom on exit.
    pub unzoom: Option<WindowId>,
    /// `wme->kill`.
    pub kill: bool,
}

impl SwitchMode {
    /// `window_switch_visible`: the rows above the prompt.
    #[must_use]
    pub fn visible(&self) -> usize {
        self.sy.saturating_sub(1)
    }

    /// `window_switch_set_current`.
    fn set_current(&mut self, current: usize) {
        if self.rows.is_empty() {
            self.current = 0;
            self.offset = 0;
            return;
        }
        self.current = current.min(self.rows.len() - 1);
        let visible = self.visible();
        if self.current < self.offset {
            self.offset = self.current;
        } else if visible != 0 && self.current >= self.offset + visible {
            self.offset = self.current + 1 - visible;
        }
    }
}

/// `window_switch_key` (window-switch.c:563-570) first: the flags go
/// (`window_pane_key`), and C-p/C-k are Up, C-n/C-j Down, before the
/// prompt sees them.
#[must_use]
pub fn remap(key: Key) -> Key {
    let key = key.without_flags();
    let c = Key::ascii;
    if key == c(b'p').ctrl() || key == c(b'k').ctrl() {
        Key::UP
    } else if key == c(b'n').ctrl() || key == c(b'j').ctrl() {
        Key::DOWN
    } else {
        key
    }
}

/// The keys `window_switch_key` handles before its prompt: Enter and the
/// ways out (`key` through [`remap`]).
#[must_use]
pub fn takes_first(key: Key) -> bool {
    let c = Key::ascii;
    [
        c(b'\r'),
        c(0x1b),
        c(b'[').ctrl(),
        c(b'c').ctrl(),
        c(b'g').ctrl(),
    ]
    .contains(&key)
}

impl Server {
    /// `window_switch_build`: every session (or window of every session),
    /// sorted by name, its text the format, filtered and ordered by the
    /// fuzzy score (ties in list order).
    fn switch_build(&self, m: &mut SwitchMode) {
        let mut targets: Vec<(Target, String)> = Vec::new();
        for s in self.model.sessions_by_name() {
            if m.windows {
                for (&i, &w) in &s.windows {
                    let name = self.model.windows.get(&w).map_or("", |x| x.name.as_str());
                    targets.push((Target::Window(s.id, i), name.to_owned()));
                }
            } else {
                targets.push((Target::Session(s.id), s.name.clone()));
            }
        }
        // sort_winlink_cmp with SORT_NAME: by window name, a stable sort
        // keeping the session order between equal names (qsort is not
        // stable in general; glibc's is a merge sort for small arrays).
        if m.windows {
            targets.sort_by(|a, b| a.1.cmp(&b.1));
        }
        let clients = self.client_views();
        let mut rows = Vec::new();
        for (order, (target, _)) in targets.into_iter().enumerate() {
            let found = match target {
                Target::Session(s) => Found {
                    session: Some(s),
                    ..Found::default()
                },
                Target::Window(s, i) => {
                    let f = Found::from_winlink(&self.model, s, i);
                    Found { pane: None, ..f }
                }
            };
            let mut ctx =
                crate::format_ctx::Ctx::new(&self.model, &self.info, &self.global_env, found);
            ctx.clients = std::rc::Rc::clone(&clients);
            // format_create(NULL, ...) + format_defaults(ft, NULL, s, wl,
            // NULL): the type is the window's or the session's.
            let is_window = matches!(target, Target::Window(..));
            ctx.extra
                .push(("window_format".into(), u8::from(is_window).to_string()));
            ctx.extra
                .push(("session_format".into(), u8::from(!is_window).to_string()));
            ctx.extra.push(("pane_format".into(), "0".into()));
            let text = vtmux_core::format::expand_str(&m.format, &ctx);
            rows.push(Row {
                target,
                text,
                matched: Vec::new(),
                score: 0,
                order,
            });
        }
        if !m.filter.is_empty() {
            rows.retain_mut(
                |r| match vtmux_core::format::fuzzy_match(&m.filter, &r.text, m.sx) {
                    Some((score, cols)) => {
                        r.score = score;
                        r.matched = cols;
                        true
                    }
                    None => false,
                },
            );
            rows.sort_by(|a, b| b.score.cmp(&a.score).then(a.order.cmp(&b.order)));
        }
        m.rows = rows;
    }

    /// `switch-mode` (`window_switch_init`): the mode on pane `p` for
    /// client `c`, with its search prompt.
    pub(crate) fn enter_switch_mode(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        args: &vtmux_core::cmd::Args,
        found: Found,
    ) {
        if !self
            .model
            .pane_may_enter(p, |m| matches!(m, Mode::Switch(_)))
        {
            return;
        }
        self.reset_no_stack(p);
        let Some(w) = self.model.panes.get(&p).map(|x| x.window) else {
            return;
        };
        let mut unzoom = None;
        if args.flag('Z')
            && !self
                .model
                .windows
                .get(&w)
                .is_some_and(crate::model::Window::zoomed)
        {
            self.model.zoom(w, p);
            unzoom = Some(w);
        }
        let Some((sx, sy)) = self
            .model
            .panes
            .get(&p)
            .map(|x| (usize::from(x.term.cols()), usize::from(x.term.rows())))
        else {
            return;
        };
        let mut m = SwitchMode {
            key_mode: super::screen_key_mode(&self.model),
            windows: args.flag('w'),
            format: args.value('F').unwrap_or(DEFAULT_FORMAT).to_owned(),
            command: args
                .positional_text(0)
                .map_or_else(|| DEFAULT_COMMAND.to_owned(), std::borrow::Cow::into_owned),
            filter: String::new(),
            rows: Vec::new(),
            current: 0,
            offset: 0,
            sx,
            sy,
            unzoom,
            kill: args.flag('k'),
        };
        self.switch_build(&mut m);
        let existing = self
            .model
            .panes
            .get(&p)
            .and_then(|x| x.modes.iter().position(|m| matches!(m, Mode::Switch(_))));
        let kill = m.kill;
        self.model.push_mode(
            p,
            |m| matches!(m, Mode::Switch(_)),
            |_| Mode::Switch(Box::new(m)),
            kill,
        );
        if existing.is_some() {
            if let Some(Mode::Switch(s)) = self
                .model
                .panes
                .get_mut(&p)
                .and_then(|x| x.modes.first_mut())
            {
                s.kill = kill;
            }
            return;
        }
        // The prompt (prompt_create with PROMPT_INCREMENTAL|PROMPT_NOFORMAT|
        // PROMPT_ISMODE|PROMPT_EDITARROWS, the session's options).
        let mut editor = match c {
            Some(c) => self.prompt_editor(
                c,
                "(search) ".to_owned(),
                "",
                Kind::Incremental,
                PromptType::Search,
            ),
            None => crate::prompt::Prompt::new(
                "(search) ".to_owned(),
                "",
                Kind::Incremental,
                PromptType::Search,
            ),
        };
        editor.no_format = true;
        editor.edit_arrows = true;
        // The prompt is on the last row whatever status-position says
        // (window_switch_draw_screen: prompt_line = sy - 1).
        self.mode_prompt_set(p, c, editor, found, Owner::Mode(Box::new(SwitchPrompt)));
        if let Some(mp) = self
            .model
            .panes
            .get_mut(&p)
            .and_then(|x| x.mode_prompt.as_mut())
        {
            mp.top = false;
        }
    }

    /// Run `f` on pane `p`'s switch mode on top, then redraw the pane.
    fn with_switch<R>(
        &mut self,
        p: PaneId,
        f: impl FnOnce(&Server, &mut SwitchMode) -> R,
    ) -> Option<R> {
        let pane = self.model.panes.get_mut(&p)?;
        if !matches!(pane.modes.first(), Some(Mode::Switch(_))) {
            return None;
        }
        let Mode::Switch(mut m) = pane.modes.remove(0) else {
            unreachable!()
        };
        let r = f(self, &mut m);
        if let Some(pane) = self.model.panes.get_mut(&p) {
            pane.modes.insert(0, Mode::Switch(m));
        }
        self.model.redraw_panes.insert(p);
        Some(r)
    }

    /// `window_switch_resize`: built again for the new size.
    pub(crate) fn switch_resize(&mut self, p: PaneId) {
        let Some((sx, sy)) = self
            .model
            .panes
            .get(&p)
            .map(|x| (usize::from(x.term.cols()), usize::from(x.term.rows())))
        else {
            return;
        };
        self.with_switch(p, |server, m| {
            if (m.sx, m.sy) == (sx, sy) {
                return;
            }
            m.sx = sx;
            m.sy = sy;
            server.switch_build(m);
            m.set_current(m.current);
        });
    }

    /// Every switch mode whose pane changed size is built again.
    pub(crate) fn switch_resizes(&mut self) {
        let due: Vec<PaneId> = self
            .model
            .panes
            .iter()
            .filter(|(_, x)| match x.modes.first() {
                Some(Mode::Switch(m)) => {
                    (m.sx, m.sy) != (usize::from(x.term.cols()), usize::from(x.term.rows()))
                }
                _ => false,
            })
            .map(|(&p, _)| p)
            .collect();
        for p in due {
            self.switch_resize(p);
        }
    }

    /// `window_switch_run_command`: the selected row's target in the
    /// command, appended to client `c`'s queue. False with no rows.
    fn switch_run(&mut self, p: PaneId, c: Option<ClientId>) -> bool {
        let Some(Some((target, command))) = self.with_switch(p, |_, m| {
            m.rows.get(m.current).map(|r| (r.target, m.command.clone()))
        }) else {
            return false;
        };
        let (name, fs) = match target {
            Target::Session(s) => {
                let Some(session) = self.model.sessions.get(&s) else {
                    return false;
                };
                (
                    format!("={}:", session.name),
                    Found::from_session(&self.model, s),
                )
            }
            Target::Window(s, i) => {
                let Some(session) = self.model.sessions.get(&s) else {
                    return false;
                };
                if !session.windows.contains_key(&i) {
                    return false;
                }
                (
                    format!("={}:{i}.", session.name),
                    Found::from_winlink(&self.model, s, i),
                )
            }
        };
        self.tree_run_command(c, Some(fs), &command, &name);
        true
    }

    /// `window_switch_key` for a key the prompt did not use up.
    /// `key` is already through [`remap`].
    pub(crate) fn switch_key(&mut self, p: PaneId, c: Option<ClientId>, key: Key) {
        let c_ = |b: u8| Key::ascii(b);
        if key == c_(b'\r') {
            if self.switch_run(p, c) {
                self.reset_mode(p);
            }
            return;
        }
        if [c_(0x1b), c_(b'[').ctrl(), c_(b'c').ctrl(), c_(b'g').ctrl()].contains(&key) {
            self.reset_mode(p);
            return;
        }
        self.with_switch(p, |_, m| {
            let size = m.rows.len();
            let current = m.current;
            if key == Key::UP {
                if size != 0 {
                    m.set_current(if current == 0 { size - 1 } else { current - 1 });
                }
            } else if key == Key::DOWN {
                if size != 0 {
                    m.set_current(if current == size - 1 { 0 } else { current + 1 });
                }
            } else if key == Key::PPAGE {
                let visible = m.visible();
                m.set_current(current.saturating_sub(visible));
            } else if key == Key::NPAGE {
                let visible = m.visible();
                m.set_current(current + visible);
            } else if key == Key::HOME {
                m.set_current(0);
            } else if key == Key::END && size > 0 {
                m.set_current(size - 1);
            }
        });
    }

    /// `window_switch_key` for mouse event `ev` at `at` in the pane. A
    /// click on the prompt row (which tmux gives `prompt_mouse` to move the
    /// cursor) is not done: it does nothing.
    pub(crate) fn switch_mouse_key(
        &mut self,
        p: PaneId,
        c: ClientId,
        ev: &crate::mouse::MouseEvent,
        at: Option<(u32, u32)>,
    ) {
        let Some((_, y)) = at else {
            return;
        };
        let y = y as usize;
        let key = ev.key;
        let c = Some(c);
        let wheel_up = Key::mouse(MouseKind::WheelUp, 0, MouseLocation::Pane);
        let wheel_down = Key::mouse(MouseKind::WheelDown, 0, MouseLocation::Pane);
        let down = Key::mouse(MouseKind::Down, 1, MouseLocation::Pane);
        let double = Key::mouse(MouseKind::DoubleClick, 1, MouseLocation::Pane);
        let key = key.without_flags();
        if key == wheel_up || key == wheel_down {
            self.with_switch(p, |_, m| {
                let (size, current) = (m.rows.len(), m.current);
                if key == wheel_up && size != 0 && current != 0 {
                    m.set_current(current - 1);
                } else if key == wheel_down && size != 0 && current != size - 1 {
                    m.set_current(current + 1);
                }
            });
            return;
        }
        if key != down && key != double {
            return;
        }
        let hit = self.with_switch(p, |_, m| {
            if y >= m.visible() || m.offset + y >= m.rows.len() {
                return false;
            }
            m.set_current(m.offset + y);
            true
        });
        if hit == Some(true) && key == double && self.switch_run(p, c) {
            self.reset_mode(p);
        }
    }
}

/// The search prompt's owner (`window_switch_prompt_callback`): each edit
/// filters the rows again and selects the first.
#[derive(Debug)]
struct SwitchPrompt;

impl super::prompt::ModePromptOwner for SwitchPrompt {
    fn input(&mut self, server: &mut Server, p: PaneId, _c: Option<ClientId>, line: Line) -> Reply {
        let Line::Changed(s) = line else {
            return Reply::Continue;
        };
        // The incremental prefix (`=`, `+`, `-`) goes.
        let filter = s.get(1..).unwrap_or("").to_owned();
        server.with_switch(p, |server, m| {
            m.filter = filter;
            server.switch_build(m);
            m.current = 0;
            m.offset = 0;
        });
        Reply::Continue
    }
}
