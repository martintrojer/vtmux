//! tmux-shaped capture of terminal contents (`capture-pane`).
//!
//! Mirrors `cmd_capture_pane_history` (cmd-capture-pane.c:315-413) and
//! `grid_string_cells` / `grid_string_cells_code` (grid.c:859-1260).

use libghostty_vt::Terminal;
use libghostty_vt::error::Error;
use libghostty_vt::screen::{CellContentTag, CellWide, GridRef};
use libghostty_vt::terminal::{Point, PointCoordinate};

use crate::snapshot::{Cell, Color, Style, Underline};
use std::rc::Rc;

use crate::term::{HistoryRow, LineMark, PaneTerm};

mod raw;
pub use raw::capture_grid;

/// One end of the captured range, as `-S` / `-E` give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineRef {
    /// No flag: the first visible row for the start, the last for the end.
    Visible,
    /// `-S n` / `-E n`: row `n` of the visible area, negative is history.
    Line(i64),
    /// `-S -` / `-E -`: the oldest history line / the last visible line.
    Start,
    /// Alias for the `-` form at the end of the range.
    End,
}

/// Options for [`capture`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "one per capture-pane flag")]
pub struct CaptureOpts {
    /// `-e`: emit SGR sequences for styles.
    pub escapes: bool,
    /// `-J`: join soft-wrapped rows and keep trailing spaces.
    pub join: bool,
    /// `-C`: escape the sequences and a backslash (`\033`, `\\`).
    pub octal: bool,
    /// `-N`: keep trailing spaces.
    pub keep_spaces: bool,
    /// `-T`: stop at the last cell with a character.
    pub used_only: bool,
    /// `-L`: each line starts with its number.
    pub numbers: bool,
    /// `-I`: each line starts with when it entered the history.
    pub times: bool,
    /// `-F`: each line starts with its flags.
    pub flags: bool,
    pub start: LineRef,
    pub end: LineRef,
}

impl Default for CaptureOpts {
    fn default() -> Self {
        Self {
            escapes: false,
            join: false,
            octal: false,
            keep_spaces: false,
            used_only: false,
            numbers: false,
            times: false,
            flags: false,
            start: LineRef::Visible,
            end: LineRef::Visible,
        }
    }
}

/// `capture-pane -a` (cmd-capture-pane.c:268-279): the primary screen
/// saved when the alternate screen came up (`saved_grid`), which has no
/// history; `None` while no alternate screen is up.
#[must_use]
pub fn capture_alternate(term: &PaneTerm, opts: CaptureOpts) -> Option<Vec<u8>> {
    let rows = term.saved_rows()?;
    let last = rows.len().saturating_sub(1);
    let mut top = resolve(opts.start, 0, last, true);
    let mut bottom = resolve(opts.end, 0, last, false);
    if bottom < top {
        std::mem::swap(&mut top, &mut bottom);
    }
    let mut out = Vec::new();
    let mut last_style = Style::default();
    for (y, row) in rows.iter().enumerate().take(bottom + 1).skip(top) {
        let (cells, wrapped) = row.thaw();
        if opts.numbers {
            out.extend_from_slice(format!("{y} ").as_bytes());
        }
        if opts.times {
            out.extend_from_slice(b"0 ");
        }
        if opts.flags {
            out.extend_from_slice(row_flags(&cells, wrapped, None).as_bytes());
        }
        emit_row(&mut out, &cells, opts, &mut last_style);
        if !opts.join || !wrapped {
            out.push(b'\n');
        }
    }
    Some(out)
}

/// Capture `term` the way `capture-pane` does, returning the buffer before
/// `-p` strips its final newline.
#[must_use]
pub fn capture(term: &PaneTerm, opts: CaptureOpts) -> Vec<u8> {
    let sy = usize::from(term.rows());
    let hsize = term.history_size();
    let last = hsize + sy - 1;

    let mut top = resolve(opts.start, hsize, last, true);
    let mut bottom = resolve(opts.end, hsize, last, false);
    if bottom < top {
        std::mem::swap(&mut top, &mut bottom);
    }

    let cols = usize::from(term.cols());
    let mut out = Vec::new();
    // tmux threads one `lastgc` through every line of a capture (cmd-capture-pane.c:333).
    let mut last_style = Style::default();
    let marks = if opts.flags {
        term.line_marks()
    } else {
        Vec::new()
    };
    for y in top..=bottom {
        let (cells, wrapped) = read_row(term, y, hsize, cols);
        // cmd-capture-pane.c:375-410: the line's number, history time and
        // flags first.
        if opts.numbers {
            let n = i64::try_from(y).unwrap_or(0) - i64::try_from(hsize).unwrap_or(0);
            out.extend_from_slice(format!("{n} ").as_bytes());
        }
        if opts.times {
            let t = if y < hsize {
                term.history_time(term.history_id(y))
            } else {
                0
            };
            out.extend_from_slice(format!("{t} ").as_bytes());
        }
        if opts.flags {
            let mark = marks.iter().find(|(row, _)| *row == y).map(|(_, m)| *m);
            out.extend_from_slice(row_flags(&cells, wrapped, mark).as_bytes());
        }
        emit_row(&mut out, &cells, opts, &mut last_style);
        if !opts.join || !wrapped {
            out.push(b'\n');
        }
    }
    out
}

/// tmux's `GRID_LINE_EXTENDED` (grid.c `grid_need_extended_cell`): a cell
/// wider than one column or of more than one byte, an RGB colour, an
/// attribute past the first eight, an underline colour, a link or a tab.
fn row_extended(cells: &[RowCell]) -> bool {
    cells.iter().any(|c| {
        let st = &c.style;
        c.link
            || c.tab != 0
            || c.wide == Width::Double
            || c.text.len() > 1
            || matches!(st.fg, Color::Rgb(..))
            || matches!(st.bg, Color::Rgb(..))
            || st.underline_color != Color::Default
            || st.strikethrough
            || st.overline
            || !matches!(st.underline, Underline::None | Underline::Single)
    })
}

/// `capture-pane -F` (cmd-capture-pane.c:390-410): the line's flags and a
/// space (`X`: [`row_extended`]). vtmux has no dead lines (`D`).
fn row_flags(cells: &[RowCell], wrapped: bool, mark: Option<LineMark>) -> String {
    let link = cells.iter().any(|c| c.link);
    let extended = row_extended(cells);
    let mut f = String::new();
    if link {
        f.push('H');
    }
    if mark == Some(LineMark::Output) {
        f.push('O');
    }
    if mark == Some(LineMark::Prompt) {
        f.push('P');
    }
    if wrapped {
        f.push('W');
    }
    if extended {
        f.push('X');
    }
    if f.is_empty() {
        f.push('-');
    }
    f.push(' ');
    f
}

/// tmux's range arithmetic: cmd-capture-pane.c:354-395.
fn resolve(r: LineRef, hsize: usize, last: usize, is_start: bool) -> usize {
    let n = match r {
        LineRef::Start if is_start => return 0,
        LineRef::Start | LineRef::End => return last,
        LineRef::Visible => return if is_start { hsize } else { last },
        LineRef::Line(n) => n,
    };
    // `args_strtonum_and_expand(INT_MIN, SHRT_MAX)`: out of range is the default.
    if !(i64::from(i32::MIN)..=i64::from(i16::MAX)).contains(&n) {
        return if is_start { hsize } else { last };
    }
    let pos = if n < 0 {
        let back = usize::try_from(-n).unwrap_or(usize::MAX);
        // Before the oldest line: clamp to it, for -S and -E alike.
        hsize.saturating_sub(back)
    } else {
        hsize + usize::try_from(n).unwrap_or(0)
    };
    pos.min(last)
}

/// How a cell sits in a wide character.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Width {
    #[default]
    Narrow,
    Double,
    /// The second column of a wide character (tmux's PADDING cell).
    SpacerTail,
    /// The last column left empty when a wide character wrapped.
    SpacerHead,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RowCell {
    text: String,
    style: Style,
    /// Blank cell with no text (tmux stores a space).
    empty: bool,
    wide: Width,
    acs: bool,
    link: bool,
    /// The hyperlink's URI, read only for `capture-pane -R`.
    uri: Option<String>,
    /// Ghostty's tab mark (`Cell.tab`, `patches/0002-tab-cell-mark.patch`):
    /// 0 none, 1..=32 the first cell of a tab that many columns wide,
    /// [`TAB_CONTINUATION`] for the other cells of it.
    tab: u8,
}

/// `Cell.tab_continuation` in the patched Ghostty.
const TAB_CONTINUATION: u8 = 255;

impl RowCell {
    fn is_default_blank(&self) -> bool {
        self.empty
            && self.style == Style::default()
            && self.wide == Width::Narrow
            && !self.link
            && self.tab == 0
    }

    /// Cells tmux does not store: the second column of a wide character and
    /// the columns after the first of a tab (its PADDING cells).
    fn spacer(&self) -> bool {
        matches!(self.wide, Width::SpacerTail | Width::SpacerHead) || self.tab == TAB_CONTINUATION
    }

    /// A cell tmux writes along with the one before it: the second column
    /// of a wide character, or any column of a tab. Not the blank left at
    /// a line's end when a wide character wrapped, which tmux never writes.
    fn padding(&self) -> bool {
        self.wide == Width::SpacerTail || self.tab != 0
    }

    /// The cell as the renderer sees it (as [`crate::snapshot`] reads it).
    fn to_cell(&self) -> Cell {
        // tmux stores a tab as one cell as wide as the tab, then padding.
        let (text, width, tab) = match (self.tab, self.wide) {
            (TAB_CONTINUATION, _) => (String::new(), 0, false),
            (n @ 1..=32, _) => (" ".repeat(usize::from(n)), n, true),
            (_, Width::Double) => (self.text.clone(), 2, false),
            (_, Width::SpacerTail) => (self.text.clone(), 0, false),
            (_, Width::Narrow | Width::SpacerHead) => (self.text.clone(), 1, false),
        };
        Cell {
            text,
            width,
            style: self.style,
            acs: self.acs,
            link: self.link,
            tab,
        }
    }
}

/// One row of tmux's grid as copy mode reads it (`grid_get_line`): every
/// column, the second column of a wide character with width 0 (tmux's
/// PADDING cell), and whether the line wrapped onto the next
/// (`GRID_LINE_WRAPPED`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    /// `grid_line_length` (grid.c:1669-1686): one past the last cell that
    /// is not a blank, a padding cell counting as content.
    #[must_use]
    pub fn length(&self) -> usize {
        self.cells
            .iter()
            .rposition(|c| c.width == 0 || !(c.text.is_empty() || c.text == " "))
            .map_or(0, |i| i + 1)
    }

    /// `grid_line_limit` (grid.c:1690-1706): the last column of the line's
    /// content that is not padding, 0 for an empty line.
    #[must_use]
    pub fn limit(&self) -> usize {
        let mut x = self.length().saturating_sub(1);
        while x > 0 && self.cells.get(x).is_some_and(|c| c.width == 0) {
            x -= 1;
        }
        x
    }

    /// The line's text up to [`Row::length`], padding skipped
    /// (`format_grid_line`, format.c:7171-7196).
    #[must_use]
    pub fn text(&self) -> String {
        self.cells[..self.length()]
            .iter()
            .filter(|c| c.width != 0)
            .map(|c| match (c.tab, c.text.is_empty()) {
                (true, _) => "\t",
                (false, true) => " ",
                (false, false) => &c.text,
            })
            .collect()
    }
}

impl PaneTerm {
    /// History row `id` (see [`PaneTerm::history_id`]) while it is still
    /// retained.
    #[must_use]
    pub fn history_row(&self, id: u64) -> Option<Row> {
        let cols = usize::from(self.cols());
        let (cells, wrapped) = match self.history_screen_row(id)? {
            HistoryRow::Screen(y) => {
                self.note_history_read();
                read_screen_row(&self.term, y, cols)
            }
            HistoryRow::Frozen(f) => f.thaw(),
        };
        Some(Row {
            cells: cells.iter().map(RowCell::to_cell).collect(),
            wrapped,
        })
    }

    /// Row `y` of tmux's grid: `0..history_size()` is history, oldest
    /// first, then the visible rows. While the alternate screen is up the
    /// history is the primary screen's, copied on entry. Out of range is an
    /// empty row.
    #[must_use]
    pub fn row(&self, y: usize) -> Row {
        let hsize = self.history_size();
        if y >= hsize + usize::from(self.rows()) {
            return Row::default();
        }
        let (cells, wrapped) = read_row(self, y, hsize, usize::from(self.cols()));
        Row {
            cells: cells.iter().map(RowCell::to_cell).collect(),
            wrapped,
        }
    }
}

/// A copy of a pane's grid as it is now (tmux's `window_copy_clone_screen`
/// copies the screen's grid), read a row at a time: Ghostty copies the
/// screen's pages in one go, so taking it costs a memory copy, not a read
/// of every row. Later output to the pane does not show in it and rows the
/// pane drops from its history stay.
pub struct ScreenCopy {
    /// The pane's active screen, history included.
    term: Option<Terminal<'static, 'static>>,
    /// Ghostty's history rows in `term`: tmux's row `y` is screen row
    /// `y + retained - hsize` (see [`read_row`]).
    retained: usize,
    /// The primary history copied when the alternate screen came up, which
    /// is tmux's history while it is up (see [`read_row`]).
    frozen: Option<Rc<Vec<FrozenRow>>>,
    hsize: usize,
    rows: usize,
    cols: usize,
    /// `grid_line_time` of each history row.
    times: Vec<i64>,
    marks: Vec<(usize, LineMark)>,
}

impl std::fmt::Debug for ScreenCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenCopy")
            .field("hsize", &self.hsize)
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .finish_non_exhaustive()
    }
}

impl ScreenCopy {
    /// Row `y` as [`PaneTerm::row`] gave it when the copy was taken.
    #[must_use]
    pub fn row(&self, y: usize) -> Row {
        if y >= self.hsize + self.rows {
            return Row::default();
        }
        let (cells, wrapped) = match (&self.frozen, &self.term) {
            (Some(frozen), _) if y < self.hsize => {
                frozen.get(y).map(FrozenRow::thaw).unwrap_or_default()
            }
            (_, Some(term)) => read_screen_row(term, y + self.retained - self.hsize, self.cols),
            (_, None) => return Row::default(),
        };
        Row {
            cells: cells.iter().map(RowCell::to_cell).collect(),
            wrapped,
        }
    }

    /// The hyperlink of the cell at `(x, y)` when the copy was taken (see
    /// [`PaneTerm::hyperlink`]); history hidden by an alternate screen has
    /// none.
    #[must_use]
    pub fn hyperlink(&self, x: usize, y: usize) -> Option<String> {
        if y >= self.hsize + self.rows || (self.frozen.is_some() && y < self.hsize) {
            return None;
        }
        hyperlink_at(self.term.as_ref()?, y + self.retained - self.hsize, x)
    }

    /// `history_size` when the copy was taken.
    #[must_use]
    pub fn history_size(&self) -> usize {
        self.hsize
    }

    /// When history row `y` scrolled in (see [`PaneTerm::history_time`]).
    #[must_use]
    pub fn history_time(&self, y: usize) -> i64 {
        self.times.get(y).copied().unwrap_or(0)
    }

    /// See [`PaneTerm::line_marks`].
    #[must_use]
    pub fn line_marks(&self) -> &[(usize, LineMark)] {
        &self.marks
    }
}

impl PaneTerm {
    /// The OSC 8 URI of the cell at `(x, y)` of tmux's grid (history
    /// first), stepping left off a wide character's padding
    /// (`format_grid_hyperlink`, format.c:7200-7218). History hidden by an
    /// alternate screen has none.
    #[must_use]
    pub fn hyperlink(&self, x: usize, y: usize) -> Option<String> {
        let hsize = self.history_size();
        if y >= hsize + usize::from(self.rows()) || (self.alternate_on() && y < hsize) {
            return None;
        }
        let retained = self.term.scrollback_rows().unwrap_or(0);
        hyperlink_at(&self.term, y + retained - hsize, x)
    }

    /// Copy the grid (see [`ScreenCopy`]). Compressed history pages are
    /// copied compressed: the pane's stay so, the copy decodes a page on
    /// its first read.
    #[must_use]
    pub fn copy_screen(&self) -> ScreenCopy {
        let hsize = self.history_size();
        let frozen = if self.alternate_on() {
            self.frozen_rows()
        } else {
            if hsize > 0 {
                self.note_history_read();
            }
            None
        };
        let term = self.term.clone_screen().ok();
        let retained = term
            .as_ref()
            .and_then(|t| t.scrollback_rows().ok())
            .unwrap_or(0);
        ScreenCopy {
            term,
            retained,
            frozen,
            hsize,
            rows: usize::from(self.rows()),
            cols: usize::from(self.cols()),
            times: (0..hsize)
                .map(|y| self.history_time(self.history_id(y)))
                .collect(),
            marks: self.line_marks(),
        }
    }
}

/// A history row copied out of Ghostty's primary screen before an alternate
/// screen hid it. Trailing default blanks are not stored.
#[derive(Clone, Debug)]
pub(crate) struct FrozenRow {
    cells: Vec<RowCell>,
    len: usize,
    wrapped: bool,
}

impl FrozenRow {
    /// Copy screen row `y` (history included) of the active screen.
    pub(crate) fn capture(term: &Terminal<'_, '_>, y: usize, cols: usize) -> Self {
        let (mut cells, wrapped) = read_screen_row(term, y, cols);
        let len = cells.len();
        while cells.last().is_some_and(RowCell::is_default_blank) {
            cells.pop();
        }
        Self {
            cells,
            len,
            wrapped,
        }
    }

    fn thaw(&self) -> (Vec<RowCell>, bool) {
        let mut cells = self.cells.clone();
        cells.resize(
            self.len,
            RowCell {
                empty: true,
                ..RowCell::default()
            },
        );
        (cells, self.wrapped)
    }
}

/// Row `y` of tmux's grid: `0..hsize` is history, then the visible rows.
///
/// While the alternate screen is up, tmux keeps showing the primary history
/// (screen.c:700-721 saves only the visible rows), but Ghostty can only read
/// the active screen, so history comes from the copy taken on entry.
fn read_row(term: &PaneTerm, y: usize, hsize: usize, cols: usize) -> (Vec<RowCell>, bool) {
    if term.alternate_on() && y < hsize {
        return term.frozen_row(y).map(FrozenRow::thaw).unwrap_or_default();
    }
    if y < hsize {
        term.note_history_read();
    }
    // Ghostty may retain more history than tmux has: skip the oldest rows.
    let retained = term.term.scrollback_rows().unwrap_or(0);
    read_screen_row(&term.term, y + retained - hsize, cols)
}

/// Read one row of the active screen, spacer cells included: one page-list
/// walk for the row and one call for its cells; a cell's style and
/// grapheme are looked up only when it has them.
fn read_screen_row(term: &Terminal<'_, '_>, y: usize, cols: usize) -> (Vec<RowCell>, bool) {
    // Ghostty resolves a screen point by walking its pages from the oldest
    // and an active point from the newest: a row of the active area is
    // read as one, at a cost that does not grow with the history.
    let (at, y): (fn(PointCoordinate) -> Point, usize) =
        match y.checked_sub(term.scrollback_rows().unwrap_or(0)) {
            Some(active) => (Point::Active, active),
            None => (Point::Screen, y),
        };
    #[cfg(test)]
    tests::LOOKUPS.with(|n| n.set(n.get() + 1));
    let Ok(row_ref) = term.grid_ref(at(PointCoordinate {
        x: 0,
        y: u32::try_from(y).unwrap_or(u32::MAX),
    })) else {
        return (Vec::new(), false);
    };
    let (wrapped, styled, linked) = row_ref.row().map_or((false, true, true), |row| {
        (
            row.is_wrapped().unwrap_or(false),
            // Per-row flags let most cells skip the style and link lookups.
            row.is_styled().unwrap_or(true),
            row.has_hyperlink().unwrap_or(true),
        )
    });
    let mut raw = Vec::new();
    if row_ref.row_cells(&mut raw).is_err() {
        return (Vec::new(), wrapped);
    }
    // An incomplete reflow can leave the page narrower than the screen.
    raw.truncate(cols);
    let mut cells = Vec::with_capacity(raw.len());
    for (x, &cell) in raw.iter().enumerate() {
        let gr = row_ref.with_x(u16::try_from(x).unwrap_or(u16::MAX));
        let wide = match cell.wide() {
            Ok(CellWide::Wide) => Width::Double,
            Ok(CellWide::SpacerTail) => Width::SpacerTail,
            Ok(CellWide::SpacerHead) => Width::SpacerHead,
            _ => Width::Narrow,
        };
        // Most cells are unstyled: skip the style lookup for them.
        let mut style = if styled && cell.has_styling().unwrap_or(true) {
            gr.style().map(Style::from).unwrap_or_default()
        } else {
            Style::default()
        };
        let tab = cell.tab().unwrap_or(0);
        let text = match cell.content_tag() {
            // One codepoint: no grapheme buffer needed.
            Ok(CellContentTag::Codepoint) => cell
                .codepoint()
                .ok()
                .and_then(char::from_u32)
                .filter(|&c| c != '\0')
                .map(String::from)
                .unwrap_or_default(),
            Ok(CellContentTag::CodepointGrapheme) => cell_text(&gr),
            Ok(CellContentTag::BgColorPalette) => {
                if let Ok(i) = cell.bg_color_palette() {
                    style.bg = Color::Palette(i.0);
                }
                String::new()
            }
            Ok(CellContentTag::BgColorRgb) => {
                if let Ok(c) = cell.bg_color_rgb() {
                    style.bg = Color::Rgb(c.r, c.g, c.b);
                }
                String::new()
            }
            Err(_) => String::new(),
        };
        // A tab's first cell is a TAB cell: `\t` in a capture (grid.c
        // `grid_string_cells`), spaces everywhere else (`to_cell`).
        let text = if matches!(tab, 1..=32) {
            "\t".to_owned()
        } else {
            text
        };
        let link = linked && cell.has_hyperlink().unwrap_or(false);
        cells.push(RowCell {
            empty: text.is_empty(),
            text,
            style,
            wide,
            acs: cell.is_protected().unwrap_or(false),
            link,
            // Links are rare: their URI is read as the cell is.
            uri: link.then(|| hyperlink_uri(&gr)).flatten(),
            tab,
        });
    }
    whole_tabs(&mut cells);
    (cells, wrapped)
}

/// Undo what a resize does to a tab: Ghostty reflows cell by cell and can cut a
/// tab at the row's end, leaving its first cell without its columns or its
/// continuation at the start of the next row. tmux wraps a TAB cell whole
/// (`grid_reflow`), so such halves are plain blanks here. A tab tmux itself
/// could hold (`window_copy` over a partly erased tab) stays.
fn whole_tabs(cells: &mut [RowCell]) {
    let blank = |c: &mut RowCell| {
        c.tab = 0;
        c.text.clear();
        c.empty = true;
    };
    if cells.first().is_some_and(|c| c.tab == TAB_CONTINUATION) {
        let n = cells
            .iter()
            .take_while(|c| c.tab == TAB_CONTINUATION)
            .count();
        cells[..n].iter_mut().for_each(blank);
    }
    if let Some(x) = cells
        .iter()
        .rposition(|c| c.tab != 0 && c.tab != TAB_CONTINUATION)
        && x + usize::from(cells[x].tab) > cells.len()
    {
        let n = cells.len() - x;
        cells[x..x + n].iter_mut().for_each(blank);
    }
}

/// The URI of the hyperlink on the cell at `gr`.
fn hyperlink_uri(gr: &GridRef<'_>) -> Option<String> {
    let mut buf = vec![0u8; 256];
    let n = loop {
        match gr.hyperlink_uri(&mut buf) {
            Ok(n) => break n,
            Err(Error::OutOfSpace { required }) if required > buf.len() => {
                buf.resize(required, 0);
            }
            Err(_) => return None,
        }
    };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// tmux's cell data size: `UTF8_SIZE` (tmux.h:721).
const UTF8_SIZE: usize = 32;

/// The URI of cell `x` of screen row `y` (history included), or of the
/// character a padding cell belongs to.
fn hyperlink_at(term: &Terminal<'_, '_>, y: usize, x: usize) -> Option<String> {
    let (cells, _) = read_screen_row(term, y, usize::MAX);
    let mut x = x;
    cells.get(x)?;
    while cells[x].wide == Width::SpacerTail {
        x = x.checked_sub(1)?;
    }
    if !cells[x].link {
        return None;
    }
    let (at, y): (fn(PointCoordinate) -> Point, usize) =
        match y.checked_sub(term.scrollback_rows().unwrap_or(0)) {
            Some(active) => (Point::Active, active),
            None => (Point::Screen, y),
        };
    let gr = term
        .grid_ref(at(PointCoordinate {
            x: u16::try_from(x).ok()?,
            y: u32::try_from(y).ok()?,
        }))
        .ok()?;
    let mut buf = vec![0u8; 256];
    let n = loop {
        match gr.hyperlink_uri(&mut buf) {
            Ok(n) => break n,
            Err(Error::OutOfSpace { required }) if required > buf.len() => buf.resize(required, 0),
            Err(_) => return None,
        }
    };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// The cell's cluster as tmux stores it: a combining character that would
/// take the cell past `UTF8_SIZE` bytes is discarded, the base stays
/// (`screen_write_combine`, screen-write.c:2922-2925).
fn cell_text(gr: &GridRef<'_>) -> String {
    let mut buf = vec!['\0'; 16];
    let n = loop {
        match gr.graphemes(&mut buf) {
            Ok(n) => break n,
            Err(Error::OutOfSpace { required }) if required > buf.len() => {
                buf.resize(required, '\0');
            }
            Err(_) => return String::new(),
        }
    };
    let mut text = String::new();
    for c in buf.iter().take(n) {
        if text.len() + c.len_utf8() <= UTF8_SIZE {
            text.push(*c);
        }
    }
    text
}

fn emit_row(out: &mut Vec<u8>, cells: &[RowCell], opts: CaptureOpts, last: &mut Style) {
    // `cellused`: one past the last cell with text.
    let used = cells.iter().rposition(|c| !c.empty).map_or(0, |i| i + 1);
    // GRID_STRING_EMPTY_CELLS unless -J or -T (cmd-capture-pane.c:349-350):
    // to `cellsize`, else `cellused`.
    let end = if opts.join || opts.used_only {
        used
    } else if opts.escapes || opts.keep_spaces {
        tmux_cellsize(cells, used)
    } else {
        cells.len()
    };

    let mut line = Vec::new();
    // Spacer cells are tmux's PADDING cells: skipped (grid.c:1214-1215).
    for c in cells[..end.min(cells.len())].iter().filter(|c| !c.spacer()) {
        if opts.escapes {
            let at = line.len();
            sgr_code(&mut line, last, &c.style);
            if opts.octal {
                escape_esc(&mut line, at);
            }
            *last = c.style;
        }
        if c.empty {
            line.push(b' ');
        } else if opts.octal && c.text == "\\" {
            line.extend_from_slice(b"\\\\");
        } else {
            line.extend_from_slice(c.text.as_bytes());
        }
    }
    if !opts.join && !opts.keep_spaces {
        // GRID_STRING_TRIM_SPACES (grid.c:1263).
        while line.last() == Some(&b' ') {
            line.pop();
        }
    }
    out.extend_from_slice(&line);
}

/// `GRID_STRING_ESCAPE_SEQUENCES`: each ESC from `at` on as `\033`.
fn escape_esc(line: &mut Vec<u8>, at: usize) {
    let tail: Vec<u8> = line.split_off(at);
    for b in tail {
        if b == 0x1b {
            line.extend_from_slice(b"\\033");
        } else {
            line.push(b);
        }
    }
}

/// How many cells tmux has allocated for a row, which decides how many
/// trailing blank cells can carry an SGR reset under `-e` without `-N`.
///
/// tmux grows a line with `grid_expand_line` (grid.c:564-589) once per run of
/// same-attribute text that `screen_write` flushes (`grid_set_cells`,
/// grid.c:690-715): to a quarter of the width if the run ends below it, to a
/// half if below that, else to the full width. Reproduced by treating each
/// maximal run of written cells with equal style as one flush. Approximation: it does not
/// know about rows that were written and later erased or rewritten out of
/// order, which keep their old allocation in tmux.
fn tmux_cellsize(cells: &[RowCell], used: usize) -> usize {
    let width = cells.len();
    // A wide character's padding and a tab's columns are written with it.
    let written = |c: &RowCell| !c.empty || c.style.bg != Color::Default || c.padding();
    let extent = cells
        .iter()
        .rposition(written)
        .map_or(0, |i| i + 1)
        .max(used);
    let mut size = 0;
    let mut i = 0;
    while i < extent {
        if !written(&cells[i]) {
            i += 1;
            continue;
        }
        // A run is what one grid_set_cells writes: same style, charset
        // and link.
        let same = |a: &RowCell, b: &RowCell| {
            b.spacer() || (a.style == b.style && a.acs == b.acs && a.uri == b.uri)
        };
        let mut j = i + 1;
        while j < extent && written(&cells[j]) && same(&cells[j], &cells[i]) {
            j += 1;
        }
        if j > size {
            size = if j < width / 4 {
                width / 4
            } else if j < width / 2 {
                width / 2
            } else if width > j {
                width
            } else {
                j
            };
        }
        i = j;
    }
    size
}

// --- SGR, after grid_string_cells_code (grid.c:1066-1177) -------------------

const ATTRS: [(u32, u32); 13] = [
    (1 << 0, 1),   // bright
    (1 << 1, 2),   // dim
    (1 << 2, 3),   // italics
    (1 << 3, 4),   // underscore
    (1 << 4, 5),   // blink
    (1 << 5, 7),   // reverse
    (1 << 6, 8),   // hidden
    (1 << 7, 9),   // strikethrough
    (1 << 8, 42),  // underscore 2
    (1 << 9, 43),  // underscore 3
    (1 << 10, 44), // underscore 4
    (1 << 11, 45), // underscore 5
    (1 << 12, 53), // overline
];

fn attr_bits(s: &Style) -> u32 {
    let mut a = 0;
    a |= u32::from(s.bold);
    a |= u32::from(s.faint) << 1;
    a |= u32::from(s.italic) << 2;
    a |= u32::from(s.blink) << 4;
    a |= u32::from(s.inverse) << 5;
    a |= u32::from(s.invisible) << 6;
    a |= u32::from(s.strikethrough) << 7;
    a |= u32::from(s.overline) << 12;
    a |= match s.underline {
        Underline::None => 0,
        Underline::Single => 1 << 3,
        Underline::Double => 1 << 8,
        Underline::Curly => 1 << 9,
        Underline::Dotted => 1 << 10,
        Underline::Dashed => 1 << 11,
    };
    a
}

/// `grid_string_cells_fg`/`_bg` (grid.c:859-968). `base` is 30 for fg, 40 for bg.
///
/// Ghostty keeps one palette index where tmux distinguishes `31`, `91` and
/// `38;5;1`; indices 0-7 and 8-15 are emitted in the short forms and
/// 16-255 as `38;5;n`.
fn colour_values(c: Color, base: i32, ext: i32) -> Vec<i32> {
    match c {
        Color::Default => vec![base + 9],
        Color::Palette(i) if i < 8 => vec![base + i32::from(i)],
        Color::Palette(i) if i < 16 => vec![base + 60 + i32::from(i) - 8],
        Color::Palette(i) => vec![ext, 5, i32::from(i)],
        Color::Rgb(r, g, b) => vec![ext, 2, i32::from(r), i32::from(g), i32::from(b)],
    }
}

/// `grid_string_cells_us` (grid.c:971-1000): nothing for the default.
fn underline_values(c: Color) -> Vec<i32> {
    match c {
        Color::Default => vec![],
        Color::Palette(i) => vec![58, 5, i32::from(i)],
        Color::Rgb(r, g, b) => vec![58, 2, i32::from(r), i32::from(g), i32::from(b)],
    }
}

/// `grid_string_cells_add_code` (grid.c:1003-1032).
fn add_code(out: &mut Vec<u8>, reset: bool, new: &[i32], old: &[i32]) {
    if new.is_empty() || (!reset && new == old) {
        return;
    }
    if reset && (new[0] == 49 || new[0] == 39) {
        return;
    }
    out.extend_from_slice(b"\x1b[");
    let parts: Vec<String> = new.iter().map(ToString::to_string).collect();
    out.extend_from_slice(parts.join(";").as_bytes());
    out.push(b'm');
}

fn sgr_code(out: &mut Vec<u8>, last: &Style, cur: &Style) {
    let (attr, mut lastattr) = (attr_bits(cur), attr_bits(last));
    let mut s: Vec<u32> = Vec::new();

    // If any attribute is removed, begin with 0.
    let us_cleared =
        last.underline_color != Color::Default && cur.underline_color == Color::Default;
    if ATTRS
        .iter()
        .any(|(mask, _)| (!attr & mask != 0 && lastattr & mask != 0) || us_cleared)
    {
        s.push(0);
        lastattr = 0;
    }
    for (mask, code) in ATTRS {
        if attr & mask != 0 && lastattr & mask == 0 {
            s.push(code);
        }
    }
    if !s.is_empty() {
        out.extend_from_slice(b"\x1b[");
        for (i, v) in s.iter().enumerate() {
            if *v < 10 {
                out.extend_from_slice(v.to_string().as_bytes());
            } else {
                out.extend_from_slice(format!("{}:{}", v / 10, v % 10).as_bytes());
            }
            if i + 1 < s.len() {
                out.push(b';');
            }
        }
        out.push(b'm');
    }
    let reset = s.first() == Some(&0);
    add_code(
        out,
        reset,
        &colour_values(cur.fg, 30, 38),
        &colour_values(last.fg, 30, 38),
    );
    add_code(
        out,
        reset,
        &colour_values(cur.bg, 40, 48),
        &colour_values(last.bg, 40, 48),
    );
    add_code(
        out,
        reset,
        &underline_values(cur.underline_color),
        &underline_values(last.underline_color),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Ghostty point lookups by [`read_screen_row`] on this thread.
        pub(super) static LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A row costs one point lookup, not one per cell: a lookup walks
    /// Ghostty's page list, so per cell a long history made every row
    /// (view mode's sync, a capture) cost its length times the width.
    #[test]
    fn a_row_is_one_lookup() {
        let mut t = PaneTerm::new(80, 3, 100);
        t.feed("\x1b[1mab\x1b[m\u{e9}\r\nc\r\nd\r\ne".as_bytes());
        let before = LOOKUPS.with(std::cell::Cell::get);
        let rows: Vec<String> = (0..t.history_size() + 3).map(|y| t.row(y).text()).collect();
        assert_eq!(LOOKUPS.with(std::cell::Cell::get) - before, rows.len());
        assert_eq!(rows[0].trim_end(), "ab\u{e9}");
        assert_eq!(rows[3].trim_end(), "e");
    }

    /// format.c:7200-7218: a cell's OSC 8 URI, from history too and from
    /// the padding of a wide character; none outside the link, none from
    /// a copy for history the alternate screen hides.
    #[test]
    fn hyperlink_of_a_cell() {
        let mut t = PaneTerm::new(20, 2, 100);
        t.feed("\x1b]8;;http://a/\x1b\\L\u{4e2d}\x1b]8;;\x1b\\ x\r\n\r\n\r\n".as_bytes());
        assert_eq!(t.history_size(), 2);
        assert_eq!(t.hyperlink(0, 0).as_deref(), Some("http://a/"));
        assert_eq!(t.hyperlink(2, 0).as_deref(), Some("http://a/"));
        assert_eq!(t.hyperlink(4, 0), None);
        assert_eq!(t.hyperlink(15, 0), None);
        let copy = t.copy_screen();
        assert_eq!(copy.hyperlink(1, 0).as_deref(), Some("http://a/"));
        t.feed(b"\x1b[?1049h");
        assert_eq!(t.hyperlink(0, 0), None);
        assert_eq!(t.copy_screen().hyperlink(0, 0), None);
    }

    fn cap(cols: u16, rows: u16, bytes: &[u8], opts: CaptureOpts) -> String {
        let mut t = PaneTerm::new(cols, rows, 100);
        t.feed(bytes);
        String::from_utf8(capture(&t, opts)).unwrap()
    }

    #[test]
    fn plain_trims_and_pads_blank_rows() {
        assert_eq!(
            cap(20, 3, b"hi  \r\nthere", CaptureOpts::default()),
            "hi\nthere\n\n"
        );
    }

    #[test]
    fn history_ranges() {
        let mut t = PaneTerm::new(10, 3, 100);
        for i in 0..6 {
            t.feed(format!("l{i}\r\n").as_bytes());
        }
        // 7 rows used: l0..l5 plus cursor row; hsize = 4 (l0-l3), visible l4 l5 "".
        let all = CaptureOpts {
            start: LineRef::Start,
            end: LineRef::End,
            ..Default::default()
        };
        assert_eq!(
            String::from_utf8(capture(&t, all)).unwrap(),
            "l0\nl1\nl2\nl3\nl4\nl5\n\n"
        );
        let hist = CaptureOpts {
            start: LineRef::Line(-2),
            end: LineRef::Line(-1),
            ..Default::default()
        };
        assert_eq!(String::from_utf8(capture(&t, hist)).unwrap(), "l2\nl3\n");
        let swapped = CaptureOpts {
            start: LineRef::Line(-1),
            end: LineRef::Line(-2),
            ..Default::default()
        };
        assert_eq!(String::from_utf8(capture(&t, swapped)).unwrap(), "l2\nl3\n");
        let clamp = CaptureOpts {
            start: LineRef::Line(-99),
            end: LineRef::Line(-3),
            ..Default::default()
        };
        assert_eq!(String::from_utf8(capture(&t, clamp)).unwrap(), "l0\nl1\n");
    }

    /// History and visible rows through one index, wide characters with a
    /// padding cell, the wrap flag, and tmux's line length and limit.
    #[test]
    fn rows_cover_history_and_screen() {
        let mut t = PaneTerm::new(6, 3, 100);
        t.feed("l0\r\nl1\r\nab\u{4e16}  \r\nabcdefgh".as_bytes());
        // l0 l1 history; visible "ab世", "abcdef" (wrapped), "gh".
        assert_eq!(t.history_size(), 2);
        assert_eq!(t.row(0).text(), "l0");
        let wide = t.row(2);
        assert_eq!(wide.cells.len(), 6);
        assert_eq!((wide.cells[2].width, wide.cells[3].width), (2, 0));
        assert_eq!((wide.length(), wide.limit()), (4, 2));
        assert_eq!(wide.text(), "ab\u{4e16}");
        assert!(t.row(3).wrapped);
        assert!(!t.row(4).wrapped);
        assert_eq!(t.row(4).text(), "gh");
        assert_eq!(t.row(5), Row::default());
        // History ids stay with their rows as more scroll in.
        let id = t.history_id(1);
        assert_eq!(t.history_row(id).unwrap().text(), "l1");
        assert!(t.history_time(id) > 0);
        t.feed(b"\r\nx\r\ny");
        assert_eq!(t.history_size(), 4);
        assert_eq!(t.history_row(id).unwrap().text(), "l1");
        assert_eq!(t.history_id(1), id);
        assert!(t.history_row(t.history_id(4)).is_none());
        // The primary history stays readable under the alternate screen.
        t.feed(b"\x1b[?1049h");
        assert_eq!(t.row(1).text(), "l1");
        assert_eq!(t.history_row(id).unwrap().text(), "l1");
        let generation = t.history_generation();
        t.feed(b"\x1b[3J");
        assert_ne!(t.history_generation(), generation);
    }

    #[test]
    fn sgr_transitions() {
        let o = CaptureOpts {
            escapes: true,
            ..Default::default()
        };
        assert_eq!(
            cap(20, 1, b"\x1b[1;31mA\x1b[39mB\x1b[0mC", o),
            "\x1b[1m\x1b[31mA\x1b[39mB\x1b[0mC\n"
        );
        assert_eq!(
            cap(20, 1, b"\x1b[38;2;1;2;3mA\x1b[48;5;200mB", o),
            "\x1b[38;2;1;2;3mA\x1b[48;5;200mB\x1b[39m\x1b[49m\n"
        );
        assert_eq!(cap(20, 1, b"\x1b[4:3mA", o), "\x1b[4:3mA\x1b[0m\n");
    }
}
