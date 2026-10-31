//! System calls the rest of the server cannot make safely: fork, daemon,
//! forkpty, exec, the passwd database and `localtime_r` (D10). Every
//! `unsafe` block in the server lives under this module and carries a
//! `SAFETY` comment. Everything exported is a safe function.

pub mod daemon;
pub mod job;
pub mod pty;
pub mod signal;
pub mod user;
