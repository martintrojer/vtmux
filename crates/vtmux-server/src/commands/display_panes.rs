//! `display-panes` (cmd-choose-tree.c:100-152): enter `panes-mode` on the
//! target pane ([`crate::mode::panes`]).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &Item,
    args: &Args,
    source: Found,
    found: Found,
) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    crate::mode::panes::enter(server, p, args, found, source, item.source.clone())
}
