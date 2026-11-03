//! Raw tty lifetime. Terminal escapes belong to the server (D14).
use rustix::termios::{self, OptionalActions, Termios};
use std::io;

/// The terminal's saved modes and file status flags; raw while it lives.
pub struct RawTty(Termios, rustix::fs::OFlags);

impl RawTty {
    pub fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = termios::tcgetattr(&stdin)?;
        let flags = rustix::fs::fcntl_getfl(&stdin)?;
        let guard = Self(saved, flags);
        guard.resume()?;
        Ok(guard)
    }

    /// Raw and non-blocking again after [`RawTty::suspend`].
    pub fn resume(&self) -> io::Result<()> {
        let stdin = io::stdin();
        let mut raw = self.0.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        rustix::fs::fcntl_setfl(&stdin, self.1 | rustix::fs::OFlags::NONBLOCK)?;
        Ok(())
    }

    /// The modes and flags the terminal had before [`RawTty::enter`], for
    /// whoever has it while the client is stopped.
    pub fn suspend(&self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
        let _ = rustix::fs::fcntl_setfl(io::stdin(), self.1);
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        self.suspend();
    }
}
