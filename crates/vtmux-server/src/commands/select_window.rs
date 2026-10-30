//! `select-window`, `next-window`, `previous-window` and `last-window`
//! (cmd-select-window.c:84-150). `-a` (windows with an alert) finds
//! nothing: vtmux has no alerts yet.

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
    _item: &mut Item,
    args: &Args,
    found: Found,
    which: Which,
) -> Result<(), CmdError> {
    let Some(s) = found.session else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let model = &mut server.model;
    let no = |what: &str| EdgeError::Message(format!("no {what} window"));
    if which == Which::Next || args.flag('n') {
        model
            .next_window(s, args.flag('a'))
            .ok_or_else(|| no("next"))?;
    } else if which == Which::Previous || args.flag('p') {
        model
            .previous_window(s, args.flag('a'))
            .ok_or_else(|| no("previous"))?;
    } else if which == Which::Last || args.flag('l') {
        last(model, s).ok_or_else(|| no("last"))?;
    } else {
        let index = found.index.ok_or(EdgeError::NoCurrentTarget)?;
        // -T on the current window goes to the last one instead.
        if args.flag('T') && model.sessions[&s].current == index {
            last(model, s).ok_or_else(|| no("last"))?;
        } else {
            model.set_current(s, index);
        }
    }
    Ok(())
}

/// `session_last`, where "already current" (1) is an error too.
fn last(model: &mut crate::model::Model, s: vtmux_core::ids::SessionId) -> Option<()> {
    model.last_window(s).filter(|changed| *changed).map(|_| ())
}
