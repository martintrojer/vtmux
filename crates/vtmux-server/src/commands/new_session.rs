//! `new-session` (cmd-new-session.c:67-380) for M0: one window with one
//! pane. Without `-d` a client with a terminal is attached through the
//! server's attach hook; session groups (`-t`) and `-A` are M1.

use std::path::PathBuf;

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::{Item, expand};
use crate::names::{check_name, clean_name};
use crate::server::{NewSession, Server};
use crate::target::Found;

/// `NEW_SESSION_TEMPLATE` (cmd-new-session.c:34).
const TEMPLATE: &str = "#{session_name}:";

/// `-x` / `-y` (cmd-new-session.c:222-253): `-` is the client's size (or
/// the default), else `strtonum(1, USHRT_MAX)`.
fn size(
    value: Option<&str>,
    what: &'static str,
    client: Option<u32>,
    default: u16,
) -> Result<Option<u16>, EdgeError> {
    let Some(v) = value else { return Ok(None) };
    if v == "-" {
        return Ok(Some(
            client
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(default),
        ));
    }
    vtmux_core::cmd::strtonum(v, 1, i64::from(u16::MAX))
        .map(|n| Some(u16::try_from(n).expect("in u16 range")))
        .map_err(|cause| EdgeError::BadNumber { what, cause })
}

#[allow(clippy::too_many_lines)] // cmd_new_session_exec, in order
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    if args.flag('t') && (!args.positionals.is_empty() || args.flag('n')) {
        return Err(EdgeError::TargetWithCommand.into());
    }
    let window_name = match args.value('n') {
        Some(n) => {
            let n = expand(server, found, n);
            if !check_name(&n) {
                return Err(EdgeError::InvalidWindowName(n).into());
            }
            Some(clean_name(&n))
        }
        None => None,
    };
    let session_name = match args.value('s') {
        Some(n) => {
            let n = expand(server, found, n);
            if !check_name(&n) {
                return Err(EdgeError::InvalidSessionName(n).into());
            }
            Some(clean_name(&n))
        }
        None => None,
    };
    if args.flag('A') {
        let existing = match &session_name {
            Some(n) => server.model.session_named(n).map(|s| s.id),
            None => found.session,
        };
        if let Some(s) = existing {
            return super::attach(server, item, s);
        }
    }
    if let Some(n) = &session_name
        && server.model.session_named(n).is_some()
    {
        return Err(EdgeError::DuplicateSession(n.clone()).into());
    }
    if args.flag('t') {
        return Err(EdgeError::NotImplemented("session groups (new-session -t)".into()).into());
    }
    // A client-less run (config file) is always detached.
    let detached = args.flag('d') || item.client.is_none();
    let cwd = match args.value('c') {
        Some(c) => {
            let c = expand(server, found, c);
            if c.starts_with('/') {
                PathBuf::from(c)
            } else {
                // spawn.c:295-301: relative to the client's directory.
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
    // A control client is attached without a terminal and sized like a
    // detached session (cmd-new-session.c:165-168, 250-266).
    let control = item.control.is_some();
    if !detached && !control && item.tty.is_none() {
        return Err(EdgeError::OpenTerminalFailed("not a terminal".into()).into());
    }
    let client_size = item.tty.as_ref().map(|t| (t.cols, t.rows));
    let x = size(args.value('x'), "width", client_size.map(|c| c.0), 80)?;
    let y = size(args.value('y'), "height", client_size.map(|c| c.1), 24)?;
    // Detached: -x/-y or 80x24 (default-size); attached: the client's
    // terminal less the status line (cmd-new-session.c:255-276).
    let (cols, rows) = match (detached || control, client_size) {
        (false, Some((c, r))) => (
            u16::try_from(c).unwrap_or(u16::MAX).max(1),
            u16::try_from(r.saturating_sub(1))
                .unwrap_or(u16::MAX)
                .max(1),
        ),
        _ => (x.unwrap_or(80), y.unwrap_or(24)),
    };
    let spec = NewSession {
        name: session_name,
        window_name,
        argv: args
            .positionals
            .iter()
            .map(|p| p.text().into_owned())
            .collect(),
        cwd,
        cols,
        rows,
        client_env: item.env.clone(),
        extra_env: args.values('e').map(str::to_owned).collect(),
        update_env: item.client.is_some() && !args.flag('E'),
        default_size: (args.flag('x') || args.flag('y'))
            .then(|| (x.unwrap_or(cols), y.unwrap_or(rows))),
    };
    let s = server
        .create_session(spec)
        .map_err(|e| EdgeError::CreateWindowFailed(e.to_string()))?;
    if args.flag('P') {
        let template = args.value('F').unwrap_or(TEMPLATE);
        let text = expand(server, Found::from_session(&server.model, s), template);
        item.print(&text);
    }
    if !detached {
        super::attach(server, item, s)?;
    }
    // Later commands in the batch see the new session as current: it
    // is the most recently active one (cmd_find_best_session).
    Ok(())
}
