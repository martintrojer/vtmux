//! `new-window` (cmd-new-window.c:53-187, spawn.c `spawn_window`). Empty
//! panes (`-E`, a single empty command) are not done yet.

use std::path::PathBuf;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::{Item, expand};
use crate::names::{check_name, clean_name};
use crate::server::{NewWindow, Server, client_path};
use crate::target::Found;

/// `NEW_WINDOW_TEMPLATE` (cmd-new-window.c:33).
const TEMPLATE: &str = "#{session_name}:#{window_index}.#{pane_index}";

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let count = args.positionals.len();
    let empty_command = count == 1 && args.positional(0) == Some("");
    if args.flag('E') && count != 0 && !empty_command {
        return Err(EdgeError::Message("command cannot be given for empty pane".into()).into());
    }
    let Some(s) = found.session else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let session_only = Found {
        session: Some(s),
        ..Found::default()
    };
    let name = match args.value('n') {
        Some(n) => {
            let n = expand(server, session_only, n);
            if !check_name(&n) {
                return Err(EdgeError::InvalidWindowName(n).into());
            }
            Some(clean_name(&n))
        }
        None => None,
    };
    if args.flag('S')
        && let Some(index) = select_existing(server, s, found.new_index, name.as_deref())?
    {
        if !args.flag('d') {
            server.model.set_current(s, index);
        }
        return Ok(());
    }
    if args.flag('E') || empty_command {
        return Err(EdgeError::NotImplemented("new-window empty pane".into()).into());
    }
    let mut index = found.new_index;
    let before = args.flag('b');
    if args.flag('a') || before {
        // winlink_shuffle_up from the target's winlink (cmd-new-window.c:
        // 130-131); a free index names none and no room keeps the target's.
        if let Some(wl) = found.index {
            index = server.model.shuffle_up(s, wl, before).or(index);
        }
    }
    let cwd = match args.value('c') {
        Some(c) => {
            // spawn.c:270-283: expanded against the target session,
            // relative to the client's directory.
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
        }
        None => item.cwd.clone(),
    };
    let extra_env: Vec<String> = args.values('e').map(str::to_owned).collect();
    let index = server
        .new_window(&NewWindow {
            session: s,
            index,
            kill: args.flag('k'),
            detached: args.flag('d'),
            name,
            argv: args
                .positionals
                .iter()
                .map(|p| p.text().into_owned())
                .collect(),
            cwd,
            extra_env: &extra_env,
            client_path: item.client.and(client_path(&item.env)),
        })
        .map_err(EdgeError::CreateWindowFailed)?;
    if args.flag('P') {
        let w = server.model.sessions[&s].windows[&index];
        let new = Found {
            session: Some(s),
            index: Some(index),
            window: Some(w),
            pane: server
                .model
                .windows
                .get(&w)
                .map(crate::model::Window::active),
            new_index: None,
        };
        let text = expand(server, new, args.value('F').unwrap_or(TEMPLATE));
        item.print(&text);
    }
    Ok(())
}

/// `-S` (cmd-new-window.c:105-125): the window at the target index, or the
/// only window with that name.
fn select_existing(
    server: &Server,
    s: vtmux_core::ids::SessionId,
    index: Option<u32>,
    name: Option<&str>,
) -> Result<Option<u32>, EdgeError> {
    let session = &server.model.sessions[&s];
    if let Some(i) = index {
        return Ok(session.windows.contains_key(&i).then_some(i));
    }
    let Some(name) = name else { return Ok(None) };
    let wanted = expand(
        server,
        Found {
            session: Some(s),
            ..Found::default()
        },
        name,
    );
    let mut hit = None;
    for (i, w) in &session.windows {
        if server
            .model
            .windows
            .get(w)
            .is_some_and(|x| x.name == wanted)
        {
            if hit.is_some() {
                return Err(EdgeError::Message(format!("multiple windows named {name}")));
            }
            hit = Some(*i);
        }
    }
    Ok(hit)
}
