//! Owned, renderable copy of a pane's visible grid.

use libghostty_vt::render::{CellIterator, RenderState, RowIterator};
use libghostty_vt::screen::{CellContentTag, CellWide};
use libghostty_vt::style::{self, StyleColor};

use crate::term::PaneTerm;

/// A colour as the grid stores it. Palette indices are unresolved so the
/// compositor can encode them for each client's terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    /// Palette index 0-255.
    Palette(u8),
    Rgb(u8, u8, u8),
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// Grapheme cluster; empty for a blank cell.
    pub text: String,
    /// Columns covered: 1, 2 for a wide character, 0 for the spacer after one
    /// (do not draw it).
    pub width: u8,
    pub style: Style,
}

/// The visible grid at one moment, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    /// `cols * rows` cells.
    pub cells: Vec<Cell>,
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
            fg: s.fg_color.into(),
            bg: s.bg_color.into(),
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

/// Copy the visible grid out of `term`.
///
/// # Panics
/// Only if libghostty fails to allocate.
#[must_use]
pub fn snapshot(term: &PaneTerm) -> Snapshot {
    let mut state = RenderState::new().expect("render state");
    let mut rows = RowIterator::new().expect("row iterator");
    let mut cells = CellIterator::new().expect("cell iterator");
    let snap = state.update(&term.term).expect("render update");

    let cols = snap.cols().unwrap_or(0);
    let nrows = snap.rows().unwrap_or(0);
    let mut out = Vec::with_capacity(usize::from(cols) * usize::from(nrows));

    let mut row_iter = rows.update(&snap).expect("row iteration");
    while let Some(row) = row_iter.next() {
        let mut cell_iter = cells.update(row).expect("cell iteration");
        while let Some(cell) = cell_iter.next() {
            let mut style: Style = cell.style().map(Style::from).unwrap_or_default();
            let mut width = 1;
            if let Ok(raw) = cell.raw_cell() {
                // A cell erased with a background has no text and carries the
                // colour in its content tag instead of its style.
                match raw.content_tag() {
                    Ok(CellContentTag::BgColorPalette) => {
                        if let Ok(i) = raw.bg_color_palette() {
                            style.bg = Color::Palette(i.0);
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
            }
            let mut text = String::new();
            let _ = cell.graphemes_utf8(&mut text);
            out.push(Cell { text, width, style });
        }
    }

    Snapshot {
        cols,
        rows: nrows,
        cells: out,
        cursor: snap.cursor_viewport().ok().flatten().map(|c| (c.x, c.y)),
        cursor_visible: snap.cursor_visible().unwrap_or(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn hidden_cursor_is_reported() {
        let mut t = PaneTerm::new(10, 3, 100);
        t.feed(b"\x1b[?25l");
        assert!(!snapshot(&t).cursor_visible);
    }
}
