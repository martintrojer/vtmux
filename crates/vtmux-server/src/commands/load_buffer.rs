//! `load-buffer` (cmd-load-buffer.c:90-130): a file, or the client's
//! stdin for `-`, into a paste buffer; `-w` also sends it to the target
//! client's clipboard (OSC 52).

use vtmux_core::cmd::{Args, CmdError};

use super::Item;
use super::file_io::{self, Then};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    // CMD_CLIENT_TFLAG|CMD_CLIENT_CANFAIL: resolved before the command.
    let clip = args
        .flag('w')
        .then(|| server.find_client(item.client, item.origin, args.value('t')))
        .flatten();
    let then = Then::Load {
        name: args.value('b').map(str::to_owned),
        clip,
    };
    let target = super::expand(server, found, &args.positional_text(0).unwrap_or_default());
    let path = (target != "-").then(|| file_io::path(server, item, &target));
    file_io::read(server, item, path.as_deref(), then).map_err(Into::into)
}
