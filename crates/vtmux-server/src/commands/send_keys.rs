//! `send-keys` (cmd-send-keys.c:155-268) for keys and literal text; modes,
//! `-K`, `-M`, `-X` and `-R` are M1.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::keys::Key as CoreKey;

use super::Item;
use crate::keys::{KeyModes, send_keys_bytes};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    if args.flag('K') {
        return inject(server, item, args);
    }
    for flag in ['M', 'X', 'R', 'F', 'c'] {
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
            let arg = &*arg.text();
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
    // window_pane_key (window.c:2095): nothing reaches an input-off pane.
    if let Some(io) = pane.io.as_mut().filter(|_| !pane.input_off) {
        io.write(&bytes);
    }
    server.pane_output_queued(p);
    Ok(())
}

/// `send-keys -K`: each key goes to the target client as if typed
/// (cmd-send-keys.c:75-92), handled after this command. No target client
/// is not an error (`CMD_CLIENT_CANFAIL`). Names that are not keys are
/// sent character by character (`cmd_send_keys_inject_string`).
fn inject(server: &mut Server, item: &Item, args: &Args) -> Result<(), CmdError> {
    if args.value('c').is_some() {
        return Err(EdgeError::NotImplemented("send-keys -c".into()).into());
    }
    let Some(tc) = server.target_client(item.client, item.origin) else {
        return Ok(());
    };
    let repeat = if args.flag('N') {
        args.value('N')
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(1)
    } else {
        1
    };
    for _ in 0..repeat {
        for arg in &args.positionals {
            let arg = &*arg.text();
            let key = (!args.flag('l'))
                .then(|| CoreKey::from_name(arg))
                .flatten()
                .filter(|k| *k != CoreKey::NONE);
            match key {
                Some(k) => server.injected_keys.push((tc, k)),
                None => server
                    .injected_keys
                    .extend(arg.chars().map(|c| (tc, CoreKey::from_char(c)))),
            }
        }
    }
    Ok(())
}
