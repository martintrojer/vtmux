//! `clear-history` (cmd-capture-pane.c:424-431): the pane leaves its modes
//! and loses its history; `-H` also forgets the hyperlinks of the screen
//! in use ([`vtmux_vt::PaneTerm::reset_hyperlinks`], D-018).

use std::time::Instant;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use crate::model::Redraw;
use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, args: &Args, found: Found) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    server.reset_mode_all(p);
    let Some(pane) = server.model.panes.get_mut(&p) else {
        return Ok(());
    };
    pane.term.clear_history();
    // grid_clear_history, then screen_reset_hyperlinks. A screen rebuilt
    // holds its history as rows again: the compressor takes it up.
    if args.flag('H') && pane.term.reset_hyperlinks() {
        pane.compress.restored(&pane.term, Instant::now());
    }
    let w = pane.window;
    server.model.redraw(Redraw::Window(w));
    Ok(())
}
