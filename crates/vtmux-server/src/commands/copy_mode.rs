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
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    if args.flag('q') {
        server.reset_mode_all(p);
        return Ok(());
    }
    if args.flag('M') {
        return Err(EdgeError::NotImplemented("copy-mode -M".into()).into());
    }
    let swp = if args.flag('s') {
        source.pane.unwrap_or(p)
    } else {
        p
    };
    let flags = (args.flag('e'), args.flag('H'));
    let vi = server.model.mode_keys_vi(p);
    server.enter_copy_mode(p, args.flag('k'), |model| {
        let size = (
            usize::from(model.panes[&p].term.cols()),
            usize::from(model.panes[&p].term.rows()),
        );
        let source = &model.panes[&swp].term;
        let search = model.panes[&p].search.clone();
        CopyMode::new(source, size, swp != p, flags, vi, search)
    });
    // window_copy_set_line_numbers(wp, 1): keys and commands, not the mouse.
    with_copy(server, p, |m, env| m.set_line_numbers(env, true, false));
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
