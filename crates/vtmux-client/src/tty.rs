//! Raw tty lifetime. Terminal escapes belong to the server (D14).
use rustix::termios::{self, OptionalActions, Termios};
use std::io;

pub struct RawTty(Termios);

impl RawTty {
    pub fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = termios::tcgetattr(&stdin)?;
        let mut raw = saved.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        Ok(Self(saved))
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
    }
}
