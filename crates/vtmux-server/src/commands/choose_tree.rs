//! `choose-tree` (cmd-choose-tree.c:120-156, window-tree.c:1107-1158):
//! tree mode on the target pane. `-G` (session groups) does nothing: vtmux
//! has none.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::mode::window_tree::{self, Options, Start, WindowTree};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let sort = window_tree::sort_option(args.value('O'))?;
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
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
        command: args.positional_text(0).map(std::borrow::Cow::into_owned),
        hide_this_pane: args.flag('h'),
        accept: args.flag('y'),
    };
    // mode_tree_zoom: -Z zooms the pane; the mode unzooms on exit unless
    // the window was zoomed already.
    let mut unzoom = None;
    if args.flag('Z')
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
    let Some(size) = server
        .model
        .panes
        .get(&p)
        .map(|x| (usize::from(x.term.cols()), usize::from(x.term.rows())))
    else {
        return Ok(());
    };
    let mut mode = WindowTree::new(p, size, found, options);
    mode.unzoom = unzoom;
    mode.build(server);
    server
        .model
        .set_tree_mode(p, args.flag('k'), |_| Some(mode));
    Ok(())
}
