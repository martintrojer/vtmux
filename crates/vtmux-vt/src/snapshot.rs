//! Owned, renderable copy of a pane's visible grid.

use libghostty_vt::render::{CellIteration, CellIterator, Dirty, RenderState, RowIterator};
use libghostty_vt::screen::{CellContentTag, CellWide};
use libghostty_vt::style::{self, StyleColor};

use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::term::PaneTerm;
use crate::text::CellText;

/// A colour as the grid stores it. Palette indices are unresolved so the
/// compositor can encode them for each client's terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    /// Palette index 0-255; 0-15 as `30-37`/`90-97` set them.
    Palette(u8),
    /// Palette index 0-15 set in its 256-colour form (`38;5;N`): tmux keeps
    /// `COLOUR_FLAG_256` on it (input.c `input_csi_dispatch_sgr_256_do`),
    /// so `capture-pane -e` writes it back as `38;5;N` and an 8- or
    /// 16-colour terminal maps it through `colour_256to16` (tty.c
    /// `tty_check_fg`) rather than as an aixterm colour.
    Palette256(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// The palette index, whichever form set it.
    #[must_use]
    pub fn palette_index(self) -> Option<u8> {
        match self {
            Self::Palette(n) | Self::Palette256(n) => Some(n),
            _ => None,
        }
    }

    /// `self` with a low-16 palette index in its 256-colour form when
    /// `long` (Ghostty's `fg_palette_256`/`bg_palette_256`).
    #[must_use]
    pub fn in_form(self, long: bool) -> Self {
        match self {
            Self::Palette(n) if long && n < 16 => Self::Palette256(n),
            other => other,
        }
    }
}

/// Underline shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Cell attributes.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per SGR attribute, mirroring the grid"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Underline,
}

/// One grid cell.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// Grapheme cluster; empty for a blank cell.
    pub text: CellText,
    /// Columns covered: 1, 2 for a wide character, 0 for the spacer after one
    /// (do not draw it).
    pub width: u8,
    pub style: Style,
    /// Printed under DEC Special Graphics (tmux's `GRID_ATTR_CHARSET`).
    /// Ghostty keeps no charset per cell; [`PaneTerm`] carries it in the
    /// cell's DECSCA protection bit (see `term.rs`).
    pub acs: bool,
    /// Inside an OSC 8 hyperlink (tmux's `gc->link != 0`).
    pub link: bool,
    /// A TAB cell (tmux's `GRID_FLAG_TAB`), only in a [`crate::Row`]: the
    /// text is the `width` spaces the tab covers and the cells after it
    /// are width 0. [`Snapshot`] cells never have it (a tab draws as the
    /// blanks it covers).
    pub tab: bool,
}

impl Cell {
    /// The text as tmux's grid holds it: a character printed under DEC
    /// Special Graphics is the ASCII one the program sent (tmux maps it
    /// only when drawing, `tty_acs_get`), where Ghostty stores the glyph.
    #[must_use]
    pub fn data(&self) -> std::borrow::Cow<'_, str> {
        if self.acs {
            self.text
                .chars()
                .map(crate::capture::dec_special_source)
                .collect::<String>()
                .into()
        } else {
            self.text.as_str().into()
        }
    }
}

/// `clone_from` reuses the text's buffer (a derived one allocates a new
/// `String`): frames copy whole screens of cells.
impl Clone for Cell {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            width: self.width,
            style: self.style,
            acs: self.acs,
            link: self.link,
            tab: self.tab,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.text.clone_from(&source.text);
        self.width = source.width;
        self.style = source.style;
        self.acs = source.acs;
        self.link = source.link;
        self.tab = source.tab;
    }
}

/// The visible grid at one moment, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    /// `cols * rows` cells.
    pub cells: Vec<Cell>,
    /// Per row: the line soft-wrapped onto the next (tmux's
    /// `GRID_LINE_WRAPPED`). Empty for a composed client screen.
    pub wrapped: Vec<bool>,
    /// Cursor cell in grid terms (x never exceeds `cols - 1`), `None` when it
    /// is outside the viewport.
    pub cursor: Option<(u16, u16)>,
    pub cursor_visible: bool,
}

impl Snapshot {
    /// The cell at column `x`, row `y`.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells
            .get(usize::from(y) * usize::from(self.cols) + usize::from(x))
    }
}

impl From<StyleColor> for Color {
    fn from(c: StyleColor) -> Self {
        match c {
            StyleColor::None => Self::Default,
            StyleColor::Palette(i) => Self::Palette(i.0),
            StyleColor::Rgb(rgb) => Self::Rgb(rgb.r, rgb.g, rgb.b),
        }
    }
}

impl From<style::Underline> for Underline {
    fn from(u: style::Underline) -> Self {
        match u {
            style::Underline::Single => Self::Single,
            style::Underline::Double => Self::Double,
            style::Underline::Curly => Self::Curly,
            style::Underline::Dotted => Self::Dotted,
            style::Underline::Dashed => Self::Dashed,
            _ => Self::None,
        }
    }
}

impl From<style::Style> for Style {
    fn from(s: style::Style) -> Self {
        Self {
            fg: Color::from(s.fg_color).in_form(s.fg_palette_256),
            bg: Color::from(s.bg_color).in_form(s.bg_palette_256),
            underline_color: s.underline_color.into(),
            bold: s.bold,
            faint: s.faint,
            italic: s.italic,
            blink: s.blink,
            inverse: s.inverse,
            invisible: s.invisible,
            strikethrough: s.strikethrough,
            overline: s.overline,
            underline: s.underline.into(),
        }
    }
}

/// Copy the visible grid out of `term` ([`PaneTerm::screen`]).
///
/// # Panics
/// Only if libghostty fails to allocate.
#[must_use]
pub fn snapshot(term: &PaneTerm) -> Snapshot {
    term.screen().snapshot().clone()
}

/// A pane's visible grid kept up to date between frames: a persistent
/// Ghostty render state re-reads only the rows Ghostty marked dirty, so a
/// key's echo costs one row, not the whole screen. Each row carries the
/// [`ScreenView::stamp`] of the update that last changed it, which lets
/// the compositor skip rows that did not change.
///
/// Ghostty's dirty flags have one consumer, so a pane has one view
/// ([`PaneTerm::screen`]); a fresh one reads every row.
pub struct ScreenView {
    state: Option<(
        RenderState<'static>,
        RowIterator<'static>,
        CellIterator<'static>,
    )>,
    /// Shared with the frames that show it: an update copies it only while
    /// a frame still holds the previous one.
    snap: Rc<Snapshot>,
    /// Per row: the stamp of the update that last changed it.
    rows: Vec<u64>,
}

/// Row stamps are unique across every view, so a row of a pane's new
/// terminal (`respawn-pane`) never looks like an old one.
static STAMP: AtomicU64 = AtomicU64::new(1);

/// A row stamp no view has used.
pub(crate) fn fresh_stamp() -> u64 {
    STAMP.fetch_add(1, Ordering::Relaxed)
}

impl std::fmt::Debug for ScreenView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenView").finish_non_exhaustive()
    }
}

impl Default for ScreenView {
    fn default() -> Self {
        Self {
            state: None,
            snap: Rc::new(Snapshot {
                cols: 0,
                rows: 0,
                cells: Vec::new(),
                wrapped: Vec::new(),
                cursor: None,
                cursor_visible: true,
            }),
            rows: Vec::new(),
        }
    }
}

impl ScreenView {
    /// The grid as of the last [`Self::update`].
    #[must_use]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

    /// The grid as of the last [`Self::update`], shared.
    #[must_use]
    pub fn shared(&self) -> Rc<Snapshot> {
        Rc::clone(&self.snap)
    }

    /// Whether the view has read the terminal (holds a render state).
    #[must_use]
    pub(crate) fn is_live(&self) -> bool {
        self.state.is_some()
    }

    /// Per row, the stamp of the update that last changed it; a row whose
    /// stamp is unchanged since an earlier look holds the same cells.
    #[must_use]
    pub fn row_stamps(&self) -> &[u64] {
        &self.rows
    }

    /// Bring the grid up to date with `term`, reading only dirty rows.
    /// Returns whether any cell, the cursor or the size changed.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    pub(crate) fn update(&mut self, term: &PaneTerm) -> bool {
        let stamp = STAMP.fetch_add(1, Ordering::Relaxed);
        let (state, rows, cells) = self.state.get_or_insert_with(|| {
            (
                RenderState::new().expect("render state"),
                RowIterator::new().expect("row iterator"),
                CellIterator::new().expect("cell iterator"),
            )
        });
        let snap = state.update(&term.term).expect("render update");
        let cols = snap.cols().unwrap_or(0);
        let nrows = snap.rows().unwrap_or(0);
        let resized = (cols, nrows) != (self.snap.cols, self.snap.rows);
        let dirty = snap.dirty().unwrap_or(Dirty::Full);
        if dirty == Dirty::Clean && !resized {
            // Nothing to read; the cursor still may have moved.
            return update_cursor(&mut self.snap, &snap);
        }
        let full = resized || dirty == Dirty::Full;
        let mut changed = resized;
        let view = Rc::make_mut(&mut self.snap);
        if resized {
            let n = usize::from(cols) * usize::from(nrows);
            view.cells = vec![Cell::default(); n];
            view.wrapped = vec![false; usize::from(nrows)];
            self.rows = vec![stamp; usize::from(nrows)];
            view.cols = cols;
            view.rows = nrows;
        }
        let width = usize::from(cols);
        let mut scratch = String::new();
        let mut row_iter = rows.update(&snap).expect("row iteration");
        let mut y = 0;
        while let Some(row) = row_iter.next() {
            if y >= usize::from(nrows) {
                break;
            }
            if full || row.dirty().unwrap_or(true) {
                let _ = row.set_dirty(false);
                let wrapped = row
                    .raw_row()
                    .and_then(libghostty_vt::screen::Row::is_wrapped)
                    .unwrap_or(false);
                let line = &mut view.cells[y * width..(y + 1) * width];
                let mut row_changed = view.wrapped[y] != wrapped;
                view.wrapped[y] = wrapped;
                let mut cell_iter = cells.update(row).expect("cell iteration");
                let mut x = 0;
                while let Some(cell) = cell_iter.next() {
                    let Some(slot) = line.get_mut(x) else { break };
                    row_changed |= read_cell(cell, slot, &mut scratch);
                    x += 1;
                }
                if row_changed {
                    self.rows[y] = stamp;
                    changed = true;
                }
            }
            y += 1;
        }
        let _ = snap.set_dirty(Dirty::Clean);
        update_cursor(&mut self.snap, &snap) || changed
    }
}

/// Take the cursor from Ghostty's render state; whether it moved.
fn update_cursor(view: &mut Rc<Snapshot>, snap: &libghostty_vt::render::Snapshot<'_, '_>) -> bool {
    let cursor = snap.cursor_viewport().ok().flatten().map(|c| (c.x, c.y));
    let cursor_visible = snap.cursor_visible().unwrap_or(true);
    if (cursor, cursor_visible) == (view.cursor, view.cursor_visible) {
        return false;
    }
    let view = Rc::make_mut(view);
    view.cursor = cursor;
    view.cursor_visible = cursor_visible;
    true
}

/// Read one cell into `slot`, reusing its text buffer; whether it changed.
fn read_cell(cell: &CellIteration<'_, '_>, slot: &mut Cell, scratch: &mut String) -> bool {
    let raw = cell.raw_cell();
    // Most cells are unstyled: Ghostty's style lookup is the dear part of
    // a read.
    let styled = raw
        .as_ref()
        .map_or(true, |r| r.has_styling().unwrap_or(true));
    let mut style: Style = if styled {
        cell.style().map(Style::from).unwrap_or_default()
    } else {
        Style::default()
    };
    let (mut width, mut acs, mut link) = (1, false, false);
    if let Ok(raw) = raw {
        // A cell erased with a background has no text and carries the
        // colour in its content tag instead of its style.
        match raw.content_tag() {
            Ok(CellContentTag::BgColorPalette) => {
                if let Ok(i) = raw.bg_color_palette() {
                    let long = raw.bg_color_palette_256().unwrap_or(false);
                    style.bg = Color::Palette(i.0).in_form(long);
                }
            }
            Ok(CellContentTag::BgColorRgb) => {
                if let Ok(c) = raw.bg_color_rgb() {
                    style.bg = Color::Rgb(c.r, c.g, c.b);
                }
            }
            _ => {}
        }
        width = match raw.wide() {
            Ok(CellWide::Wide) => 2,
            Ok(CellWide::SpacerTail) => 0,
            _ => 1,
        };
        acs = raw.is_protected().unwrap_or(false);
        link = raw.has_hyperlink().unwrap_or(false);
    }
    scratch.clear();
    let _ = cell.graphemes_utf8(scratch);
    let changed = slot.width != width
        || slot.style != style
        || slot.acs != acs
        || slot.link != link
        || slot.tab
        || slot.text != *scratch;
    if changed {
        slot.text = CellText::from(scratch.as_str());
        slot.width = width;
        slot.style = style;
        slot.acs = acs;
        slot.link = link;
        slot.tab = false;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unstyled cell skips Ghostty's style lookup ([`read_cell`]): it
    /// reads as the default style, a styled one beside it does not.
    #[test]
    fn unstyled_cells_read_as_the_default_style() {
        let mut t = PaneTerm::new(10, 2, 100);
        t.feed(b"\x1b[1;31ma\x1b[0mb\x1b[4mc\x1b[24md");
        let s = snapshot(&t);
        let a = &s.cell(0, 0).unwrap().style;
        assert!(a.bold);
        assert_eq!(a.fg, Color::Palette(1));
        assert_eq!(s.cell(1, 0).unwrap().style, Style::default());
        assert_ne!(s.cell(2, 0).unwrap().style, Style::default());
        assert_eq!(s.cell(3, 0).unwrap().style, Style::default());
        assert_eq!(s.cell(5, 1).unwrap().style, Style::default());
    }

    #[test]
    fn snapshot_reads_text_style_and_cursor() {
        let mut t = PaneTerm::new(10, 3, 100);
        t.feed(b"a\x1b[1;38;5;196mb\x1b[0m\xe4\xb8\x96\x1b[41m\x1b[K");
        let s = snapshot(&t);
        assert_eq!((s.cols, s.rows, s.cells.len()), (10, 3, 30));
        assert_eq!(s.cell(0, 0).unwrap().text, "a");
        let b = s.cell(1, 0).unwrap();
        assert_eq!(b.text, "b");
        assert!(b.style.bold);
        assert_eq!(b.style.fg, Color::Palette(196));
        let wide = s.cell(2, 0).unwrap();
        assert_eq!((wide.text.as_str(), wide.width), ("世", 2));
        assert_eq!(s.cell(3, 0).unwrap().width, 0);
        let erased = s.cell(9, 0).unwrap();
        assert_eq!(erased.text, "");
        assert_eq!(erased.style.bg, Color::Palette(1));
        assert_eq!(s.cursor, Some((4, 0)));
        assert!(s.cursor_visible);
    }

    /// tmux marks cells printed under `ESC ( 0` (input.c `input_print`) and
    /// inside OSC 8; both reach the snapshot, as does the soft-wrap flag.
    #[test]
    fn snapshot_reads_charset_link_and_wrap() {
        let mut t = PaneTerm::new(4, 3, 100);
        t.feed(b"\x1b(0 x\x1b(B \x1b]8;;http://x\x07 \x1b]8;;\x07\r\nabcde");
        let s = snapshot(&t);
        let flags: Vec<_> = (0..4)
            .map(|x| {
                let c = s.cell(x, 0).unwrap();
                (c.acs, c.link)
            })
            .collect();
        assert_eq!(
            flags,
            [(true, false), (true, false), (false, false), (false, true)]
        );
        assert_eq!(s.cell(1, 0).unwrap().text, "│");
        assert_eq!(s.wrapped, [false, true, false]);
    }

    /// The kept view, read after every piece of output, matches a fresh
    /// read of the same grid, and only the rows a piece changed get a new
    /// stamp.
    #[test]
    fn kept_view_matches_a_fresh_read() {
        let pieces: [&[u8]; 12] = [
            b"one\r\ntwo\r\nthree",
            b"\x1b[2;2H\x1b[31mX",
            b"\r\n\r\n\r\nscrolled\r\nmore\r\n",
            b"\x1b[?1049hALT\x1b[41m\x1b[K",
            b"\x1b[?1049l",
            b"\x1b]4;1;rgb:ff/00/00\x07",
            b"\x1b[3;1H\xe4\xb8\x96wide\x1b(0q\x1b(B",
            b"\x1b]8;;http://x\x07link\x1b]8;;\x07",
            b"\x1b[2J\x1b[H",
            b"0123456789abcdefwrapped",
            b"\x1bc",
            b"\x1b[?25lend",
        ];
        let mut kept = PaneTerm::new(16, 4, 100);
        let mut fresh = PaneTerm::new(16, 4, 100);
        for (i, piece) in pieces.iter().enumerate() {
            kept.feed(piece);
            fresh.feed(piece);
            if i == 6 {
                kept.resize(12, 5);
                fresh.resize(12, 5);
            }
            let mut view = ScreenView::default();
            view.update(&fresh);
            assert_eq!(kept.screen().snapshot(), view.snapshot(), "after piece {i}");
        }
        let before = kept.screen().row_stamps().to_vec();
        kept.feed(b"\x1b[2;1Hz");
        let after = kept.screen().row_stamps().to_vec();
        let moved: Vec<_> = (0..after.len())
            .filter(|&y| before[y] != after[y])
            .collect();
        assert_eq!(moved, [1], "only the written row changed");
        assert!(
            !kept.view.borrow_mut().update(&kept),
            "nothing new: no change"
        );
    }

    /// A released view (a hidden pane's) is read again whole: the same
    /// grid, with output written while it was gone, and every row stamped
    /// anew so no cached row of the old view is taken for it.
    #[test]
    fn released_view_reads_the_screen_again() {
        let mut t = PaneTerm::new(10, 3, 100);
        t.feed(b"one\r\ntwo");
        let before = t.screen().row_stamps().to_vec();
        t.release_screen();
        assert!(!t.view.borrow().is_live());
        t.feed(b"\r\nthree");
        let mut fresh = PaneTerm::new(10, 3, 100);
        fresh.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(t.screen().snapshot(), &snapshot(&fresh));
        let after = t.screen().row_stamps().to_vec();
        assert!(
            before.iter().all(|s| !after.contains(s)),
            "{before:?} {after:?}"
        );
    }

    #[test]
    fn hidden_cursor_is_reported() {
        let mut t = PaneTerm::new(10, 3, 100);
        t.feed(b"\x1b[?25l");
        assert!(!snapshot(&t).cursor_visible);
    }
}
