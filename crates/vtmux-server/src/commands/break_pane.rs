//! `break-pane` (cmd-break-pane.c:89-209): move a pane out to a new window
//! of its own in the target session, or relink a one-pane window at the
//! target index. `-W` floats the pane in place instead, sized and placed by
//! `-x -y -X -Y` (or where it floated before).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::layout::{Layout, WindowPanes};
use vtmux_core::options::{TreeId, Value};

use super::{Item, expand, expand_for_target};
use crate::model::Window;
use crate::names::{check_name, clean_name, parse_window_name, stringify_argv};
use crate::server::Server;
use crate::target::Found;

/// `BREAK_PANE_TEMPLATE` (cmd-break-pane.c:29).
const TEMPLATE: &str = "#{session_name}:#{window_index}.#{pane_index}";

#[allow(clippy::too_many_lines)] // cmd-break-pane.c's exec, step by step
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    source: Found,
    found: Found,
) -> Result<(), CmdError> {
    let (Some(src_s), Some(mut src_idx), Some(w), Some(wp)) =
        (source.session, source.index, source.window, source.pane)
    else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    // pane-moved's old index is the source's before any -a/-b shuffle
    // (`old_idx = wl->idx`, cmd-break-pane.c:103).
    let old_idx = src_idx;
    // cmd-break-pane.c:106-109.
    if server.model.windows[&w].panes.modal == Some(wp) {
        return Err(EdgeError::Message("pane is modal".into()).into());
    }
    if args.flag('W') {
        return float(server, args, source, w, wp);
    }
    let Some(dst_s) = found.session else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let name = args.value('n');
    if let Some(n) = name
        && !check_name(n)
    {
        return Err(EdgeError::InvalidWindowName(n.to_owned()).into());
    }
    let mut idx = found.new_index;
    let before = args.flag('b');
    if args.flag('a') || before {
        let from = found
            .index
            .or_else(|| server.model.sessions.get(&dst_s).map(|s| s.current))
            .unwrap_or(0);
        // tmux keeps the source winlink pointer across winlink_shuffle_up
        // (cmd-break-pane.c:120-145), so a source in the shuffled run
        // [start, first free) is unlinked from its new index.
        let start = if before { from } else { from.saturating_add(1) };
        let shifted = src_s == dst_s
            && src_idx >= start
            && (start..=src_idx).all(|i| server.model.sessions[&dst_s].windows.contains_key(&i));
        // winlink_shuffle_up failing returns an error without a message.
        let i = server
            .model
            .shuffle_up(dst_s, from, before)
            .ok_or(CmdError::Quiet)?;
        if shifted {
            src_idx += 1;
        }
        idx = Some(i);
    }
    server.model.server_unzoom(w);
    let detached = args.flag('d');

    let (index, w) = if server.model.windows[&w].panes.panes.len() == 1 {
        // server_link_window then server_unlink_window: the window itself
        // moves, but not to another session of its group (server-fn.c:
        // 296-300), after any -a/-b shuffle.
        if server.model.grouped_together(src_s, dst_s) {
            return Err(EdgeError::Message("sessions are grouped".into()).into());
        }
        let index = server
            .model
            .link_window(dst_s, w, idx)
            .map_err(EdgeError::Message)?;
        server.model.move_mark((src_s, src_idx), (dst_s, index));
        if !detached {
            server.model.set_current(dst_s, index);
        }
        if let Some(n) = name {
            set_name(server, w, n);
        }
        // server_unlink_window: server_destroy_session_group.
        if server.model.detach_window(src_s, src_idx) {
            for x in server.model.group_peers(src_s) {
                server.destroy_session(x);
            }
        }
        let index = server.model.index_of(dst_s, w).unwrap_or(index);
        // cmd-break-pane.c:145.
        server
            .model
            .fire_pane_moved(wp, (w, Some(old_idx)), (w, Some(index)));
        (index, w)
    } else {
        if let Some(i) = idx
            && server.model.sessions[&dst_s].windows.contains_key(&i)
        {
            return Err(EdgeError::Message(format!("index in use: {i}")).into());
        }
        let tc = server.target_client(item.client, item.origin);
        let new_w = move_to_new_window(server, w, wp, name, tc);
        let index = server
            .model
            .link_window(dst_s, new_w, idx)
            .map_err(EdgeError::Message)?;
        // layout_init after session_attach (cmd-break-pane.c:179-181): the
        // pane's pane-resized comes after window-linked, from its new
        // window.
        server.model.fix_panes(new_w);
        // cmd-break-pane.c:185-187.
        server.model.fire_window("window-created", new_w);
        server
            .model
            .fire_pane_moved(wp, (w, Some(old_idx)), (new_w, Some(index)));
        if !detached {
            server.model.set_current(dst_s, index);
            // cmd-break-pane.c:188-191 (not for a one-pane window, which
            // goes to `out` first).
            item.new_current = Some(Found::from_session(&server.model, dst_s));
        }
        (index, new_w)
    };

    if args.flag('P') {
        let at = Found {
            session: Some(dst_s),
            index: Some(index),
            window: Some(w),
            pane: Some(wp),
            new_index: None,
        };
        // format_single(item, template, tc, ...) (cmd-break-pane.c:204).
        let text = expand_for_target(server, item, at, args.value('F').unwrap_or(TEMPLATE));
        item.print(&text);
    }
    Ok(())
}

/// `window_set_name(w, name, 0)` with automatic-rename off.
fn set_name(server: &mut Server, w: vtmux_core::ids::WindowId, name: &str) {
    let name = clean_name(name);
    server
        .model
        .options
        .set_table_value(TreeId::Window(w), "automatic-rename", Value::Flag(false));
    server.model.set_window_name(w, &name);
}

/// Take `wp` out of window `w` (`window_lost_pane`, `layout_close_pane`)
/// and give it a new window of `w`'s size (`window_create`,
/// `layout_init`), named `default_window_name` unless `name` is given.
fn move_to_new_window(
    server: &mut Server,
    w: vtmux_core::ids::WindowId,
    wp: vtmux_core::ids::PaneId,
    name: Option<&str>,
    latest: Option<crate::server::ClientId>,
) -> vtmux_core::ids::WindowId {
    let m = &mut server.model;
    // window_lost_pane: a moved pane loses its mark.
    m.clear_mark_of(wp);
    // window_lost_pane comes before layout_close_pane
    // (cmd-break-pane.c:154-157), so its window-pane-changed comes first.
    m.lose_pane(w, wp);
    let old = m.windows.get_mut(&w).expect("source window");
    old.layout.close_pane(wp);
    let (sx, sy) = (old.sx(), old.sy());
    // cmd-break-pane.c:159: the old window's cell size too.
    let cell_pixels = old.cell_pixels;
    let opts = old.layout.opts;
    m.fix_panes(w);
    // layout_close_pane always fires window-layout-changed (layout.c:1548).
    m.layout_changed(w);

    let new_w = m.window_ids.allocate();
    m.options.add_window(new_w);
    m.options.move_pane(wp, new_w);
    let pane = m.panes.get_mut(&wp).expect("source pane");
    pane.window = new_w;
    pane.changed = true;
    let default_name = {
        let cmd = stringify_argv(&pane.argv);
        if cmd.is_empty() {
            parse_window_name(&pane.shell.to_string_lossy())
        } else {
            parse_window_name(&cmd)
        }
    };
    let mut layout = Layout::new(sx, sy, wp);
    layout.opts = opts;
    let activity = m.touch();
    m.windows.insert(
        new_w,
        Window {
            id: new_w,
            name: default_name,
            activity,
            activity_secs: crate::model::now_epoch(),
            layout,
            panes: WindowPanes::new(wp),
            zoom: None,
            was_zoomed: None,
            last_layout: None,
            old_layout: None,
            pending_size: None,
            pending_pixels: (0, 0),
            cell_pixels,
            manual_size: (sx, sy),
            // cmd-break-pane.c:166.
            latest,
            alerts: crate::alerts::WindowAlerts::default(),
            menu: None,
            menu_last: (0, 0),
        },
    );
    // window_create -> window_update_activity.
    m.alerts_queue(new_w, crate::alerts::ACTIVITY);
    if let Some(n) = name {
        let n = clean_name(n);
        m.options.set_table_value(
            TreeId::Window(new_w),
            "automatic-rename",
            Value::Flag(false),
        );
        m.windows.get_mut(&new_w).expect("just made").name = n;
    }
    new_w
}

/// `cmd_break_pane_float` (cmd-break-pane.c:49-87): the tiled pane floats
/// where it is, on top of the z-order.
fn float(
    server: &mut Server,
    args: &Args,
    source: Found,
    w: vtmux_core::ids::WindowId,
    wp: vtmux_core::ids::PaneId,
) -> Result<(), CmdError> {
    let window = &server.model.windows[&w];
    if window.is_floating_now(wp) {
        return Err(EdgeError::Message("pane is already floating".into()).into());
    }
    if window.zoomed() {
        return Err(EdgeError::Message("can't float a pane while window is zoomed".into()).into());
    }
    let border = server.model.window_has_border(w);
    let fa = vtmux_core::layout::FloatArgs {
        width: args.value('x').map(|v| expand(server, source, v)),
        height: args.value('y').map(|v| expand(server, source, v)),
        x: args.value('X').map(|v| expand(server, source, v)),
        y: args.value('Y').map(|v| expand(server, source, v)),
    };
    let window = server.model.windows.get_mut(&w).expect("source window");
    let start = window.layout.saved_float(wp);
    let g = window
        .layout
        .floating_geometry(&fa, border, start)
        .map_err(|e| EdgeError::Message(format!("failed to float pane: {e}")))?;
    window.layout.float_pane(wp, g);
    window.panes.z_order.retain(|&p| p != wp);
    window.panes.z_order.insert(0, wp);
    if !args.flag('d') {
        server.model.set_active(w, wp);
    }
    server.model.fix_panes(w);
    server.model.layout_changed(w);
    server.model.redraw(crate::model::Redraw::Window(w));
    Ok(())
}
