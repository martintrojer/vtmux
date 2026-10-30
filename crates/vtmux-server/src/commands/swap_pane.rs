//! `swap-pane` (cmd-swap-pane.c:57-210): exchange two panes' places (pane
//! order, z-order, layout cells and windows), or swap the target with the
//! next (`-D`) or previous (`-U`) tiled pane. `-d` keeps the active panes,
//! `-Z` keeps a zoomed window zoomed. Modal panes are not modelled.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::PaneId;

use crate::model::relabel_cell;
use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, args: &Args, source: Found, found: Found) -> Result<(), CmdError> {
    let (Some(dst_w), Some(dst_wp)) = (found.window, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let (Some(mut src_w), Some(mut src_wp)) = (source.window, source.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let zoom = args.flag('Z');
    let m = &mut server.model;
    let dst_zoomed = m.push_zoom(dst_w, false, zoom);
    let (down, up) = (args.flag('D'), args.flag('U'));
    if down || up {
        if m.windows[&dst_w].is_floating(dst_wp) {
            m.pop_zoom(dst_w, dst_zoomed);
            let which = if down { "down" } else { "up" };
            return Err(EdgeError::Message(format!("cannot swap {which} on floating pane")).into());
        }
        src_w = dst_w;
        let window = &m.windows[&dst_w];
        let tiled = |p: &&PaneId| !window.is_floating(**p);
        let panes = &window.panes.panes;
        let at = panes.iter().position(|p| *p == dst_wp).unwrap_or(0);
        // cmd_swap_pane_next/prev_tiled_pane, wrapping to the first/last.
        let pick = if down {
            panes[at + 1..]
                .iter()
                .find(tiled)
                .or_else(|| panes.iter().find(tiled))
        } else {
            panes[..at]
                .iter()
                .rev()
                .find(tiled)
                .or_else(|| panes.iter().rev().find(tiled))
        };
        if let Some(p) = pick {
            src_wp = *p;
        }
    }
    let same = src_w == dst_w;
    let src_zoomed = if same {
        dst_zoomed
    } else {
        m.push_zoom(src_w, false, zoom)
    };
    if src_wp != dst_wp {
        swap(server, args.flag('d'), (src_w, src_wp), (dst_w, dst_wp));
    }
    server.model.pop_zoom(src_w, src_zoomed);
    if !same {
        server.model.pop_zoom(dst_w, dst_zoomed);
    }
    Ok(())
}

fn swap(
    server: &mut Server,
    detached: bool,
    (src_w, src_wp): (vtmux_core::ids::WindowId, PaneId),
    (dst_w, dst_wp): (vtmux_core::ids::WindowId, PaneId),
) {
    let m = &mut server.model;
    let same = src_w == dst_w;
    // The pane lists and z-orders: each pane takes the other's place.
    let replace = |list: &mut Vec<PaneId>, from: PaneId, to: PaneId| {
        if let Some(i) = list.iter().position(|p| *p == from) {
            list[i] = to;
        }
    };
    if same {
        let w = m.windows.get_mut(&src_w).expect("source window");
        for list in [&mut w.panes.panes, &mut w.panes.z_order] {
            let a = list.iter().position(|p| *p == src_wp);
            let b = list.iter().position(|p| *p == dst_wp);
            if let (Some(a), Some(b)) = (a, b) {
                list.swap(a, b);
            }
        }
        // Swap the cells through a placeholder no pane has.
        let hole = PaneId(u32::MAX);
        relabel_cell(&mut w.layout, src_wp, hole);
        relabel_cell(&mut w.layout, dst_wp, src_wp);
        relabel_cell(&mut w.layout, hole, dst_wp);
    } else {
        let s = m.windows.get_mut(&src_w).expect("source window");
        replace(&mut s.panes.panes, src_wp, dst_wp);
        replace(&mut s.panes.z_order, src_wp, dst_wp);
        relabel_cell(&mut s.layout, src_wp, dst_wp);
        let d = m.windows.get_mut(&dst_w).expect("target window");
        replace(&mut d.panes.panes, dst_wp, src_wp);
        replace(&mut d.panes.z_order, dst_wp, src_wp);
        relabel_cell(&mut d.layout, dst_wp, src_wp);
        for (p, w) in [(src_wp, dst_w), (dst_wp, src_w)] {
            if let Some(pane) = m.panes.get_mut(&p) {
                pane.window = w;
            }
            m.options.move_pane(p, w);
        }
    }

    // window_set_active_pane: a cross-window swap pushes a pane that has
    // left the window onto its last-used stack; that is cleaned below.
    if detached {
        if m.windows[&src_w].panes.active == Some(src_wp) {
            m.set_active(src_w, dst_wp);
        }
        if m.windows[&dst_w].panes.active == Some(dst_wp) {
            m.set_active(dst_w, src_wp);
        }
    } else if same {
        m.set_active(src_w, dst_wp);
    } else {
        m.set_active(src_w, dst_wp);
        m.set_active(dst_w, src_wp);
    }
    if !same {
        // window_pane_stack_remove: neither keeps the other window's pane
        // in its last-used stack.
        let s = m.windows.get_mut(&src_w).expect("source window");
        s.panes.last.retain(|p| *p != src_wp);
        let d = m.windows.get_mut(&dst_w).expect("target window");
        d.panes.last.retain(|p| *p != dst_wp);
        m.fix_panes(src_w);
    }
    m.fix_panes(dst_w);
}
