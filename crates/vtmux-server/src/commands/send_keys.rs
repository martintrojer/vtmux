//! `send-keys` (cmd-send-keys.c:155-268) for keys and literal text, `-K`,
//! and `-N`, `-X` and keys for a pane in any mode ([`PaneMode`]); `-c`
//! names the client whose read-only flag and `-K` keys count. `-M` and
//! `-R` are later.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::keys::Key as CoreKey;

use vtmux_core::ids::PaneId;

use super::Item;
use crate::cmdq::{End, NewBatch};
use crate::jobs::JobSpec;
use crate::keys::send_keys_bytes;
use crate::mode::PaneMode;
use crate::mode::copy::{Action, CommandError, Copied, Pipe};
use crate::server::{ClientId, Server};
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    // CMD_CLIENT_CFLAG|CMD_CLIENT_CANFAIL: the `-c` client, or the
    // current one; none is not an error.
    let tc = target_client(server, item, args);
    // cmd-send-keys.c:178-181.
    if !args.flag('X')
        && tc.is_some_and(|c| server.clients.get(&c).is_some_and(|c| c.flags.read_only))
    {
        return Err(EdgeError::Message("client is read-only".into()).into());
    }
    if args.flag('K') {
        inject(server, args, tc);
        return Ok(());
    }
    for flag in ['M', 'R'] {
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
    let top = server
        .model
        .panes
        .get(&p)
        .and_then(|pane| pane.modes.first());
    let in_mode = top.is_some();
    let takes_commands = top.is_some_and(PaneMode::takes_commands);
    // The repeat count of a mode command (cmd-send-keys.c:191-197): a mode
    // with no command callback is "not in a mode".
    if args.flag('N') && in_mode && (args.flag('X') || args.positionals.is_empty()) {
        if !takes_commands {
            return Err(EdgeError::Message("not in a mode".into()).into());
        }
        let n = u32::try_from(repeat).unwrap_or(u32::MAX);
        server.with_mode(p, |m, _| m.set_prefix(n));
    }
    if args.flag('X') {
        if !takes_commands {
            return Err(EdgeError::Message("not in a mode".into()).into());
        }
        return mode_command(server, args, found, p, tc);
    }
    if in_mode {
        mode_keys(server, item, args, p, repeat, tc);
        return Ok(());
    }
    let literal = args.flag('l');
    let hex = args.flag('H');
    let Some(modes) = server.model.key_modes(p) else {
        return Ok(());
    };
    let Some(pane) = server.model.panes.get_mut(&p) else {
        return Ok(());
    };
    let mut bytes = Vec::new();
    for _ in 0..repeat {
        for arg in &args.positionals {
            // `utf8_fromcstr` keeps a byte that is not UTF-8 as a one-byte
            // character, and the pane gets it as it is.
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
fn mode_command(
    server: &mut Server,
    args: &Args,
    found: Found,
    p: PaneId,
    tc: Option<ClientId>,
) -> Result<(), CmdError> {
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
    // The buffer prefix and the pipe command are formats expanded before
    // the command runs, while the pane's mode still shows its cursor and
    // selection (`format_single` in the command, window-copy.c:1396-1412).
    if let Some((name, rest)) = words.split_first() {
        let expanded =
            crate::mode::copy::expand_args(name, rest, &|t| super::expand(server, found, t));
        if expanded != rest {
            words.truncate(1);
            words.extend(expanded);
        }
    }
    let Some((name, rest)) = words.split_first() else {
        // window_copy_command: no command, nothing to do.
        return Ok(());
    };
    // `c` of `wme->mode->command`: the target client, whose read-only
    // flag a mode command checks itself (window-copy.c:3925-3933).
    let read_only = tc.is_some_and(|c| server.clients.get(&c).is_some_and(|c| c.flags.read_only));
    let result = server
        .with_mode(p, |m, env| m.command(env, name, rest, read_only))
        .ok_or_else(|| CmdError::from(EdgeError::Message("not in a mode".into())))?;
    let outcome = match result {
        Ok(o) => o,
        Err(CommandError::ReadOnly) => {
            // status_message_set(c, -1, 1, 0, 0, "client is read-only"):
            // the command itself succeeds.
            if let Some(c) = tc {
                server.status_message_set(c, "client is read-only".to_owned());
            }
            return Ok(());
        }
        Err(CommandError::NotImplemented(c)) => {
            return Err(EdgeError::NotImplemented(format!("copy mode command {c}")).into());
        }
    };
    // window_copy_pipe_run runs before window_copy_copy_buffer.
    if let Some(pipe) = outcome.pipe {
        run_pipe(server, found, pipe);
    }
    if let Some(copied) = outcome.copied {
        store_copy(server, p, copied);
    }
    if outcome.refresh {
        server.copy_refresh(p, true);
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
fn store_copy(server: &mut Server, p: PaneId, copied: Copied) {
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
        server.model.fire_pane("pane-set-clipboard", p);
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
    server.model.paste.add_prefixed(
        copied.data,
        copied.prefix.as_deref().unwrap_or("buffer"),
        limit,
    );
}

/// `window_copy_pipe_run` (window-copy.c:6125-6142): the selection to a
/// job's stdin, the command given or else `copy-command`. The server does
/// not wait for it (`JOB_NOWAIT`), and its output and status are dropped.
fn run_pipe(server: &mut Server, found: Found, pipe: Pipe) {
    let command = if pipe.command.is_empty() {
        server
            .model
            .options
            .get(vtmux_core::options::TreeId::Server, "copy-command")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default()
    } else {
        pipe.command
    };
    if command.is_empty() {
        return;
    }
    let spec = JobSpec {
        command,
        session: found.session,
        cwd: None,
        input: pipe.data,
        show_stderr: false,
        nowait: true,
        update: None,
    };
    // A job that cannot start is silently dropped, as in tmux.
    let _ = server.job_run(&spec, Box::new(|_, _, _| {}));
}

/// Keys for a pane in a mode go through the mode's key table, not to the
/// program (`cmd_send_keys_inject_key`, cmd-send-keys.c:93-104): a bound
/// key runs its commands after this command, any other key does nothing.
/// The commands run with the command's own current target, where tmux
/// gives them the send-keys target.
fn mode_keys(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    p: PaneId,
    repeat: i64,
    tc: Option<ClientId>,
) {
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
    let table = server.model.mode_key_table(p);
    let mut after = item.step;
    for _ in 0..repeat {
        for &key in &keys {
            // A mode with no key table takes the key itself
            // (`window_pane_key`, cmd-send-keys.c:97-98).
            let Some(table) = table else {
                server.mode_key(p, tc, key, false);
                continue;
            };
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
fn inject(server: &mut Server, args: &Args, tc: Option<ClientId>) {
    let Some(tc) = tc else {
        return;
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
}

/// `cmd_find_client` for `-c` (a miss is quiet: `CMD_CLIENT_CANFAIL`), else
/// the current client.
fn target_client(server: &Server, item: &Item, args: &Args) -> Option<ClientId> {
    server.find_client(item.client, item.origin, args.value('c'))
}

#[cfg(test)]
mod tests {
    use vtmux_core::cmd::parse_argv_bytes;

    use super::*;
    use crate::mode::Mode;
    use crate::mode::prompt::tests::setup;

    fn run(server: &mut Server, p: PaneId, words: &[&str]) -> Result<(), String> {
        let mut argv: Vec<Vec<u8>> = vec![b"send-keys".to_vec()];
        argv.extend(words.iter().map(|w| w.as_bytes().to_vec()));
        let calls = parse_argv_bytes(&argv).map_err(|e| e.to_string())?;
        let mut item = Item::new(None, std::path::PathBuf::new());
        let found = Found {
            pane: Some(p),
            ..Found::default()
        };
        exec(server, &mut item, &calls[0].args, found).map_err(|e| e.to_string())
    }

    /// cmd-send-keys.c:191-203 for a mode with no command callback:
    /// `-X`, and `-N` with `-X` or with no keys, are "not in a mode"; keys
    /// and `-N` with keys are not, and a mode with no key table gets them
    /// from its own handler (a probe takes none).
    #[test]
    fn a_mode_without_a_command_callback_is_not_in_a_mode() {
        let (mut server, p, _, _, dir) = setup();
        server.reset_mode_all(p);
        server
            .model
            .panes
            .get_mut(&p)
            .unwrap()
            .modes
            .push(Mode::Probe);
        let not = Err("not in a mode".to_owned());
        assert_eq!(run(&mut server, p, &["-X", "cancel"]), not);
        assert_eq!(run(&mut server, p, &["-N", "3", "-X", "cancel"]), not);
        assert_eq!(run(&mut server, p, &["-N", "3"]), not);
        assert_eq!(run(&mut server, p, &["-X"]), not);
        assert_eq!(run(&mut server, p, &["-N", "2", "a", "b"]), Ok(()));
        assert_eq!(run(&mut server, p, &["a"]), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Copy mode behaves as before through the trait: `-N` sets its repeat
    /// count for the next command, `-X` with no command does nothing, an
    /// unknown command is nothing, and `cancel` leaves the mode.
    #[test]
    fn copy_mode_still_takes_commands() {
        let (mut server, p, _, _, dir) = setup();
        assert_eq!(run(&mut server, p, &["-N", "3", "-X"]), Ok(()));
        assert_eq!(
            server.model.panes[&p].modes[0].as_copy().map(|m| m.prefix),
            Some(3)
        );
        assert_eq!(run(&mut server, p, &["-X"]), Ok(()));
        assert_eq!(run(&mut server, p, &["-X", "no-such-command"]), Ok(()));
        assert_eq!(run(&mut server, p, &["-X", "cancel"]), Ok(()));
        assert!(server.model.panes[&p].modes.is_empty());
        // No mode: -X is "not in a mode" again.
        assert_eq!(
            run(&mut server, p, &["-X", "cancel"]),
            Err("not in a mode".to_owned())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
