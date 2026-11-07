//! What an import accepts (live upgrade spec, decision 23: a malformed
//! state is an error, never a crash). Rows are the cross-version and
//! on-disk form, so nothing in a [`TermState`] is trusted: every count,
//! index, width and position is checked here before Ghostty sees it, and
//! Ghostty checks again what only it can (a blob's checksum and `Page`
//! value, patch 0017 `importBlob`). A blob's page is trusted for where it
//! comes from (see [`super::ImportCheck`]), not checked here.
//!
//! A state some vtmux exported always passes; the rules are Ghostty's
//! own invariants (`page.zig` `verifyIntegrity`, `PageList.Builder`), the
//! bounds Ghostty's parser and pages put on what a live terminal can hold
//! (each `MAX_*` below, with where it comes from), and the bounds tmux puts
//! on what it models. Past a Ghostty bound the import would not just fail:
//! Ghostty's allocators and reference counts assume it and crash or write
//! out of bounds.

use std::collections::HashSet;

use super::{
    FrozenRowState, GhosttyCharset, GhosttyCursor, LinkId, LinkState, PATCH_STACK, PageRows,
    PageState, PaneState, RowState, ScreenState, StateError, StyleState, TermState,
};
use crate::snapshot::{Color, Underline};

/// tmux's largest window (`WINDOW_MAXIMUM`, tmux.h).
pub(super) const MAX_SIZE: u16 = 10_000;

/// The largest count of rows or lines the model keeps: tmux's
/// `history-limit` is at most `INT_MAX` (options-table.c).
const MAX_LINES: usize = i32::MAX as usize;

/// The largest row id or generation: far above anything reachable, and
/// far enough below `u64::MAX` that adding a row count never overflows.
const MAX_ID: u64 = i64::MAX as u64;

/// Ghostty's charsets (`charsets.zig` `Charset`) and slots (`Slots`).
const CHARSETS: u8 = 4;

/// The longest OSC 8 Ghostty's parser takes: it holds what follows `8;`
/// (`id=ID;URI`, or `;URI`) in its fixed buffer (`osc.zig`
/// `Parser.MAX_BUF`, 2048 bytes) with a NUL after it
/// (`osc/parsers/hyperlink.zig`), so no live link is longer. Past it a
/// URI or id is a `string_alloc` span of more than 64 32-byte chunks,
/// which the bitmap allocator's long-span search reads and writes past its
/// bitmaps (`bitmap_allocator.zig` `findFreeChunks`), and a URI and id
/// that are each up to 64 chunks but more together pass a reflow's check
/// one at a time and fail it together (`PageList.zig`
/// `hyperlinkStringsFit`, then `unreachable`).
pub(super) const MAX_OSC8_BYTES: usize = 2047;

/// The most codepoints a cell keeps after its first (`page.zig`
/// `grapheme_max_len`; `Page.appendGrapheme` drops the rest, so more
/// would not import as they are).
pub(super) const MAX_GRAPHEME: usize = 64;

/// The most styles a page holds besides the default: a page's style
/// capacity is 16-bit (`size.zig` `StyleCountInt`), whose set
/// (`ref_counted_set.zig` `Layout.init`) has a 65536-bucket table with
/// 13/16 of that as items, id 0 reserved.
pub(super) const MAX_STYLES: usize = 65_536 * 13 / 16 - 1;

/// The most cells a page has room for (its rows, or its capacity's when
/// larger, times its width). A style or hyperlink is counted once for each
/// cell that uses it in a 16-bit count (`size.zig` `CellCountInt`, the
/// sets' `RefCountInt`), and Ghostty adds a few references of its own
/// (the table's while importing, the cursor's, a resize's); Ghostty's
/// pages never come near (a standard page is 215 x 215 cells and
/// `Capacity.adjust` keeps its size at another width).
pub(super) const MAX_PAGE_CELLS: usize = u16::MAX as usize - 16;

fn invalid(what: &'static str) -> StateError {
    StateError::Invalid(what)
}

fn check(ok: bool, what: &'static str) -> Result<(), StateError> {
    if ok { Ok(()) } else { Err(invalid(what)) }
}

/// What is not a page: sizes, cursors, modes, regions and the pane's
/// model. The pages come one at a time ([`page`]); what ties the model
/// to them is checked once they are in ([`super::StateImport::finish`]).
pub(super) fn head(s: &TermState) -> Result<(), StateError> {
    check(
        (1..=MAX_SIZE).contains(&s.cols) && (1..=MAX_SIZE).contains(&s.rows),
        "terminal size",
    )?;
    check(
        !s.alternate_active || s.alternate.is_some(),
        "alternate screen active but absent",
    )?;
    screen(&s.primary, s)?;
    if let Some(alt) = &s.alternate {
        screen(alt, s)?;
    }
    // Leaving a screen ends its link (Terminal.zig `switchScreen`), and
    // coming back asserts there is none.
    let inactive = if s.alternate_active {
        Some(&s.primary)
    } else {
        s.alternate.as_ref()
    };
    check(
        inactive.is_none_or(|sc| sc.cursor.link.is_none()),
        "hyperlink open on the inactive screen",
    )?;
    let g = &s.ghostty;
    let (top, bottom, left, right) = g.scroll_region;
    // DECSTBM and DECSLRM never leave a region out of order or off the
    // screen (Terminal.zig `setTopAndBottomMargin`, `setLeftAndRightMargin`).
    check(
        top <= bottom && bottom < s.rows && left <= right && right < s.cols,
        "scroll region",
    )?;
    check(
        g.previous_char.is_none_or(|c| char::from_u32(c).is_some()),
        "previous character",
    )?;
    check(
        g.tab_stops.iter().all(|&c| c < s.cols),
        "tab stop off the screen",
    )?;
    pane(&s.pane)
}

fn screen(sc: &ScreenState, s: &TermState) -> Result<(), StateError> {
    cursor(&sc.cursor, s)?;
    charset(sc.charset)?;
    if let Some(saved) = &sc.saved_cursor {
        // A saved cursor may be off a screen shrunk since; restoring it
        // clamps it (Terminal.zig `restoreCursor`).
        charset(saved.charset)?;
    }
    // Ghostty's `ProtectedMode` and its 8-deep Kitty flag stack of 5-bit
    // flags (`kitty/key.zig` `FlagStack`).
    check(sc.protected_mode <= 2, "protected mode")?;
    check(
        sc.kitty_index < 8 && sc.kitty_flags.iter().all(|&f| f < 32),
        "kitty keyboard flags",
    )?;
    for page in &sc.pages {
        self::page(page, s.cols)?;
    }
    Ok(())
}

fn cursor(c: &GhosttyCursor, s: &TermState) -> Result<(), StateError> {
    // Ghostty's cursor is always on the screen; a pending wrap keeps it
    // in the last column.
    check(c.x < s.cols && c.y < s.rows, "cursor off the screen")?;
    c.link.as_ref().map_or(Ok(()), link)
}

/// An OSC 8 link as the parser makes one (`osc/parsers/hyperlink.zig`):
/// an empty URI closes the link and an empty `id=` is no id, so neither is
/// ever stored (and `Page.insertHyperlink` cannot store an empty string),
/// and the sequence fits [`MAX_OSC8_BYTES`].
fn link(l: &LinkState) -> Result<(), StateError> {
    let osc8 = match &l.id {
        LinkId::Explicit(id) => {
            check(!id.is_empty(), "hyperlink id empty")?;
            "id=;".len() + id.len()
        }
        LinkId::Implicit(_) => ";".len(),
    };
    check(!l.uri.is_empty(), "hyperlink URI empty")?;
    check(
        osc8.saturating_add(l.uri.len()) <= MAX_OSC8_BYTES,
        "hyperlink longer than an OSC 8",
    )
}

/// A style as Ghostty compares them (`style.zig` `Style.eql`): a palette
/// colour's form is in the 256 flags, not in the colour.
fn style_key(s: &StyleState) -> ([[u8; 4]; 3], u16) {
    let color = |c: Color| match c {
        Color::Default => [0; 4],
        Color::Palette(n) | Color::Palette256(n) => [1, n, 0, 0],
        Color::Rgb(r, g, b) => [2, r, g, b],
    };
    let t = &s.style;
    let flags = [
        t.bold,
        t.faint,
        t.italic,
        t.blink,
        t.inverse,
        t.invisible,
        t.strikethrough,
        t.overline,
        s.fg_256,
        s.bg_256,
    ]
    .iter()
    .enumerate()
    .fold(0u16, |acc, (i, &b)| acc | (u16::from(b) << i));
    let underline = match t.underline {
        Underline::None => 0,
        Underline::Single => 1,
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
    };
    (
        [color(t.fg), color(t.bg), color(t.underline_color)],
        flags | (underline << 10),
    )
}

/// The page's style and hyperlink tables as an export writes them: each
/// entry once (an entry added twice is counted twice, toward the 16-bit
/// count [`MAX_PAGE_CELLS`] keeps clear of), within Ghostty's capacity.
fn tables(p: &PageRows) -> Result<(), StateError> {
    check(p.styles.len() <= MAX_STYLES + 1, "page style table size")?;
    if p.styles.len() > 2 {
        let default = style_key(&StyleState::default());
        let mut seen = HashSet::with_capacity(p.styles.len());
        check(
            p.styles[1..]
                .iter()
                .map(style_key)
                .all(|k| k == default || seen.insert(k)),
            "page style twice",
        )?;
    }
    check(
        p.links.first().is_some_and(Option::is_none) && p.links[1..].iter().all(Option::is_some),
        "page hyperlink table",
    )?;
    p.links[1..].iter().flatten().try_for_each(link)?;
    if p.links.len() > 2 {
        let mut seen = HashSet::with_capacity(p.links.len());
        check(
            p.links[1..].iter().flatten().all(|l| seen.insert(l)),
            "page hyperlink twice",
        )?;
    }
    Ok(())
}

fn charset(c: GhosttyCharset) -> Result<(), StateError> {
    check(
        c.g.iter().all(|&g| g < CHARSETS)
            && c.gl < CHARSETS
            && c.gr < CHARSETS
            && c.single_shift.is_none_or(|s| s < CHARSETS),
        "charset",
    )
}

fn pane(p: &PaneState) -> Result<(), StateError> {
    check(
        p.history_limit <= MAX_LINES && p.hsize <= MAX_LINES && p.alt_collected <= MAX_LINES,
        "history size",
    )?;
    // A history row's id counts the rows scrolled before it, and its time
    // is kept from `times_base` on (`PaneTerm::account_scroll`).
    check(
        p.scrolled <= MAX_ID
            && p.generation <= MAX_ID
            && p.times_base <= p.scrolled
            && p.times.len() as u64 <= p.scrolled,
        "history ids",
    )?;
    // tmux's `input-buffer-size` is a 32-bit option.
    check(
        u32::try_from(p.input_buffer_size).is_ok(),
        "input buffer size",
    )?;
    for rows in [&p.frozen, &p.saved].into_iter().flatten() {
        rows.iter().try_for_each(frozen_row)?;
    }
    if let Some((held, _)) = &p.sync {
        let cells = usize::from(held.cols) * usize::from(held.rows);
        check(
            held.cols <= MAX_SIZE
                && held.rows <= MAX_SIZE
                && held.cells.len() == cells
                && (held.wrapped.is_empty() || held.wrapped.len() == usize::from(held.rows))
                && held
                    .cursor
                    .is_none_or(|(x, y)| x < held.cols && y < held.rows),
            "held screen",
        )?;
    }
    Ok(())
}

fn frozen_row(r: &FrozenRowState) -> Result<(), StateError> {
    check(
        r.len <= usize::from(MAX_SIZE)
            && r.cells.len() <= r.len
            && r.cells.iter().all(|c| c.wide <= 3),
        "frozen row",
    )
}

/// One page of either form.
pub(super) fn page(p: &PageState, cols: u16) -> Result<(), StateError> {
    match p {
        PageState::Rows(rows) => self::rows(rows, cols),
        PageState::Blob(blob) if blob.stack != PATCH_STACK => Err(StateError::ForeignBlob),
        // Ghostty checks the rest: the checksum, the `Page` value against
        // its capacity's layout, the width.
        PageState::Blob(blob) => check(!blob.bytes.is_empty(), "empty blob"),
    }
}

/// Cell flags: content kind (bits 0-1), width (2-3), protected (4), the
/// palette-256 form (5), semantic content (6-7). See [`super::CellState`].
const KIND_TEXT: u16 = 0;
const KIND_GRAPHEME: u16 = 1;
const KIND_PALETTE: u16 = 2;
const WIDE: u16 = 1;
const SPACER_TAIL: u16 = 2;
const SPACER_HEAD: u16 = 3;

/// A page of rows as Ghostty's own pages are (`page.zig`
/// `verifyIntegrity`) and as the export writes them: every row's cells
/// in order, one run after another; every index into the page's tables;
/// wide characters with their spacers; graphemes on text cells.
fn rows(p: &PageRows, cols: u16) -> Result<(), StateError> {
    // Every page of a screen has the screen's width: Ghostty's page list
    // assumes it (`PageList.resizeCols`, `beginUpdate`).
    check(p.cols == cols, "page width")?;
    check(
        !p.rows.is_empty() && u16::try_from(p.rows.len()).is_ok(),
        "page row count",
    )?;
    check(!p.styles.is_empty(), "page without its default style")?;
    tables(p)?;
    let cap = p.capacity;
    if cap.rows != 0 {
        // Ghostty's `Capacity` fields (rows, styles and hyperlink bytes
        // are 16-bit); Ghostty refuses a layout over its page limit.
        check(
            u16::try_from(cap.rows).is_ok_and(|r| usize::from(r) >= p.rows.len())
                && u16::try_from(cap.styles).is_ok()
                && u16::try_from(cap.hyperlink_bytes).is_ok(),
            "page capacity",
        )?;
    }
    // The page Ghostty makes has room for this many rows (the capacity's
    // when it is used, else at least the rows given).
    let room = p.rows.len().max(cap.rows as usize);
    check(
        room.saturating_mul(usize::from(cols)) <= MAX_PAGE_CELLS,
        "page cell count",
    )?;
    let mut graphemes = Graphemes::new(&p.graphemes);
    let mut next = 0usize;
    for row in &p.rows {
        let start = row.cell_start as usize;
        let count = usize::from(row.cell_count);
        check(start == next && count <= usize::from(cols), "row cells")?;
        next = start + count;
        let cells = p.cells.get(start..next).ok_or(invalid("row cells"))?;
        check(
            row.flags & !0x0f == 0 && (row.flags >> 2) & 3 != 3,
            "row flags",
        )?;
        // tmux's `cellused` outlives a narrowing without a reflow (the
        // alternate screen: screen.c `screen_resize_cursor` only changes
        // `grid->sx`), so it may pass the width; Ghostty clamps it to the
        // row where it reads it (`Row.tmuxCellsUsed`).
        check(row.used <= MAX_SIZE, "row cells used")?;
        for (x, c) in cells.iter().enumerate() {
            let kind = c.flags & 3;
            let width = (c.flags >> 2) & 3;
            check(c.flags & !0xff == 0 && c.flags >> 6 != 3, "cell flags")?;
            check(
                kind == KIND_PALETTE || c.flags & (1 << 5) == 0,
                "cell palette form",
            )?;
            check(
                match kind {
                    KIND_TEXT => c.content == 0 || char::from_u32(c.content).is_some(),
                    KIND_GRAPHEME => c.content != 0 && char::from_u32(c.content).is_some(),
                    KIND_PALETTE => c.content <= 0xff,
                    _ => c.content <= 0xff_ffff,
                },
                "cell content",
            )?;
            let before = x.checked_sub(1).map(|i| (cells[i].flags >> 2) & 3);
            check(
                match width {
                    // A wide character is followed by its spacer tail
                    // (`Terminal.zig` `print`), which is never a default
                    // cell, so it is in the row's run.
                    WIDE => cells
                        .get(x + 1)
                        .is_some_and(|n| (n.flags >> 2) & 3 == SPACER_TAIL),
                    SPACER_TAIL => before == Some(WIDE),
                    SPACER_HEAD => x + 1 == usize::from(cols) && row.flags & RowState::WRAPPED != 0,
                    _ => true,
                },
                "wide character and its spacer",
            )?;
            check(usize::from(c.style) < p.styles.len(), "cell style")?;
            check(usize::from(c.link) < p.links.len(), "cell hyperlink")?;
            let index = u32::try_from(start + x).map_err(|_| invalid("page cell count"))?;
            check(
                graphemes.at(index)? == (kind == KIND_GRAPHEME),
                "grapheme cell",
            )?;
        }
    }
    check(next == p.cells.len(), "page cells")?;
    check(graphemes.done(), "grapheme table")
}

/// The page's grapheme table (cell index, count, codepoints, ...), read in
/// cell order alongside the cells.
struct Graphemes<'a> {
    rest: &'a [u32],
}

impl<'a> Graphemes<'a> {
    fn new(table: &'a [u32]) -> Self {
        Self { rest: table }
    }

    /// Whether cell `index` has an entry; the entries must come in cell
    /// order, each with at least one valid codepoint.
    fn at(&mut self, index: u32) -> Result<bool, StateError> {
        let Some(&first) = self.rest.first() else {
            return Ok(false);
        };
        if first != index {
            // An entry behind the cells read so far is out of order.
            check(first > index, "grapheme table order")?;
            return Ok(false);
        }
        let n = self.rest.get(1).ok_or(invalid("grapheme table"))?;
        let n = usize::try_from(*n).map_err(|_| invalid("grapheme table"))?;
        let cps = self
            .rest
            .get(2..2usize.saturating_add(n))
            .ok_or(invalid("grapheme table"))?;
        check(
            (1..=MAX_GRAPHEME).contains(&n)
                && cps
                    .iter()
                    .all(|&cp| cp != 0 && char::from_u32(cp).is_some()),
            "grapheme codepoints",
        )?;
        self.rest = &self.rest[2 + n..];
        Ok(true)
    }

    fn done(&self) -> bool {
        self.rest.is_empty()
    }
}
