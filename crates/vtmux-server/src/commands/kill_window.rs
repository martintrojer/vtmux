//! `kill-window` (cmd-kill-window.c:56-149).

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
    let filter = args.value('f');
    if filter.is_some() && !args.flag('a') {
        return Err(EdgeError::FilterNeedsAll.into());
    }
    let (Some(s), Some(w)) = (found.session, found.window) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    if !args.flag('a') {
        server.kill_window(w, true);
        return Ok(());
    }
    // cmd_kill_window_all: nothing to do for a session's only winlink.
    if server.model.sessions[&s].windows.len() == 1 {
        return Ok(());
    }
    let matches = |server: &Server, i: u32, x| {
        filter.is_none_or(|f| {
            let ctx = server.format_ctx(
                Found {
                    session: Some(s),
                    index: Some(i),
                    window: Some(x),
                    ..Found::default()
                },
                None,
            );
            format::is_true(&format::expand_str(f, &ctx))
        })
    };
    // Every other window matching the filter, one at a time from the
    // lowest index.
    while let Some(session) = server.model.sessions.get(&s) {
        let victim = session
            .windows
            .iter()
            .filter(|(_, x)| **x != w)
            .find(|(i, x)| matches(server, **i, **x))
            .map(|(_, x)| *x);
        let Some(victim) = victim else { break };
        server.kill_window(victim, false);
    }
    // The current window linked in more than once goes too when any of
    // its links matches (cmd-kill-window.c:118-131).
    if let Some(session) = server.model.sessions.get(&s) {
        let links: Vec<u32> = session
            .windows
            .iter()
            .filter(|(_, x)| **x == w)
            .map(|(i, _)| *i)
            .collect();
        if links.len() > 1 && links.iter().any(|i| matches(server, *i, w)) {
            server.kill_window(w, false);
        }
    }
    server.model.renumber_all();
    Ok(())
}
