//! `send-keys` (cmd-send-keys.c:155-268) for keys and literal text, `-K`,
//! and `-X` and keys for a pane in a mode; `-M`, `-R` and `-F` are later.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::keys::Key as CoreKey;

use vtmux_core::ids::PaneId;

use super::Item;
use crate::cmdq::{End, NewBatch};
use crate::keys::{KeyModes, send_keys_bytes};
use crate::mode::copy::{Action, CommandError, Copied};
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
    for flag in ['M', 'R', 'c'] {
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
    let in_mode = server
        .model
        .panes
        .get(&p)
        .is_some_and(|pane| !pane.modes.is_empty());
    // The repeat count of a mode command (cmd-send-keys.c:191-197).
    if args.flag('N') && in_mode && (args.flag('X') || args.positionals.is_empty()) {
        let n = u32::try_from(repeat).unwrap_or(u32::MAX);
        super::copy_mode::with_copy(server, p, |m, _| m.prefix = n);
    }
    if args.flag('X') {
        return mode_command(server, args, found, p);
    }
    if in_mode {
        mode_keys(server, item, args, p, repeat);
        return Ok(());
    }
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

/// `send-keys -X`: a command for the pane's mode (`window_copy_command`).
fn mode_command(server: &mut Server, args: &Args, found: Found, p: PaneId) -> Result<(), CmdError> {
    let mut words: Vec<String> = args
        .positionals
        .iter()
        .map(|a| a.text().into_owned())
        .collect();
    // window_copy_expand_search_string: `-F` makes a search's string a
    // format (expanded for the pane); an empty result searches nothing.
    if args.flag('F')
        && words.first().is_some_and(|w| {
            matches!(
                w.as_str(),
                "search-forward"
                    | "search-backward"
                    | "search-forward-text"
                    | "search-backward-text"
            )
        })
    {
        // The search string is the command's first argument after its own
        // `--` (window_copy_expand_search_string reads `cs->wargs`, which
        // args_parse built with `--` ending the options).
        let at = if words.get(1).is_some_and(|w| w == "--") {
            2
        } else {
            1
        };
        if let Some(arg) = words.get_mut(at) {
            *arg = super::expand(server, found, arg);
        }
    }
    let Some((name, rest)) = words.split_first() else {
        // window_copy_command: no command, nothing to do.
        return if server
            .model
            .panes
            .get(&p)
            .is_some_and(|x| !x.modes.is_empty())
        {
            Ok(())
        } else {
            Err(EdgeError::Message("not in a mode".into()).into())
        };
    };
    let result = super::copy_mode::with_copy(server, p, |m, env| m.command(env, name, rest))
        .ok_or_else(|| CmdError::from(EdgeError::Message("not in a mode".into())))?;
    let outcome = match result {
        Ok(o) => o,
        Err(CommandError::NotImplemented(c)) => {
            return Err(EdgeError::NotImplemented(format!("copy mode command {c}")).into());
        }
    };
    if let Some(copied) = outcome.copied {
        store_copy(server, found, p, copied);
    }
    if outcome.action == Action::Cancel {
        server.reset_mode(p);
    }
    Ok(())
}

/// `window_copy_copy_buffer` and `window_copy_append_selection`
/// (window-copy.c:6096-6123, 6183-6215): to the clipboard of every client
/// showing the pane's window when `set-clipboard` is on or external, then
/// into a new paste buffer, or onto the top one for an append.
fn store_copy(server: &mut Server, found: Found, p: PaneId, copied: Copied) {
    let clip = copied.clip
        && server
            .model
            .number_option(vtmux_core::options::TreeId::Server, "set-clipboard")
            != 0;
    if clip {
        let window = server.model.panes.get(&p).map(|pane| pane.window);
        let clients: Vec<_> = server
            .clients
            .keys()
            .copied()
            .filter(|&c| server.client_window(c).is_some() && server.client_window(c) == window)
            .collect();
        for c in clients {
            server.set_selection(c, &copied.data);
        }
    }
    if !copied.paste {
        return;
    }
    let limit = super::buffer_limit(server);
    if copied.append {
        let mut data = Vec::new();
        let name = server.model.paste.top().map(|top| {
            data.extend_from_slice(&top.data);
            top.name.clone()
        });
        data.extend_from_slice(&copied.data);
        let _ = server.model.paste.set(data, name.as_deref(), limit);
        return;
    }
    // format_single on the prefix argument.
    let prefix = copied.prefix.map(|t| super::expand(server, found, &t));
    server
        .model
        .paste
        .add_prefixed(copied.data, prefix.as_deref().unwrap_or("buffer"), limit);
}

/// Keys for a pane in a mode go through the mode's key table, not to the
/// program (`cmd_send_keys_inject_key`, cmd-send-keys.c:93-104): a bound
/// key runs its commands after this command, any other key does nothing.
/// The commands run with the command's own current target, where tmux
/// gives them the send-keys target.
fn mode_keys(server: &mut Server, item: &mut Item, args: &Args, p: PaneId, repeat: i64) {
    let Some(table) = server.model.mode_key_table(p) else {
        return;
    };
    let mut keys = Vec::new();
    for arg in &args.positionals {
        let arg = &*arg.text();
        let key = (!args.flag('l') && !args.flag('H'))
            .then(|| CoreKey::from_name(arg))
            .flatten()
            .filter(|k| *k != CoreKey::NONE);
        match key {
            Some(k) => keys.push(k),
            None if args.flag('H') => {}
            None => keys.extend(arg.chars().map(CoreKey::from_char)),
        }
    }
    let mut after = item.step;
    for _ in 0..repeat {
        for &key in &keys {
            let Some(commands) = server
                .key_bindings
                .get(table, key)
                .map(|b| b.commands.clone())
            else {
                continue;
            };
            // key_bindings_dispatch(bd, after, ...): a batch of its own
            // right after this command, each key's after the last.
            let mut batch = Item::new(item.client, item.cwd.clone());
            batch.origin = item.origin;
            batch.env.clone_from(&item.env);
            batch.utf8 = item.utf8;
            let batch = NewBatch::new(batch, commands.calls, End::Quiet);
            after = match after {
                Some(step) => server.cmdq_insert_batch(step, batch),
                None => server.cmdq_append(item.client, batch),
            };
        }
    }
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
