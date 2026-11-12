//! `new-session` (cmd-new-session.c:67-380): one window with one pane.
//! Without `-d` a client with a terminal is attached through the server's
//! attach hook; `-t` makes the session part of a session group
//! ([`crate::groups`]).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::options::TreeId;

use super::{Item, command_client, expand_as, expand_for_client};
use crate::format_ctx::Kind;
use crate::names::{check_name, clean_name};
use crate::server::{NewGroup, NewSession, Server};
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

/// The group `-t value` names (cmd-new-session.c:143-160): the group of
/// the session found, or that session (grouped with the new one under its
/// name), or a group called `value` that may not exist yet.
fn group_for(
    server: &Server,
    value: &str,
    with: Option<vtmux_core::ids::SessionId>,
) -> Result<NewGroup, EdgeError> {
    let m = &server.model;
    if let Some(s) = with {
        let name = match m.groups.name_of(s) {
            Some(g) => g.to_owned(),
            None => m.sessions[&s].name.clone(),
        };
        return Ok(NewGroup {
            name,
            with: Some(s),
        });
    }
    if m.groups.named(value).is_some() {
        return Ok(NewGroup {
            name: value.to_owned(),
            with: None,
        });
    }
    if !check_name(value) {
        return Err(EdgeError::Message(format!(
            "invalid session group name: {value}"
        )));
    }
    Ok(NewGroup {
        name: clean_name(value),
        with: None,
    })
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
            // format_single(item, tmp, c, NULL, NULL, NULL)
            // (cmd-new-session.c:103, 113, 178).
            let n = expand_as(
                server,
                found,
                command_client(server, item),
                Kind::Unknown,
                n,
            );
            if !check_name(&n) {
                return Err(EdgeError::InvalidWindowName(n).into());
            }
            Some(clean_name(&n))
        }
        None => None,
    };
    let session_name = match args.value('s') {
        Some(n) => {
            let n = expand_as(
                server,
                found,
                command_client(server, item),
                Kind::Unknown,
                n,
            );
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
            // cmd-new-session.c:116-123: attach-session's checks and flags
            // (-c, -D, -X, -E, -f) over the session found.
            if super::check_nested(server, item) {
                return Err(super::nested_error().into());
            }
            let found = Found::from_session(&server.model, s);
            if let Some(cwd) = args.value('c') {
                let cwd = expand_for_client(server, item, found, cwd);
                if let Some(x) = server.model.sessions.get_mut(&s) {
                    x.cwd = vtmux_core::rawstr::to_os_string(&cwd).into();
                }
            }
            if let (Some(c), Some(flags)) = (item.client, args.value('f')) {
                server.set_client_flags(c, flags);
            }
            let detach = (args.flag('D') || args.flag('X')).then(|| args.flag('X'));
            super::attach(server, item, s, !args.flag('E'), true, detach)?;
            // cmd_attach_session ends with cfg_show_causes (cmd-attach-session.c:170-171).
            if server.cfg_finished {
                server.cfg_show_causes(Some(s));
            }
            return Ok(());
        }
    }
    if let Some(n) = &session_name
        && server.model.session_named(n).is_some()
    {
        return Err(EdgeError::DuplicateSession(n.clone()).into());
    }
    // cmd-new-session.c:143-160: is this going to be part of a session
    // group? The target session's group, else the target session (whose
    // name the group takes), else a group by the given name.
    let group = match args.value('t') {
        Some(t) => Some(group_for(server, t, found.session)?),
        None => None,
    };
    // A client-less run (config file) is always detached.
    let detached = args.flag('d') || item.client.is_none();
    let cwd = match args.value('c') {
        Some(c) => {
            let c = expand_as(
                server,
                found,
                command_client(server, item),
                Kind::Unknown,
                c,
            );
            // spawn.c:295-301: relative to the client's directory.
            super::start_path(&server.client_cwd(item.client, None), &c)
        }
        None => server.client_cwd(item.client, None),
    };
    // A control client is attached without a terminal and sized like a
    // detached session (cmd-new-session.c:165-168, 250-266).
    let control = item.control.is_some();
    // A client that has a session already has its terminal open and is
    // not checked for nesting (cmd-new-session.c:178-197).
    let already_attached = item
        .client
        .is_some_and(|c| server.client_session(c).is_some());
    let tty = super::client_tty(server, item);
    if !detached
        && !control
        && !already_attached
        && tty.is_some()
        && super::check_nested(server, item)
    {
        return Err(super::nested_error().into());
    }
    if !detached && !control && tty.is_none() {
        return Err(EdgeError::OpenTerminalFailed("not a terminal".into()).into());
    }
    // cmd-new-session.c:207-213: the terminal is opened before the session
    // is made.
    if !detached
        && !control
        && !already_attached
        && let Some(client) = item.client
    {
        server.check_terminal(client)?;
    }
    let client_size = tty.as_ref().map(|t| (t.cols, t.rows));
    let x = size(args.value('x'), "width", client_size.map(|c| c.0), 80)?;
    let y = size(args.value('y'), "height", client_size.map(|c| c.1), 24)?;
    // Attached: the client's terminal less one row when the global
    // `status` is on, whatever its line count (cmd-new-session.c:250-254);
    // attaching fits it after. Detached: the global default-size with
    // -x/-y over it, 80x24 when it does not parse (255-266).
    let status = u32::from(server.model.status_lines(TreeId::GlobalSession) > 0);
    let clamp = |n: u32| u16::try_from(n).unwrap_or(u16::MAX).max(1);
    let (cols, rows) = if let (false, Some((c, r))) = (detached || control, client_size) {
        (clamp(c), clamp(r.saturating_sub(status)))
    } else {
        let (dx, dy) = server.default_size(TreeId::GlobalSession);
        (x.unwrap_or(clamp(dx)), y.unwrap_or(clamp(dy)))
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
        client_env: item.env().to_vec(),
        extra_env: args.values('e').map(str::to_owned).collect(),
        update_env: item.client.is_some() && !args.flag('E'),
        default_size: (args.flag('x') || args.flag('y'))
            .then(|| (x.unwrap_or(cols), y.unwrap_or(rows))),
        cell_pixels: server.new_session_pixels(item.client.filter(|_| !detached)),
        group,
        // cmd-new-session.c:191-202: the termios of a client that attaches
        // and has no session yet, read with the nesting check.
        control_chars: (!detached && !control && !already_attached && tty.is_some())
            .then(|| server.client_control_chars(item.client?))
            .flatten(),
    };
    let s = server
        .create_session(spec)
        .map_err(|e| EdgeError::CreateWindowFailed(crate::server::spawn_failed(&e)))?;
    if args.flag('P') {
        let template = args.value('F').unwrap_or(TEMPLATE);
        // format_single(item, template, c, s, s->curw, NULL)
        // (cmd-new-session.c:351): a window's tree.
        let text = expand_as(
            server,
            Found::from_session(&server.model, s),
            command_client(server, item),
            Kind::Window,
            template,
        );
        item.print(&text);
    }
    if !detached {
        // cmd-new-session.c:335-336.
        if let (Some(c), Some(flags)) = (item.client, args.value('f')) {
            server.set_client_flags(c, flags);
        }
        super::attach(server, item, s, false, false, None)?;
    }
    // cmd-new-session.c:358-359: the commands after this one start from
    // the new session, unless it was made in the background.
    if !args.flag('d') {
        item.new_current = Some(Found::from_session(&server.model, s));
    }
    // cmd-new-session.c:361-362.
    item.hook_target = Some(Some(Found::from_session(&server.model, s)));
    // cmd-new-session.c:364-365: also for a detached session another
    // client may be attached to.
    if server.cfg_finished {
        server.cfg_show_causes(Some(s));
    }
    // Later commands in the batch see the new session as current: it
    // is the most recently active one (cmd_find_best_session).
    Ok(())
}
