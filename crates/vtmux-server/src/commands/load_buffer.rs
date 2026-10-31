//! `load-buffer` from a file (cmd-load-buffer.c:90-130). `-` (the
//! client's stdin) and `-w` (OSC 52) are not implemented.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::{Item, file_io};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let target = super::expand(server, found, &args.positional_text(0).unwrap_or_default());
    if target == "-" {
        return Err(EdgeError::NotImplemented("load-buffer -".into()).into());
    }
    let path = file_io::path(server, item, &target);
    let attached = file_io::is_attached(server, item);
    let data = file_io::read(&path, attached)
        .map_err(|e| EdgeError::Message(file_io::describe(&e, &path)))?;
    if data.is_empty() {
        return Ok(());
    }
    let limit = super::buffer_limit(server);
    server
        .model
        .paste
        .set(data, args.value('b'), limit)
        .map_err(EdgeError::Message)?;
    if args.flag('w')
        && server
            .find_client(item.client, item.origin, args.value('t'))
            .is_some_and(|c| server.client_session(c).is_some())
    {
        return Err(EdgeError::NotImplemented("load-buffer -w".into()).into());
    }
    Ok(())
}
