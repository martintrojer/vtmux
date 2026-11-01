//! The key commands over the server's tables: `bind-key` (cmd-bind-key.c),
//! `unbind-key` (cmd-unbind-key.c), `list-keys` (cmd-list-keys.c),
//! `send-prefix` (cmd-send-keys.c:221-227) and `switch-client`
//! (cmd-switch-client.c). Parsing, storing and printing are
//! `vtmux_core::keys`; this is the edge.

use vtmux_core::cmd::{Args, CmdError, CommandCall};
use vtmux_core::error::EdgeError;
use vtmux_core::keys::{CmdFailed, Key};
use vtmux_core::options::{OptionValue, TreeId, Value};

use super::Item;
use crate::server::{ClientId, GlobalEnv, Server};
use crate::target::{self, Found};

fn failed(e: CmdFailed) -> CmdError {
    match e.message {
        Some(m) => EdgeError::Message(m).into(),
        None => CmdError::Quiet,
    }
}

/// After a table is removed, a client in it moves to a table of the same
/// name, created empty (`key_bindings_remove_table` ->
/// `server_client_set_key_table(c, NULL)`, key-bindings.c:335-338).
fn keep_client_tables(server: &mut Server) {
    let tables: Vec<String> = server
        .clients
        .values()
        .map(|c| c.keys.table.clone())
        .collect();
    let ids: Vec<ClientId> = server.clients.keys().copied().collect();
    for (id, table) in ids.into_iter().zip(tables) {
        if server.key_bindings.table(&table).is_none() {
            server.set_key_table(id, None);
        }
    }
}

/// `bind-key` and `unbind-key`.
pub fn bind_or_unbind(server: &mut Server, call: &CommandCall) -> Result<(), CmdError> {
    let mut bindings = std::mem::take(&mut server.key_bindings);
    let result = bindings
        .execute(
            call,
            &mut GlobalEnv::new(&mut server.global_env, &server.model, &server.info),
        )
        .expect("a key command");
    server.key_bindings = bindings;
    keep_client_tables(server);
    result.map_err(failed)
}

fn global_prefix(server: &Server) -> Key {
    match server
        .model
        .options
        .get(TreeId::GlobalSession, "prefix")
        .map(|e| &e.value)
    {
        Some(OptionValue::Single(Value::Key(k))) => *k,
        _ => Key::NONE,
    }
}

/// `list-keys`. With `-1`, an attached terminal client gets the line as a
/// status message instead (cmd-list-keys.c:238).
pub fn list_keys(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let tc = server.target_client(item.client, item.origin);
    let listed = {
        let ctx = server.format_ctx(found, tc);
        server
            .key_bindings
            .list_keys(args, global_prefix(server), &ctx)
            .map_err(failed)?
    };
    let to_status = listed.single
        && tc.is_some_and(|c| !server.is_control(c) && server.client_session(c).is_some());
    for line in listed.printed() {
        if to_status {
            if let Some(c) = tc {
                server.status_message_set(c, line.to_owned());
            }
        } else {
            item.print(line);
        }
    }
    Ok(())
}

/// `send-prefix [-2]`: the session's prefix key to the pane, as
/// `send-keys` would send it.
pub fn send_prefix(
    server: &mut Server,
    item: &Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let (Some(s), Some(p)) = (found.session, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let name = if args.flag('2') { "prefix2" } else { "prefix" };
    let key = match server
        .model
        .options
        .get(TreeId::Session(s), name)
        .map(|e| &e.value)
    {
        Some(OptionValue::Single(Value::Key(k))) => *k,
        _ => Key::NONE,
    };
    if let Some(k) = crate::keys::Key::from_core(key) {
        let tc = server.target_client(item.client, item.origin);
        server.forward_key(p, tc, &k);
    }
    Ok(())
}

/// `switch-client` (cmd-switch-client.c:48-163): `-T` switches the key
/// table, `-n`/`-p`/`-l` and `-t` the session, `-c` names the client and
/// `-r` toggles its read-only flag. `-F` is in the usage but tmux 3.8
/// reads nothing from it.
pub fn switch_client(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    // CMD_CLIENT_CFLAG: the target client must exist before anything else.
    let tc = super::command_prompt::target_client(server, item, args.value('c'))?;

    // -t: a pane-ish target (`:`, `.`, `%`, or `=`) or a session that
    // prefers unattached ones (cmd-switch-client.c:66-74).
    let tflag = args.target('t');
    let pane_like = args
        .value('t')
        .is_some_and(|t| t.contains([':', '.', '%']) || t == "=");
    let kind = if pane_like {
        vtmux_core::cmd::TargetKind::Pane
    } else {
        vtmux_core::cmd::TargetKind::Session
    };
    let found = target::resolve(
        &server.model,
        item.origin,
        tflag.as_ref(),
        kind,
        vtmux_core::cmd::FindFlags::NONE,
    )?;

    // -r: toggle read-only. A read-only client may only be changed by its
    // own user (cmd-switch-client.c:82-95); every client here is ours.
    if args.flag('r')
        && let Some(c) = server.clients.get_mut(&tc)
    {
        let read_only = !c.flags.read_only;
        c.flags.read_only = read_only;
        c.flags.ignore_size = read_only;
        server.recalculate_sizes();
    }

    if let Some(table) = args.value('T') {
        if server.key_bindings.table(table).is_none() {
            return Err(EdgeError::Message(format!("table {table} doesn't exist")).into());
        }
        if let Some(c) = server.clients.get_mut(&tc) {
            table.clone_into(&mut c.keys.table);
        }
        return Ok(());
    }

    let order = super::list_sessions::order(args.value('O'))?;
    let current = server.client_session(tc);
    let s = if args.flag('n') || args.flag('p') {
        let next = args.flag('n');
        let list = super::list_sessions::sorted(server, order, args.flag('r'));
        let at = current.and_then(|c| list.iter().position(|s| *s == c));
        let found = at.map(|i| {
            if next {
                list[(i + 1) % list.len()]
            } else {
                list[(i + list.len() - 1) % list.len()]
            }
        });
        found.ok_or_else(|| {
            EdgeError::Message(format!(
                "can't find {} session",
                if next { "next" } else { "previous" }
            ))
        })?
    } else if args.flag('l') {
        server
            .clients
            .get(&tc)
            .and_then(|c| c.last_session)
            .filter(|s| server.model.sessions.contains_key(s))
            .ok_or_else(|| EdgeError::Message("can't find last session".into()))?
    } else {
        if item.client.is_none() {
            return Ok(());
        }
        let s = found.session.ok_or(EdgeError::NoCurrentTarget)?;
        if let (Some(w), Some(p)) = (found.window, found.pane)
            && server
                .model
                .windows
                .get(&w)
                .is_some_and(|x| x.active() != p)
        {
            let zflag = args.flag('Z');
            let visible = server.model.windows[&w].is_visible(p);
            let was = !visible && server.model.push_zoom(w, false, zflag);
            server.model.set_active(w, p);
            if !visible {
                server.model.pop_zoom(w, was);
            }
        }
        if let Some(index) = found.index {
            server.model.set_current(s, index);
        }
        s
    };
    if !args.flag('E') {
        server.update_session_environ(tc, s);
    }
    server.switch_session(tc, s);
    if !item.repeat {
        server.set_key_table(tc, None);
    }
    Ok(())
}
