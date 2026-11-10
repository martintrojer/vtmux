//! `respawn-pane` (cmd-respawn-pane.c:46-100, spawn.c `spawn_pane` with
//! `SPAWN_RESPAWN`): run a new process in an existing pane. Without `-k`
//! the pane must be dead (kept by remain-on-exit, its PTY closed). The
//! pane keeps its ID, its command unless a new one is given, its directory
//! unless `-c`, and its shell. With `-E` the command and directory are
//! stored and nothing runs (an empty pane).

use std::path::PathBuf;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, SessionId, WindowId};

use super::{Item, expand};
use crate::server::{PaneSpawn, Server, client_path};
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let (Some(s), Some(index), Some(w), Some(wp)) =
        (found.session, found.index, found.window, found.pane)
    else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let pane = &server.model.panes[&wp];
    if pane.io.is_some() && !args.flag('k') {
        let session = &server.model.sessions[&s].name;
        let pidx = server.model.pane_index(wp).unwrap_or(0);
        return Err(EdgeError::Message(format!(
            "respawn pane failed: pane {session}:{index}.{pidx} still active"
        ))
        .into());
    }
    respawn(server, item, args, (s, index, w, wp), "pane")?;
    // cmd-respawn-pane.c:92: PANE_REDRAW.
    server.model.redraw_panes.insert(wp);
    Ok(())
}

/// The part of `spawn_pane` (spawn.c:312-345) both respawn commands share:
/// stop the pane's process and start the new command in it. `what` names
/// the command in the error (`respawn pane failed: ...`).
pub(super) fn respawn(
    server: &mut Server,
    item: &Item,
    args: &Args,
    (s, index, w, wp): (SessionId, u32, WindowId, PaneId),
    what: &str,
) -> Result<(), CmdError> {
    let cwd = start_dir(server, item, args, s);
    let pane = &server.model.panes[&wp];
    let command: Vec<String> = if args.positionals.is_empty() {
        pane.argv.clone()
    } else {
        args.positionals
            .iter()
            .map(|p| p.text().into_owned())
            .collect()
    };
    let cwd = cwd.unwrap_or_else(|| pane.cwd.clone());
    let shell = Some(pane.shell.clone());
    let rect = pane.rect;
    let extra_env: Vec<String> = args.values('e').map(str::to_owned).collect();
    let session_env = server.model.sessions[&s].env.clone();
    let session_chars = server.model.sessions[&s].control_chars.clone();
    server
        .respawn_pane(&PaneSpawn {
            pane: wp,
            window: w,
            session: s,
            session_env: &session_env,
            control_chars: session_chars.as_deref(),
            extra_env: &extra_env,
            client_path: item.client.and(client_path(&item.env)),
            argv: command,
            cwd,
            rect,
            cell_pixels: server.model.windows[&w].cell_pixels,
            shell,
            // cmd-respawn-pane.c:78-79.
            empty: args.flag('E'),
        })
        .map_err(|e| {
            EdgeError::Message(format!(
                "respawn {what} failed: {}",
                crate::server::spawn_failed(&e)
            ))
        })?;
    // spawn_pane (spawn.c:592): pane-created with created_respawn, for
    // respawn-window too (its spawn_window ends in spawn_pane).
    server.fire_pane_created(s, index, wp, args.flag('E'), true);
    Ok(())
}

/// `-c` as spawn.c:288-305 reads it: expanded against the target session
/// and made absolute from the client's directory; `None` keeps the pane's
/// own.
pub(super) fn start_dir(
    server: &Server,
    item: &Item,
    args: &Args,
    s: SessionId,
) -> Option<PathBuf> {
    args.value('c').map(|c| {
        let session_only = Found {
            session: Some(s),
            ..Found::default()
        };
        let c = expand(server, session_only, c);
        super::start_path(&server.client_cwd(item.client, Some(s)), &c)
    })
}
