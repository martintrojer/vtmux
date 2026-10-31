//! `resize-window` (cmd-resize-window.c:47-117): a size from `-x`/`-y`,
//! adjusted by `-L -R -U -D` (one of them, in that order), or the largest
//! (`-A`) or smallest (`-a`) of the window's clients; the window becomes
//! `window-size manual` at that size and is resized at once.

use vtmux_core::cmd::{Args, CmdError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::options::{TreeId, Value};

use super::Item;
use crate::resize::{SizePolicy, WINDOW_MAXIMUM, WINDOW_MINIMUM};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let (Some(s), Some(w)) = (found.session, found.window) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let adjust = match args.positional(0) {
        None => 1,
        Some(a) => strtonum(a, 1, i64::from(i32::MAX))
            .map_err(|cause| EdgeError::Message(format!("adjustment {cause}")))?,
    };
    let adjust = u32::try_from(adjust).expect("in i32 range");
    let window = &server.model.windows[&w];
    let (mut sx, mut sy) = (window.sx(), window.sy());
    let bound = |flag: char, what: &'static str| -> Result<Option<u32>, EdgeError> {
        if !args.flag(flag) {
            return Ok(None);
        }
        args.number(flag, i64::from(WINDOW_MINIMUM), i64::from(WINDOW_MAXIMUM))
            .map(|n| Some(u32::try_from(n).expect("in window range")))
            .map_err(|cause| EdgeError::BadNumber { what, cause })
    };
    if let Some(x) = bound('x', "width")? {
        sx = x;
    }
    if let Some(y) = bound('y', "height")? {
        sy = y;
    }
    if args.flag('L') {
        if sx >= adjust {
            sx -= adjust;
        }
    } else if args.flag('R') {
        sx += adjust;
    } else if args.flag('U') {
        if sy >= adjust {
            sy -= adjust;
        }
    } else if args.flag('D') {
        sy += adjust;
    }
    if args.flag('A') {
        (sx, sy) = server.default_window_size(None, s, Some(w), Some(SizePolicy::Largest));
    } else if args.flag('a') {
        (sx, sy) = server.default_window_size(None, s, Some(w), Some(SizePolicy::Smallest));
    }
    server.model.options.set_table_value(
        TreeId::Window(w),
        "window-size",
        Value::Choice(SizePolicy::Manual.choice()),
    );
    if let Some(window) = server.model.windows.get_mut(&w) {
        window.manual_size = (sx, sy);
    }
    server.recalculate_size(w, true);
    Ok(())
}
