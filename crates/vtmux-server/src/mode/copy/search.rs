//! Copy mode search (window-copy.c:4017-5128): plain and regular expression
//! matching over the copy's grid, the jump to a match, and the marks that
//! highlight the matches in view.

use std::time::{Duration, Instant};

use vtmux_core::regex::Regex;
use vtmux_vt::{Cell, CellText};

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
    /// The bytes of `text` when every cell is one character of one column
    /// other than a tab, and there is at least one: the pattern a line
    /// read as text ([`Grid::text`]) is searched for in.
    bytes: Option<Vec<u8>>,
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
        let text: String = cells.iter().map(|(t, _)| t.as_str()).collect();
        let narrow = !cells.is_empty()
            && cells.iter().all(|(t, w)| {
                let mut chars = t.chars();
                *w == 1 && chars.next().is_some_and(|c| c != '\t') && chars.next().is_none()
            });
        Self {
            bytes: narrow.then(|| text.clone().into_bytes()),
            cells,
            text,
            cis: is_lowercase(s),
        }
    }

    /// Whether a line read as text ([`Grid::text`], every cell one
    /// character of one column or a blank; `ascii` when every one is
    /// ASCII) cannot hold this pattern as plain text: some cell of it is a
    /// tab, a padding cell or a cluster, or, in an ASCII line, a character
    /// past ASCII, which such a line has no cell to match
    /// (`window_copy_search_compare`).
    fn never_in_text(&self, ascii: bool) -> bool {
        !self.cells.is_empty() && self.bytes.as_ref().is_none_or(|b| ascii && !b.is_ascii())
    }

    /// `regcomp(REG_EXTENDED)`, `REG_ICASE` when matching ignoring case.
    fn regex(&self) -> Option<Regex> {
        Regex::new(&self.text, self.cis).ok()
    }
}

/// A cell's string and width as `window_copy_cellstring` gives them: a
/// space for a blank or past the line's end, `\t` for a TAB cell, nothing
/// for padding.
fn cell_data(cell: Option<&Cell>) -> (&str, u8) {
    match cell {
        None => (" ", 1),
        Some(c) if c.width == 0 => ("", 0),
        Some(c) if c.tab => ("\t", c.width),
        Some(c) if c.text.is_empty() => (" ", 1),
        Some(c) => (&c.text, c.width),
    }
}

/// `window_copy_is_lowercase` (window-copy.c:4509-4518): `*ptr !=
/// tolower((u_char)*ptr)` compares a `char`, which is signed on x86-64 and
/// on macOS, with an unsigned value, so a byte past ASCII is never equal to
/// itself and a search string with a character past ASCII is matched with
/// case (`é+` does not match `É`).
fn is_lowercase(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase())
}

/// Tests turn the paths that read a line's text, not its cells, off to
/// compare them with the cell path.
fn text_paths_off() -> bool {
    #[cfg(test)]
    return tests::NO_TEXT.with(std::cell::Cell::get);
    #[cfg(not(test))]
    false
}

/// Whether the pattern's bytes `pat` ([`Pattern::bytes`]) match the line
/// text `text` at byte `at`, the start of a cell, entirely inside it:
/// [`Grid::matches_at`] on a line read as text ([`Grid::text`]) that does
/// not wrap, ignoring case as `window_copy_search_compare` does when `cis`
/// (only an ASCII pattern is matched so). The text is UTF-8, so bytes that
/// match from a character's start are whole characters, a cell each.
fn text_matches(text: &[u8], pat: &[u8], cis: bool, at: usize) -> bool {
    text.get(at..at + pat.len()).is_some_and(|t| {
        t.iter().zip(pat).all(|(&c, &p)| {
            if cis {
                c.to_ascii_lowercase() == p
            } else {
                c == p
            }
        })
    })
}

/// The first cell in `cells` (the last with `rev`) of a line read as text,
/// `text` (a character a cell, `sx` of them), at which the pattern's bytes
/// `pat` match ([`text_matches`]), entirely inside the line.
fn text_find(
    text: &str,
    pat: &[u8],
    cis: bool,
    sx: usize,
    cells: std::ops::RangeInclusive<usize>,
    rev: bool,
) -> Option<usize> {
    let bytes = text.as_bytes();
    let at = |b: usize| {
        #[cfg(test)]
        tests::TEXT_SEARCHED.with(|n| n.set(n.get() + 1));
        text_matches(bytes, pat, cis, b)
    };
    if bytes.len() == sx {
        // ASCII: a byte a cell.
        return if rev {
            cells.rev().find(|&x| at(x))
        } else {
            cells.into_iter().find(|&x| at(x))
        };
    }
    // A match starts at a character's first byte: a byte inside one is
    // past ASCII and not a first byte, which the pattern's first byte is.
    let byte = |x: usize| text.char_indices().nth(x).map_or(bytes.len(), |(b, _)| b);
    let (lo, hi) = (byte(*cells.start()), byte(*cells.end()));
    let found = if rev {
        (lo..=hi).rev().find(|&b| at(b))
    } else {
        (lo..=hi).find(|&b| at(b))
    }?;
    Some(text[..found].chars().count())
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

    /// [`cell_data`] of the cell at `(x, y)`.
    fn data(&self, x: usize, y: usize) -> (CellText, u8) {
        let row = self.peek(y);
        let (text, width) = cell_data(row.cells.get(x));
        (text.into(), width)
    }

    /// `window_copy_search_compare`: a TAB cell matches a tab in the
    /// pattern; otherwise the cell's own data is compared (the spaces of a
    /// TAB cell).
    fn compare(&self, x: usize, y: usize, (text, width): &(String, u8), cis: bool) -> bool {
        let row = self.peek(y);
        let cell = row.cells.get(x);
        if text == "\t" && cell.is_some_and(|c| c.tab) {
            return true;
        }
        let (d, w) = match cell {
            Some(c) if c.width != 0 && c.tab => (c.text.as_str(), c.width),
            _ => cell_data(cell),
        };
        if d.len() != text.len() || w != *width {
            return false;
        }
        if cis && d.len() == 1 {
            return d.as_bytes()[0].to_ascii_lowercase() == text.as_bytes()[0];
        }
        d == text
    }

    /// Whether `pat` matches at column `ax` of line `py`, following wrapped
    /// lines. A TAB cell in the text shifts the columns after it by the
    /// padding it brings (`window_copy_search_lr`).
    fn matches_at(&self, pat: &Pattern, ax: usize, py: usize) -> bool {
        let (sx, endline) = (self.sx(), self.last());
        let mut padding = 0;
        for (bx, cell) in pat.cells.iter().enumerate() {
            let (mut px, mut y) = (ax + bx + padding, py);
            while px >= sx && y < endline && self.peek(y).wrapped {
                px -= sx;
                y += 1;
            }
            if px - padding.min(px) >= sx {
                return false;
            }
            if let Some(c) = self.peek(y).cells.get(px)
                && c.tab
            {
                padding += usize::from(c.width) - 1;
            }
            if !self.compare(px, y, cell, pat.cis) {
                return false;
            }
        }
        true
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
        let row = self.peek(py);
        for x in first..last {
            buf.push_str(cell_data(row.cells.get(x)).0);
        }
    }

    /// The text of line `py` from `first` on, with the lines it wraps onto,
    /// and how many cells that is.
    fn wrapped_text(&self, py: usize, first: usize) -> (String, usize) {
        let sx = self.sx();
        let mut buf = String::with_capacity(sx);
        self.stringify(&mut buf, py, first, sx);
        let mut len = sx - first;
        let mut y = py;
        while y < self.last() && len < SEARCH_MAX_LINE && self.peek(y).wrapped {
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
        // The lines the cells are on, then the cells.
        let rows: Vec<_> = (py..=self.last().min(py + (px + ncells).div_ceil(sx)))
            .map(|y| self.peek(y))
            .collect();
        let mut cells: Vec<&[u8]> = Vec::with_capacity(ncells);
        let (mut x, mut y) = (px, 0);
        while cells.len() < ncells {
            cells.push(cell_data(rows[y].cells.get(x)).0.as_bytes());
            x += 1;
            if x >= sx {
                x = 0;
                y += 1;
                if y == rows.len() {
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
        while px == 0 && py - 1 > endline && self.peek(py - 2).wrapped && end == old {
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
        // Most of a long history is lines of characters of one column
        // (plain or coloured log lines), and reading one as cells costs
        // many times what looking at its text does: a line of that kind
        // the pattern cannot match is passed unread. For a plain pattern
        // that is every one of them that has no cell to match one of the
        // pattern's ([`Pattern::never_in_text`]); for a regular expression
        // it is a line its text has no match in (the whole line, so not
        // the first one searched from the middle going down: the text
        // starting at the cursor can match what the whole does not, as for
        // `\<`).
        let skip_text = !text_paths_off();
        // A plain pattern of such characters is looked for in such a
        // line's text, which finds what comparing its cells finds
        // (`window_copy_search_compare` compares a cell's character, a
        // blank as a space, and only ASCII ignoring case) at a fraction of
        // the cost: a line that does not wrap holds a match only inside it
        // (`window_copy_search_lr` fails one that runs past the last
        // column).
        let plain = re.is_none().then_some(pat.bytes.as_deref()).flatten();
        let mut text = String::new();
        // Whether line `y` is passed unread, and whether its text in `text`
        // is the whole of it (read as text, not wrapped).
        let read = |y: usize, whole: bool, text: &mut String| {
            let is_text = skip_text && self.text(y, text) == Some(false);
            let skip = is_text
                && match re {
                    None => pat.never_in_text(text.len() == sx),
                    Some(re) => whole && matches!(re.span_with(text, false), Ok(None)),
                };
            #[cfg(test)]
            if skip {
                tests::TEXT_SKIPPED.with(|n| n.set(n.get() + 1));
            }
            (skip, is_text)
        };
        let find_in_text = |text: &str, cells: std::ops::RangeInclusive<usize>, rev: bool| {
            text_find(text, plain?, pat.cis, sx, cells, rev)
        };
        if down {
            for i in fy..=endline {
                let (skip, is_text) = read(i, fx == 0, &mut text);
                if skip {
                    fx = 0;
                    continue;
                }
                found = match re {
                    Some(re) => self.search_lr_regex(re, i, fx, sx).map(|(px, _)| (px, i)),
                    None if is_text && plain.is_some() => {
                        find_in_text(&text, fx..=sx - 1, false).map(|px| (px, i))
                    }
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
                let (skip, is_text) = read(i - 1, true, &mut text);
                if skip {
                    fx = sx - 1;
                    i -= 1;
                    continue;
                }
                found = match re {
                    Some(re) => self
                        .search_rl_regex(re, i - 1, 0, fx + 1)
                        .map(|m| self.search_back_overlap(re, m, i, endline))
                        .map(|(px, py)| (px, py - 1)),
                    None if is_text && plain.is_some() => {
                        find_in_text(&text, 0..=fx, true).map(|px| (px, i - 1))
                    }
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
    /// `(px, py)` with the next generation; how many cells it covers. A
    /// plain-text match over a TAB cell covers all the columns of the tab.
    fn mark_match(&mut self, px: usize, py: usize, width: usize, regex: bool) -> usize {
        let Some(b) = self.mark_at(px, py) else {
            return width;
        };
        let total = self.backing.sx() * self.backing.sy();
        let clip = |w: usize| w.min(total.saturating_sub(b));
        let mut w = clip(width);
        let generation = self.search.generation;
        let mut i = b;
        while i < b + w {
            if !regex {
                let c = self.backing.cell(px + (i - b), py);
                if c.tab {
                    w = clip(w + usize::from(c.width) - 1);
                }
            }
            if let Some(m) = self.search.marks.as_mut().and_then(|m| m.get_mut(i))
                && *m == 0
            {
                *m = generation;
            }
            i += 1;
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
        // Lines out of view are only counted, and most are characters of
        // one column: those are counted from their text alone
        // ([`Self::count_text`]).
        let (shown_from, shown_to) = self.visible_lines();
        let mut text = String::new();
        loop {
            self.search.marks = Some(vec![0; sx * sy]);
            self.search.generation = 1;
            for py in start..end {
                if !(shown_from..shown_to).contains(&py)
                    && let Some(n) = self.count_text(re.as_ref(), pat, py, &mut text)
                {
                    nfound += n;
                    let now = Instant::now();
                    if now - start_time > SEARCH_TIMEOUT {
                        self.search.timeout = true;
                        break;
                    }
                    if stop.is_some_and(|s| now > s) {
                        stopped = true;
                        break;
                    }
                    continue;
                }
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
                    let w = self.mark_match(x, py, width, re.is_some());
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
            self.set_count(nfound, stopped);
        }
        true
    }

    /// `searchcount` and `searchmore` once all of history was looked at:
    /// `stopped` when it ran out of time, so only a lower bound is known.
    fn set_count(&mut self, nfound: usize, stopped: bool) {
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

    /// How many matches line `py` has when it is out of view, counted as
    /// the loop of [`Self::search_marks`] counts them but from the line's
    /// text: `None` unless the line reads as text ([`Grid::text`]: every
    /// cell one character of one column or a blank, of any style), does
    /// not wrap, and the search can be told from its bytes (a regular
    /// expression, or a plain pattern of such characters,
    /// [`Pattern::bytes`]). The line's text is then what
    /// `window_copy_stringify` makes of its cells, a character a cell, so
    /// a match found in it from a cell's first byte is the match the cells
    /// give (`window_copy_cstrtocellpos` finds nothing else), and the next
    /// search starts at the byte where the match ends, with `REG_NOTBOL`
    /// (`window_copy_search_lr_regex`), without building the cells.
    fn count_text(
        &self,
        re: Option<&Regex>,
        pat: &Pattern,
        py: usize,
        text: &mut String,
    ) -> Option<usize> {
        if text_paths_off() {
            return None;
        }
        let never = |ascii| re.is_none() && pat.never_in_text(ascii);
        if re.is_none() && pat.bytes.is_none() && !never(false) {
            return None;
        }
        if self.backing.text(py, text)? {
            return None;
        }
        #[cfg(test)]
        tests::TEXT_COUNTED.with(|n| n.set(n.get() + 1));
        if never(text.len() == self.backing.sx()) {
            return Some(0);
        }
        let cis = pat.cis;
        let plain = pat.bytes.as_deref();
        let (mut n, mut px) = (0, 0);
        while px < text.len() {
            let end = if let Some(re) = re {
                match re.span_with(&text[px..], px != 0) {
                    Ok(Some(range)) if !range.is_empty() => px + range.end,
                    _ => break,
                }
            } else {
                let pat = plain?;
                let found = text.as_bytes()[px..].windows(pat.len()).position(|w| {
                    w.iter().zip(pat).all(|(&c, &p)| {
                        if cis {
                            c.to_ascii_lowercase() == p
                        } else {
                            c == p
                        }
                    })
                });
                let Some(at) = found else { break };
                px + at + pat.len()
            };
            n += 1;
            px = end;
        }
        Some(n)
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
                buf.push_str(&self.backing.data(x, y).0);
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
    use crate::mode::copy::CopyMode;
    use vtmux_vt::PaneTerm;

    thread_local! {
        /// Count every line the slow way.
        pub(super) static NO_TEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// Lines counted from their text ([`CopyMode::count_text`]).
        pub(super) static TEXT_COUNTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// Lines the jump passed from their text ([`Grid::search_from`]).
        pub(super) static TEXT_SKIPPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// Columns the jump looked for a plain pattern at in a line's text.
        pub(super) static TEXT_SEARCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// `repeat` blocks of one line of each kind on a 40x6 pane: plain,
    /// coloured and accented (counted from the text), and lines that have
    /// to be read as cells: wide, with a tab, wrapped (a 100 character line
    /// is two full rows and a short one). The second value is how many of
    /// each block's eight lines count from the text.
    fn mixed(repeat: usize) -> (PaneTerm, usize) {
        let mut t = PaneTerm::new(40, 6, 2000);
        for i in 0..repeat {
            let long = format!("{i} {}", "some filler text here ".repeat(5));
            for line in [
                format!("plain line {i} with some filler"),
                format!("caf\u{e9} au lait {i} some filler"),
                format!("\u{4e2d}\u{6587} wide {i} some"),
                format!("\x1b[41mred bg {i} some filler\x1b[0m"),
                format!("tab\there {i} some"),
                long,
                format!("Some CAPS {i} and some more"),
                String::new(),
            ] {
                t.feed(format!("{line}\r\n").as_bytes());
            }
        }
        (t, 5)
    }

    fn count(
        t: &PaneTerm,
        command: &str,
        pattern: &str,
        slow: bool,
    ) -> (Option<(i64, bool)>, usize, (usize, usize)) {
        let env = Env {
            vi: false,
            line_numbers: 0,
            separators: String::new(),
            wrap_search: true,
            mouse: false,
            slider: None,
            dragging: false,
        };
        NO_TEXT.with(|f| f.set(slow));
        let before = TEXT_COUNTED.with(std::cell::Cell::get);
        let size = (usize::from(t.cols()), usize::from(t.rows()));
        let mut m = CopyMode::new(t, size, false, (false, false), false, None);
        m.command(&env, command, &[pattern.to_owned()], false)
            .unwrap();
        let info = m.search_info().2;
        NO_TEXT.with(|f| f.set(false));
        (
            info,
            TEXT_COUNTED.with(std::cell::Cell::get) - before,
            m.cursor(),
        )
    }

    /// Counting a line out of view from its text gives the count the cells
    /// give, for plain text (with and without case) and regular
    /// expressions (anchors, an empty match, a match to the end of the
    /// line), and it is what counts the plain and coloured ASCII lines.
    #[test]
    fn counting_from_text_agrees_with_counting_cells() {
        let (t, fast_lines) = mixed(12);
        let mut taken = 0;
        for (command, pattern) in [
            ("search-backward-text", "some"),
            ("search-backward-text", "Some"),
            ("search-backward-text", "e"),
            ("search-backward-text", " "),
            ("search-backward-text", "filler text"),
            ("search-backward-text", "\u{e9}"),
            // Patterns no ASCII line can hold (a character past ASCII, a
            // wide one, a tab): counted as none, from the text.
            ("search-backward-text", "caf\u{e9}"),
            ("search-backward-text", "\u{e9}t"),
            ("search-backward-text", "\u{4e2d}"),
            ("search-backward-text", "\u{4e2d}\u{6587} wide"),
            ("search-backward-text", "\t"),
            ("search-backward-text", "tab\there"),
            ("search-forward-text", "\u{e9} au"),
            ("search-forward", "\u{e9}+"),
            ("search-forward", "\\<fill"),
            ("search-forward", "^[a-z]+ "),
            ("search-forward", "x*"),
            ("search-forward", "z+$"),
            ("search-backward", "e. au"),
            ("search-forward", "\u{4e2d}"),
            ("search-backward", "^plain"),
            ("search-backward", "^."),
            ("search-backward", "^[a-z]+ "),
            ("search-backward", "some$"),
            ("search-backward", "so.e"),
            ("search-backward", "e+"),
            ("search-backward", " *$"),
            ("search-backward", "x*"),
            ("search-backward", "(some|more)"),
            ("search-backward", "[0-9]+ "),
            ("search-backward", "\\bsome\\b"),
        ] {
            let (want, none, want_at) = count(&t, command, pattern, true);
            assert_eq!(none, 0);
            let (got, n, got_at) = count(&t, command, pattern, false);
            assert_eq!(got, want, "{command} {pattern}");
            assert_eq!(got_at, want_at, "{command} {pattern}: where it jumped");
            assert!(want.is_some_and(|(c, more)| c >= 0 && !more), "{want:?}");
            taken = taken.max(n);
        }
        // Every plain, coloured and accented line out of view was counted
        // from its text (the pane shows the last six).
        assert!(taken >= fast_lines * 12 - 6, "{taken}");
    }

    /// Random styled lines on a pane `cols` wide, most of them characters
    /// of one column (coloured, bold, linked, with characters past ASCII)
    /// and between them every kind a search reads from cells: wide
    /// characters, clusters, tabs, line drawing, lines that wrap.
    fn styled(seed: u64, lines: usize, cols: u16) -> PaneTerm {
        use std::fmt::Write as _;
        let mut x = seed;
        let mut below = |n: u64| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
        };
        let mut t = PaneTerm::new(cols, 6, 2000);
        let mut out = String::new();
        for i in 0..lines {
            for _ in 0..=below(5) {
                let _ = match below(22) {
                    0 => write!(out, "\x1b[3{}m", below(8)),
                    1 => write!(out, "\x1b[1;38;5;{}m", below(256)),
                    2 => write!(out, "\x1b[m"),
                    3 => write!(out, "\x1b[4{}m\x1b[K\x1b[m", below(8)),
                    4 => write!(out, "caf\u{e9} "),
                    5 => write!(out, "Caf\u{e9}s "),
                    6 => write!(out, "\u{2500}\u{2500} "),
                    7 => write!(out, "\u{fc}ber{} ", below(10)),
                    8 => write!(out, "\u{2022}x "),
                    9 => write!(out, "\x1b]8;;http://h/{i}\x1b\\l\u{e9}n\x1b]8;;\x1b\\ "),
                    10 if below(4) == 0 => write!(out, "\u{4e2d}\u{6587} "),
                    11 if below(4) == 0 => write!(out, "e\u{301} "),
                    12 if below(4) == 0 => write!(out, "a\tb "),
                    13 if below(4) == 0 => write!(out, "\x1b(0qx\x1b(B "),
                    14 if below(4) == 0 => write!(out, "{:\u{e9}<1$}", "", usize::from(cols)),
                    _ => write!(out, "w{} ", below(30)),
                };
            }
            out.push_str("\x1b[m\r\n");
        }
        t.feed(out.as_bytes());
        t
    }

    /// The search state after each step: `#{search_count}` and
    /// `#{search_count_partial}`, the cursor and `#{search_match}`.
    type Seen = Vec<(Option<(i64, bool)>, (usize, usize), String)>;

    /// `command pattern`, then `search-again` and `search-reverse` twice
    /// each, from a copy mode entered on `t`; with `slow` every line is
    /// read as cells.
    fn steps(t: &PaneTerm, command: &str, pattern: &str, slow: bool) -> Seen {
        let env = Env {
            vi: false,
            line_numbers: 0,
            separators: String::new(),
            wrap_search: true,
            mouse: false,
            slider: None,
            dragging: false,
        };
        NO_TEXT.with(|f| f.set(slow));
        let size = (usize::from(t.cols()), usize::from(t.rows()));
        let mut m = CopyMode::new(t, size, false, (false, false), false, None);
        let mut seen = Vec::new();
        for (cmd, args) in [
            (command, vec![pattern.to_owned()]),
            ("search-again", vec![]),
            ("search-again", vec![]),
            ("search-reverse", vec![]),
            ("search-reverse", vec![]),
        ] {
            m.command(&env, cmd, &args, false).unwrap();
            seen.push((m.search_info().2, m.cursor(), m.search_match()));
        }
        NO_TEXT.with(|f| f.set(false));
        seen
    }

    /// Lines of styled characters of one column, past ASCII or not, are
    /// searched from their text (task `perf_search_styled_history`: a
    /// coloured log line with an `\u{e9}` was read as cells, which made a
    /// search over 1M of them 3.4x tmux's time), and every search finds,
    /// counts and jumps to what reading every line as cells does: plain
    /// patterns with and without case, past ASCII, wide, clusters, tabs,
    /// and regular expressions, up and down, again and reversed, over
    /// random styled content.
    #[test]
    fn styled_lines_search_from_text_as_from_cells() {
        let patterns = [
            ("search-backward-text", "caf\u{e9}"),
            ("search-forward-text", "caf\u{e9}"),
            ("search-backward-text", "Caf\u{e9}"),
            ("search-backward-text", "caf"),
            ("search-forward-text", "\u{e9} "),
            ("search-backward-text", "\u{e9}"),
            ("search-backward-text", "\u{e9}\u{e9}\u{e9}"),
            ("search-forward-text", "\u{2500}"),
            ("search-backward-text", "\u{2022}x w"),
            ("search-backward-text", "\u{fc}ber1"),
            ("search-forward-text", "w1"),
            ("search-backward-text", "W2"),
            ("search-backward-text", "l\u{e9}n"),
            ("search-backward-text", "\u{4e2d}"),
            ("search-forward-text", "e\u{301}"),
            ("search-backward-text", "\t"),
            ("search-backward-text", "q"),
            ("search-backward-text", "nowhere"),
            ("search-backward", "caf\u{e9}s?"),
            ("search-forward", "\u{e9}+"),
            ("search-backward", "[\u{e9}\u{fc}]"),
            ("search-backward", "^.\u{2500}"),
            ("search-forward", "\u{2500} .$"),
            ("search-backward", "\u{e9} *$"),
            ("search-backward", "(\u{e9}|\u{fc})b"),
            ("search-forward", "\\<caf"),
            ("search-backward", "x*"),
            ("search-backward", "[^ ]+\u{e9}"),
            ("search-backward", "w[0-9]+ C"),
            ("search-forward", "\u{4e2d}."),
        ];
        let mut counted = 0;
        for (seed, cols) in [(1, 30), (7, 41), (99, 17)] {
            let t = styled(seed, 150, cols);
            for (command, pattern) in patterns {
                let want = steps(&t, command, pattern, true);
                let before = TEXT_COUNTED.with(std::cell::Cell::get);
                let got = steps(&t, command, pattern, false);
                counted = counted.max(TEXT_COUNTED.with(std::cell::Cell::get) - before);
                assert_eq!(got, want, "seed {seed}: {command} {pattern:?}");
                assert!(
                    want[0].0.is_some_and(|(c, more)| c >= 0 && !more),
                    "{want:?}"
                );
            }
        }
        // Most lines out of view were counted from their text (fewer than
        // half are ASCII).
        assert!(counted > 100, "{counted}");
    }

    /// A styled history of characters of one column, some past ASCII, is
    /// passed by a jump and counted without reading a line as cells.
    #[test]
    fn a_styled_history_is_searched_without_reading_cells() {
        let peeked = || crate::mode::grid::tests::ROWS_PEEKED.with(std::cell::Cell::get);
        let mut t = PaneTerm::new(60, 4, 1000);
        for i in 0..500 {
            t.feed(
                format!("\x1b[32m2026\x1b[m \x1b[1;34mINFO\x1b[m w[{i}] caf\u{e9} \u{2500}\x1b[44m\x1b[K\x1b[m\r\n")
                    .as_bytes(),
            );
        }
        let g = Grid::clone_of(&t, 60, 4, false).0;
        let last = g.last();
        let before = peeked();
        for (pattern, regex) in [
            ("nowhere", false),
            ("caf\u{e9}s", false),
            ("\u{e9}x+", true),
        ] {
            let pat = Pattern::new(pattern);
            let re = regex.then(|| Regex::new(pattern, false).unwrap());
            assert_eq!(
                g.search_from(&pat, re.as_ref(), (59, last), 0, false, false),
                None
            );
            assert_eq!(
                g.search_from(&pat, re.as_ref(), (0, 0), last, false, true),
                None
            );
        }
        let pat = Pattern::new("w[123] caf\u{e9} \u{2500}");
        assert_eq!(
            g.search_from(&pat, None, (59, last), 0, false, false),
            Some((10, 123))
        );
        assert!(
            peeked() - before < 10,
            "{} lines read as cells",
            peeked() - before
        );
        let (got, counted, _) = count(&t, "search-backward-text", "caf\u{e9} \u{2500}", false);
        assert_eq!(got, Some((500, false)));
        assert!(counted >= 490, "{counted} lines counted from their text");
        assert_eq!(g.kept_lines(), 0);
    }

    /// A plain pattern with a character past ASCII (task
    /// `low_history_bytes_width_search_nonascii`: over 100000 ASCII lines it
    /// counted nothing after the 200 ms, where tmux counts in 0.12 s): the
    /// ASCII lines are counted none from their text and passed by the jump
    /// without being read, and the lines it can match are still found.
    #[test]
    fn a_pattern_past_ascii_skips_ascii_lines_unread() {
        let (t, fast_lines) = mixed(12);
        for pattern in ["\u{e9}", "\u{4e2d}", "caf\u{e9} au", "\t"] {
            let (want, _, _) = count(&t, "search-backward-text", pattern, true);
            let (got, n, _) = count(&t, "search-backward-text", pattern, false);
            assert_eq!(got, want, "{pattern:?}");
            assert!(n >= fast_lines * 12 - 6, "{pattern:?}: {n} lines from text");
        }
        let mut t = PaneTerm::new(40, 4, 1000);
        for i in 0..500 {
            let line = if i == 123 {
                "un \u{e9}t\u{e9}".to_owned()
            } else {
                format!("line {i} plain")
            };
            t.feed(format!("{line}\r\n").as_bytes());
        }
        let g = Grid::clone_of(&t, 40, 4, false).0;
        let last = g.last();
        let pat = Pattern::new("\u{e9}t");
        assert!(pat.never_in_text(true));
        let skipped = || TEXT_SKIPPED.with(std::cell::Cell::get);
        let start = skipped();
        assert_eq!(
            g.search_from(&pat, None, (0, last), 0, false, false),
            Some((3, 123))
        );
        // Lines 124..=last were passed on the way up from the last.
        assert_eq!(skipped() - start, last - 123);
        let before = skipped();
        assert_eq!(
            g.search_from(&pat, None, (0, 0), last, false, true),
            Some((3, 123))
        );
        assert_eq!(skipped() - before, 123);
        // A regular expression the ASCII lines have no match in: the same.
        let re = Regex::new("\u{e9}t.", true).unwrap();
        let before = skipped();
        assert_eq!(
            g.search_from(&pat, Some(&re), (0, last), 0, false, false),
            Some((3, 123))
        );
        assert_eq!(skipped() - before, last - 123);
        let before = skipped();
        assert_eq!(
            g.search_from(&pat, Some(&re), (0, 0), last, false, true),
            Some((3, 123))
        );
        assert_eq!(skipped() - before, 123);
        let pat = Pattern::new("nowhere\u{e9}");
        let before = skipped();
        assert_eq!(g.search_from(&pat, None, (0, 0), last, false, true), None);
        assert_eq!(
            skipped() - before,
            last,
            "all but the last, which is in view"
        );
        assert_eq!(g.kept_lines(), 0);
    }

    /// A plain ASCII pattern looks for itself in the text of the ASCII lines
    /// that do not wrap and reads none of them as cells (task
    /// `audit_perf_hotpaths`: a search over 100k such lines took 3.4x
    /// tmux's time, reading every line's cells at every column), and finds
    /// what the cells find, with and without case, at the line's edges.
    #[test]
    fn a_plain_ascii_pattern_reads_ascii_lines_as_text() {
        let peeked = || crate::mode::grid::tests::ROWS_PEEKED.with(std::cell::Cell::get);
        let mut t = PaneTerm::new(40, 4, 1000);
        for i in 0..500 {
            let line = match i {
                123 => "edge at the end of this full row: Needle".to_owned(),
                321 => "needle at the start".to_owned(),
                _ => format!("line {i} plain \x1b[31mred\x1b[m text"),
            };
            t.feed(format!("{line}\r\n").as_bytes());
        }
        let g = Grid::clone_of(&t, 40, 4, false).0;
        let last = g.last();
        let before = peeked();
        let pat = Pattern::new("nowhere");
        assert_eq!(g.search_from(&pat, None, (0, last), 0, false, false), None);
        assert_eq!(g.search_from(&pat, None, (0, 0), last, false, true), None);
        assert!(
            peeked() - before < 10,
            "{} lines read as cells",
            peeked() - before
        );
        for (pattern, down, want) in [
            ("needle", false, Some((0, 321))),
            ("needle", true, Some((34, 123))),
            ("Needle", false, Some((34, 123))),
            ("Needle", true, Some((34, 123))),
            ("ne 7 pl", false, Some((2, 7))),
            ("red text", true, Some((13, 0))),
            // Blanks past the text are spaces.
            ("start  ", false, Some((14, 321))),
            ("plain  ", false, None),
        ] {
            let pat = Pattern::new(pattern);
            let from = if down { (0, 0) } else { (39, last) };
            let end = if down { last } else { 0 };
            let got = g.search_from(&pat, None, from, end, false, down);
            NO_TEXT.with(|f| f.set(true));
            let cells = g.search_from(&pat, None, from, end, false, down);
            NO_TEXT.with(|f| f.set(false));
            assert_eq!(got, cells, "{pattern} down={down}");
            assert_eq!(got, want, "{pattern} down={down}");
        }
    }

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
        // A character past ASCII makes the search case-sensitive, as
        // `window_copy_is_lowercase` does on a signed `char`.
        assert!(!Pattern::new("\u{e9}").cis && !Pattern::new("a\u{e9}").cis);
    }

    /// A search over all of history reads every line but keeps none
    /// (task `low_copy_mode_entry_clone_keeps_full`: copy mode kept a pane
    /// width of cells for each line a search passed, 25 -> 153 MB for
    /// 10000 short lines at 200 columns, tmux 7 -> 10 MB).
    #[test]
    fn searching_history_keeps_no_lines() {
        let mut t = PaneTerm::new(40, 4, 1000);
        for i in 0..500 {
            t.feed(format!("line {i}\x1b[31m red\x1b[m\tx\r\n").as_bytes());
        }
        let g = Grid::clone_of(&t, 40, 4, false).0;
        let last = g.last();
        assert_eq!(g.kept_lines(), 0);
        let pat = Pattern::new("nowhere");
        assert_eq!(g.search_from(&pat, None, (0, 0), last, false, true), None);
        let re = Regex::new("^line 4.*red", false).unwrap();
        let found = g.search_from(&pat, Some(&re), (0, last), 0, false, false);
        assert_eq!(found, Some((0, 499)));
        assert_eq!(g.row(499).text().trim_end(), "line 499 red\tx");
        assert_eq!(g.kept_lines(), 1, "only the line read for itself is kept");
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
