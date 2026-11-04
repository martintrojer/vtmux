//! `lock-server`, `lock-session` and `lock-client` (cmd-lock-server.c):
//! every client with a session, the target session's clients, or the
//! target client run `lock-command` in place of the session
//! (`server_lock`, `server_lock_session`, `server_lock_client`).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use super::detach_client::target_client;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &Item,
    name: &str,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    match name {
        "lock-server" => server.lock_server(),
        "lock-session" => server.lock_session(found.session.ok_or(EdgeError::NoCurrentTarget)?),
        _ => {
            let tc = target_client(server, item, args)?;
            server.lock_client(tc);
        }
    }
    // cmd_lock_server_exec: recalculate_sizes.
    server.recalculate_sizes();
    Ok(())
}
