//! Character width as the reference tmux 3.8 computes it: glibc `wcwidth(3)` (it is not
//! built with utf8proc), with C1 controls at width 0 (utf8.c:575-582).
//!
//! TODO: the styles work (`width.rs`) owns the shared width table; replace this stub
//! with it once it lands on m1. The tables here come from `gen_wcwidth.py`.

use super::wcwidth_tables::{WIDE, ZERO};

fn in_table(table: &[(u32, u32)], c: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// The display width of one non-ASCII character: 0, 1 or 2.
pub(super) fn char_width(c: char) -> usize {
    let c = u32::from(c);
    // Below U+00A0 only the C1 controls reach here: width 0.
    if c < 0xa0 || in_table(ZERO, c) {
        0
    } else if in_table(WIDE, c) {
        2
    } else {
        1
    }
}
