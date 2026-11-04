//! `pipe-pane` (cmd-pipe-pane.c:55-182): close the pane's pipe, then open a
//! new one running the command unless it is empty, or `-o` found one open.
//! The pipe itself is in [`crate::pipe`].

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::{EdgeError, strerror};
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, item: &Item, args: &Args, found: Found) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    let Some(pane) = server.model.panes.get(&p) else {
        return Err(EdgeError::PaneNotFound(p.to_string()).into());
    };
    // window_pane_exited: the PTY closed or the program reaped.
    if pane.exited || pane.io.is_none() {
        return Err(EdgeError::Message("target pane has exited".into()).into());
    }
    // cmd-pipe-pane.c:83-86 destroys a pane that was only waiting for its
    // pipe, but a pane that has not exited is never ready to go.
    let had = server.pipe_close(p);
    let command = args.positional_text(0).unwrap_or_default();
    if command.is_empty() {
        return Ok(());
    }
    // -o toggles: a pipe that was open is only closed.
    if args.flag('o') && had {
        return Ok(());
    }
    // Neither -I nor -O is -O.
    let (input, output) = if args.flag('I') {
        (true, args.flag('O'))
    } else {
        (false, true)
    };
    let tc = server.target_client(item.client, item.origin);
    let command = format::expand_time(&command, &server.format_ctx(found, tc));
    server
        .pipe_open(p, &command, input, output)
        .map_err(|e| EdgeError::Message(format!("fork error: {}", strerror(&e))).into())
}
