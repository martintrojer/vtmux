//! `join-pane` and `move-pane` (cmd-join-pane.c:420-539): split the target
//! pane and move the source pane into the new cell, from any window of any
//! session; a window left without panes is killed. Joining a floating pane
//! to itself tiles it. move-pane also moves a floating pane: `-P` to a named
//! place or through the z-order, `-z` to a z-index, `-X -Y` to a position,
//! `-U -D -L -R` by a count, `-M` with the mouse.

use vtmux_core::cmd::{Args, CmdError, NumError};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, WindowId};
use vtmux_core::layout::SplitRequest;

use super::Item;
use super::split_window::percentage_expanded;
use crate::model::relabel_cell;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &Item,
    args: &Args,
    source: Found,
    found: Found,
    is_move: bool,
) -> Result<(), CmdError> {
    let (Some(dst_s), Some(dst_idx), Some(dst_w), Some(dst_wp)) =
        (found.session, found.index, found.window, found.pane)
    else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    if is_move && let Some(result) = move_floating(server, item, args, found) {
        return result;
    }
    let (Some(src_w), Some(src_wp)) = (source.window, source.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    // cmd-join-pane.c:468-471.
    let is_modal = |w, p| server.model.windows[&w].panes.modal == Some(p);
    if is_modal(src_w, src_wp) || is_modal(dst_w, dst_wp) {
        return Err(EdgeError::Message("pane is modal".into()).into());
    }
    server.model.unzoom(dst_w);
    server.model.unzoom(src_w);
    if src_wp == dst_wp {
        if server.model.windows[&src_w].is_floating(src_wp) {
            return tile(server, args, src_w, src_wp);
        }
        return Err(EdgeError::Message("source and target panes must be different".into()).into());
    }
    let horizontal = args.flag('h');
    let before = args.flag('b');
    let full = args.flag('f');

    let size = tiled_size(server, args, found, (dst_w, dst_wp))?;
    let cause = |c: String| EdgeError::Message(format!("size or position {c}"));
    let req = SplitRequest {
        horizontal,
        before,
        full,
        size,
    };
    // layout_split_pane makes an empty cell (a placeholder no pane has),
    // layout_close_pane removes the source's old cell (which may resize the
    // new one), then layout_assign_pane fills it.
    let hole = PaneId(u32::MAX);
    let m = &mut server.model;
    let dst = m.windows.get_mut(&dst_w).expect("target window");
    dst.layout
        .split(dst_wp, hole, &req)
        .map_err(|e| cause(e.to_string()))?;
    m.windows
        .get_mut(&src_w)
        .expect("source window")
        .layout
        .close_pane(src_wp);
    // layout_close_pane fires window-layout-changed (layout.c:1548).
    m.layout_changed(src_w);
    if src_w != dst_w {
        // window_lost_pane: a moved pane loses its mark.
        m.clear_mark_of(src_wp);
    }
    let src = m.windows.get_mut(&src_w).expect("source window");
    if src_w == dst_w {
        src.panes.panes.retain(|p| *p != src_wp);
        src.panes.z_order.retain(|p| *p != src_wp);
    } else {
        // window_lost_pane: a new active pane, out of the last-used stack.
        src.panes.remove(src_wp);
    }
    let w = m.windows.get_mut(&dst_w).expect("target window");
    relabel_cell(&mut w.layout, hole, src_wp);
    // TAILQ_INSERT_BEFORE/AFTER dst_wp in both the pane list and z-order
    // (not window_add_pane: -f does not move it to an end).
    for list in [&mut w.panes.panes, &mut w.panes.z_order] {
        let at = list.iter().position(|p| *p == dst_wp).unwrap_or(0);
        list.insert(if before { at } else { at + 1 }, src_wp);
    }
    if let Some(pane) = m.panes.get_mut(&src_wp) {
        pane.window = dst_w;
    }
    m.options.move_pane(src_wp, dst_w);
    m.fix_panes(dst_w);
    if src_w != dst_w {
        m.fix_panes(src_w);
    }

    if !args.flag('d') {
        server.model.set_active(dst_w, src_wp);
        server.model.set_current(dst_s, dst_idx);
    }
    // cmd-join-pane.c:531-536.
    server
        .model
        .fire_pane_moved(src_wp, (src_w, source.index), (dst_w, Some(dst_idx)));
    let emptied = server
        .model
        .windows
        .get(&src_w)
        .is_some_and(|w| w.panes.panes.is_empty());
    if emptied {
        // server_kill_window(src_w, 1). The window has no panes left, so
        // nothing closes.
        server.kill_window(src_w, true);
    } else {
        server.model.layout_changed(src_w);
    }
    server.model.layout_changed(dst_w);
    Ok(())
}

/// The size half of `layout_get_tiled_cell` (layout.c:1640-1695): `-l` or
/// `-p` against the target pane (`-f`: the window), with its errors
/// prefixed `size or position`.
fn tiled_size(
    server: &Server,
    args: &Args,
    found: Found,
    (w, wp): (WindowId, PaneId),
) -> Result<Option<i64>, EdgeError> {
    let cause = |c: String| EdgeError::Message(format!("size or position {c}"));
    let window = &server.model.windows[&w];
    if window.is_floating(wp) {
        return Err(cause("can't split a floating pane".into()));
    }
    let r = if args.flag('f') {
        (window.sx(), window.sy())
    } else {
        let rect = server.model.panes[&wp].rect;
        (rect.sx, rect.sy)
    };
    let curval = i64::from(if args.flag('h') { r.0 } else { r.1 });
    let geometry = |e: NumError| cause(format!("invalid tiled geometry {e}"));
    if let Some(l) = args.value('l') {
        return percentage_expanded(server, found, l, 0, i64::from(i32::MAX), curval)
            .map(Some)
            .map_err(geometry);
    }
    if args.flag('p') {
        let p = super::number_expanded(server, found, args.value('p'), 0, 100).map_err(geometry)?;
        return Ok(Some(curval * p / 100));
    }
    Ok(None)
}

/// move-pane's forms for a floating pane (cmd-join-pane.c:441-459): `-M`
/// starts a mouse move, `-P -z -X -Y -U -D -L -R` place it. `None` when
/// none of them is given, for a join.
fn move_floating(
    server: &mut Server,
    item: &Item,
    args: &Args,
    found: Found,
) -> Option<Result<(), CmdError>> {
    let (w, wp) = (found.window?, found.pane?);
    if args.flag('M') {
        // cmd_join_pane_mouse_update: nothing without a mouse event.
        if let Some(ev) = item.mouse.clone() {
            server.move_floating_start(item.client, &ev);
        }
        return Some(Ok(()));
    }
    if !['P', 'z', 'X', 'Y', 'U', 'D', 'L', 'R']
        .iter()
        .any(|f| args.flag(*f))
    {
        return None;
    }
    if !server.model.windows[&w].is_floating_now(wp) {
        return Some(Err(EdgeError::Message("pane is not floating".into()).into()));
    }
    Some(if let Some(p) = args.value('P') {
        place(server, w, wp, p)
    } else if let Some(z) = args.value('z') {
        zindex(server, w, wp, z)
    } else {
        move_by(server, args, found, w, wp)
    })
}

/// `cmd_join_pane_place` (cmd-join-pane.c:66-196): a named place in the
/// window (inside the border), or a move through the z-order.
fn place(server: &mut Server, w: WindowId, wp: PaneId, position: &str) -> Result<(), CmdError> {
    let border = i32::from(server.model.pane_has_border(w, wp));
    let window = server.model.windows.get_mut(&w).expect("target window");
    let (wx, wy) = (window.sx().cast_signed(), window.sy().cast_signed());
    let Some(g) = window.layout.floating_cell_mut(wp).copied() else {
        return Ok(());
    };
    let (px, py) = (g.sx.cast_signed(), g.sy.cast_signed());
    let centre_x = (wx - px) / 2;
    let centre_y = (wy - py) / 2;
    let (left, right) = (border, wx - px - border);
    let (top, bottom) = (border, wy - py - border);
    let at = match position {
        "top-left" => Some((left, top)),
        "top-centre" | "top-center" => Some((centre_x, top)),
        "top-right" => Some((right, top)),
        "centre-left" | "center-left" => Some((left, centre_y)),
        "centre" | "center" => Some((centre_x, centre_y)),
        "centre-right" | "center-right" => Some((right, centre_y)),
        "bottom-left" => Some((left, bottom)),
        "bottom-centre" | "bottom-center" => Some((centre_x, bottom)),
        "bottom-right" => Some((right, bottom)),
        "top-left-centre" | "top-left-center" => Some((wx / 4 - px / 2, wy / 4 - py / 2)),
        "top-right-centre" | "top-right-center" => Some(((3 * wx) / 4 - px / 2, wy / 4 - py / 2)),
        "bottom-left-centre" | "bottom-left-center" => {
            Some((wx / 4 - px / 2, (3 * wy) / 4 - py / 2))
        }
        "bottom-right-centre" | "bottom-right-center" => {
            Some(((3 * wx) / 4 - px / 2, (3 * wy) / 4 - py / 2))
        }
        "front" | "back" | "forward" | "backward" | "forward-loop" | "backward-loop" => {
            restack(window, wp, position);
            None
        }
        _ => {
            return Err(EdgeError::Message(format!("unknown position: {position}")).into());
        }
    };
    if let Some((x, y)) = at
        && (x, y) != (g.xoff, g.yoff)
    {
        let cell = window.layout.floating_cell_mut(wp).expect("floating");
        cell.xoff = x;
        cell.yoff = y;
        server.model.fix_panes(w);
    }
    server.model.layout_changed(w);
    server.model.redraw(crate::model::Redraw::Window(w));
    Ok(())
}

/// The z-order moves of `move-pane -P` (cmd-join-pane.c:131-188):
/// `w->z_index` is topmost first, floating panes before tiled ones.
fn restack(window: &mut crate::model::Window, wp: PaneId, how: &str) {
    let floating = |window: &crate::model::Window, p: PaneId| window.is_floating(p);
    let z = window.panes.z_order.clone();
    let Some(at) = z.iter().position(|&p| p == wp) else {
        return;
    };
    // Panes with a cell (hidden ones under a zoom have none).
    let has_cell = |p: PaneId| window.layout.root.find_pane(p).is_some();
    let first_tiled = |list: &[PaneId], window: &crate::model::Window| {
        list.iter().position(|&p| !floating(window, p))
    };
    let mut list = z.clone();
    match how {
        "front" => {
            list.remove(at);
            list.insert(0, wp);
        }
        "back" => {
            list.remove(at);
            let i = first_tiled(&list, window).unwrap_or(list.len());
            list.insert(i, wp);
        }
        "forward" | "forward-loop" => {
            let prev = z[..at].iter().rposition(|&p| has_cell(p));
            match prev {
                Some(i) => {
                    list.remove(at);
                    list.insert(i, wp);
                }
                None if how == "forward-loop" => {
                    list.remove(at);
                    let i = first_tiled(&list, window).unwrap_or(list.len());
                    list.insert(i, wp);
                }
                None => {}
            }
        }
        "backward" | "backward-loop" => {
            let next = z[at + 1..]
                .iter()
                .position(|&p| has_cell(p))
                .map(|i| at + 1 + i);
            match next {
                Some(i) if window.is_floating_now(z[i]) => {
                    let other = z[i];
                    list.remove(at);
                    let j = list.iter().position(|&p| p == other).expect("in list");
                    list.insert(j + 1, wp);
                }
                _ if how == "backward-loop" => {
                    list.remove(at);
                    list.insert(0, wp);
                }
                _ => {}
            }
        }
        _ => {}
    }
    window.panes.z_order = list;
}

/// `cmd_join_pane_zindex` (cmd-join-pane.c:343-378): the pane goes before
/// the floating pane that has z-index `z` (counting those with a cell).
fn zindex(server: &mut Server, w: WindowId, wp: PaneId, s: &str) -> Result<(), CmdError> {
    let z = vtmux_core::cmd::strtonum(s, 0, i64::from(u32::MAX))
        .map_err(|e| EdgeError::Message(format!("z-index {e}")))?;
    let window = server.model.windows.get_mut(&w).expect("target window");
    window.panes.z_order.retain(|&p| p != wp);
    let mut n = 0;
    let mut at = window.panes.z_order.len();
    for (i, &p) in window.panes.z_order.iter().enumerate() {
        if !window.is_floating(p) {
            at = i;
            break;
        }
        if window.layout.root.find_pane(p).is_none() {
            continue;
        }
        if n >= z {
            at = i;
            break;
        }
        n += 1;
    }
    window.panes.z_order.insert(at, wp);
    server.model.layout_changed(w);
    server.model.redraw(crate::model::Redraw::Window(w));
    Ok(())
}

/// `cmd_join_pane_move` (cmd-join-pane.c:198-272): `-X -Y` set the
/// position (inside the border), `-U -D -L -R` move by a count (1 without
/// one).
fn move_by(
    server: &mut Server,
    args: &Args,
    found: Found,
    w: WindowId,
    wp: PaneId,
) -> Result<(), CmdError> {
    let border = i64::from(server.model.pane_has_border(w, wp));
    let window = &server.model.windows[&w];
    let (wsx, wsy) = (i64::from(window.sx()), i64::from(window.sy()));
    let Some(g) = server
        .model
        .windows
        .get_mut(&w)
        .and_then(|x| x.layout.floating_cell_mut(wp))
        .copied()
    else {
        return Ok(());
    };
    let (mut xoff, mut yoff) = (i64::from(g.xoff), i64::from(g.yoff));
    let position = |e: NumError| EdgeError::Message(format!("position {e}"));
    if let Some(v) = args.value('X') {
        xoff = percentage_expanded(server, found, v, -wsx, wsx, wsx).map_err(position)? + border;
    }
    if let Some(v) = args.value('Y') {
        yoff = percentage_expanded(server, found, v, -wsy, wsy, wsy).map_err(position)? + border;
    }
    for flag in ['U', 'D', 'L', 'R'] {
        if !args.flag(flag) {
            continue;
        }
        let v = args.value(flag).unwrap_or("1");
        let adjust = vtmux_core::cmd::strtonum(v, i64::from(i32::MIN), i64::from(i32::MAX))
            .map_err(|e| EdgeError::Message(format!("offset {e}")))?;
        match flag {
            'U' => yoff -= adjust,
            'D' => yoff += adjust,
            'L' => xoff -= adjust,
            _ => xoff += adjust,
        }
    }
    // The C stores these in ints, which wrap.
    #[expect(clippy::cast_possible_truncation, reason = "int arithmetic as the C's")]
    let (xoff, yoff) = (xoff as i32, yoff as i32);
    if (xoff, yoff) != (g.xoff, g.yoff) {
        let window = server.model.windows.get_mut(&w).expect("target window");
        let cell = window.layout.floating_cell_mut(wp).expect("floating");
        cell.xoff = xoff;
        cell.yoff = yoff;
        server.model.fix_panes(w);
        server.model.layout_changed(w);
        server.model.redraw(crate::model::Redraw::Window(w));
    }
    Ok(())
}

/// `cmd_join_pane_tile` (cmd-join-pane.c:380-418): the floating pane goes
/// into the tiled layout next to its nearest neighbour and to the back of
/// the z-order.
fn tile(server: &mut Server, args: &Args, w: WindowId, wp: PaneId) -> Result<(), CmdError> {
    if server.model.windows[&w].zoomed() {
        return Err(EdgeError::Message("can't tile a pane while window is zoomed".into()).into());
    }
    let window = server.model.windows.get_mut(&w).expect("target window");
    window
        .layout
        .tile_pane(wp)
        .map_err(|_| EdgeError::Message("no space for a new pane".into()))?;
    window.panes.z_order.retain(|&p| p != wp);
    window.panes.z_order.push(wp);
    if !args.flag('d') {
        server.model.set_active(w, wp);
    }
    server.model.fix_panes(w);
    server.model.layout_changed(w);
    server.model.redraw(crate::model::Redraw::Window(w));
    Ok(())
}
