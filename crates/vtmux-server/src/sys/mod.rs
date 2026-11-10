//! System calls the rest of the server cannot make safely: fork, daemon,
//! forkpty, exec, signals, the passwd database and descriptor waits (D10).
//! Every `unsafe` block in the server lives under this module and carries a
//! `SAFETY` comment. Everything exported is a safe function. The crate root
//! denies `unsafe_code`; this module alone allows it.

pub mod daemon;
pub mod fdwait;
pub mod job;
pub mod pty;
pub mod signal;
pub mod upgrade;
pub mod user;
