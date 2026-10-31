//! `set-buffer` (cmd-set-buffer.c:60-142): set, append to or rename a paste
//! buffer. `-w` (OSC 52 to the client) is not implemented.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::server::Server;

pub fn exec(server: &mut Server, item: &Item, args: &Args) -> Result<(), CmdError> {
    let bufname = args.value('b');
    let limit = super::buffer_limit(server);
    let pb = bufname.and_then(|n| server.model.paste.get(n));

    if args.value('n').is_some() {
        let name = match (pb, bufname) {
            (Some(b), _) => b.name.clone(),
            (None, Some(n)) => {
                return Err(EdgeError::Message(format!("unknown buffer: {n}")).into());
            }
            (None, None) => match server.model.paste.top() {
                Some(b) => b.name.clone(),
                None => return Err(EdgeError::Message("no buffer".into()).into()),
            },
        };
        let new = args.value('n').unwrap_or_default();
        return server
            .model
            .paste
            .rename(&name, new)
            .map_err(|cause| EdgeError::Message(cause).into());
    }

    if args.positionals.len() != 1 {
        return Err(EdgeError::Message("no data specified".into()).into());
    }
    let new = args.positional_text(0).unwrap_or_default();
    if new.is_empty() {
        return Ok(());
    }
    let mut data = Vec::new();
    if args.flag('a')
        && let Some(old) = pb
    {
        data.extend_from_slice(&old.data);
    }
    data.extend_from_slice(new.as_bytes());
    server
        .model
        .paste
        .set(data, bufname, limit)
        .map_err(EdgeError::Message)?;
    if args.flag('w')
        && server
            .find_client(item.client, item.origin, args.value('t'))
            .is_some()
    {
        return Err(EdgeError::NotImplemented("set-buffer -w".into()).into());
    }
    Ok(())
}
