//! `join-pane` and `move-pane` (cmd-join-pane.c:420-539): split the target
//! pane and move the source pane into the new cell, from any window of any
//! session; a window left without panes is killed. The floating-pane forms
//! of move-pane (`-P -z -X -Y -U -D -L -R`, `-M`, tiling a floating pane by
//! joining it to itself) are not done yet, apart from the errors that need
//! no floating panes.

use vtmux_core::cmd::{Args, CmdError, NumError};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, WindowId};
use vtmux_core::layout::SplitRequest;

use super::split_window::percentage_expanded;
use crate::model::relabel_cell;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
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
    if is_move {
        if args.flag('M') {
            // No mouse event: nothing to do (cmd-join-pane.c:286-287).
            return Ok(());
        }
        if ['P', 'z', 'X', 'Y', 'U', 'D', 'L', 'R']
            .iter()
            .any(|f| args.flag(*f))
        {
            if !server.model.windows[&dst_w].is_floating(dst_wp) {
                return Err(EdgeError::Message("pane is not floating".into()).into());
            }
            return Err(EdgeError::NotImplemented("move-pane floating".into()).into());
        }
    }
    let (Some(src_w), Some(src_wp)) = (source.window, source.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    server.model.unzoom(dst_w);
    server.model.unzoom(src_w);
    if src_wp == dst_wp {
        if server.model.windows[&src_w].is_floating(src_wp) {
            return Err(EdgeError::NotImplemented("join-pane tile".into()).into());
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
    let src = m.windows.get_mut(&src_w).expect("source window");
    src.layout.close_pane(src_wp);
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
