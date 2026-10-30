//! `kill-pane` (cmd-kill-pane.c:51-115).

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
    let (Some(w), Some(wp)) = (found.window, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    if !args.flag('a') {
        // server_kill_pane: the last pane takes the window with it.
        server.destroy_pane(wp);
        return Ok(());
    }
    // cmd_kill_pane_all: every other pane matching the filter, unzoomed.
    server.model.unzoom(w);
    let others: Vec<_> = server.model.windows[&w]
        .panes
        .panes
        .iter()
        .copied()
        .filter(|p| *p != wp)
        .collect();
    for p in others {
        if let Some(f) = filter {
            let ctx = server.format_ctx(
                Found {
                    pane: Some(p),
                    ..found
                },
                None,
            );
            if !format::is_true(&format::expand_str(f, &ctx)) {
                continue;
            }
        }
        server.destroy_pane(p);
    }
    Ok(())
}
