//! A pane's PTY master: non-blocking reads into the terminal, queued
//! writes (keys, terminal replies) flushed when the PTY is writable.

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

#[derive(Debug)]
pub struct PaneIo {
    master: std::fs::File,
    out: Vec<u8>,
    /// The pane is read (`EV_READ` on `wp->event`): off while every
    /// attached client is a control client that is behind
    /// (server-client.c `server_client_check_pane_buffer`).
    reading: Cell<bool>,
    /// The interest the master is in the poll set with: it leaves it
    /// while neither read nor written.
    armed: Cell<Option<Interest>>,
    /// What [`Self::read`] reads into, kept between reads.
    buf: Vec<u8>,
}

/// The most [`PaneIo::read`] takes at once.
const READ_SIZE: usize = 64 * 1024;

/// What a read produced.
pub enum ReadResult {
    Data(Vec<u8>),
    /// Nothing more for now.
    WouldBlock,
    /// EOF or EIO: the slave side is gone.
    Closed,
}

impl PaneIo {
    #[must_use]
    pub fn new(master: OwnedFd) -> Self {
        Self {
            master: std::fs::File::from(master),
            out: Vec::new(),
            reading: Cell::new(true),
            armed: Cell::new(None),
            buf: Vec::new(),
        }
    }

    #[must_use]
    pub fn fd(&self) -> &std::fs::File {
        &self.master
    }

    #[must_use]
    pub fn owned(&self) -> std::os::fd::BorrowedFd<'_> {
        self.master.as_fd()
    }

    pub fn register(&self, registry: &Registry, token: Token) -> io::Result<()> {
        let interest = if self.out.is_empty() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        registry.register(&mut SourceFd(&self.master.as_raw_fd()), token, interest)?;
        self.armed.set(Some(interest));
        Ok(())
    }

    pub fn deregister(&self, registry: &Registry) {
        if self.armed.replace(None).is_some() {
            let _ = registry.deregister(&mut SourceFd(&self.master.as_raw_fd()));
        }
    }

    /// Re-arm: READABLE while the pane is read, WRITABLE while output is
    /// queued; out of the poll set while neither.
    pub fn reregister(&self, registry: &Registry, token: Token) -> io::Result<()> {
        let interest = match (self.reading.get(), !self.out.is_empty()) {
            (true, false) => Interest::READABLE,
            (true, true) => Interest::READABLE | Interest::WRITABLE,
            (false, true) => Interest::WRITABLE,
            (false, false) => {
                self.deregister(registry);
                return Ok(());
            }
        };
        // The same interest with nothing left to write is left alone: the
        // master is read until WouldBlock or put in the backlog, so
        // re-arming it after every read and key (epoll_ctl) did nothing.
        // Output left to write re-arms, as WRITABLE must fire again.
        let armed = self.armed.replace(Some(interest));
        if armed == Some(interest) && self.out.is_empty() {
            return Ok(());
        }
        let fd = &mut SourceFd(&self.master.as_raw_fd());
        if armed.is_some() {
            registry.reregister(fd, token, interest)
        } else {
            registry.register(fd, token, interest)
        }
    }

    /// Whether the pane is read.
    #[must_use]
    pub fn reading(&self) -> bool {
        self.reading.get()
    }

    /// Turn reading on or off (`bufferevent_enable`/`_disable` of
    /// `EV_READ`); [`Self::reregister`] makes it so. Whether it changed.
    pub fn set_reading(&self, on: bool) -> bool {
        self.reading.replace(on) != on
    }

    pub fn read(&mut self) -> ReadResult {
        // Zeroed once, not per read: a key's echo read 64 KB of fresh
        // memory twice (the data, then WouldBlock).
        self.buf.resize(READ_SIZE, 0);
        loop {
            match self.master.read(&mut self.buf) {
                Ok(0) => return ReadResult::Closed,
                Ok(n) => return ReadResult::Data(self.buf[..n].to_vec()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return ReadResult::WouldBlock,
                Err(_) => return ReadResult::Closed,
            }
        }
    }

    /// Bytes queued that the PTY has not taken yet (keys, a paste the
    /// program has not read): a live upgrade carries them.
    #[must_use]
    pub fn queued(&self) -> &[u8] {
        &self.out
    }

    /// Queue bytes to write once the PTY takes them, without trying now
    /// (a live upgrade's carried queue, written from the loop).
    pub fn queue(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
    }

    /// Queue bytes and try to write them now.
    pub fn write(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
        self.flush();
    }

    /// Write as much queued output as the PTY takes.
    pub fn flush(&mut self) {
        while !self.out.is_empty() {
            match self.master.write(&self.out) {
                Ok(0) => break,
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    /// Tell the pane program its new size (`TIOCSWINSZ`; the kernel sends
    /// the foreground process group SIGWINCH), and the size in pixels
    /// (`ws_xpixel`, `ws_ypixel`).
    pub fn resize(&self, cols: u16, rows: u16, pixels: (u16, u16)) -> io::Result<()> {
        crate::sys::pty::resize(&self.master, cols, rows, pixels)
    }

    /// The size the pty has now (`TIOCGWINSZ`): columns and rows, then
    /// pixels wide and high. The kernel's SIGWINCH compares all four
    /// (`tty_do_resize`).
    pub fn size(&self) -> io::Result<((u16, u16), (u16, u16))> {
        let ws = rustix::termios::tcgetwinsize(&self.master)?;
        Ok(((ws.ws_col, ws.ws_row), (ws.ws_xpixel, ws.ws_ypixel)))
    }

    /// Bytes waiting to be read (`FIONREAD`, window.c
    /// `window_pane_destroy_ready`).
    #[must_use]
    pub fn readable_bytes(&self) -> u64 {
        rustix::io::ioctl_fionread(&self.master).unwrap_or(0)
    }
}
