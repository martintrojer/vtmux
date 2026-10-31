//! `resize-pane` (cmd-resize-pane.c:52-189): `-Z` toggles zoom, `-x`/`-y`
//! set a size (cells or a percentage of the window), `-U -D -L -R` adjust
//! by a count. Mouse resizing (`-M`) and `-T` (trim history) are not done
//! yet.

use vtmux_core::cmd::{Args, CmdError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::layout::ResizeRequest;

use super::Item;
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let (Some(w), Some(wp)) = (found.window, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    if args.flag('T') {
        return Err(EdgeError::NotImplemented("resize-pane -T".into()).into());
    }
    if args.flag('M') {
        // Without a mouse event there is nothing to do (cmd-resize-pane.c:204).
        return Ok(());
    }
    if args.flag('Z') {
        if server.model.windows[&w].zoomed() {
            server.model.unzoom(w);
        } else {
            server.model.zoom(w, wp);
        }
        return Ok(());
    }
    if !server.model.windows[&w].is_floating(wp) {
        server.model.unzoom(w);
    }
    let mut req = ResizeRequest {
        x: args
            .flag('x')
            .then(|| args.value('x').unwrap_or_default().to_owned()),
        y: args
            .flag('y')
            .then(|| args.value('y').unwrap_or_default().to_owned()),
        adjust: Vec::new(),
    };
    // Each adjustment is converted as it is reached, so an earlier -x still
    // applies when a later count is invalid; the layout call does the
    // conversion order, so check the counts first in tmux's U D L R order
    // and stop at the first bad one after applying what came before.
    let mut bad = None;
    for flag in ['U', 'D', 'L', 'R'] {
        if !args.flag(flag) {
            continue;
        }
        let value = args
            .value(flag)
            .or_else(|| args.positional(0))
            .unwrap_or("1");
        match strtonum(value, i64::from(i32::MIN), i64::from(i32::MAX)) {
            Ok(n) => req
                .adjust
                .push((flag, i32::try_from(n).expect("in i32 range"))),
            Err(cause) => {
                bad = Some(cause);
                break;
            }
        }
    }
    let window = server.model.windows.get_mut(&w).expect("target window");
    let result = window.layout.resize_pane_cmd(wp, &req);
    server.model.fix_panes(w);
    // cmd-resize-pane.c:187 (also after a failed adjustment).
    server.model.layout_changed(w);
    result.map_err(EdgeError::Message)?;
    if let Some(cause) = bad {
        return Err(EdgeError::Message(format!("adjustment {cause}")).into());
    }
    Ok(())
}
