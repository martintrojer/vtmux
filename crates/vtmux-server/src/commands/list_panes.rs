//! `list-panes` (cmd-list-panes.c:51-167) with `sort_get_panes_window`
//! (sort.c:207-250, 562-583).

use std::cmp::Ordering;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;
use vtmux_core::ids::{PaneId, SessionId};

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// The default templates by scope (cmd-list-panes.c:110-136): the window,
/// a session (`-s`) or the server (`-a`).
const BODY: &str = "[#{pane_width}x#{pane_height}\
     #{?pane_floating_flag, #{pane_x}#,#{pane_y}#,#{pane_z}}] [history \
     #{history_size}/#{history_limit}, #{history_bytes} bytes] #{pane_id}\
     #{?pane_active, (active),}#{?pane_dead, (dead),}";

/// `sort_order_from_string` (sort.c:371-394) as panes use it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Activity,
    Creation,
    Index,
    /// `modifier`: no key, ties by title.
    Modifier,
    Name,
    /// `order`: the list as it is, `-r` reverses it.
    Listed,
    Size,
    Z,
}

fn order(s: Option<&str>) -> Result<Option<Order>, EdgeError> {
    let Some(s) = s else { return Ok(None) };
    Ok(Some(match s.to_ascii_lowercase().as_str() {
        "activity" => Order::Activity,
        "creation" => Order::Creation,
        "index" | "key" => Order::Index,
        "modifier" => Order::Modifier,
        "name" | "title" => Order::Name,
        "order" => Order::Listed,
        "size" => Order::Size,
        "z" => Order::Z,
        _ => return Err(EdgeError::InvalidSortOrder),
    }))
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let ord = order(args.value('O'))?;
    let mut windows = Vec::new();
    let push_session = |s: SessionId, windows: &mut Vec<Found>| {
        for (&index, &w) in &server.model.sessions[&s].windows {
            windows.push(Found {
                session: Some(s),
                index: Some(index),
                window: Some(w),
                ..Found::default()
            });
        }
    };
    let prefix = if args.flag('a') {
        for s in server.model.sessions_by_name() {
            push_session(s.id, &mut windows);
        }
        "#{session_name}:#{window_index}.#{pane_index}: "
    } else if args.flag('s') {
        let s = found.session.ok_or(EdgeError::NoCurrentTarget)?;
        push_session(s, &mut windows);
        "#{window_index}.#{pane_index}: "
    } else {
        windows.push(Found {
            pane: None,
            ..found
        });
        "#{pane_index}: "
    };
    let default = format!("{prefix}{BODY}");
    let template = args.value('F').unwrap_or(&default);
    let filter = args.value('f');
    let mut lines = Vec::new();
    for wf in windows {
        let Some(w) = wf.window else { continue };
        let panes = sorted(server, w, ord, args.flag('r'));
        for &p in &panes {
            let mut ctx = server.format_ctx(
                Found {
                    pane: Some(p),
                    ..wf
                },
                item.client,
            );
            // `line` is the pane count (format_add(ft, "line", "%u", n)).
            ctx.extra.push(("line".into(), panes.len().to_string()));
            if let Some(f) = filter
                && !format::is_true(&format::expand_str(f, &ctx))
            {
                continue;
            }
            lines.push(format::expand_str(template, &ctx));
        }
    }
    for line in lines {
        item.print(&line);
    }
    Ok(())
}

/// `sort_qsort` with `sort_pane_cmp`: no `-O` keeps window order (and
/// ignores `-r`); `order` only reverses; the rest sort ascending, ties by
/// title, `-r` reversing the whole comparison.
fn sorted(
    server: &Server,
    w: vtmux_core::ids::WindowId,
    ord: Option<Order>,
    reversed: bool,
) -> Vec<PaneId> {
    let model = &server.model;
    let window = &model.windows[&w];
    let mut panes = window.panes.panes.clone();
    let Some(ord) = ord else { return panes };
    if ord == Order::Listed {
        if reversed {
            panes.reverse();
        }
        return panes;
    }
    let zindex = |p: PaneId| model.pane_zindex(p).unwrap_or(0);
    let title = |p: PaneId| model.panes[&p].term.title();
    panes.sort_by(|&a, &b| {
        let (pa, pb) = (&model.panes[&a], &model.panes[&b]);
        let primary = match ord {
            Order::Activity => pa.active_point.cmp(&pb.active_point),
            Order::Creation => a.cmp(&b),
            Order::Size => (u64::from(pa.rect.sx) * u64::from(pa.rect.sy))
                .cmp(&(u64::from(pb.rect.sx) * u64::from(pb.rect.sy))),
            Order::Index => model.pane_index(a).cmp(&model.pane_index(b)),
            Order::Name => title(a).cmp(&title(b)),
            Order::Z => zindex(a).cmp(&zindex(b)),
            Order::Modifier | Order::Listed => Ordering::Equal,
        };
        let o = primary.then_with(|| title(a).cmp(&title(b)));
        if reversed { o.reverse() } else { o }
    });
    panes
}
