//! `display-popup` (cmd-display-menu.c:54-66, 100-271, 569-720): size and
//! position from `-w -h -x -y` (formats with the `popup_*` variables),
//! then the popup ([`crate::popup`]); the command waits until it closes.
//! With a popup already open the command changes it instead
//! (`popup_modify`); `-C` closes it.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_core::layout::percentage;
use vtmux_core::options::TreeId;
use vtmux_core::style::RangeKind;

use super::Item;
use crate::popup::{CloseRules, Command, Open};
use crate::render::popup::BoxLines;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `cmd_display_popup_exec`.
pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    // CMD_CLIENT_CFLAG: the -c client, or the current one.
    let tc = super::command_prompt::target_client(server, item, args.value('c'))?;
    if args.flag('C') {
        server.popup_close(tc);
        return Ok(());
    }
    // cmd-display-menu.c:594: a control client cannot show a popup.
    if server.is_control(tc) {
        return Ok(());
    }
    let modify = server.popup_present(tc);
    let Some((cols, rows)) = server.tty_size(tc) else {
        return Ok(());
    };
    let Some(s) = found.session.or_else(|| server.client_session(tc)) else {
        return Ok(());
    };
    let mut geometry = None;
    if !modify {
        let size = |flag: char, cur: u16, what: &str| -> Result<u16, CmdError> {
            let Some(v) = args.value(flag) else {
                return Ok(cur / 2);
            };
            let n = percentage(v, 1, i64::from(cur), i64::from(cur))
                .map_err(|e| EdgeError::Message(format!("{what} {e}")))?;
            Ok(u16::try_from(n).unwrap_or(cur))
        };
        let h = size('h', rows, "height")?;
        let w = size('w', cols, "width")?;
        let Some((px, py)) = position(server, tc, s, found, args, (w, h), (cols, rows)) else {
            return Ok(());
        };
        geometry = Some((px, py, w, h));
    }
    let lines =
        if args.flag('B') {
            Some(BoxLines::None)
        } else if let Some(v) = args.value('b') {
            Some(BoxLines::from_name(v).ok_or_else(|| {
                EdgeError::Message(format!("popup-border-lines unknown value: {v}"))
            })?)
        } else {
            None
        };
    let ctx = server.format_ctx(found, Some(tc));
    let title = args
        .value('T')
        .map(|t| vtmux_core::format::expand_str(t, &ctx))
        .unwrap_or_default();
    drop(ctx);
    // cmd-display-menu.c:672-689: -N or a new popup starts from no flags;
    // a modify without -N, -E or -k keeps the popup's.
    let mut rules = (args.flag('N') || !modify).then(CloseRules::default);
    match args.count('E') {
        0 => {}
        1 => rules.get_or_insert_default().on_exit = true,
        _ => rules.get_or_insert_default().on_exit_zero = true,
    }
    if args.flag('k') {
        rules.get_or_insert_default().any_key = true;
    }
    let style = args.value('s').map(str::to_owned);
    let border_style = args.value('S').map(str::to_owned);
    let Some((px, py, sx, sy)) = geometry else {
        server.popup_modify(tc, title, style, border_style, lines, rules);
        return Ok(());
    };
    let open = Open {
        session: s,
        px,
        py,
        sx,
        sy,
        lines,
        rules: rules.unwrap_or_default(),
        title,
        style,
        border_style,
        command: command(server, s, args),
        cwd: match args.value('d') {
            Some(d) => super::expand(server, found, d).into(),
            None => server.client_cwd(Some(tc), Some(s)),
        },
        env: args.values('e').map(str::to_owned).collect(),
        step: item.step,
        caller: item.client,
    };
    if server.popup_display(tc, open) {
        item.blocked = true;
    }
    Ok(())
}

/// What the popup runs (cmd-display-menu.c:631-643): `default-command` or
/// the one argument through the shell; with neither, the shell itself;
/// more arguments as they are.
fn command(server: &Server, s: vtmux_core::ids::SessionId, args: &Args) -> Command {
    let tree = TreeId::Session(s);
    let words: Vec<String> = args
        .positionals
        .iter()
        .map(|a| a.text().into_owned())
        .collect();
    let one = match words.as_slice() {
        [] => server
            .model
            .options
            .get(tree, "default-command")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default(),
        [one] => one.clone(),
        _ => return Command::Argv(words),
    };
    if one.is_empty() {
        let shell = server.default_shell(tree);
        return Command::Argv(vec![shell.to_string_lossy().into_owned()]);
    }
    Command::Shell(one)
}

/// `cmd_display_menu_get_popup_pos` (cmd-display-menu.c:100-271): `-x`
/// and `-y` expanded with the `popup_*` variables (`-y` is the bottom
/// line), then fitted to the client. `None` when the popup is larger than
/// the client.
#[expect(
    clippy::many_single_char_names,
    reason = "cmd-display-menu.c's w/h/sx/sy"
)]
fn position(
    server: &Server,
    tc: ClientId,
    s: vtmux_core::ids::SessionId,
    found: Found,
    args: &Args,
    (w, h): (u16, u16),
    (sx, sy): (u16, u16),
) -> Option<(u16, u16)> {
    if w > sx || h > sy {
        return None;
    }
    let (w, h, sx, sy) = (i64::from(w), i64::from(h), i64::from(sx), i64::from(sy));
    let mut vars: Vec<(&'static str, i64)> = Vec::new();
    // No menu has been shown (menus are not implemented): menu_last_px/py
    // are 0.
    vars.push(("popup_last_x", 0));
    vars.push(("popup_last_y", h));
    let client = server.clients.get(&tc)?;
    let lines = i64::from(server.status_line_size(client));
    let top = server.model.status_top(TreeId::Session(s));
    let at_top = lines != 0 && top;
    if lines != 0 {
        let wl = found.index;
        let source = server.render_source();
        let ctx = server.format_ctx(Found::from_session(&server.model, s), Some(tc));
        let ranges = crate::render::status::status_ranges(
            &source,
            &ctx,
            s,
            usize::try_from(sx).unwrap_or(0),
            usize::try_from(lines).unwrap_or(0),
        );
        let hit = ranges.iter().enumerate().find_map(|(line, r)| {
            r.iter()
                .find(|r| matches!(r.kind, RangeKind::Window(n) if Some(n) == wl))
                .map(|r| (line, r.start))
        });
        if let Some((line, start)) = hit {
            let line = i64::try_from(line).unwrap_or(0);
            vars.push((
                "popup_window_status_line_x",
                i64::try_from(start).unwrap_or(0),
            ));
            vars.push((
                "popup_window_status_line_y",
                if at_top {
                    line + 1 + h
                } else {
                    sy - lines + line
                },
            ));
        }
        vars.push((
            "popup_status_line_y",
            if at_top { lines + h } else { sy - lines },
        ));
    }
    vars.push(("popup_width", w));
    vars.push(("popup_height", h));
    vars.push(("popup_centre_x", ((sx - 1) / 2 - w / 2).max(0)));
    let n = (sy - 1) / 2 + h / 2;
    vars.push(("popup_centre_y", if n >= sy { sy - h } else { n }));
    // The target pane, in the client's view (tty_window_offset: the
    // window is never larger than the client here, so no offset).
    if let Some(pane) = found.pane.and_then(|p| server.model.panes.get(&p)) {
        let top = if at_top { lines } else { 0 };
        let (xoff, yoff) = (i64::from(pane.rect.xoff), i64::from(pane.rect.yoff));
        let (psx, psy) = (i64::from(pane.rect.sx), i64::from(pane.rect.sy));
        let n = top + yoff + h;
        vars.push(("popup_pane_top", if n >= sy { sy - h } else { n }));
        vars.push(("popup_pane_bottom", top + yoff + psy));
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
    let mut x = expand(xp);
    if x + w >= sx {
        x = sx - w;
    } else if x < 0 {
        x = 0;
    }
    let yp = match args.value('y') {
        None | Some("C") => "#{popup_centre_y}",
        Some("P") => "#{popup_pane_bottom}",
        Some("M") => "#{popup_mouse_top}",
        Some("L") => "#{popup_last_y}",
        Some("S") => "#{popup_status_line_y}",
        Some("W") => "#{popup_window_status_line_y}",
        Some(other) => other,
    };
    let mut y = expand(yp);
    y = if y < h { 0 } else { y - h };
    if y + h >= sy {
        y = sy - h;
    }
    Some((u16::try_from(x).ok()?, u16::try_from(y).ok()?))
}

/// `strtol(p, NULL, 10)`: leading blanks, a sign, the digits there are;
/// 0 when there are none.
fn strtol(s: &str) -> i64 {
    let s = s.trim_start();
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n = digits.parse::<i64>().unwrap_or(0);
    if neg { -n } else { n }
}

#[cfg(test)]
mod tests {
    use super::strtol;

    #[test]
    fn strtol_reads_a_leading_number() {
        assert_eq!(strtol(" 12abc"), 12);
        assert_eq!(strtol("-3"), -3);
        assert_eq!(strtol("x"), 0);
        assert_eq!(strtol(""), 0);
    }
}
