//! `kill-session` (cmd-kill-session.c:47-80).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

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
        // No alerts in M0: clearing them is a no-op.
        return Ok(());
    }
    if args.flag('a') {
        if args.value('f').is_some() {
            return Err(EdgeError::NotImplemented("kill-session -f".into()).into());
        }
        let others: Vec<_> = server
            .model
            .sessions
            .keys()
            .copied()
            .filter(|x| *x != s)
            .collect();
        for other in others {
            server.destroy_session(other);
        }
        return Ok(());
    }
    // -g: no session groups in M0, so it is the plain kill.
    server.destroy_session(s);
    Ok(())
}
