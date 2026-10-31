//! `save-buffer` and `show-buffer` (cmd-save-buffer.c:66-130).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::{Item, file_io};
use crate::server::Server;
use crate::target::Found;

/// Both commands; `show` is `show-buffer`.
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
    show: bool,
) -> Result<(), CmdError> {
    let pb = match args.value('b') {
        None => server
            .model
            .paste
            .top()
            .ok_or_else(|| EdgeError::Message("no buffers".into()))?,
        Some(n) => server
            .model
            .paste
            .get(n)
            .ok_or_else(|| EdgeError::Message(format!("no buffer {n}")))?,
    };
    let data = pb.data.clone();
    let attached = file_io::is_attached(server, item);

    let target = if show {
        if attached {
            // cmdq_print_data: one line to a control client.
            item.print(&String::from_utf8_lossy(&data));
            return Ok(());
        }
        "-".to_owned()
    } else {
        super::expand(server, found, &args.positional_text(0).unwrap_or_default())
    };
    if target == "-" {
        if attached || item.client.is_none() {
            return Err(EdgeError::Message("Bad file descriptor: -".into()).into());
        }
        item.print_raw(data);
        return Ok(());
    }
    let path = file_io::path(server, item, &target);
    file_io::write(&path, &data, args.flag('a'))
        .map_err(|e| EdgeError::Message(file_io::describe(&e, &path)).into())
}
