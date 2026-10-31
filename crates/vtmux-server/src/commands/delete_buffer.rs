//! `delete-buffer` (cmd-set-buffer.c:60-80).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use crate::server::Server;

pub fn exec(server: &mut Server, args: &Args) -> Result<(), CmdError> {
    let bufname = args.value('b');
    let name = match bufname {
        Some(n) if server.model.paste.get(n).is_some() => n.to_owned(),
        Some(n) => return Err(EdgeError::Message(format!("unknown buffer: {n}")).into()),
        None => match server.model.paste.top() {
            Some(b) => b.name.clone(),
            None => return Err(EdgeError::Message("no buffer".into()).into()),
        },
    };
    server.model.paste.remove(&name);
    Ok(())
}
