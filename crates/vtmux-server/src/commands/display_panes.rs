//! `display-panes` (cmd-choose-tree.c:100-152): enter `panes-mode` on the
//! target pane ([`crate::mode::panes`]).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    source: Found,
    found: Found,
) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    // cmd-choose-tree.c:154-155: window_pane_set_mode's failure (a bad
    // `-d`, cmdq_error'd by window_panes_init) is not the command's, which
    // returns CMD_RETURN_NORMAL: the error shows and the command succeeds.
    if let Err(e) = crate::mode::panes::enter(server, p, args, found, source, item.source.clone()) {
        item.error(server, crate::cmdq::error_bytes(&e));
    }
    Ok(())
}
