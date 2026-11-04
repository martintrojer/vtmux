//! `choose-tree`, `choose-buffer` and `choose-client`
//! (cmd-choose-tree.c:120-156; window-tree.c:1107-1158,
//! window-buffer.c:376-412, window-client.c:394-434): a tree mode on the
//! target pane. `-G` (session groups) does nothing: vtmux has none.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::mode::window_tree::{self, Options, Start, WindowTree};
use crate::mode::{
    BufferMode, ClientMode, CustomizeMode, window_buffer, window_client, window_customize,
};
use crate::server::Server;
use crate::target::Found;

/// `mode_tree_zoom`: `-Z` zooms the pane; the mode unzooms on exit unless
/// the window was zoomed already. Then the pane's size, if it still is.
fn zoom_and_size(
    server: &mut Server,
    zoom: bool,
    found: Found,
    p: vtmux_core::ids::PaneId,
) -> (Option<vtmux_core::ids::WindowId>, Option<(usize, usize)>) {
    let mut unzoom = None;
    if zoom
        && let Some(w) = found.window
        && !server
            .model
            .windows
            .get(&w)
            .is_some_and(crate::model::Window::zoomed)
    {
        server.model.zoom(w, p);
        unzoom = Some(w);
    }
    let size = server
        .model
        .panes
        .get(&p)
        .map(|x| (usize::from(x.term.cols()), usize::from(x.term.rows())));
    (unzoom, size)
}

/// The template, if one is given.
fn template(args: &Args) -> Option<String> {
    args.positional_text(0).map(std::borrow::Cow::into_owned)
}

/// `choose-buffer`: nothing with no buffers.
pub fn exec_buffer(server: &mut Server, args: &Args, found: Found) -> Result<(), CmdError> {
    let sort = window_tree::sort_option(args.value('O'))?;
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    if server.model.paste.by_time().is_empty() {
        return Ok(());
    }
    // window_pane_set_mode: the same mode on top is left alone, before
    // mode_tree_zoom.
    if !server
        .model
        .pane_may_enter(p, |m| matches!(m, crate::mode::Mode::Buffer(_)))
    {
        return Ok(());
    }
    server.reset_no_stack(p);
    let (unzoom, Some(size)) = zoom_and_size(server, args.flag('Z'), found, p) else {
        return Ok(());
    };
    let options = window_buffer::Options {
        previews: args.count('N'),
        sort,
        reversed: args.flag('r'),
        filter: args.value('f').map(str::to_owned),
        format: args.value('F').map(str::to_owned),
        key_format: args.value('K').map(str::to_owned),
        command: template(args),
    };
    let mut mode = BufferMode::new(p, size, found, options);
    mode.common.unzoom = unzoom;
    mode.build(server);
    server
        .model
        .set_tree_mode(p, args.flag('k'), |_| Some(mode));
    Ok(())
}

/// `choose-client`: nothing with no attached clients.
pub fn exec_client(server: &mut Server, args: &Args, found: Found) -> Result<(), CmdError> {
    let sort = window_tree::sort_option(args.value('O'))?;
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    if <ClientMode as crate::mode::tree_mode::TreeMode>::empty(server) {
        return Ok(());
    }
    if !server
        .model
        .pane_may_enter(p, |m| matches!(m, crate::mode::Mode::Client(_)))
    {
        return Ok(());
    }
    server.reset_no_stack(p);
    let (unzoom, Some(size)) = zoom_and_size(server, args.flag('Z'), found, p) else {
        return Ok(());
    };
    let options = window_client::Options {
        previews: args.count('N'),
        sort,
        reversed: args.flag('r'),
        filter: args.value('f').map(str::to_owned),
        format: args.value('F').map(str::to_owned),
        key_format: args.value('K').map(str::to_owned),
        command: template(args),
        hide_this_pane: args.flag('h'),
        info: args.flag('i'),
    };
    let mut mode = ClientMode::new(p, size, options);
    mode.common.unzoom = unzoom;
    mode.build(server);
    server
        .model
        .set_tree_mode(p, args.flag('k'), |_| Some(mode));
    Ok(())
}

/// `customize-mode`.
pub fn exec_customize(server: &mut Server, args: &Args, found: Found) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    if !server
        .model
        .pane_may_enter(p, |m| matches!(m, crate::mode::Mode::Customize(_)))
    {
        return Ok(());
    }
    server.reset_no_stack(p);
    let (unzoom, Some(size)) = zoom_and_size(server, args.flag('Z'), found, p) else {
        return Ok(());
    };
    let options = window_customize::Options {
        previews: args.count('N'),
        filter: args.value('f').map(str::to_owned),
        format: args.value('F').map(str::to_owned),
        accept: args.flag('y'),
    };
    let mut mode = CustomizeMode::new(p, size, found, options);
    mode.common.unzoom = unzoom;
    mode.build(server);
    server
        .model
        .set_tree_mode(p, args.flag('k'), |_| Some(mode));
    Ok(())
}

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let sort = window_tree::sort_option(args.value('O'))?;
    let options = Options {
        start: if args.flag('s') {
            Start::Session
        } else if args.flag('w') {
            Start::Window
        } else {
            Start::Pane
        },
        previews: args.count('N'),
        sort,
        reversed: args.flag('r'),
        filter: args.value('f').map(str::to_owned),
        format: args.value('F').map(str::to_owned),
        key_format: args.value('K').map(str::to_owned),
        command: template(args),
        hide_this_pane: args.flag('h'),
        accept: args.flag('y'),
    };
    open(server, found, options, args.flag('Z'), args.flag('k'))
}

/// `window_pane_set_mode(wp, NULL, &window_tree_mode, ...)`: open the tree
/// on the target pane, `zoom` being `-Z` and `kill` `-k`.
pub(super) fn open(
    server: &mut Server,
    found: Found,
    options: Options,
    zoom: bool,
    kill: bool,
) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    let (unzoom, Some(size)) = zoom_and_size(server, zoom, found, p) else {
        return Ok(());
    };
    let mut mode = WindowTree::new(p, size, found, options);
    mode.common.unzoom = unzoom;
    mode.build(server);
    server.model.set_tree_mode(p, kill, |_| Some(mode));
    Ok(())
}
