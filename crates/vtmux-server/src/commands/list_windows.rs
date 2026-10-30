//! `list-windows` (cmd-list-windows.c:65-125) with `sort_get_winlinks`
//! (sort.c:252-303, 586-633).

use std::cmp::Ordering;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// `LIST_WINDOWS_TEMPLATE` (cmd-list-windows.c:30-35).
const TEMPLATE: &str = "#{window_index}: #{window_name}#{window_raw_flags} \
     (#{window_panes} panes) [#{window_width}x#{window_height}] \
     [layout #{window_layout}] #{window_id}#{?window_active, (active),}";
/// `LIST_WINDOWS_WITH_SESSION_TEMPLATE` (cmd-list-windows.c:36-40): it
/// ends with a space.
const ALL_TEMPLATE: &str = "#{session_name}:#{window_index}: #{window_name}#{window_raw_flags} \
     (#{window_panes} panes) [#{window_width}x#{window_height}] ";

/// `sort_order_from_string` (sort.c:371-394) as windows use it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Index,
    Creation,
    Activity,
    Name,
    Size,
    /// `modifier` and `z`: no key, ties by name.
    Unkeyed,
    /// `order`: the list as it is, `-r` reverses it.
    Listed,
}

fn order(s: Option<&str>) -> Result<Option<Order>, EdgeError> {
    let Some(s) = s else { return Ok(None) };
    Ok(Some(match s.to_ascii_lowercase().as_str() {
        "index" | "key" => Order::Index,
        "creation" => Order::Creation,
        "activity" => Order::Activity,
        "name" | "title" => Order::Name,
        "size" => Order::Size,
        "modifier" | "z" => Order::Unkeyed,
        "order" => Order::Listed,
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
    let model = &server.model;
    let sessions: Vec<_> = if args.flag('a') {
        model.sessions_by_name().map(|s| s.id).collect()
    } else {
        vec![found.session.ok_or(EdgeError::NoCurrentTarget)?]
    };
    let mut links: Vec<Found> = sessions
        .iter()
        .flat_map(|&s| {
            model.sessions[&s]
                .windows
                .iter()
                .map(move |(&i, &w)| Found {
                    session: Some(s),
                    index: Some(i),
                    window: Some(w),
                    ..Found::default()
                })
        })
        .collect();
    sort(server, &mut links, ord, args.flag('r'));
    let default = if args.flag('a') {
        ALL_TEMPLATE
    } else {
        TEMPLATE
    };
    let template = args.value('F').unwrap_or(default);
    let filter = args.value('f');
    let mut out = Vec::new();
    for link in &links {
        let mut ctx = server.format_ctx(*link, item.client);
        ctx.extra.push(("line", links.len().to_string()));
        if let Some(f) = filter
            && !format::is_true(&format::expand_str(f, &ctx))
        {
            continue;
        }
        out.push(format::expand_str(template, &ctx));
    }
    for line in out {
        item.print(&line);
    }
    Ok(())
}

/// `sort_qsort` with `sort_winlink_cmp`: no `-O` keeps the list (and
/// ignores `-r`); `order` only reverses; the rest sort, ties by name, `-r`
/// reversing the whole comparison.
fn sort(server: &Server, links: &mut [Found], ord: Option<Order>, reversed: bool) {
    let Some(ord) = ord else { return };
    if ord == Order::Listed {
        if reversed {
            links.reverse();
        }
        return;
    }
    let window = |f: &Found| &server.model.windows[&f.window.expect("listed link")];
    links.sort_by(|a, b| {
        let (wa, wb) = (window(a), window(b));
        let primary = match ord {
            Order::Index => a.index.cmp(&b.index),
            // Window IDs are handed out in creation order.
            Order::Creation => wa.id.cmp(&wb.id),
            Order::Activity => wb.activity.cmp(&wa.activity),
            Order::Size => (u64::from(wa.sx()) * u64::from(wa.sy()))
                .cmp(&(u64::from(wb.sx()) * u64::from(wb.sy()))),
            Order::Name | Order::Unkeyed | Order::Listed => Ordering::Equal,
        };
        let o = primary.then_with(|| wa.name.cmp(&wb.name));
        if reversed { o.reverse() } else { o }
    });
}
