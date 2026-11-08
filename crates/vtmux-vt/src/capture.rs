//! tmux-shaped capture of terminal contents (`capture-pane`).
//!
//! Mirrors `cmd_capture_pane_history` (cmd-capture-pane.c:315-413) and
//! `grid_string_cells` / `grid_string_cells_code` (grid.c:859-1260).

use libghostty_vt::Terminal;
use libghostty_vt::error::Error;
use libghostty_vt::screen::{CellContentTag, CellWide, GridRef, HyperlinkId};
use libghostty_vt::terminal::{Point, PointCoordinate};

use crate::snapshot::{Cell, Color, Style, Underline};
use crate::text::CellText;
use std::rc::Rc;

use crate::term::{HistoryRow, LineMark, PaneTerm};

mod raw;
pub use raw::capture_grid;
use raw::dec_special_source;

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
    /// `-H`: each line's hyperlinks instead of its text.
    pub hyperlinks: bool,
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
            hyperlinks: false,
            start: LineRef::Visible,
            end: LineRef::Visible,
        }
    }
}

/// A grid other than the pane's that `capture-pane -M` reads: a mode's
/// screen (copy mode's backing grid). Its rows carry no hyperlink URIs, so
/// `-e` and `-H` see no links there.
pub trait Lines {
    /// History lines before the screen's (`hsize`).
    fn history_size(&self) -> usize;
    /// Screen lines (`sy`).
    fn screen_rows(&self) -> usize;
    fn line(&self, y: usize) -> &Row;
    /// When line `y` entered the history, 0 if never (`grid_line_time`).
    fn time(&self, y: usize) -> i64;
    /// OSC 133 marked line `y` as a prompt's start, or with `output` as
    /// command output's start.
    fn starts(&self, y: usize, output: bool) -> bool;
}

/// Where a capture reads (cmd-capture-pane.c:265-291).
enum Source<'a> {
    /// The pane's grid, history included.
    Pane(&'a PaneTerm),
    /// `-a`: the primary screen saved under the alternate one, no history,
    /// read `sx` columns wide (the pane's width now).
    Saved(Rc<Vec<FrozenRow>>, usize),
    /// `-M`: a mode's screen, `sx` columns.
    Mode(&'a dyn Lines, usize),
}

/// One line of a capture: its cells, wrap flag, history time and OSC 133
/// prompt/output marks.
struct Line {
    cells: Vec<RowCell>,
    wrapped: bool,
    time: i64,
    prompt: bool,
    output: bool,
}

impl Source<'_> {
    /// `hsize` and `sy`.
    fn size(&self) -> (usize, usize) {
        match self {
            Self::Pane(t) => (t.history_size(), usize::from(t.rows())),
            Self::Saved(rows, _) => (0, rows.len()),
            Self::Mode(l, _) => (l.history_size(), l.screen_rows()),
        }
    }

    fn line(&self, y: usize, marks: &[(usize, LineMark)]) -> Line {
        let marked = |m: LineMark| marks.iter().any(|&(my, mm)| my == y && mm == m);
        let (cells, wrapped, time, prompt, output) = match self {
            Self::Pane(t) => {
                let hsize = t.history_size();
                let (cells, wrapped) = read_row(t, y, hsize, usize::from(t.cols()));
                let time = if y < hsize {
                    t.history_time(t.history_id(y))
                } else {
                    0
                };
                let (p, o) = (marked(LineMark::Prompt), marked(LineMark::Output));
                (cells, wrapped, time, p, o)
            }
            Self::Saved(rows, sx) => {
                let (mut cells, wrapped) = rows.get(y).map(FrozenRow::thaw).unwrap_or_default();
                // grid_string_cells reads the pane's width of a line
                // whatever its own, so a narrower pane cuts it.
                cells.truncate(*sx);
                (cells, wrapped, 0, false, false)
            }
            Self::Mode(l, sx) => {
                let row = l.line(y);
                let mut cells: Vec<RowCell> = Vec::with_capacity(row.cells.len());
                for c in &row.cells {
                    let after_tab = cells.last().is_some_and(|p: &RowCell| p.tab != 0);
                    cells.push(RowCell::from_cell(c, after_tab));
                }
                let blank = RowCell {
                    empty: true,
                    ..RowCell::default()
                };
                cells.resize((*sx).max(cells.len()), blank);
                (
                    cells,
                    row.wrapped,
                    l.time(y),
                    l.starts(y, false),
                    l.starts(y, true),
                )
            }
        };
        Line {
            cells,
            wrapped,
            time,
            prompt,
            output,
        }
    }
}

/// `capture-pane -a` (cmd-capture-pane.c:268-279): the primary screen
/// saved when the alternate screen came up (`saved_grid`), which has no
/// history, as wide as the pane is now (`sx`, cmd-capture-pane.c:269);
/// `None` while no alternate screen is up.
#[must_use]
pub fn capture_alternate(term: &PaneTerm, opts: CaptureOpts) -> Option<Vec<u8>> {
    let sx = usize::from(term.cols());
    Some(capture_from(&Source::Saved(term.saved_rows()?, sx), opts))
}

/// Capture `term` the way `capture-pane` does, returning the buffer before
/// `-p` strips its final newline.
#[must_use]
pub fn capture(term: &PaneTerm, opts: CaptureOpts) -> Vec<u8> {
    capture_from(&Source::Pane(term), opts)
}

/// `capture-pane -M` over a mode's screen, `sx` the pane's width.
#[must_use]
pub fn capture_lines(lines: &dyn Lines, sx: usize, opts: CaptureOpts) -> Vec<u8> {
    capture_from(&Source::Mode(lines, sx), opts)
}

/// `cmd_capture_pane_history` (cmd-capture-pane.c:254-422).
fn capture_from(src: &Source<'_>, opts: CaptureOpts) -> Vec<u8> {
    let (hsize, sy) = src.size();
    let last = (hsize + sy).saturating_sub(1);
    let mut top = resolve(opts.start, hsize, last, true);
    let mut bottom = resolve(opts.end, hsize, last, false);
    if bottom < top {
        std::mem::swap(&mut top, &mut bottom);
    }
    let marks = match src {
        Source::Pane(t) if opts.flags => t.line_marks(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    // tmux threads one `lastgc` through every line of a capture
    // (cmd-capture-pane.c:333), and one list of the links -H has listed.
    let mut last = Gc::default();
    let mut seen: Vec<LinkKey> = Vec::new();
    for y in top..=bottom {
        let line = src.line(y, &marks);
        let mut text = Vec::new();
        if opts.hyperlinks {
            line_links(&mut text, &line.cells, &mut seen);
            if text.is_empty() {
                continue;
            }
        } else {
            emit_row(&mut text, &line.cells, opts, &mut last);
        }
        // cmd-capture-pane.c:363-401: the line's number, history time and
        // flags first.
        if opts.numbers {
            let n = i64::try_from(y).unwrap_or(0) - i64::try_from(hsize).unwrap_or(0);
            out.extend_from_slice(format!("{n} ").as_bytes());
        }
        if opts.times {
            out.extend_from_slice(format!("{} ", line.time).as_bytes());
        }
        if opts.flags {
            out.extend_from_slice(
                row_flags(&line.cells, line.wrapped, line.prompt, line.output).as_bytes(),
            );
        }
        out.extend_from_slice(&text);
        if !opts.join || !line.wrapped {
            out.push(b'\n');
        }
    }
    out
}

/// A hyperlink as tmux tells them apart (`hyperlinks_put`): one URI and
/// explicit id are one link, a link without an id is its own.
type LinkKey = (String, HyperlinkId);

/// `cmd_capture_pane_hyperlinks` (cmd-capture-pane.c:216-252): the URIs
/// of the line's links not listed on an earlier line, space separated.
fn line_links(out: &mut Vec<u8>, cells: &[RowCell], seen: &mut Vec<LinkKey>) {
    let used = cells.iter().rposition(|c| !c.empty).map_or(0, |i| i + 1);
    for c in cells[..used].iter().filter(|c| !c.spacer()) {
        let Some(key) = c.link_key() else { continue };
        if seen.contains(&key) {
            continue;
        }
        if seen.len() == cells.len() {
            break;
        }
        if !out.is_empty() {
            out.push(b' ');
        }
        out.extend_from_slice(key.0.as_bytes());
        seen.push(key);
    }
}

/// tmux's `GRID_LINE_EXTENDED` (grid.c `grid_need_extended_cell`): a cell
/// wider than one column or of more than one byte, an RGB colour, an
/// attribute past the first eight, an underline colour, a link or a tab.
fn row_extended(cells: &[RowCell]) -> bool {
    cells.iter().any(cell_extended)
}

/// [`row_extended`] for one cell: one `grid_extd_entry` in tmux's line.
fn cell_extended(c: &RowCell) -> bool {
    let st = &c.style;
    c.link
        || c.tab != 0
        || c.wide == Width::Double
        || c.data().len() > 1
        || matches!(st.fg, Color::Rgb(..))
        || matches!(st.bg, Color::Rgb(..))
        || st.underline_color != Color::Default
        || st.strikethrough
        || st.overline
        || !matches!(st.underline, Underline::None | Underline::Single)
}

/// `#{history_bytes}` (format.c `format_cb_history_bytes`): tmux's grid
/// memory for the history and the screen. Ghostty's grid has no such
/// allocation, so it is modelled on the rows read back in tmux's shapes:
/// `sizeof(struct grid_line)` (40) per line, `cellsize` entries of 5 bytes
/// ([`tmux_cellsize`]) and one 23-byte extended entry per cell that needs
/// one (grid.c `grid_need_extended_cell`; a tab's first cell, not the
/// padding after it). Like [`tmux_cellsize`] it cannot
/// know a line's earlier allocation, so a line that was written wide and
/// erased is counted at the size its remaining cells need.
#[must_use]
pub fn history_bytes(term: &PaneTerm) -> usize {
    let c = history_counts(term);
    c.lines * GRID_LINE + c.cells * CELL_ENTRY + c.extended * EXTD_ENTRY
}

/// `sizeof(struct grid_line)`, `sizeof(struct grid_cell_entry)` and
/// `sizeof(struct grid_extd_entry)` on tmux's 64-bit builds.
pub const GRID_LINE: usize = 40;
pub const CELL_ENTRY: usize = 5;
pub const EXTD_ENTRY: usize = 23;

/// The counts [`history_bytes`] sums: lines, cell entries and extended
/// entries (`#{history_all_bytes}`, format.c `format_cb_history_all_bytes`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryCounts {
    pub lines: usize,
    pub cells: usize,
    pub extended: usize,
}

/// See [`HistoryCounts`] and [`history_bytes`]'s model.
#[must_use]
pub fn history_counts(term: &PaneTerm) -> HistoryCounts {
    let sx = usize::from(term.cols());
    let hsize = term.history_size();
    let rows = hsize + usize::from(term.rows());
    let mut counts = HistoryCounts {
        lines: rows,
        ..HistoryCounts::default()
    };
    for y in 0..rows {
        let (cells, _) = read_row(term, y, hsize, sx);
        let used = cells
            .iter()
            .rposition(|c| !c.empty || c.padding())
            .map_or(0, |i| i + 1);
        counts.cells += tmux_cellsize(&cells, used).max(used);
        counts.extended += cells
            .iter()
            .filter(|c| !c.spacer() && cell_extended(c))
            .count();
    }
    counts
}

/// `capture-pane -F` (cmd-capture-pane.c:390-410): the line's flags and a
/// space (`X`: [`row_extended`]). vtmux has no dead lines (`D`).
fn row_flags(cells: &[RowCell], wrapped: bool, prompt: bool, output: bool) -> String {
    let mut f = String::new();
    for (on, c) in [
        (cells.iter().any(|c| c.link), 'H'),
        (output, 'O'),
        (prompt, 'P'),
        (wrapped, 'W'),
        (row_extended(cells), 'X'),
    ] {
        if on {
            f.push(c);
        }
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

/// A [`RowCell`]'s link: the URI and OSC 8 id, either of which may be
/// unreadable.
#[derive(Clone, Debug, PartialEq)]
struct LinkTarget {
    uri: Option<String>,
    id: Option<HyperlinkId>,
}

impl LinkTarget {
    /// The link of the cell at `gr`; none if neither part reads.
    fn read(gr: &GridRef<'_>) -> Option<Box<Self>> {
        let target = Self {
            uri: hyperlink_uri(gr),
            id: gr.hyperlink_id().ok(),
        };
        (target.uri.is_some() || target.id.is_some()).then(|| Box::new(target))
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RowCell {
    text: CellText,
    style: Style,
    /// Blank cell with no text (tmux stores a space).
    empty: bool,
    wide: Width,
    acs: bool,
    link: bool,
    /// The hyperlink's URI and OSC 8 id (Ghostty patch 0006), boxed: links
    /// are rare and a row of cells is read for every line a search passes.
    target: Option<Box<LinkTarget>>,
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

    /// A cell that [`RowCell::to_cell`] makes a blank of width 1 with no
    /// colour, link or line drawing: past the last used cell, tmux's
    /// `cellused` does not count it.
    fn unused(&self) -> bool {
        self.tab == 0
            && matches!(self.wide, Width::Narrow | Width::SpacerHead)
            && self.text.is_empty()
            && !self.link
            && !self.acs
            && self.style == Style::default()
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

    /// The cell's text as tmux stores it: a character printed under DEC
    /// Special Graphics is the ASCII byte tmux keeps (it maps it only when
    /// drawing, `tty_acs_get`), where Ghostty stores the glyph.
    pub(crate) fn data(&self) -> std::borrow::Cow<'_, str> {
        if self.acs {
            self.text
                .chars()
                .map(dec_special_source)
                .collect::<String>()
                .into()
        } else {
            self.text.as_str().into()
        }
    }

    fn uri(&self) -> Option<&str> {
        self.target.as_deref()?.uri.as_deref()
    }

    fn link_id(&self) -> Option<&HyperlinkId> {
        self.target.as_deref()?.id.as_ref()
    }

    /// The cell's link as tmux tells links apart.
    fn link_key(&self) -> Option<LinkKey> {
        let target = self.target.as_deref()?;
        Some((target.uri.clone()?, target.id.clone()?))
    }

    /// A [`Row`]'s cell back in capture form (`-M`). Rows keep no URIs. A
    /// width-0 cell is a wide character's padding, or a tab's when the
    /// cell before is a tab or a tab's padding ([`RowCell::to_cell`]).
    fn from_cell(c: &Cell, after_tab: bool) -> Self {
        let (text, wide, tab) = match (c.tab, c.width) {
            (true, n) => (CellText::from('\t'), Width::Narrow, n),
            (false, 0) if after_tab => (CellText::new(), Width::Narrow, TAB_CONTINUATION),
            (false, 0) => (CellText::new(), Width::SpacerTail, 0),
            (false, 2) => (c.text.clone(), Width::Double, 0),
            (false, _) => (c.text.clone(), Width::Narrow, 0),
        };
        Self {
            empty: text.is_empty(),
            text,
            style: c.style,
            wide,
            acs: c.acs,
            link: c.link,
            target: None,
            tab,
        }
    }

    /// The cell as the renderer sees it (as [`crate::snapshot`] reads it).
    fn to_cell(&self) -> Cell {
        // tmux stores a tab as one cell as wide as the tab, then padding.
        let (text, width, tab) = match (self.tab, self.wide) {
            (TAB_CONTINUATION, _) => (CellText::new(), 0, false),
            (n @ 1..=32, _) => (" ".repeat(usize::from(n)).into(), n, true),
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
        let (cells, wrapped) = self.row_cells(y, false);
        Row {
            cells: cells.iter().map(RowCell::to_cell).collect(),
            wrapped,
        }
    }

    /// [`ScreenCopy::row`] without its trailing unused cells (blanks
    /// nothing wrote with a colour, a link or the line-drawing set: tmux's
    /// `cellused`), and no room kept for them.
    #[must_use]
    pub fn used_row(&self, y: usize) -> Row {
        let (cells, wrapped) = self.row_cells(y, true);
        let used = cells.iter().rposition(|c| !c.unused()).map_or(0, |x| x + 1);
        Row {
            cells: cells[..used].iter().map(RowCell::to_cell).collect(),
            wrapped,
        }
    }

    /// The row's cells; with `used_only` they may stop after the last
    /// cell Ghostty has anything in.
    fn row_cells(&self, y: usize, used_only: bool) -> (Vec<RowCell>, bool) {
        if y >= self.hsize + self.rows {
            return (Vec::new(), false);
        }
        match (&self.frozen, &self.term) {
            (Some(frozen), _) if y < self.hsize => {
                frozen.get(y).map(FrozenRow::thaw).unwrap_or_default()
            }
            (_, Some(term)) => {
                read_screen_cells(term, y + self.retained - self.hsize, self.cols, used_only)
            }
            (_, None) => (Vec::new(), false),
        }
    }

    /// Row `y` as the text a search reads (`window_copy_stringify`: a
    /// space for a blank, one byte for each cell), appended to `out`
    /// (cleared first), and whether the row wraps onto the next. `None`
    /// when a cell is anything but a narrow ASCII character or a blank (a
    /// wide, grapheme, tab or line-drawing cell, or any other character),
    /// or the row cannot be read from the copy; the row then has to be
    /// read with [`ScreenCopy::used_row`]. It costs one page-list walk and
    /// one call for the cells, and builds no cell structures: a search
    /// over a long history reads every line.
    pub fn ascii_text(&self, y: usize, out: &mut String) -> Option<bool> {
        out.clear();
        if y >= self.hsize + self.rows || (self.frozen.is_some() && y < self.hsize) {
            return None;
        }
        let term = self.term.as_ref()?;
        let row_ref = row_ref_at(term, y + self.retained - self.hsize)?;
        let wrapped = row_ref.row().ok()?.is_wrapped().ok()?;
        let mut raw = Vec::new();
        row_ref.row_cells(&mut raw).ok()?;
        // An incomplete reflow can leave the page narrower than the screen.
        raw.truncate(self.cols);
        out.reserve(self.cols);
        for cell in &raw {
            out.push(CellBits(cell.bits()).ascii()?);
        }
        // A cell past the end of the row is a blank.
        out.extend(std::iter::repeat_n(
            ' ',
            self.cols.saturating_sub(raw.len()),
        ));
        Some(wrapped)
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

    /// The copy's width.
    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
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

    /// This row as [`crate::TermState`] carries it.
    pub(crate) fn to_state(&self) -> crate::FrozenRowState {
        crate::FrozenRowState {
            cells: self
                .cells
                .iter()
                .map(|c| crate::FrozenCell {
                    text: c.text.as_str().to_owned(),
                    style: c.style,
                    empty: c.empty,
                    wide: match c.wide {
                        Width::Narrow => 0,
                        Width::Double => 1,
                        Width::SpacerTail => 2,
                        Width::SpacerHead => 3,
                    },
                    acs: c.acs,
                    link: c.link,
                    target: c
                        .target
                        .as_ref()
                        .map(|t| (t.uri.clone(), t.id.as_ref().map(crate::FrozenLinkId::from))),
                    tab: c.tab,
                })
                .collect(),
            len: self.len,
            wrapped: self.wrapped,
        }
    }

    /// The row [`FrozenRow::to_state`] gave.
    pub(crate) fn from_state(state: &crate::FrozenRowState) -> Self {
        Self {
            cells: state
                .cells
                .iter()
                .map(|c| RowCell {
                    text: CellText::from(c.text.as_str()),
                    style: c.style,
                    empty: c.empty,
                    wide: match c.wide {
                        1 => Width::Double,
                        2 => Width::SpacerTail,
                        3 => Width::SpacerHead,
                        _ => Width::Narrow,
                    },
                    acs: c.acs,
                    link: c.link,
                    target: c.target.as_ref().map(|(uri, id)| {
                        Box::new(LinkTarget {
                            uri: uri.clone(),
                            id: id.as_ref().map(HyperlinkId::from),
                        })
                    }),
                    tab: c.tab,
                })
                .collect(),
            len: state.len,
            wrapped: state.wrapped,
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

/// The reference to row `y` of a screen (history first, `y` counted from
/// the oldest row Ghostty keeps). Ghostty resolves a screen point by
/// walking its pages from the oldest and an active point from the newest:
/// a row of the active area is read as one, at a cost that does not grow
/// with the history.
fn row_ref_at<'t>(term: &'t Terminal<'_, '_>, y: usize) -> Option<GridRef<'t>> {
    let (at, y): (fn(PointCoordinate) -> Point, usize) =
        match y.checked_sub(term.scrollback_rows().unwrap_or(0)) {
            Some(active) => (Point::Active, active),
            None => (Point::Screen, y),
        };
    #[cfg(test)]
    tests::LOOKUPS.with(|n| n.set(n.get() + 1));
    term.grid_ref(at(PointCoordinate {
        x: 0,
        y: u32::try_from(y).unwrap_or(u32::MAX),
    }))
    .ok()
}

/// Read one row of the active screen, spacer cells included: one page-list
/// walk for the row and one call for its cells; a cell's style and
/// grapheme are looked up only when it has them.
fn read_screen_row(term: &Terminal<'_, '_>, y: usize, cols: usize) -> (Vec<RowCell>, bool) {
    read_screen_cells(term, y, cols, false)
}

/// [`read_screen_row`]; with `used_only`, the cells Ghostty has nothing in
/// after the last it has something in are not read (they are blanks, and
/// a tab is judged by the whole row's width all the same).
fn read_screen_cells(
    term: &Terminal<'_, '_>,
    y: usize,
    cols: usize,
    used_only: bool,
) -> (Vec<RowCell>, bool) {
    let Some(row_ref) = row_ref_at(term, y) else {
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
    let width = raw.len();
    if used_only {
        raw.truncate(raw.iter().rposition(|c| c.bits() != 0).map_or(0, |x| x + 1));
    }
    let mut cells = Vec::with_capacity(raw.len());
    for (x, &cell) in raw.iter().enumerate() {
        let cell = CellBits(cell.bits());
        if cell.0 == 0 {
            // Most of a row: a blank nothing wrote.
            cells.push(RowCell {
                empty: true,
                ..RowCell::default()
            });
            continue;
        }
        let gr = row_ref.with_x(u16::try_from(x).unwrap_or(u16::MAX));
        let wide = match cell.wide() {
            CellWide::Wide => Width::Double,
            CellWide::SpacerTail => Width::SpacerTail,
            CellWide::SpacerHead => Width::SpacerHead,
            CellWide::Narrow => Width::Narrow,
        };
        // Most cells are unstyled: skip the style lookup for them.
        let mut style = if styled && cell.has_styling() {
            gr.style().map(Style::from).unwrap_or_default()
        } else {
            Style::default()
        };
        let tab = cell.tab();
        let text = match cell.content_tag() {
            // One codepoint: no grapheme buffer needed.
            CellContentTag::Codepoint => char::from_u32(cell.codepoint())
                .filter(|&c| c != '\0')
                .map(CellText::from)
                .unwrap_or_default(),
            CellContentTag::CodepointGrapheme => cell_text(&gr).into(),
            CellContentTag::BgColorPalette => {
                style.bg = Color::Palette(cell.palette()).in_form(cell.palette_256());
                CellText::new()
            }
            CellContentTag::BgColorRgb => {
                let (r, g, b) = cell.rgb();
                style.bg = Color::Rgb(r, g, b);
                CellText::new()
            }
        };
        // A tab's first cell is a TAB cell: `\t` in a capture (grid.c
        // `grid_string_cells`), spaces everywhere else (`to_cell`).
        let text = if matches!(tab, 1..=32) {
            CellText::from('\t')
        } else {
            text
        };
        let link = linked && cell.has_hyperlink();
        cells.push(RowCell {
            empty: text.is_empty(),
            text,
            style,
            wide,
            acs: cell.is_protected(),
            link,
            // Links are rare: their URI and id are read as the cell is.
            target: if link { LinkTarget::read(&gr) } else { None },
            tab,
        });
    }
    whole_tabs(&mut cells, width, wrapped);
    (cells, wrapped)
}

/// A Ghostty cell read from its bits rather than a `ghostty_cell_get` call
/// per field. `page.zig`'s `Cell` is a `packed struct(u64)`, fields from
/// the least significant bit: `content_tag` (2 bits), `content` (24: a
/// codepoint in the low 21, a palette index in the low 8, or r, g, b),
/// `style_id` (16), `wide` (2), `protected`, `hyperlink`,
/// `semantic_content` (2), `tab` (8, patch 0002); a palette cell's index
/// is followed by its `palette_256` bit (patch 0010). The test
/// `cell_bits_match_the_getters` holds it to the getters.
#[derive(Clone, Copy)]
struct CellBits(u64);

impl CellBits {
    fn field(self, shift: u32, bits: u32) -> u32 {
        u32::try_from((self.0 >> shift) & ((1 << bits) - 1)).expect("masked to 24 bits")
    }

    fn content_tag(self) -> CellContentTag {
        match self.field(0, 2) {
            0 => CellContentTag::Codepoint,
            1 => CellContentTag::CodepointGrapheme,
            2 => CellContentTag::BgColorPalette,
            _ => CellContentTag::BgColorRgb,
        }
    }

    /// `Cell.codepoint()`: 0 for a colour-only cell.
    fn codepoint(self) -> u32 {
        match self.content_tag() {
            CellContentTag::Codepoint | CellContentTag::CodepointGrapheme => self.field(2, 21),
            _ => 0,
        }
    }

    fn palette(self) -> u8 {
        self.byte(2)
    }

    /// The `palette_256` bit after the palette index (patch 0010).
    fn palette_256(self) -> bool {
        self.field(10, 1) != 0
    }

    fn rgb(self) -> (u8, u8, u8) {
        (self.byte(2), self.byte(10), self.byte(18))
    }

    fn byte(self, shift: u32) -> u8 {
        u8::try_from(self.field(shift, 8)).expect("masked to 8 bits")
    }

    fn has_styling(self) -> bool {
        self.field(26, 16) != 0
    }

    fn wide(self) -> CellWide {
        match self.field(42, 2) {
            0 => CellWide::Narrow,
            1 => CellWide::Wide,
            2 => CellWide::SpacerTail,
            _ => CellWide::SpacerHead,
        }
    }

    fn is_protected(self) -> bool {
        self.field(44, 1) != 0
    }

    fn has_hyperlink(self) -> bool {
        self.field(45, 1) != 0
    }

    fn tab(self) -> u8 {
        self.byte(48)
    }

    /// The cell as a search sees it when that is one ASCII byte: a blank
    /// (nothing written, or only a colour) is a space. `None` for a wide,
    /// spacer, tab or protected (line-drawing) cell, a grapheme cluster
    /// and any codepoint past ASCII.
    fn ascii(self) -> Option<char> {
        if self.0 == 0 {
            return Some(' ');
        }
        if self.field(42, 2) != 0 || self.tab() != 0 || self.is_protected() {
            return None;
        }
        match self.content_tag() {
            CellContentTag::Codepoint => match self.codepoint() {
                0 => Some(' '),
                c @ 1..=0x7f => char::from_u32(c),
                _ => None,
            },
            CellContentTag::CodepointGrapheme => None,
            CellContentTag::BgColorPalette | CellContentTag::BgColorRgb => Some(' '),
        }
    }
}

/// A tab whose first cell has fewer columns left in the row than it is wide
/// (a resize without reflow cut it) is plain blanks here. A reflow never
/// cuts one: it moves a tab whole or splits it after its first cell as tmux
/// does (patch 0015), and the columns moved to the next row are padding
/// there, as tmux's are. A tab wider than the row starts a wrapped row of
/// its own there and stays a TAB cell, as in tmux (`grid_reflow_split`
/// keeps a first cell that does not fit). A tab tmux itself could hold
/// (`window_copy` over a partly erased tab) stays. `width` is the row's,
/// which `cells` may stop short of.
fn whole_tabs(cells: &mut [RowCell], width: usize, wrapped: bool) {
    let blank = |c: &mut RowCell| {
        c.tab = 0;
        c.text.clear();
        c.empty = true;
    };
    if let Some(x) = cells
        .iter()
        .rposition(|c| c.tab != 0 && c.tab != TAB_CONTINUATION)
        && x + usize::from(cells[x].tab) > width
        && !(x == 0 && wrapped)
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

/// What `grid_string_cells_code` compares between cells (its `lastgc`).
#[derive(Clone, Default, PartialEq)]
struct Gc {
    style: Style,
    acs: bool,
    link: Option<LinkKey>,
}

fn emit_row(out: &mut Vec<u8>, cells: &[RowCell], opts: CaptureOpts, last: &mut Gc) {
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
    let mut has_link = false;
    // The last cell's sequences (`code`), which the closing OSC 8 is
    // appended to.
    let (mut code_at, mut code_len) = (0, 0);
    // Spacer cells are tmux's PADDING cells: skipped (grid.c:1214-1215).
    // But the column a wide character that did not fit left at the line's
    // end is no padding in tmux: screen_write_cell wraps without writing
    // it, so it is the default blank grid_expand_line left.
    let blank = RowCell {
        empty: true,
        ..RowCell::default()
    };
    for c in cells[..end.min(cells.len())]
        .iter()
        .filter(|c| !c.spacer() || c.wide == Width::SpacerHead)
    {
        let c = if c.wide == Width::SpacerHead {
            &blank
        } else {
            c
        };
        if opts.escapes {
            let at = line.len();
            code_at = at;
            let gc = Gc {
                style: c.style,
                acs: c.acs,
                link: c.link_key(),
            };
            sgr_code(&mut line, &last.style, &gc.style);
            if opts.octal {
                escape_esc(&mut line, at);
            }
            // SO/SI on the charset (a leading 0 keeps it).
            if gc.acs != last.acs {
                line.extend_from_slice(match (gc.acs, opts.octal) {
                    (true, false) => b"\x0e",
                    (true, true) => b"\\016",
                    (false, false) => b"\x0f",
                    (false, true) => b"\\017",
                });
            }
            if gc.link != last.link {
                if let Some((uri, id)) = &gc.link {
                    let id: &[u8] = match id {
                        HyperlinkId::Explicit(id) => id,
                        _ => b"",
                    };
                    has_link = add_hyperlink(&mut line, id, uri.as_bytes(), opts.octal);
                } else if has_link {
                    add_hyperlink(&mut line, b"", b"", opts.octal);
                    has_link = false;
                }
            }
            *last = gc;
            code_len = line.len() - code_at;
        }
        if c.empty {
            line.push(b' ');
        } else if opts.octal && c.text == "\\" {
            line.extend_from_slice(b"\\\\");
        } else {
            line.extend_from_slice(c.data().as_bytes());
        }
    }
    if has_link {
        // grid.c:1255-1258 appends the close to `code` with strlcat, which
        // still holds the last cell's sequences: they come out again.
        let code = line[code_at..code_at + code_len].to_vec();
        line.extend_from_slice(&code);
        add_hyperlink(&mut line, b"", b"", opts.octal);
    }
    if !opts.join && !opts.keep_spaces {
        // GRID_STRING_TRIM_SPACES (grid.c:1263).
        while line.last() == Some(&b' ') {
            line.pop();
        }
    }
    out.extend_from_slice(&line);
}

/// `grid_string_cells_add_hyperlink` (grid.c:1035-1060): OSC 8 with `id=`
/// when there is one; ST is `ESC \` (`\033\\` under `-C`).
fn add_hyperlink(out: &mut Vec<u8>, id: &[u8], uri: &[u8], octal: bool) -> bool {
    if uri.len() + id.len() + 17 >= 8192 {
        return false;
    }
    let esc: &[u8] = if octal { b"\\033" } else { b"\x1b" };
    out.extend_from_slice(esc);
    out.extend_from_slice(b"]8;");
    if !id.is_empty() {
        out.extend_from_slice(b"id=");
        out.extend_from_slice(id);
    }
    out.push(b';');
    out.extend_from_slice(uri);
    out.extend_from_slice(esc);
    out.extend_from_slice(if octal { b"\\\\" } else { b"\\" });
    true
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
            b.spacer() || (a.style == b.style && a.acs == b.acs && a.target == b.target)
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
/// `31`, `91` and `38;5;1` stay apart as tmux's `COLOUR_FLAG_256` keeps
/// them ([`Color::Palette256`]).
fn colour_values(c: Color, base: i32, ext: i32) -> Vec<i32> {
    match c {
        Color::Default => vec![base + 9],
        Color::Palette(i) if i < 8 => vec![base + i32::from(i)],
        Color::Palette(i) if i < 16 => vec![base + 60 + i32::from(i) - 8],
        Color::Palette(i) | Color::Palette256(i) => vec![ext, 5, i32::from(i)],
        Color::Rgb(r, g, b) => vec![ext, 2, i32::from(r), i32::from(g), i32::from(b)],
    }
}

/// `grid_string_cells_us` (grid.c:971-1000): nothing for the default.
fn underline_values(c: Color) -> Vec<i32> {
    match c {
        Color::Default => vec![],
        Color::Palette(i) | Color::Palette256(i) => vec![58, 5, i32::from(i)],
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

    /// The column a wide character that did not fit left at the end of a
    /// line is a default blank inside tmux's `cellsize` (its line grew to
    /// the whole width): `-N` keeps it, `-e` resets the pen on it, whatever
    /// the pen was. Expected: tmux 3.8.
    #[test]
    fn a_wrapped_wide_character_leaves_a_default_blank() {
        let cap = |pen: &str, opts: CaptureOpts| {
            let mut t = PaneTerm::new(11, 3, 0);
            t.feed(format!("\x1b[{pen}mabcdefghij\u{6f22}\x1b[m").as_bytes());
            String::from_utf8_lossy(&capture(&t, opts)).into_owned()
        };
        let e = CaptureOpts {
            escapes: true,
            ..Default::default()
        };
        let n = CaptureOpts {
            keep_spaces: true,
            ..Default::default()
        };
        assert!(cap("1", e).starts_with("\x1b[1mabcdefghij\x1b[0m\n\x1b[1m\u{6f22}\x1b[0m\n"));
        assert!(cap("41", e).starts_with("\x1b[41mabcdefghij\x1b[49m\n\x1b[41m\u{6f22}\x1b[49m\n"));
        let plain = cap("1", n);
        assert!(plain.starts_with("abcdefghij \n\u{6f22}   \n"), "{plain:?}");
        let j = CaptureOpts {
            escapes: true,
            join: true,
            ..Default::default()
        };
        let joined = cap("1", j);
        assert!(
            joined.starts_with("\x1b[1mabcdefghij\u{6f22}\n"),
            "{joined:?}"
        );
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

    /// [`CellBits`] reads every field the way the getters do, over cells
    /// with each kind of content: text, a cluster, palette and RGB
    /// colour-only blanks, styles, wide characters and their spacers (head
    /// and tail), DECSCA protection, a hyperlink, tabs.
    #[test]
    fn cell_bits_match_the_getters() {
        let mut t = PaneTerm::new(8, 3, 100);
        t.feed(
            "a\x1b[1;31mb\x1b[m\u{e9}e\u{301}\x1b[44m\x1b[K\x1b[m\r\n\x1b[48;2;1;2;3m\x1b[K\x1b[mab\r\nabcdefg\u{4e2d}\r\n\
             \x1b(0q\x1b(B\x1b]8;;http://a/\x1b\\L\x1b]8;;\x1b\\\tx\r\n\x1b[48;5;200m\x1b[2K\x1b[m\u{1f600}"
                .as_bytes(),
        );
        let term = &t.term;
        let rows = term.scrollback_rows().unwrap_or(0) + 3;
        let mut seen = 0;
        for y in 0..rows {
            let row_ref = term
                .grid_ref(Point::Screen(PointCoordinate {
                    x: 0,
                    y: u32::try_from(y).unwrap(),
                }))
                .unwrap();
            let mut raw = Vec::new();
            row_ref.row_cells(&mut raw).unwrap();
            for cell in raw {
                let bits = CellBits(cell.bits());
                let at = format!("row {y} {:#x}", cell.bits());
                let tag = cell.content_tag().unwrap();
                assert_eq!(bits.content_tag(), tag, "{at}");
                assert_eq!(bits.codepoint(), cell.codepoint().unwrap(), "{at}");
                if tag == CellContentTag::BgColorPalette {
                    assert_eq!(bits.palette(), cell.bg_color_palette().unwrap().0, "{at}");
                    let long = cell.bg_color_palette_256().unwrap();
                    assert_eq!(bits.palette_256(), long, "{at}");
                    seen |= 1 << (12 + u32::from(long));
                }
                if tag == CellContentTag::BgColorRgb {
                    let c = cell.bg_color_rgb().unwrap();
                    assert_eq!(bits.rgb(), (c.r, c.g, c.b), "{at}");
                }
                assert_eq!(bits.has_styling(), cell.has_styling().unwrap(), "{at}");
                assert_eq!(bits.wide(), cell.wide().unwrap(), "{at}");
                assert_eq!(bits.is_protected(), cell.is_protected().unwrap(), "{at}");
                assert_eq!(bits.has_hyperlink(), cell.has_hyperlink().unwrap(), "{at}");
                assert_eq!(bits.tab(), cell.tab().unwrap(), "{at}");
                seen |= 1 << (tag as u32);
                seen |= 1 << (4 + bits.wide() as u32);
                seen |= u32::from(bits.is_protected()) << 8;
                seen |= u32::from(bits.has_hyperlink()) << 9;
                seen |= u32::from(bits.tab() != 0) << 10;
                seen |= u32::from(bits.has_styling()) << 11;
            }
        }
        assert_eq!(seen, 0x3fff, "every kind of cell was read: {seen:#b}");
    }

    /// `used_row` is `row` with the trailing cells copy mode drops (blank,
    /// width 1, no colour, link or line drawing) cut off, whatever ends
    /// the row: spaces, a coloured erase, a tab (whole or partly erased),
    /// the blank a wrapped wide character leaves, line drawing, a link.
    #[test]
    fn used_row_is_row_without_its_unused_tail() {
        let mut t = PaneTerm::new(6, 2, 100);
        t.feed(
            "ab  \r\nx\x1b[44m\x1b[K\x1b[m\r\na\tb\r\nq\t\r\n\t\x1b[3G\x1b[K\r\nabcde\u{4e2d}\r\n\x1b(0q\x1b(B\r\n\x1b]8;;http://a/\x1b\\L\x1b]8;;\x1b\\\r\n\r\nz"
                .as_bytes(),
        );
        let copy = t.copy_screen();
        for y in 0..copy.history_size() + 2 {
            let mut want = copy.row(y);
            while want.cells.last().is_some_and(|c| {
                c.text.is_empty()
                    && c.width == 1
                    && !c.link
                    && !c.acs
                    && c.style == Style::default()
            }) {
                want.cells.pop();
            }
            let got = copy.used_row(y);
            assert_eq!(got, want, "row {y}");
            assert_eq!(got.cells.capacity(), got.cells.len(), "row {y}");
        }
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
