//! `list-commands` (cmd-list-commands.c:40-100).

use vtmux_core::cmd::{self, Args, CmdEntry, CmdError, TMUX_COMMANDS};
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// `LIST_COMMANDS_TEMPLATE`.
const TEMPLATE: &str =
    "#{command_list_name}#{?command_list_alias, (#{command_list_alias}),} #{command_list_usage}";

/// `cmd_list_single_command`: an empty expansion prints no line.
fn single(server: &Server, item: &mut Item, entry: &CmdEntry, template: &str) {
    let mut ctx = server.format_ctx(Found::default(), item.client);
    for (k, v) in [
        ("command_list_name", entry.name),
        ("command_list_alias", entry.alias.unwrap_or("")),
        ("command_list_usage", entry.usage),
    ] {
        ctx.extra.push((k.into(), v.to_owned()));
    }
    let line = format::expand_str(template, &ctx);
    if !line.is_empty() {
        item.print(&line);
    }
}

pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    let template = args.value('F').unwrap_or(TEMPLATE);
    match args.positional(0) {
        None => {
            for entry in &TMUX_COMMANDS {
                single(server, item, entry, template);
            }
        }
        Some(name) => single(server, item, cmd::table::find(name)?, template),
    }
    Ok(())
}
