//! `select-layout`, `next-layout`, `previous-layout`
//! (cmd-select-layout.c:71-154): a named layout, a layout string (v1 or
//! v2), the next/previous named layout, `-E` to spread the cells around the
//! target pane evenly, and `-o` to go back to the layout before the last
//! select-layout. Every change ends in `recalculate_sizes`
//! (cmd-select-layout.c:146), so a layout string of another size is fitted
//! back to an attached client.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::WindowId;
use vtmux_core::layout::{NamedLayout, SetOpts};
use vtmux_core::options::TreeId;

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// `cycle`: `Some(true)` for next-layout, `Some(false)` for
/// previous-layout, `None` for select-layout.
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
    cycle: Option<bool>,
) -> Result<(), CmdError> {
    let Some(w) = found.window else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    if select(server, item, args, found, cycle)? {
        server.recalculate_size(w, false);
        // cmd-select-layout.c:143-147.
        server.model.layout_changed(w);
    }
    Ok(())
}

/// The layout change itself; `true` where tmux jumps to `changed:`.
fn select(
    server: &mut Server,
    item: &Item,
    args: &Args,
    found: Found,
    cycle: Option<bool>,
) -> Result<bool, CmdError> {
    let (Some(w), Some(wp)) = (found.window, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    server.model.unzoom(w);
    let next = cycle == Some(true) || args.flag('n');
    let previous = cycle == Some(false) || args.flag('p');

    // A control client without new-layouts gets the v1 form
    // (LAYOUT_CUSTOM_OLD_FORMAT, cmd-select-layout.c:93-96).
    let old_format = item.control.is_some()
        && !item
            .client
            .and_then(|c| server.clients.get(&c))
            .is_some_and(|c| c.flags.new_layouts);
    let current = dump(server, w, old_format);
    let window = server.model.windows.get_mut(&w).expect("target window");
    let old = std::mem::replace(&mut window.old_layout, current);

    if next || previous {
        let which = if next {
            NamedLayout::next(window.last_layout)
        } else {
            NamedLayout::previous(window.last_layout)
        };
        apply_named(server, w, which);
        return Ok(true);
    }
    if args.flag('E') {
        window.layout.spread_out(wp);
        server.model.fix_panes(w);
        return Ok(true);
    }
    let undo = args.flag('o');
    let name = match args.positional(0) {
        Some(n) => Some(n.to_owned()),
        None if undo => old.clone(),
        None => None,
    };
    if !undo {
        let named = match &name {
            None => window.last_layout,
            Some(n) => NamedLayout::lookup(n),
        };
        if let Some(which) = named {
            apply_named(server, w, which);
            return Ok(true);
        }
    }
    let Some(name) = name else {
        return Ok(false);
    };
    let window = server.model.windows.get_mut(&w).expect("target window");
    let before = window.panes.active;
    if let Err(cause) = window.layout.select_custom(w, &mut window.panes, &name) {
        window.old_layout = old;
        return Err(EdgeError::Message(format!("{cause}: {name}")).into());
    }
    // layout_parse_apply_ctx sets the "a" pane with window_set_active_pane,
    // which stamps its active point.
    if let Some(active) = window.panes.active
        && Some(active) != before
    {
        server.model.stamp_active(active);
    }
    server.model.fix_panes(w);
    Ok(true)
}

/// `layout_dump` of the window's layout (`None` where tmux's is NULL).
fn dump(server: &Server, w: WindowId, v1: bool) -> Option<String> {
    let window = &server.model.windows[&w];
    if v1 {
        return Some(window.layout.dump_v1());
    }
    let mut panes = window.panes.clone();
    panes.base_index = server.model.pane_base_index(w);
    window.layout.dump_v2(&panes)
}

/// `layout_set_select`: arrange and remember it as `w->lastlayout`. An
/// arrangement fires window-layout-changed itself (layout-set.c:201, 312,
/// 411, 510, 610, 733), so with `changed:` the window gets two; a window of
/// one pane is not arranged and gets one.
fn apply_named(server: &mut Server, w: WindowId, which: NamedLayout) {
    let opts = set_opts(server, w);
    let window = server.model.windows.get_mut(&w).expect("target window");
    let panes = window.panes.panes.clone();
    let arranged = window.layout.select_named(which, &panes, &opts);
    window.last_layout = Some(which);
    server.model.fix_panes(w);
    if arranged {
        server.model.layout_changed(w);
    }
}

/// The window options layout-set.c reads.
fn set_opts(server: &Server, w: WindowId) -> SetOpts {
    let tree = TreeId::Window(w);
    let text = |name: &str| {
        server
            .model
            .options
            .get(tree, name)
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default()
    };
    SetOpts {
        main_pane_height: text("main-pane-height"),
        main_pane_width: text("main-pane-width"),
        other_pane_height: text("other-pane-height"),
        other_pane_width: text("other-pane-width"),
        tiled_layout_max_columns: u32::try_from(
            server.model.number_option(tree, "tiled-layout-max-columns"),
        )
        .unwrap_or(0),
    }
}
