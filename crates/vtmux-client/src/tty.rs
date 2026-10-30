//! Raw tty lifetime. Terminal escapes belong to the server (D14).
use rustix::termios::{self, OptionalActions, Termios};
use std::io;

pub struct RawTty(Termios, rustix::fs::OFlags);

impl RawTty {
    pub fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = termios::tcgetattr(&stdin)?;
        let mut raw = saved.clone();
        raw.make_raw();
        let flags = rustix::fs::fcntl_getfl(&stdin)?;
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        let guard = Self(saved, flags);
        rustix::fs::fcntl_setfl(&stdin, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(guard)
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
        let _ = rustix::fs::fcntl_setfl(io::stdin(), self.1);
    }
}
