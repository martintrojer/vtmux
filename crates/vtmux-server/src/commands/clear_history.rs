//! `clear-history` (cmd-capture-pane.c:424-431): the pane leaves its modes
//! and loses its history; `-H` also forgets its hyperlinks.
//!
//! libghostty-vt keeps hyperlinks per page and drops them with the rows
//! that hold them, so `-H` has nothing more to reset: vtmux does not
//! expose the link table (`capture-pane -H` is not implemented either).

use vtmux_core::cmd::CmdError;
use vtmux_core::error::EdgeError;

use crate::model::Redraw;
use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, found: Found) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    server.reset_mode_all(p);
    let Some(pane) = server.model.panes.get_mut(&p) else {
        return Ok(());
    };
    pane.term.clear_history();
    let w = pane.window;
    server.model.redraw(Redraw::Window(w));
    Ok(())
}
