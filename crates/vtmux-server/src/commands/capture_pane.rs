//! `capture-pane` (cmd-capture-pane.c:254-485): `-p` to stdout, else into a
//! paste buffer (`-b`), with `-a -q -e -C -J -N -T -L -I -F -S -E`.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;
use vtmux_vt::{CaptureOpts, LineRef, capture, capture_alternate};

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
    for flag in ['H', 'M', 'P', 'R'] {
        if args.flag(flag) {
            return Err(EdgeError::NotImplemented(format!("capture-pane -{flag}")).into());
        }
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
        octal: args.flag('C'),
        keep_spaces: args.flag('N'),
        used_only: args.flag('T'),
        numbers: args.flag('L'),
        times: args.flag('I'),
        flags: args.flag('F'),
        start,
        end,
    };
    let Some(pane) = server.model.panes.get(&p) else {
        return Err(EdgeError::PaneNotFound(p.to_string()).into());
    };
    let mut buf = if args.flag('a') {
        // cmd-capture-pane.c:268-276: no saved screen is an error unless -q.
        match capture_alternate(&pane.term, opts) {
            Some(buf) => buf,
            None if args.flag('q') => Vec::new(),
            None => return Err(EdgeError::Message("no alternate screen".into()).into()),
        }
    } else {
        capture(&pane.term, opts)
    };
    if !args.flag('p') {
        // cmd-capture-pane.c:471-481: the text, trailing newline kept, goes
        // to a buffer named by -b (an automatic one otherwise).
        let limit = super::buffer_limit(server);
        return server
            .model
            .paste
            .set(buf, args.value('b'), limit)
            .map_err(|cause| EdgeError::Message(cause).into());
    }
    // -p: drop one trailing newline, then print with a newline
    // (cmd-capture-pane.c:455-468).
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    buf.push(b'\n');
    item.print_raw(buf);
    Ok(())
}
