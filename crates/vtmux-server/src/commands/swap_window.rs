//! `swap-window` (cmd-swap-window.c:49-100): the two winlinks keep their
//! indices, flags and place in their sessions and trade windows. Session
//! groups do not exist yet.

use vtmux_core::cmd::{Args, CmdError};
use vtmux_core::error::EdgeError;

use crate::model::Redraw;
use crate::server::Server;
use crate::target::Found;

pub fn exec(server: &mut Server, args: &Args, source: Found, found: Found) -> Result<(), CmdError> {
    let (Some(src_s), Some(src_idx), Some(w_src), Some(dst_s), Some(dst_idx), Some(w_dst)) = (
        source.session,
        source.index,
        source.window,
        found.session,
        found.index,
        found.window,
    ) else {
        return Err(EdgeError::NoCurrentTarget.into());
    };
    if w_src == w_dst {
        return Ok(());
    }
    let m = &mut server.model;
    // Each winlink is appended to the list of the window it now has.
    let (stamp_dst, stamp_src) = (m.next_link_stamp(), m.next_link_stamp());
    let session = m.sessions.get_mut(&dst_s).expect("target session");
    session.windows.insert(dst_idx, w_src);
    session.link_seq.insert(dst_idx, stamp_dst);
    let session = m.sessions.get_mut(&src_s).expect("source session");
    session.windows.insert(src_idx, w_dst);
    session.link_seq.insert(src_idx, stamp_src);
    // -d selects (not the opposite): cmd-swap-window.c:83-87.
    if args.flag('d') {
        m.set_current(dst_s, dst_idx);
        if src_s != dst_s {
            m.set_current(src_s, src_idx);
        }
    }
    m.redraw(Redraw::Session(src_s));
    if src_s != dst_s {
        m.redraw(Redraw::Session(dst_s));
    }
    server.recalculate_sizes();
    Ok(())
}
