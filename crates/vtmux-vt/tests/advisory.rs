//! GHSA-w9q4-5jvv-mcv4 (ghostty e257bb45): a scroll-clear (`CSI 22 J`)
//! whose pruning frees the page under the cursor left the cursor's style
//! and hyperlink ids pointing into the freed page. The next print used
//! them on the page the cursor moved to: memory corruption, a safety panic
//! in a `ReleaseSafe` Ghostty (`ref_counted_set.zig` `useMultiple`).
//!
//! The cursor's page must be pruned by the clear itself, which over the C
//! API takes a clear that scrolls more than a page while the byte budget
//! keeps at most two pages: a wide pane and `set_history_budget` near
//! Ghostty's minimum. tmux ignores ED 22, so there is no reference output:
//! the printed cell must carry the cursor's attributes, as Ghostty's own
//! e257bb45 tests check.
//!
//! Checked against ghostty 3bd54e025 with e257bb45 reverted
//! (`GHOSTTY_SOURCE_DIR=<tree> LIBGHOSTTY_VT_SYS_OPTIMIZE=...`): the
//! default `ReleaseFast` build fails the style test (`lines=52`: bold lost)
//! and passes the hyperlink one; a `ReleaseSafe` build aborts in both
//! ("reached unreachable code", 110 of the 201 line counts in a C sweep).

use vtmux_vt::{PaneTerm, snapshot};

fn clear_after(lines: usize, attrs: &[u8]) -> PaneTerm {
    let mut t = PaneTerm::new(1000, 40, 2000);
    t.set_history_budget(1);
    t.feed(&b"x\r\n".repeat(lines));
    t.feed(b"x\x1b[H");
    t.feed(attrs);
    t.feed(b"\x1b[22Jy");
    t
}

#[test]
fn styled_cursor_survives_a_clear_that_prunes_its_page() {
    for lines in 0..=200 {
        let mut t = clear_after(lines, b"\x1b[1m");
        let s = snapshot(&t);
        let cell = s.cell(0, 0).unwrap();
        assert_eq!(
            (cell.text.as_str(), cell.style.bold),
            ("y", true),
            "lines={lines}"
        );
        // Keep using styles on the cursor's new page.
        t.feed(&b"\x1b[0mz\x1b[4mq\x1b[1;31mr\r\n".repeat(50));
        let s = snapshot(&t);
        assert!(
            s.cells.iter().any(|c| c.text == "r" && c.style.bold),
            "lines={lines}"
        );
    }
}

#[test]
fn linked_cursor_survives_a_clear_that_prunes_its_page() {
    for lines in 0..=200 {
        let mut t = clear_after(lines, b"\x1b]8;;http://example.com\x1b\\");
        let s = snapshot(&t);
        let cell = s.cell(0, 0).unwrap();
        assert_eq!(
            (cell.text.as_str(), cell.link),
            ("y", true),
            "lines={lines}"
        );
        t.feed(&b"\x1b]8;;\x1b\\z\x1b]8;;http://o\x1b\\q\r\n".repeat(50));
        let s = snapshot(&t);
        assert!(
            s.cells.iter().any(|c| c.text == "q" && c.link),
            "lines={lines}"
        );
    }
}
