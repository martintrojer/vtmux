//! `unlink-window` (cmd-kill-window.c:75-82).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::model::Redraw;
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
    // session_is_linked: the window has another winlink (no groups yet).
    if !args.flag('k') && server.model.window_links(w).len() <= 1 {
        return Err(EdgeError::Message("window only linked to one session".into()).into());
    }
    let links = server.control_links();
    // server_unlink_window: the session's only window cannot be detached,
    // so the session goes with it.
    let panes = if server.model.detach_window(s, index) {
        server.model.remove_session(s)
    } else {
        server.model.redraw(Redraw::Session(s));
        server.model.drop_if_unlinked(w)
    };
    for pane in panes {
        server.close_pane_io(pane);
    }
    server.control_links_changed(&links);
    server.recalculate_sizes();
    Ok(())
}
