#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]
//! Server: event loop, model, commands, rendering.
//!
//! `unsafe` is confined to [`sys`] (D10): the crate denies `unsafe_code`
//! and `sys` alone allows it.

pub mod acl;
pub mod alerts;
pub mod client_flags;
pub mod cmdq;
pub mod commands;
pub mod compress;
pub mod control;
pub mod control_queue;
pub mod control_subs;
pub mod editor;
pub mod environ;
pub mod events;
pub mod format_ctx;
pub mod format_jobs;
pub mod groups;
pub mod input;
pub mod jobs;
pub mod key_dispatch;
pub mod keys;
pub mod live;
pub mod menu;
pub mod mode;
pub mod model;
pub mod mouse;
pub mod names;
pub mod notify;
pub mod open_prompt;
pub mod outer_cursor;
pub mod pane_io;
pub mod pane_kitty;
pub mod paste;
pub mod persist;
pub mod pipe;
pub mod popup;
pub mod prompt;
pub mod prompt_history;
pub mod remain;
pub mod render;
pub mod resize;
pub mod scrollbar;
pub mod server;
pub mod shim;
pub mod snapshot_term;
#[allow(unsafe_code)]
pub mod sys;
pub mod target;
pub mod term_queries;
pub mod view;
pub mod wait;

pub use server::{ClientId, Server, ServerConfig, run_server};
