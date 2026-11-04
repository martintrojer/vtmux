//! `respawn-window` (cmd-respawn-window.c:46-110, spawn.c `spawn_window`
//! with `SPAWN_RESPAWN`): kill every pane but the first, give it the whole
//! window and run a new process in it. The window keeps its ID and index.
//! Without `-k` it fails while any pane is alive (not kept dead by
//! remain-on-exit). As `spawn_window` ends with `session_select`
//! (no `SPAWN_DETACHED`), the window becomes the session's current one.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use super::respawn_pane::respawn;
use crate::model::Redraw;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let (Some(s), Some(index), Some(w)) = (found.session, found.index, found.window) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let m = &server.model;
    if !args.flag('k')
        && m.windows[&w]
            .panes
            .panes
            .iter()
            .any(|p| m.panes[p].io.is_some())
    {
        let name = &m.sessions[&s].name;
        return Err(EdgeError::Message(format!(
            "respawn window failed: window {name}:{index} still active"
        ))
        .into());
    }
    let Some((wp, gone)) = server.model.reset_window_panes(w) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    for pane in gone {
        server.close_pane_io(pane);
    }
    respawn(server, item, args, (s, index, w, wp), "window")?;
    // spawn.c:228-238: session_select, then the group follows.
    server.model.set_current(s, index);
    server.model.group_sync_from(s);
    server.model.redraw(Redraw::Window(w));
    server.recalculate_sizes();
    Ok(())
}
