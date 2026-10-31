//! `wait-for` (cmd-wait-for.c): block the client's command queue on a
//! channel until `-S` signals it, or take (`-L`) and release (`-U`) the
//! channel's lock. Flag precedence follows `cmd_wait_for_exec`: `-E`, `-l`,
//! `-w`, `-S`, `-L`, `-U`, then a plain wait.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::server::Server;
use crate::wait::Outcome;

pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    let name = args.positional_text(0).unwrap_or_default().into_owned();
    if args.flag('E') {
        // Event waits need hook events, which vtmux does not fire yet.
        return Err(EdgeError::NotImplemented("wait-for -E".into()).into());
    }
    if args.flag('l') {
        for c in server.waits.blocked(&name) {
            let line = server.client_name(c);
            item.print(&line);
        }
        return Ok(());
    }
    if let Some(waiter) = args.value('w') {
        // cmd_wait_for_wake: unknown waiters are not an error.
        if let Some(c) = server
            .waits
            .blocked(&name)
            .into_iter()
            .find(|&c| server.client_name(c) == waiter)
        {
            server.waits.wake(&name, c);
            server.wake(c);
        }
        return Ok(());
    }
    if args.flag('S') {
        for c in server.waits.signal(&name) {
            server.wake(c);
        }
        return Ok(());
    }
    let lock = args.flag('L');
    if args.flag('U') && !lock {
        let next = server
            .waits
            .unlock(&name)
            .map_err(|_| EdgeError::Message(format!("channel {name} not locked")))?;
        if let Some(c) = next {
            server.wake(c);
        }
        return Ok(());
    }
    let Some(client) = item.client else {
        let what = if lock { "lock" } else { "wait" };
        return Err(EdgeError::Message(format!("not able to {what}")).into());
    };
    let outcome = if lock {
        server.waits.lock(&name, client)
    } else {
        server.waits.wait(&name, client)
    };
    if outcome == Outcome::Block {
        item.blocked = true;
    }
    Ok(())
}
