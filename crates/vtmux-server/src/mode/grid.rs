//! Copy mode's own copy of a pane's grid (`data->backing`).
//!
//! Taken on entry (`window_copy_clone_screen`, window-copy.c:392-449), so
//! output the pane writes afterwards does not show and rows the pane drops
//! from its history stay. Resized with the mode as tmux resizes the copy
//! (`screen_resize_cursor` with reflow, screen.c:354-401): a change of
//! height moves the history boundary, a change of width reflows the lines
//! (`grid_reflow`, grid.c:1318-1611).

use vtmux_vt::{LineMark, PaneTerm, Row};

/// One line of the copy.
#[derive(Clone, Debug, Default)]
struct Line {
    /// The used cells (`cellused`): trailing blanks nothing wrote with a
    /// colour or a link are dropped, so `row.cells.len()` is the count.
    row: Row,
    /// When it scrolled into history, 0 if never (`grid_line_time`).
    time: i64,
    /// `GRID_LINE_START_PROMPT` and `GRID_LINE_START_OUTPUT`: OSC 133
    /// marked the line. Reflow keeps them on a line's first part.
    prompt: bool,
    output: bool,
}

impl Line {
    fn used(&self) -> usize {
        self.row.cells.len()
    }
}

/// A cursor position counted along unwrapped lines (`grid_wrap_position`):
/// `x` is `None` at or past the end of the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapPos {
    x: Option<usize>,
    y: usize,
}

/// The copy: history first, then the screen rows.
#[derive(Clone, Debug)]
pub struct Grid {
    /// `hsize + sy` lines.
    lines: Vec<Line>,
    hsize: usize,
    /// History lines that scrolled there and were not cleared
    /// (`hscrolled`): a taller screen pulls these back first.
    hscrolled: usize,
    sx: usize,
    sy: usize,
}

static EMPTY: Row = Row {
    cells: Vec::new(),
    wrapped: false,
};

/// Drop the trailing cells nothing wrote (Ghostty's empty cells with the
/// default style), keeping written spaces and coloured blanks as tmux's
/// `cellused` does.
fn used_cells(mut row: Row) -> Row {
    while row.cells.last().is_some_and(|c| {
        c.text.is_empty()
            && c.width == 1
            && !c.link
            && !c.acs
            && c.style == vtmux_vt::Style::default()
    }) {
        row.cells.pop();
    }
    row
}

impl Grid {
    /// `window_copy_clone_screen`: a copy of `src` for a view `sx` x `sy`
    /// and the cursor in it (column, line). `trim` drops trailing empty
    /// lines (`copy-mode -s`).
    #[must_use]
    pub fn clone_of(src: &PaneTerm, sx: usize, sy: usize, trim: bool) -> (Self, usize, usize) {
        let hsize = src.history_size();
        let mut n = hsize + usize::from(src.rows());
        if trim {
            while n > hsize + 1 && used_cells(src.row(n - 1)).cells.is_empty() {
                n -= 1;
            }
        }
        let mut lines: Vec<Line> = (0..n)
            .map(|y| Line {
                row: used_cells(src.row(y)),
                time: if y < hsize {
                    src.history_time(src.history_id(y))
                } else {
                    0
                },
                prompt: false,
                output: false,
            })
            .collect();
        for (y, mark) in src.line_marks() {
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
            hsize,
            hscrolled: hsize,
            sx: usize::from(src.cols()),
            sy: gsy,
        };
        let (cx, cy) = grid.resize_keeping(sx, sy, cx, hsize + cy);
        (grid, cx, cy)
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
        self.lines.get(y).map_or(&EMPTY, |l| &l.row)
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
            for (i, c) in line.row.cells.iter().enumerate() {
                let w = usize::from(c.width);
                if at == 0 && width + w > sx {
                    at = i;
                }
                width += w;
            }
            let wrapped = line.row.wrapped;
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
            if !next.row.wrapped {
                wrapped = false;
            }
            if next.used() == 0 {
                if !wrapped {
                    break;
                }
                lines += 1;
                continue;
            }
            if width + usize::from(next.row.cells[0].width) > sx {
                break;
            }
            want = 0;
            for c in &next.row.cells {
                if width + usize::from(c.width) > sx {
                    break;
                }
                width += usize::from(c.width);
                target[to].row.cells.push(c.clone());
                want += 1;
            }
            from = Some(line);
            lines += 1;
            if !wrapped || want != next.used() || width == sx {
                break;
            }
        }
        let Some(from) = from else { return };
        if lines == 0 {
            return;
        }
        let rest = source[from].as_mut().expect("live line");
        if want == rest.used() {
            if !wrapped {
                target[to].row.wrapped = false;
            }
        } else {
            rest.row.cells.drain(..want);
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
        let was_wrapped = gl.row.wrapped;
        let mut lines = 2;
        let mut width = 0;
        for c in &gl.row.cells[at..] {
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
        for c in gl.row.cells.drain(at..) {
            let w = usize::from(c.width);
            if width + w > sx {
                target[line].row.wrapped = true;
                line += 1;
                width = 0;
            }
            width += w;
            target[line].row.cells.push(c);
        }
        if was_wrapped {
            target[line].row.wrapped = true;
        }
        gl.row.wrapped = true;
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
            if line.row.wrapped {
                ax += line.used();
            } else {
                ax = 0;
                ay += 1;
            }
        }
        let used = self.lines.get(py).map_or(0, Line::used);
        WrapPos {
            x: (px < used).then_some(ax + px),
            y: ay,
        }
    }

    /// `grid_unwrap_position`.
    #[must_use]
    pub fn unwrap_position(&self, pos: WrapPos) -> (usize, usize) {
        let ey = self.lines.len() - 1;
        let wrapped = |y: usize| self.lines[y].row.wrapped;
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
            return (self.lines[yy].used(), yy);
        };
        while yy < ey && wrapped(yy) {
            let used = self.lines[yy].used();
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
mod tests {
    use super::*;
    use vtmux_vt::Cell;

    fn texts(g: &Grid) -> Vec<(String, bool)> {
        g.lines
            .iter()
            .map(|l| (l.row.text(), l.row.wrapped))
            .collect()
    }

    fn line(s: &str, wrapped: bool) -> Line {
        Line {
            row: Row {
                cells: s
                    .chars()
                    .map(|c| Cell {
                        text: c.to_string(),
                        width: 1,
                        ..Cell::default()
                    })
                    .collect(),
                wrapped,
            },
            time: 0,
            prompt: false,
            output: false,
        }
    }

    fn grid(sx: usize, sy: usize, hsize: usize, lines: Vec<Line>) -> Grid {
        Grid {
            lines,
            hsize,
            hscrolled: hsize,
            sx,
            sy,
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
}
