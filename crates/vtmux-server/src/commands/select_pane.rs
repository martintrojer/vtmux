//! `select-pane` and `last-pane` (cmd-select-pane.c:146-291).

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
    if args.flag('m') || args.flag('M') {
        mark(server, found, wp, args.flag('M'));
        return Ok(());
    }
    // cmd-select-pane.c:221-235: -P sets the pane's window-style and
    // window-active-style (unchecked, options_set_string); -g prints its
    // window-style and stops.
    let pane_tree = vtmux_core::options::TreeId::Pane(wp);
    if let Some(style) = args.value('P') {
        for name in ["window-style", "window-active-style"] {
            server.model.options.set_table_value(
                pane_tree,
                name,
                vtmux_core::options::Value::String(style.to_owned()),
            );
        }
        server.model.redraw_panes.insert(wp);
    }
    if args.flag('g') {
        let style = server
            .model
            .options
            .get(pane_tree, "window-style")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        item.print(&style);
        return Ok(());
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
            // cmd-select-pane.c:259-269: a title screen_set_title takes,
            // changed or not, redraws the status and fires the hook.
            if p.term.set_title(&title) {
                server.model.redraw(Redraw::WindowStatus(w));
                // cmd-select-pane.c:259-266.
                let ev = crate::events::Event::new("pane-title-changed")
                    .target(Found::from_pane(&server.model, wp))
                    .pane("pane", wp)
                    .window(&server.model, "window", w)
                    .text("new_title", title);
                server.model.push_event(ev);
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

/// `cmd_select_pane_marked_pane` (cmd-select-pane.c:84-143): `-m` marks
/// the pane, or clears the mark if it is the marked pane; `-M` clears it.
/// `-m` on a pane hidden by a zoom does nothing. Then the
/// marked-pane-changed event, and both panes' borders and status redrawn.
fn mark(server: &mut Server, found: Found, wp: PaneId, clear: bool) {
    let m = &mut server.model;
    let (Some(s), Some(index), Some(w)) = (found.session, found.index, found.window) else {
        return;
    };
    if !clear && !m.windows[&w].is_visible(wp) {
        return;
    }
    let lwp = m.marked().and_then(|f| f.pane);
    if clear || m.is_marked(s, index, wp) {
        m.set_marked(None);
    } else {
        m.set_marked(Some(Found {
            session: Some(s),
            index: Some(index),
            window: Some(w),
            pane: Some(wp),
            new_index: None,
        }));
    }
    let mwp = m.marked().and_then(|f| f.pane);
    let about = mwp.or(lwp).unwrap_or(wp);
    let about_window = m.panes.get(&about).map_or(w, |p| p.window);
    let mut ev = crate::events::Event::new("marked-pane-changed")
        .target(Found::from_pane(m, about))
        .pane("pane", about);
    if let Some(n) = mwp {
        ev = ev.pane("new_pane", n);
    }
    ev = ev.window(m, "window", about_window);
    if let Some(l) = lwp {
        ev = ev.pane("old_pane", l);
    }
    ev = ev.int("marked", i64::from(mwp.is_some()));
    m.push_event(ev);
    for p in [lwp, mwp].into_iter().flatten() {
        if let Some(pw) = m.panes.get(&p).map(|x| x.window) {
            m.redraw_panes.insert(p);
            m.redraw(Redraw::Window(pw));
            m.redraw(Redraw::WindowStatus(pw));
        }
    }
    // cmd-select-pane.c:140-141: marking a floating pane raises it and
    // makes it active.
    if m.windows[&w].is_floating_now(wp) {
        m.switch_active(w, wp);
    }
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
    // cmd-select-pane.c:276-279: under a modal pane nothing changes.
    let window = &server.model.windows[&w];
    let visible = window.modal_blocks(wp) || window.is_visible(wp);
    let was = !visible && server.model.push_zoom(w, false, zflag);
    let changed = server.model.switch_active(w, wp);
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
