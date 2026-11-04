//! `split-window` and `new-pane` (cmd-split-window.c:73-345, layout.c
//! `layout_get_tiled_cell`, `layout_get_floating_cell`): a new pane next to
//! the target, or next to the whole layout with `-f`; a split of a floating
//! pane floats beside it. `new-pane` makes a floating pane (`-x -y` its
//! size, `-X -Y` its place, default staggered) unless `-L` asks for a tiled
//! one; `new-pane -M` sizes it by the mouse drag that ran it. `-E`, `-I` or
//! one empty argument make an empty pane (no process); `-W` waits for the
//! pane's process to exit; `-k -m` keep it once it exits (remain-on-exit).
//! Modal (`-O` with `-C -D -K`) and float-over-zoom (`-A`) panes are not
//! done yet.

use std::path::PathBuf;

use vtmux_core::cmd::{Args, CmdError, NumError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::layout::{AddFlags, PANE_MAXIMUM, SplitRequest};
use vtmux_core::options::{TreeId, Value};

use super::{Item, expand};
use crate::server::{PaneSpawn, Server, client_path};
use crate::target::Found;

/// `SPLIT_WINDOW_TEMPLATE` (cmd-split-window.c:33).
const TEMPLATE: &str = "#{session_name}:#{window_index}.#{pane_index}";

/// `args_string_percentage_and_expand` (arguments.c:1067-1110): a number
/// in `min..=max` or a percentage of `cur`, formats expanded first.
pub(super) fn percentage_expanded(
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
    new_pane: bool,
) -> Result<(), CmdError> {
    if args.flag('O') {
        return Err(EdgeError::NotImplemented("new-pane -O".into()).into());
    }
    if args.flag('A') {
        return Err(EdgeError::NotImplemented("new-pane -A".into()).into());
    }
    let (Some(w), Some(wp)) = (found.window, found.pane) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    // cmd-split-window.c:97-108: new-pane floats unless -L; a split floats
    // when its target does.
    let floating = if new_pane {
        !args.flag('L')
    } else {
        server.model.windows[&w].is_floating(wp)
    };
    // cmd-split-window.c:124-127: -M needs the mouse event that ran it.
    let drag_client = item.mouse.as_ref().and(item.client);
    if new_pane && floating && args.flag('M') && drag_client.is_none() {
        return Ok(());
    }
    // cmd-split-window.c:149-161: -I, or one empty argument, is an empty
    // pane like -E, and a command with one is an error.
    let empty_command = args.positionals.len() == 1 && args.positional(0) == Some("");
    let input = args.flag('I');
    let empty = input || empty_command || args.flag('E');
    if empty && !args.positionals.is_empty() && !empty_command {
        return Err(EdgeError::Message("command cannot be given for empty pane".into()).into());
    }
    // cmd-split-window.c:163-173: -B is a pane-border-lines choice.
    let lines = match args.value('B') {
        Some(v) => Some(
            vtmux_core::options::find("pane-border-lines")
                .and_then(|def| match def.kind {
                    vtmux_core::options::Kind::Choice(names) => names.iter().rposition(|n| *n == v),
                    _ => None,
                })
                .ok_or_else(|| {
                    EdgeError::Message(format!("pane-border-lines unknown value: {v}"))
                })?,
        ),
        None => None,
    };
    let (Some(s), Some(index)) = (found.session, found.index) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    // No float-over-zoom panes, so restore_zoom is always 0: the window is
    // unzoomed before the split (cmd-split-window.c:102-108), or by
    // window_push_zoom in layout_get_*_cell for new-pane.
    server.model.unzoom(w);
    let window = &server.model.windows[&w];
    // cmd-split-window.c:163-173: -B, else the window's pane-border-lines.
    let border = lines.map_or_else(
        || server.model.window_has_border(w),
        |n| i64::try_from(n).ok() != Some(crate::model::LINES_NONE),
    );
    let horizontal = args.flag('h');
    let before = args.flag('b');
    let full = args.flag('f');
    let zoom = args.flag('Z');

    // The pane is numbered once its cell exists (window_pane_create runs
    // after layout_get_*_cell), so a split with no room uses no ID.
    let new = server.model.pane_ids.peek();
    if floating {
        let window = server.model.windows.get_mut(&w).expect("target window");
        let g = if new_pane {
            // layout_floating_args_parse (layout.c:1727-1832).
            let fa = vtmux_core::layout::FloatArgs {
                width: args.value('x').map(|v| expand(server, found, v)),
                height: args.value('y').map(|v| expand(server, found, v)),
                x: args.value('X').map(|v| expand(server, found, v)),
                y: args.value('Y').map(|v| expand(server, found, v)),
            };
            let window = server.model.windows.get_mut(&w).expect("target window");
            window
                .layout
                .floating_geometry(&fa, border, None)
                .map_err(EdgeError::Message)?
        } else {
            // layout_split_floating_cell with SPAWN_SPLIT.
            window
                .layout
                .split_floating(
                    wp,
                    &SplitRequest {
                        horizontal,
                        before,
                        full,
                        size: None,
                    },
                    border,
                )
                .map_err(|e| EdgeError::Message(e.to_string()))?
        };
        let window = server.model.windows.get_mut(&w).expect("target window");
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
        shell: None,
        empty,
    });
    let mut pane = match spawned {
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
    // window_add_pane creates the pane at the window's size and
    // layout_assign_pane resizes it to its cell (window.c:1156, 1702-1740):
    // that queued size changes nothing for the program, which started at
    // the cell's size, but sending it starts the pane's resize timer.
    let created = (window_rect.sx, window_rect.sy);
    let assigned = !zoom && created != (rect.sx, rect.sy);
    if assigned {
        pane.resizes.push((created, (rect.sx, rect.sy)));
    }
    server.model.options.add_pane(new, w);
    // cmd-split-window.c:254-255 (set here, before the pane's rectangle is
    // worked out, which reads it for a floating pane's padding).
    if let Some(n) = lines {
        server.model.options.set_table_value(
            TreeId::Pane(new),
            "pane-border-lines",
            Value::Choice(n),
        );
    }
    // cmd-split-window.c:256-262: -k and -m keep the pane until a key once
    // its program goes, -m with its own dead line.
    if args.flag('k') || args.flag('m') {
        let options = &mut server.model.options;
        options.set_table_value(TreeId::Pane(new), "remain-on-exit", Value::Choice(3));
        if let Some(m) = args.value('m') {
            options.set_table_value(
                TreeId::Pane(new),
                "remain-on-exit-format",
                Value::String(m.to_owned()),
            );
        }
    }
    server.model.panes.insert(new, pane);
    // That resize is a window_pane_resize in layout_fix_panes, so it
    // fires pane-resized, in window order with the other panes'.
    server
        .model
        .fix_panes_resized_from(w, zoom.then_some(new), assigned.then_some((new, created)));
    let new_found = Found {
        session: Some(s),
        index: Some(index),
        window: Some(w),
        pane: Some(new),
        new_index: None,
    };
    // cmd-split-window.c:225-255: the new pane's own styles (unchecked,
    // options_set_string) and border lines.
    let pane_tree = vtmux_core::options::TreeId::Pane(new);
    for (flag, names) in [
        ('s', &["window-style", "window-active-style"][..]),
        ('S', &["pane-active-border-style"][..]),
        ('R', &["pane-border-style"][..]),
    ] {
        if let Some(style) = args.value(flag) {
            for name in names {
                server.model.options.set_table_value(
                    pane_tree,
                    name,
                    vtmux_core::options::Value::String(style.to_owned()),
                );
            }
        }
    }
    let title = args.value('T').map(|t| expand(server, found, t));
    if let Some(title) = &title
        && let Some(p) = server.model.panes.get_mut(&new)
    {
        p.term.set_title(title);
    }
    // spawn_pane (spawn.c:590-612): pane-created, the new pane active
    // (window-pane-changed, above), then window-layout-changed.
    server.fire_pane_created(s, index, new, empty, false);
    if !args.flag('d') {
        server.model.set_active(w, new);
        // cmd-split-window.c:288-289.
        item.new_current = Some(new_found);
    }
    server.model.layout_changed(w);
    // cmd-split-window.c:264-275: -T fires pane-title-changed (whether or
    // not the title changed) after spawn_pane's events.
    if let Some(title) = title {
        let ev = crate::events::Event::new("pane-title-changed")
            .target(Found::from_pane(&server.model, new))
            .pane("pane", new)
            .window(&server.model, "window", w)
            .text("new_title", title);
        server.model.push_event(ev);
    }
    // window_pop_zoom: -Z zooms the (possibly new) active pane; never
    // after a floating pane (cmd-split-window.c:288-294).
    server.model.pop_zoom(w, zoom && !floating);
    // cmd-split-window.c:300-304: the drag that ran new-pane -M sizes it.
    if new_pane
        && floating
        && args.flag('M')
        && let (Some(c), Some(ev)) = (drag_client, item.mouse.clone())
    {
        server.start_new_pane_drag(c, new, &ev);
    }
    // cmd-split-window.c:314-315.
    item.hook_target = Some(Some(new_found));
    if args.flag('P') {
        let template = args.value('F').unwrap_or(TEMPLATE);
        let text = expand(server, new_found, template);
        item.print(&text);
    }
    // cmd-split-window.c:276-286, 320-321: -I reads the client's stdin into
    // the new pane, the command waiting until it ends.
    if input {
        return super::file_io::pane_input(server, item, new).map_err(Into::into);
    }
    // cmd-split-window.c:322-330: -W waits until the pane's process exits
    // (window_pane_wait_finish).
    if args.flag('W')
        && let Some(step) = item.step
        && let Some(pane) = server.model.panes.get_mut(&new)
    {
        pane.wait_step = Some(step);
        item.blocked = true;
    }
    Ok(())
}
