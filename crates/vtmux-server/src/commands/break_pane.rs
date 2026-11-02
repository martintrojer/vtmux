//! `break-pane` (cmd-break-pane.c:89-209): move a pane out to a new window
//! of its own in the target session, or relink a one-pane window at the
//! target index. `-W` (float the pane in place) and its `-x -y -X -Y` are
//! not done yet.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::layout::{Layout, WindowPanes};
use vtmux_core::options::{TreeId, Value};

use super::{Item, expand};
use crate::model::Window;
use crate::names::{check_name, clean_name, parse_window_name, stringify_argv};
use crate::server::Server;
use crate::target::Found;

/// `BREAK_PANE_TEMPLATE` (cmd-break-pane.c:29).
const TEMPLATE: &str = "#{session_name}:#{window_index}.#{pane_index}";

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    source: Found,
    found: Found,
) -> Result<(), CmdError> {
    if args.flag('W') {
        return Err(EdgeError::NotImplemented("break-pane -W".into()).into());
    }
    let (Some(src_s), Some(mut src_idx), Some(w), Some(wp)) =
        (source.session, source.index, source.window, source.pane)
    else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
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
        let Some(i) = server.model.shuffle_up(dst_s, from, before) else {
            item.status = 1;
            return Ok(());
        };
        if shifted {
            src_idx += 1;
        }
        idx = Some(i);
    }
    server.model.unzoom(w);
    let detached = args.flag('d');

    let (index, w) = if server.model.windows[&w].panes.panes.len() == 1 {
        // server_link_window then server_unlink_window: the window itself
        // moves.
        let index = server
            .model
            .link_window(dst_s, w, idx)
            .map_err(EdgeError::Message)?;
        if !detached {
            server.model.set_current(dst_s, index);
        }
        if let Some(n) = name {
            set_name(server, w, n);
        }
        if server.model.detach_window(src_s, src_idx) {
            server.destroy_session(src_s);
        }
        let index = server.model.index_of(dst_s, w).unwrap_or(index);
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
        if !detached {
            server.model.set_current(dst_s, index);
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
        let text = expand(server, at, args.value('F').unwrap_or(TEMPLATE));
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
    let old = m.windows.get_mut(&w).expect("source window");
    old.panes.remove(wp);
    old.layout.close_pane(wp);
    let (sx, sy) = (old.sx(), old.sy());
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
            last_layout: None,
            old_layout: None,
            pending_size: None,
            manual_size: (sx, sy),
            // cmd-break-pane.c:166.
            latest,
            alerts: crate::alerts::WindowAlerts::default(),
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
    m.fix_panes(new_w);
    new_w
}
