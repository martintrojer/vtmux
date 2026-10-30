//! `rename-window` (cmd-rename-window.c:44-68).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::options::{TreeId, Value};

use super::{Item, expand};
use crate::names::{check_name, clean_name};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let Some(w) = found.window else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let name = expand(server, found, &args.positional_text(0).unwrap_or_default());
    if !check_name(&name) {
        return Err(EdgeError::InvalidWindowName(name).into());
    }
    let name = clean_name(&name);
    if let Some(window) = server.model.windows.get_mut(&w) {
        window.name.clone_from(&name);
    }
    server
        .model
        .options
        .set_table_value(TreeId::Window(w), "automatic-rename", Value::Flag(false));
    // window_set_name fires window-renamed whether or not it changed.
    server.control_window_renamed(w, &name);
    Ok(())
}
