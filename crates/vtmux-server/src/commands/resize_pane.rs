//! `resize-pane` (cmd-resize-pane.c:52-189): `-Z` toggles zoom, `-x`/`-y`
//! set a size (cells or a percentage of the window), `-U -D -L -R` adjust
//! by a count, `-M` resizes tiled panes by dragging their borders and a
//! floating pane by its borders and corners, `-T` trims history.

use vtmux_core::cmd::{Args, CmdError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::layout::ResizeRequest;

use super::Item;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let (Some(w), Some(wp)) = (found.window, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    // cmd-resize-pane.c:71-81: -T trims the rows below the cursor, pulling
    // history into view; nothing while the pane is in a mode.
    if args.flag('T') {
        if let Some(pane) = server.model.panes.get_mut(&wp)
            && pane.modes.is_empty()
        {
            pane.term.trim_below_cursor();
            pane.compress.observe(&pane.term, std::time::Instant::now());
            server.model.redraw_panes.insert(wp);
        }
        return Ok(());
    }
    if args.flag('M') {
        mouse_update(server, item);
        return Ok(());
    }
    if args.flag('Z') {
        if server.model.windows[&w].zoomed() {
            server.model.unzoom(w);
        } else {
            server.model.zoom(w, wp);
        }
        return Ok(());
    }
    if !server.model.windows[&w].is_floating_now(wp) {
        server.model.unzoom(w);
    }
    let mut req = ResizeRequest {
        x: args
            .flag('x')
            .then(|| args.value('x').unwrap_or_default().to_owned()),
        y: args
            .flag('y')
            .then(|| args.value('y').unwrap_or_default().to_owned()),
        adjust: Vec::new(),
    };
    // Each adjustment is converted as it is reached, so an earlier -x still
    // applies when a later count is invalid; the layout call does the
    // conversion order, so check the counts first in tmux's U D L R order
    // and stop at the first bad one after applying what came before.
    let mut bad = None;
    for flag in ['U', 'D', 'L', 'R'] {
        if !args.flag(flag) {
            continue;
        }
        let value = args
            .value(flag)
            .or_else(|| args.positional(0))
            .unwrap_or("1");
        match strtonum(value, i64::from(i32::MIN), i64::from(i32::MAX)) {
            Ok(n) => req
                .adjust
                .push((flag, i32::try_from(n).expect("in i32 range"))),
            Err(cause) => {
                bad = Some(cause);
                break;
            }
        }
    }
    let border = server.model.pane_has_border(w, wp);
    let window = server.model.windows.get_mut(&w).expect("target window");
    let mut resized = 0;
    let result = window
        .layout
        .resize_pane_cmd(wp, &req, border, &mut resized);
    server.model.fix_panes(w);
    // layout_resize_layout's own event for each cell resize (layout.c:1002),
    // then cmd-resize-pane.c:187 (also after a failed adjustment).
    for _ in 0..resized {
        server.model.layout_changed(w);
    }
    server.model.layout_changed(w);
    result.map_err(EdgeError::Message)?;
    if let Some(cause) = bad {
        return Err(EdgeError::Message(format!("adjustment {cause}")).into());
    }
    Ok(())
}

/// `cmd_resize_pane_mouse_update` (cmd-resize-pane.c:194-221): the
/// mouse's pane, for a client on its session, gets the drag callback and
/// its first call. Without a mouse event nothing happens.
fn mouse_update(server: &mut Server, item: &Item) {
    let Some(event) = item.mouse.as_ref() else {
        return;
    };
    let Some(found) = event.target.pane_found(&server.model) else {
        return;
    };
    let (Some(client), Some(w), Some(wp)) = (item.client, found.window, found.pane) else {
        return;
    };
    if server.client_session(client) != found.session {
        return;
    }
    let event = event.clone();
    if server.model.windows[&w].is_floating(wp) {
        server.resize_floating_start(client, w, wp, &event);
        return;
    }
    server.set_drag_action(client, Some(crate::mouse::DragAction::ResizeTiled));
    server.resize_drag(client, &event);
}
