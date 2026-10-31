//! `list-buffers` (cmd-list-buffers.c:44-92, sort.c:64-100).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;

use super::Item;
use crate::paste::PasteBuffer;
use crate::server::Server;
use crate::target::Found;

/// `LIST_BUFFERS_TEMPLATE`.
const TEMPLATE: &str = "#{buffer_name}: #{buffer_size} bytes: \"#{buffer_sample}\"";

/// The sort orders a buffer list tells apart; every other valid order
/// compares equal and falls back to the name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Name,
    Creation,
    Size,
    /// `order`: the time list as it is, reversed by `-r`.
    Listed,
    /// Valid, but no buffer key: name order.
    Other,
}

/// `sort_order_from_string` (sort.c:371-394): case-insensitive; `None` is
/// no `-O`, which sorts nothing and ignores `-r`.
fn order(s: Option<&str>) -> Result<Option<Order>, EdgeError> {
    let Some(s) = s else { return Ok(None) };
    Ok(Some(match s.to_ascii_lowercase().as_str() {
        "name" | "title" => Order::Name,
        "creation" => Order::Creation,
        "size" => Order::Size,
        "order" => Order::Listed,
        "activity" | "index" | "key" | "modifier" | "z" => Order::Other,
        _ => return Err(EdgeError::InvalidSortOrder),
    }))
}

/// `sort_get_buffers` over `paste_walk` (time, most recent first).
fn sorted(mut list: Vec<&PasteBuffer>, ord: Option<Order>, reversed: bool) -> Vec<&PasteBuffer> {
    match ord {
        None => {}
        Some(Order::Listed) => {
            if reversed {
                list.reverse();
            }
        }
        Some(ord) => list.sort_by(|a, b| {
            let primary = match ord {
                Order::Creation => b.order.cmp(&a.order),
                Order::Size => a.data.len().cmp(&b.data.len()),
                _ => std::cmp::Ordering::Equal,
            };
            let o = primary.then_with(|| a.name.cmp(&b.name));
            if reversed { o.reverse() } else { o }
        }),
    }
    list
}

pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    let template = args.value('F').unwrap_or(TEMPLATE);
    let filter = args.value('f');
    let ord = order(args.value('O'))?;
    let names: Vec<String> = sorted(server.model.paste.by_time(), ord, args.flag('r'))
        .into_iter()
        .map(|b| b.name.clone())
        .collect();
    let mut lines = Vec::new();
    for name in names {
        let mut ctx = server.format_ctx(Found::default(), None);
        ctx.buffer = Some(name);
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
