//! `server-access` (cmd-server-access.c:60-152): list, add, remove and
//! set read-only or writable the users and groups in the server's access
//! list ([`crate::acl`]).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::acl::Subject;
use crate::server::Server;
use crate::sys::user;
use crate::target::Found;

fn error(message: String) -> CmdError {
    EdgeError::Message(message).into()
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    _found: Found,
) -> Result<(), CmdError> {
    if args.flag('l') {
        list(server, item);
        return Ok(());
    }
    let Some(arg) = args.positional_text(0) else {
        return Err(error("missing user or group argument".into()));
    };
    // format_single(item, arg, c, NULL, NULL, NULL): the target client and
    // its session (format_defaults, format.c:7015-7020), never `-t`'s.
    let tc = server.target_client(item.client, item.origin);
    let at = tc
        .and_then(|c| server.client_session(c))
        .map(|s| Found::from_session(&server.model, s))
        .unwrap_or_default();
    let mut ctx = server.format_ctx(at, tc);
    ctx.kind = crate::format_ctx::Kind::Unknown;
    let arg = vtmux_core::format::expand_str(&arg, &ctx);
    let (kind, subject, name) = if args.flag('g') {
        let g = user::group_named(&arg);
        (
            "group",
            g.as_ref().map(|g| Subject::Group(g.gid)),
            g.map(|g| g.name),
        )
    } else {
        let p = user::user_named(&arg);
        (
            "user",
            p.as_ref().map(|p| Subject::User(p.uid)),
            p.map(|p| p.name),
        )
    };
    let (Some(subject), Some(name)) = (subject, name) else {
        return Err(error(format!("unknown {kind}: {arg}")));
    };
    if let Subject::User(uid) = subject
        && (uid == 0 || uid == rustix::process::getuid().as_raw())
    {
        return Err(error(format!(
            "{name} owns the server, can't change access"
        )));
    }
    if args.flag('a') && args.flag('d') {
        return Err(error("-a and -d cannot be used together".into()));
    }
    if args.flag('w') && args.flag('r') {
        return Err(error("-r and -w cannot be used together".into()));
    }
    let not_found = || error(format!("{kind} {name} not found"));
    if args.flag('d') {
        if !server.acl.contains(subject) {
            return Err(not_found());
        }
        server.acl.deny(subject);
        server.acl_update();
        return Ok(());
    }
    if args.flag('a') {
        if server.acl.contains(subject) {
            return Err(error(format!("{kind} {name} is already added")));
        }
        // -r or -w may come with -a.
        server.acl.allow(subject);
    } else if args.flag('r') || args.flag('w') {
        // Either alone adds the entry first.
        server.acl.allow(subject);
    }
    for (flag, read_only) in [('w', false), ('r', true)] {
        if args.flag(flag) {
            if !server.acl.contains(subject) {
                return Err(not_found());
            }
            server.acl.set_read_only(subject, read_only);
            server.acl_update();
            return Ok(());
        }
    }
    Ok(())
}

/// `server_acl_display` (server-acl.c:134-166): `name (U,W)` or
/// `name (G,R)` per entry, root left out.
fn list(server: &Server, item: &mut Item) {
    for (subject, read_only) in server.acl.entries() {
        let (name, kind) = match subject {
            Subject::User(0) => continue,
            Subject::User(uid) => (user::user_by_uid(uid).map(|p| p.name), 'U'),
            Subject::Group(gid) => (user::group_by_gid(gid).map(|g| g.name), 'G'),
        };
        let access = if read_only { 'R' } else { 'W' };
        let name = name.unwrap_or_else(|| "unknown".into());
        item.print(&format!("{name} ({kind},{access})"));
    }
}
