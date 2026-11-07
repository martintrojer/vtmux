#![deny(unsafe_op_in_unsafe_fn)]
//! The only FFI crate: safe libghostty-vt wrapper.
//!
//! One [`PaneTerm`] per pane, with tmux-shaped capture, replies, cursor
//! reporting and a renderable [`Snapshot`]. Grid state follows Ghostty;
//! everything serialised follows tmux 3.8 (spec D4).

pub mod capture;
mod dcs;
pub mod palette;
pub mod snapshot;
pub mod term;
mod text;

pub use capture::{
    CaptureOpts, LineRef, Lines, Row, ScreenCopy, capture, capture_alternate, capture_grid,
    capture_lines,
};
pub use palette::{OscEnd, Reply, Rgb, colour_reply};
pub use snapshot::{Cell, Color, ScreenView, Snapshot, Style, Underline, snapshot};
pub use text::CellText;

/// tmux's `codepoint-widths` for every pane: from now on printed text takes
/// these widths over Ghostty's table (`utf8_update_width_cache`; cells
/// already printed keep theirs, as tmux's grid does). `(first, last,
/// width)`, inclusive, sorted and not overlapping, widths 0 to 2; empty
/// clears them. Process-wide, like tmux's width cache.
///
/// # Panics
/// When the ranges are not sorted, overlap or have a width above 2.
pub fn set_width_overrides(ranges: &[(u32, u32, u8)]) {
    libghostty_vt::unicode::set_width_overrides(ranges).expect("valid width ranges");
}
pub use palette::PaletteState;
pub use term::{
    CellState, CompressedPages, Compression, CompressionActivity, ExtendedKeys, FrozenCell,
    FrozenLinkId, FrozenRowState, GhosttyCharset, GhosttyCursor, GhosttyState, HeldScreen,
    ImportCheck, KeyMode, LineMark, LinkId, LinkState, ModeState, MouseModes, MouseTracking,
    Osc133, PATCH_STACK, PageBlob, PageCapacity, PageRows, PageState, PaneState, PaneTerm,
    Passthrough, RowState, SavedCursor, ScreenKind, ScreenState, StateError, StateImport,
    StyleState, TermState, compress_all_of, history_reads, page_rows,
};
