//! A client's view of a window larger than its terminal: which part of the
//! window it shows (tty.c `tty_window_offset1`), following the active
//! pane's cursor unless the client panned it by hand (`refresh-client
//! -U/-D/-L/-R`, cmd-refresh-client.c:171-233).
//!
//! The view is the client's, worked out again whenever its frame is drawn
//! and when it pans (tmux's `tty_update_client_offset`); the scene, the
//! cursor, mouse positions, menus, popups and the `window_offset_*`
//! formats all read the one stored in the client.

use vtmux_core::ids::WindowId;

use crate::server::{ClientId, Server};

/// The part of the current window a client shows (`tty->oox`, `ooy`,
/// `osx`, `osy`, `oflag`): from window column `ox`, row `oy`, `sx` x `sy`
/// cells. `bigger` is set when the window does not fit the client's area
/// (the terminal less its status lines); otherwise the view is the whole
/// window from its origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub ox: u32,
    pub oy: u32,
    pub sx: u32,
    pub sy: u32,
    pub bigger: bool,
}

/// A view panned by hand (`c->pan_window`, `pan_ox`, `pan_oy`): it holds
/// while the client shows `window` and the window does not fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pan {
    pub window: WindowId,
    pub ox: u32,
    pub oy: u32,
}

/// A `refresh-client` pan by `n` cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanBy {
    Left(u32),
    Right(u32),
    Up(u32),
    Down(u32),
}

/// What [`follow`] needs to know about the client's current window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub window: WindowId,
    /// `w->sx`, `w->sy`.
    pub size: (u32, u32),
    /// The active pane's cursor in window coordinates, when its screen
    /// shows one (`MODE_CURSOR`).
    pub cursor: Option<(i64, i64)>,
}

/// `tty_window_offset1` (tty.c:974-1036) for a client whose terminal has
/// `area` cells for the window (`tty->sx`, `tty->sy` less the status
/// lines). A pan of another window, or of a window that now fits, is
/// dropped; a pan past the window's edge is pulled back to it.
#[must_use]
pub fn follow(area: (u32, u32), shown: Shown, pan: &mut Option<Pan>) -> View {
    let (wsx, wsy) = shown.size;
    let (sx, sy) = area;
    if sx >= wsx && sy >= wsy {
        *pan = None;
        return View {
            ox: 0,
            oy: 0,
            sx: wsx,
            sy: wsy,
            bigger: false,
        };
    }
    if let Some(p) = pan.as_mut().filter(|p| p.window == shown.window) {
        p.ox = clamp_pan(p.ox, sx, wsx);
        p.oy = clamp_pan(p.oy, sy, wsy);
        return View {
            ox: p.ox,
            oy: p.oy,
            sx,
            sy,
            bigger: true,
        };
    }
    *pan = None;
    // Without MODE_CURSOR the view is the window's top left. tmux's
    // unsigned arithmetic needs the window wider (taller) than the view
    // on an axis where the cursor is past the view; a cursor left of or
    // above the window (a floating pane moved off it) is at its edge.
    let (ox, oy) = shown.cursor.map_or((0, 0), |(cx, cy)| {
        (
            follow_axis(cx, sx, wsx, |cx, sx| cx - sx / 2),
            follow_axis(cy, sy, wsy, |cy, sy| cy - sy + 1),
        )
    });
    View {
        ox,
        oy,
        sx,
        sy,
        bigger: true,
    }
}

/// `tty_window_offset1`'s pan check on one axis: no offset when the view
/// covers the window, else at most the window's size less the view's.
fn clamp_pan(o: u32, view: u32, window: u32) -> u32 {
    if view >= window {
        0
    } else if o + view > window {
        window - view
    } else {
        o
    }
}

/// `tty_window_offset1`'s cursor rule on one axis: the origin while the
/// cursor is in the first view's worth of cells, the far edge once it is
/// in the last, else `centre`.
fn follow_axis(c: i64, view: u32, window: u32, centre: impl Fn(u32, u32) -> u32) -> u32 {
    let Ok(c) = u32::try_from(c) else {
        return 0;
    };
    if c < view || view >= window {
        0
    } else if c > window - view {
        window - view
    } else {
        centre(c, view)
    }
}

/// `cmd_refresh_client_exec`'s pan (cmd-refresh-client.c:203-230): from
/// the shown view when the pan starts (or is of another window), by `by`
/// cells, right and down at most to the window's size less the view's.
#[must_use]
pub fn pan(view: View, window: (WindowId, u32, u32), by: PanBy, pan: Option<Pan>) -> Pan {
    let (w, wsx, wsy) = window;
    let mut p = pan.filter(|p| p.window == w).unwrap_or(Pan {
        window: w,
        ox: view.ox,
        oy: view.oy,
    });
    match by {
        PanBy::Left(n) => p.ox = p.ox.saturating_sub(n),
        PanBy::Right(n) => p.ox = p.ox.saturating_add(n).min(wsx.saturating_sub(view.sx)),
        PanBy::Up(n) => p.oy = p.oy.saturating_sub(n),
        PanBy::Down(n) => p.oy = p.oy.saturating_add(n).min(wsy.saturating_sub(view.sy)),
    }
    p
}

impl Server {
    /// `tty_update_client_offset` (tty.c:1052-1076): client `id`'s view of
    /// its current window worked out again; a change redraws the window
    /// and the status line (`window_offset_x` and `window_bigger` are in
    /// the default `status-right`). Only a terminal client has a view.
    pub(crate) fn update_view(&mut self, id: ClientId) {
        let Some((area, shown)) = self.view_inputs(id) else {
            return;
        };
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let view = follow(area, shown, &mut c.pan);
        if view != c.view {
            c.view = view;
            c.status.request_full_redraw();
        }
    }

    /// [`Self::update_view`] for each of `clients` before their frames:
    /// tmux updates the offsets after the cursor moves
    /// (`screen_write_offset_timer`), on a resize and on a change of window
    /// or pane, so each frame, its status line and the commands after it
    /// see the same view.
    pub(crate) fn update_views(&mut self, clients: impl IntoIterator<Item = ClientId>) {
        for id in clients {
            self.update_view(id);
        }
    }

    /// `refresh-client -U/-D/-L/-R` for client `id`: the view panned and
    /// redrawn. A client without a window does nothing.
    pub(crate) fn pan_view(&mut self, id: ClientId, by: PanBy) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let Some((_, w, _)) = c.attached.and_then(|s| self.model.current_of(s)) else {
            return;
        };
        let Some(win) = self.model.windows.get(&w) else {
            return;
        };
        let p = pan(c.view, (w, win.sx(), win.sy()), by, c.pan);
        if let Some(c) = self.clients.get_mut(&id) {
            c.pan = Some(p);
        }
        self.update_view(id);
    }

    /// `refresh-client -c`: back to following the cursor.
    pub(crate) fn stop_pan(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.pan = None;
        }
        self.update_view(id);
    }

    /// The client's area for the window and what [`follow`] needs about
    /// that window; `None` for a client with no terminal (`~CLIENT_TERMINAL`)
    /// or no session.
    fn view_inputs(&self, id: ClientId) -> Option<((u32, u32), Shown)> {
        let c = self.clients.get(&id)?;
        if c.control.is_some() {
            return None;
        }
        let tty = c.hello.as_ref()?.tty.as_ref()?;
        let s = c.attached?;
        let (_, w, active) = self.model.current_of(s)?;
        let win = self.model.windows.get(&w)?;
        let lines = self.status_line_size(c);
        let area = (tty.cols, tty.rows.saturating_sub(lines));
        let size = (win.sx(), win.sy());
        // The cursor matters only when the window does not fit.
        let cursor = if area.0 >= size.0 && area.1 >= size.1 {
            None
        } else {
            let pane = self.model.panes.get(&active)?;
            let caps = c.caps?;
            self.render_source()
                .screen_cursor(pane, s, caps)
                .map(|(x, y)| {
                    (
                        i64::from(pane.rect.xoff) + i64::from(x),
                        i64::from(pane.rect.yoff) + i64::from(y),
                    )
                })
        };
        Some((
            area,
            Shown {
                window: w,
                size,
                cursor,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: WindowId = WindowId(1);

    fn shown(size: (u32, u32), cursor: Option<(i64, i64)>) -> Shown {
        Shown {
            window: W,
            size,
            cursor,
        }
    }

    #[test]
    fn a_window_that_fits_is_shown_whole_and_drops_a_pan() {
        let mut p = Some(Pan {
            window: W,
            ox: 3,
            oy: 4,
        });
        let v = follow((80, 23), shown((80, 23), Some((79, 22))), &mut p);
        assert_eq!(
            v,
            View {
                ox: 0,
                oy: 0,
                sx: 80,
                sy: 23,
                bigger: false
            }
        );
        assert_eq!(p, None);
    }

    #[test]
    fn the_view_follows_the_cursor() {
        let mut p = None;
        // tty.c:1017-1028: origin, centred, far edge.
        let at = |size, c, p: &mut Option<Pan>| {
            let v = follow((40, 14), shown(size, Some(c)), p);
            (v.ox, v.oy)
        };
        let big = (100, 40);
        assert_eq!(at(big, (39, 13), &mut p), (0, 0));
        assert_eq!(at(big, (50, 20), &mut p), (30, 7));
        assert_eq!(at(big, (99, 39), &mut p), (60, 26));
        // Past the view and within the window's last view's worth: the
        // far edge.
        assert_eq!(at((60, 20), (40, 14), &mut p), (20, 6));
        // An axis that fits stays at 0.
        assert_eq!(at((30, 40), (25, 20), &mut p), (0, 7));
        // Off the window's left or top.
        assert_eq!(at(big, (-3, -1), &mut p), (0, 0));
        // No cursor (MODE_CURSOR off): the top left.
        let v = follow((40, 14), shown(big, None), &mut p);
        assert_eq!((v.ox, v.oy, v.sx, v.sy, v.bigger), (0, 0, 40, 14, true));
    }

    #[test]
    fn a_pan_holds_for_its_window_and_is_clamped() {
        let view = follow((40, 14), shown((60, 20), Some((50, 19))), &mut None);
        assert_eq!((view.ox, view.oy), (20, 6));
        // The pan starts from the view shown.
        let p = pan(view, (W, 60, 20), PanBy::Left(5), None);
        assert_eq!((p.ox, p.oy), (15, 6));
        let p = pan(view, (W, 60, 20), PanBy::Down(100), Some(p));
        assert_eq!((p.ox, p.oy), (15, 6));
        let p = pan(view, (W, 60, 20), PanBy::Up(100), Some(p));
        assert_eq!((p.ox, p.oy), (15, 0));
        let p = pan(view, (W, 60, 20), PanBy::Right(100), Some(p));
        assert_eq!((p.ox, p.oy), (20, 0));
        let mut held = Some(p);
        let v = follow((40, 14), shown((60, 20), Some((0, 0))), &mut held);
        assert_eq!((v.ox, v.oy), (20, 0));
        // The window shrinks: the pan is pulled back inside it and kept so.
        let v = follow((40, 14), shown((50, 20), None), &mut held);
        assert_eq!((v.ox, v.oy), (10, 0));
        assert_eq!(held.map(|p| p.ox), Some(10));
        // Another window: the pan goes, the view follows the cursor.
        let mut other = Some(Pan {
            window: WindowId(2),
            ..p
        });
        let v = follow((40, 14), shown((60, 20), None), &mut other);
        assert_eq!((v.ox, v.oy, other), (0, 0, None));
    }
}
