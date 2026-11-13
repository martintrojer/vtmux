//! A socket held by a real tmux server. Run as `tmux`, vtmux uses tmux's
//! socket directory (D20), so its client can meet a tmux 3.8 server there,
//! which speaks imsg, not vtmux's protocol. Such a server reads the `Hello`
//! as an imsg of protocol version 0 and either closes the connection
//! without a word or waits for the rest of an imsg that never comes; the
//! client would print `server exited unexpectedly` or hang. So when the
//! server says nothing at once, the client asks the socket in tmux's
//! words, on a second connection, and names what it found instead.
//!
//! Neither server is disturbed: tmux answers any imsg of another protocol
//! version with `MSG_VERSION` and drops that connection (proc.c
//! `peer_check_version`, then `proc_event_cb` once it is written), and the
//! probe, read as a vtmux frame, declares a length over
//! [`protocol::MAX_FRAME`], so a vtmux server drops it unanswered.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::net::{RecvFlags, recv};
use vtmux_core::protocol;

/// How long a server may take to answer a `Hello` before the socket is
/// asked whether it is tmux's. A vtmux server that takes longer (a slow
/// command, a loaded host) is asked too, which costs it one connection
/// dropped unread.
const QUIET: Duration = Duration::from_millis(200);

/// How long a probe waits to be answered or dropped, writing and then
/// reading. A tmux server answers on its next loop turn and a vtmux one
/// drops the probe on its next read, both well under a millisecond: 2 s
/// leaves room for a loaded host and stays short next to a command the
/// user waits on. A server that does neither in time (stopped, wedged, a
/// long upgrade's exec) is taken for vtmux: the client goes on to its
/// relay at most 2 s later than without the probe, the server's answer
/// waiting in the socket meanwhile, so nothing is lost. A tmux server that
/// slow gets the old outcome, `server exited unexpectedly` or a client
/// that waits.
const PROBE_LIMIT: Duration = Duration::from_secs(2);

/// tmux's `MSG_VERSION` (tmux-protocol.h).
const MSG_VERSION: u32 = 12;

/// The size of an imsg header (compat/imsg.h `struct imsg_hdr`: `type`
/// u32, `len` u16, `flags` u16, `peerid` u32, `pid` u32, in the host's
/// byte order), all of a `MSG_VERSION`.
const IMSG_HEADER: usize = 16;

/// The one line a client prints when the server at `path` is tmux's.
pub fn message(path: &Path) -> String {
    format!(
        "not a vtmux server: {} belongs to a tmux server; stop it with the real tmux \
         (its kill-server), or run vtmux as vtmux or with -L <name>",
        path.display()
    )
}

/// Whether the server at `path`, whose connection `stream` has just sent
/// its `Hello`, is a tmux server: it answered with tmux's `MSG_VERSION`,
/// or it said nothing for [`QUIET`] or closed the connection without a
/// word and a probe finds tmux there.
pub fn is_tmux(stream: &UnixStream, path: &Path) -> bool {
    let mut head = [0u8; IMSG_HEADER];
    match peek_within(stream, &mut head, QUIET) {
        First::Bytes(n) => tmux_version(&head[..n]).is_some(),
        First::Nothing | First::End => probe(path),
        // The relay reports a socket that fails.
        First::Error => false,
    }
}

/// What the server sent first.
enum First {
    /// This many bytes, peeked: the relay still reads them.
    Bytes(usize),
    /// End of stream (or a reset) before any byte.
    End,
    /// Nothing within the limit.
    Nothing,
    /// The socket can't be polled.
    Error,
}

fn peek_within(stream: &UnixStream, buf: &mut [u8], limit: Duration) -> First {
    let end = Instant::now() + limit;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let Ok(timeout) = Timespec::try_from(left) else {
            return First::Error;
        };
        let mut fds = [PollFd::new(stream, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => return First::Nothing,
            Ok(_) => break,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return First::Error,
        }
    }
    loop {
        match recv(stream, &mut *buf, RecvFlags::PEEK) {
            Ok((_, 0)) => return First::End,
            Ok((_, n)) => return First::Bytes(n),
            Err(rustix::io::Errno::INTR) => {}
            // A peer that closed with our `Hello` unread (ECONNRESET on
            // some systems) is gone as at end of stream.
            Err(_) => return First::End,
        }
    }
}

/// The imsg a probe sends: no tmux protocol's version (`peerid` 0), and
/// a `type` that, read as a vtmux frame's length, is over
/// [`protocol::MAX_FRAME`].
fn probe_bytes() -> [u8; IMSG_HEADER] {
    const _: () = assert!(protocol::MAX_FRAME < u32::MAX as usize);
    let mut b = [0u8; IMSG_HEADER];
    b[0..4].copy_from_slice(&u32::MAX.to_ne_bytes());
    b[4..6].copy_from_slice(&u16::try_from(IMSG_HEADER).expect("16").to_ne_bytes());
    b[12..16].copy_from_slice(&std::process::id().to_ne_bytes());
    b
}

/// tmux's protocol version, if `b` is its `MSG_VERSION`.
fn tmux_version(b: &[u8]) -> Option<u32> {
    let b: &[u8; IMSG_HEADER] = b.get(..IMSG_HEADER)?.try_into().ok()?;
    let word = |at: usize| u32::from_ne_bytes(b[at..at + 4].try_into().expect("4 bytes"));
    let len = u16::from_ne_bytes([b[4], b[5]]);
    (word(0) == MSG_VERSION && usize::from(len) == IMSG_HEADER).then(|| word(8) & 0xff)
}

/// Ask the socket in tmux's words whether a tmux server holds it.
fn probe(path: &Path) -> bool {
    let Ok(mut s) = UnixStream::connect(path) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(PROBE_LIMIT));
    let _ = s.set_write_timeout(Some(PROBE_LIMIT));
    let mut reply = [0u8; IMSG_HEADER];
    s.write_all(&probe_bytes()).is_ok()
        && s.read_exact(&mut reply).is_ok()
        && tmux_version(&reply).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tmux_version_message_is_recognised() {
        let mut v = [0u8; IMSG_HEADER];
        v[0..4].copy_from_slice(&MSG_VERSION.to_ne_bytes());
        v[4..6].copy_from_slice(&16u16.to_ne_bytes());
        v[8..12].copy_from_slice(&8u32.to_ne_bytes());
        assert_eq!(tmux_version(&v), Some(8));
        assert_eq!(tmux_version(&v[..15]), None, "cut short");
        // A vtmux frame: a u32 payload length, a tag.
        let frame = protocol::encode(&protocol::ServerMsg::Stdout(b"hello world!".to_vec()))
            .expect("encodes");
        assert_eq!(tmux_version(&frame), None);
        v[4] = 17;
        assert_eq!(tmux_version(&v), None, "an imsg with data");
    }

    #[test]
    fn a_probe_is_an_oversized_vtmux_frame() {
        let b = probe_bytes();
        assert!(matches!(
            protocol::decode::<protocol::ClientMsg>(&b),
            Err(protocol::ProtocolError::Oversized(_))
        ));
        assert_eq!(protocol::hello_version(&b), None);
    }
}
