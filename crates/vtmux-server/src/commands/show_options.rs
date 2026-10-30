//! `show-options` and `show-window-options` (cmd-show-options.c).

use vtmux_core::cmd::CommandCall;
use vtmux_core::options::{OptionError, apply_show};

use super::Item;
use super::set_option::{Host, option_target};
use crate::server::{GlobalEnv, Server};
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    call: &CommandCall,
    found: Found,
) -> Result<(), OptionError> {
    let lines = apply_show(
        &server.model.options,
        call,
        &option_target(found),
        &mut Host {
            model: &server.model,
            info: &server.info,
            env: GlobalEnv(&mut server.global_env),
            found,
        },
    )?;
    for line in lines {
        item.print(&line);
    }
    Ok(())
}
