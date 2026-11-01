//! Copy mode search (window-copy.c:4017-5128): plain and regular expression
//! matching over the copy's grid, the jump to a match, and the marks that
//! highlight the matches in view.

use std::time::{Duration, Instant};

use vtmux_core::regex::Regex;

use super::super::grid::Grid;
use super::{CopyMode, Env};

/// `WINDOW_COPY_SEARCH_TIMEOUT`: a search taking longer gives up.
const SEARCH_TIMEOUT: Duration = Duration::from_millis(10_000);
/// `WINDOW_COPY_SEARCH_ALL_TIMEOUT`: marking all of history taking longer
/// marks only the lines in view.
const SEARCH_ALL_TIMEOUT: Duration = Duration::from_millis(200);
/// `WINDOW_COPY_SEARCH_MAX_LINE`: how far a regular expression follows
/// wrapped lines.
const SEARCH_MAX_LINE: usize = 2000;

/// Which way the last search went (`searchtype`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchKind {
    #[default]
    Off,
    Up,
    Down,
}

/// The search state (`data->search*`).
#[allow(clippy::struct_excessive_bools)] // tmux's independent flags
#[derive(Clone, Debug, Default)]
pub struct Search {
    pub kind: SearchKind,
    /// The next `search-again` is a regular expression (`searchregex`).
    pub regex: bool,
    /// `searchstr`.
    pub string: Option<String>,
    /// The last search went down (`searchdirection`).
    pub down: bool,
    /// Where an incremental search started: cursor and offset (`searchx`,
    /// `searchy`, `searcho`).
    pub origin: Option<(usize, usize, usize)>,
    /// The next search marks all of history (`searchall`).
    pub all: bool,
    /// One mark per cell in view, 0 for none, else the match's generation
    /// (`searchmark`, `searchgen`).
    pub marks: Option<Vec<u8>>,
    pub generation: u8,
    /// `searchcount`, -1 for unknown, and `searchmore`.
    pub count: i64,
    pub more: bool,
    /// `data->timeout`.
    pub timeout: bool,
}

/// The search string written into a one-line screen (`screen_write_nputs`):
/// a cell per character, a padding cell after a wide one.
pub(super) struct Pattern {
    cells: Vec<(String, u8)>,
    /// The string read back from those cells (`window_copy_stringify`).
    text: String,
    /// `window_copy_is_lowercase`: match ignoring case.
    cis: bool,
}

impl Pattern {
    fn new(s: &str) -> Self {
        let mut cells: Vec<(String, u8)> = Vec::new();
        for c in s.chars() {
            if c.is_ascii() {
                // Only tabs and printable characters are written.
                if c == '\t' || (' '..='~').contains(&c) {
                    cells.push((c.to_string(), 1));
                }
                continue;
            }
            match vtmux_core::width::char_width(c) {
                0 => {
                    if let Some(last) = cells.iter_mut().rev().find(|(_, w)| *w != 0) {
                        last.0.push(c);
                    }
                }
                w => {
                    cells.push((c.to_string(), w));
                    if w == 2 {
                        cells.push((String::new(), 0));
                    }
                }
            }
        }
        let text = cells.iter().map(|(t, _)| t.as_str()).collect();
        Self {
            cells,
            text,
            cis: !s.bytes().any(|b| b.is_ascii_uppercase()),
        }
    }

    /// `regcomp(REG_EXTENDED)`, `REG_ICASE` when matching ignoring case.
    fn regex(&self) -> Option<Regex> {
        Regex::new(&self.text, self.cis).ok()
    }
}

/// tmux's `"^$*+()?[].\\"`: a search with none of these is plain text.
fn has_regex_chars(s: &str) -> bool {
    s.chars().any(|c| "^$*+()?[].\\".contains(c))
}

impl Grid {
    #[must_use]
    pub fn sy(&self) -> usize {
        self.last() + 1 - self.hsize()
    }

    /// A cell's data and width as `grid_get_cell` gives them: a space for
    /// a blank, nothing for padding.
    fn data(&self, x: usize, y: usize) -> (&str, u8) {
        match self.row(y).cells.get(x) {
            None => (" ", 1),
            Some(c) if c.width == 0 => ("", 0),
            Some(c) if c.text.is_empty() => (" ", 1),
            Some(c) => (&c.text, c.width),
        }
    }

    /// `window_copy_search_compare`.
    fn compare(&self, x: usize, y: usize, (text, width): &(String, u8), cis: bool) -> bool {
        let (d, w) = self.data(x, y);
        if d.len() != text.len() || w != *width {
            return false;
        }
        if cis && d.len() == 1 {
            return d.as_bytes()[0].to_ascii_lowercase() == text.as_bytes()[0];
        }
        d == text
    }

    /// Whether `pat` matches at column `ax` of line `py`, following wrapped
    /// lines.
    fn matches_at(&self, pat: &Pattern, ax: usize, py: usize) -> bool {
        let (sx, endline) = (self.sx(), self.last());
        pat.cells.iter().enumerate().all(|(bx, cell)| {
            let (mut px, mut y) = (ax + bx, py);
            while px >= sx && y < endline && self.wrapped(y) {
                px -= sx;
                y += 1;
            }
            px < sx && self.compare(px, y, cell, pat.cis)
        })
    }

    /// `window_copy_search_lr`: the first match starting in
    /// `first..last` of line `py`.
    fn search_lr(&self, pat: &Pattern, py: usize, first: usize, last: usize) -> Option<usize> {
        (first..last).find(|&ax| self.matches_at(pat, ax, py))
    }

    /// `window_copy_search_rl`: the last one.
    fn search_rl(&self, pat: &Pattern, py: usize, first: usize, last: usize) -> Option<usize> {
        (first..last).rev().find(|&ax| self.matches_at(pat, ax, py))
    }

    /// `window_copy_stringify`: columns `first..last` of line `py`.
    fn stringify(&self, buf: &mut String, py: usize, first: usize, last: usize) {
        if py > self.last() {
            return;
        }
        for x in first..last {
            buf.push_str(self.data(x, py).0);
        }
    }

    /// The text of line `py` from `first` on, with the lines it wraps onto,
    /// and how many cells that is.
    fn wrapped_text(&self, py: usize, first: usize) -> (String, usize) {
        let sx = self.sx();
        let mut buf = String::new();
        self.stringify(&mut buf, py, first, sx);
        let mut len = sx - first;
        let mut y = py;
        while y < self.last() && len < SEARCH_MAX_LINE && self.wrapped(y) {
            y += 1;
            self.stringify(&mut buf, y, 0, sx);
            len += sx;
        }
        (buf, len)
    }

    /// `window_copy_cstrtocellpos`: the cell, of the `ncells` from
    /// `(px, py)` on, where the text `s` starts; one past the end if none.
    fn cell_of(&self, ncells: usize, (px, py): (usize, usize), s: &[u8]) -> (usize, usize) {
        let sx = self.sx();
        if py > self.last() {
            return (px, py);
        }
        let mut cells = Vec::with_capacity(ncells);
        let (mut x, mut y) = (px, py);
        while cells.len() < ncells {
            cells.push(self.data(x, y).0.as_bytes());
            x += 1;
            if x >= sx {
                x = 0;
                y += 1;
                if y > self.last() {
                    break;
                }
            }
        }
        // As tmux: the first cell from which every cell matches the text,
        // the text not running out first.
        let starts_here = |cell: usize| {
            let mut pos = 0;
            for d in &cells[cell..] {
                if pos >= s.len() {
                    return false;
                }
                let n = if d.len() == 1 {
                    1
                } else {
                    d.len().min(s.len() - pos)
                };
                if s[pos..pos + n] != d[..n] {
                    return false;
                }
                pos += n;
            }
            true
        };
        let cell = (0..cells.len())
            .find(|&c| starts_here(c))
            .unwrap_or(cells.len());
        let (mut x, mut y) = (px + cell, py);
        while x >= sx {
            x -= sx;
            y += 1;
        }
        (x, y)
    }

    /// The column count from `(px, py)` to `(ex, ey)`.
    fn span(&self, px: usize, py: usize, (ex, ey): (usize, usize)) -> usize {
        (ex + (ey - py) * self.sx()).saturating_sub(px)
    }

    /// `window_copy_search_lr_regex`: the first match starting in
    /// `first..last` of line `py`, and its width.
    fn search_lr_regex(
        &self,
        re: &Regex,
        py: usize,
        first: usize,
        last: usize,
    ) -> Option<(usize, usize)> {
        if first >= last {
            return None;
        }
        let (buf, len) = self.wrapped_text(py, first);
        let m = re.find_with(&buf, first != 0).ok()??;
        let range = m.captures[0].clone()?;
        if range.is_empty() {
            return None;
        }
        let (fx, fy) = self.cell_of(len, (first, py), &buf.as_bytes()[range.start..]);
        if fy != py || fx >= last {
            return None;
        }
        let end = self.cell_of(len - (fx - first), (fx, fy), &buf.as_bytes()[range.end..]);
        Some((fx, self.span(fx, py, end)))
    }

    /// `window_copy_search_rl_regex` and `window_copy_last_regex`: the last
    /// match starting in `first..last` of line `py`, and its width. As
    /// tmux, every match after the first is looked for with the first's
    /// `REG_NOTBOL`.
    fn search_rl_regex(
        &self,
        re: &Regex,
        py: usize,
        first: usize,
        last: usize,
    ) -> Option<(usize, usize)> {
        let (buf, mut len) = self.wrapped_text(py, first);
        let bytes = buf.as_bytes();
        let (mut found, mut oldx, mut px) = ((first, py), first, 0);
        let (mut save_x, mut save_width) = (0, 0);
        while let Ok(Some(m)) = re.find_with(&buf[px..], first != 0) {
            let Some(range) = m.captures[0].clone() else {
                break;
            };
            if range.is_empty() {
                break;
            }
            found = self.cell_of(len, found, &bytes[px + range.start..]);
            if found.1 > py || found.0 >= last {
                break;
            }
            len = len.saturating_sub(found.0 - oldx);
            save_x = found.0;
            found = self.cell_of(len, found, &bytes[px + range.end..]);
            if found.1 > py || found.0 >= last {
                return Some((save_x, self.span(save_x, py, found)));
            }
            save_width = found.0 - save_x;
            len = len.saturating_sub(save_width);
            oldx = found.0;
            px += range.end;
        }
        (save_width > 0).then_some((save_x, save_width))
    }

    /// `window_copy_search_back_overlap`: a match found backwards at the
    /// start of a wrapped line may begin on the lines before; `(px, sx)`
    /// and `py` (one past the line) move to its start.
    fn search_back_overlap(
        &self,
        re: &Regex,
        (px0, sx0): (usize, usize),
        py0: usize,
        endline: usize,
    ) -> (usize, usize) {
        let sx = self.sx();
        let wrap = |mut x: usize, mut y: usize| {
            while x > sx - 1 {
                x -= sx;
                y += 1;
            }
            (x, y)
        };
        let old = wrap(px0 + sx0, py0 - 1);
        let (mut ppx, mut ppy) = (px0, py0);
        let (mut px, mut py, mut end) = (px0, py0, old);
        while px == 0 && py - 1 > endline && self.wrapped(py - 2) && end == old {
            py -= 1;
            let Some((x, w)) = self.search_rl_regex(re, py - 1, 0, sx) else {
                break;
            };
            px = x;
            end = wrap(x + w, py - 1);
            if end == old {
                (ppx, ppy) = (px, py);
            }
        }
        (ppx, ppy)
    }

    /// `window_copy_search_jump` without the jump: where the search for
    /// `pat` from `(fx, fy)` towards `endline` finds a match, wrapping
    /// round the grid once if `wrap`.
    #[allow(clippy::too_many_arguments)]
    fn search_from(
        &self,
        pat: &Pattern,
        re: Option<&Regex>,
        (mut fx, fy): (usize, usize),
        endline: usize,
        wrap: bool,
        down: bool,
    ) -> Option<(usize, usize)> {
        let sx = self.sx();
        let mut found = None;
        if down {
            for i in fy..=endline {
                found = match re {
                    Some(re) => self.search_lr_regex(re, i, fx, sx).map(|(px, _)| (px, i)),
                    None => self.search_lr(pat, i, fx, sx).map(|px| (px, i)),
                };
                if found.is_some() {
                    break;
                }
                fx = 0;
            }
        } else {
            let mut i = fy + 1;
            while endline < i {
                found = match re {
                    Some(re) => self
                        .search_rl_regex(re, i - 1, 0, fx + 1)
                        .map(|m| self.search_back_overlap(re, m, i, endline))
                        .map(|(px, py)| (px, py - 1)),
                    None => self.search_rl(pat, i - 1, 0, fx + 1).map(|px| (px, i - 1)),
                };
                if found.is_some() {
                    break;
                }
                fx = sx - 1;
                i -= 1;
            }
        }
        if found.is_some() || !wrap {
            return found;
        }
        let start = if down { (0, 0) } else { (sx - 1, self.last()) };
        self.search_from(pat, re, start, fy, false, down)
    }
}

impl CopyMode {
    /// `window_copy_search_mark_at`: the index of `(px, py)` in the marks,
    /// `None` off the screen.
    fn mark_at(&self, px: usize, py: usize) -> Option<usize> {
        let top = self.hsize() - self.oy;
        let sy = self.backing.sy();
        (py >= top && py < top + sy).then(|| (py - top) * self.backing.sx() + px)
    }

    fn mark(&self, at: usize) -> u8 {
        self.search
            .marks
            .as_ref()
            .and_then(|m| m.get(at))
            .copied()
            .unwrap_or(0)
    }

    /// `window_copy_move_left` on the grid.
    fn move_left(&self, (x, y): (usize, usize), wrap: bool) -> (usize, usize) {
        let sx = self.backing.sx();
        match (x, y) {
            (0, 0) if wrap => (sx - 1, self.last_row()),
            (0, 0) => (0, 0),
            (0, y) => (sx - 1, y - 1),
            (x, y) => (x - 1, y),
        }
    }

    /// `window_copy_move_right` on the grid.
    fn move_right(&self, (x, y): (usize, usize), wrap: bool) -> (usize, usize) {
        let sx = self.backing.sx();
        if x == sx - 1 {
            if y == self.last_row() {
                return if wrap { (0, 0) } else { (x, y) };
            }
            return (0, y + 1);
        }
        (x + 1, y)
    }

    /// `window_copy_move_after_search_mark`: past the mark under `pos`.
    fn after_mark(&self, mut pos: (usize, usize), wrap: bool) -> (usize, usize) {
        let Some(start) = self.mark_at(pos.0, pos.1) else {
            return pos;
        };
        let id = self.mark(start);
        if id == 0 {
            return pos;
        }
        while let Some(at) = self.mark_at(pos.0, pos.1) {
            if self.mark(at) != id {
                break;
            }
            if !wrap && pos.0 == self.backing.sx() - 1 && pos.1 == self.last_row() {
                break;
            }
            pos = self.move_right(pos, wrap);
        }
        pos
    }

    /// The cursor's grid position.
    fn grid_cursor(&self) -> (usize, usize) {
        (self.cx, self.abs(self.cy))
    }

    /// Put the cursor at grid position `(x, y)` without scrolling.
    fn set_grid_cursor(&mut self, (x, y): (usize, usize)) {
        self.cx = x;
        self.cy = y + self.oy - self.hsize();
    }

    /// `window_copy_clear_marks`.
    pub(super) fn clear_marks(&mut self) {
        self.search.count = -1;
        self.search.more = false;
        self.search.marks = None;
    }

    /// The marks follow the view (`window_copy_search_marks(..., 1)` after
    /// a scroll).
    pub(super) fn refresh_marks(&mut self) {
        if self.search.marks.is_some() && !self.search.timeout {
            self.search_marks(None, self.search.regex, true);
        }
    }

    /// `window_copy_search` (window-copy.c:4667-4807): search up or down
    /// for `searchstr`, then mark the matches. Whether one was found.
    pub(super) fn search(&mut self, env: &Env, down: bool, mut regex: bool) -> bool {
        let Some(s) = self.search.string.clone() else {
            return false;
        };
        if regex && !has_regex_chars(&s) {
            regex = false;
        }
        self.search.down = down;
        if self.search.timeout {
            return false;
        }
        let visible_only =
            if self.search.all || self.pane_search.as_ref().is_none_or(|(_, r)| *r != regex) {
                self.search.all = false;
                false
            } else {
                self.pane_search.as_ref().is_some_and(|(p, _)| *p == s)
            };
        if !visible_only && self.search.marks.is_some() {
            self.clear_marks();
        }
        self.pane_search = Some((s.clone(), regex));

        let pat = Pattern::new(&s);
        if pat.cells.is_empty() {
            return false;
        }
        let wrap = env.wrap_search;
        let mut from = self.grid_cursor();
        let endline = if down {
            // Emacs keys leave the cursor after a match, vi keys on it: vi
            // starts after the match under the cursor.
            if env.vi {
                from = if self.search.marks.is_some() {
                    self.after_mark(from, wrap)
                } else {
                    self.move_right(from, wrap)
                };
            }
            self.last_row()
        } else {
            from = self.move_left(from, wrap);
            0
        };
        // regcomp failing finds nothing.
        let re = if regex {
            match pat.regex() {
                Some(re) => Some(re),
                None => return false,
            }
        } else {
            None
        };
        let Some((px, py)) = self
            .backing
            .search_from(&pat, re.as_ref(), from, endline, wrap, down)
        else {
            return false;
        };
        self.scroll_to_pos(env, px, py, true);
        self.search_marks(Some(&pat), regex, visible_only);
        let mut at_cursor = self.grid_cursor();
        // Searching down from inside a match: on to the next.
        if down
            && let Some(at) = self.mark_at(at_cursor.0, at_cursor.1)
            && at > 0
            && self.search.marks.is_some()
            && self.mark(at) == self.mark(at - 1)
        {
            let from = self.after_mark(at_cursor, wrap);
            if let Some((px, py)) =
                self.backing
                    .search_from(&pat, re.as_ref(), from, endline, wrap, down)
            {
                self.scroll_to_pos(env, px, py, true);
            }
            at_cursor = self.grid_cursor();
        }
        if down {
            if !env.vi {
                let after = self.after_mark(at_cursor, wrap);
                self.set_grid_cursor(after);
            }
        } else if let Some(start) = self.mark_at(at_cursor.0, at_cursor.1) {
            // Back to the start of the match.
            let id = self.mark(start);
            let mut pos = at_cursor;
            while let Some(at) = self.mark_at(pos.0, pos.1) {
                if self.search.marks.is_none() || self.mark(at) != id {
                    break;
                }
                self.set_grid_cursor(pos);
                if at == 0 {
                    break;
                }
                pos = self.move_left(pos, false);
            }
        }
        true
    }

    /// `window_copy_visible_lines`: the lines in view, from the start of
    /// the wrapped line at the top.
    fn visible_lines(&self) -> (usize, usize) {
        let top = self.hsize() - self.oy;
        let mut start = top;
        while start > 0 && self.backing.wrapped(start - 1) {
            start -= 1;
        }
        (start, top + self.backing.sy())
    }

    /// `window_copy_search_mark_match`: mark `width` cells from
    /// `(px, py)` with the next generation; how many cells it covers.
    fn mark_match(&mut self, px: usize, py: usize, width: usize) -> usize {
        let Some(b) = self.mark_at(px, py) else {
            return width;
        };
        let total = self.backing.sx() * self.backing.sy();
        let w = width.min(total.saturating_sub(b));
        let generation = self.search.generation;
        if let Some(marks) = &mut self.search.marks {
            for m in marks.iter_mut().skip(b).take(w) {
                if *m == 0 {
                    *m = generation;
                }
            }
        }
        self.search.generation = if generation == u8::MAX {
            1
        } else {
            generation + 1
        };
        w
    }

    /// `window_copy_search_marks`: mark the matches in view, counting them
    /// over all of history unless `visible_only`. False if the expression
    /// does not compile.
    pub(super) fn search_marks(
        &mut self,
        pat: Option<&Pattern>,
        regex: bool,
        visible_only: bool,
    ) -> bool {
        let owned;
        let pat = if let Some(p) = pat {
            p
        } else {
            owned = Pattern::new(self.search.string.as_deref().unwrap_or_default());
            &owned
        };
        let re = if regex {
            let Some(re) = pat.regex() else {
                self.search.marks = None;
                return false;
            };
            Some(re)
        } else {
            None
        };
        let (sx, sy) = (self.backing.sx(), self.backing.sy());
        let start_time = Instant::now();
        let (mut start, mut end, mut stop) = if visible_only {
            let (s, e) = self.visible_lines();
            (s, e, None)
        } else {
            (0, self.hsize() + sy, Some(start_time + SEARCH_ALL_TIMEOUT))
        };
        let mut nfound = 0usize;
        let mut stopped = false;
        loop {
            self.search.marks = Some(vec![0; sx * sy]);
            self.search.generation = 1;
            for py in start..end {
                let mut px = 0;
                loop {
                    let found = match &re {
                        Some(re) => self.backing.search_lr_regex(re, py, px, sx),
                        None => self
                            .backing
                            .search_lr(pat, py, px, sx)
                            .map(|x| (x, pat.cells.len())),
                    };
                    let Some((x, width)) = found else { break };
                    nfound += 1;
                    let w = self.mark_match(x, py, width);
                    if w == 0 {
                        break;
                    }
                    px = x + w;
                }
                let now = Instant::now();
                if now - start_time > SEARCH_TIMEOUT {
                    self.search.timeout = true;
                    break;
                }
                if stop.is_some_and(|s| now > s) {
                    stopped = true;
                    break;
                }
            }
            if self.search.timeout {
                self.clear_marks();
                return true;
            }
            if stopped && stop.is_some() {
                // Too slow for all of history: just what is in view.
                (start, end) = self.visible_lines();
                stop = None;
                continue;
            }
            break;
        }
        if !visible_only {
            if stopped {
                self.search.count = match nfound {
                    n if n > 1000 => 1000,
                    n if n > 100 => 100,
                    n if n > 10 => 10,
                    _ => -1,
                };
                self.search.more = true;
            } else {
                self.search.count = i64::try_from(nfound).unwrap_or(i64::MAX);
                self.search.more = false;
            }
        }
        true
    }

    /// `window_copy_match_start_end`: the run of cells with the mark at
    /// `at`.
    fn match_start_end(&self, at: usize) -> (usize, usize) {
        let last = self.backing.sx() * self.backing.sy() - 1;
        let id = self.mark(at);
        let (mut start, mut end) = (at, at);
        while start != 0 && self.mark(start) == id {
            start -= 1;
        }
        if self.mark(start) != id {
            start += 1;
        }
        while end != last && self.mark(end) == id {
            end += 1;
        }
        if self.mark(end) != id {
            end -= 1;
        }
        (start, end)
    }

    /// `#{search_match}` (`window_copy_match_at_cursor`): the match under
    /// the cursor or just before it.
    #[must_use]
    pub fn search_match(&self) -> String {
        self.search_match_at().unwrap_or_default()
    }

    pub(super) fn search_match_at(&self) -> Option<String> {
        self.search.marks.as_ref()?;
        let (cx, cy) = self.grid_cursor();
        let mut at = self.mark_at(cx, cy)?;
        if self.mark(at) == 0 {
            if at == 0 || self.mark(at - 1) == 0 {
                return None;
            }
            at -= 1;
        }
        let (start, end) = self.match_start_end(at);
        let sx = self.backing.sx();
        let top = self.hsize() - self.oy;
        let mut buf = String::new();
        for at in start..=end {
            let (x, y) = (at % sx, top + at / sx);
            if !self.backing.padding(x, y) {
                buf.push_str(self.backing.data(x, y).0);
            }
        }
        Some(buf)
    }

    /// How a cell in view is drawn for the search (`window_copy_update_style`
    /// after the mark): `None` outside a match, else whether it is in the
    /// current match. `vi` is `mode-keys` now.
    pub(super) fn match_paint(&self, fx: usize, fy: usize, vi: bool) -> Option<bool> {
        self.search.marks.as_ref()?;
        let current = self.mark_at(fx, fy)?;
        let id = self.mark(current);
        if id == 0 {
            return None;
        }
        let (cx, cy) = self.grid_cursor();
        if let Some(mut cursor) = self.mark_at(cx, cy) {
            let found = if cursor != 0 && !vi && self.search.down {
                let f = self.mark(cursor - 1) == id;
                if f {
                    cursor -= 1;
                }
                f
            } else {
                self.mark(cursor) == id
            };
            if found {
                let (start, end) = self.match_start_end(cursor);
                if (start..=end).contains(&current) {
                    return Some(true);
                }
            }
        }
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtmux_vt::PaneTerm;

    fn grid(text: &str, cols: u16) -> Grid {
        let mut t = PaneTerm::new(cols, 4, 100);
        t.feed(text.as_bytes());
        Grid::clone_of(&t, usize::from(cols), 4, false).0
    }

    #[test]
    fn plain_search_follows_wrapped_lines_and_case() {
        let g = grid("abcdefgh\r\nxyz ABC", 6);
        // abcdef / gh (wrapped) / xyz ABC
        let pat = Pattern::new("fg");
        assert_eq!(g.search_lr(&pat, 0, 0, 6), Some(5));
        let pat = Pattern::new("abc");
        assert_eq!(g.search_rl(&pat, 2, 0, 6), Some(4));
        assert!(Pattern::new("abc").cis && !Pattern::new("Abc").cis);
        assert_eq!(g.search_lr(&Pattern::new("Abc"), 2, 0, 6), None);
    }

    #[test]
    fn regex_positions_are_cells() {
        let g = grid("a\u{4e16}bc bc", 12);
        let re = Regex::new("b+c", false).unwrap();
        // a 世(2 cells) b c ' ' b c. As tmux, `window_copy_cstrtocellpos`
        // matches 世's padding cell as empty text, so the first match
        // starts there (tmux 3.8: search-forward 'b+c' from the start
        // leaves emacs keys' cursor at 5).
        assert_eq!(g.search_lr_regex(&re, 0, 0, 12), Some((2, 3)));
        assert_eq!(g.search_rl_regex(&re, 0, 0, 12), Some((6, 2)));
        assert_eq!(g.search_lr_regex(&re, 0, 4, 12), Some((6, 2)));
        // ^ does not match after the start of the line.
        let re = Regex::new("^b", false).unwrap();
        assert_eq!(g.search_lr_regex(&re, 0, 3, 12), None);
    }
}
