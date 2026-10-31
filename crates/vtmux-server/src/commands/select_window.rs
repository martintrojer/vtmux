//! `select-window`, `next-window`, `previous-window` and `last-window`
//! (cmd-select-window.c:84-150). `-a` only counts windows with an alert
//! flag.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// Which command, before the `-n -p -l` flags of select-window.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Select,
    Next,
    Previous,
    Last,
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
    which: Which,
) -> Result<(), CmdError> {
    let Some(s) = found.session else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let model = &mut server.model;
    let no = |what: &str| EdgeError::Message(format!("no {what} window"));
    // server_redraw_session after a change: next/previous/last and -T fail
    // when nothing changes, but plain select-window on the current window
    // succeeds without a redraw (cmd-select-window.c:136-139,
    // session_select returning 1).
    let mut changed = true;
    if which == Which::Next || args.flag('n') {
        model
            .next_window(s, args.flag('a'))
            .filter(|changed| *changed)
            .ok_or_else(|| no("next"))?;
    } else if which == Which::Previous || args.flag('p') {
        model
            .previous_window(s, args.flag('a'))
            .filter(|changed| *changed)
            .ok_or_else(|| no("previous"))?;
    } else if which == Which::Last || args.flag('l') {
        last(model, s).ok_or_else(|| no("last"))?;
    } else {
        let index = found.index.ok_or(EdgeError::NoCurrentTarget)?;
        // -T on the current window goes to the last one instead.
        if args.flag('T') && model.sessions[&s].current == index {
            last(model, s).ok_or_else(|| no("last"))?;
        } else {
            changed = model.set_current(s, index) == Some(true);
        }
    }
    if changed {
        model.redraw(crate::model::Redraw::Session(s));
    }
    // cmd-select-window.c:145-147: an attached calling client becomes the
    // new window's latest, and the window takes any size pending.
    if let Some(c) = super::latest_client(server, item) {
        server.set_latest(s, c);
    }
    server.recalculate_sizes();
    Ok(())
}

/// `session_last`, where "already current" (1) is an error too.
fn last(model: &mut crate::model::Model, s: vtmux_core::ids::SessionId) -> Option<()> {
    model.last_window(s).filter(|changed| *changed).map(|_| ())
}
