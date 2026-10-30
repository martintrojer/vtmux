#![deny(unsafe_op_in_unsafe_fn)]
//! Server: event loop, model, commands, rendering.
//!
//! `unsafe` is confined to [`sys`] (D10).

pub mod commands;
pub mod control;
pub mod environ;
pub mod format_ctx;
pub mod input;
pub mod key_dispatch;
pub mod keys;
pub mod model;
pub mod names;
pub mod pane_io;
pub mod render;
pub mod server;
pub mod sys;
pub mod target;
pub mod wait;

pub use server::{ClientId, Server, ServerConfig, run_server};
