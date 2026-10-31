#![deny(unsafe_op_in_unsafe_fn)]
//! The only FFI crate: safe libghostty-vt wrapper.
//!
//! One [`PaneTerm`] per pane, with tmux-shaped capture, replies, cursor
//! reporting and a renderable [`Snapshot`]. Grid state follows Ghostty;
//! everything serialised follows tmux 3.8 (spec D4).

pub mod capture;
pub mod snapshot;
pub mod term;

pub use capture::{CaptureOpts, LineRef, Row, capture};
pub use snapshot::{Cell, Color, Snapshot, Style, Underline, snapshot};
pub use term::{LineMark, PaneTerm};
