//! `send-keys` (cmd-send-keys.c:155-268) for keys and literal text; modes,
//! `-K`, `-M`, `-X` and `-R` are M1.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::keys::{KeyModes, send_keys_bytes};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    for flag in ['K', 'M', 'X', 'R', 'F', 'c'] {
        if args.flag(flag) {
            return Err(EdgeError::NotImplemented(format!("send-keys -{flag}")).into());
        }
    }
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    let repeat = if args.flag('N') {
        super::number_expanded(server, found, args.value('N'), 1, i64::from(u32::MAX)).map_err(
            |cause| EdgeError::BadNumber {
                what: "repeat count",
                cause,
            },
        )?
    } else {
        1
    };
    let literal = args.flag('l');
    let hex = args.flag('H');
    let Some(pane) = server.model.panes.get_mut(&p) else {
        return Ok(());
    };
    let modes = KeyModes {
        app_cursor: pane.term.app_cursor(),
    };
    let mut bytes = Vec::new();
    for _ in 0..repeat {
        for arg in &args.positionals {
            if hex {
                // -H: each argument is one hex byte; bad ones are skipped.
                if let Ok(b) = u8::from_str_radix(arg, 16)
                    && !arg.is_empty()
                {
                    bytes.push(b);
                }
                continue;
            }
            bytes.extend(send_keys_bytes(arg, literal, modes));
        }
    }
    if let Some(io) = pane.io.as_mut() {
        io.write(&bytes);
    }
    server.pane_output_queued(p);
    Ok(())
}
