//! `unlink-window` (cmd-kill-window.c:75-82).

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
    let (Some(s), Some(index), Some(w)) = (found.session, found.index, found.window) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    // session_is_linked: the window has links beyond one per member of
    // the session's group.
    if !args.flag('k') && !server.model.linked_outside(s, w) {
        return Err(EdgeError::Message("window only linked to one session".into()).into());
    }
    // server_unlink_window: the session's only window cannot be detached,
    // so the session (and its group) goes with it.
    let panes = if server.model.detach_window(s, index) {
        server.model.remove_session_group(s)
    } else {
        server.model.redraw_group(s);
        server.model.drop_if_unlinked(w)
    };
    for pane in panes {
        server.close_pane_io(pane);
    }
    server.recalculate_sizes();
    Ok(())
}
