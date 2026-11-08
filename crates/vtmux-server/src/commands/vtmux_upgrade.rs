//! `vtmux-upgrade [-b binary]` (D27, docs/specs/2026-11-06-live-upgrade.md,
//! *Command*): move the server onto `binary` in place, keeping every pane,
//! client and id. vtmux only: tmux has no such command (its FAQ answers a
//! protocol mismatch with `kill-server`). The work is
//! [`crate::server::upgrade`]'s; the command waits there
//! (`CMD_RETURN_WAIT`) and is continued with the reason if the upgrade is
//! refused.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::server::Server;
use crate::server::upgrade::Requester;

pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    // `-b` from the requester's directory (the server's is not its);
    // without it, the path the server was started as, resolved now
    // (decision 22).
    let binary = match args.value('b') {
        Some(b) => item.cwd.join(b),
        None => server
            .upgrade_default_binary()
            .map_err(EdgeError::Message)?,
    };
    // A control client's stdin lines skip the read-only gate
    // (control.c:695-727); this command runs a binary as the server's
    // owner, so it checks for itself.
    if item
        .client
        .and_then(|c| server.clients.get(&c))
        .is_some_and(|c| c.flags.read_only)
    {
        return Err(EdgeError::Message("client is read-only".to_owned()).into());
    }
    let Some(step) = item.step else {
        return Err(EdgeError::Message("not able to upgrade".to_owned()).into());
    };
    let requester = Requester::Command {
        step,
        client: item.client,
    };
    server
        .upgrade_start(requester, &binary)
        .map_err(EdgeError::Message)?;
    item.blocked = true;
    Ok(())
}
