//! A cursor that walks copy mode's grid (grid-reader.c), and the character
//! classes word movement uses (`grid_in_set`, grid.c:1709-1737).

use vtmux_vt::Cell;

use super::grid::Grid;

/// tmux's `WHITESPACE` (tmux.h:672): a set containing a space also matches
/// Unicode whitespace.
pub const WHITESPACE: &str = "\t ";

/// `utf8_has_whitespace` (utf8.c:611-669): any code point of the cell is
/// `White_Space` (or U+0085).
fn has_whitespace(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c,
            '\t' | '\n'
                | '\u{0B}'
                | '\u{0C}'
                | '\r'
                | ' '
                | '\u{85}'
                | '\u{A0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
        )
    })
}

/// A cell's text as tmux stores it: a blank is a space.
#[must_use]
pub fn cell_text(c: &Cell) -> &str {
    if c.text.is_empty() { " " } else { &c.text }
}

impl Grid {
    /// `grid_get_cell`: past the line's cells, the default cell (a space).
    #[must_use]
    pub fn cell(&self, x: usize, y: usize) -> Cell {
        self.row(y).cells.get(x).cloned().unwrap_or(Cell {
            width: 1,
            ..Cell::default()
        })
    }

    #[must_use]
    pub fn padding(&self, x: usize, y: usize) -> bool {
        self.row(y).cells.get(x).is_some_and(|c| c.width == 0)
    }

    #[must_use]
    pub fn wrapped(&self, y: usize) -> bool {
        self.row(y).wrapped
    }

    /// `grid_line_length`.
    #[must_use]
    pub fn length(&self, y: usize) -> usize {
        self.row(y).length().min(self.sx())
    }

    /// `grid_line_limit`.
    #[must_use]
    pub fn limit(&self, y: usize) -> usize {
        let mut x = self.length(y);
        if x == 0 {
            return 0;
        }
        x -= 1;
        while x > 0 && self.padding(x, y) {
            x -= 1;
        }
        x
    }

    /// `grid_in_set`: the columns the character at `(x, y)` covers if it
    /// is in `set`, else 0. A TAB cell is in any set holding a space or a
    /// tab and covers the columns of the tab.
    #[must_use]
    pub fn in_set(&self, x: usize, y: usize, set: &str) -> usize {
        let has_tab = set.contains('\t');
        let has_space = set.contains(' ');
        let c = self.cell(x, y);
        if c.width == 0 {
            if !has_tab && !has_space {
                return 0;
            }
            let mut px = x;
            let mut prev;
            loop {
                px -= 1;
                prev = self.cell(px, y);
                if px == 0 || prev.width != 0 {
                    break;
                }
            }
            if prev.tab || (has_space && has_whitespace(cell_text(&prev))) {
                return usize::from(prev.width).saturating_sub(x - px);
            }
            return 0;
        }
        if (has_tab || has_space) && c.tab {
            return usize::from(c.width);
        }
        if has_space && has_whitespace(cell_text(&c)) {
            return usize::from(c.width.max(1));
        }
        // utf8_cstrhas: the cell is one of the set's characters.
        let text = cell_text(&c);
        let mut chars = text.chars();
        let one = chars.next().filter(|_| chars.next().is_none());
        usize::from(one.is_some_and(|ch| set.contains(ch)))
    }
}

/// `struct grid_reader`.
pub struct Reader<'a> {
    gd: &'a Grid,
    pub cx: usize,
    pub cy: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub fn new(gd: &'a Grid, cx: usize, cy: usize) -> Self {
        Self { gd, cx, cy }
    }

    #[must_use]
    pub fn get(&self) -> (usize, usize) {
        (self.cx, self.cy)
    }

    fn line_length(&self) -> usize {
        self.gd.length(self.cy)
    }

    #[must_use]
    pub fn in_set(&self, set: &str) -> usize {
        self.gd.in_set(self.cx, self.cy, set)
    }

    fn skip_padding_left(&mut self) {
        while self.cx > 0 && self.gd.padding(self.cx, self.cy) {
            self.cx -= 1;
        }
    }

    /// `grid_reader_cursor_right`.
    pub fn cursor_right(&mut self, wrap: bool, all: bool, onemore: bool) {
        let px = if all {
            self.gd.sx()
        } else if onemore {
            self.line_length()
        } else {
            self.gd.limit(self.cy)
        };
        if wrap && self.cx >= px && self.cy < self.gd.last() {
            self.start_of_line(false);
            self.cursor_down();
        } else if self.cx < px {
            self.cx += 1;
            while self.cx < px && self.gd.padding(self.cx, self.cy) {
                self.cx += 1;
            }
        }
    }

    /// `grid_reader_cursor_left`.
    pub fn cursor_left(&mut self, wrap: bool) {
        self.skip_padding_left();
        if self.cx == 0 && self.cy > 0 && (wrap || self.gd.wrapped(self.cy - 1)) {
            self.cursor_up();
            self.end_of_line(false, false);
        } else if self.cx > 0 {
            self.cx -= 1;
        }
    }

    /// `grid_reader_cursor_down`.
    pub fn cursor_down(&mut self) {
        if self.cy < self.gd.last() {
            self.cy += 1;
        }
        self.skip_padding_left();
    }

    /// `grid_reader_cursor_up`.
    pub fn cursor_up(&mut self) {
        self.cy = self.cy.saturating_sub(1);
        self.skip_padding_left();
    }

    /// `grid_reader_cursor_start_of_line`.
    pub fn start_of_line(&mut self, wrap: bool) {
        if wrap {
            while self.cy > 0 && self.gd.wrapped(self.cy - 1) {
                self.cy -= 1;
            }
        }
        self.cx = 0;
    }

    /// `grid_reader_cursor_end_of_line`.
    pub fn end_of_line(&mut self, wrap: bool, all: bool) {
        if wrap {
            while self.cy < self.gd.last() && self.gd.wrapped(self.cy) {
                self.cy += 1;
            }
        }
        self.cx = if all {
            self.gd.sx()
        } else {
            self.line_length()
        };
    }

    /// `grid_reader_handle_wrap`: keep the cursor within `xx`, wrapping
    /// down; false past the bottom of the grid.
    fn handle_wrap(&mut self, xx: &mut usize) -> bool {
        while self.cx > *xx {
            if self.cy == self.gd.last() {
                return false;
            }
            self.start_of_line(false);
            self.cursor_down();
            *xx = if self.gd.wrapped(self.cy) {
                self.gd.sx() - 1
            } else {
                self.line_length()
            };
        }
        true
    }

    /// The bound word movement starts with: wrapped words are not broken.
    fn word_bound(&self) -> usize {
        if self.gd.wrapped(self.cy) {
            self.gd.sx() - 1
        } else {
            self.line_length()
        }
    }

    /// `grid_reader_cursor_next_word`.
    pub fn next_word(&mut self, separators: &str) {
        let mut xx = self.word_bound();
        if !self.handle_wrap(&mut xx) {
            return;
        }
        if self.in_set(WHITESPACE) == 0 {
            if self.in_set(separators) != 0 {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx)
                        && self.in_set(separators) != 0
                        && self.in_set(WHITESPACE) == 0)
                    {
                        break;
                    }
                }
            } else {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx)
                        && self.in_set(separators) == 0
                        && self.in_set(WHITESPACE) == 0)
                    {
                        break;
                    }
                }
            }
        }
        while self.handle_wrap(&mut xx) {
            let width = self.in_set(WHITESPACE);
            if width == 0 {
                break;
            }
            self.cx += width;
        }
    }

    /// `grid_reader_cursor_next_word_end`.
    pub fn next_word_end(&mut self, separators: &str) {
        let mut xx = self.word_bound();
        while self.handle_wrap(&mut xx) {
            if self.in_set(WHITESPACE) != 0 {
                self.cx += 1;
            } else if self.in_set(separators) != 0 {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx)
                        && self.in_set(separators) != 0
                        && self.in_set(WHITESPACE) == 0)
                    {
                        break;
                    }
                }
                return;
            } else {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx)
                        && self.in_set(WHITESPACE) == 0
                        && self.in_set(separators) == 0)
                    {
                        break;
                    }
                }
                return;
            }
        }
    }

    /// `grid_reader_cursor_previous_word`.
    pub fn previous_word(&mut self, separators: &str, already: bool, stop_at_eol: bool) {
        let word_is_letters;
        if already || self.in_set(WHITESPACE) != 0 {
            loop {
                if self.cx > 0 {
                    self.cx -= 1;
                    if self.in_set(WHITESPACE) == 0 {
                        word_is_letters = self.in_set(separators) == 0;
                        break;
                    }
                } else {
                    if self.cy == 0 {
                        return;
                    }
                    self.cursor_up();
                    self.end_of_line(false, false);
                    // Stop if separator at EOL.
                    if stop_at_eol && self.cx > 0 {
                        let at_eol = self.gd.in_set(self.cx - 1, self.cy, WHITESPACE) != 0;
                        if at_eol {
                            word_is_letters = false;
                            break;
                        }
                    }
                }
            }
        } else {
            word_is_letters = self.in_set(separators) == 0;
        }
        // Back to the beginning of this word.
        let mut old;
        loop {
            old = (self.cx, self.cy);
            if self.cx == 0 {
                if self.cy == 0 || !self.gd.wrapped(self.cy - 1) {
                    break;
                }
                self.cursor_up();
                self.end_of_line(false, true);
            }
            if self.cx > 0 {
                self.cx -= 1;
            }
            if !(self.in_set(WHITESPACE) == 0 && word_is_letters != (self.in_set(separators) != 0))
            {
                break;
            }
        }
        (self.cx, self.cy) = old;
    }

    /// `grid_reader_cursor_back_to_indentation`.
    pub fn back_to_indentation(&mut self) {
        let old = (self.cx, self.cy);
        self.start_of_line(true);
        for py in self.cy..=self.gd.last() {
            let xx = self.gd.length(py);
            for px in 0..xx {
                let c = self.gd.cell(px, py);
                if c.width != 0 && !c.tab && cell_text(&c) != " " {
                    (self.cx, self.cy) = (px, py);
                    return;
                }
            }
            if !self.gd.wrapped(py) {
                break;
            }
        }
        (self.cx, self.cy) = old;
    }
}
