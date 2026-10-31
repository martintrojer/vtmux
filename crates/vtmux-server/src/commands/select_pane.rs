//! `select-pane` and `last-pane` (cmd-select-pane.c:146-291). Marking
//! (`-m -M`) and the deprecated `-P`/`-g` are not done yet.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, WindowId};

use super::{Item, expand};
use crate::model::{Redraw, Side};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
    last_pane: bool,
) -> Result<(), CmdError> {
    let (Some(w), Some(mut wp)) = (found.window, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let zflag = args.flag('Z');
    if last_pane || args.flag('l') {
        return last(server, w, args, zflag);
    }
    for flag in ['m', 'M', 'P', 'g'] {
        if args.flag(flag) {
            return Err(EdgeError::NotImplemented(format!("select-pane -{flag}")).into());
        }
    }
    let side = [
        ('L', Side::Left),
        ('R', Side::Right),
        ('U', Side::Up),
        ('D', Side::Down),
    ]
    .into_iter()
    .find(|(f, _)| args.flag(*f));
    if let Some((_, side)) = side {
        // window_push_zoom(w, 0, 1) .. window_pop_zoom: the neighbour is
        // found in the full layout and the zoom comes back.
        let was = server.model.push_zoom(w, false, true);
        let next = server.model.neighbour(wp, side);
        server.model.pop_zoom(w, was);
        match next {
            Some(p) => wp = p,
            None => return Ok(()),
        }
    }
    if args.flag('e') || args.flag('d') {
        if let Some(p) = server.model.panes.get_mut(&wp) {
            p.input_off = args.flag('d') && !args.flag('e');
        }
        server.model.redraw(Redraw::WindowStatus(w));
        return Ok(());
    }
    if let Some(t) = args.value('T') {
        let title = expand(server, found, t);
        if let Some(p) = server.model.panes.get_mut(&wp) {
            // screen_set_title: a change redraws the status.
            let before = p.term.title();
            p.term.set_title(&title);
            if p.term.title() != before {
                server.model.redraw(Redraw::WindowStatus(w));
            }
        }
        return Ok(());
    }
    if server.model.windows[&w].active() == wp {
        return Ok(());
    }
    // cmd-select-pane.c:283-285: the hook's target is the new pane when
    // it became active, else the queue's current state.
    let changed = activate(server, w, wp, zflag);
    item.hook_target = Some(if changed {
        Some(Found {
            window: Some(w),
            pane: Some(wp),
            ..found
        })
    } else {
        item.origin.current
    });
    Ok(())
}

/// `last-pane` / `select-pane -l` (cmd-select-pane.c:167-205).
fn last(server: &mut Server, w: WindowId, args: &Args, zflag: bool) -> Result<(), CmdError> {
    let window = &server.model.windows[&w];
    let mut lastwp = window.panes.last.first().copied();
    // A pane spawned without being visited (split-window -d) is not on the
    // stack: with two panes, the other one.
    if lastwp.is_none() && window.panes.panes.len() == 2 {
        let panes = &window.panes.panes;
        let at = panes.iter().position(|p| *p == window.active());
        lastwp = at.and_then(|i| {
            i.checked_sub(1)
                .map(|j| panes[j])
                .or_else(|| panes.get(i + 1).copied())
        });
    }
    let Some(lastwp) = lastwp else {
        return Err(EdgeError::Message("no last pane".into()).into());
    };
    if args.flag('e') || args.flag('d') {
        if let Some(p) = server.model.panes.get_mut(&lastwp) {
            p.input_off = args.flag('d') && !args.flag('e');
        }
        server.model.redraw(Redraw::WindowStatus(w));
        return Ok(());
    }
    activate(server, w, lastwp, zflag);
    Ok(())
}

/// Make `wp` active the way select-pane does: a hidden pane in a zoomed
/// window unzooms it, and with `-Z` the zoom comes back on the new pane
/// (`window_push_zoom(w, 0, Z)` .. `window_pop_zoom`).
fn activate(server: &mut Server, w: WindowId, wp: PaneId, zflag: bool) -> bool {
    let visible = server.model.windows[&w].is_visible(wp);
    let was = !visible && server.model.push_zoom(w, false, zflag);
    let changed = server.model.set_active(w, wp);
    if changed {
        // cmd_select_pane_redraw: every client of a session with the
        // window redraws its status.
        server.model.redraw(Redraw::WindowStatus(w));
    }
    if !visible {
        server.model.pop_zoom(w, was);
    }
    changed
}
