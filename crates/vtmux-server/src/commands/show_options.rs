//! `show-options` and `show-window-options` (cmd-show-options.c).

use vtmux_core::cmd::CommandCall;
use vtmux_core::options::{OptionError, apply_show};

use super::Item;
use super::set_option::{Host, client_parts, option_target};
use crate::server::{GlobalEnv, Server};
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    call: &CommandCall,
    found: Found,
) -> Result<(), OptionError> {
    let (clients, client) = client_parts(server, item, found);
    let lines = apply_show(
        &server.model.options,
        call,
        &option_target(found),
        &mut Host {
            model: &server.model,
            info: &server.info,
            env: GlobalEnv::new(&mut server.global_env, &server.model, &server.info),
            found,
            clients,
            client,
            formats: server.cmd_formats.clone(),
        },
    )?;
    for line in lines {
        item.print(&line);
    }
    Ok(())
}
