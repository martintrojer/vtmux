//! Reading a client's connection without keeping descriptors it passes.

use std::io::{self, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::AsFd;

use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};

/// How many descriptors one read makes room for. tmux's client passes two
/// (its stdin and stdout, `MSG_IDENTIFY_STDIN`/`_STDOUT` in client.c
/// `client_send_identify`), one per `sendmsg` (imsg's `msgbuf_write`).
const ROOM: usize = 16;

/// `read(2)` on a client's socket, any descriptors that came with the
/// bytes closed. vtmux's protocol passes none, but tmux's client does, and
/// macOS installs passed descriptors in the receiver's table even for a
/// plain `read`, where Linux discards them: a vtmux server met by a tmux
/// client there kept that client's terminal or pipes open for ever.
pub fn read_closing_fds(socket: impl AsFd, buf: &mut [u8]) -> io::Result<usize> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(ROOM))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    #[cfg(not(target_vendor = "apple"))]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(target_vendor = "apple")]
    let flags = RecvFlags::empty();
    let got = rustix::net::recvmsg(socket, &mut [IoSliceMut::new(buf)], &mut control, flags)?;
    for msg in control.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = msg {
            fds.for_each(drop);
        }
    }
    Ok(got.bytes)
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};

    use super::*;

    /// A descriptor sent with the bytes is closed once read: its pipe's
    /// reader sees the end of file when the sender's copy goes too.
    #[test]
    fn a_passed_descriptor_is_closed() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (mut reader, writer) = io::pipe().unwrap();
        rustix::fs::fcntl_setfl(&reader, rustix::fs::OFlags::NONBLOCK).unwrap();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let fds = [writer.as_fd()];
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        rustix::net::sendmsg(
            &theirs,
            &[io::IoSlice::new(b"hi")],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        drop(writer);
        (&theirs).write_all(b"!").unwrap();
        let mut buf = [0; 8];
        let mut got = Vec::new();
        while got.len() < 3 {
            let n = read_closing_fds(&ours, &mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"hi!");
        // No writer left: the read end reads end of file, where a copy
        // kept by the reader would leave it waiting (`WouldBlock`).
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
    }
}
