//! `clock-mode` (cmd-copy-mode.c:43-54, 81-85): enter clock mode.

use vtmux_core::cmd::CmdError;
use vtmux_core::error::EdgeError;

use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, found: Found) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    server.model.set_clock_mode(p);
    Ok(())
}
