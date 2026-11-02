//! Waiting for a descriptor to become readable the way tmux's event loop
//! sees it.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

/// Block until `fd` is readable, at end of file, or in error (the next
/// `read(2)` says which).
///
/// On macOS tmux runs libevent on `select` (osdep-darwin.c
/// `osdep_event_init` turns kqueue and poll off), and there it matters for
/// a FIFO: `poll` and kqueue never report the end of file once its writer
/// has gone, so a reader waiting on them hangs; `select` reports it as
/// readable.
pub fn wait_readable(fd: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if usize::try_from(fd.as_raw_fd()).is_ok_and(|n| n < libc::FD_SETSIZE) {
        loop {
            // SAFETY: fd_set is plain data and FD_ZERO/FD_SET write only
            // inside it; `fd` is open and below FD_SETSIZE.
            let ready = unsafe {
                let mut set: libc::fd_set = std::mem::zeroed();
                libc::FD_ZERO(&raw mut set);
                libc::FD_SET(fd.as_raw_fd(), &raw mut set);
                libc::select(
                    fd.as_raw_fd() + 1,
                    &raw mut set,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if ready >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
    poll_readable(fd)
}

fn poll_readable(fd: BorrowedFd<'_>) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, poll};
    loop {
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}
