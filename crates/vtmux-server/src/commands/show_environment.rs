//! `show-environment` (cmd-show-environment.c:40-170).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use super::set_environment::missing_session;
use crate::environ::Entry;
use crate::server::Server;
use crate::target::Found;

/// `cmd_show_environment_escape`: POSIX double-quote escaping.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for c in value.chars() {
        if matches!(c, '$' | '`' | '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `cmd_show_environment_print`: nothing for an entry on the wrong side of
/// `-h`.
fn line(args: &Args, name: &str, entry: &Entry) -> Option<String> {
    if entry.hidden != args.flag('h') {
        return None;
    }
    Some(match (&entry.value, args.flag('s')) {
        (Some(v), false) => format!("{name}={v}"),
        (None, false) => format!("-{name}"),
        (Some(v), true) => format!("{name}=\"{}\"; export {name};", escape(v)),
        (None, true) => format!("unset {name};"),
    })
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let session = found
        .session
        .filter(|s| server.model.sessions.contains_key(s));
    if session.is_none() && args.flag('t') {
        return Err(missing_session(args).into());
    }
    let env = if args.flag('g') {
        &server.global_env
    } else {
        let s = session.ok_or_else(|| missing_session(args))?;
        &server.model.sessions[&s].env
    };
    let lines: Vec<String> = if let Some(name) = args.positional_text(0) {
        let entry = env
            .find(&name)
            .ok_or_else(|| EdgeError::Message(format!("unknown variable: {name}")))?;
        line(args, &name, entry).into_iter().collect()
    } else {
        env.iter()
            .filter_map(|(name, entry)| line(args, name, entry))
            .collect()
    };
    for l in lines {
        item.print(&l);
    }
    Ok(())
}
