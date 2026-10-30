//! `list-sessions` (cmd-list-sessions.c:57-105).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// `LIST_SESSIONS_TEMPLATE` (cmd-list-sessions.c:31-37).
const TEMPLATE: &str = "#{session_name}: #{session_windows} windows \
     (created #{t:session_created})\
     #{?session_grouped, (group ,}\
     #{session_group}#{?session_grouped,),}\
     #{?session_attached, (attached),}";

/// `sort_order_from_string` (sort.c:371-394) for the orders a session list
/// accepts; everything else valid is "no sort" (name order).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Name,
    Index,
    Creation,
    Activity,
}

fn order(s: Option<&str>) -> Result<Option<Order>, EdgeError> {
    let Some(s) = s else { return Ok(None) };
    let s = s.to_ascii_lowercase();
    Ok(Some(match s.as_str() {
        "name" | "title" => Order::Name,
        "index" | "key" => Order::Index,
        "creation" => Order::Creation,
        "activity" => Order::Activity,
        "modifier" | "order" | "size" | "z" => return Ok(None),
        _ => return Err(EdgeError::InvalidSortOrder),
    }))
}

pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    let template = args.value('F').unwrap_or(TEMPLATE);
    let filter = args.value('f');
    let ord = order(args.value('O'))?;
    let mut sessions: Vec<_> = server.model.sessions_by_name().collect();
    // sort_qsort (sort.c:39-61): no -O keeps the tree (name) order and
    // ignores -r; otherwise sort, ties by name, -r reverses.
    if let Some(ord) = ord {
        sessions.sort_by(|a, b| {
            let primary = match ord {
                Order::Name => std::cmp::Ordering::Equal,
                Order::Index => a.id.cmp(&b.id),
                Order::Creation => a.created.cmp(&b.created),
                Order::Activity => b.activity.cmp(&a.activity),
            };
            let o = primary.then_with(|| a.name.cmp(&b.name));
            if args.flag('r') { o.reverse() } else { o }
        });
    }
    let ids: Vec<_> = sessions.iter().map(|s| s.id).collect();
    let mut lines = Vec::new();
    for (i, id) in ids.into_iter().enumerate() {
        let found = Found {
            session: Some(id),
            ..Found::default()
        };
        let mut ctx = server.format_ctx(found, item.client);
        ctx.extra.push(("line", i.to_string()));
        if let Some(f) = filter {
            let v = format::expand_str(f, &ctx);
            if v.is_empty() || v == "0" {
                continue;
            }
        }
        lines.push(format::expand_str(template, &ctx));
    }
    for line in lines {
        item.print(&line);
    }
    Ok(())
}
