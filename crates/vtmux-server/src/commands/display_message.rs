//! `display-message` (cmd-display-message.c:61-180), without a status
//! line: `-p` prints, and with no attached client to show the message on,
//! nothing is shown (`status_message_set` needs a target client).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::format;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// `DISPLAY_MESSAGE_TEMPLATE` (cmd-display-message.c:30-33).
const TEMPLATE: &str = "[#{session_name}] #{window_index}:#{window_name}, current pane #{pane_index} - (%H:%M %d-%b-%y)";

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    if args.flag('I') {
        return Err(EdgeError::NotImplemented("display-message -I".into()).into());
    }
    if args.flag('F') && !args.positionals.is_empty() {
        return Err(EdgeError::FormatAndArgument.into());
    }
    if let Some(d) = args.value('d') {
        super::number_expanded(server, found, Some(d), 0, i64::from(u32::MAX)).map_err(
            |cause| EdgeError::BadNumber {
                what: "delay",
                cause,
            },
        )?;
    }
    let template = args
        .positional(0)
        .or(args.value('F'))
        .unwrap_or(if args.flag('j') { "" } else { TEMPLATE });
    // cmd-display-message.c:117-126: -c when it is on the target session, else
    // the best client for the session. (-c names are not resolved yet, so the
    // calling client stands in for it.)
    let tc = server.target_client(item.client, item.origin);
    let client = match found.session {
        s @ Some(_) if tc.and_then(|c| server.client_session(c)) == s => tc,
        Some(s) => server.best_client(Some(s)),
        None => None,
    };
    let ctx = server.format_ctx(found, client);
    if args.flag('a') {
        return Err(EdgeError::NotImplemented("display-message -a".into()).into());
    }
    let msg = if args.flag('l') {
        template.to_owned()
    } else {
        format::expand_time(template, &ctx)
    };
    if args.flag('j') {
        return Err(EdgeError::NotImplemented("display-message -j".into()).into());
    }
    if item.client.is_none() {
        // cmdq_error with no client: a config cause.
        item.error(server, msg.into_bytes());
        item.status = 0;
        return Ok(());
    }
    if args.flag('p') {
        item.print(&msg);
    } else if item.client.is_some_and(|c| server.is_control(c)) && args.value('c').is_none() {
        // cmd-display-message.c:163-169: a control target client gets
        // `%message <msg>` through server_client_print, so it lands inside
        // the command's %begin/%end guard.
        item.print(&format!("%message {msg}"));
    }
    // Otherwise the message goes to the target client's status line; an
    // unattached command client has none, so nothing is shown.
    Ok(())
}
