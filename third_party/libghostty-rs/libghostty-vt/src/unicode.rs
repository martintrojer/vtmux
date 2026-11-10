//! Unicode terminal-width helpers.

use crate::ffi;

/// Returns the terminal display width of a Unicode codepoint in terminal grid cells.
///
/// This is the same width table the terminal itself uses when laying out
/// printed text, so callers can predict column layout that exactly matches what
/// the terminal will do when the text is actually written to it.
///
/// Returns 0 for zero-width characters, 2 for wide characters, and 1 for
/// everything else. This operates on a single character only and therefore
/// cannot account for grapheme-cluster-level width rules. For cluster-accurate
/// widths, use [`grapheme_width`]. Summing per-character widths is only correct
/// when mode 2027, grapheme clustering, is disabled.
///
/// This function allocates nothing and is thread-safe. Concurrent option
/// updates may make a query observe either configuration.
#[must_use]
pub fn codepoint_width(codepoint: char) -> u8 {
    unsafe { ffi::ghostty_unicode_codepoint_width(u32::from(codepoint)) }
}

/// vtmux: replace the process-wide width overrides that printing (and
/// [`codepoint_width`]) use ahead of the table: `(first, last, width)`,
/// inclusive, sorted by `first` and not overlapping, widths 0 to 2. An empty
/// slice clears them. Cells already printed keep their width. Safe while
/// other threads print or query widths. The library keeps each distinct set
/// of ranges for the life of the process (no lock on the print path), so
/// set them at configuration time, not per character.
///
/// # Errors
/// Ranges out of order, overlapping or with a width above 2.
pub fn set_width_overrides(ranges: &[(u32, u32, u8)]) -> crate::error::Result<()> {
    let ranges: Vec<ffi::UnicodeWidthRange> = ranges
        .iter()
        .map(|&(first, last, width)| ffi::UnicodeWidthRange { first, last, width })
        .collect();
    // SAFETY: the pointer and length describe `ranges`, which outlives the
    // call; the library copies them.
    crate::error::from_result(unsafe {
        ffi::ghostty_unicode_set_width_overrides(ranges.as_ptr(), ranges.len())
    })
}

/// vtmux: make grapheme clustering (mode 2027) join characters into a cell
/// as tmux's `screen_write_combine` does instead of by UAX #29: zero-width
/// characters, U+200D and U+FE0F join the cell before (U+FE0F making a
/// one-column cell two columns when `vs16_wide`, tmux's
/// `variation-selector-always-wide`), anything joins a cell that ends with
/// U+200D, two lone regional indicators and an emoji skin-tone modifier
/// with one of tmux's bases join into two columns, Hangul jamo join as
/// tmux's do and U+3164 is dropped. Process-wide; cells already printed
/// keep their text. Safe while other threads print.
pub fn set_tmux_clusters(enabled: bool, vs16_wide: bool) {
    // SAFETY: two flags read by printing; no pointers.
    unsafe { ffi::ghostty_unicode_set_tmux_clusters(enabled, vs16_wide) }
}

/// Measures the terminal display width of the first grapheme cluster in a sequence.
///
/// This uses the exact same grapheme segmentation and cluster width rules the
/// terminal itself uses when printing text with grapheme clustering enabled,
/// mode 2027, so callers can predict column layout that exactly matches what
/// the terminal will do when the text is actually written to it. Unlike
/// [`codepoint_width`], this accounts for cluster-level rules: emoji variation
/// selectors, ZWJ sequences, combining marks, and skin tone modifiers.
///
/// Reads characters until the terminal would consider the grapheme cluster
/// complete and returns the number of characters consumed with the cluster's
/// total width in cells. Returns `(0, 0)` if and only if the input is empty.
///
/// This is not a streaming API. The provided sequence must contain a complete
/// first grapheme cluster, or the logical end of the string. If input arrives in
/// chunks, keep buffering while this function consumes all available characters
/// and the stream may still continue; a later character could still extend the
/// cluster and change its width.
///
/// Mode dependence: this models mode 2027, grapheme clustering, enabled. When
/// mode 2027 is disabled, clusters never combine and variation selectors never
/// change width; predict layout in that case by summing [`codepoint_width`] over
/// each character instead.
#[must_use]
pub fn grapheme_width(chars: &[char]) -> (usize, u8) {
    let codepoints = chars.iter().copied().map(u32::from).collect::<Vec<_>>();
    let mut width = 0;
    let consumed = unsafe {
        ffi::ghostty_unicode_grapheme_width(codepoints.as_ptr(), codepoints.len(), &raw mut width)
    };
    (consumed, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn width_overrides_can_change_while_readers_and_terminals_run() {
        // Isolate the process-wide options and contain the pre-fix native crash.
        const CHILD: &str = "VTMUX_WIDTH_RACE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "unicode::tests::width_overrides_can_change_while_readers_and_terminals_run",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "width race subprocess failed: {status}");
            return;
        }
        let start = Arc::new(Barrier::new(4));
        let done = Arc::new(AtomicBool::new(false));
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let start = start.clone();
                let done = done.clone();
                scope.spawn(move || {
                    let mut terminal = crate::Terminal::new(crate::TerminalOptions {
                        cols: 10,
                        rows: 2,
                        max_scrollback: 0,
                    })
                    .unwrap();
                    terminal.vt_write(b"\x1b[?2027h");
                    start.wait();
                    while !done.load(Ordering::Relaxed) {
                        assert!((1..=2).contains(&codepoint_width('界')));
                        let (consumed, width) = grapheme_width(&['界']);
                        assert_eq!(consumed, 1);
                        assert!((1..=2).contains(&width));
                        terminal.vt_write("\x1b[Hé\u{200d}é✌\u{fe0f}界".as_bytes());
                    }
                });
            }
            let writers: Vec<_> = [1, 2]
                .into_iter()
                .map(|width| {
                    let start = start.clone();
                    scope.spawn(move || {
                        start.wait();
                        for _ in 0..20_000 {
                            set_width_overrides(&[(0xe9, 0xe9, width), (0x100, 0x10ffff, width)])
                                .unwrap();
                            set_tmux_clusters(true, width == 2);
                            set_width_overrides(&[]).unwrap();
                            set_tmux_clusters(false, true);
                        }
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            done.store(true, Ordering::Relaxed);
        });
    }
}
