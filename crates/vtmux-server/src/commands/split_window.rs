//! `split-window` (cmd-split-window.c:73-345, layout.c
//! `layout_get_tiled_cell`): a new pane next to the target, or next to the
//! whole layout with `-f`. Styles, borders (`-B -s -S -R`), empty panes
//! (`-E -I`), `-k -m -W` and `-M` are not done yet.

use std::path::PathBuf;

use vtmux_core::cmd::{Args, CmdError, NumError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::layout::{AddFlags, PANE_MAXIMUM, SplitRequest};

use super::{Item, expand};
use crate::server::{PaneSpawn, Server, client_path};
use crate::target::Found;

/// `SPLIT_WINDOW_TEMPLATE` (cmd-split-window.c:33).
const TEMPLATE: &str = "#{session_name}:#{window_index}.#{pane_index}";

/// `args_string_percentage_and_expand` (arguments.c:1067-1110): a number
/// in `min..=max` or a percentage of `cur`, formats expanded first.
fn percentage_expanded(
    server: &Server,
    found: Found,
    value: &str,
    min: i64,
    max: i64,
    cur: i64,
) -> Result<i64, NumError> {
    if let Some(p) = value.strip_suffix('%') {
        let n = strtonum(&expand(server, found, p), 0, 1000)?;
        let ll = cur * n / 100;
        if ll < min {
            return Err(NumError::TooSmall);
        }
        if ll > max {
            return Err(NumError::TooLarge);
        }
        return Ok(ll);
    }
    strtonum(&expand(server, found, value), min, max)
}

#[allow(clippy::too_many_lines)] // cmd_split_window_exec, in order
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    for flag in ['B', 'E', 'I', 'k', 'm', 'M', 'R', 's', 'S', 'W'] {
        if args.flag(flag) {
            return Err(EdgeError::NotImplemented(format!("split-window -{flag}")).into());
        }
    }
    if args.positionals.len() == 1 && args.positional(0) == Some("") {
        return Err(EdgeError::NotImplemented("split-window empty pane".into()).into());
    }
    let (Some(s), Some(index), Some(w), Some(wp)) =
        (found.session, found.index, found.window, found.pane)
    else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    // No float-over-zoom panes, so restore_zoom is always 0 and the window
    // is unzoomed before the split (cmd-split-window.c:102-108).
    server.model.unzoom(w);
    let window = &server.model.windows[&w];
    let floating = window.is_floating(wp);
    let horizontal = args.flag('h');
    let before = args.flag('b');
    let full = args.flag('f');
    let zoom = args.flag('Z');

    // The pane is numbered once its cell exists (window_pane_create runs
    // after layout_get_*_cell), so a split with no room uses no ID.
    let new = server.model.pane_ids.peek();
    if floating {
        // layout_get_floating_cell with SPAWN_SPLIT.
        let window = server.model.windows.get_mut(&w).expect("target window");
        let g = window
            .layout
            .split_floating(wp, horizontal, before, full)
            .map_err(|e| EdgeError::Message(e.to_string()))?;
        window.layout.add_floating(Some(wp), new, g);
    } else {
        // layout_get_tiled_cell (layout.c:1640-1695).
        let curval = {
            let r = if full {
                (window.sx(), window.sy())
            } else {
                let rect = server.model.panes[&wp].rect;
                (rect.sx, rect.sy)
            };
            i64::from(if horizontal { r.0 } else { r.1 })
        };
        let geometry =
            |cause: NumError| EdgeError::Message(format!("invalid tiled geometry {cause}"));
        let size = if let Some(l) = args.value('l') {
            Some(
                percentage_expanded(server, found, l, 0, i64::from(i32::MAX), curval)
                    .map_err(geometry)?,
            )
        } else if args.flag('p') {
            let p =
                super::number_expanded(server, found, args.value('p'), 0, 100).map_err(geometry)?;
            Some(curval * p / 100)
        } else {
            None
        };
        let req = SplitRequest {
            horizontal,
            before,
            full,
            size,
        };
        let window = server.model.windows.get_mut(&w).expect("target window");
        window
            .layout
            .split(wp, new, &req)
            .map_err(|e| EdgeError::Message(e.to_string()))?;
    }

    let new = server.model.pane_ids.allocate();
    // spawn_pane: window_add_pane, then the cell gets the pane
    // (layout_assign_pane; -Z leaves the new pane at the window's size).
    let window = server.model.windows.get_mut(&w).expect("target window");
    window.panes.add(
        new,
        Some(wp),
        AddFlags {
            before,
            full,
            floating,
        },
    );
    let window_rect =
        vtmux_core::layout::Geometry::new(window.sx().min(PANE_MAXIMUM), window.sy(), 0, 0);
    let rect = if zoom {
        window_rect
    } else {
        window.layout.pane_rect(new).unwrap_or(window_rect)
    };
    let cwd = match args.value('c') {
        Some(c) => {
            // spawn.c:270-283: expanded against the target session only,
            // relative to the client's directory.
            let session_only = Found {
                session: Some(s),
                ..Found::default()
            };
            let c = expand(server, session_only, c);
            if c.starts_with('/') {
                PathBuf::from(c)
            } else {
                let mut base = item.cwd.clone().into_os_string();
                if !c.is_empty() {
                    base.push("/");
                    base.push(&c);
                }
                PathBuf::from(base)
            }
        }
        None => item.cwd.clone(),
    };
    let extra_env: Vec<String> = args.values('e').map(str::to_owned).collect();
    let session_env = server.model.sessions[&s].env.clone();
    let spawned = server.spawn_pane(&PaneSpawn {
        pane: new,
        window: w,
        session: s,
        session_env: &session_env,
        extra_env: &extra_env,
        client_path: item.client.and(client_path(&item.env)),
        argv: args
            .positionals
            .iter()
            .map(|p| p.text().into_owned())
            .collect(),
        cwd,
        rect,
    });
    let pane = match spawned {
        Ok(pane) => pane,
        Err(e) => {
            // spawn_pane's failure path removes the pane and its cell.
            let window = server.model.windows.get_mut(&w).expect("target window");
            window.layout.close_pane(new);
            window.panes.remove(new);
            server.model.fix_panes(w);
            return Err(EdgeError::Message(format!("create pane failed: {e}")).into());
        }
    };
    server.model.options.add_pane(new, w);
    server.model.panes.insert(new, pane);
    server.model.fix_panes_except(w, zoom.then_some(new));
    if !args.flag('d') {
        server.model.set_active(w, new);
    }
    let new_found = Found {
        session: Some(s),
        index: Some(index),
        window: Some(w),
        pane: Some(new),
        new_index: None,
    };
    if let Some(t) = args.value('T') {
        let title = expand(server, found, t);
        if let Some(p) = server.model.panes.get_mut(&new) {
            p.term.set_title(&title);
        }
    }
    // window_pop_zoom: -Z zooms the (possibly new) active pane.
    server.model.pop_zoom(w, zoom);
    if args.flag('P') {
        let template = args.value('F').unwrap_or(TEMPLATE);
        let text = expand(server, new_found, template);
        item.print(&text);
    }
    Ok(())
}
