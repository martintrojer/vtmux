//! `capture-pane` (cmd-capture-pane.c:254-485): `-p` to stdout with `-e`,
//! `-J`, `-S`, `-E`. Paste buffers are M1.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_vt::{CaptureOpts, LineRef, capture};

use super::Item;
use crate::server::Server;
use crate::target::Found;

/// `-S` / `-E`: `-` is the edge; otherwise a format-expanded number in
/// `INT_MIN..=SHRT_MAX`, and anything invalid is the default
/// (cmd-capture-pane.c:294-326).
fn line_ref(server: &Server, found: Found, value: Option<&str>) -> LineRef {
    match value {
        None => LineRef::Visible,
        Some("-") => LineRef::Start,
        Some(v) => super::number_expanded(
            server,
            found,
            Some(v),
            i64::from(i32::MIN),
            i64::from(i16::MAX),
        )
        .map_or(LineRef::Visible, LineRef::Line),
    }
}

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    for flag in ['a', 'C', 'H', 'M', 'N', 'P', 'R', 'T', 'L', 'F', 'I'] {
        if args.flag(flag) {
            return Err(EdgeError::NotImplemented(format!("capture-pane -{flag}")).into());
        }
    }
    if !args.flag('p') {
        return Err(EdgeError::NotImplemented("capture-pane into a buffer".into()).into());
    }
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    let start = line_ref(server, found, args.value('S'));
    let end = match args.value('E') {
        Some("-") => LineRef::End,
        v => line_ref(server, found, v),
    };
    let opts = CaptureOpts {
        escapes: args.flag('e'),
        join: args.flag('J'),
        start,
        end,
    };
    let Some(pane) = server.model.panes.get(&p) else {
        return Err(EdgeError::PaneNotFound(p.to_string()).into());
    };
    let mut buf = capture(&pane.term, opts);
    // -p: drop one trailing newline, then print with a newline
    // (cmd-capture-pane.c:455-468).
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    buf.push(b'\n');
    item.print_raw(buf);
    Ok(())
}
