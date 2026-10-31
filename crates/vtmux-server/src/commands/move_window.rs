//! `link-window` and `move-window` (cmd-move-window.c:62-130, which both
//! use `cmd_move_window_exec`; `server_link_window`, server-fn.c:283-336).
//! `move-window -r` renumbers a session. Session groups do not exist yet,
//! so the `sessions are grouped` error is not reachable.

use vtmux_core::cmd::{Args, CmdError, FindFlags, TargetKind};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::model::Redraw;
use crate::server::Server;
use crate::target::{self, Found};

pub fn exec(
    server: &mut Server,
    item: &mut Item,
    args: &Args,
    source: Found,
    moving: bool,
) -> Result<(), CmdError> {
    if args.flag('r') {
        // The lookup is quiet: a failure drops the queue without a message
        // (and so without an exit status).
        let spec = args.target('t');
        let found = target::resolve(
            &server.model,
            item.origin,
            spec.as_ref(),
            TargetKind::Session,
            FindFlags::NONE,
        )
        .map_err(|_| CmdError::Quiet)?;
        let s = found.session.ok_or(CmdError::Quiet)?;
        server.model.renumber(s);
        server.recalculate_sizes();
        server.model.redraw(Redraw::SessionStatus(s));
        return Ok(());
    }
    let (Some(src_s), Some(mut src_idx), Some(w)) = (source.session, source.index, source.window)
    else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    let spec = args.target('t');
    let dst = target::resolve(
        &server.model,
        item.origin,
        spec.as_ref(),
        TargetKind::Window,
        FindFlags::WINDOW_INDEX,
    )?;
    let dst_s = dst.session.ok_or(EdgeError::NoCurrentTarget)?;
    let mut idx = dst.new_index;
    let before = args.flag('b');
    if args.flag('a') || before {
        let from = dst
            .index
            .or_else(|| server.model.sessions.get(&dst_s).map(|s| s.current))
            .unwrap_or(0);
        // The source winlink is kept across winlink_shuffle_up, so a source
        // in the shuffled run [start, first free) is unlinked from its new
        // index.
        let start = if before { from } else { from.saturating_add(1) };
        let shifted = src_s == dst_s
            && src_idx >= start
            && (start..=src_idx).all(|i| server.model.sessions[&dst_s].windows.contains_key(&i));
        // A failed shuffle is an error without a message.
        idx = Some(
            server
                .model
                .shuffle_up(dst_s, from, before)
                .ok_or(CmdError::Quiet)?,
        );
        if shifted {
            src_idx += 1;
        }
    }

    // server_link_window.
    let mut select = !args.flag('d');
    if let Some(i) = idx
        && let Some(&x) = server.model.sessions[&dst_s].windows.get(&i)
    {
        if x == w {
            return Err(EdgeError::Message(format!("same index: {i}")).into());
        }
        if args.flag('k') {
            // Unlinked without session_detach (which keeps the only
            // window); replacing the current window always selects.
            let session = server.model.sessions.get_mut(&dst_s).expect("target");
            if session.current == i {
                select = true;
                // `dst->curw = NULL`: no index is ever u32::MAX.
                session.current = u32::MAX;
            }
            for pane in server.model.unlink_index(dst_s, i) {
                server.close_pane_io(pane);
            }
        }
    }
    let index = server
        .model
        .link_window(dst_s, w, idx)
        .map_err(EdgeError::Message)?;
    if select {
        server.model.set_current(dst_s, index);
    }
    server.model.redraw(Redraw::Session(dst_s));
    // window-linked fires before the unlink's window-unlinked.

    if moving {
        // server_unlink_window: the source's only window cannot go, so the
        // session does.
        if server.model.detach_window(src_s, src_idx) {
            for pane in server.model.remove_session(src_s) {
                server.close_pane_io(pane);
            }
        } else {
            server.model.redraw(Redraw::Session(src_s));
        }
    }
    // Only the source is renumbered, and only without -s.
    if !args.flag('s') && server.model.sessions.contains_key(&src_s) {
        server.model.renumber_if_set(src_s);
    }
    server.recalculate_sizes();
    Ok(())
}
