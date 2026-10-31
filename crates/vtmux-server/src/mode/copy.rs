//! Copy mode (window-copy.c): a scrollable view of a pane's history and
//! screen with its own cursor, over a copy of the pane's grid taken on
//! entry ([`Grid`]).
//!
//! Selections, search, jumps, marks, copying, refresh and drawing line
//! numbers are not here yet.

use vtmux_vt::{Cell, PaneTerm, Row, Snapshot};

use super::grid::Grid;

/// What a copy mode command asks for afterwards
/// (`enum window_copy_cmd_action`); only leaving is visible to vtmux, which
/// draws the whole mode screen every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Stay,
    Cancel,
}

/// `RECENTRE_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recentre {
    Top,
    Middle,
    Bottom,
}

/// The options copy mode reads as it goes.
#[derive(Clone, Copy, Debug)]
pub struct Env {
    /// `mode-keys` is `vi`.
    pub vi: bool,
    /// `copy-mode-line-numbers`: off, default, absolute, relative, hybrid.
    pub line_numbers: i64,
}

/// One copy mode (`struct window_copy_mode_data`).
#[derive(Clone, Debug)]
pub struct CopyMode {
    backing: Grid,
    sx: usize,
    sy: usize,
    /// Lines scrolled up into history.
    oy: usize,
    cx: usize,
    cy: usize,
    /// Position and size of the last line with content the cursor left.
    lastcx: usize,
    lastsx: usize,
    scroll_exit: bool,
    hide_position: bool,
    /// 0 off, 1 from the option, 2 the default numbers when the option is off.
    line_numbers: u8,
    recentre: Recentre,
    recentre_line: usize,
    /// `wme->prefix`: the repeat count for the next command.
    pub prefix: u32,
    /// `copy-mode -k`: kill the pane when the mode exits.
    pub kill: bool,
}

/// The copy mode commands tmux has (`window_copy_cmd_table`), so one vtmux
/// has not got yet is an error rather than ignored like an unknown name.
const TMUX_COMMANDS: &[&str] = &[
    "append-selection",
    "append-selection-and-cancel",
    "back-to-indentation",
    "begin-selection",
    "bottom-line",
    "cancel",
    "clear-selection",
    "copy-end-of-line",
    "copy-end-of-line-and-cancel",
    "copy-pipe-end-of-line",
    "copy-pipe-end-of-line-and-cancel",
    "copy-line",
    "copy-line-and-cancel",
    "copy-pipe-line",
    "copy-pipe-line-and-cancel",
    "copy-pipe-no-clear",
    "copy-pipe",
    "copy-pipe-and-cancel",
    "copy-selection-no-clear",
    "copy-selection",
    "copy-selection-and-cancel",
    "cursor-down",
    "cursor-down-and-cancel",
    "cursor-left",
    "cursor-right",
    "cursor-up",
    "cursor-centre-vertical",
    "cursor-centre-horizontal",
    "end-of-line",
    "goto-line",
    "halfpage-down",
    "halfpage-down-and-cancel",
    "halfpage-up",
    "history-bottom",
    "history-top",
    "jump-again",
    "jump-backward",
    "jump-forward",
    "jump-reverse",
    "jump-to-backward",
    "jump-to-forward",
    "jump-to-mark",
    "line-numbers-on",
    "line-numbers-off",
    "line-numbers-toggle",
    "next-prompt",
    "previous-prompt",
    "middle-line",
    "next-matching-bracket",
    "next-paragraph",
    "next-space",
    "next-space-end",
    "next-word",
    "next-word-end",
    "other-end",
    "page-down",
    "page-down-and-cancel",
    "page-up",
    "pipe-no-clear",
    "pipe",
    "pipe-and-cancel",
    "previous-matching-bracket",
    "previous-paragraph",
    "previous-space",
    "previous-word",
    "recentre-top-bottom",
    "rectangle-on",
    "rectangle-off",
    "rectangle-toggle",
    "refresh-on",
    "refresh-off",
    "refresh-now",
    "refresh-toggle",
    "scroll-bottom",
    "scroll-down",
    "scroll-down-and-cancel",
    "scroll-exit-on",
    "scroll-exit-off",
    "scroll-exit-toggle",
    "scroll-middle",
    "scroll-to-mouse",
    "scroll-top",
    "scroll-up",
    "search-again",
    "search-backward",
    "search-backward-text",
    "search-backward-incremental",
    "search-forward",
    "search-forward-text",
    "search-forward-incremental",
    "search-reverse",
    "select-line",
    "select-word",
    "selection-mode",
    "set-mark",
    "start-of-line",
    "stop-selection",
    "toggle-position",
    "top-line",
];

/// A command [`CopyMode::command`] could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// tmux has it, vtmux not yet.
    NotImplemented(String),
}

impl CopyMode {
    /// `window_copy_init`: a copy of `source`'s grid (`copy-mode -s`, or
    /// the pane itself) in a `sx` x `sy` view, `-e` and `-H` given. A
    /// source other than the pane loses its trailing empty lines
    /// (`window_copy_clone_screen`'s `trim`).
    #[must_use]
    pub fn new(
        source: &PaneTerm,
        (sx, sy): (usize, usize),
        trim: bool,
        scroll_exit: bool,
        hide_position: bool,
    ) -> Self {
        let (backing, cx, cursor_row) = Grid::clone_of(source, sx, sy, trim);
        let hsize = backing.hsize();
        let (cy, oy) = if cursor_row < hsize {
            (0, hsize - cursor_row)
        } else {
            (cursor_row - hsize, 0)
        };
        Self {
            backing,
            sx,
            sy,
            oy,
            cx,
            cy,
            lastcx: 0,
            lastsx: 0,
            scroll_exit,
            hide_position,
            line_numbers: 1,
            recentre: Recentre::Middle,
            recentre_line: 0,
            prefix: 1,
            kill: false,
        }
    }

    /// `window_copy_resize` (window-copy.c:1212-1243): the copy is resized
    /// (reflowed if the width changed) keeping the cursor on its text; a
    /// cursor line now in history is shown at the top, else the view goes
    /// to the bottom.
    pub fn resize(&mut self, sx: usize, sy: usize) {
        let (sx, sy) = (sx.max(1), sy.max(1));
        let hsize = self.hsize();
        self.oy = self.oy.min(hsize + self.cy);
        let cy = hsize + self.cy - self.oy;
        let (cx, cy) = self.backing.resize_keeping(sx, sy, self.cx, cy);
        (self.sx, self.sy) = (sx, sy);
        let hsize = self.hsize();
        self.cx = cx;
        (self.cy, self.oy) = if cy < hsize {
            (0, hsize - cy)
        } else {
            (cy - hsize, 0)
        };
    }

    // --- grid access ------------------------------------------------------

    fn hsize(&self) -> usize {
        self.backing.hsize()
    }

    /// Last row of the backing grid (`hsize + sy - 1`).
    fn last_row(&self) -> usize {
        self.hsize() + self.sy - 1
    }

    /// Backing row `y`, history first.
    fn row(&self, y: usize) -> &Row {
        self.backing.row(y)
    }

    fn padding(&self, x: usize, y: usize) -> bool {
        self.row(y).cells.get(x).is_some_and(|c| c.width == 0)
    }

    fn wrapped(&self, y: usize) -> bool {
        self.row(y).wrapped
    }

    /// `window_copy_find_length`: `grid_line_length`.
    fn find_length(&self, py: usize) -> usize {
        self.row(py).length()
    }

    /// `window_copy_cursor_limit`: vi keys stop on the last character.
    fn cursor_limit(&self, env: Env, py: usize, allow_onemore: bool) -> usize {
        let row = self.row(py);
        if allow_onemore || !env.vi {
            row.length()
        } else {
            row.limit()
        }
    }

    /// The backing row under screen row `cy`.
    fn abs(&self, cy: usize) -> usize {
        self.hsize() + cy - self.oy
    }

    // --- formats ----------------------------------------------------------

    /// `scroll_position`.
    #[must_use]
    pub fn scroll_position(&self) -> usize {
        self.oy
    }

    /// `copy_cursor_x`, `copy_cursor_y`.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.cx, self.cy)
    }

    /// `window_copy_line_number_mode`.
    fn line_number_mode(&self, option: i64) -> i64 {
        match self.line_numbers {
            0 => 0,
            2 if option == 0 => 1,
            _ => option,
        }
    }

    /// `copy_line_numbers`.
    #[must_use]
    pub fn line_numbers_active(&self, option: i64) -> bool {
        self.line_number_mode(option) != 0
    }

    /// `copy_position` and `copy_position_limit`
    /// (`window_copy_formats`, window-copy.c:1106-1130).
    #[must_use]
    pub fn position(&self, option: i64) -> (usize, usize) {
        let hsize = self.hsize();
        if self.line_number_mode(option) >= 2 {
            (hsize - self.oy + 1, hsize + self.sy)
        } else {
            (self.oy, hsize)
        }
    }

    /// `top_line_time`: when the top line of the view scrolled into
    /// history, 0 for a line of the screen.
    #[must_use]
    pub fn top_line_time(&self) -> i64 {
        self.backing.time(self.hsize() - self.oy)
    }

    /// `copy_cursor_line` (`window_copy_get_line`, `format_grid_line`).
    #[must_use]
    pub fn cursor_line(&self) -> String {
        self.row(self.abs(self.cy)).text()
    }

    /// `copy_cursor_word` (`window_copy_get_word`, `format_grid_word`,
    /// format.c:7102-7168): the word around the cursor, following wrapped
    /// lines, ended by `separators`, a space or an empty cell.
    #[must_use]
    pub fn cursor_word(&self, separators: &str) -> String {
        let is_sep = |c: &Cell| c.text.is_empty() || c.text == " " || separators.contains(&*c.text);
        // Past the grid's cells, grid_get_cell gives the default cell: a space.
        let cell = |x: usize, y: usize| {
            self.row(y).cells.get(x).cloned().unwrap_or(Cell {
                width: 1,
                ..Cell::default()
            })
        };
        let (mut x, mut y) = (self.cx, self.abs(self.cy));
        let mut found = false;
        loop {
            let c = cell(x, y);
            if c.width != 0 && is_sep(&c) {
                found = true;
                break;
            }
            if x == 0 {
                if y == 0 || !self.wrapped(y - 1) {
                    break;
                }
                y -= 1;
                x = self.find_length(y);
                if x == 0 {
                    break;
                }
            }
            x -= 1;
        }
        let mut word = String::new();
        loop {
            if found {
                let end = self.find_length(y);
                if end == 0 || x == end - 1 {
                    if y == self.last_row() || !self.wrapped(y) {
                        break;
                    }
                    y += 1;
                    x = 0;
                } else {
                    x += 1;
                }
            }
            found = true;
            let c = cell(x, y);
            if c.width == 0 {
                continue;
            }
            if is_sep(&c) {
                break;
            }
            word.push_str(&c.text);
        }
        word
    }

    // --- drawing ----------------------------------------------------------

    /// Whether the position indicator is drawn (`!data->hide_position`).
    #[must_use]
    pub fn shows_position(&self) -> bool {
        !self.hide_position
    }

    /// The mode's screen (`window_copy_write_line` for every row, without
    /// the indicator): the backing rows in view, and the cursor.
    #[must_use]
    pub fn screen(&self) -> Snapshot {
        let (sx, sy) = (self.sx, self.sy);
        let mut cells = Vec::with_capacity(sx * sy);
        for py in 0..sy {
            let row = self.row(self.abs(py));
            for x in 0..sx {
                let blank = || Cell {
                    width: 1,
                    ..Cell::default()
                };
                let mut c = row.cells.get(x).cloned().unwrap_or_else(blank);
                // window_copy_write_one: a wide character cut by the edge is
                // a space.
                if c.width == 2 && x + 1 >= sx {
                    c = blank();
                }
                cells.push(c);
            }
        }
        let clamp = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        Snapshot {
            cols: clamp(sx),
            rows: clamp(sy),
            cells,
            wrapped: vec![false; sy],
            cursor: Some((clamp(self.cx.min(sx - 1)), clamp(self.cy))),
            cursor_visible: true,
        }
    }

    /// A cursor past the last column shows as `$` there, drawn last
    /// (window-copy.c:5483-5487). Written over the second half of a wide
    /// character, it clears the character (`screen_write_overwrite`,
    /// screen-write.c:3007-3025), so the screen says what the terminal
    /// shows and the character is drawn again once the `$` goes.
    pub fn mark_overflow(&self, screen: &mut Snapshot) {
        if self.cx < self.sx {
            return;
        }
        let row = self.cy * self.sx;
        let mut x = self.sx - 1;
        while x > 0 && screen.cells.get(row + x).is_some_and(|c| c.width == 0) {
            x -= 1;
            if let Some(c) = screen.cells.get_mut(row + x) {
                *c = Cell {
                    width: 1,
                    ..Cell::default()
                };
            }
        }
        if let Some(c) = screen.cells.get_mut(row + self.sx - 1) {
            *c = Cell {
                text: "$".into(),
                width: 1,
                ..Cell::default()
            };
        }
    }

    // --- commands ---------------------------------------------------------

    /// `window_copy_command`: run `name`, `prefix` times where it repeats.
    /// Unknown names do nothing, as in tmux.
    ///
    /// # Errors
    /// A copy mode command tmux has that vtmux does not yet.
    pub fn command(
        &mut self,
        env: Env,
        name: &str,
        args: &[String],
    ) -> Result<Action, CommandError> {
        let np = std::mem::replace(&mut self.prefix, 1).max(1);
        let action = self.run(env, name, args, np);
        if action.is_none() && TMUX_COMMANDS.contains(&name) {
            return Err(CommandError::NotImplemented(name.to_owned()));
        }
        Ok(action.unwrap_or(Action::Stay))
    }

    #[allow(clippy::too_many_lines)] // one arm per command
    fn run(&mut self, env: Env, name: &str, args: &[String], np: u32) -> Option<Action> {
        let repeat = |m: &mut Self, f: &mut dyn FnMut(&mut Self)| {
            for _ in 0..np {
                f(m);
            }
        };
        let stay = Some(Action::Stay);
        match name {
            "cancel" => return Some(Action::Cancel),
            "cursor-up" => repeat(self, &mut |m| m.cursor_up(env, false)),
            "cursor-down" => repeat(self, &mut |m| m.cursor_down(env, false)),
            "cursor-left" => repeat(self, &mut |m| m.cursor_left(env)),
            "cursor-right" => repeat(self, &mut |m| m.cursor_right(env)),
            "cursor-down-and-cancel" => {
                let cy = self.cy;
                repeat(self, &mut |m| m.cursor_down(env, false));
                if cy == self.cy && self.oy == 0 {
                    return Some(Action::Cancel);
                }
            }
            "start-of-line" => self.cursor_start_of_line(env),
            "back-to-indentation" => self.cursor_back_to_indentation(env),
            "end-of-line" => self.cursor_end_of_line(env),
            "top-line" => (self.cx, self.cy) = (0, 0),
            "middle-line" => (self.cx, self.cy) = (0, (self.sy - 1) / 2),
            "bottom-line" => (self.cx, self.cy) = (0, self.sy - 1),
            "cursor-centre-vertical" => self.update_cursor(env, self.cx, self.sy / 2),
            "cursor-centre-horizontal" => self.update_cursor(env, self.sx / 2, self.cy),
            "page-up" => repeat(self, &mut |m| m.pageup(env, false)),
            "halfpage-up" => repeat(self, &mut |m| m.pageup(env, true)),
            "page-down" | "halfpage-down" | "page-down-and-cancel" | "halfpage-down-and-cancel" => {
                let half = name.starts_with("half");
                let exit = name.ends_with("-and-cancel") || self.scroll_exit;
                for _ in 0..np {
                    if self.pagedown(env, half, exit) {
                        return Some(Action::Cancel);
                    }
                }
            }
            "history-top" => (self.cx, self.cy, self.oy) = (0, 0, self.hsize()),
            "history-bottom" => {
                self.cy = self.sy - 1;
                self.cx = self.cursor_limit(env, self.hsize() + self.cy, false);
                self.oy = 0;
            }
            "scroll-up" => {
                if self.oy != self.hsize() {
                    repeat(self, &mut |m| m.cursor_up(env, true));
                }
            }
            "scroll-down" => {
                if self.oy == 0 {
                    return Some(if self.scroll_exit {
                        Action::Cancel
                    } else {
                        Action::Stay
                    });
                }
                repeat(self, &mut |m| m.cursor_down(env, true));
                if self.scroll_exit && self.oy == 0 {
                    return Some(Action::Cancel);
                }
            }
            "scroll-down-and-cancel" => {
                repeat(self, &mut |m| m.cursor_down(env, true));
                if self.oy == 0 {
                    return Some(Action::Cancel);
                }
            }
            "scroll-top" => self.scroll_to(0),
            "scroll-middle" => self.scroll_to((self.sy - 1) / 2),
            "scroll-bottom" => self.scroll_to(self.sy - 1),
            "scroll-exit-on" => self.scroll_exit = true,
            "scroll-exit-off" => self.scroll_exit = false,
            "scroll-exit-toggle" => self.scroll_exit = !self.scroll_exit,
            "toggle-position" => self.hide_position = !self.hide_position,
            "goto-line" => {
                if let Some(line) = args.first().filter(|a| !a.is_empty()) {
                    self.goto_line(env, line);
                }
            }
            "recentre-top-bottom" => self.recentre_top_bottom(),
            // Line numbers are not drawn yet: only turning them off works.
            "line-numbers-off" => self.set_line_numbers(env, false, false),
            _ => return None,
        }
        stay
    }

    /// `window_copy_set_line_numbers1`.
    pub fn set_line_numbers(&mut self, env: Env, enabled: bool, force: bool) {
        self.line_numbers = if !enabled {
            0
        } else if force && env.line_numbers == 0 {
            2
        } else {
            1
        };
    }

    /// `window_copy_update_cursor`, the cursor part: clamped to the line.
    fn update_cursor(&mut self, env: Env, cx: usize, cy: usize) {
        let mut cx = cx;
        if cy < self.sy {
            cx = cx.min(self.cursor_limit(env, self.abs(cy), false));
        }
        (self.cx, self.cy) = (cx, cy);
    }

    /// `window_copy_scroll_up`: the view moves down `ny` lines (towards the
    /// screen).
    fn scroll_up(&mut self, ny: usize) {
        self.oy -= ny.min(self.oy);
    }

    /// `window_copy_scroll_down`: the view moves up `ny` lines into history.
    fn scroll_down(&mut self, ny: usize) {
        let hsize = self.hsize();
        if ny > hsize {
            return;
        }
        let ny = ny.min(hsize - self.oy);
        self.oy += ny;
    }

    /// Remember where the cursor was on its line before a vertical move,
    /// then put it back at the end of the new line if it was at the end.
    fn remember_column(&mut self) {
        let ox = self.find_length(self.abs(self.cy));
        if self.cx != ox {
            self.lastcx = self.cx;
            self.lastsx = ox;
        }
    }

    fn keep_end_of_line(&mut self, env: Env) {
        let px = self.find_length(self.abs(self.cy));
        if (self.cx >= self.lastsx && self.cx != px) || self.cx > px {
            self.update_cursor(env, px, self.cy);
        }
    }

    /// `window_copy_cursor_up` (window-copy.c:6489-6567), no selection.
    fn cursor_up(&mut self, env: Env, scroll_only: bool) {
        self.remember_column();
        if scroll_only && env.vi && self.cy < self.sy - 1 {
            self.update_cursor(env, self.cx, self.cy + 1);
        }
        if scroll_only || self.cy == 0 {
            self.cx = self.lastcx;
            self.scroll_down(1);
        } else {
            self.update_cursor(env, self.lastcx, self.cy - 1);
        }
        self.keep_end_of_line(env);
    }

    /// `window_copy_cursor_down` (window-copy.c:6569-6639), no selection.
    fn cursor_down(&mut self, env: Env, scroll_only: bool) {
        self.remember_column();
        if scroll_only && env.vi && self.cy > 0 {
            self.update_cursor(env, self.cx, self.cy - 1);
        }
        if scroll_only || self.cy == self.sy - 1 {
            self.cx = self.lastcx;
            self.scroll_up(1);
        } else {
            self.update_cursor(env, self.lastcx, self.cy + 1);
        }
        self.keep_end_of_line(env);
    }

    /// `window_copy_acquire_cursor_up`: scroll up if the reader's position
    /// `(px, py)` is above the view, then put the cursor there.
    fn acquire_cursor_up(&mut self, env: Env, px: usize, py: usize) {
        let top = self.hsize() - self.oy;
        let (ny, cy) = if py < top {
            (top - py, 0)
        } else {
            (0, py - top)
        };
        for _ in 0..ny {
            self.cursor_up(env, true);
        }
        self.update_cursor(env, px, cy);
    }

    /// `window_copy_acquire_cursor_down`.
    fn acquire_cursor_down(&mut self, env: Env, px: usize, py: usize) {
        let cy = py + self.oy - self.hsize();
        let yy = self.sy - 1;
        for _ in 0..cy.saturating_sub(yy) {
            self.cursor_down(env, true);
        }
        self.update_cursor(env, px, cy.min(yy));
    }

    /// `grid_reader_cursor_left` with wrap, then
    /// `window_copy_acquire_cursor_up`.
    fn cursor_left(&mut self, env: Env) {
        let (mut x, mut y) = (self.cx, self.abs(self.cy));
        while x > 0 && self.padding(x, y) {
            x -= 1;
        }
        if x == 0 && y > 0 {
            // grid_reader_cursor_up, then end of line without wrap.
            y -= 1;
            x = self.find_length(y);
        } else {
            x = x.saturating_sub(1);
        }
        self.acquire_cursor_up(env, x, y);
    }

    /// `window_copy_cursor_right`: `grid_reader_cursor_right` with wrap,
    /// one past the end for emacs keys.
    fn cursor_right(&mut self, env: Env) {
        let (mut x, mut y) = (self.cx, self.abs(self.cy));
        let px = self.cursor_limit(env, y, false);
        if x >= px && y < self.last_row() {
            x = 0;
            y += 1;
        } else if x < px {
            x += 1;
            while x < px && self.padding(x, y) {
                x += 1;
            }
        }
        self.acquire_cursor_down(env, x, y);
    }

    /// `grid_reader_cursor_start_of_line` with wrap.
    fn start_of_wrapped_line(&self, mut y: usize) -> usize {
        while y > 0 && self.wrapped(y - 1) {
            y -= 1;
        }
        y
    }

    fn cursor_start_of_line(&mut self, env: Env) {
        let y = self.start_of_wrapped_line(self.abs(self.cy));
        self.acquire_cursor_up(env, 0, y);
    }

    /// `grid_reader_cursor_back_to_indentation` (grid-reader.c:411-438):
    /// the first non-space of the (wrapped) line, else stay.
    fn cursor_back_to_indentation(&mut self, env: Env) {
        let (mut px, mut py) = (self.cx, self.abs(self.cy));
        let mut y = self.start_of_wrapped_line(py);
        'lines: while y <= self.last_row() {
            let row = self.row(y);
            for (x, c) in row.cells[..row.length()].iter().enumerate() {
                if c.width != 0 && !(c.text.is_empty() || c.text == " ") {
                    (px, py) = (x, y);
                    break 'lines;
                }
            }
            if !row.wrapped {
                break;
            }
            y += 1;
        }
        self.acquire_cursor_up(env, px, py);
    }

    /// `window_copy_cursor_end_of_line`: the end of the wrapped line.
    fn cursor_end_of_line(&mut self, env: Env) {
        let mut y = self.abs(self.cy);
        while y < self.last_row() && self.wrapped(y) {
            y += 1;
        }
        let px = self.cursor_limit(env, y, false);
        self.acquire_cursor_down(env, px, y);
    }

    /// The page size for page and half-page moves.
    fn page(&self, half: bool) -> usize {
        match (self.sy > 2, half) {
            (false, _) => 1,
            (true, true) => self.sy / 2,
            (true, false) => self.sy - 2,
        }
    }

    /// The shared start of the page moves: remember the column and go back
    /// to it.
    fn page_start(&mut self) {
        self.remember_column();
        self.cx = self.lastcx;
    }

    fn page_end(&mut self, env: Env) {
        let px = self.find_length(self.abs(self.cy));
        if (self.cx >= self.lastsx && self.cx != px) || self.cx > px {
            self.cursor_end_of_line(env);
        }
    }

    /// `window_copy_pageup1`.
    pub fn pageup(&mut self, env: Env, half: bool) {
        self.page_start();
        let n = self.page(half);
        if self.oy + n > self.hsize() {
            self.oy = self.hsize();
            self.cy = self.cy.saturating_sub(n);
        } else {
            self.oy += n;
        }
        self.page_end(env);
    }

    /// `window_copy_pagedown1`: whether the mode should exit.
    pub fn pagedown(&mut self, env: Env, half: bool, scroll_exit: bool) -> bool {
        self.page_start();
        let n = self.page(half);
        if self.oy < n {
            self.oy = 0;
            // As tmux: the whole page, not what was left of it.
            self.cy = (self.cy + n).min(self.sy - 1);
        } else {
            self.oy -= n;
        }
        self.page_end(env);
        scroll_exit && self.oy == 0
    }

    /// `copy-mode -S` without a mouse event (`window_copy_scroll1` with the
    /// slider at the top): the view goes to the top of history, the cursor
    /// keeps its row. Whether the mode should exit.
    pub fn scroll_to_top(&mut self, env: Env, scroll_exit: bool) -> bool {
        self.page_start();
        self.oy = self.hsize();
        self.page_end(env);
        scroll_exit && self.oy == 0
    }

    /// `window_copy_cmd_scroll_to`: scroll the cursor's line to row `to`.
    fn scroll_to(&mut self, to: usize) {
        if self.cy > to && self.oy >= self.cy - to {
            let delta = self.cy - to;
            self.scroll_up(delta);
            self.cy -= delta;
        } else if self.cy < to && self.hsize() - self.oy >= to - self.cy {
            let delta = to - self.cy;
            self.scroll_down(delta);
            self.cy += delta;
        }
    }

    /// `window_copy_goto_line`.
    fn goto_line(&mut self, env: Env, line: &str) {
        let Ok(lineno) = vtmux_core::cmd::strtonum(line, -1, i64::from(i32::MAX)) else {
            return;
        };
        let hsize = self.hsize();
        if self.line_number_mode(env.line_numbers) >= 2 {
            let line = usize::try_from(lineno).unwrap_or(0).clamp(1, hsize + 1);
            self.oy = hsize - (line - 1);
        } else {
            self.oy = usize::try_from(lineno)
                .ok()
                .filter(|&n| n <= hsize)
                .unwrap_or(hsize);
        }
    }

    /// `window_copy_cmd_recentre_top_bottom`: the cursor's line to the
    /// middle, then the top, then the bottom.
    fn recentre_top_bottom(&mut self) {
        let (cy, oy) = (self.cy, self.oy);
        let sy = self.sy - 1;
        let sm = sy / 2;
        let line = self.abs(cy);
        if self.recentre_line != line {
            self.recentre = Recentre::Middle;
            self.recentre_line = line;
        }
        let target = self.recentre;
        self.recentre = match target {
            Recentre::Middle => Recentre::Top,
            Recentre::Top => Recentre::Bottom,
            Recentre::Bottom => Recentre::Middle,
        };
        match target {
            Recentre::Middle => {
                if cy < sm {
                    self.scroll_down(sm - cy);
                } else if cy > sm {
                    self.scroll_up(cy - sm);
                }
                if self.oy != oy {
                    self.cy = cy + self.oy - oy;
                }
            }
            Recentre::Top => {
                self.scroll_up(cy);
                self.cy = cy - (oy - self.oy);
            }
            Recentre::Bottom => {
                self.scroll_down(sy - cy);
                self.cy = cy + (self.oy - oy);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten numbered lines on a 30x6 pane leave five in history and the
    /// cursor on the empty sixth row, as the reference's
    /// `printf 'line%d\n' 1..10` in `new -x 30 -y 6`.
    fn pane() -> PaneTerm {
        let mut t = PaneTerm::new(30, 6, 2000);
        for i in 1..=10 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        t
    }

    const ENV: Env = Env {
        vi: false,
        line_numbers: 0,
    };

    fn enter(t: &PaneTerm, scroll_exit: bool) -> CopyMode {
        let size = (usize::from(t.cols()), usize::from(t.rows()));
        CopyMode::new(t, size, false, scroll_exit, false)
    }

    fn cmd(m: &mut CopyMode, name: &str) -> Action {
        m.command(ENV, name, &[]).unwrap()
    }

    /// Reference (tmux 3.8, 30x6, emacs keys): entering puts the cursor at
    /// the pane's (0, 5); page-up scrolls 4 with the cursor following the
    /// end of `line7`; history-top is (0, 0) at offset 5.
    #[test]
    fn entering_and_paging() {
        let t = pane();
        let mut m = enter(&t, false);
        assert_eq!((m.scroll_position(), m.cursor()), (0, (0, 5)));
        assert_eq!(m.position(0), (0, 5));
        cmd(&mut m, "page-up");
        assert_eq!((m.scroll_position(), m.cursor()), (4, (5, 5)));
        assert_eq!(m.cursor_line(), "line7");
        cmd(&mut m, "history-top");
        assert_eq!((m.scroll_position(), m.cursor()), (5, (0, 0)));
        assert_eq!(m.cursor_line(), "line1");
        cmd(&mut m, "history-bottom");
        assert_eq!((m.scroll_position(), m.cursor()), (0, (0, 5)));
        cmd(&mut m, "cursor-up");
        assert_eq!(m.cursor(), (6, 4));
        assert_eq!(m.cursor_line(), "line10");
        cmd(&mut m, "cursor-up");
        assert_eq!(m.cursor(), (5, 3));
    }

    #[test]
    fn scroll_exit_and_cancel() {
        let t = pane();
        let mut m = enter(&t, true);
        cmd(&mut m, "page-up");
        assert_eq!(cmd(&mut m, "page-down"), Action::Cancel);
        let mut m = enter(&t, false);
        assert_eq!(cmd(&mut m, "scroll-down"), Action::Stay);
        assert_eq!(cmd(&mut m, "cursor-down-and-cancel"), Action::Cancel);
        assert_eq!(cmd(&mut m, "cancel"), Action::Cancel);
        assert_eq!(cmd(&mut m, "nosuch"), Action::Stay);
        assert_eq!(
            m.command(ENV, "begin-selection", &[]),
            Err(CommandError::NotImplemented("begin-selection".into()))
        );
    }

    /// The view is the copy taken on entry: later output does not show.
    #[test]
    fn view_survives_new_output() {
        let mut t = pane();
        let mut m = enter(&t, false);
        cmd(&mut m, "history-top");
        t.feed(b"more\r\nand more\r\n");
        assert_eq!(m.cursor_line(), "line1");
        let s = m.screen();
        assert_eq!(s.cell(0, 5).unwrap().text, "l");
        assert_eq!(s.cell(4, 5).unwrap().text, "6");
        cmd(&mut m, "history-bottom");
        assert_eq!(m.cursor_line(), "");
        assert_eq!(m.position(0), (0, 5));
    }

    /// `window_copy_resize`: a shorter view keeps the cursor on its line,
    /// shown at the top when that line is now in history; growing again
    /// pulls the lines back. Reference (tmux 3.8, 40x9 pane of line1..30,
    /// page-up then cursor-up, split -v): `[5] [6,0] line23`.
    #[test]
    fn resize_keeps_the_cursor_line() {
        let mut t = PaneTerm::new(40, 9, 2000);
        for i in 1..=30 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        let mut m = enter(&t, false);
        cmd(&mut m, "page-up");
        cmd(&mut m, "cursor-up");
        assert_eq!((m.scroll_position(), m.cursor()), (7, (6, 7)));
        assert_eq!(m.cursor_line(), "line23");
        t.resize(40, 4);
        m.resize(40, 4);
        assert_eq!((m.scroll_position(), m.cursor()), (5, (6, 0)));
        assert_eq!(m.cursor_line(), "line23");
        m.resize(40, 9);
        assert_eq!((m.scroll_position(), m.cursor()), (0, (6, 0)));
        assert_eq!(m.cursor_line(), "line23");
    }

    #[test]
    fn cursor_word_follows_separators() {
        let mut t = PaneTerm::new(30, 3, 100);
        t.feed(b"foo bar-baz qux");
        let mut m = enter(&t, false);
        m.cx = 5;
        m.cy = 0;
        assert_eq!(m.cursor_word("-"), "bar");
        m.cx = 8;
        assert_eq!(m.cursor_word("-"), "baz");
        assert_eq!(m.cursor_word(""), "bar-baz");
        // Past the end of the line: a blank, so no word.
        m.cx = 30;
        assert_eq!(m.cursor_word(""), "");
    }
}
