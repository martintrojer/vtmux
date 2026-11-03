//! `display-menu` (cmd-display-menu.c:37-51, 69-92, 291-566): the items
//! from `name key command` groups (an empty name is a separator), placed
//! by `-x -y` (formats with the `popup_*` variables, in window
//! coordinates), then shown on the target's window ([`crate::menu`]). The
//! command does not wait for a choice.

use vtmux_core::cmd::{Args, CmdError, NumError};
use vtmux_core::error::EdgeError;
use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;
use vtmux_core::style::RangeKind;

use super::Item;
use super::display_popup::strtol;
use crate::menu::{Items, Menu};
use crate::render::popup::BoxLines;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `cmd_display_menu_exec`.
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    // CMD_CLIENT_CFLAG: the -c client, or the current one.
    let tc = super::command_prompt::target_client(server, item, args.value('c'))?;
    let start = match args.value('C') {
        None => 0,
        Some("-") => -1,
        Some(_) => args
            .number('C', 0, i64::from(u32::MAX))
            .map_err(|e: NumError| EdgeError::Message(format!("starting choice {e}")))?,
    };
    let Some(w) = found.window else {
        return Ok(());
    };
    let ctx = server.format_ctx(found, Some(tc));
    let title = args
        .value('T')
        .map(|t| vtmux_core::format::expand_str(t, &ctx))
        .unwrap_or_default();
    // menu_add_item: `c->tty.sx`.
    let cols = server.tty_size(tc).map_or(80, |(c, _)| u32::from(c));
    let mut items = Items::new(title);
    let words: Vec<String> = args
        .positionals
        .iter()
        .map(|a| a.text().into_owned())
        .collect();
    let mut i = 0;
    while i < words.len() {
        let name = &words[i];
        i += 1;
        if name.is_empty() {
            items.separator();
            continue;
        }
        if words.len() - i < 2 {
            return Err(EdgeError::Message("not enough arguments".into()).into());
        }
        let key = Key::from_name(&words[i]);
        let command = &words[i + 1];
        i += 2;
        // format_single_from_state for the name and the command, at once.
        let text = vtmux_core::format::expand_str(name, &ctx);
        let command = vtmux_core::format::expand_str(command, &ctx);
        items.item(&text, key, command, cols);
    }
    drop(ctx);
    if items.entries.is_empty() {
        return Ok(());
    }
    let size = (
        u32::try_from(items.width + 4).unwrap_or(u32::MAX),
        u32::try_from(items.entries.len() + 2).unwrap_or(u32::MAX),
    );
    let Some((px, py)) = position(server, item, tc, found, args, size) else {
        return Ok(());
    };
    let lines = match args.value('b') {
        Some(v) => BoxLines::from_name(v)
            .ok_or_else(|| EdgeError::Message(format!("menu-border-lines unknown value: {v}")))?,
        // menu_display: the window's option.
        None => BoxLines::from_choice(
            server
                .model
                .number_option(TreeId::Window(w), "menu-border-lines"),
        ),
    };
    let no_mouse = item.mouse.is_none() && !args.flag('M');
    let mut menu = Menu {
        items,
        px,
        py,
        choice: None,
        lines,
        style: args.value('s').map(str::to_owned),
        selected_style: args.value('H').map(str::to_owned),
        border_style: args.value('S').map(str::to_owned),
        stay_open: args.flag('O'),
        no_mouse,
        found,
        key: item.key,
        mouse: item.mouse.clone(),
    };
    if no_mouse {
        menu.start_at(start);
    }
    server.menu_display(w, menu);
    Ok(())
}

/// `cmd_display_menu_get_menu_pos` (cmd-display-menu.c:291-458): `-x` and
/// `-y` expanded with the `popup_*` variables in the target window's
/// coordinates (`-y` is the bottom line), then kept inside the window.
#[expect(
    clippy::many_single_char_names,
    reason = "cmd-display-menu.c's w/h/sx/sy"
)]
#[expect(clippy::too_many_lines, reason = "one variable per popup_* format")]
fn position(
    server: &Server,
    item: &Item,
    tc: ClientId,
    found: Found,
    args: &Args,
    (w, h): (u32, u32),
) -> Option<(u32, u32)> {
    let window = server.model.windows.get(&found.window?)?;
    let client = server.clients.get(&tc)?;
    let (w, h) = (i64::from(w), i64::from(h));
    let (wsx, wsy) = (i64::from(window.sx()), i64::from(window.sy()));
    let max_x = (wsx - w).max(0);
    let max_y = (wsy - h).max(0);
    let lines = i64::from(server.status_line_size(client));
    // tty_window_offset: the window is never panned here, so no offset;
    // the visible rows are the window's, or the client's below the status.
    let rows = server.tty_size(tc).map_or(wsy, |(_, r)| i64::from(r));
    let sy = wsy.min(rows - lines);
    let mut vars: Vec<(&'static str, i64)> = Vec::new();
    if let Some(m) = &item.mouse {
        let x = i64::from(m.input.x);
        let y = i64::from(m.input.y);
        let my = match m.status_at {
            Some(0) if y >= i64::from(m.status_lines) => y - i64::from(m.status_lines),
            Some(0) => 0,
            Some(at) if y >= i64::from(at) => sy - 1,
            _ => y,
        };
        // D23: the window sits one cell in under the focus ring.
        let off = i64::from(m.offset);
        let (mx, my) = ((x - off).max(0), (my - off).max(0));
        vars.push(("popup_mouse_x", mx));
        vars.push(("popup_mouse_y", my));
        vars.push(("popup_mouse_centre_x", (mx - w / 2).max(0)));
        let n = my - h / 2;
        vars.push(("popup_mouse_centre_y", if n + h >= wsy { max_y } else { n }));
        let n = my + h;
        vars.push(("popup_mouse_top", if n >= wsy { wsy - 1 } else { n }));
        vars.push(("popup_mouse_bottom", (my - h).max(0)));
    }
    let (last_x, last_y) = window.menu_last;
    vars.push(("popup_last_x", i64::from(last_x)));
    vars.push(("popup_last_y", i64::from(last_y) + h));
    if lines != 0 {
        let s = client.attached?;
        let top = server.model.status_top(TreeId::Session(s));
        let source = server.render_source();
        let ctx = server.format_ctx(Found::from_session(&server.model, s), Some(tc));
        let cols = server.tty_size(tc).map_or(80, |(c, _)| usize::from(c));
        let ranges = crate::render::status::status_ranges(
            &source,
            &ctx,
            s,
            cols,
            usize::try_from(lines).unwrap_or(0),
        );
        let hit = ranges.iter().find_map(|r| {
            r.iter()
                .find(|r| matches!(r.kind, RangeKind::Window(n) if Some(n) == found.index))
                .map(|r| r.start)
        });
        let line_y = if top { h } else { wsy };
        if let Some(start) = hit {
            vars.push((
                "popup_window_status_line_x",
                i64::try_from(start).unwrap_or(0),
            ));
            vars.push(("popup_window_status_line_y", line_y));
        }
        vars.push(("popup_status_line_y", line_y));
    }
    vars.push(("popup_width", w));
    vars.push(("popup_height", h));
    vars.push(("popup_centre_x", ((wsx - 1) / 2 - w / 2).max(0)));
    let n = (wsy - 1) / 2 + h / 2;
    vars.push(("popup_centre_y", if n >= wsy { max_y } else { n }));
    if let Some(pane) = found.pane.and_then(|p| server.model.panes.get(&p)) {
        let (xoff, yoff) = (i64::from(pane.rect.xoff), i64::from(pane.rect.yoff));
        let (psx, psy) = (i64::from(pane.rect.sx), i64::from(pane.rect.sy));
        let n = yoff + h;
        vars.push(("popup_pane_top", if n >= wsy { max_y } else { n }));
        vars.push(("popup_pane_bottom", yoff + psy));
        vars.push(("popup_pane_left", xoff));
        vars.push(("popup_pane_right", (xoff + psx - w).max(0)));
    }
    let mut ctx = server.format_ctx(found, Some(tc));
    ctx.extra
        .extend(vars.into_iter().map(|(k, v)| (k.into(), v.to_string())));
    let expand = |spec: &str| strtol(&vtmux_core::format::expand_str(spec, &ctx));
    let xp = match args.value('x') {
        None | Some("C") => "#{popup_centre_x}",
        Some("R") => "#{popup_pane_right}",
        Some("P") => "#{popup_pane_left}",
        Some("M") => "#{popup_mouse_centre_x}",
        Some("L") => "#{popup_last_x}",
        Some("W") => "#{popup_window_status_line_x}",
        Some(other) => other,
    };
    let x = expand(xp).clamp(0, max_x);
    let yp = match args.value('y') {
        None | Some("C") => "#{popup_centre_y}",
        Some("P") => "#{popup_pane_bottom}",
        Some("M") => "#{popup_mouse_top}",
        Some("L") => "#{popup_last_y}",
        Some("S") => "#{popup_status_line_y}",
        Some("W") => "#{popup_window_status_line_y}",
        Some(other) => other,
    };
    let y = expand(yp);
    let y = if y < h { 0 } else { y - h }.min(max_y);
    Some((u32::try_from(x).ok()?, u32::try_from(y).ok()?))
}
