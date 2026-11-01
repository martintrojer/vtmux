//! `clock-mode` (cmd-copy-mode.c:43-54, 81-85): enter clock mode.

use vtmux_core::cmd::CmdError;
use vtmux_core::error::EdgeError;

use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, found: Found) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    // window_pane_set_mode: a mode that is not stacked goes first
    // (window.c:1755-1756).
    if server
        .model
        .panes
        .get(&p)
        .and_then(|x| x.modes.first())
        .is_some_and(crate::mode::Mode::no_stack)
    {
        server.reset_mode(p);
    }
    server.model.set_clock_mode(p);
    Ok(())
}
