//! `set-environment` (cmd-set-environment.c:46-120).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// The `no such session` / `no current session` pair shared with
/// show-environment (cmd-set-environment.c:78-82).
pub(super) fn missing_session(args: &Args) -> EdgeError {
    match args.value('t') {
        Some(t) => EdgeError::Message(format!("no such session: {t}")),
        None => EdgeError::Message("no current session".into()),
    }
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let name = args.positional_text(0).unwrap_or_default().into_owned();
    if name.is_empty() {
        return Err(EdgeError::Message("empty variable name".into()).into());
    }
    if name.contains('=') {
        return Err(EdgeError::Message("variable name contains =".into()).into());
    }
    let mut value = args.positional_text(1).map(std::borrow::Cow::into_owned);
    if args.flag('F')
        && let Some(v) = &value
    {
        // format_single_from_target: the target and its best client.
        let tc = server.target_client(item.client, item.origin);
        value = Some(format::expand_str(v, &server.format_ctx(found, tc)));
    }
    let env = if args.flag('g') {
        &mut server.global_env
    } else {
        let session = found.session.ok_or_else(|| missing_session(args))?;
        &mut server
            .model
            .sessions
            .get_mut(&session)
            .ok_or_else(|| missing_session(args))?
            .env
    };
    if args.flag('u') {
        if value.is_some() {
            return Err(EdgeError::Message("can't specify a value with -u".into()).into());
        }
        env.unset(&name);
    } else if args.flag('r') {
        if value.is_some() {
            return Err(EdgeError::Message("can't specify a value with -r".into()).into());
        }
        env.clear(&name);
    } else {
        let Some(value) = value else {
            return Err(EdgeError::Message("no value specified".into()).into());
        };
        env.set(&name, &value, args.flag('h'));
    }
    Ok(())
}
