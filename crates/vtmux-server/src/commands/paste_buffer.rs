//! `paste-buffer` (cmd-paste-buffer.c:60-150).

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use crate::paste::vis_paste;
use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, args: &Args, found: Found) -> Result<(), CmdError> {
    let p = found.pane.ok_or(EdgeError::NoCurrentTarget)?;
    let Some(pane) = server.model.panes.get(&p) else {
        return Err(EdgeError::PaneNotFound(p.to_string()).into());
    };
    if pane.exited || pane.io.is_none() {
        return Err(EdgeError::Message("target pane has exited".into()).into());
    }
    let pb = match args.value('b') {
        None => server.model.paste.top(),
        Some(n) => Some(
            server
                .model
                .paste
                .get(n)
                .ok_or_else(|| EdgeError::Message(format!("no buffer {n}")))?,
        ),
    };
    let Some(pb) = pb else {
        return Ok(());
    };
    let name = pb.name.clone();

    if !pane.input_off {
        let sep = args
            .value('s')
            .unwrap_or(if args.flag('r') { "\n" } else { "\r" })
            .as_bytes();
        let bracket = args.flag('p') && pane.term.bracketed_paste();
        let raw = args.flag('S');
        let mut out = Vec::new();
        if bracket {
            out.extend_from_slice(b"\x1b[200~");
        }
        let mut rest = &pb.data[..];
        let piece = |out: &mut Vec<u8>, line: &[u8]| {
            if raw {
                out.extend_from_slice(line);
            } else {
                out.extend_from_slice(&vis_paste(line));
            }
        };
        while let Some(at) = rest.iter().position(|&b| b == b'\n') {
            piece(&mut out, &rest[..at]);
            out.extend_from_slice(sep);
            rest = &rest[at + 1..];
        }
        if !rest.is_empty() {
            piece(&mut out, rest);
        }
        if bracket {
            out.extend_from_slice(b"\x1b[201~");
        }
        if let Some(io) = server
            .model
            .panes
            .get_mut(&p)
            .and_then(|pane| pane.io.as_mut())
        {
            io.write(&out);
        }
        server.pane_output_queued(p);
    }
    if args.flag('d') {
        server.model.paste.remove(&name);
    }
    Ok(())
}
