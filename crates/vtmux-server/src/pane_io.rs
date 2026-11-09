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
    /// The master is in the poll set: it leaves it while neither read
    /// nor written.
    registered: Cell<bool>,
}

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
            registered: Cell::new(false),
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
        registry.register(
            &mut SourceFd(&self.master.as_raw_fd()),
            token,
            Interest::READABLE,
        )?;
        self.registered.set(true);
        Ok(())
    }

    pub fn deregister(&self, registry: &Registry) {
        if self.registered.replace(false) {
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
        let fd = &mut SourceFd(&self.master.as_raw_fd());
        if self.registered.replace(true) {
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
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => return ReadResult::Closed,
                Ok(n) => {
                    buf.truncate(n);
                    return ReadResult::Data(buf);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return ReadResult::WouldBlock,
                Err(_) => return ReadResult::Closed,
            }
        }
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

    #[must_use]
    pub fn has_pending_output(&self) -> bool {
        !self.out.is_empty()
    }

    /// Tell the pane program its new size (`TIOCSWINSZ`; the kernel sends
    /// the foreground process group SIGWINCH).
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        crate::sys::pty::resize(&self.master, cols, rows)
    }

    /// The size the pty has now (`TIOCGWINSZ`): columns and rows.
    pub fn size(&self) -> io::Result<(u16, u16)> {
        let ws = rustix::termios::tcgetwinsize(&self.master)?;
        Ok((ws.ws_col, ws.ws_row))
    }

    /// Bytes waiting to be read (`FIONREAD`, window.c
    /// `window_pane_destroy_ready`).
    #[must_use]
    pub fn readable_bytes(&self) -> u64 {
        rustix::io::ioctl_fionread(&self.master).unwrap_or(0)
    }
}
