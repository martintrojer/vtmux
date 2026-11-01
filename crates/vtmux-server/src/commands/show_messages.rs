//! `show-messages` (cmd-show-messages.c:44-110): the server's message log,
//! `-J` the jobs, `-T` the terminals.
//!
//! `-T` lists every terminfo capability of each client terminal
//! (`tty_term_describe`); vtmux has no terminfo model (D8, D-007), so it
//! prints the `Terminal N:` header and the features it knows, one per line.

use vtmux_core::cmd::Args;
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// `SHOW_MESSAGES_TEMPLATE`.
const TEMPLATE: &str = "#{t/p:message_time}: #{message_text}";

/// `cmd_show_messages_terminals`: whether anything was printed.
fn terminals(server: &Server, item: &mut Item, args: &Args, blank: bool) -> bool {
    let target = args
        .flag('t')
        .then(|| server.find_client(item.client, item.origin, args.value('t')))
        .flatten();
    let mut blank = blank;
    let mut n = 0;
    // tmux's `tty_terms` list is newest first.
    for (&id, c) in server.clients.iter().rev() {
        if c.attached.is_none() || c.control.is_some() {
            continue;
        }
        if args.flag('t') && target.is_some_and(|t| t != id) {
            continue;
        }
        let Some(term) = server.client_term(id) else {
            continue;
        };
        if blank {
            item.print("");
            blank = false;
        }
        item.print(&format!(
            "Terminal {n}: {} for {}, flags=0x{:x}:",
            term.0,
            server.client_name(id),
            term.1
        ));
        item.print(&format!("   features: {}", term.2));
        n += 1;
    }
    n != 0
}

pub fn exec(server: &Server, item: &mut Item, args: &Args) {
    let mut done = false;
    let mut blank = false;
    if args.flag('T') {
        blank = terminals(server, item, args, blank);
        done = true;
    }
    if args.flag('J') {
        for line in server.jobs.summary() {
            if blank {
                item.print("");
                blank = false;
            }
            item.print(&line);
        }
        done = true;
    }
    if done {
        return;
    }
    let lines: Vec<String> = server
        .messages_newest_first()
        .map(|m| {
            let mut ctx = server.format_ctx(Found::default(), item.client);
            ctx.extra.push(("message_text".into(), m.text.clone()));
            ctx.extra
                .push(("message_number".into(), m.number.to_string()));
            ctx.extra.push(("message_time".into(), m.time.to_string()));
            format::expand_str(TEMPLATE, &ctx)
        })
        .collect();
    for line in lines {
        item.print(&line);
    }
}
