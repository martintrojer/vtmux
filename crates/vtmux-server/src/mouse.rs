//! Mouse events from an attached client's terminal: where they are and
//! which key they become (server-client.c `server_client_check_mouse`,
//! 858-1260), the click timer (`server_client_click_timer`), the mouse
//! target (`cmd_mouse_pane`, `cmd_mouse_window`, `cmd_mouse_at`) and the
//! outer terminal's mouse mode (`server_client_reset_state` and
//! `tty_update_mode`, tty.c:882-921).
//!
//! The located event rides on the key batch (tmux's `cmdq_state.event`):
//! `=` and `{mouse}` targets and the `mouse_*` formats read it.
//!
//! Under the focus ring (D23) the window is drawn one cell in from the
//! corner of its area; a position is moved back by that cell before it is
//! located, and a click on the ring's frame is the border of the pane it
//! frames.
//!
//! Not here: forwarding to panes (`send -M`, the unbound path:
//! `mouse_forward`), drag callbacks (`resize-pane -M`, `copy-mode -M`:
//! `mouse_drag`), scrollbars (off by default), pane-border control ranges
//! (`window_pane_status_get_range`) and menus.

use std::time::{Duration, Instant};

use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::keys::{Key, MouseKind, MouseLocation};
use vtmux_core::options::TreeId;
use vtmux_core::style::RangeKind;
use vtmux_vt::MouseTracking;

use crate::input::{MOUSE_MASK_BUTTONS, MouseInput, mouse_wheel};
use crate::model::Model;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `KEYC_CLICK_TIMEOUT` (tmux.h:234), in milliseconds.
const CLICK_TIMEOUT: Duration = Duration::from_millis(300);

/// `MOUSE_MASK_*` (tmux.h:1655-1660).
const MASK_SHIFT: u32 = 4;
const MASK_META: u32 = 8;
const MASK_CTRL: u32 = 16;
const MASK_DRAG: u32 = 32;

fn drag(b: u32) -> bool {
    b & MASK_DRAG != 0
}

fn release(b: u32) -> bool {
    b & MOUSE_MASK_BUTTONS == 3
}

/// What a mouse event names (`m->s`, `m->w`, `m->wp`): the targets `=`
/// and `{mouse}` resolve to, looked up when a command asks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseTarget {
    pub session: Option<SessionId>,
    pub window: Option<WindowId>,
    pub pane: Option<PaneId>,
}

impl MouseTarget {
    /// `cmd_mouse_window`: the session's window the event names, or its
    /// current one.
    #[must_use]
    pub fn window_found(&self, model: &Model) -> Option<Found> {
        let s = self.session?;
        let session = model.sessions.get(&s)?;
        let index = match self.window {
            None => session.current,
            Some(w) => *session.windows.iter().find(|(_, x)| **x == w)?.0,
        };
        let f = Found::from_winlink(model, s, index);
        f.window.is_some().then_some(f)
    }

    /// `cmd_mouse_pane`: the pane the event names (the window's active one
    /// if none), in its window.
    #[must_use]
    pub fn pane_found(&self, model: &Model) -> Option<Found> {
        let mut f = self.window_found(model)?;
        if let Some(p) = self.pane {
            let pane = model.panes.get(&p)?;
            if Some(pane.window) != f.window {
                return None;
            }
            f.pane = Some(p);
        }
        f.pane?;
        Some(f)
    }
}

/// A located mouse event (`struct mouse_event` once `m->valid`): what the
/// key's commands see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    /// The bound key (`m->key`).
    pub key: Key,
    /// The report, with the previous position.
    pub input: MouseInput,
    /// `m->statusat` (`None` for no status line) and `m->statuslines`.
    pub status_at: Option<u32>,
    pub status_lines: u32,
    pub target: MouseTarget,
    /// The focus ring's offset of the window the event was located in.
    pub offset: u32,
    /// The status line's `range=` under the event, when it is on the
    /// status line (`status_get_range`, for `mouse_status_range`).
    pub status_range: Option<RangeKind>,
}

impl MouseEvent {
    /// `cmd_mouse_at` (cmd.c:764-791): the position inside pane `p`, if it
    /// is in it.
    #[must_use]
    pub fn at(&self, model: &Model, p: PaneId) -> Option<(u32, u32)> {
        let pane = model.panes.get(&p)?;
        let x = i64::from(self.input.x);
        let mut y = i64::from(self.input.y);
        if self.status_at == Some(0) && y >= i64::from(self.status_lines) {
            y -= i64::from(self.status_lines);
        }
        let (x, y) = (x - i64::from(self.offset), y - i64::from(self.offset));
        let r = pane.rect;
        let (xoff, yoff) = (i64::from(r.xoff), i64::from(r.yoff));
        if x < xoff || x >= xoff + i64::from(r.sx) || y < yoff || y >= yoff + i64::from(r.sy) {
            return None;
        }
        Some((u32::try_from(x - xoff).ok()?, u32::try_from(y - yoff).ok()?))
    }

    /// The `mouse_*` formats (format.c:1310-1440, 2032-2125) for a client
    /// whose terminal has started (`started`). `mouse_word`, `mouse_line`
    /// and `mouse_hyperlink` are not here yet: empty, as with no event.
    #[must_use]
    pub fn formats(&self, model: &Model, started: bool) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        let pane = self.target.pane_found(model).and_then(|f| f.pane);
        if let Some(p) = pane {
            out.push(("mouse_pane", p.to_string()));
        }
        let on_status = self.status_line().filter(|_| started);
        match pane.and_then(|p| self.at(model, p)) {
            Some((x, y)) => {
                out.push(("mouse_x", x.to_string()));
                out.push(("mouse_y", y.to_string()));
            }
            None => {
                if let Some(line) = on_status {
                    out.push(("mouse_x", self.input.x.to_string()));
                    out.push(("mouse_y", line.to_string()));
                }
            }
        }
        if let Some(line) = on_status {
            out.push(("mouse_status_line", line.to_string()));
            let range = match &self.status_range {
                Some(RangeKind::Left) => Some("left".to_owned()),
                Some(RangeKind::Right) => Some("right".to_owned()),
                Some(RangeKind::Pane(_)) => Some("pane".to_owned()),
                Some(RangeKind::Window(_)) => Some("window".to_owned()),
                Some(RangeKind::Session(_)) => Some("session".to_owned()),
                Some(RangeKind::User(name)) => Some(name.clone()),
                Some(RangeKind::Control(_)) => Some("control".to_owned()),
                None => None,
            };
            if let Some(r) = range {
                out.push(("mouse_status_range", r));
            }
        }
        out
    }

    /// Whether the event is on the status line, and the line: for
    /// `mouse_status_line` and the status branches of `mouse_x`/`mouse_y`.
    #[must_use]
    pub fn status_line(&self) -> Option<u32> {
        let at = self.status_at?;
        let y = self.input.y;
        if at == 0 && y < self.status_lines {
            Some(y)
        } else if at > 0 && y >= at {
            Some(y - at)
        } else {
            None
        }
    }
}

/// A client's mouse state (`c->tty.mouse_*`, `c->click_*`, the
/// `CLIENT_DOUBLECLICK` and `CLIENT_TRIPLECLICK` flags).
#[derive(Clone, Debug, Default)]
pub struct ClientMouse {
    /// `mouse_drag_flag`: the button of the drag in progress, plus one.
    drag_flag: u32,
    /// `mouse_drag_status`: the drag started on the status line.
    drag_status: bool,
    drag_x: u32,
    drag_y: u32,
    /// `mouse_last_pane`.
    last_pane: Option<PaneId>,
    /// `CLIENT_DOUBLECLICK` / `CLIENT_TRIPLECLICK`.
    double: bool,
    triple: bool,
    /// `c->click_timer`, and the event it fires with (`c->click_event`).
    click_deadline: Option<Instant>,
    click_event: Option<MouseInput>,
    click_button: u32,
    click_loc: Option<MouseLocation>,
    click_pane: Option<PaneId>,
    /// The outer terminal's mouse modes as last written (`tty->mode &
    /// ALL_MOUSE_MODES`); `None` before the first update after the
    /// terminal started (`tty_invalidate` sets every mode).
    outer: Option<MouseTracking>,
}

impl ClientMouse {
    /// When the click timer fires.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.click_deadline
    }

    /// `tty_invalidate`: the next update writes the modes again.
    pub fn invalidate(&mut self) {
        self.outer = None;
    }
}

/// What a report becomes.
pub enum Located {
    /// `KEYC_UNKNOWN`: nothing.
    Nothing,
    /// A key with its event.
    Key(MouseEvent),
}

impl Server {
    /// `status_at_line` and `status_line_size` for client `id`: the line
    /// the status starts at and how many lines it has.
    fn status_place(&self, id: ClientId) -> (Option<u32>, u32) {
        let Some(c) = self.clients.get(&id) else {
            return (None, 0);
        };
        let lines = self.status_line_size(c);
        if lines == 0 {
            return (None, 0);
        }
        let Some(s) = c.attached else {
            return (None, 0);
        };
        if self.model.status_top(TreeId::Session(s)) {
            return (Some(0), lines);
        }
        let rows = c
            .hello
            .as_ref()
            .and_then(|h| h.tty.as_ref())
            .map_or(24, |t| t.rows);
        (Some(rows.saturating_sub(lines)), lines)
    }

    /// `status_get_range`: the `range=` of status line `line` at column `x`
    /// for client `id`. The ranges are drawn again from the formats now,
    /// where tmux keeps those of the last redraw.
    pub(crate) fn status_range_at(&self, id: ClientId, x: u32, line: u32) -> Option<RangeKind> {
        let c = self.clients.get(&id)?;
        let s = c.attached?;
        let cols = c.hello.as_ref()?.tty.as_ref()?.cols;
        let lines = self.status_line_size(c);
        let source = crate::render::window::Source {
            model: &self.model,
            info: &self.info,
            env: &self.global_env,
        };
        let ctx = self.format_ctx(Found::from_session(&self.model, s), Some(id));
        let ranges = crate::render::status::status_ranges(
            &source,
            &ctx,
            s,
            usize::try_from(cols).ok()?,
            usize::try_from(lines).ok()?,
        );
        let x = usize::try_from(x).ok()?;
        ranges
            .get(usize::try_from(line).ok()?)?
            .iter()
            .find(|r| x >= r.start && x < r.end)
            .map(|r| r.kind.clone())
    }

    /// `server_client_check_mouse` for client `id`: the key a report is
    /// bound as, with its event. `double` is the click timer's
    /// `KEYC_DOUBLECLICK` with the saved event.
    pub(crate) fn check_mouse(&mut self, id: ClientId, m: MouseInput, double: bool) -> Located {
        let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
            return Located::Nothing;
        };
        let Some((_, w, _)) = self.model.current_of(s) else {
            return Located::Nothing;
        };
        let mut st = std::mem::take(&mut self.clients.get_mut(&id).expect("client").mouse);
        let located = self.check_mouse_with(&mut st, id, s, w, m, double);
        if let Some(c) = self.clients.get_mut(&id) {
            c.mouse = st;
        }
        located
    }

    #[allow(clippy::too_many_lines, reason = "one function, as tmux's")]
    fn check_mouse_with(
        &self,
        st: &mut ClientMouse,
        id: ClientId,
        session: SessionId,
        window: WindowId,
        report: MouseInput,
        double: bool,
    ) -> Located {
        let lwp = st.last_pane.filter(|p| self.model.panes.contains_key(p));
        let sgr_drag_motion = report.sgr.is_some_and(|g| drag(g.b) && release(g.b));
        let x10_motion =
            report.sgr.is_none() && drag(report.b) && release(report.b) && release(report.lb);
        let (kind, mut x, mut y, b);
        if double {
            (kind, x, y, b) = (MouseKind::DoubleClick, report.x, report.y, report.b);
        } else if sgr_drag_motion || x10_motion {
            (kind, x, y, b) = (MouseKind::Move, report.x, report.y, 0);
        } else if drag(report.b) {
            kind = MouseKind::Drag;
            if st.drag_flag != 0 {
                (x, y, b) = (report.x, report.y, report.b);
                if x == report.lx && y == report.ly {
                    return Located::Nothing;
                }
                if st.drag_status {
                    (x, y) = (st.drag_x, st.drag_y);
                }
            } else {
                (x, y, b) = (report.lx, report.ly, report.lb);
                st.drag_status = false;
            }
        } else if mouse_wheel(report.b) {
            kind = if report.b & MOUSE_MASK_BUTTONS == 64 {
                MouseKind::WheelUp
            } else {
                MouseKind::WheelDown
            };
            (x, y, b) = (report.x, report.y, report.b);
        } else if release(report.b) {
            kind = MouseKind::Up;
            (x, y) = (report.x, report.y);
            b = report.sgr.filter(|g| g.release).map_or(report.lb, |g| g.b);
        } else if st.double {
            st.click_deadline = None;
            st.double = false;
            st.triple = true;
            (kind, x, y, b) = (MouseKind::SecondClick, report.x, report.y, report.b);
        } else if st.triple {
            st.click_deadline = None;
            st.triple = false;
            (kind, x, y, b) = (MouseKind::TripleClick, report.x, report.y, report.b);
        } else {
            (kind, x, y, b) = (MouseKind::Down, report.x, report.y, report.b);
            st.double = true;
        }

        let (status_at, status_lines) = self.status_place(id);
        let mut ev = MouseEvent {
            key: Key::NONE,
            input: report,
            status_at,
            status_lines,
            target: MouseTarget {
                session: Some(session),
                ..MouseTarget::default()
            },
            offset: 0,
            status_range: None,
        };
        let mut loc = None;
        let (mut px, mut py) = (0, 0);
        // Is this on the status line?
        if let Some(at) = status_at
            && y >= at
            && y < at + status_lines
        {
            if kind == MouseKind::Drag && st.drag_flag == 0 {
                st.drag_status = true;
            }
            ev.status_range = self.status_range_at(id, x, y - at);
            loc = Some(match ev.status_range.clone() {
                None => MouseLocation::StatusDefault,
                Some(RangeKind::Left) => MouseLocation::StatusLeft,
                Some(RangeKind::Right) => MouseLocation::StatusRight,
                Some(RangeKind::Pane(id)) => {
                    let pane = PaneId(id);
                    if !self.model.panes.contains_key(&pane) {
                        return Located::Nothing;
                    }
                    ev.target.pane = Some(pane);
                    MouseLocation::Status
                }
                Some(RangeKind::Window(n)) => {
                    let Some(&rw) = self
                        .model
                        .sessions
                        .get(&session)
                        .and_then(|x| x.windows.get(&n))
                    else {
                        return Located::Nothing;
                    };
                    ev.target.window = Some(rw);
                    MouseLocation::Status
                }
                Some(RangeKind::Session(n)) => {
                    let rs = SessionId(n);
                    if !self.model.sessions.contains_key(&rs) {
                        return Located::Nothing;
                    }
                    ev.target.session = Some(rs);
                    MouseLocation::Status
                }
                Some(RangeKind::User(_)) => MouseLocation::Status,
                Some(RangeKind::Control(n)) => MouseLocation::control(n),
            });
        }

        // Not on the status line: the window area.
        let mut wp = None;
        if loc.is_none() {
            px = x;
            py = if status_at == Some(0) && y >= status_lines {
                y - status_lines
            } else if let Some(at) = status_at.filter(|at| *at > 0 && y >= *at) {
                at - 1
            } else {
                y
            };
            let win = &self.model.windows[&window];
            // D23: the window sits one cell in under the focus ring.
            let ring = self
                .model
                .number_option(TreeId::Window(window), "vtmux-focus-ring")
                != 0;
            ev.offset = u32::from(ring);
            let (sx, sy) = (win.sx() + 2 * ev.offset, win.sy() + 2 * ev.offset);
            if px > sx || py > sy {
                return Located::Nothing;
            }
            // Window coordinates: the ring's frame is one cell outside.
            let (wx, wy) = (
                i64::from(px) - i64::from(ev.offset),
                i64::from(py) - i64::from(ev.offset),
            );
            // The ring's frame (the cells just outside the window, corners
            // included) is the border of the pane it frames: the pane at
            // the nearest window cell.
            let (wsx, wsy) = (i64::from(win.sx()), i64::from(win.sy()));
            let frame = ring
                && (-1..=wsx).contains(&wx)
                && (-1..=wsy).contains(&wy)
                && (wx < 0 || wy < 0 || wx >= wsx || wy >= wsy);
            let found = if kind == MouseKind::Drag && lwp.is_some() {
                lwp
            } else if frame {
                self.model
                    .pane_at(window, wx.clamp(0, wsx - 1), wy.clamp(0, wsy - 1))
            } else {
                self.model.pane_at(window, wx, wy)
            };
            match found {
                None => {
                    loc = Some(MouseLocation::Empty);
                    ev.target.window = Some(window);
                }
                Some(p) => {
                    loc = if frame {
                        Some(MouseLocation::Border)
                    } else {
                        self.mouse_in_pane(window, p, wx, wy)
                    };
                    ev.target.pane = Some(p);
                    ev.target.window = Some(window);
                    wp = Some(p);
                }
            }
        }

        // Reset the click type or start a click timer.
        let mut kind = kind;
        if matches!(
            kind,
            MouseKind::Down | MouseKind::SecondClick | MouseKind::TripleClick
        ) {
            if kind != MouseKind::Down
                && (report.b != st.click_button
                    || loc != st.click_loc
                    || ev.target.pane != st.click_pane)
            {
                kind = MouseKind::Down;
                st.triple = false;
                st.double = true;
            }
            if kind != MouseKind::TripleClick {
                st.click_event = Some(report);
                st.click_button = report.b;
                st.click_loc = loc;
                st.click_pane = ev.target.pane;
                st.click_deadline = Some(Instant::now() + CLICK_TIMEOUT);
            }
        }
        let Some(loc) = loc else {
            return Located::Nothing;
        };

        // Stop dragging if needed: the drag's button ends it.
        let mut b = b;
        if !matches!(
            kind,
            MouseKind::Drag
                | MouseKind::WheelUp
                | MouseKind::WheelDown
                | MouseKind::DoubleClick
                | MouseKind::TripleClick
        ) && st.drag_flag != 0
        {
            // TODO(mouse_drag): mouse_drag_release.
            kind = MouseKind::DragEnd;
            b = st.drag_flag - 1;
            st.drag_flag = 0;
            st.last_pane = None;
        }

        let mut key = None;
        if kind == MouseKind::Move && loc == MouseLocation::Pane {
            // focus-follows-mouse (off by default) needs all-motion mode,
            // which is only on when a pane asks for it.
            key = Some(Key::mouse(MouseKind::Move, 0, MouseLocation::Pane));
        }
        if kind == MouseKind::Drag {
            // TODO(mouse_drag): KEYC_DRAGGING once a command installs
            // mouse_drag_update.
            if st.drag_flag == 0 {
                (st.drag_x, st.drag_y) = if st.drag_status { (x, y) } else { (px, py) };
            }
            st.drag_flag = (b & MOUSE_MASK_BUTTONS) + 1;
            if !st.drag_status && lwp.is_none() {
                st.last_pane = wp.or_else(|| {
                    let off = i64::from(ev.offset);
                    self.model
                        .pane_at(window, i64::from(px) - off, i64::from(py) - off)
                });
            }
        }
        let mut key = key.unwrap_or_else(|| {
            let button = match b & MOUSE_MASK_BUTTONS {
                0 => 1,
                1 => 2,
                2 => 3,
                66 => 6,
                67 => 7,
                128 => 8,
                129 => 9,
                130 => 10,
                131 => 11,
                _ => 0,
            };
            Key::mouse(kind, button, loc)
        });
        if b & MASK_META != 0 {
            key = key.meta();
        }
        if b & MASK_CTRL != 0 {
            key = key.ctrl();
        }
        if b & MASK_SHIFT != 0 {
            key = key.shift();
        }
        // ox/oy are 0: vtmux does not pan a window larger than the client.
        ev.key = key;
        Located::Key(ev)
    }

    /// `server_client_check_mouse_in_pane` (server-client.c:711-855) for a
    /// tiled or floating pane without scrollbars: inside the pane, on a
    /// border, or nowhere. `x`, `y` are window coordinates.
    fn mouse_in_pane(&self, wid: WindowId, p: PaneId, x: i64, y: i64) -> Option<MouseLocation> {
        let window = self.model.windows.get(&wid)?;
        let pane = self.model.panes.get(&p)?;
        let rect = pane.rect;
        let (xoff, yoff) = (i64::from(rect.xoff), i64::from(rect.yoff));
        let (sx, sy) = (i64::from(rect.sx), i64::from(rect.sy));
        let status = self.model.pane_status(wid);
        let status_line = match status {
            vtmux_core::layout::PaneStatus::Top => yoff - 1,
            vtmux_core::layout::PaneStatus::Bottom => yoff + sy,
            vtmux_core::layout::PaneStatus::Off => -1,
        };
        let off = status != vtmux_core::layout::PaneStatus::Off;
        let in_rows = (off && y != status_line && y != yoff + sy)
            || (yoff == 0 && y < sy)
            || (y >= yoff && y < yoff + sy);
        if in_rows && x < xoff + sx {
            let floating = window.is_floating(p);
            let lines = self
                .model
                .number_option(TreeId::Pane(p), "pane-border-lines");
            // pane-border-lines none (6) on a floating pane: no border.
            if floating && lines != 6 && (x == xoff - 1 || y == yoff - 1 || y == yoff + sy) {
                return Some(MouseLocation::Border);
            }
            return Some(MouseLocation::Pane);
        }
        // Try the pane borders.
        let border = window.panes.panes.iter().any(|&q| {
            if !window.is_visible(q) {
                return false;
            }
            let Some(other) = self.model.panes.get(&q) else {
                return false;
            };
            let floating = window.is_floating(q);
            if floating
                && self
                    .model
                    .number_option(TreeId::Pane(q), "pane-border-lines")
                    == 6
            {
                return false;
            }
            let r = other.rect;
            let (fx, fy) = (i64::from(r.xoff), i64::from(r.yoff));
            let (fsx, fsy) = (i64::from(r.sx), i64::from(r.sy));
            let left = fx - 1;
            if y >= fy - 1 && y <= fy + fsy {
                if x == fx + fsx {
                    return true;
                }
                if window.is_floating(p) && x == left {
                    return true;
                }
            }
            x >= left && x <= fx + fsx && (y == fy + fsy || y == fy - 1)
        });
        border.then_some(MouseLocation::Border)
    }

    /// `window_pane_key` for mouse key `ev` from client `c` (window.c:
    /// 2072-2106): a pane in a mode gives it to the mode (motion is
    /// dropped); clock mode ends on it, tree modes select a row with it.
    /// A pane's own program does not get it yet.
    pub(crate) fn forward_mouse(&mut self, p: PaneId, c: ClientId, ev: &MouseEvent) {
        let Some(pane) = self.model.panes.get(&p) else {
            return;
        };
        // TODO(mouse_forward): with no mode, input_key_pane for a pane
        // that asked for mouse events, when m->wp is this pane. Copy mode
        // takes keys only through its table.
        match pane.modes.first() {
            Some(_) if ev.key.mouse_kind() == Some(MouseKind::Move) => {}
            Some(crate::mode::Mode::Clock(_)) => self.reset_mode(p),
            Some(crate::mode::Mode::Tree(_)) => {
                let at = ev.at(&self.model, p);
                self.tree_mouse_key(p, c, ev.key, at);
            }
            // window_panes_key: a mouse key keeps the mode. Choosing the
            // pane box clicked (window_panes_find_pane) is not here yet.
            Some(crate::mode::Mode::Panes(_)) => self.panes_key(p, c, ev.key),
            _ => {}
        }
    }

    /// `server_client_click_timer`: the click timers that are due fire a
    /// double click if a second click is waiting for a third.
    pub(crate) fn click_timers(&mut self, now: Instant) {
        let due: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.mouse.click_deadline.is_some_and(|d| d <= now))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            let c = self.clients.get_mut(&id).expect("listed");
            c.mouse.click_deadline = None;
            let fire = c.mouse.triple.then_some(c.mouse.click_event).flatten();
            c.mouse.double = false;
            c.mouse.triple = false;
            if let Some(m) = fire {
                self.handle_mouse_input(id, m, Vec::new(), true);
            }
        }
        self.cmdq_process();
    }

    /// `server_client_reset_state` and `tty_update_mode` for the mouse
    /// modes of client `id`: with `mouse` on, button mode (all-motion when
    /// a pane of the window asked for it); with it off, the active pane's
    /// own mode. The bytes for a change, if any.
    pub(crate) fn outer_mouse_bytes(&mut self, id: ClientId) -> Vec<u8> {
        let Some(c) = self.clients.get(&id) else {
            return Vec::new();
        };
        let Some(s) = c.attached else {
            return Vec::new();
        };
        let kmous = c
            .term
            .as_ref()
            .is_some_and(|t| t.has(vtmux_core::term_features::Cap::Kmous));
        let popup = c.popup.is_some();
        let prompt = c.status.prompt().is_some();
        let Some((_, w, active)) = self.model.current_of(s) else {
            return Vec::new();
        };
        // The screen whose modes count: the popup's, the active pane's, or
        // the status line's (no mouse modes) under a prompt.
        let mut mode = if let Some(p) = c.popup.as_ref() {
            p.term.mouse_modes().tracking
        } else if prompt {
            MouseTracking::None
        } else {
            self.model
                .panes
                .get(&active)
                .map_or(MouseTracking::None, |p| p.term.mouse_modes().tracking)
        };
        let tree = TreeId::Session(s);
        if self.model.number_option(tree, "mouse") != 0 {
            if !popup {
                mode = MouseTracking::None;
                let any_all = self.model.windows.get(&w).is_some_and(|window| {
                    window.panes.panes.iter().any(|p| {
                        self.model
                            .panes
                            .get(p)
                            .is_some_and(|x| x.term.mouse_modes().tracking == MouseTracking::All)
                    })
                });
                if any_all {
                    mode = MouseTracking::All;
                }
            }
            if self.model.number_option(tree, "focus-follows-mouse") != 0 {
                mode = MouseTracking::All;
            } else if mode != MouseTracking::All {
                mode = MouseTracking::Button;
            }
        }
        let c = self.clients.get_mut(&id).expect("client");
        if c.mouse.outer == Some(mode) {
            return Vec::new();
        }
        // `outer` is `None` after tty_invalidate (every mode set), so the
        // first update after the terminal starts always writes
        // (tty.c:2319-2320).
        c.mouse.outer = Some(mode);
        if !kmous {
            return Vec::new();
        }
        let mut out = b"\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l".to_vec();
        if mode != MouseTracking::None {
            out.extend_from_slice(b"\x1b[?1006h");
        }
        out.extend_from_slice(match mode {
            MouseTracking::All => b"\x1b[?1000h\x1b[?1002h\x1b[?1003h",
            MouseTracking::Button => b"\x1b[?1000h\x1b[?1002h",
            MouseTracking::Standard => b"\x1b[?1000h",
            MouseTracking::None => b"",
        });
        out
    }
}
