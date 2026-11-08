//! tmux's `hscrolled` through the resize that ends an alternate screen.
//!
//! tmux leaves the primary grid alone while an alternate screen is up
//! (`GRID_HISTORY` off, no reflow: window.c `window_pane_resize`) and
//! resizes it once on the way back, from the size it had to the current
//! one, history on (screen.c `screen_alternate_off`). Ghostty resizes the
//! primary screen at every resize instead, so [`super::PaneTerm`] works out
//! what that one resize does to `hscrolled` from the copy of the primary
//! screen it took as the alternate one came up.

/// A line of tmux's grid as `grid_reflow` reads it: each stored cell's
/// width up to `cellused`, and `GRID_LINE_WRAPPED`.
pub(super) struct Line {
    cells: Vec<u8>,
    wrapped: bool,
    /// Cells a join took from the front (`grid_move_cells`).
    start: usize,
    /// Joined whole into a line above (`GRID_LINE_DEAD`).
    dead: bool,
}

impl Line {
    pub(super) fn new(cells: Vec<u8>, wrapped: bool) -> Self {
        Self {
            cells,
            wrapped,
            start: 0,
            dead: false,
        }
    }

    fn used(&self) -> &[u8] {
        &self.cells[self.start..]
    }
}

/// The lines of a grid, made as the reflow first reaches each.
pub(super) struct Lines<F> {
    made: Vec<Line>,
    len: usize,
    make: F,
}

impl<F: FnMut(usize) -> Line> Lines<F> {
    /// `len` lines, line `y` being `make(y)`.
    pub(super) fn new(len: usize, make: F) -> Self {
        Self {
            made: Vec::new(),
            len,
            make,
        }
    }

    fn get(&mut self, y: usize) -> &mut Line {
        while self.made.len() <= y {
            let line = (self.make)(self.made.len());
            self.made.push(line);
        }
        &mut self.made[y]
    }
}

/// `screen_resize_y` (screen.c) with history on, for `hscrolled` (`h`): a
/// screen `rows` shorter deletes up to `below` lines under the cursor and
/// pushes the rest into history, which adds them; a taller one pulls back
/// at most `h` lines. Returns the new `h`, the lines deleted from the
/// bottom and the blank lines added there.
pub(super) fn resize_y(h: usize, from: usize, to: usize, below: usize) -> (usize, usize, usize) {
    if to < from {
        let needed = from - to;
        let eat = needed.min(below);
        (h + needed - eat, eat, 0)
    } else {
        let pull = h.min(to - from);
        (h - pull, 0, to - from - pull)
    }
}

/// What `grid_reflow` (grid.c) of `lines` to `sx` columns does to
/// `hscrolled` (`h`), as if it were the index of a line from the top:
/// `grid_reflow_split` adds a split line's extra lines when its index is
/// not past it, `grid_reflow_join` takes away the lines it consumes before
/// it, or stops it at the line they join. The caller caps the result at
/// the new history size, as `grid_reflow` does.
pub(super) fn reflow<F: FnMut(usize) -> Line>(
    lines: &mut Lines<F>,
    sx: usize,
    mut h: usize,
) -> usize {
    let mut target = 0;
    for yy in 0..lines.len {
        let line = lines.get(yy);
        if line.dead {
            continue;
        }
        // A later split's index is past it and a later join's line is
        // not before it: nothing after moves it.
        if yy > h && target > h {
            break;
        }
        let (mut at, mut width) = (0, 0);
        for (i, &w) in line.used().iter().enumerate() {
            if at == 0 && width + usize::from(w) > sx {
                at = i;
            }
            width += usize::from(w);
        }
        let wrapped = line.wrapped;
        if width == sx {
            target += 1;
        } else if width > sx {
            // grid_reflow_split: as many lines as the cells from `at` need
            // after the first, the last as wide as what is left.
            let (mut count, mut last) = (2, 0);
            for &w in &line.used()[at..] {
                if last + usize::from(w) > sx {
                    count += 1;
                    last = 0;
                }
                last += usize::from(w);
            }
            target += count;
            if yy <= h {
                h += count - 1;
            }
            if last < sx && wrapped {
                h = join(lines, sx, yy, last, target - 1, h);
            }
        } else if wrapped {
            target += 1;
            h = join(lines, sx, yy, width, target - 1, h);
        } else {
            target += 1;
        }
    }
    h
}

/// `grid_reflow_join`: line `yy` ends on target line `to`, `width` wide;
/// join the lines after it onto it while they fit.
fn join<F: FnMut(usize) -> Line>(
    lines: &mut Lines<F>,
    sx: usize,
    yy: usize,
    mut width: usize,
    to: usize,
    h: usize,
) -> usize {
    let (mut consumed, mut wrapped, mut from, mut want) = (0, true, None, 0);
    loop {
        let y = yy + 1 + consumed;
        if y == lines.len {
            break;
        }
        let line = lines.get(y);
        if !line.wrapped {
            wrapped = false;
        }
        let cells = line.used();
        if cells.is_empty() {
            if !wrapped {
                break;
            }
            consumed += 1;
            continue;
        }
        if width + usize::from(cells[0]) > sx {
            break;
        }
        width += usize::from(cells[0]);
        from = Some(y);
        want = 1;
        while want < cells.len() && width + usize::from(cells[want]) <= sx {
            width += usize::from(cells[want]);
            want += 1;
        }
        consumed += 1;
        if !wrapped || want != cells.len() || width == sx {
            break;
        }
    }
    let Some(from) = from else {
        return h;
    };
    // What is left of a line not consumed whole stays a line of its own.
    let line = lines.get(from);
    if line.used().len() != want {
        line.start += want;
        consumed -= 1;
    }
    for y in yy + 1..yy + 1 + consumed {
        lines.get(y).dead = true;
    }
    if h > to + consumed {
        h - consumed
    } else if h > to {
        to
    } else {
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[(usize, bool)]) -> Vec<Line> {
        rows.iter()
            .map(|&(n, wrapped)| Line::new(vec![1; n], wrapped))
            .collect()
    }

    fn run(rows: Vec<Line>, sx: usize, h: usize) -> usize {
        let mut rows = rows.into_iter().map(Some).collect::<Vec<_>>();
        let len = rows.len();
        let mut lines = Lines::new(len, |y| rows[y].take().expect("made once"));
        reflow(&mut lines, sx, h)
    }

    /// By hand from grid.c: a split line adds its extra lines while its
    /// index is not past `hscrolled`, so once the first line splits every
    /// later one does (the count runs on); a short first line stops it.
    #[test]
    fn a_split_runs_on_from_a_line_not_past_it() {
        // Three lines of 75 cells to 25 columns: 0 -> 2 -> 4 -> 6.
        assert_eq!(run(grid(&[(75, false); 3]), 25, 0), 6);
        // A short first line: line 1 is past 0.
        assert_eq!(run(grid(&[(4, false), (75, false), (75, false)]), 25, 0), 0);
        // The history of the reviewer's probe: 11 of 60 lines scrolled.
        let mut rows = grid(&[(75, false); 60]);
        rows.extend(grid(&[(2, false)]));
        assert_eq!(run(rows, 25, 11), 131);
    }

    /// A join takes away the lines it consumes before `hscrolled`, or
    /// stops it at the line they join; what is left of a line consumed in
    /// part is reflowed as a line of its own.
    #[test]
    fn a_join_takes_away_the_lines_before_it() {
        // Three lines of 75 split at 25, back to 80: 6 -> 4 -> 2 -> 2.
        let rows = grid(&[(25, true), (25, true), (25, false)].repeat(3));
        assert_eq!(run(rows, 80, 6), 2);
        // Stops at the joined line.
        let rows = grid(&[(25, true), (25, false), (10, false)]);
        assert_eq!(run(rows, 80, 1), 0);
        // 30 + 30 at 40 columns: 10 cells of the second line are left,
        // which stay a line: two target lines, nothing consumed.
        let rows = grid(&[(30, true), (30, false), (5, false)]);
        assert_eq!(run(rows, 40, 2), 2);
        // An empty wrapped line is consumed with the line after it.
        let rows = grid(&[(10, true), (0, true), (10, false), (5, false)]);
        assert_eq!(run(rows, 40, 3), 1);
    }

    /// A wide character is one cell of width 2 and a padding cell of
    /// width 0: it goes whole to the next line.
    #[test]
    fn a_wide_character_splits_whole() {
        let mut cells = vec![1; 4];
        cells.extend([2, 0]);
        // 4 + 2 at 5 columns: two lines.
        assert_eq!(run(vec![Line::new(cells, false)], 5, 0), 1);
    }

    #[test]
    fn a_resize_y_pushes_past_the_cursor_and_pulls_at_most_hscrolled() {
        // 50 rows to 20 with the cursor 10 rows above the bottom: 10 eaten,
        // 20 pushed.
        assert_eq!(resize_y(11, 50, 20, 10), (31, 10, 0));
        // 20 rows to 50 with 11 to pull: 19 blank.
        assert_eq!(resize_y(11, 20, 50, 0), (0, 0, 19));
        assert_eq!(resize_y(41, 20, 50, 0), (11, 0, 0));
    }
}
