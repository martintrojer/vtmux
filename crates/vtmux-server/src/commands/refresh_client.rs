//! `refresh-client` (cmd-refresh-client.c:133-286): `-S` and plain
//! redraws, `-f`/`-F` client flags, `-A` pane output state, `-C` control
//! client sizes and `-l` the clipboard query.
//!
//! Not here yet: `-c -D -L -R -U` (panning a window larger than the
//! terminal: no client offsets in the compositor), `-B` (control
//! subscriptions) and `-r` (colour reports for panes; accepted and
//! ignored).

use vtmux_core::cmd::{Args, CmdError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, WindowId};

use super::Item;
use crate::resize::{WINDOW_MAXIMUM, WINDOW_MINIMUM};
use crate::server::{ClientId, Server};

pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    // CMD_CLIENT_TFLAG: the `-t` client, found before anything else.
    let tc = super::command_prompt::target_client(server, item, args.value('t'))?;

    if ['c', 'L', 'R', 'U', 'D'].iter().any(|&f| args.flag(f)) {
        // The adjustment is checked first (cmd-refresh-client.c:159-169).
        if let Some(adjust) = args.positional(0) {
            strtonum(adjust, 1, i64::from(i32::MAX)).map_err(|cause| EdgeError::BadNumber {
                what: "adjustment",
                cause,
            })?;
        }
        let flag = ['c', 'L', 'R', 'U', 'D']
            .into_iter()
            .find(|&f| args.flag(f))
            .unwrap_or('c');
        return Err(EdgeError::NotImplemented(format!("refresh-client -{flag}")).into());
    }

    if args.flag('l') {
        server.clipboard_query(tc);
        return Ok(());
    }

    // -F is an alias for -f.
    for flag in ['F', 'f'] {
        if let Some(flags) = args.value(flag) {
            server.set_client_flags(tc, flags);
        }
    }
    // -r: the colours a terminal reported for a pane, which only matter to
    // the pane's own colour queries.

    let control = server.is_control(tc);
    if args.flag('A') {
        if !control {
            return Err(not_control());
        }
        for value in args.values('A') {
            update_offset(server, tc, value);
        }
        return Ok(());
    }
    if args.flag('B') {
        if !control {
            return Err(not_control());
        }
        return Err(EdgeError::NotImplemented("refresh-client -B".into()).into());
    }
    if let Some(size) = args.value('C') {
        if !control {
            return Err(not_control());
        }
        return control_size(server, tc, size);
    }

    if args.flag('S') {
        server.status_redraw_client(tc);
    } else {
        server.redraw_client(tc);
    }
    Ok(())
}

fn not_control() -> CmdError {
    EdgeError::Message("not a control client".into()).into()
}

/// `cmd_refresh_client_update_offset`: `%pane:on|off|continue|pause`;
/// anything else is ignored.
fn update_offset(server: &mut Server, tc: ClientId, value: &str) {
    let Some((pane, state)) = value.split_once(':') else {
        return;
    };
    let Ok(pane) = pane.parse::<PaneId>() else {
        return;
    };
    if server.model.panes.contains_key(&pane) {
        server.control_pane_state(tc, pane, state);
    }
}

/// The unsigned number at the start of `s` (`sscanf` `%u`, which skips
/// white space) and what follows it.
fn number(s: &str) -> Option<(u32, &str)> {
    let s = s.trim_start();
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    let n = s[..end].parse::<u64>().ok()?;
    Some((u32::try_from(n & u64::from(u32::MAX)).ok()?, &s[end..]))
}

/// `@%u:%ux%u`.
fn window_size(s: &str) -> Option<(u32, u32, u32)> {
    let (w, rest) = number(s.strip_prefix('@')?)?;
    let (x, rest) = number(rest.strip_prefix(':')?)?;
    let (y, _) = number(rest.strip_prefix('x')?)?;
    Some((w, x, y))
}

/// `%u,%u` or `%ux%u`.
fn size(s: &str) -> Option<(u32, u32)> {
    let pair = |sep: char| {
        let (x, rest) = number(s)?;
        let (y, _) = number(rest.strip_prefix(sep)?)?;
        Some((x, y))
    };
    pair(',').or_else(|| pair('x'))
}

fn too_big(x: u32, y: u32) -> bool {
    !(WINDOW_MINIMUM..=WINDOW_MAXIMUM).contains(&x)
        || !(WINDOW_MINIMUM..=WINDOW_MAXIMUM).contains(&y)
}

/// `cmd_refresh_client_control_client_size`: `-C @N:XxY` for one window,
/// `-C @N:` to forget it, `-C X,Y` or `XxY` for the client.
fn control_size(server: &mut Server, tc: ClientId, text: &str) -> Result<(), CmdError> {
    let bad = |m: &str| CmdError::from(EdgeError::Message(m.to_owned()));
    if let Some((w, x, y)) = window_size(text) {
        if too_big(x, y) {
            return Err(bad("size too small or too big"));
        }
        server.control_window_size(tc, WindowId(w), Some((x, y)));
        if let Some(c) = server.clients.get_mut(&tc) {
            c.flags.window_size_changed = true;
        }
        server.recalculate_sizes_now(true);
        return Ok(());
    }
    if let Some((w, rest)) = text.strip_prefix('@').and_then(number)
        && rest.starts_with(':')
    {
        server.control_window_size(tc, WindowId(w), None);
        server.recalculate_sizes_now(true);
        return Ok(());
    }
    let Some((x, y)) = size(text) else {
        return Err(bad("bad size argument"));
    };
    if too_big(x, y) {
        return Err(bad("size too small or too big"));
    }
    server.control_set_size(tc, (x, y));
    if let Some(c) = server.clients.get_mut(&tc) {
        c.flags.size_changed = true;
    }
    server.recalculate_sizes_now(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sscanf` forms of cmd-refresh-client.c:68-95.
    #[test]
    fn size_forms() {
        assert_eq!(size("100,50"), Some((100, 50)));
        assert_eq!(size("100x50"), Some((100, 50)));
        assert_eq!(size("100x50junk"), Some((100, 50)));
        assert_eq!(size("100"), None);
        assert_eq!(size("x50"), None);
        assert_eq!(window_size("@3:40x10"), Some((3, 40, 10)));
        assert_eq!(window_size("@3:"), None);
        assert!(too_big(0, 5) && too_big(5, 10_001) && !too_big(1, 10_000));
    }
}
