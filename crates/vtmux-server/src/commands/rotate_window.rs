//! `rotate-window` (cmd-rotate-window.c:44-114): shift the panes one place
//! along the window's cells, `-D` the last pane to the front, otherwise
//! (`-U`, the default; `-D` wins when both are given) the first to the
//! back. `-Z` keeps a zoomed window zoomed.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::model::Redraw;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let Some(w) = found.window else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let m = &mut server.model;
    let zoomed = m.push_zoom(w, false, args.flag('Z'));
    m.rotate_panes(w, args.flag('D'));
    // cmd-rotate-window.c:109-110: the commands after this one start from
    // the newly active pane.
    item.new_current = Some(Found {
        pane: m.windows.get(&w).and_then(|x| x.panes.active),
        ..found
    });
    m.pop_zoom(w, zoomed);
    m.redraw(Redraw::Window(w));
    Ok(())
}
