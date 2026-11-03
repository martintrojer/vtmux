//! `detach-client` (cmd-detach-client.c:63-117): `-t` names the client
//! (else the current one), `-s` detaches every client on a session, `-a`
//! every client but the target, `-P` has the client hang up its parent
//! (`MSG_DETACHKILL`), `-E` runs a command in place of the client
//! (`server_client_exec`). `suspend-client` shares the entry point
//! (`server_client_suspend`).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::server::{ClientId, Server};
use crate::target;

/// `CMD_CLIENT_TFLAG` (cmd-queue.c:604-609): the target client is found
/// before anything else, and a miss is an error.
fn target_client(server: &Server, item: &Item, args: &Args) -> Result<ClientId, CmdError> {
    Ok(match args.value('t') {
        None => server
            .target_client(item.client, item.origin)
            .ok_or(EdgeError::NoCurrentClient)?,
        Some(name) => server
            .find_client(item.client, item.origin, Some(name))
            .ok_or_else(|| {
                let name = name.strip_suffix(':').unwrap_or(name);
                EdgeError::Message(format!("can't find client: {name}"))
            })?,
    })
}

/// `suspend-client` (cmd-detach-client.c:68-71).
pub fn suspend(server: &mut Server, item: &Item, args: &Args) -> Result<(), CmdError> {
    let tc = target_client(server, item, args)?;
    server.suspend_client(tc);
    Ok(())
}

pub fn exec(server: &mut Server, item: &Item, args: &Args) -> Result<(), CmdError> {
    let tc = target_client(server, item, args)?;
    let hup = args.flag('P');
    let command = args.value('E');
    let leave = |server: &mut Server, id: ClientId| match command {
        Some(command) => server.exec_client(id, command),
        None => server.detach_client(id, hup),
    };

    if args.flag('s') {
        // CMD_FIND_CANFAIL: no such session is not an error.
        let spec = args.target('s');
        let source = target::resolve(
            &server.model,
            item.origin,
            spec.as_ref(),
            vtmux_core::cmd::TargetKind::Session,
            vtmux_core::cmd::FindFlags::CANFAIL,
        )?;
        if let Some(s) = source.session {
            for id in server.session_clients(s) {
                leave(server, id);
            }
        }
    } else if args.flag('a') {
        let others: Vec<_> = server
            .attached_clients()
            .into_iter()
            .filter(|&id| id != tc)
            .collect();
        for id in others {
            leave(server, id);
        }
    } else {
        leave(server, tc);
    }
    Ok(())
}
