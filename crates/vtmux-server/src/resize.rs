//! Window sizes from the clients that see them (resize.c), and the client
//! that last used each window (`w->latest`, server-client.c).
//!
//! `window-size` picks how: the largest or smallest client, a fixed size
//! (`manual`, set by resize-window), or the latest client when several
//! see the window. `aggressive-resize` counts only clients whose current
//! window it is. A new size waits (`WINDOW_RESIZE`) until the window is
//! current in an attached session ([`Server::check_window_resizes`]);
//! a manual one applies at once.

use std::time::Instant;

use vtmux_core::ids::{SessionId, WindowId};
use vtmux_core::options::TreeId;

use crate::server::{Client, ClientId, Server};

/// `WINDOW_MINIMUM` and `WINDOW_MAXIMUM` (tmux.h:115-116).
pub const WINDOW_MINIMUM: u32 = 1;
pub const WINDOW_MAXIMUM: u32 = 10_000;

/// `window-size` (options-table.c:91, `WINDOW_SIZE_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizePolicy {
    Largest,
    Smallest,
    Manual,
    Latest,
}

impl SizePolicy {
    /// The option's choice index.
    #[must_use]
    pub fn from_choice(n: i64) -> Self {
        match n {
            0 => Self::Largest,
            1 => Self::Smallest,
            2 => Self::Manual,
            _ => Self::Latest,
        }
    }

    /// The choice index for setting the option.
    #[must_use]
    pub fn choice(self) -> usize {
        match self {
            Self::Largest => 0,
            Self::Smallest => 1,
            Self::Manual => 2,
            Self::Latest => 3,
        }
    }
}

/// Which clients a size is worked out from (`skip_client`).
enum Among {
    /// `default_window_size_skip_client`: clients of sessions that have
    /// `window`, or attached to `session` when there is no window.
    Default {
        session: SessionId,
        window: Option<WindowId>,
    },
    /// `recalculate_size_skip_client`: clients of sessions that have the
    /// window, or (`current`, aggressive-resize) whose current window it is.
    Window { window: WindowId, current: bool },
}

fn clamp_size((sx, sy): (u32, u32)) -> (u32, u32) {
    (
        sx.clamp(WINDOW_MINIMUM, WINDOW_MAXIMUM),
        sy.clamp(WINDOW_MINIMUM, WINDOW_MAXIMUM),
    )
}

/// `default-size` as `%ux%u`.
fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (x, y) = text.split_once('x')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

impl Server {
    /// `window-size` of window `w`.
    #[must_use]
    pub fn window_policy(&self, w: WindowId) -> SizePolicy {
        SizePolicy::from_choice(self.model.number_option(TreeId::Window(w), "window-size"))
    }

    /// `default-size` seen from `tree`, 80x24 when it does not parse.
    #[must_use]
    pub fn default_size(&self, tree: TreeId) -> (u32, u32) {
        self.model
            .options
            .get(tree, "default-size")
            .map(|e| e.value.to_text(e.def, None, false))
            .and_then(|t| parse_size(&t))
            .unwrap_or((80, 24))
    }

    /// `ignore_client_size` (resize.c:87-113): no session, exiting, a
    /// `CLIENT_IGNORESIZE` client while an attached client is without it,
    /// or a control client that has not given a size
    /// (`refresh-client -C`).
    fn ignore_client_size(&self, c: &Client) -> bool {
        if c.attached.is_none() || c.closing {
            return true;
        }
        if c.flags.ignore_size
            && self
                .clients
                .values()
                .any(|l| l.attached.is_some() && !l.closing && !l.flags.ignore_size)
        {
            return true;
        }
        c.control.is_some() && !c.flags.size_changed && !c.flags.window_size_changed
    }

    fn session_has(&self, s: Option<SessionId>, w: WindowId) -> bool {
        s.and_then(|s| self.model.sessions.get(&s))
            .is_some_and(|s| s.windows.values().any(|x| *x == w))
    }

    fn skip_client(&self, c: &Client, among: &Among) -> bool {
        match *among {
            Among::Default {
                window: Some(w), ..
            } => !self.session_has(c.attached, w),
            Among::Default { session, .. } => c.attached != Some(session),
            Among::Window { window, current } => {
                let Some(cw) = c.attached.and_then(|s| self.model.current_of(s)) else {
                    return true;
                };
                if current {
                    cw.1 != window
                } else {
                    !self.session_has(c.attached, window)
                }
            }
        }
    }

    /// The client's terminal less its status lines (resize.c:194-198),
    /// and less `frame` cells each way for the focus ring. A control
    /// client has the size `refresh-client -C` gave it (80x24 before
    /// that), or for window `w` the one it gave that window
    /// (resize.c:189-195).
    fn client_size(&self, c: &Client, frame: u32, w: Option<WindowId>) -> Option<(u32, u32)> {
        let (cols, rows) = if let Some(cs) = &c.control {
            w.and_then(|w| cs.window_size(w))
                .unwrap_or_else(|| cs.size())
        } else {
            let tty = c.hello.as_ref()?.tty.as_ref()?;
            (tty.cols, tty.rows.saturating_sub(self.status_line_size(c)))
        };
        Some((
            cols.saturating_sub(frame).max(1),
            rows.saturating_sub(frame).max(1),
        ))
    }

    /// D23: under `vtmux-focus-ring` (seen from `tree`) a window fitted to
    /// a client is 2 cells smaller each way, for the frame drawn round it.
    /// Manual and `default-size` sizes are the window's own and keep it.
    fn ring_frame(&self, tree: TreeId) -> u32 {
        2 * u32::from(self.model.number_option(tree, "vtmux-focus-ring") != 0)
    }

    /// `clients_with_window` (resize.c:116-129), counting to two.
    fn clients_with_window(&self, w: WindowId) -> usize {
        self.clients
            .values()
            .filter(|c| !self.ignore_client_size(c) && self.session_has(c.attached, w))
            .take(2)
            .count()
    }

    /// `clients_calculate_size` (resize.c:131-276): `given` always counts.
    /// Under `latest` with more than one client on the window only the
    /// window's latest client counts; under `manual` the window's own
    /// manual size is the answer.
    fn clients_calculate_size(
        &self,
        policy: SizePolicy,
        given: Option<ClientId>,
        among: &Among,
        w: Option<WindowId>,
        frame: u32,
    ) -> Option<(u32, u32)> {
        let window = w.and_then(|w| self.model.windows.get(&w));
        if policy == SizePolicy::Manual {
            return window.map(|w| w.manual_size);
        }
        let only = match (policy, w, window) {
            (SizePolicy::Latest, Some(w), Some(window)) if self.clients_with_window(w) > 1 => {
                Some(window.latest)
            }
            _ => None,
        };
        let mut size: Option<(u32, u32)> = None;
        for (&id, c) in &self.clients {
            if Some(id) != given && (self.ignore_client_size(c) || self.skip_client(c, among)) {
                continue;
            }
            if only.is_some_and(|latest| latest != Some(id)) {
                continue;
            }
            let Some((cx, cy)) = self.client_size(c, frame, w) else {
                continue;
            };
            size = Some(match (size, policy) {
                (None, _) => (cx, cy),
                (Some((sx, sy)), SizePolicy::Largest) => (sx.max(cx), sy.max(cy)),
                (Some((sx, sy)), _) => (sx.min(cx), sy.min(cy)),
            });
        }
        // resize.c:225-252: no size is larger than a per-window size a
        // control client gave this window.
        let (Some(w), Some(mut size)) = (w, size) else {
            return size;
        };
        for (&id, c) in &self.clients {
            if Some(id) != given && (self.ignore_client_size(c) || self.skip_client(c, among)) {
                continue;
            }
            if !c.flags.window_size_changed {
                continue;
            }
            let Some((cx, cy)) = c.control.as_ref().and_then(|cs| cs.window_size(w)) else {
                continue;
            };
            if cx != 0 {
                size.0 = size.0.min(cx);
            }
            if cy != 0 {
                size.1 = size.1.min(cy);
            }
        }
        Some(size)
    }

    /// `default_window_size` (resize.c:291-343): the size for a new window
    /// of session `s` (or for resize-window -a/-A on `w`). `policy` `None`
    /// is the global `window-size`. Under `latest` a given terminal client
    /// with a session decides; otherwise the clients are measured, and
    /// failing that the session's `default-size`.
    #[must_use]
    pub fn default_window_size(
        &self,
        given: Option<ClientId>,
        s: SessionId,
        w: Option<WindowId>,
        policy: Option<SizePolicy>,
    ) -> (u32, u32) {
        let policy = policy.unwrap_or_else(|| {
            SizePolicy::from_choice(
                self.model
                    .number_option(TreeId::GlobalWindow, "window-size"),
            )
        });
        // A new window has only the global window options yet.
        let frame = self.ring_frame(w.map_or(TreeId::GlobalWindow, TreeId::Window));
        let given = given.and_then(|id| Some((id, self.clients.get(&id)?)));
        if policy == SizePolicy::Latest
            && let Some((_, c)) = given
            && !self.ignore_client_size(c)
            && let Some(size) = self.client_size(c, frame, w)
        {
            return clamp_size(size);
        }
        // A control client creating the window does not size it.
        let given = given.filter(|(_, c)| c.control.is_none()).map(|(id, _)| id);
        let among = Among::Default {
            session: s,
            window: w,
        };
        let size = self
            .clients_calculate_size(policy, given, &among, w, frame)
            .unwrap_or_else(|| self.default_size(TreeId::Session(s)));
        clamp_size(size)
    }

    /// `recalculate_size` (resize.c:345-424): work out window `w`'s size
    /// from its clients (inside the focus ring, D23). Nothing happens when no client gives one or the
    /// size is the one already pending (else the window's own) unless
    /// `now`. `now` or `manual` resize at once; otherwise the size waits
    /// for [`Self::check_window_resizes`].
    pub fn recalculate_size(&mut self, w: WindowId, now: bool) {
        if !self.model.windows.contains_key(&w) {
            return;
        }
        let policy = self.window_policy(w);
        let current = self
            .model
            .number_option(TreeId::Window(w), "aggressive-resize")
            != 0;
        let among = Among::Window { window: w, current };
        let frame = self.ring_frame(TreeId::Window(w));
        let Some(size) = self.clients_calculate_size(policy, None, &among, Some(w), frame) else {
            return;
        };
        let window = &self.model.windows[&w];
        if !now && window.pending_size.unwrap_or((window.sx(), window.sy())) == size {
            return;
        }
        if now || policy == SizePolicy::Manual {
            self.model.resize_window(w, size.0, size.1);
        } else if let Some(window) = self.model.windows.get_mut(&w) {
            window.pending_size = Some(size);
        }
    }

    /// `recalculate_sizes` (resize.c:426-466): [`Self::recalculate_size`]
    /// for every window.
    pub fn recalculate_sizes(&mut self) {
        self.recalculate_sizes_now(false);
    }

    /// `recalculate_sizes_now`: with `now`, windows are resized at once
    /// rather than at the next check.
    pub fn recalculate_sizes_now(&mut self, now: bool) {
        let windows: Vec<_> = self.model.windows.keys().copied().collect();
        for w in windows {
            self.recalculate_size(w, now);
        }
    }

    /// `server_client_check_window_resize` (server-client.c:1953-1970),
    /// run every loop before drawing: a window with a pending size takes
    /// it once it is the current window of a session with a client
    /// attached. A background window keeps its size (and its panes get no
    /// SIGWINCH) until then.
    pub(crate) fn check_window_resizes(&mut self) {
        let current: Vec<WindowId> = self
            .clients
            .values()
            .filter_map(|c| c.attached)
            .filter_map(|s| self.model.current_of(s))
            .map(|(_, w, _)| w)
            .collect();
        for w in current {
            if let Some((sx, sy)) = self.model.windows.get(&w).and_then(|x| x.pending_size) {
                self.model.resize_window(w, sx, sy);
            }
        }
    }

    /// `s->curw->window->latest = c` (server-client.c:465,
    /// cmd-select-window.c:145-146, cmd-new-window.c:123-124): client `id`
    /// becomes the latest of session `s`'s current window.
    pub fn set_latest(&mut self, s: SessionId, id: ClientId) {
        if let Some((_, w, _)) = self.model.current_of(s)
            && let Some(window) = self.model.windows.get_mut(&w)
        {
            window.latest = Some(id);
        }
    }

    /// `server_client_update_latest` (server-client.c:1371-1390): a key or
    /// a resize from client `id` makes it the latest of the window it
    /// looks at, which is sized again under `window-size latest`.
    pub(crate) fn update_latest(&mut self, id: ClientId) {
        let Some(w) = self.client_window(id) else {
            return;
        };
        let window = self.model.windows.get_mut(&w).expect("current window");
        if window.latest == Some(id) {
            return;
        }
        window.latest = Some(id);
        if self.window_policy(w) == SizePolicy::Latest {
            self.recalculate_size(w, false);
        }
    }

    /// `server_client_attached_lost` (server-client.c:361-391): every
    /// window client `id` was the latest of passes to the most recently
    /// active client that has it as its current window, if any; with none
    /// it keeps pointing at the lost client, so no client wins `latest`
    /// while several see it.
    pub(crate) fn attached_lost(&mut self, id: ClientId) {
        let windows: Vec<WindowId> = self
            .model
            .windows
            .values()
            .filter(|w| w.latest == Some(id))
            .map(|w| w.id)
            .collect();
        for w in windows {
            let mut found: Option<(ClientId, u64)> = None;
            for (&cid, c) in &self.clients {
                if cid == id {
                    continue;
                }
                let Some((_, cw, _)) = c.attached.and_then(|s| self.model.current_of(s)) else {
                    continue;
                };
                // timercmp(>): the first of equals stays.
                if cw == w && found.is_none_or(|(_, a)| c.activity > a) {
                    found = Some((cid, c.activity));
                }
            }
            if let Some((cid, _)) = found {
                self.update_latest(cid);
            }
        }
    }

    /// `server_client_check_pane_resize` for every pane (server-client.c:
    /// 1932-1937), after the clients are drawn.
    pub(crate) fn send_pane_resizes(&mut self) {
        let now = Instant::now();
        for pane in self.model.panes.values_mut() {
            pane.send_resizes(now);
        }
    }

    /// The soonest a pane's queued size is due.
    pub(crate) fn pane_resize_deadline(&self) -> Option<Instant> {
        self.model
            .panes
            .values()
            .filter_map(crate::model::Pane::resize_deadline)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_choices_round_trip() {
        for p in [
            SizePolicy::Largest,
            SizePolicy::Smallest,
            SizePolicy::Manual,
            SizePolicy::Latest,
        ] {
            assert_eq!(
                SizePolicy::from_choice(i64::try_from(p.choice()).unwrap()),
                p
            );
        }
    }

    #[test]
    fn default_size_parses_like_sscanf() {
        assert_eq!(parse_size("80x24"), Some((80, 24)));
        assert_eq!(parse_size("x24"), None);
        assert_eq!(clamp_size((0, 20_000)), (1, WINDOW_MAXIMUM));
    }
}
