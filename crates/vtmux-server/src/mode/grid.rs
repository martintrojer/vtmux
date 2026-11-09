//! Copy mode's own copy of a pane's grid (`data->backing`).
//!
//! Taken on entry (`window_copy_clone_screen`, window-copy.c:392-449), so
//! output the pane writes afterwards does not show and rows the pane drops
//! from its history stay. Resized with the mode as tmux resizes the copy
//! (`screen_resize_cursor` with reflow, screen.c:354-401): a change of
//! height moves the history boundary, a change of width reflows the lines
//! (`grid_reflow`, grid.c:1318-1611).

use std::cell::{OnceCell, RefCell};
use std::collections::VecDeque;
use std::ops::Deref;
use std::rc::Rc;

use vtmux_vt::{LineMark, PaneTerm, Row, ScreenCopy};

/// `capture-pane -M` reads the copy (`window_copy_get_screen` is
/// `data->backing`).
impl vtmux_vt::Lines for Grid {
    fn history_size(&self) -> usize {
        self.hsize
    }

    fn screen_rows(&self) -> usize {
        self.sy
    }

    fn line(&self, y: usize) -> &Row {
        self.row(y)
    }

    fn time(&self, y: usize) -> i64 {
        Grid::time(self, y)
    }

    fn starts(&self, y: usize, output: bool) -> bool {
        Grid::starts(self, y, output)
    }

    /// A line still unread is the entry copy's row, while the copy is as
    /// wide as the grid (as [`Grid::text`] reads it): a capture of
    /// all of history then keeps no row for each line.
    fn entry_row(&self, y: usize) -> Option<(&ScreenCopy, usize)> {
        let line = self.lines.get(y)?;
        let (src, source) = (line.src?, self.source.as_deref()?);
        (line.row.get().is_none() && source.cols() == self.sx).then_some((source, src))
    }
}

/// One line of the copy.
#[derive(Clone, Debug, Default)]
struct Line {
    /// The used cells (`cellused`): trailing blanks nothing wrote with a
    /// colour or a link are dropped, so `row.cells.len()` is the count.
    /// Read from the source on first use.
    row: OnceCell<Row>,
    /// The source row `row` comes from while it is unread.
    src: Option<usize>,
    /// When it scrolled into history, 0 if never (`grid_line_time`).
    time: i64,
    /// `GRID_LINE_START_PROMPT` and `GRID_LINE_START_OUTPUT`: OSC 133
    /// marked the line. Reflow keeps them on a line's first part.
    prompt: bool,
    output: bool,
    /// The hyperlinks of a line restored from a snapshot (D27), by column:
    /// it has no entry copy to ask.
    links: Box<[(usize, String)]>,
}

impl Line {
    /// The row of a line already read (all of them once a reflow starts).
    fn row(&self) -> &Row {
        self.row.get().expect("line read")
    }

    fn row_mut(&mut self) -> &mut Row {
        if self.row.get().is_none() {
            let _ = self.row.set(Row::default());
        }
        self.row.get_mut().expect("line read")
    }
}

/// A cursor position counted along unwrapped lines (`grid_wrap_position`):
/// `x` is `None` at or past the end of the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapPos {
    x: Option<usize>,
    y: usize,
}

/// A line as [`Grid::peek`] gives it.
pub enum RowRef<'a> {
    /// The line's own row, read before.
    Kept(&'a Row),
    /// Read from the entry copy for this look only.
    Read(Rc<Row>),
}

impl Deref for RowRef<'_> {
    type Target = Row;

    fn deref(&self) -> &Row {
        match self {
            Self::Kept(row) => row,
            Self::Read(row) => row,
        }
    }
}

/// Lines [`Grid::peek`] keeps read without keeping them in the copy: a
/// search looks at a line and the few it wraps onto several times over.
const PEEKED: usize = 4;

/// The copy: history first, then the screen rows.
#[derive(Clone, Debug)]
pub struct Grid {
    /// `hsize + sy` lines.
    lines: Vec<Line>,
    /// The pane's grid as it was on entry, which unread lines come from.
    source: Option<Rc<ScreenCopy>>,
    hsize: usize,
    /// History lines that scrolled there and were not cleared
    /// (`hscrolled`): a taller screen pulls these back first.
    hscrolled: usize,
    sx: usize,
    sy: usize,
    /// The last lines [`Grid::peek`] read from `source` (by line number;
    /// emptied when the lines move).
    peeked: RefCell<VecDeque<(usize, Rc<Row>)>>,
}

static EMPTY: Row = Row {
    cells: Vec::new(),
    wrapped: false,
};

/// Drop the trailing cells nothing wrote (Ghostty's empty cells with the
/// default style), keeping written spaces and coloured blanks as tmux's
/// `cellused` does. The row keeps no room for the dropped cells: a copy
/// whose every line is read (a search, a reflow) holds only what tmux's
/// grid holds, not a pane width of cells per line.
fn used_cells(mut row: Row) -> Row {
    while row.cells.last().is_some_and(|c| {
        c.text.is_empty()
            && c.width == 1
            && c.link.is_none()
            && !c.acs
            && c.style == vtmux_vt::Style::default()
    }) {
        row.cells.pop();
    }
    row.cells.shrink_to_fit();
    row
}

/// Line `y` of `lines` if it is at or after `from`.
fn lines_at(lines: &mut [Line], y: usize, from: usize) -> Option<&mut Line> {
    if y < from { None } else { lines.get_mut(y) }
}

impl Grid {
    /// `window_copy_clone_screen`: a copy of `src` for a view `sx` x `sy`
    /// and the cursor in it (column, line). `trim` drops trailing empty
    /// lines (`copy-mode -s`).
    #[must_use]
    pub fn clone_of(src: &PaneTerm, sx: usize, sy: usize, trim: bool) -> (Self, usize, usize) {
        let copy = src.copy_screen();
        let hsize = copy.history_size();
        let mut n = hsize + usize::from(src.rows());
        let mut tail = Vec::new();
        if trim {
            loop {
                let row = used_cells(copy.row(n - 1));
                if n <= hsize + 1 || !row.cells.is_empty() {
                    tail.push(row);
                    break;
                }
                n -= 1;
            }
        }
        let mut lines: Vec<Line> = (0..n)
            .map(|y| Line {
                src: Some(y),
                time: copy.history_time(y),
                ..Line::default()
            })
            .collect();
        if let (Some(row), Some(line)) = (tail.pop(), lines.last_mut()) {
            line.row = OnceCell::from(row);
        }
        for &(y, mark) in copy.line_marks() {
            if let Some(line) = lines.get_mut(y) {
                match mark {
                    LineMark::Prompt => line.prompt = true,
                    LineMark::Output => line.output = true,
                }
            }
        }
        let gsy = n - hsize;
        let (scx, scy) = src.cursor();
        let (cx, cy) = if usize::from(scy) > gsy - 1 {
            (0, gsy - 1)
        } else {
            (usize::from(scx), usize::from(scy))
        };
        let mut grid = Self {
            lines,
            source: Some(Rc::new(copy)),
            hsize,
            hscrolled: hsize,
            sx: usize::from(src.cols()),
            sy: gsy,
            peeked: RefCell::default(),
        };
        let (cx, cy) = grid.resize_keeping(sx, sy, cx, hsize + cy);
        (grid, cx, cy)
    }

    /// The lines of `src` from line `from` on, read again over this copy's
    /// own (view mode's `window_copy_vadd`, which writes into the copy):
    /// `src` is the terminal the copy was taken from, at the copy's size,
    /// and only scrolled since, so the lines before `from` (above the
    /// cursor where it wrote) are as they were.
    pub fn extend_from(&mut self, src: &PaneTerm, from: usize) {
        self.peeked.get_mut().clear();
        let hsize = src.history_size();
        let n = hsize + usize::from(src.rows());
        let from = from.min(self.lines.len()).min(n);
        self.lines.truncate(from);
        #[cfg(test)]
        tests::ROWS_READ.with(|r| r.set(r.get() + (n - from)));
        // Each row stepped to from the one before (`PaneTerm::rows_in`):
        // `from` can be the top of a long history.
        self.lines
            .extend((from..n).zip(src.rows_in(from..n)).map(|(y, row)| Line {
                row: OnceCell::from(used_cells(row)),
                src: None,
                time: if y < hsize {
                    src.history_time(src.history_id(y))
                } else {
                    0
                },
                prompt: false,
                output: false,
                links: Box::default(),
            }));
        for (y, mark) in src.line_marks() {
            if let Some(line) = lines_at(&mut self.lines, y, from) {
                match mark {
                    LineMark::Prompt => line.prompt = true,
                    LineMark::Output => line.output = true,
                }
            }
        }
        // grid_scroll_history: each line scrolled is history scrolled.
        self.hscrolled += hsize.saturating_sub(self.hsize);
        self.hsize = hsize;
        self.sy = n - hsize;
    }

    #[must_use]
    pub fn hsize(&self) -> usize {
        self.hsize
    }

    #[must_use]
    pub fn sx(&self) -> usize {
        self.sx
    }

    /// The last line (`hsize + sy - 1`).
    #[must_use]
    pub fn last(&self) -> usize {
        self.hsize + self.sy - 1
    }

    /// Line `y`, empty past the end.
    #[must_use]
    pub fn row(&self, y: usize) -> &Row {
        self.lines.get(y).map_or(&EMPTY, |l| self.line_row(l))
    }

    /// Line `y` as [`Grid::row`] gives it, but a line not read yet is read
    /// without keeping it: a search over all of history (`search_marks`
    /// counts every match) would otherwise keep a row of cells for every
    /// line, several times what tmux's grid holds, and take as long again
    /// to allocate them.
    #[must_use]
    pub fn peek(&self, y: usize) -> RowRef<'_> {
        let Some(line) = self.lines.get(y) else {
            return RowRef::Kept(&EMPTY);
        };
        if let Some(row) = line.row.get() {
            return RowRef::Kept(row);
        }
        let (Some(src), Some(source)) = (line.src, &self.source) else {
            return RowRef::Kept(&EMPTY);
        };
        let mut peeked = self.peeked.borrow_mut();
        if let Some((_, row)) = peeked.iter().rev().find(|(py, _)| *py == y) {
            return RowRef::Read(Rc::clone(row));
        }
        #[cfg(test)]
        tests::ROWS_PEEKED.with(|r| r.set(r.get() + 1));
        let row = Rc::new(source.used_row(src));
        if peeked.len() == PEEKED {
            peeked.pop_front();
        }
        peeked.push_back((y, Rc::clone(&row)));
        RowRef::Read(row)
    }

    /// Line `y` as the text a search reads, a character for each of the
    /// `sx` cells (ASCII exactly when it is `sx` bytes long), in `out`;
    /// whether it wraps onto the next line. `None` unless the line is
    /// still the entry copy's, unread, and made of characters of one
    /// column and blanks only, of any style ([`ScreenCopy::text`]): the
    /// line then has to be read as a [`Row`] with [`Grid::peek`].
    pub fn text(&self, y: usize, out: &mut String) -> Option<bool> {
        let line = self.lines.get(y)?;
        let (src, source) = (line.src?, self.source.as_ref()?);
        if line.row.get().is_some() || source.cols() != self.sx {
            return None;
        }
        source.text(src, out)
    }

    /// The hyperlink of the cell at `(x, y)`: from the pane's grid as it
    /// was on entry, while the line is still that grid's (a reflowed line
    /// has none).
    #[must_use]
    pub fn hyperlink(&self, x: usize, y: usize) -> Option<String> {
        let line = self.lines.get(y)?;
        if line.src.is_none() {
            return line
                .links
                .iter()
                .find(|(col, _)| *col == x)
                .map(|(_, uri)| uri.clone());
        }
        self.source.as_ref()?.hyperlink(x, line.src?)
    }

    /// The copy as a snapshot carries it (D27): its lines as rows (time,
    /// OSC 133 marks and hyperlinks with them), `hsize` of them history;
    /// `hscrolled`, `sx` and `sy`.
    #[must_use]
    pub fn export(&self) -> (Vec<vtmux_snapshot::Row>, usize, usize, usize, usize) {
        let rows = (0..self.lines.len())
            .map(|y| {
                let line = &self.lines[y];
                let row = self.peek(y);
                let mut out =
                    crate::server::snapshot::rows::row_out(&row, |x| self.hyperlink(x, y));
                out.time = line.time;
                let flags = if line.prompt { 0x8 } else { 0 } | if line.output { 0x40 } else { 0 };
                if flags != 0 {
                    out.mark = Some(vtmux_snapshot::Osc133 {
                        flags,
                        ..vtmux_snapshot::Osc133::default()
                    });
                }
                out
            })
            .collect();
        (rows, self.hsize, self.hscrolled, self.sx, self.sy)
    }

    /// A copy of the rows [`Self::export`] gave, `pane` the terminal it
    /// copies: a link's cells are drawn with the pane's link for its URI
    /// when it has just one, else with a new link per URI (the rows keep
    /// no link's id, D-068).
    #[must_use]
    pub fn import(
        rows: &[vtmux_snapshot::Row],
        (hsize, hscrolled, sx, sy): (usize, usize, usize, usize),
        pane: &PaneTerm,
    ) -> Self {
        let mut known: std::collections::HashMap<String, std::sync::Arc<vtmux_vt::Link>> =
            std::collections::HashMap::new();
        let mut link = |uri: &str| {
            let link = known.entry(uri.to_owned()).or_insert_with(|| {
                pane.drawn_link_of(uri)
                    .unwrap_or_else(|| pane.link_numbers().fresh(uri))
            });
            std::sync::Arc::clone(link)
        };
        let lines = rows
            .iter()
            .map(|r| {
                let (row, links) = crate::server::snapshot::rows::row_in(r, &mut link);
                let flags = r.mark.map_or(0, |m| m.flags);
                Line {
                    row: OnceCell::from(row),
                    src: None,
                    time: r.time,
                    prompt: flags & 0x8 != 0,
                    output: flags & 0x40 != 0,
                    links: links.into_boxed_slice(),
                }
            })
            .collect();
        Self {
            lines,
            source: None,
            hsize: hsize.min(rows.len()),
            hscrolled,
            sx,
            sy,
            peeked: RefCell::default(),
        }
    }

    fn line_row<'a>(&self, line: &'a Line) -> &'a Row {
        line.row.get_or_init(|| match (line.src, &self.source) {
            (Some(y), Some(source)) => source.used_row(y),
            _ => Row::default(),
        })
    }

    fn used(&self, line: &Line) -> usize {
        self.line_row(line).cells.len()
    }

    fn wrapped_line(&self, line: &Line) -> bool {
        self.line_row(line).wrapped
    }

    /// Read every unread line: a reflow rewrites them all.
    fn read_all(&mut self) {
        let mut lines = std::mem::take(&mut self.lines);
        for line in &mut lines {
            self.line_row(line);
            line.src = None;
            // A reflowed line has no hyperlinks, restored or not.
            line.links = Box::default();
        }
        self.lines = lines;
    }

    /// Lines whose row the copy keeps.
    #[cfg(test)]
    pub(in crate::mode) fn kept_lines(&self) -> usize {
        self.lines.iter().filter(|l| l.row.get().is_some()).count()
    }

    /// When line `y` scrolled into history (`grid_line_time`), 0 if never.
    #[must_use]
    pub fn time(&self, y: usize) -> i64 {
        self.lines.get(y).map_or(0, |l| l.time)
    }

    /// Whether OSC 133 marked line `y` as a prompt's start, or with
    /// `output` as command output's start.
    #[must_use]
    pub fn starts(&self, y: usize, output: bool) -> bool {
        self.lines
            .get(y)
            .is_some_and(|l| if output { l.output } else { l.prompt })
    }

    /// Resize to `sx` x `sy`, the cursor `(cx, cy)` (`cy` a line) kept on
    /// the same text when the lines reflow (`window_copy_resize`,
    /// window-copy.c:1212-1243). Returns the cursor's new place.
    pub fn resize_keeping(&mut self, sx: usize, sy: usize, cx: usize, cy: usize) -> (usize, usize) {
        let wrap = (sx.max(1) != self.sx).then(|| self.wrap_position(cx, cy));
        self.resize(sx, sy);
        wrap.map_or((cx, cy), |w| self.unwrap_position(w))
    }

    /// `screen_resize_cursor(s, sx, sy, 1, 0, 0)`.
    fn resize(&mut self, sx: usize, sy: usize) {
        self.peeked.get_mut().clear();
        let (sx, sy) = (sx.max(1), sy.max(1));
        let reflow = sx != self.sx;
        self.sx = sx;
        if sy != self.sy {
            self.resize_y(sy);
        }
        if reflow {
            self.reflow();
        }
    }

    /// `screen_resize_y` without eating empty lines, history on
    /// (screen.c:410-488): a shorter screen pushes its top lines into
    /// history; a taller one pulls lines back from history, then adds
    /// blank lines at the bottom.
    fn resize_y(&mut self, sy: usize) {
        if sy < self.sy {
            let needed = self.sy - sy;
            self.hscrolled += needed;
            self.hsize += needed;
        } else {
            let available = self.hscrolled.min(sy - self.sy);
            self.hscrolled -= available;
            self.hsize -= available;
        }
        self.sy = sy;
        self.lines.resize_with(self.hsize + sy, Line::default);
    }

    /// `grid_reflow` to the current `sx`.
    fn reflow(&mut self) {
        self.read_all();
        let sx = self.sx;
        let mut source: Vec<Option<Line>> = std::mem::take(&mut self.lines)
            .into_iter()
            .map(Some)
            .collect();
        let mut target: Vec<Line> = Vec::with_capacity(source.len());
        for yy in 0..source.len() {
            let Some(line) = source[yy].as_ref() else {
                continue;
            };
            // `at`: the first cell past the width; `width`: the whole width.
            let (mut at, mut width) = (0, 0);
            for (i, c) in line.row().cells.iter().enumerate() {
                let w = usize::from(c.width);
                if at == 0 && width + w > sx {
                    at = i;
                }
                width += w;
            }
            let wrapped = line.row().wrapped;
            if width > sx {
                self.reflow_split(&mut target, &mut source, yy, at);
            } else if width < sx && wrapped {
                self.reflow_join(&mut target, &mut source, yy, width, false);
            } else {
                target.push(source[yy].take().expect("live line"));
            }
        }
        if target.len() < self.sy {
            target.resize_with(self.sy, Line::default);
        }
        self.hsize = target.len() - self.sy;
        self.hscrolled = self.hscrolled.min(self.hsize);
        self.lines = target;
    }

    /// `grid_reflow_join`: pull cells of the lines below onto line `yy`
    /// (moved to `target` first unless `already`, then it is the last
    /// target line) while they fit and the lines are wrapped.
    fn reflow_join(
        &mut self,
        target: &mut Vec<Line>,
        source: &mut [Option<Line>],
        yy: usize,
        mut width: usize,
        already: bool,
    ) {
        let sx = self.sx;
        if !already {
            target.push(source[yy].take().expect("live line"));
        }
        let to = target.len() - 1;
        let (mut lines, mut from, mut want, mut wrapped) = (0, None, 0, true);
        loop {
            let line = yy + 1 + lines;
            if line == source.len() {
                break;
            }
            let next = source[line].as_ref().expect("lines below are live");
            if !next.row().wrapped {
                wrapped = false;
            }
            if next.row().cells.is_empty() {
                if !wrapped {
                    break;
                }
                lines += 1;
                continue;
            }
            if width + usize::from(next.row().cells[0].width) > sx {
                break;
            }
            want = 0;
            for c in &next.row().cells {
                if width + usize::from(c.width) > sx {
                    break;
                }
                width += usize::from(c.width);
                target[to].row_mut().cells.push(c.clone());
                want += 1;
            }
            from = Some(line);
            lines += 1;
            if !wrapped || want != next.row().cells.len() || width == sx {
                break;
            }
        }
        let Some(from) = from else { return };
        if lines == 0 {
            return;
        }
        let rest = source[from].as_mut().expect("live line");
        if want == rest.row().cells.len() {
            if !wrapped {
                target[to].row_mut().wrapped = false;
            }
        } else {
            rest.row_mut().cells.drain(..want);
            lines -= 1;
        }
        for dead in &mut source[yy + 1..yy + 1 + lines] {
            *dead = None;
        }
        if self.hscrolled > to + lines {
            self.hscrolled -= lines;
        } else if self.hscrolled > to {
            self.hscrolled = to;
        }
    }

    /// `grid_reflow_split`: line `yy` is too wide; it keeps its first
    /// `at` cells and the rest go on new lines.
    fn reflow_split(
        &mut self,
        target: &mut Vec<Line>,
        source: &mut [Option<Line>],
        yy: usize,
        at: usize,
    ) {
        let sx = self.sx;
        let mut gl = source[yy].take().expect("live line");
        let was_wrapped = gl.row().wrapped;
        let mut lines = 2;
        let mut width = 0;
        for c in &gl.row().cells[at..] {
            let w = usize::from(c.width);
            if width + w > sx {
                lines += 1;
                width = 0;
            }
            width += w;
        }
        let first = target.len();
        target.resize_with(first + lines, Line::default);
        let mut line = first + 1;
        width = 0;
        for c in gl.row_mut().cells.drain(at..) {
            let w = usize::from(c.width);
            if width + w > sx {
                target[line].row_mut().wrapped = true;
                line += 1;
                width = 0;
            }
            width += w;
            target[line].row_mut().cells.push(c);
        }
        if was_wrapped {
            target[line].row_mut().wrapped = true;
        }
        gl.row_mut().wrapped = true;
        target[first] = gl;
        if yy <= self.hscrolled {
            self.hscrolled += lines - 1;
        }
        if width < sx && was_wrapped {
            // The last new line has room: the next lines join it.
            target.truncate(line + 1);
            self.reflow_join(target, source, yy, width, true);
        }
    }

    /// `grid_wrap_position`.
    #[must_use]
    pub fn wrap_position(&self, px: usize, py: usize) -> WrapPos {
        let (mut ax, mut ay) = (0, 0);
        for line in &self.lines[..py.min(self.lines.len())] {
            if self.wrapped_line(line) {
                ax += self.used(line);
            } else {
                ax = 0;
                ay += 1;
            }
        }
        let used = self.lines.get(py).map_or(0, |l| self.used(l));
        WrapPos {
            x: (px < used).then_some(ax + px),
            y: ay,
        }
    }

    /// `grid_unwrap_position`.
    #[must_use]
    pub fn unwrap_position(&self, pos: WrapPos) -> (usize, usize) {
        let ey = self.lines.len() - 1;
        let wrapped = |y: usize| self.wrapped_line(&self.lines[y]);
        let mut yy = 0;
        let mut ay = 0;
        while yy < ey {
            if ay == pos.y {
                break;
            }
            if !wrapped(yy) {
                ay += 1;
            }
            yy += 1;
        }
        let Some(mut wx) = pos.x else {
            while yy < ey && wrapped(yy) {
                yy += 1;
            }
            return (self.used(&self.lines[yy]), yy);
        };
        while yy < ey && wrapped(yy) {
            let used = self.used(&self.lines[yy]);
            if wx < used {
                break;
            }
            wx -= used;
            yy += 1;
        }
        (wx, yy)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use vtmux_vt::Cell;

    thread_local! {
        /// Rows [`Grid::extend_from`] read on this thread.
        pub(in crate::mode) static ROWS_READ: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
        /// Rows [`Grid::peek`] read as cells from the entry copy.
        pub(in crate::mode) static ROWS_PEEKED: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
    }

    fn texts(g: &Grid) -> Vec<(String, bool)> {
        g.lines
            .iter()
            .map(|l| (g.line_row(l).text(), g.line_row(l).wrapped))
            .collect()
    }

    fn line(s: &str, wrapped: bool) -> Line {
        let row = Row {
            cells: s
                .chars()
                .map(|c| Cell {
                    text: c.into(),
                    width: 1,
                    ..Cell::default()
                })
                .collect(),
            wrapped,
        };
        Line {
            row: OnceCell::from(row),
            ..Line::default()
        }
    }

    fn grid(sx: usize, sy: usize, hsize: usize, lines: Vec<Line>) -> Grid {
        Grid {
            lines,
            source: None,
            hsize,
            hscrolled: hsize,
            sx,
            sy,
            peeked: RefCell::default(),
        }
    }

    #[test]
    fn narrower_splits_and_wider_joins() {
        let mut g = grid(
            6,
            2,
            1,
            vec![line("abcdef", true), line("gh", false), line("xy", false)],
        );
        let pos = g.wrap_position(1, 1);
        assert_eq!(pos, WrapPos { x: Some(7), y: 0 });
        g.resize(4, 2);
        assert_eq!(
            texts(&g),
            [
                ("abcd".into(), true),
                ("efgh".into(), false),
                ("xy".into(), false)
            ]
        );
        assert_eq!(g.unwrap_position(pos), (3, 1));
        g.resize(10, 2);
        assert_eq!(
            texts(&g),
            [("abcdefgh".into(), false), ("xy".into(), false)]
        );
        assert_eq!(g.hsize, 0);
    }

    /// A shorter screen pushes lines into history; a taller one pulls
    /// them back before adding blanks.
    #[test]
    fn height_moves_the_history_boundary() {
        let mut g = grid(
            4,
            3,
            1,
            (0..4).map(|i| line(&i.to_string(), false)).collect(),
        );
        g.resize(4, 2);
        assert_eq!((g.hsize, g.lines.len()), (2, 4));
        g.resize(4, 6);
        assert_eq!((g.hsize, g.lines.len()), (0, 6));
        assert_eq!(g.row(3).text(), "3");
        assert_eq!(g.row(5).text(), "");
    }

    #[test]
    fn copy_keeps_rows_the_pane_drops() {
        let mut t = PaneTerm::new(10, 3, 5);
        for i in 0..8 {
            t.feed(format!("l{i}\r\n").as_bytes());
        }
        let (g, cx, cy) = Grid::clone_of(&t, 10, 3, false);
        let top = g.row(0).text();
        assert_eq!((cx, cy), (0, g.hsize + 2));
        for i in 0..5000 {
            t.feed(format!("m{i}\r\n").as_bytes());
        }
        assert_eq!(g.row(0).text(), top);
        assert!(g.time(0) > 0);
    }

    /// Rows are read on demand, from the copy taken on entry: a line first
    /// read after the pane cleared, overwrote or compressed its history
    /// still shows the entry's text, and the pane's compressed pages stay
    /// compressed.
    #[test]
    fn unread_lines_come_from_the_entry_copy() {
        let mut t = PaneTerm::new(40, 5, 5000);
        for i in 0..3000 {
            t.feed(format!("line {i} \x1b[31mred\x1b[m\r\n").as_bytes());
        }
        while t.compress_step() == vtmux_vt::Compression::Pending {}
        let compressed = t.compressed_pages();
        assert!(compressed > 0);
        let want: Vec<Row> = (0..t.history_size() + 5)
            .map(|y| used_cells(t.row(y)))
            .collect();
        // Reading the rows above restored the pane's pages: compress again.
        while t.compress_step() == vtmux_vt::Compression::Pending {}
        let compressed = t.compressed_pages().max(compressed);
        let (g, _, _) = Grid::clone_of(&t, 40, 5, false);
        assert_eq!(t.compressed_pages(), compressed, "entry decoded the pane");
        t.feed(b"\x1b[3J\x1b[2J\x1b[Hgone");
        for i in 0..100 {
            t.feed(format!("new {i}\r\n").as_bytes());
        }
        assert_eq!(g.last() + 1, want.len());
        for (y, row) in want.iter().enumerate() {
            assert_eq!(g.row(y), row, "line {y}");
        }
    }

    /// `capture-pane -M` of all of a copy writes an unread line of plain
    /// text from the entry copy, keeping no row for it (a 1M-line history
    /// kept a row of cells per line, and took 6x tmux's time), and writes
    /// what it writes once every line is read.
    #[test]
    fn capture_of_the_copy_keeps_no_lines() {
        let mut t = PaneTerm::new(40, 5, 5000);
        for i in 0..3000 {
            let red = if i % 10 == 0 { "\x1b[31mred\x1b[m" } else { "" };
            t.feed(format!("line {i} \\ {red}\r\n").as_bytes());
        }
        let (g, _, _) = Grid::clone_of(&t, 40, 5, false);
        let opts = vtmux_vt::CaptureOpts {
            start: vtmux_vt::LineRef::Start,
            end: vtmux_vt::LineRef::End,
            ..vtmux_vt::CaptureOpts::default()
        };
        let captured = vtmux_vt::capture_lines(&g, 40, opts);
        assert!(g.kept_lines() <= 300, "{} lines kept", g.kept_lines());
        for y in 0..=g.last() {
            let _ = g.row(y);
        }
        assert_eq!(g.kept_lines(), g.last() + 1);
        assert_eq!(vtmux_vt::capture_lines(&g, 40, opts), captured);
        assert_eq!(captured, vtmux_vt::capture(&t, opts));
    }
}
