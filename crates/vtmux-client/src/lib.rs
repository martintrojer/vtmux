#![forbid(unsafe_code)]
//! Client: attach, raw tty, dumb pipe.

pub mod attach;
pub mod tty;

use std::io::Write;

/// A line on stdout as tmux's `printf` writes it: a failed write (EIO from
/// a terminal that hung up) is ignored, where `println!` would panic.
pub fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// [`say`] on stderr.
pub fn say_err(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}
