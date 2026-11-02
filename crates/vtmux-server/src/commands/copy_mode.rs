//! `copy-mode` (cmd-copy-mode.c:56-115).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use vtmux_core::ids::PaneId;

use super::Item;
use crate::mode::CopyMode;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    source: Found,
    found: Found,
) -> Result<(), CmdError> {
    let mut p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    if args.flag('q') {
        server.reset_mode_all(p);
        return Ok(());
    }
    // cmd-copy-mode.c:74-79: -M is the mouse's pane, for the client on
    // its session only; without a mouse event nothing happens.
    if args.flag('M') {
        let Some(m) = item.mouse.as_ref() else {
            return Ok(());
        };
        let Some(f) = m.target.pane_found(&server.model) else {
            return Ok(());
        };
        let Some(c) = item.client else {
            return Ok(());
        };
        if server.client_session(c) != f.session {
            return Ok(());
        }
        p = f.pane.ok_or(EdgeError::NoCurrentTarget)?;
    }
    let swp = if args.flag('s') {
        source.pane.unwrap_or(p)
    } else {
        p
    };
    let flags = (args.flag('e'), args.flag('H'));
    let vi = server.model.mode_keys_vi(p);
    let entered = server.enter_copy_mode(p, args.flag('k'), |model| {
        let size = (
            usize::from(model.panes[&p].term.cols()),
            usize::from(model.panes[&p].term.rows()),
        );
        let source = &model.panes[&swp].term;
        let search = model.panes[&p].search.clone();
        CopyMode::new(source, size, swp != p, flags, vi, search)
    });
    // window_copy_set_line_numbers: keys and commands, not the mouse.
    let line_numbers = !item.key.is_some_and(vtmux_core::keys::Key::is_mouse);
    with_copy(server, p, |m, env| {
        m.set_line_numbers(env, line_numbers, false);
    });
    if entered
        && args.flag('M')
        && let Some(m) = item.mouse.clone()
    {
        server.copy_start_drag(item.client, &m);
    }
    if args.flag('u') {
        with_copy(server, p, |m, env| m.pageup(env, false));
    }
    if args.flag('d')
        && with_copy(server, p, |m, env| m.pagedown(env, false, args.flag('e'))) == Some(true)
    {
        server.reset_mode(p);
    }
    // -S scrolls to the mouse on the scrollbar; with no mouse event (a key
    // or a hook) the event's y is 0, the top of the bar. No client, nothing.
    if args.flag('S')
        && item.client.is_some()
        && with_copy(server, p, |m, env| m.scroll_to_top(env, args.flag('e'))) == Some(true)
    {
        server.reset_mode(p);
    }
    Ok(())
}

/// Run `f` on pane `p`'s copy mode if it is the shown mode.
pub(crate) fn with_copy<R>(
    server: &mut Server,
    p: PaneId,
    f: impl FnOnce(&mut CopyMode, &crate::mode::copy::Env) -> R,
) -> Option<R> {
    server
        .with_mode(p, |mode, env| mode.as_copy_mut().map(|m| f(m, env)))
        .flatten()
}
