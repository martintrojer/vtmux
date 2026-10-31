//! `rename-session` (cmd-rename-session.c:45-90).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::{Item, expand};
use crate::model::Redraw;
use crate::names::{check_name, clean_name};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let Some(s) = found.session else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let name = expand(server, found, &args.positional_text(0).unwrap_or_default());
    if !check_name(&name) {
        return Err(EdgeError::InvalidSessionName(name).into());
    }
    let name = clean_name(&name);
    let old = server.model.sessions[&s].name.clone();
    if name == old {
        return Ok(());
    }
    if server.model.session_named(&name).is_some() {
        return Err(EdgeError::DuplicateSession(name).into());
    }
    // The sessions are listed in name order: re-key.
    server.model.by_name.remove(&old);
    server.model.by_name.insert(name.clone(), s);
    server
        .model
        .sessions
        .get_mut(&s)
        .expect("target session")
        .name
        .clone_from(&name);
    server.model.redraw(Redraw::SessionStatus(s));
    server.control_session_renamed(s, &name);
    Ok(())
}
