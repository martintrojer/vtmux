//! `kill-session` (cmd-kill-session.c:47-80).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let s = found.session.ok_or(EdgeError::NoCurrentTarget)?;
    if args.value('f').is_some() && (!args.flag('a') || args.flag('C')) {
        return Err(EdgeError::FilterNeedsAll.into());
    }
    if args.flag('C') {
        // Every window of the session loses its alert flags.
        server.model.alerts_clear_session(s);
        return Ok(());
    }
    if args.flag('a') {
        // cmd_kill_session_all: in name order, each other session the
        // filter holds for, expanded against that session alone
        // (format_defaults(ft, NULL, s, NULL, NULL)) once the ones before
        // it are gone.
        let filter = args.value('f');
        let others: Vec<_> = server
            .model
            .by_name
            .values()
            .copied()
            .filter(|x| *x != s)
            .collect();
        for other in others {
            if !server.model.sessions.contains_key(&other) {
                continue;
            }
            if let Some(f) = filter {
                let session_only = Found {
                    session: Some(other),
                    ..Found::default()
                };
                if !format::is_true(&super::expand(server, session_only, f)) {
                    continue;
                }
            }
            server.destroy_session(other);
        }
        return Ok(());
    }
    // -g: every member of the session's group, in the order they joined.
    if args.flag('g') {
        for x in server.model.group_peers(s) {
            server.destroy_session(x);
        }
        return Ok(());
    }
    server.destroy_session(s);
    Ok(())
}
