//! A pane's PTY master: non-blocking reads into the terminal, queued
//! writes (keys, terminal replies) flushed when the PTY is writable.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

#[derive(Debug)]
pub struct PaneIo {
    master: std::fs::File,
    out: Vec<u8>,
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
        )
    }

    pub fn deregister(&self, registry: &Registry) {
        let _ = registry.deregister(&mut SourceFd(&self.master.as_raw_fd()));
    }

    /// Re-arm with WRITABLE while output is queued.
    pub fn reregister(&self, registry: &Registry, token: Token) -> io::Result<()> {
        let interest = if self.out.is_empty() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        registry.reregister(&mut SourceFd(&self.master.as_raw_fd()), token, interest)
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

    /// Bytes waiting to be read (`FIONREAD`, window.c
    /// `window_pane_destroy_ready`).
    #[must_use]
    pub fn readable_bytes(&self) -> u64 {
        rustix::io::ioctl_fionread(&self.master).unwrap_or(0)
    }
}
