//! `pipe-pane` (cmd-pipe-pane.c): a shell command joined to a pane by a
//! socket pair. The pane's output goes to it as it is read
//! (`window_pane_read_callback`, window.c:1643-1649); with `-I` what it
//! writes goes into the pane as if typed.
//!
//! The pipe closes when the pane goes (`server_destroy_pane`,
//! `window_pane_destroy`), when a new `pipe-pane` replaces it, or on its
//! own error: EOF from a `-I` command, or a write the command no longer
//! takes (`cmd_pipe_pane_error_callback`). The command's process is never
//! signalled; it sees EOF or EPIPE.

use std::os::fd::{AsRawFd, OwnedFd};

use mio::unix::SourceFd;
use mio::{Interest, Token};
use vtmux_core::ids::PaneId;

use crate::server::Server;

/// Pipe tokens: `PIPE_BASE + pane id`, between the jobs' and the popups'.
pub(crate) const PIPE_BASE: usize = (1 << 31) + (1 << 29);

/// A pane's open pipe (`wp->pipe_fd`, `wp->pipe_event`, `wp->pipe_pid`).
#[derive(Debug)]
pub struct PanePipe {
    fd: OwnedFd,
    /// `#{pane_pipe_pid}`.
    pub pid: i32,
    /// `-I`: what the command writes goes into the pane (`EV_READ`).
    input: bool,
    /// Pane output not yet taken by the command (`pipe_event->output`).
    out: Vec<u8>,
}

impl PanePipe {
    /// The descriptor a live upgrade keeps open (D27).
    #[must_use]
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.fd.as_raw_fd()
    }

    /// `-I`: the command's output goes into the pane.
    #[must_use]
    pub fn input(&self) -> bool {
        self.input
    }

    /// Bytes of pane output the command has not taken yet.
    #[must_use]
    pub fn pending(&self) -> &[u8] {
        &self.out
    }
}

impl Server {
    /// Take a pipe kept across an upgrade (D27) back onto pane `p`: its
    /// descriptor, process, `-I` and the output it was still owed.
    pub(crate) fn pipe_adopt(
        &mut self,
        p: PaneId,
        fd: OwnedFd,
        pid: i32,
        input: bool,
        out: Vec<u8>,
    ) -> std::io::Result<PanePipe> {
        let interest = if input {
            Interest::READABLE | Interest::WRITABLE
        } else {
            Interest::WRITABLE
        };
        self.poll.registry().register(
            &mut SourceFd(&fd.as_raw_fd()),
            Token(PIPE_BASE + p.0 as usize),
            interest,
        )?;
        Ok(PanePipe {
            fd,
            pid,
            input,
            out,
        })
    }
}

impl PanePipe {
    /// Nothing is waiting to be written (`EVBUFFER_LENGTH(output) == 0`).
    #[must_use]
    pub fn drained(&self) -> bool {
        self.out.is_empty()
    }

    /// Write what the command takes. False on an error that closes the
    /// pipe (EPIPE once the command has gone).
    fn flush(&mut self) -> bool {
        while !self.out.is_empty() {
            match rustix::io::write(&self.fd, &self.out) {
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => return false,
            }
        }
        true
    }
}

impl Server {
    /// Open a pipe running `command` on pane `p` (cmd-pipe-pane.c:110-182).
    /// Every byte of the pane's output from now on goes to it; with `input`
    /// its stdout comes back into the pane, with `output` its stdin is the
    /// pane's output (else `/dev/null`).
    pub(crate) fn pipe_open(
        &mut self,
        p: PaneId,
        command: &str,
        input: bool,
        output: bool,
    ) -> std::io::Result<()> {
        let spawned = crate::sys::job::spawn_pipe(command, input, output)?;
        // bufferevent_socket_new enables EV_WRITE; EV_READ only with -I.
        let interest = if input {
            Interest::READABLE | Interest::WRITABLE
        } else {
            Interest::WRITABLE
        };
        self.poll.registry().register(
            &mut SourceFd(&spawned.fd.as_raw_fd()),
            Token(PIPE_BASE + p.0 as usize),
            interest,
        )?;
        let pipe = PanePipe {
            fd: spawned.fd,
            pid: spawned.pid.as_raw_nonzero().get(),
            input,
            out: Vec::new(),
        };
        match self.model.panes.get_mut(&p) {
            Some(pane) => pane.pipe = Some(pipe),
            None => self.pipe_drop(pipe),
        }
        // What the command already wrote: edge-triggered events only
        // report what comes later.
        self.pipe_event(p);
        Ok(())
    }

    /// Close pane `p`'s pipe (`bufferevent_free`, `close`). True if it had
    /// one.
    pub(crate) fn pipe_close(&mut self, p: PaneId) -> bool {
        let Some(pipe) = self.model.panes.get_mut(&p).and_then(|x| x.pipe.take()) else {
            return false;
        };
        self.pipe_drop(pipe);
        true
    }

    /// Close a pipe taken off its pane: the command sees EOF on its stdin
    /// and EPIPE on its stdout. Its process is not signalled.
    pub(crate) fn pipe_drop(&self, pipe: PanePipe) {
        let _ = self
            .poll
            .registry()
            .deregister(&mut SourceFd(&pipe.fd.as_raw_fd()));
        drop(pipe);
    }

    /// Pane `p` produced `bytes`: they go to its pipe, if any.
    pub(crate) fn pipe_write(&mut self, p: PaneId, bytes: &[u8]) {
        let Some(pipe) = self.model.panes.get_mut(&p).and_then(|x| x.pipe.as_mut()) else {
            return;
        };
        pipe.out.extend_from_slice(bytes);
        self.pipe_service(p);
    }

    /// An event on pane `p`'s pipe ([`Self::pipe_service`]). A pane whose
    /// program has gone and whose output the pipe held is destroyed once
    /// it is all written or the pipe closed (`cmd_pipe_pane_write_callback`,
    /// `cmd_pipe_pane_read_callback`, `cmd_pipe_pane_error_callback`).
    pub(crate) fn pipe_event(&mut self, p: PaneId) {
        self.pipe_service(p);
        if self.pane_destroy_ready(p) {
            self.pane_exited(p);
        }
    }

    /// A `-I` command's output goes into the pane, then queued pane output
    /// is written. EOF or a write error closes the pipe. The read comes
    /// first: a command that wrote and exited before the pane's next output
    /// makes that write fail, and what it wrote must still reach the pane.
    fn pipe_service(&mut self, p: PaneId) {
        let Some(pipe) = self.model.panes.get_mut(&p).and_then(|x| x.pipe.as_mut()) else {
            return;
        };
        let mut closed = false;
        let mut got = Vec::new();
        if pipe.input {
            let mut buf = [0u8; 16 * 1024];
            closed = loop {
                match rustix::io::read(&pipe.fd, &mut buf) {
                    Ok(0) => break true,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(rustix::io::Errno::AGAIN) => break false,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(_) => break true,
                }
            };
        }
        closed |= !pipe.flush();
        // bufferevent_write(wp->event, ...): raw bytes to the PTY, whatever
        // the pane's modes; nothing once it is closed.
        if !got.is_empty()
            && let Some(io) = self.model.panes.get_mut(&p).and_then(|x| x.io.as_mut())
        {
            io.write(&got);
            self.pane_output_queued(p);
        }
        if closed {
            self.pipe_close(p);
        }
    }

    /// `window_pane_destroy_ready` (window.c:494-516): the pipe has written
    /// everything, the PTY has nothing left to read, and the program's
    /// exit status is in.
    pub(crate) fn pane_destroy_ready(&self, p: PaneId) -> bool {
        let Some(pane) = self.model.panes.get(&p) else {
            return false;
        };
        if pane.pipe.as_ref().is_some_and(|x| !x.drained()) {
            return false;
        }
        if pane.io.as_ref().is_some_and(|io| io.readable_bytes() > 0) {
            return false;
        }
        pane.status.is_some()
    }
}
