//! `list-clients` (cmd-list-clients.c:50-105): attached clients, sorted per
//! `sort_get_clients` / `sort_client_cmp` (sort.c:106-150, 451-476).

use std::cmp::Ordering;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;

use super::Item;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `LIST_CLIENTS_TEMPLATE` (cmd-list-clients.c:31-36).
const TEMPLATE: &str = "#{client_name}: #{session_name} \
     [#{client_width}x#{client_height} #{client_termname}] \
     #{?#{!=:#{client_uid},#{uid}},\
     [user #{?client_user,#{client_user},#{client_uid},}] ,}\
     #{?client_flags,(,}#{client_flags}#{?client_flags,),}";

/// `sort_order_from_string` (sort.c:371-394), as far as clients care.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Name,
    Size,
    Creation,
    Activity,
    /// `index`, `modifier`, `z`: no key, so the name breaks every tie.
    Tie,
    /// `order`: the list as it is, reversed by `-r`.
    Listed,
}

fn order(s: &str) -> Result<Order, EdgeError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "activity" => Order::Activity,
        "creation" => Order::Creation,
        "index" | "key" | "modifier" | "z" => Order::Tie,
        "name" | "title" => Order::Name,
        "order" => Order::Listed,
        "size" => Order::Size,
        _ => return Err(EdgeError::InvalidSortOrder),
    })
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let only = args.flag('t').then_some(found.session);
    let template = args.value('F').unwrap_or(TEMPLATE);
    let filter = args.value('f');
    let ord = args.value('O').map(order).transpose()?;
    let reversed = args.flag('r');

    // Attached clients in connection order.
    let mut clients: Vec<(ClientId, String, (u32, u32), u64)> = server
        .clients
        .iter()
        .filter(|(_, c)| c.attached.is_some())
        .map(|(&id, c)| {
            let size = c
                .hello
                .as_ref()
                .and_then(|h| h.tty.as_ref())
                .map_or((0, 0), |t| (t.cols, t.rows));
            (id, server.client_name(id), size, c.activity)
        })
        .collect();
    match ord {
        None => {}
        Some(Order::Listed) => {
            if reversed {
                clients.reverse();
            }
        }
        Some(ord) => clients.sort_by(|a, b| {
            let primary = match ord {
                Order::Size => a.2.cmp(&b.2),
                Order::Creation => a.0.cmp(&b.0),
                Order::Activity => b.3.cmp(&a.3),
                _ => Ordering::Equal,
            };
            let o = primary.then_with(|| a.1.cmp(&b.1));
            if reversed { o.reverse() } else { o }
        }),
    }

    let mut lines = Vec::new();
    for (i, (id, ..)) in clients.iter().enumerate() {
        let session = server.clients[id].attached;
        if only.is_some_and(|s| s != session) {
            continue;
        }
        let found = session.map_or_else(Found::default, |s| Found::from_session(&server.model, s));
        let mut ctx = server.format_ctx(found, Some(*id));
        ctx.extra.push(("line", i.to_string()));
        if let Some(f) = filter {
            let v = format::expand_str(f, &ctx);
            if v.is_empty() || v == "0" {
                continue;
            }
        }
        lines.push(format::expand_str(template, &ctx));
    }
    for l in lines {
        item.print(&l);
    }
    Ok(())
}
