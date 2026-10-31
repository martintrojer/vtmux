//! `show-prompt-history` and `clear-prompt-history`
//! (cmd-show-prompt-history.c): every type, or the `-T` one.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::prompt_history::PromptType;
use crate::server::Server;

/// `-T`, or every type.
fn types(args: &Args) -> Result<Vec<PromptType>, CmdError> {
    match args.value('T') {
        None => Ok(PromptType::ALL.to_vec()),
        Some(t) => PromptType::parse(t)
            .map(|t| vec![t])
            .ok_or_else(|| EdgeError::Message(format!("invalid type: {t}")).into()),
    }
}

pub fn show(server: &Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    for ty in types(args)? {
        item.print(&format!("History for {}:\n", ty.name()));
        for (i, line) in server.prompt_history.lines(ty).iter().enumerate() {
            item.print(&format!("{}: {line}", i + 1));
        }
        item.print("");
    }
    Ok(())
}

pub fn clear(server: &mut Server, args: &Args) -> Result<(), CmdError> {
    for ty in types(args)? {
        server.prompt_history.clear(ty);
    }
    Ok(())
}
