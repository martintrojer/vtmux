//! Window layouts (D15): a tiled tree plus floating cells, tmux 3.8's layout
//! strings (v2 JSON and the v1 checksum format), the named layouts, and the
//! split/resize/close arithmetic. Pure: no server, no I/O.
//!
//! Behaviour comes from tmux 3.8 `layout.c`, `layout-custom.c`, `layout-set.c`
//! and `json.c`, read as a spec; the structure is Rust's. The tree is
//! [`Cell`] (`Pane`, `Floating`, `Node`); operations are methods on
//! [`Layout`], which owns the tree and the window facts the arithmetic needs
//! (window size, pane-border-status, scrollbar reserve). Window state that is
//! not geometry (pane order, active pane, last-used stack, z-order,
//! `pane-base-index`) lives in [`WindowPanes`]; emission and parsing take it
//! as input because `"a"`, `"l"`, `"i"` and `"z"` come from there.
//!
//! Not modelled, on purpose (server work): zoom (`saved_layout_cell`
//! variants), modal panes and scrollbar geometry inside a pane.
//! Error values are tmux's `cause` strings; callers add `: layout-name`
//! (`cmd-select-layout.c:150`).

// The arithmetic mirrors layout.c's `u_int`/`int` mixing and its long argument
// lists on purpose (results must match tmux cell for cell, wrap-around
// included), so these pedantic lints are off for the module.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

mod arith;
mod cell;
mod emit;
mod json;
mod named;
mod parse;
mod window;

#[cfg(test)]
mod golden;

pub use arith::{
    FloatArgs, PercentError, ResizeRequest, SplitError, SplitRequest, percentage, split_sizes,
};
pub use cell::{Cell, Dir, Geometry, Leaf, Node, PANE_MAXIMUM, PANE_MINIMUM, Path, WINDOW_MAXIMUM};
pub use emit::{dump_v1, dump_v2};
pub use named::{NamedLayout, SetOpts};
pub use parse::{Parsed, ParsedState, WindowPane, parse};
pub use window::{AddFlags, WindowPanes};

use crate::ids::PaneId;

/// `pane-border-status` as the layout arithmetic sees it
/// (`window_get_pane_status`: the `*-floating` values count as off).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaneStatus {
    #[default]
    Off,
    Top,
    Bottom,
}

/// The window options layout arithmetic reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opts {
    pub status: PaneStatus,
    /// `pane-scrollbars always`: width + pad the scrollbar reserves.
    pub scrollbar_always: Option<u32>,
    /// Floating panes have a border (`pane-border-lines` is not `none`).
    pub float_border: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            status: PaneStatus::Off,
            scrollbar_always: None,
            float_border: true,
        }
    }
}

/// A window's layout: the tree and the facts the arithmetic needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Window size (`w->sx`, `w->sy`).
    pub sx: u32,
    pub sy: u32,
    pub root: Cell<PaneId>,
    pub opts: Opts,
    /// Where the last default-placed floating pane went
    /// (`w->last_new_pane_x/y`).
    pub last_new: (i32, i32),
}

impl Layout {
    /// `layout_init`: one pane filling a `sx` by `sy` window.
    #[must_use]
    pub fn new(sx: u32, sy: u32, pane: PaneId) -> Self {
        Self {
            sx,
            sy,
            root: Cell::Pane(Leaf {
                pane,
                g: Geometry::new(sx, sy, 0, 0),
            }),
            opts: Opts::default(),
            last_new: (0, 0),
        }
    }

    /// Emit as `#{window_layout}` prints (v2), for [`WindowPanes`] state.
    #[must_use]
    pub fn dump_v2(&self, win: &WindowPanes) -> Option<String> {
        dump_v2(&self.root, win)
    }

    /// Emit as a control client without `new-layouts` sees it (v1).
    #[must_use]
    pub fn dump_v1(&self) -> String {
        dump_v1(&self.root)
    }
}
