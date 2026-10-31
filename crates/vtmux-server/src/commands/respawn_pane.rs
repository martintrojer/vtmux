//! `respawn-pane` (cmd-respawn-pane.c:46-100, spawn.c `spawn_pane` with
//! `SPAWN_RESPAWN`): run a new process in an existing pane. Without `-k`
//! the pane must be dead; vtmux has no remain-on-exit yet, so a live pane
//! is the only kind there is. The pane keeps its ID, its command unless a
//! new one is given, its directory unless `-c`, and its shell. Empty panes
//! (`-E`) are not done yet.

use std::path::PathBuf;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

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
    // spawn.c:288-305: -c is expanded against the target session and made
    // absolute from the client's directory; otherwise the pane's own.
    let cwd = args.value('c').map(|c| {
        let session_only = Found {
            session: Some(s),
            ..Found::default()
        };
        let c = expand(server, session_only, c);
        if c.starts_with('/') {
            PathBuf::from(c)
        } else {
            let mut base = item.cwd.clone().into_os_string();
            if !c.is_empty() {
                base.push("/");
                base.push(&c);
            }
            PathBuf::from(base)
        }
    });
    let pane = &server.model.panes[&wp];
    if pane.io.is_some() && !args.flag('k') {
        let session = &server.model.sessions[&s].name;
        let pidx = server.model.pane_index(wp).unwrap_or(0);
        return Err(EdgeError::Message(format!(
            "respawn pane failed: pane {session}:{index}.{pidx} still active"
        ))
        .into());
    }
    if args.flag('E') {
        return Err(EdgeError::NotImplemented("respawn-pane -E".into()).into());
    }
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
    server
        .respawn_pane(&PaneSpawn {
            pane: wp,
            window: w,
            session: s,
            session_env: &session_env,
            extra_env: &extra_env,
            client_path: item.client.and(client_path(&item.env)),
            argv: command,
            cwd,
            rect,
            shell,
        })
        .map_err(|e| EdgeError::Message(format!("respawn pane failed: fork failed: {e}")))?;
    // cmd-respawn-pane.c:92: PANE_REDRAW.
    server.model.redraw_panes.insert(wp);
    Ok(())
}
