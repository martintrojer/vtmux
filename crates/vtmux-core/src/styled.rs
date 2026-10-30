//! `#[...]` styles in expanded format text turned into a styled row of cells:
//! `format_draw` and `format_width` of tmux 3.8 (`format-draw.c`).
//!
//! [`draw`] walks the text as tmux does. Characters go to one of eight
//! "sections" (left, centre, right, absolute centre, the window list, its two
//! markers, and what follows the list), chosen by the `align=` and `list=`
//! styles in force; `push-default`, `pop-default` and `set-default` move the
//! colours `default` returns to; `fill=` paints the row first; `range=` styles
//! record clickable ranges as data ([`StyleRange`]). The sections are then
//! arranged over the row by the `format_draw_*` family, trimming centre, list,
//! right and left in the order tmux does. The result is a [`Line`] of
//! [`Cell`]s; [`Line::spans`] groups them into runs a renderer can emit.
//!
//! Faithfully kept, though they look like bugs (each is pinned by a case in
//! `tests/style_cases.sh`, or noted here where the reference cannot show it):
//!
//! - an unterminated `#[` draws nothing at all, not even the text before it;
//! - an invalid style is skipped silently and the text carries on;
//! - a range still open at the end of the text is dropped; ranges close only
//!   at the next style that differs (`format_is_type`);
//! - ranges in a section that is never placed on the row (the list markers
//!   when the list fits, text after an empty list) are reported with raw,
//!   unshifted positions;
//! - with no list left to draw, the text after the list is not drawn, because
//!   `format_draw_left/centre/right` copy it past the end of its target
//!   section and then draw that section by its old width.
//!
//! Widths and cell merging are [`crate::width`]'s (`screen_write_cell`'s
//! combining, as the status line's screen applies it).

use crate::colour::Colour;
use crate::style::{Align, CellStyle, List, RangeKind, Style, StyleSyntax};
use crate::width::{Cluster, Combine, char_width, combine_at_start, text_width};

/// One column-aligned cell of a [`Line`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The cell's text; empty for the second column of a wide character
    /// ("padding", `width` 0).
    pub text: String,
    /// 1, 2, or 0 for padding.
    pub width: u8,
    pub style: CellStyle,
}

impl Cell {
    fn blank(style: &CellStyle) -> Self {
        Self {
            text: " ".to_owned(),
            width: 1,
            style: style.clone(),
        }
    }

    fn padding(bg: Colour) -> Self {
        Self {
            text: String::new(),
            width: 0,
            style: CellStyle {
                bg,
                ..CellStyle::default()
            },
        }
    }

    #[must_use]
    pub fn is_padding(&self) -> bool {
        self.width == 0
    }
}

/// A clickable range (`range=...` styles), in columns, `end` excluded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleRange {
    pub kind: RangeKind,
    pub start: usize,
    pub end: usize,
}

/// A run of cells with one style, as a renderer wants them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    /// Columns the run covers.
    pub width: usize,
    pub style: CellStyle,
}

/// The drawn row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
    pub ranges: Vec<StyleRange>,
}

impl Line {
    /// Consecutive cells of equal style merged; padding cells add no text.
    #[must_use]
    pub fn spans(&self) -> Vec<Span> {
        let mut out: Vec<Span> = Vec::new();
        for cell in &self.cells {
            match out.last_mut() {
                Some(span) if span.style == cell.style => {
                    span.text.push_str(&cell.text);
                    span.width += usize::from(cell.width);
                }
                _ => out.push(Span {
                    text: cell.text.clone(),
                    width: usize::from(cell.width),
                    style: cell.style.clone(),
                }),
            }
        }
        out
    }
}

/// `u_int` arithmetic, as tmux's layout code does it.
const U32_MASK: usize = 0xffff_ffff;

const LEFT: usize = 0;
const CENTRE: usize = 1;
const RIGHT: usize = 2;
const ABSOLUTE_CENTRE: usize = 3;
const LIST: usize = 4;
const LIST_LEFT: usize = 5;
const LIST_RIGHT: usize = 6;
const AFTER: usize = 7;
const TOTAL: usize = 8;

/// A range as `format_draw` tracks it: positions are in `section`'s columns
/// until a placement shifts them onto the row.
struct FRange {
    section: usize,
    start: usize,
    end: usize,
    kind: RangeKind,
}

/// `format_is_type` (format-draw.c:46): does this open range continue under
/// `sy`? Pane, window, session and user ranges compare their argument;
/// left, right and control ranges are one range whatever follows.
fn same_range(open: &FRange, sy: &Style) -> bool {
    use RangeKind::{Control, Left, Pane, Right, Session, User, Window};
    match (&open.kind, &sy.range) {
        (Left, Some(Left)) | (Right, Some(Right)) | (Control(_), Some(Control(_))) => true,
        (Pane(a), Some(Pane(b)))
        | (Window(a), Some(Window(b)))
        | (Session(a), Some(Session(b))) => a == b,
        (User(a), Some(User(b))) => a == b,
        _ => false,
    }
}

/// `format_skip(s, "]")` (format.c:4829): the index of the `]` that closes a
/// style starting at `s`, skipping `#,` `##` `#{` `#}` `#:` escapes and
/// `#{...}` groups. `None` when there is none.
fn skip_to_bracket(s: &[u8]) -> Option<usize> {
    let mut brackets = 0i32;
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'#' && s.get(i + 1) == Some(&b'{') {
            brackets += 1;
        }
        if s[i] == b'#' && s.get(i + 1).is_some_and(|c| b",#{}:".contains(c)) {
            i += 2;
            continue;
        }
        if s[i] == b'}' {
            brackets -= 1;
        }
        if s[i] == b']' && brackets == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `screen_write_cell` into a one-row section screen that never overflows.
fn write_char(section: &mut Vec<Cell>, c: char, style: &CellStyle) {
    if c == '\u{3164}' {
        return;
    }
    let mut c_width = if c.is_ascii() { 1 } else { char_width(c) };
    let combine = match section.last() {
        None => combine_at_start(c),
        Some(last) => {
            // The cell to combine with: the last one, or the one before when
            // the last is the padding of a wide character.
            let (idx, n) = if last.is_padding() && section.len() > 1 {
                (section.len() - 2, 2)
            } else {
                (section.len() - 1, 1)
            };
            let cell = &section[idx];
            if cell.is_padding() || u8::try_from(n) != Ok(cell.width) {
                if c_width == 0 {
                    Combine::Dropped
                } else {
                    Combine::Own
                }
            } else {
                let mut cluster = Cluster {
                    text: cell.text.clone(),
                    width: cell.width,
                };
                let outcome = cluster.push(c);
                if outcome == Combine::Joined {
                    let grew = cluster.width != cell.width;
                    let bg = cell.style.bg;
                    // utf8_from_data stores the size in 5 bits (UTF8_GET_SIZE,
                    // utf8.c:128): a cell that reaches exactly 32 bytes reads back
                    // as size 0, so it shows no text and the next joiner starts over.
                    section[idx].text = if cluster.text.len() == 32 {
                        String::new()
                    } else {
                        cluster.text
                    };
                    section[idx].width = cluster.width;
                    if grew {
                        section.push(Cell::padding(bg));
                    }
                }
                outcome
            }
        }
    };
    if combine != Combine::Own {
        return;
    }
    if c_width == 0 {
        // Cannot happen: zero-width characters never start a cell.
        c_width = 1;
    }
    section.push(Cell {
        text: c.to_string(),
        width: c_width,
        style: style.clone(),
    });
    for _ in 1..c_width {
        section.push(Cell::padding(style.bg));
    }
}

fn draw_many(section: &mut Vec<Cell>, style: &CellStyle, ch: char, n: usize) {
    for _ in 0..n {
        write_char(section, ch, style);
    }
}

/// The output row and the ranges, plus the sections to place on it.
struct Drawer {
    row: Vec<Cell>,
    sections: [Vec<Cell>; TOTAL],
    frs: Vec<FRange>,
}

impl Drawer {
    /// `format_update_ranges` (format-draw.c:75): after placing `width` cells
    /// of `section` starting at `start` at column `offset`.
    fn update_ranges(&mut self, section: usize, offset: usize, start: usize, width: usize) {
        self.frs.retain_mut(|fr| {
            if fr.section != section {
                return true;
            }
            if fr.end <= start || fr.start >= start + width {
                return false;
            }
            fr.start = fr.start.max(start);
            fr.end = fr.end.min(start + width);
            if fr.start == fr.end {
                return false;
            }
            fr.start = (fr.start - start).wrapping_add(offset) & U32_MASK;
            fr.end = (fr.end - start).wrapping_add(offset) & U32_MASK;
            true
        });
    }

    /// `screen_write_fast_copy` of one row (screen-write.c:668) into the row at
    /// `at`. A wide character cut by the right edge stops the copy.
    fn copy(&mut self, section: usize, at: usize, px: usize, nx: usize) {
        if nx == 0 {
            return;
        }
        // `screen_write_cursormove` clamps to the last column.
        let first = at.min(self.row.len().saturating_sub(1));
        for (dst, xx) in (first..).zip(px..px + nx) {
            let cell = self.sections[section]
                .get(xx)
                .cloned()
                .unwrap_or_else(|| Cell::blank(&CellStyle::default()));
            if xx + usize::from(cell.width) > px + nx || dst >= self.row.len() {
                break;
            }
            self.row[dst] = cell;
        }
    }

    /// `format_draw_put` (format-draw.c:120). tmux computes offsets in `u_int`;
    /// the list layouts can subtract past zero (a centre wider than half the
    /// free space), which wraps, and `screen_write_cursormove` then clamps the
    /// absurd column to the last one. (A wrap to exactly -1 would leave the
    /// cursor where it was; that case is not reproduced.)
    fn put(&mut self, section: usize, offset: usize, start: usize, width: usize) {
        let offset = offset & U32_MASK;
        self.copy(section, offset, start, width);
        self.update_ranges(section, offset, start, width);
    }

    fn cx(&self, section: usize) -> usize {
        self.sections[section].len()
    }

    /// `format_draw_put_list` (format-draw.c:137).
    fn put_list(&mut self, mut offset: usize, mut width: usize, focus: (usize, usize)) {
        let list_cx = self.cx(LIST);
        if width >= list_cx {
            self.put(LIST, offset, 0, width);
            return;
        }
        // Trimmed: keep the focus visible.
        let (focus_start, focus_end) = focus;
        let focus_centre = focus_start + (focus_end - focus_start) / 2;
        let mut start = focus_centre.saturating_sub(width / 2);
        if start + width > list_cx {
            start = list_cx - width;
        }
        let left_cx = self.cx(LIST_LEFT);
        if start != 0 && width > left_cx {
            self.copy(LIST_LEFT, offset, 0, left_cx);
            offset += left_cx;
            start += left_cx;
            width -= left_cx;
        }
        let right_cx = self.cx(LIST_RIGHT);
        if start + width < list_cx && width > right_cx {
            self.copy(LIST_RIGHT, offset + width - right_cx, 0, right_cx);
            width -= right_cx;
        }
        self.put(LIST, offset, start, width);
    }

    /// The part of every layout that places the absolute centre section
    /// (format-draw.c:`format_draw_none` tail).
    fn put_abs_centre(&mut self, available: usize) {
        let width = self.cx(ABSOLUTE_CENTRE).min(available);
        self.put(ABSOLUTE_CENTRE, (available - width) / 2, 0, width);
    }

    /// `format_draw_none` (format-draw.c:187).
    fn draw_none(&mut self, available: usize) {
        let (wl, mut wc, mut wr) = (self.cx(LEFT), self.cx(CENTRE), self.cx(RIGHT));
        let mut wl_t = wl;
        while wl_t + wc + wr > available {
            if wc > 0 {
                wc -= 1;
            } else if wr > 0 {
                wr -= 1;
            } else {
                wl_t -= 1;
            }
        }
        self.put(LEFT, 0, 0, wl_t);
        let right_cx = self.cx(RIGHT);
        self.put(RIGHT, available - wr, right_cx - wr, wr);
        let centre_cx = self.cx(CENTRE);
        self.put(
            CENTRE,
            (wl_t + ((available - wr) - wl_t) / 2).wrapping_sub(wc / 2),
            (centre_cx / 2).wrapping_sub(wc / 2),
            wc,
        );
        self.put_abs_centre(available);
    }

    /// The common opening of the list layouts: trim `widths` (left, centre,
    /// right, list, after) in `order` until they fit.
    fn trim(widths: &mut [usize; 5], available: usize, order: [usize; 5]) {
        // indices: 0 left, 1 centre, 2 right, 3 list, 4 after
        while widths.iter().sum::<usize>() > available {
            match order.iter().find(|&&i| widths[i] > 0) {
                Some(&i) => widths[i] -= 1,
                None => break,
            }
        }
    }

    fn widths(&self) -> [usize; 5] {
        [
            self.cx(LEFT),
            self.cx(CENTRE),
            self.cx(RIGHT),
            self.cx(LIST),
            self.cx(AFTER),
        ]
    }

    /// `format_draw_left` (format-draw.c:283).
    fn draw_left(&mut self, available: usize, focus: (isize, isize)) {
        let mut w = self.widths();
        // centre, then list, then right, then after, then left
        Self::trim(&mut w, available, [1, 3, 2, 4, 0]);
        let [wl, wc, wr, wlist, wafter] = w;
        if wlist == 0 {
            // tmux copies `after` past the end of `left` and then draws `left`
            // by its own width: the text after the list is lost.
            self.draw_none(available);
            return;
        }
        self.put(LEFT, 0, 0, wl);
        let right_cx = self.cx(RIGHT);
        self.put(RIGHT, available - wr, right_cx - wr, wr);
        self.put(AFTER, wl + wlist, 0, wafter);
        let centre_cx = self.cx(CENTRE);
        let before = wl + wlist + wafter;
        self.put(
            CENTRE,
            (before + ((available - wr) - before) / 2).wrapping_sub(wc / 2),
            (centre_cx / 2).wrapping_sub(wc / 2),
            wc,
        );
        let focus = if focus.0 < 0 || focus.1 < 0 {
            (0, 0)
        } else {
            (focus.0.unsigned_abs(), focus.1.unsigned_abs())
        };
        self.put_list(wl, wlist, focus);
        self.put_abs_centre(available);
    }

    /// `format_draw_centre` (format-draw.c:381).
    fn draw_centre(&mut self, available: usize, focus: (isize, isize)) {
        let mut w = self.widths();
        // list, then after, then centre, then right, then left
        Self::trim(&mut w, available, [3, 4, 1, 2, 0]);
        let [wl, wc, wr, wlist, wafter] = w;
        if wlist == 0 {
            self.draw_none(available);
            return;
        }
        self.put(LEFT, 0, 0, wl);
        let right_cx = self.cx(RIGHT);
        self.put(RIGHT, available - wr, right_cx - wr, wr);
        let middle = wl + ((available - wr) - wl) / 2;
        self.put(CENTRE, (middle - wlist / 2).wrapping_sub(wc), 0, wc);
        self.put(AFTER, middle - wlist / 2 + wlist, 0, wafter);
        let focus = if focus.0 < 0 || focus.1 < 0 {
            (self.cx(LIST) / 2, self.cx(LIST) / 2)
        } else {
            (focus.0.unsigned_abs(), focus.1.unsigned_abs())
        };
        self.put_list(middle - wlist / 2, wlist, focus);
        self.put_abs_centre(available);
    }

    /// `format_draw_right` (format-draw.c:481).
    fn draw_right(&mut self, available: usize, focus: (isize, isize)) {
        let mut w = self.widths();
        // centre, then list, then right, then after, then left
        Self::trim(&mut w, available, [1, 3, 2, 4, 0]);
        let [wl, wc, wr, wlist, wafter] = w;
        if wlist == 0 {
            self.draw_none(available);
            return;
        }
        self.put(LEFT, 0, 0, wl);
        let after_cx = self.cx(AFTER);
        self.put(AFTER, available - wafter, after_cx - wafter, wafter);
        self.put(RIGHT, available - wr - wlist - wafter, 0, wr);
        let centre_cx = self.cx(CENTRE);
        let stop = available - wr - wlist - wafter;
        self.put(
            CENTRE,
            (wl + (stop - wl) / 2).wrapping_sub(wc / 2),
            (centre_cx / 2).wrapping_sub(wc / 2),
            wc,
        );
        let focus = if focus.0 < 0 || focus.1 < 0 {
            (0, 0)
        } else {
            (focus.0.unsigned_abs(), focus.1.unsigned_abs())
        };
        self.put_list(available - wlist - wafter, wlist, focus);
        self.put_abs_centre(available);
    }

    /// `format_draw_absolute_centre` (format-draw.c:577).
    fn draw_absolute_centre(&mut self, available: usize, focus: (isize, isize)) {
        let (mut wl, mut wc, mut wr) = (self.cx(LEFT), self.cx(CENTRE), self.cx(RIGHT));
        let (mut wlist, mut wafter, mut wabs) =
            (self.cx(LIST), self.cx(AFTER), self.cx(ABSOLUTE_CENTRE));
        while wl + wc + wr > available {
            if wc > 0 {
                wc -= 1;
            } else if wr > 0 {
                wr -= 1;
            } else {
                wl -= 1;
            }
        }
        while wlist + wafter + wabs > available {
            if wlist > 0 {
                wlist -= 1;
            } else if wafter > 0 {
                wafter -= 1;
            } else {
                wabs -= 1;
            }
        }
        self.put(LEFT, 0, 0, wl);
        let right_cx = self.cx(RIGHT);
        self.put(RIGHT, available - wr, right_cx - wr, wr);
        let middle = wl + ((available - wr) - wl) / 2;
        self.put(CENTRE, middle.wrapping_sub(wc), 0, wc);
        let focus = if focus.0 < 0 || focus.1 < 0 {
            (self.cx(LIST) / 2, self.cx(LIST) / 2)
        } else {
            (focus.0.unsigned_abs(), focus.1.unsigned_abs())
        };
        let mut at = (available - wlist - wabs) / 2;
        self.put(ABSOLUTE_CENTRE, at, 0, wabs);
        at += wabs;
        self.put_list(at, wlist, focus);
        at += wlist;
        self.put(AFTER, at, 0, wafter);
    }
}

/// Draw `expanded` onto a row of `available` cells that starts out as spaces in
/// `base` (what `status_redraw` writes before calling `format_draw`).
///
/// `want_ranges` is tmux's `srs != NULL`: collect `range=` styles.
/// `default_colours` is tmux's last argument: every style's colours are reset
/// to `base`'s (used by mode-tree and prompts, never by the status line).
#[must_use]
#[expect(clippy::too_many_lines, reason = "one loop, as format_draw")]
pub fn draw(
    base: &CellStyle,
    available: usize,
    expanded: &str,
    want_ranges: bool,
    default_colours: bool,
) -> Line {
    let mut base_default = CellStyle {
        link: None,
        ..base.clone()
    };
    let mut current_default = base_default.clone();
    let mut sy = Style::new(&current_default);
    let mut d = Drawer {
        row: vec![Cell::blank(base); available],
        sections: Default::default(),
        frs: Vec::new(),
    };
    let mut current = LEFT;
    let mut map = [LEFT, LEFT, CENTRE, RIGHT, ABSOLUTE_CENTRE];
    let (mut focus_start, mut focus_end) = (-1isize, -1isize);
    let (mut list_state, mut list_align) = (-1i32, Align::Default);
    let mut fill: Option<Colour> = None;
    let mut open: Option<FRange> = None;

    let bytes = expanded.as_bytes();
    let mut cp = 0;
    while cp < bytes.len() {
        let next = bytes.get(cp + 1).copied();
        // Runs of `#`.
        if bytes[cp] == b'#' && next.is_some_and(|c| c != b'[') {
            let mut n = 1;
            while bytes.get(cp + n) == Some(&b'#') {
                n += 1;
            }
            let even = n % 2 == 0;
            if bytes.get(cp + n) != Some(&b'[') {
                cp += n;
                let count = if even { n / 2 } else { n / 2 + 1 };
                draw_many(&mut d.sections[current], &sy.cell, '#', count);
                continue;
            }
            cp += if even { n + 1 } else { n - 1 };
            if sy.ignore {
                continue;
            }
            draw_many(&mut d.sections[current], &sy.cell, '#', n / 2);
            if even {
                write_char(&mut d.sections[current], '[', &sy.cell);
            }
            continue;
        }

        // Not a style: draw one character.
        if bytes[cp] != b'#' || next != Some(b'[') || sy.ignore {
            let c = expanded[cp..].chars().next().unwrap_or(' ');
            cp += c.len_utf8();
            if c.is_ascii() && !(' '..='~').contains(&c) {
                continue; // nonprintable
            }
            write_char(&mut d.sections[current], c, &sy.cell);
            continue;
        }

        // A style: find its end and parse it.
        let Some(len) = skip_to_bracket(&bytes[cp + 2..]) else {
            // No terminating `]`: tmux gives up and draws nothing.
            return Line {
                cells: vec![Cell::blank(base); available],
                ranges: Vec::new(),
            };
        };
        let text = &expanded[cp + 2..cp + 2 + len];
        let saved = sy.clone();
        let parsed: Result<(), StyleSyntax> = sy.parse(&current_default, text);
        cp += 2 + len + 1;
        if parsed.is_err() {
            continue;
        }
        if default_colours {
            sy.cell.bg = base_default.bg;
            sy.cell.fg = base_default.fg;
        }
        if sy.fill != Colour::Default {
            fill = Some(sy.fill);
        }
        let strip = |c: &CellStyle| CellStyle {
            link: None,
            ..c.clone()
        };
        match sy.default_action {
            crate::style::DefaultAction::Push => {
                current_default = strip(&saved.cell);
                sy.default_action = crate::style::DefaultAction::None;
            }
            crate::style::DefaultAction::Pop => {
                current_default = base_default.clone();
                sy.default_action = crate::style::DefaultAction::None;
            }
            crate::style::DefaultAction::Set => {
                base_default = strip(&saved.cell);
                current_default = base_default.clone();
                sy.default_action = crate::style::DefaultAction::None;
            }
            crate::style::DefaultAction::None => {}
        }

        // The list state machine.
        let list_cx = i64::try_from(d.cx(LIST)).unwrap_or(0);
        let list_cx = isize::try_from(list_cx).unwrap_or(0);
        match sy.list {
            List::On => {
                if list_state != 0 {
                    open = None;
                    list_state = 0;
                    list_align = sy.align;
                }
                if focus_start != -1 && focus_end == -1 {
                    focus_end = list_cx;
                }
                current = LIST;
            }
            List::Focus => {
                if list_state == 0 && focus_start == -1 {
                    focus_start = list_cx;
                }
            }
            List::Off => {
                if list_state == 0 {
                    open = None;
                    if focus_start != -1 && focus_end == -1 {
                        focus_end = list_cx;
                    }
                    map[align_index(list_align)] = AFTER;
                    if list_align == Align::Left {
                        map[align_index(Align::Default)] = AFTER;
                    }
                    list_state = 1;
                }
                current = map[align_index(sy.align)];
            }
            List::LeftMarker | List::RightMarker => {
                let (marker, ok) = if sy.list == List::LeftMarker {
                    (LIST_LEFT, d.cx(LIST_LEFT) == 0)
                } else {
                    (LIST_RIGHT, d.cx(LIST_RIGHT) == 0)
                };
                if list_state == 0 && ok {
                    open = None;
                    if focus_start != -1 && focus_end == -1 {
                        focus_start = -1;
                        focus_end = -1;
                    }
                    current = marker;
                }
            }
        }

        // Ranges: end the open one if the style no longer continues it, and
        // start a new one if the style asks for it.
        if want_ranges {
            if let Some(fr) = open.take_if(|fr| !same_range(fr, &sy)) {
                let at = d.cx(current);
                if at != fr.start {
                    d.frs.push(FRange { end: at, ..fr });
                }
            }
            if open.is_none()
                && let Some(kind) = &sy.range
            {
                open = Some(FRange {
                    section: current,
                    start: d.cx(current),
                    end: 0,
                    kind: kind.clone(),
                });
            }
        }
    }
    // A range still open here is dropped.
    drop(open);

    // Clear the available area with the fill colour.
    if let Some(bg) = fill {
        let blank = CellStyle {
            bg,
            ..CellStyle::default()
        };
        d.row = vec![Cell::blank(&blank); available];
    }

    let focus = (focus_start, focus_end);
    match list_align {
        Align::Default => d.draw_none(available),
        Align::Left => d.draw_left(available, focus),
        Align::Centre => d.draw_centre(available, focus),
        Align::Right => d.draw_right(available, focus),
        Align::AbsoluteCentre => d.draw_absolute_centre(available, focus),
    }

    blank_orphan_padding(&mut d.row, base);
    Line {
        cells: d.row,
        ranges: d
            .frs
            .into_iter()
            .map(|fr| StyleRange {
                kind: fr.kind,
                start: fr.start,
                end: fr.end,
            })
            .collect(),
    }
}

/// A padding cell is only the second column of a wide character. When a copy
/// cut the wide character off (a section trimmed or placed mid-character), the
/// padding is left alone in the row, and `tty_draw_line` (tty-draw.c:100, 170-200)
/// draws it as a blank in the previous cell's background (the status style's
/// at the start of the row).
fn blank_orphan_padding(row: &mut [Cell], base: &CellStyle) {
    for i in 0..row.len() {
        if row[i].width != 0 || (i > 0 && row[i - 1].width == 2) {
            continue;
        }
        let bg = if i == 0 { base.bg } else { row[i - 1].style.bg };
        row[i] = Cell::blank(&CellStyle {
            bg,
            ..CellStyle::default()
        });
    }
}

fn align_index(a: Align) -> usize {
    match a {
        Align::Default => 0,
        Align::Left => 1,
        Align::Centre => 2,
        Align::Right => 3,
        Align::AbsoluteCentre => 4,
    }
}

/// `format_leading_hashes` (format-draw.c:535): count the `#`s at the start of
/// `cp`; returns (bytes to skip, columns they draw). When the run is an odd
/// count before `[` it is a style: the skip stops at the last `#`.
fn leading_hashes(cp: &[u8]) -> (usize, usize) {
    let n = cp.iter().take_while(|&&b| b == b'#').count();
    if n == 0 {
        return (0, 0);
    }
    if cp.get(n) != Some(&b'[') {
        return (n, n.div_ceil(2));
    }
    if n % 2 == 0 {
        // All escaped: not a style, the caller goes on from the `[`.
        return (n, n / 2);
    }
    (n - 1, n / 2)
}

/// `format_width` (format-draw.c:1128): the columns `expanded` takes, not
/// counting `#[...]` styles and with `##` as one. An unterminated style makes
/// the whole width 0.
#[must_use]
pub fn format_width(expanded: &str) -> usize {
    let bytes = expanded.as_bytes();
    let mut width = 0;
    let mut cp = 0;
    while cp < bytes.len() {
        if bytes[cp] == b'#' {
            let (skip, cols) = leading_hashes(&bytes[cp..]);
            width += cols;
            cp += skip;
            if bytes.get(cp) == Some(&b'#') {
                match skip_to_bracket(&bytes[cp + 2..]) {
                    Some(end) => cp += 2 + end + 1,
                    None => return 0,
                }
            }
        } else {
            let c = expanded[cp..].chars().next().unwrap_or(' ');
            cp += c.len_utf8();
            width += text_width(c.encode_utf8(&mut [0; 4]));
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colour::Colour;
    use crate::style::Attrs;
    use vtmux_vt::{Color, PaneTerm, snapshot};

    /// One reference status line (see `tests/style_cases.sh`).
    struct Golden {
        name: &'static str,
        /// What `format_draw` was given.
        expanded: &'static str,
        cols: u16,
        /// The `status-style` option.
        base: &'static str,
        /// What a reference client wrote to its terminal.
        bytes: &'static [u8],
        /// `mouse_status_range` per column, when the case has ranges.
        ranges: Option<&'static [&'static str]>,
    }

    // BEGIN GENERATED styled_cases
    // One case per status line. `bytes` is what a reference client attached under a PTY wrote
    // (set -g status-style BASE; set -g status-format[0] FORMAT, at COLS x 5), replayed through
    // PaneTerm by the test. `expanded` is `display-message -p FORMAT`, what format_draw is
    // given. `ranges` is #{mouse_status_range} (with window_index, pane_id and session_id for
    // those kinds) under a click on every column of the status row.
    #[expect(clippy::too_many_lines, reason = "generated data")]
    fn golden_cases() -> Vec<Golden> {
        vec![
            Golden {
                name: "plain",
                expanded: "hello world",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nhello world         \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nhello world         \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "empty",
                expanded: "",
                cols: 10,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "colours and attrs",
                expanded: "#[fg=red,bg=#102030,bold]hi#[default] there",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[48;2;16;32;48m\x1b[1m\r\nhi\x1b(B\x1b[m there\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[48;2;16;32;48m\x1b[1m\r\nhi\x1b(B\x1b[m there\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "nested styles",
                expanded: "#[fg=red]a#[bg=blue]b#[nobold,italics]c#[bold]d#[none]e#[default]f",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na\x1b[44mb\x1b[3mc\x1b[1md\x1b(B\x1b[m\x1b[31m\x1b[44me\x1b[39m\x1b[49mf\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na\x1b[44mb\x1b[3mc\x1b[1md\x1b(B\x1b[m\x1b[31m\x1b[44me\x1b[39m\x1b[49mf\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "every attribute",
                expanded: "#[bold]a#[nobold,dim]b#[nodim,underscore]c#[nounderscore,blink]d#[noblink,reverse]e#[noreverse,hidden]f#[nohidden,italics]g#[noitalics,strikethrough]h#[nostrikethrough,overline]i#[none]j",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[1m\r\na\x1b(B\x1b[m\x1b[2mb\x1b(B\x1b[m\x1b[4mc\x1b(B\x1b[m\x1b[5md\x1b(B\x1b[m\x1b[7me\x1b(B\x1b[m\x1b[8mf\x1b(B\x1b[m\x1b[3mg\x1b(B\x1b[m\x1b[9mh\x1b(B\x1b[m\x1b[53mi\x1b(B\x1b[mj\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[1m\r\na\x1b(B\x1b[m\x1b[2mb\x1b(B\x1b[m\x1b[4mc\x1b(B\x1b[m\x1b[5md\x1b(B\x1b[m\x1b[7me\x1b(B\x1b[m\x1b[8mf\x1b(B\x1b[m\x1b[3mg\x1b(B\x1b[m\x1b[9mh\x1b(B\x1b[m\x1b[53mi\x1b(B\x1b[mj\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "underline kinds",
                expanded: "#[double-underscore]a#[nodouble-underscore,curly-underscore]b#[nocurly-underscore,dotted-underscore]c#[nodotted-underscore,dashed-underscore]d#[nodashed-underscore,us=#ff8800,underscore]e#[us=red,italics]f#[us=default,noitalics]g#[us=colour9,bold]h",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[4:2m\r\na\x1b(B\x1b[m\x1b[4:3mb\x1b(B\x1b[m\x1b[4:4mc\x1b(B\x1b[m\x1b[4:5md\x1b(B\x1b[m\x1b[58:2:255:136:0m\x1b[4me\x1b[58:5:1m\x1b[3mf\x1b(B\x1b[m\x1b[4mg\x1b[58:5:9m\x1b[1mh\x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[4:2m\r\na\x1b(B\x1b[m\x1b[4:3mb\x1b(B\x1b[m\x1b[4:4mc\x1b(B\x1b[m\x1b[4:5md\x1b(B\x1b[m\x1b[58:2:255:136:0m\x1b[4me\x1b[58:5:1m\x1b[3mf\x1b(B\x1b[m\x1b[4mg\x1b[58:5:9m\x1b[1mh\x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "colour forms",
                expanded: "#[fg=colour196]a#[fg=brightred]b#[fg=91]c#[fg=#00ff00]d#[fg=terminal]e#[fg=default]f#[bg=alice blue]g#[bg=colour17]h#[fg=3,bg=7]i#[bg=terminal]j",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;5;196m\r\na\x1b[91mbc\x1b(B\x1b[m\x1b[38;2;0;255;0md\x1b[39mefg\x1b[48;5;17mh\x1b[33m\x1b[47mi\x1b[49mj\x1b[39m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;5;196m\r\na\x1b[91mbc\x1b(B\x1b[m\x1b[38;2;0;255;0md\x1b[39mefg\x1b[48;5;17mh\x1b[33m\x1b[47mi\x1b[49mj\x1b[39m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "base style and default",
                expanded: "x#[fg=red]a#[default]b#[fg=default]c#[bg=default]d",
                cols: 12,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nx\x1b[31ma\x1b[33mbcd       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nx\x1b[31ma\x1b[33mbcd       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "base attrs and default",
                expanded: "x#[nobold]a#[default]b#[none]c",
                cols: 12,
                base: "bold,fg=green,underscore",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[32m\x1b[1m\x1b[4m\r\nx\x1b(B\x1b[m\x1b[32m\x1b[4ma\x1b[1mb\x1b(B\x1b[m\x1b[32mc\x1b[1m\x1b[4m        \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[32m\x1b[1m\x1b[4m\r\nx\x1b(B\x1b[m\x1b[32m\x1b[4ma\x1b[1mb\x1b(B\x1b[m\x1b[32mc\x1b[1m\x1b[4m        \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "push default",
                expanded: "#[bg=blue]a#[push-default]b#[fg=red]c#[default]d#[pop-default]e#[default]f",
                cols: 16,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[44m\r\nab\x1b[31mc\x1b[39mde\x1b[49mf\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[44m\r\nab\x1b[31mc\x1b[39mde\x1b[49mf\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "pop default",
                expanded: "#[fg=red,push-default]a#[bg=blue]b#[default]c#[pop-default]d#[default]e",
                cols: 16,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na\x1b[44mb\x1b[39m\x1b[49mcde\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na\x1b[44mb\x1b[39m\x1b[49mcde\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "set default",
                expanded: "#[fg=red,set-default]a#[bg=blue]b#[default]c#[pop-default]d#[default]e",
                cols: 16,
                base: "fg=yellow",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na\x1b[44mb\x1b[49m\x1b[33mcde\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na\x1b[44mb\x1b[49m\x1b[33mcde\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "push default under base",
                expanded: "#[bg=red]a#[push-default]b#[default]c#[pop-default]d#[default]e",
                cols: 16,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[41m\r\nabcd\x1b[44me\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[41m\r\nabcd\x1b[44me\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "status-left shape",
                expanded: "#[align=left range=left default]#[push-default]L #[pop-default]#[norange default]#[nolist align=right range=right default]#[push-default]R#[pop-default]#[norange default]",
                cols: 20,
                base: "bg=blue,fg=white",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[37m\x1b[44m\r\nL\x1b[18X\x1b[18CR\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[37m\x1b[44m\r\nL\x1b[18X\x1b[18CR\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["left", "left", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "right"]),
            },
            Golden {
                name: "align left centre right",
                expanded: "L#[align=centre]C#[align=right]R",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL         C        R\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL         C        R\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "align all four",
                expanded: "#[align=left]LL#[align=centre]CC#[align=right]RR#[align=absolute-centre]AA",
                cols: 21,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nLL       AA        RR\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nLL       AA        RR\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "align overflow",
                expanded: "#[align=left]LLLL#[align=centre]CCCC#[align=right]RRRR",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLRRRR\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLRRRR\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "align overflow hard",
                expanded: "#[align=left]LLLLLL#[align=centre]CCCC#[align=right]RRRRRR",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLLLRR\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLLLRR\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "align overflow left only",
                expanded: "#[align=left]LLLLLLLLLLLL#[align=right]RR",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLLLLL\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLLLLL\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "align odd widths",
                expanded: "#[align=left]L#[align=centre]CCC#[align=right]R",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL    CCC   R\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL    CCC   R\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "absolute centre",
                expanded: "#[align=absolute-centre]MID",
                cols: 11,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n    MID    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n    MID    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "absolute centre with sides",
                expanded: "left#[align=absolute-centre]MID#[align=right]right",
                cols: 21,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nleft     MID    right\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nleft     MID    right\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "absolute centre overflow",
                expanded: "#[align=absolute-centre]MIDDLEPART",
                cols: 6,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l      \x1b[2;1H      \x1b[3;1H      \x1b[4;1H      \x1b[5;1HMIDDLE\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l      \x1b[2;1H      \x1b[3;1H      \x1b[4;1H      \x1b[5;1HMIDDLE\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "styled aligns",
                expanded: "#[fg=red]L#[align=centre,fg=green]C#[align=right,fg=blue,bold]R",
                cols: 15,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\nL\x1b[39m      \x1b[32mC\x1b[39m      \x1b[34m\x1b[1mR\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\nL\x1b[39m      \x1b[32mC\x1b[39m      \x1b[34m\x1b[1mR\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "noalign",
                expanded: "#[align=right]R#[noalign]L",
                cols: 10,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL        R\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL        R\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "fill",
                expanded: "#[fill=blue]x",
                cols: 10,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nx\x1b[44m         \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nx\x1b[44m         \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "fill and align",
                expanded: "#[fill=colour4]a#[align=right,bg=red]b",
                cols: 10,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\x1b[44m        \x1b[41mb\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\x1b[44m        \x1b[41mb\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "fill over base",
                expanded: "#[fill=red]x",
                cols: 10,
                base: "bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[44m\r\nx\x1b[41m         \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[44m\r\nx\x1b[41m         \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "hash escapes",
                expanded: "a#b#[fg=red]c",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na#b\x1b[31mc\x1b[39m        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na#b\x1b[31mc\x1b[39m        \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "hash runs before style",
                expanded: "###[fg=red]x####[fg=red]y",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[31mx##[fg=red]y\x1b[39m\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[31mx##[fg=red]y\x1b[39m\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "hash at end",
                expanded: "abc",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Habc     \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Habc     \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "ignore",
                expanded: "#[ignore]#[fg=red]a#[noignore]#[fg=green]b",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#[fg=red]a#[\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#[fg=red]a#[\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "ignore hashes",
                expanded: "#[ignore]##[x #[noignore]y",
                cols: 16,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nx #[noignore]y  \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nx #[noignore]y  \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "invalid style skipped",
                expanded: "a#[bogus]b#[fg=nope]c#[fg=red]d",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabc\x1b[31md\x1b[39m        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabc\x1b[31md\x1b[39m        \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "invalid style rolls back",
                expanded: "#[fg=red,bogus]a#[fg=blue]b",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\x1b[34mb\x1b[39m\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\x1b[34mb\x1b[39m\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "unterminated style",
                expanded: "abc#[fg=red",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "unterminated after align",
                expanded: "#[align=right]ab#[fg=red",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "style with escapes",
                expanded: "#[fg=red]a#[align=#]]b",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na]b\x1b[39m         \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\na]b\x1b[39m         \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "layout-only styles",
                expanded: "#[width=5,pad=2]ab#[width=7]cd#[dim=50]ef",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcdef      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcdef      \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "link style",
                expanded: "#[link=http://x]ab#[nolink]cd",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcd        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcd        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "dim style",
                expanded: "#[dim=50,fg=red]ab#[dim=0]cd",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\nabcd\x1b[39m        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\r\nabcd\x1b[39m        \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "wide characters",
                expanded: "a\u{4e16}\u{754c}b",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\xe4\xb8\x96\xe7\x95\x8cb      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\xe4\xb8\x96\xe7\x95\x8cb      \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "wide cut at the edge",
                expanded: "ab\u{4e16}\u{754c}",
                cols: 5,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1Hab\xe4\xb8\x96 \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1Hab\xe4\xb8\x96 \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "wide cut in centre",
                expanded: "#[align=centre]\u{4e16}\u{754c}\u{4e16}\u{754c}",
                cols: 7,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[5;1H \xe7\x95\x8c\xe4\xb8\x96\xe7\x95\x8c\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[5;1H \xe7\x95\x8c\xe4\xb8\x96\xe7\x95\x8c\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "wide right trim",
                expanded: "#[align=right]\u{4e16}\u{754c}\u{4e16}",
                cols: 5,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1H \xe7\x95\x8c\xe4\xb8\x96\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1H \xe7\x95\x8c\xe4\xb8\x96\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "combining mark",
                expanded: "e\u{301}x",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1He\xcc\x81x      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1He\xcc\x81x      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "emoji",
                expanded: "a\u{1f600}b",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x98\x80b    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x98\x80b    \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "emoji tone",
                expanded: "a\u{1f44d}\u{1f3fd}b",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdb    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdb    \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "zwj sequence",
                expanded: "a\u{1f468}\u{200d}\u{1f469}b",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9b    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9b    \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "flag",
                expanded: "a\u{1f1ef}\u{1f1f5}b",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x87\xaf\xf0\x9f\x87\xb5b    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Ha\xf0\x9f\x87\xaf\xf0\x9f\x87\xb5b    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "control chars dropped",
                expanded: "a\u{1}b\u{7f}c\u{9}d",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Habcd    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Habcd    \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "wide with style",
                expanded: "#[fg=red,bg=blue]\u{4e16}#[default]\u{754c}",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[31m\x1b[44m\x1b[5;1H\xe4\xb8\x96\x1b[39m\x1b[49m\xe7\x95\x8c    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[31m\x1b[44m\x1b[5;1H\xe4\xb8\x96\x1b[39m\x1b[49m\xe7\x95\x8c    \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list fits",
                expanded: "#[align=left]A#[list=on]1 2 3#[nolist]Z",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA1 2 3Z\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA1 2 3Z\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "list focus fits",
                expanded: "#[align=left]A#[list=on]1 #[list=focus]2#[list=on] 3#[nolist]Z",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA1 2 3Z\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA1 2 3Z\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "list markers fit",
                expanded: "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]abc #[list=focus]def#[list=on] ghi#[nolist]Z",
                cols: 30,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAabc def ghiZ\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAabc def ghiZ\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "list overflow focus middle",
                expanded: "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd eeee#[nolist]Z",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA< bbbb cccc dddd >Z\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA< bbbb cccc dddd >Z\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list overflow focus start",
                expanded: "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]#[list=focus]aaaa#[list=on] bbbb cccc dddd eeee#[nolist]Z",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAaaaa bbbb cccc dd>Z\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAaaaa bbbb cccc dd>Z\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "list overflow focus end",
                expanded: "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]aaaa bbbb cccc dddd #[list=focus]eeee#[nolist]Z",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA<bb cccc dddd eeeeZ\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nA<bb cccc dddd eeeeZ\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "list overflow no focus",
                expanded: "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]aaaa bbbb cccc dddd eeee#[nolist]Z",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAaaaa bbbb cccc dd>Z\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAaaaa bbbb cccc dd>Z\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list overflow no markers",
                expanded: "#[align=left]A#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd eeee#[nolist]Z",
                cols: 14,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAbbb cccc dddZ\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nAbbb cccc dddZ\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list centre",
                expanded: "L#[align=centre]C#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd#[nolist]Z#[align=right]R",
                cols: 30,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL    Caaaa bbbb cccc ddddZ   R\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL    Caaaa bbbb cccc ddddZ   R\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list centre fits",
                expanded: "L#[align=centre]C#[list=on]abc#[list=focus]def#[nolist]Z#[align=right]R",
                cols: 30,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[10X\x1b[10CCabcdefZ\x1b[10X\x1b[10CR\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[10X\x1b[10CCabcdefZ\x1b[10X\x1b[10CR\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list right",
                expanded: "L#[align=right]R#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd#[nolist]Z",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL  Raaaa bbbb cccc ddddZ\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL  Raaaa bbbb cccc ddddZ\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list right fits",
                expanded: "L#[align=right]R#[list=on]abc#[list=focus]def#[nolist]Z",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[15X\x1b[15CRabcdefZ\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[15X\x1b[15CRabcdefZ\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list absolute centre",
                expanded: "L#[align=absolute-centre]X#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd#[nolist]Z#[align=right]R",
                cols: 30,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL    Xaaaa bbbb cccc ddddZ   R\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL    Xaaaa bbbb cccc ddddZ   R\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list absolute centre fits",
                expanded: "L#[align=absolute-centre]X#[list=on]ab#[list=focus]cd#[nolist]Z",
                cols: 30,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[11X\x1b[11CXabcdZ\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[11X\x1b[11CXabcdZ\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "list squeezed to nothing",
                expanded: "#[align=left]LLLLLLLL#[list=on]abc#[nolist]Z",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLLLLL\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLLLLLL\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "list with window-status shape",
                expanded: "#[align=left range=left default]#[push-default]0 #[pop-default]#[norange default]#[list=on align=left]#[list=left-marker]<#[list=right-marker]>#[list=on]#[range=window|0 list=focus underscore]#[push-default]0:w*#[pop-default]#[norange list=on default] #[range=window|1 default]#[push-default]1:second #[pop-default]#[norange default]#[nolist align=right range=right default]#[push-default]R#[pop-default]#[norange default]",
                cols: 30,
                base: "bg=blue,fg=white",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[37m\x1b[44m\r\n0 \x1b[4m0:w*\x1b(B\x1b[m\x1b[37m\x1b[44m 1:second\x1b[14X\x1b[14CR\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[37m\x1b[44m\r\n0 \x1b[4m0:w*\x1b(B\x1b[m\x1b[37m\x1b[44m 1:second\x1b[14X\x1b[14CR\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["left", "left", "window:0", "window:0", "window:0", "window:0", "", "window:1", "window:1", "window:1", "window:1", "window:1", "window:1", "window:1", "window:1", "window:1", "", "", "", "", "", "", "", "", "", "", "", "", "", "right"]),
            },
            Golden {
                name: "list with window-status overflow",
                expanded: "#[align=left range=left default]#[push-default]LEFT #[pop-default]#[norange default]#[list=on align=left]#[list=left-marker]<#[list=right-marker]>#[list=on]#[range=window|0 default]#[push-default]0:one #[pop-default]#[norange list=on default] #[range=window|1 list=focus default]#[push-default]1:two* #[pop-default]#[norange list=on default] #[range=window|2 default]#[push-default]2:three #[pop-default]#[norange list=on default] #[range=window|3 default]#[push-default]3:four #[pop-default]#[norange default]#[nolist align=right range=right default]#[push-default]RIGHT#[pop-default]#[norange default]",
                cols: 40,
                base: "bg=blue,fg=white",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[37m\x1b[44m\r\nLEFT 0:one  1:two*  2:three  3:fou>RIGHT\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[37m\x1b[44m\r\nLEFT 0:one  1:two*  2:three  3:fou>RIGHT\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["left", "left", "left", "left", "left", "window:0", "window:0", "window:0", "window:0", "window:0", "window:0", "", "window:1", "window:1", "window:1", "window:1", "window:1", "window:1", "window:1", "", "window:2", "window:2", "window:2", "window:2", "window:2", "window:2", "window:2", "window:2", "", "window:3", "window:3", "window:3", "window:3", "window:3", "", "right", "right", "right", "right", "right"]),
            },
            Golden {
                name: "ranges",
                expanded: "#[range=user|ab]hello#[norange] #[range=window|2]win#[norange]#[range=pane|%0]P#[norange]#[range=session|$1]S#[norange]#[align=right]#[range=left]R#[norange]",
                cols: 30,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nhello winPS\x1b[18X\x1b[18CR\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nhello winPS\x1b[18X\x1b[18CR\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["user:ab", "user:ab", "user:ab", "user:ab", "user:ab", "", "window:2", "window:2", "window:2", "pane:%0", "session:$1", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "left"]),
            },
            Golden {
                name: "range runs into next",
                expanded: "#[range=window|1]ab#[range=window|2]cd#[norange]ef",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcdef      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcdef      \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["window:1", "window:1", "window:2", "window:2", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range same type continues",
                expanded: "#[range=left]ab#[fg=red]cd#[range=left]ef#[norange]gh",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nab\x1b[31mcdefgh\x1b[39m    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nab\x1b[31mcdefgh\x1b[39m    \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["left", "left", "left", "left", "left", "left", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range left and right",
                expanded: "#[range=left]L#[norange]#[align=right]#[range=right]R#[norange]",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[10X\x1b[10CR\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nL\x1b[10X\x1b[10CR\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["left", "", "", "", "", "", "", "", "", "", "", "right"]),
            },
            Golden {
                name: "range open at end dropped",
                expanded: "ab#[range=left]cd",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcd        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcd        \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range empty dropped",
                expanded: "ab#[range=left]#[norange]cd",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcd        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nabcd        \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range in centre",
                expanded: "a#[align=centre]#[range=user|mid]mid#[norange]#[align=right]z",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na        mid       z\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na        mid       z\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "user:mid", "user:mid", "user:mid", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range cut by width",
                expanded: "#[range=user|a]abcdefgh#[norange]",
                cols: 5,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1Habcde\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1Habcde\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["user:a", "user:a", "user:a", "user:a", "user:a"]),
            },
            Golden {
                name: "range in right cut",
                expanded: "#[align=left]LLL#[align=right]#[range=user|r]rrrrrr#[norange]",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLrrrrr\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1HLLLrrrrr\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "user:r", "user:r", "user:r", "user:r", "user:r"]),
            },
            Golden {
                name: "range in wide text",
                expanded: "#[range=user|w]\u{4e16}\u{754c}#[norange]x",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1H\xe4\xb8\x96\xe7\x95\x8cx   \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1H\xe4\xb8\x96\xe7\x95\x8cx   \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["user:w", "user:w", "user:w", "user:w", "", "", "", ""]),
            },
            Golden {
                name: "range user label",
                expanded: "#[range=user|0123456789abcdef]ab#[norange]",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Hab      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Hab      \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["user:0123456789abcde", "user:0123456789abcde", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range control",
                expanded: "#[range=control|3]ab#[norange]",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Hab      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Hab      \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["control", "control", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range in list",
                expanded: "#[align=left]#[list=on]#[range=user|a]aa#[norange]#[range=user|b]bb#[norange]#[nolist]",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\naabb        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\naabb        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["user:a", "user:a", "user:b", "user:b", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range in list overflow",
                expanded: "#[align=left]#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]#[range=user|a]aaaa#[norange]#[range=user|b list=focus]bbbb#[norange list=on]#[range=user|c]cccc#[norange]#[nolist]",
                cols: 10,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<aabbbbcc>\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<aabbbbcc>\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "user:a", "user:a", "user:b", "user:b", "user:b", "user:b", "user:c", "user:c", ""]),
            },
            Golden {
                name: "range in list markers",
                expanded: "#[align=left]#[list=on]#[list=left-marker]#[range=user|lm]<#[norange]#[list=right-marker]#[range=user|rm]>#[norange]#[list=on]aaaa bbbb cccc#[nolist]",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Haaaa bb>\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Haaaa bb>\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["user:lm", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "range after the list",
                expanded: "#[align=left]#[list=on]aa#[nolist]#[range=user|z]zz#[norange]",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\naazz        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\naazz        \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "user:z", "user:z", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 00",
                expanded: "#[range=user|a]#[default]<#[list=on]#[range=user|b]#[align=left]#[fg=red]#[range=user|b]#[fill=green]\u{4e16}\u{754c}#[nobold,italics]",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 01",
                expanded: "#[default]#[norange]##[pop-default]#[list=focus]#[noignore]e\u{301}#[push-default]\u{4e16}\u{754c}",
                cols: 24,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n#[pop-default]e\xcc\x81\xe4\xb8\x96\xe7\x95\x8c     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n#[pop-default]e\xcc\x81\xe4\xb8\x96\xe7\x95\x8c     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 02",
                expanded: "#[bg=blue]#[range=user|a]#[list=left-marker]#[bg=blue]#[bg=blue]#[noalign]#[norange]",
                cols: 11,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 03",
                expanded: "#[align=centre]e\u{301}#[bold]>#[range=right]",
                cols: 22,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n          e\xcc\x81\x1b[1m>\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n          e\xcc\x81\x1b[1m>\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 04",
                expanded: "##[fg=#445566,us=red,underscore]#[fill=green]\u{1f44d}#[range=user|a]",
                cols: 34,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#[fg=#445566,us=red,underscore]\xf0\x9f\x91\x8d\x1b[39m\x1b[42m \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#[fg=#445566,us=red,underscore]\xf0\x9f\x91\x8d\x1b[39m\x1b[42m \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 05",
                expanded: "a#[range=right]\u{1f44d}def#[norange]\u{1f44d}<<#[align=left]",
                cols: 31,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na\xf0\x9f\x91\x8ddef\xf0\x9f\x91\x8d<<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na\xf0\x9f\x91\x8ddef\xf0\x9f\x91\x8d<<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "right", "right", "right", "right", "right", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 06",
                expanded: "##[range=user|a]#[range=user|a]>#[align=centre]x y<#[list=focus]",
                cols: 38,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#[range=user|a]>         x y<         \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#[range=user|a]>         x y<         \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 07",
                expanded: "#[align=right]#[pop-default]#[range=user|a]#[range=left]#[fill=green]#[noignore]#[fill=green]#[pop-default]",
                cols: 32,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[42m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[42m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 08",
                expanded: "x ya#[fill=green]#[default]\u{1f44d}#[range=right]#[pop-default]",
                cols: 25,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nx ya\xf0\x9f\x91\x8d\x1b[39m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nx ya\xf0\x9f\x91\x8d\x1b[39m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 09",
                expanded: "#[nobold,italics]#[norange]#[range=left]#[push-default]#[noalign]",
                cols: 30,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 10",
                expanded: "##[push-default]#[range=right]#[align=right]def#[ignore]#[nolist]#[list=on]\u{4e16}<#[fg=red]e\u{301}a#[bg=blue]",
                cols: 16,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n#[push-default]]\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n#[push-default]]\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 11",
                expanded: "\u{4e16}\u{4e16}\u{754c}#[align=absolute-centre]#[fg=#445566,us=red,underscore]#[fill=green]>#[range=user|b]\u{4e16}#[align=left]#[bg=blue] #[list=left-marker]",
                cols: 22,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n\xe4\xb8\x96\xe4\xb8\x96\xe7\x95\x8c\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m \x1b(B\x1b[m\x1b[42m  \x1b[38;2;68;85;102m\x1b[41m\x1b[58:5:1m\x1b[4m>\xe4\xb8\x96\x1b(B\x1b[m\x1b[42m          \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n\xe4\xb8\x96\xe4\xb8\x96\xe7\x95\x8c\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m \x1b(B\x1b[m\x1b[42m  \x1b[38;2;68;85;102m\x1b[41m\x1b[58:5:1m\x1b[4m>\xe4\xb8\x96\x1b(B\x1b[m\x1b[42m          \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 12",
                expanded: "#[align=left]#\u{1f44d}#[list=on]aa#[range=right]#[default]#[fg=#445566,us=red,underscore]<#[push-default]><#[noignore]#[bogus]def",
                cols: 8,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1H#\xf0\x9f\x91\x8daa\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m<><\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1H#\xf0\x9f\x91\x8daa\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m<><\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 13",
                expanded: "#[align=centre]#[push-default]\u{4e16}\u{754c}\u{1f44d}\u{1f3fd}#[nobold,italics]#[range=user|b]\u{4e16}#[bold]a#[noignore]#[range=user|a]#[push-default]#[ignore]#[ignore]x y",
                cols: 32,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n      \xe4\xb8\x96\xe7\x95\x8c\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b[3m\xe4\xb8\x96\x1b[1ma#[ignore]x y\x1b(B\x1b[m\x1b[41m     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n      \xe4\xb8\x96\xe7\x95\x8c\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b[3m\xe4\xb8\x96\x1b[1ma#[ignore]x y\x1b(B\x1b[m\x1b[41m     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "user:b", "user:b", "user:b", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 14",
                expanded: "###[list=focus]\u{1f44d}\u{1f3fd}#[bogus]#[bogus]#[bold]e\u{301}>\u{1f44d}\u{1f3fd}\u{4e16}\u{754c}ghij#[fill=green]#[range=user|b] #[align=centre]",
                cols: 35,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b[1me\xcc\x81>\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xe4\xb8\x96\xe7\x95\x8cghij \x1b(B\x1b[m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b[1me\xcc\x81>\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xe4\xb8\x96\xe7\x95\x8cghij \x1b(B\x1b[m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 15",
                expanded: "#[bogus]#[noalign]#[bg=blue]#[align=left]#[nobold,italics]",
                cols: 15,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 16",
                expanded: "<#[bg=blue]x yx y###[align=left]#[noalign]#[range=left]<",
                cols: 7,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1H<\x1b[44mx yx y\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1H<\x1b[44mx yx y\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 17",
                expanded: "#[nobold,italics]ghij<#[bogus]a#[fg=red]#[bogus]ghij",
                cols: 9,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[41m\x1b[3m\x1b[5;1Hghij<a\x1b[31mghi\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[41m\x1b[3m\x1b[5;1Hghij<a\x1b[31mghi\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 18",
                expanded: "a<#[nobold,italics]",
                cols: 15,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 19",
                expanded: "def#[list=on]#[bold]#[norange]\u{4e16}#[noignore]#",
                cols: 17,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 20",
                expanded: "#[list=right-marker]ghij#[bold]#[list=right-marker]e\u{301}#[pop-default]\u{1f44d}\u{1f3fd}bc\u{4e16}#<ghij#[range=user|a]#[align=absolute-centre]#[list=left-marker]#[ignore]",
                cols: 29,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nghij\x1b[1me\xcc\x81\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdbc\xe4\xb8\x96#<ghij\x1b(B\x1b[m\x1b[41m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nghij\x1b[1me\xcc\x81\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdbc\xe4\xb8\x96#<ghij\x1b(B\x1b[m\x1b[41m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 21",
                expanded: "#[noalign]#[bogus] e\u{301}#[ignore]e\u{301}#[pop-default]#[align=centre]#[ignore]e\u{301}#[bogus]",
                cols: 40,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n e\xcc\x81e\xcc\x81#[pop-default]#[align=centre]#[ignore\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n e\xcc\x81e\xcc\x81#[pop-default]#[align=centre]#[ignore\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 22",
                expanded: "x y#[nolist]#[default]#[range=user|a]#[bogus]bc#[list=right-marker]#[noalign]#[range=user|a]#[default]#[range=right]#[pop-default]#[fg=#445566,us=red,underscore] ",
                cols: 21,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nx ybc\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m \x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nx ybc\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m \x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "user:a", "user:a", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 23",
                expanded: "#[noignore]e\u{301}#[align=left]#[nolist]#[range=left]#[range=user|b]#[list=on]#[fill=green]",
                cols: 29,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\ne\xcc\x81\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\ne\xcc\x81\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 24",
                expanded: "#[bg=blue]#[align=right]e\u{301}#[bogus]#[fill=green]#[ignore]",
                cols: 6,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l      \x1b[2;1H      \x1b[3;1H      \x1b[4;1H      \x1b[42m\x1b[5;1H     \x1b[44me\xcc\x81\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l      \x1b[2;1H      \x1b[3;1H      \x1b[4;1H      \x1b[42m\x1b[5;1H     \x1b[44me\xcc\x81\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 25",
                expanded: "<\u{1f44d}#[nobold,italics]#[fill=green]x y#[fg=red]ghij><bc#[align=absolute-centre]#[noalign]",
                cols: 38,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<\xf0\x9f\x91\x8d\x1b[3mx y\x1b[31mghij><bc\x1b(B\x1b[m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<\xf0\x9f\x91\x8d\x1b[3mx y\x1b[31mghij><bc\x1b(B\x1b[m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 26",
                expanded: "\u{1f44d}#[list=focus]#[list=right-marker]#[list=focus]#[fg=#445566,us=red,underscore]#[range=right]\u{4e16}#[default]#[push-default]#[range=left]\u{1f44d}<#[list=on]#[nolist]#[bg=blue]",
                cols: 38,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n\xf0\x9f\x91\x8d\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m\xe4\xb8\x96\x1b(B\x1b[m\x1b[41m\xf0\x9f\x91\x8d<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n\xf0\x9f\x91\x8d\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m\xe4\xb8\x96\x1b(B\x1b[m\x1b[41m\xf0\x9f\x91\x8d<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "right", "right", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 27",
                expanded: "#[align=absolute-centre]#[noalign]x y #[fg=red]<>#[fill=green]<x y\u{4e16}",
                cols: 39,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nx y \x1b[31m<><x y\xe4\xb8\x96\x1b[39m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nx y \x1b[31m<><x y\xe4\xb8\x96\x1b[39m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "random 28",
                expanded: "\u{4e16}#[fg=red]#[range=user|b]#[default]#[list=left-marker]<##[align=right]#[default]#[fg=#445566,us=red,underscore]#",
                cols: 18,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96<#[align=right]\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m#\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96<#[align=right]\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m#\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 29",
                expanded: "\u{4e16}#[push-default]#[list=left-marker]<\u{1f44d}\u{1f3fd}<#[range=user|a]#[range=left]#[fg=red]#[default]#[align=left]#[fg=#445566,us=red,underscore]",
                cols: 32,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96<\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96<\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd<\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 30",
                expanded: "#[bogus]#[list=right-marker]#[range=right]#[range=user|a]#[range=user|a]>#[align=right]#[range=right]#[bold]#[fill=green]#[nobold,italics]#[push-default]\u{4e16}",
                cols: 4,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[33m\x1b[44m\x1b[5;1H>\x1b[3m\xe4\xb8\x96\x1b(B\x1b[m\x1b[42m \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[33m\x1b[44m\x1b[5;1H>\x1b[3m\xe4\xb8\x96\x1b(B\x1b[m\x1b[42m \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["user:a", "", "", ""]),
            },
            Golden {
                name: "random 31",
                expanded: "#[range=right]bc\u{1f44d}e\u{301} #[range=right]#[bg=blue]e\u{301}##[nolist]#[range=right]#[align=absolute-centre]#[default]",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nbc\xf0\x9f\x91\x8de\xcc\x81 \x1b[44me\xcc\x81#[nolist]\x1b[49m        \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nbc\xf0\x9f\x91\x8de\xcc\x81 \x1b[44me\xcc\x81#[nolist]\x1b[49m        \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 32",
                expanded: "#[bg=blue]#[default]#[noignore]#[align=centre]\u{4e16}#[list=left-marker]#[pop-default]e\u{301}#[fill=green] #[norange]#[range=user|b]#[norange]#[align=right]\u{1f44d}#[list=focus]",
                cols: 16,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[42m\r\n     \x1b[33m\x1b[44m\xe4\xb8\x96e\xcc\x81 \xf0\x9f\x91\x8d\x1b[39m\x1b[42m     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[42m\r\n     \x1b[33m\x1b[44m\xe4\xb8\x96e\xcc\x81 \xf0\x9f\x91\x8d\x1b[39m\x1b[42m     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 33",
                expanded: "#[bogus]aghijdef#[push-default]#[align=absolute-centre]x y#[align=absolute-centre]#[fg=#445566,us=red,underscore]#[bogus]ghij\u{1f44d}",
                cols: 13,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nagx y\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4mghij\xf0\x9f\x91\x8d\x1b(B\x1b[m\x1b[33m\x1b[44m  \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nagx y\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4mghij\xf0\x9f\x91\x8d\x1b(B\x1b[m\x1b[33m\x1b[44m  \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 34",
                expanded: "#[range=right]#[bogus]<",
                cols: 25,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n<\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 35",
                expanded: "#[fill=green]a#[align=right]#[default]#[range=user|b]e\u{301}",
                cols: 15,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na\x1b[39m\x1b[42m \x1b[12X\x1b[33m\x1b[44m\x1b[12Ce\xcc\x81\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na\x1b[39m\x1b[42m \x1b[12X\x1b[33m\x1b[44m\x1b[12Ce\xcc\x81\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 36",
                expanded: "#[default]#[range=right]a#[range=user|a]#[bg=blue]#[default]a#[bogus]\u{1f44d}#[list=on]",
                cols: 17,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\naa\xf0\x9f\x91\x8d\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\naa\xf0\x9f\x91\x8d\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["right", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 37",
                expanded: "#[fg=#445566,us=red,underscore]\u{1f44d}\u{1f3fd}\u{1f44d}\u{1f3fd}#[align=absolute-centre]\u{1f44d}\u{1f3fd}#[range=user|a]#[noignore]#[fg=#445566,us=red,underscore]#[nolist]#[fg=#445566,us=red,underscore]\u{4e16}#[list=on]\u{4e16}\u{754c}#[align=left]#[align=centre]",
                cols: 21,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m\r\n\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m\x1b[33m\x1b[44m  \x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xe4\xb8\x96\xe4\xb8\x96\xe7\x95\x8c\x1b(B\x1b[m\x1b[33m\x1b[44m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m\r\n\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m\x1b[33m\x1b[44m  \x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xe4\xb8\x96\xe4\xb8\x96\xe7\x95\x8c\x1b(B\x1b[m\x1b[33m\x1b[44m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 38",
                expanded: "ghij\u{4e16}\u{754c}#[fill=green]#[fg=#445566,us=red,underscore]#[norange]#[range=right]#[list=left-marker]#[noignore]#[fg=red]",
                cols: 32,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nghij\xe4\xb8\x96\xe7\x95\x8c\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nghij\xe4\xb8\x96\xe7\x95\x8c\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 39",
                expanded: "e\u{301}#[bogus]#[fill=green]#[fg=#445566,us=red,underscore]#[fg=#445566,us=red,underscore]#[bogus]#[align=left]#[norange]#[ignore]#[bg=blue]#[fg=red]",
                cols: 16,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\ne\xcc\x81\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m#[bg=blue]#[fg=\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\ne\xcc\x81\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m#[bg=blue]#[fg=\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 40",
                expanded: "#[default]#[range=right]#[list=right-marker]#[range=user|b]#[align=right]#[list=focus]#[list=left-marker]",
                cols: 27,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 41",
                expanded: "#[list=focus]#[range=user|a]#[align=centre]#[list=focus]\u{4e16}<#[list=on]",
                cols: 6,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l      \x1b[2;1H      \x1b[3;1H      \x1b[4;1H      \x1b[5;1H\xe4\xb8\x96<   \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l      \x1b[2;1H      \x1b[3;1H      \x1b[4;1H      \x1b[5;1H\xe4\xb8\x96<   \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", ""]),
            },
            Golden {
                name: "random 42",
                expanded: ">#[range=user|a]a#[noignore]#[fg=#445566,us=red,underscore]#[align=centre]\u{4e16}\u{754c}#[list=left-marker]#[align=right]#[nobold,italics]#[list=focus]",
                cols: 39,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n>a\x1b[16X\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m\x1b[16C\xe4\xb8\x96\xe7\x95\x8c\x1b(B\x1b[m\x1b[41m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n>a\x1b[16X\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m\x1b[16C\xe4\xb8\x96\xe7\x95\x8c\x1b(B\x1b[m\x1b[41m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 43",
                expanded: "#[range=right]x y#[nolist]#[pop-default]def#[list=left-marker]#[nobold,italics]bc#[ignore]#[pop-default]#[align=absolute-centre]",
                cols: 21,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nx ydef\x1b[3mbc#[pop-default\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nx ydef\x1b[3mbc#[pop-default\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 44",
                expanded: "bc#[noignore]#[list=left-marker]\u{4e16}#[bold]\u{4e16}\u{754c}#<#[fg=#445566,us=red,underscore]",
                cols: 32,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nbc\xe4\xb8\x96\x1b[1m\xe4\xb8\x96\xe7\x95\x8c#<\x1b(B\x1b[m\x1b[41m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nbc\xe4\xb8\x96\x1b[1m\xe4\xb8\x96\xe7\x95\x8c#<\x1b(B\x1b[m\x1b[41m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 45",
                expanded: "#[list=right-marker]#[range=user|b]#[align=absolute-centre]#[push-default]#[align=left]#[range=left]#[nobold,italics]#[ignore]#[nobold,italics]#[range=user|a]",
                cols: 12,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\x1b[3m\r\n#[nobold,ita\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\x1b[3m\r\n#[nobold,ita\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 46",
                expanded: "#[norange]e\u{301}#[list=right-marker]bc#[align=right]#[fill=green]#[pop-default]#[range=user|b]#[range=right]",
                cols: 16,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ne\xcc\x81bc\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ne\xcc\x81bc\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 47",
                expanded: "#[ignore]#[list=focus]#[align=absolute-centre]#[bogus]ghij#[align=centre]#[list=left-marker]#[range=right]#[list=on]#[fg=#445566,us=red,underscore]#[ignore]bc#[range=left]",
                cols: 17,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#[list=focus]#[al\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#[list=focus]#[al\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 48",
                expanded: "ghij#[fill=green]#[list=left-marker] \u{1f44d}\u{1f3fd}bc\u{4e16}\u{1f44d}#[range=user|b]#[norange]#[range=right]#[bogus]",
                cols: 35,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nghij \xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdbc\xe4\xb8\x96\xf0\x9f\x91\x8d\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nghij \xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdbc\xe4\xb8\x96\xf0\x9f\x91\x8d\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 49",
                expanded: "#[pop-default]#[noignore]#[noignore]#[range=right]  #[list=left-marker]#[bold]\u{4e16}a#[default]#[bold]a",
                cols: 25,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n  \x1b[1m\xe4\xb8\x96aa\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n  \x1b[1m\xe4\xb8\x96aa\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 50",
                expanded: "#[fg=red]#[bg=blue]##[default]x y#[range=left]#[align=right]##[default]x ya#[bg=blue]#[noalign]\u{1f44d}",
                cols: 29,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[44m\r\n#[default]x y\xf0\x9f\x91\x8d#[default]x ya\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[44m\r\n#[default]x y\xf0\x9f\x91\x8d#[default]x ya\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 51",
                expanded: "#[range=right]#[nobold,italics]#[list=focus]>\u{1f44d}#[noalign]#[align=absolute-centre]#[list=focus] #[norange]",
                cols: 16,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\x1b[3m\r\n>\xf0\x9f\x91\x8d \x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\x1b[3m\r\n>\xf0\x9f\x91\x8d \x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["right", "right", "right", "right", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 52",
                expanded: "#[fill=green]#[bold]def#[push-default]",
                cols: 19,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\x1b[1m\r\ndef\x1b(B\x1b[m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\x1b[1m\r\ndef\x1b(B\x1b[m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 53",
                expanded: "###[align=absolute-centre]#[range=right]e\u{301}#[noignore]e\u{301}#[range=user|b]ghijx y>",
                cols: 35,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[11X\x1b[11Ce\xcc\x81e\xcc\x81ghijx y>\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[11X\x1b[11Ce\xcc\x81e\xcc\x81ghijx y>\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "right", "right", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 54",
                expanded: "#[list=focus]#[default]a#",
                cols: 32,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\na#\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\na#\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "random 55",
                expanded: "#[list=right-marker]#[pop-default]ghij#[list=on]#[push-default]#[range=user|b]#[nobold,italics]#[list=on]",
                cols: 4,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[41m\x1b[5;1Hghij\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[41m\x1b[5;1Hghij\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", ""]),
            },
            Golden {
                name: "random 56",
                expanded: "x y#[bogus]#[fill=green]#[list=right-marker]#[norange]\u{1f44d}\u{1f3fd}#[nolist]bc#[fill=green]#[fill=green]#[align=right]#[list=right-marker]\u{1f44d}#[list=right-marker]#[align=absolute-centre]\u{1f44d}",
                cols: 40,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nx y\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdbc\x1b[42m \x1b[28X\x1b[41m\x1b[28C\xf0\x9f\x91\x8d\xf0\x9f\x91\x8d\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nx y\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbdbc\x1b[42m \x1b[28X\x1b[41m\x1b[28C\xf0\x9f\x91\x8d\xf0\x9f\x91\x8d\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 57",
                expanded: "#[fg=red]def#[align=right]\u{1f44d}\u{1f3fd}#[push-default]\u{1f44d}\u{4e16}#[bogus]\u{4e16}#[noalign]#[range=left]#[noignore]#[bold]",
                cols: 24,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[44m\r\ndef\x1b[33m\x1b[13X\x1b[31m\x1b[13C\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xf0\x9f\x91\x8d\xe4\xb8\x96\xe4\xb8\x96\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[44m\r\ndef\x1b[33m\x1b[13X\x1b[31m\x1b[13C\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xf0\x9f\x91\x8d\xe4\xb8\x96\xe4\xb8\x96\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 58",
                expanded: "#[bogus]\u{1f44d}>#[range=right]#[noalign]#[bold]##[range=left]#[range=left]#[align=centre]#[range=left]#[align=right]#[list=right-marker]#[align=right]\u{1f44d}\u{1f3fd}#[bogus]",
                cols: 9,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[5;1H\xf0\x9f\x91\x8d>\x1b[1m#[rang\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[5;1H\xf0\x9f\x91\x8d>\x1b[1m#[rang\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "right", "right", "right", "right", "right", "right"]),
            },
            Golden {
                name: "random 59",
                expanded: "x y#[default]#[range=user|b]#[range=user|a]#[list=focus]x y#[default]a#[fg=red]",
                cols: 9,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[5;1Hx yx ya  \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[5;1Hx yx ya  \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 60",
                expanded: "#[fg=#445566,us=red,underscore]#[range=left]ghij#[fg=red]a",
                cols: 21,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m\r\nghij\x1b[31m\x1b[58:5:1ma\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m\r\nghij\x1b[31m\x1b[58:5:1ma\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 61",
                expanded: "#[ignore]a#[fg=#445566,us=red,underscore]bc\u{4e16}\u{754c}x y#[range=user|a]\u{1f44d}#[list=left-marker]#[norange]#[list=right-marker]bcghij#[list=left-marker]",
                cols: 18,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na#[fg=#445566,us=r\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na#[fg=#445566,us=r\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 62",
                expanded: " #\u{1f44d}<#[push-default]#[align=absolute-centre]#[fill=green]#[noalign]#[noignore]#[pop-default]\u{1f44d}\u{1f3fd}a#",
                cols: 18,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n #\xf0\x9f\x91\x8d<\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbda#\x1b[42m         \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n #\xf0\x9f\x91\x8d<\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbda#\x1b[42m         \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 63",
                expanded: "#[fg=red]#[list=right-marker]#[bold]ghij#[push-default]\u{1f44d}\u{1f3fd}#[noignore]#[pop-default]#[range=left]#[range=user|b]",
                cols: 12,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[1m\r\nghij\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m      \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[1m\r\nghij\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m      \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 64",
                expanded: "#[bold]#[ignore]ghij#[norange]a#[fill=green]",
                cols: 4,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[1m\x1b[5;1Hghij\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[1m\x1b[5;1Hghij\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 65",
                expanded: " >bcx y#[noignore]\u{1f44d}#[range=user|a]#[fg=red]#[fg=red]\u{4e16}e\u{301}#[list=focus]#[pop-default]#[range=right]#[range=left]",
                cols: 13,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n >bcx y\xf0\x9f\x91\x8d\x1b[31m\xe4\xb8\x96e\xcc\x81\x1b[39m \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n >bcx y\xf0\x9f\x91\x8d\x1b[31m\xe4\xb8\x96e\xcc\x81\x1b[39m \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "user:a", "user:a", "user:a", ""]),
            },
            Golden {
                name: "random 66",
                expanded: "##[list=right-marker]#[nolist]#[nobold,italics]#[range=left]#[align=right]\u{1f44d}#[align=left]#[noignore]###[range=left]",
                cols: 7,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1H#[list=\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1H#[list=\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 67",
                expanded: "#[align=right]defa#[nolist]#[align=left]def#[nolist]#[bogus]>#[align=absolute-centre]#[noalign]#[range=user|a]",
                cols: 7,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1Hdef>efa\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1Hdef>efa\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 68",
                expanded: "#[nobold,italics]##[bold]",
                cols: 21,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[3m\r\n#[bold]\x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[3m\r\n#[bold]\x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 69",
                expanded: "\u{4e16}\u{754c}<e\u{301}##[norange]#[bg=blue]#[align=right]#[list=on]#[align=centre]#[noalign]#[push-default]\u{1f44d}\u{1f3fd}",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\xe4\xb8\x96\xe7\x95\x8c<e\xcc\x81#[norange]      \x1b[44m\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\xe4\xb8\x96\xe7\x95\x8c<e\xcc\x81#[norange]      \x1b[44m\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 70",
                expanded: "\u{4e16}#[list=focus]e\u{301}\u{1f44d}\u{1f3fd}<#[noignore]bc #[list=on]#[list=right-marker]",
                cols: 39,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n\xe4\xb8\x96e\xcc\x81\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd<bc\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n\xe4\xb8\x96e\xcc\x81\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd<bc\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 71",
                expanded: "a\u{4e16}\u{754c}#[list=right-marker]x y#>#[pop-default]#[list=right-marker]\u{4e16}#[nobold,italics]>e\u{301}#[list=focus]#bc#[fg=#445566,us=red,underscore]",
                cols: 16,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\xe4\xb8\x96\xe7\x95\x8cx y#>\xe4\xb8\x96\x1b[3m>e\xcc\x81#b\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\na\xe4\xb8\x96\xe7\x95\x8cx y#>\xe4\xb8\x96\x1b[3m>e\xcc\x81#b\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 72",
                expanded: "#[fg=#445566,us=red,underscore] ##[ignore]#[fg=#445566,us=red,underscore]#[align=absolute-centre]#[pop-default]#[bold]x y#[list=focus]#[range=user|a]",
                cols: 39,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m\r\n #[ignore]\x1b(B\x1b[m\x1b[33m\x1b[44m        \x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[1m\x1b[4mx y\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4m\r\n #[ignore]\x1b(B\x1b[m\x1b[33m\x1b[44m        \x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[1m\x1b[4mx y\x1b(B\x1b[m\x1b[33m\x1b[44m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 73",
                expanded: "#[align=centre]ghij#[nolist]\u{1f44d}\u{1f3fd}\u{4e16}\u{754c}bc#[bogus]#[align=centre]#[list=focus]#[align=absolute-centre]#\u{4e16}\u{754c}#[range=right]#[bogus]#[range=left]",
                cols: 19,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n ghij\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xe4\xb8\x96\xe7\x95\x8cbc#\xe4\xb8\x96\xe7\x95\x8c \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n ghij\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xe4\xb8\x96\xe7\x95\x8cbc#\xe4\xb8\x96\xe7\x95\x8c \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 74",
                expanded: "#[range=left]#[range=right]#[list=focus]def#[range=right]#[align=centre]#[fg=red]",
                cols: 19,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 75",
                expanded: "#[align=centre]#[range=left]#[align=left]#[fg=#445566,us=red,underscore]#[default]\u{4e16}\u{754c}#[range=user|b]#[align=right]##[align=right]#[nolist]###[default]",
                cols: 5,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1H\xe4\xb8\x96\xe7\x95\x8c#\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H     \x1b[2;1H     \x1b[3;1H     \x1b[4;1H     \x1b[5;1H\xe4\xb8\x96\xe7\x95\x8c#\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", ""]),
            },
            Golden {
                name: "random 76",
                expanded: "#[pop-default]a#[list=left-marker]#[norange]#[fill=green]",
                cols: 23,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na\x1b[39m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\na\x1b[39m\x1b[42m \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 77",
                expanded: "def\u{4e16}x ybc#[noalign]a#[list=right-marker]",
                cols: 9,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[5;1Hdef\xe4\xb8\x96x yb\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l         \x1b[2;1H         \x1b[3;1H         \x1b[4;1H         \x1b[5;1Hdef\xe4\xb8\x96x yb\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 78",
                expanded: "#[list=right-marker]#[align=absolute-centre]#[noalign]#[fg=#445566,us=red,underscore]###[fg=#445566,us=red,underscore]x y#[align=centre]#[default]>#[nolist]\u{4e16}#[default]#[range=left]\u{1f44d}\u{1f3fd}#[list=right-marker]",
                cols: 14,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[41m\x1b[58:5:1m\x1b[4m\r\n#x y\x1b(B\x1b[m\x1b[41m>  \xe4\xb8\x96\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd   \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[41m\x1b[58:5:1m\x1b[4m\r\n#x y\x1b(B\x1b[m\x1b[41m>  \xe4\xb8\x96\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd   \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 79",
                expanded: "#[noignore]\u{1f44d}\u{1f3fd}def#[range=user|a]#[pop-default]#[align=centre] a\u{4e16}\u{754c}",
                cols: 36,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbddef\x1b[13X\x1b[13Ca\xe4\xb8\x96\xe7\x95\x8c\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbddef\x1b[13X\x1b[13Ca\xe4\xb8\x96\xe7\x95\x8c\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 80",
                expanded: "#[list=on]#[list=on]#[norange]",
                cols: 29,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 81",
                expanded: "#[range=right]e\u{301}#[noalign]#[nolist]#[norange]def#[pop-default]#[noalign]#[push-default]#x y#[pop-default]#[range=user|b]",
                cols: 38,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ne\xcc\x81def#x y\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ne\xcc\x81def#x y\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["right", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 82",
                expanded: "#[noalign]# #[noalign]#[ignore]",
                cols: 32,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 83",
                expanded: "#[range=user|a]\u{4e16}\u{754c}#[noalign]#[bogus]#[range=user|b]#[fg=#445566,us=red,underscore]x y#[list=on]#[pop-default]",
                cols: 7,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[33m\x1b[44m\x1b[5;1H\xe4\xb8\x96\xe7\x95\x8c\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4mx y\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[33m\x1b[44m\x1b[5;1H\xe4\xb8\x96\xe7\x95\x8c\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4mx y\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["user:a", "user:a", "user:a", "user:a", "", "", ""]),
            },
            Golden {
                name: "random 84",
                expanded: "##[fg=#445566,us=red,underscore]#[noalign]#[push-default]#[range=user|a]#[bg=blue]#[norange]>",
                cols: 21,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#[fg=#445566,us=red,u\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#[fg=#445566,us=red,u\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 85",
                expanded: "#[range=right]#[range=user|a]#[list=focus]#[align=absolute-centre]\u{4e16}#[fg=red]e\u{301}#[bold]\u{4e16}#[bogus]#[range=right]x ybc",
                cols: 12,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96\x1b[31me\xcc\x81\x1b[1m\xe4\xb8\x96x ybc\x1b(B\x1b[m\x1b[33m\x1b[44m  \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96\x1b[31me\xcc\x81\x1b[1m\xe4\xb8\x96x ybc\x1b(B\x1b[m\x1b[33m\x1b[44m  \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["user:a", "user:a", "user:a", "user:a", "user:a", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 86",
                expanded: "#[list=right-marker]def\u{1f44d}\u{1f3fd}#[nolist]\u{1f44d}#[noalign]#[norange]#[range=right]#[bold]#[default]#[fg=#445566,us=red,underscore]def#[nobold,italics]",
                cols: 20,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xf0\x9f\x91\x8d\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4mdef\x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\xf0\x9f\x91\x8d\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4mdef\x1b(B\x1b[m\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 87",
                expanded: ">\u{1f44d}\u{4e16}#[range=left] #[range=right]\u{4e16}\u{4e16}#[fg=red]\u{1f44d}#[align=left]#[nolist]#[range=left]#[range=left]",
                cols: 17,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n>\xf0\x9f\x91\x8d\xe4\xb8\x96 \xe4\xb8\x96\xe4\xb8\x96\x1b[31m\xf0\x9f\x91\x8d\x1b[33m     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n>\xf0\x9f\x91\x8d\xe4\xb8\x96 \xe4\xb8\x96\xe4\xb8\x96\x1b[31m\xf0\x9f\x91\x8d\x1b[33m     \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "left", "right", "right", "right", "right", "right", "right", "", "", "", "", ""]),
            },
            Golden {
                name: "random 88",
                expanded: "#a#[nolist]",
                cols: 33,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#a\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#a\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 89",
                expanded: "#[norange]#[nolist]>#[nolist]#[fill=green]#[list=on]#[align=absolute-centre]x y\u{4e16}##[push-default]",
                cols: 8,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1H>\x1b[39m\x1b[42m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1H>\x1b[39m\x1b[42m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 90",
                expanded: "#ghij#[range=left]\u{1f44d}\u{1f3fd}#[ignore]#[nolist]\u{4e16}\u{754c}#[bold]\u{4e16}x ybc>#[align=left]bc",
                cols: 4,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[33m\x1b[44m\x1b[5;1H#ghi\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[33m\x1b[44m\x1b[5;1H#ghi\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", ""]),
            },
            Golden {
                name: "random 91",
                expanded: "#[nolist]def#[nobold,italics]",
                cols: 40,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\ndef\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 92",
                expanded: "#[bold]#[pop-default]##[bg=blue]e\u{301}#[list=focus]#[fg=#445566,us=red,underscore]<\u{4e16}#[norange]#[align=right]###[fill=green]>#[list=on]<",
                cols: 15,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[1m\r\n#[bg=blue]e\xcc\x81\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m<\xe4\xb8\x96#\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[1m\r\n#[bg=blue]e\xcc\x81\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m<\xe4\xb8\x96#\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 93",
                expanded: "<##[fill=green]#[bogus]#[noignore]\u{4e16}#[range=right]#[noignore]#[align=right]#[bg=blue]",
                cols: 7,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[5;1H<#[fill\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[5;1H<#[fill\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 94",
                expanded: "#[list=focus]\u{4e16}\u{754c}e\u{301}#[push-default]",
                cols: 25,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96\xe7\x95\x8ce\xcc\x81\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n\xe4\xb8\x96\xe7\x95\x8ce\xcc\x81\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 95",
                expanded: "#[list=right-marker]>#[bg=blue]##[list=focus]#[align=left]\u{1f44d}\u{1f3fd}",
                cols: 23,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n>\x1b[44m#[list=focus]\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b[41m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n>\x1b[44m#[list=focus]\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b[41m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 96",
                expanded: "#[list=right-marker]#[align=left]#[fg=#445566,us=red,underscore]#[align=right]",
                cols: 37,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 97",
                expanded: "#[align=left]##[fg=red] ",
                cols: 39,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n#[fg=red]\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n#[fg=red]\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 98",
                expanded: "#[noignore]bcbc#[range=user|a]#[list=on]<a#[range=left]#[ignore]#[nolist]#[fg=red]#[ignore]#[align=centre]#[list=right-marker]",
                cols: 19,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nbcbc\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\nbcbc\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["user:a", "user:a", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 99",
                expanded: "#[align=absolute-centre]#[bold]\u{1f44d}#[noalign]",
                cols: 10,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n    \x1b[1m\xf0\x9f\x91\x8d\x1b(B\x1b[m    \x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n    \x1b[1m\xf0\x9f\x91\x8d\x1b(B\x1b[m    \x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 100",
                expanded: "#[ignore]#[range=left]#[norange]",
                cols: 24,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#[range=left]#[norange] \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#[range=left]#[norange] \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 101",
                expanded: "#[push-default]#[range=user|a]#[bold]#[noalign]#[fg=#445566,us=red,underscore]#[list=right-marker]#[ignore]>#[align=absolute-centre]#[bold]\u{4e16}\u{754c}#[push-default]bc",
                cols: 32,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[41m\x1b[58:5:1m\x1b[1m\x1b[4m\r\n>#[align=absolute-centre]#[bold]\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[38;2;68;85;102m\x1b[41m\x1b[58:5:1m\x1b[1m\x1b[4m\r\n>#[align=absolute-centre]#[bold]\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 102",
                expanded: ">\u{1f44d}>#[range=user|b]#[noignore]#[bold]#[range=user|a]e\u{301}<#[align=centre]#[noignore]#[norange]#[list=left-marker]\u{1f44d}#[list=on]#[align=right]",
                cols: 8,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1H>\xf0\x9f\x91\x8d>\x1b[1me\xcc\x81<\xf0\x9f\x91\x8d\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1H>\xf0\x9f\x91\x8d>\x1b[1me\xcc\x81<\xf0\x9f\x91\x8d\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 103",
                expanded: "#[bogus]#[bg=blue]#[align=absolute-centre]#[list=left-marker]#[pop-default]ae\u{301}bc#[range=right]#[align=centre]#[align=centre]e\u{301}",
                cols: 12,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n   ae\xcc\x81bce\xcc\x81    \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n   ae\xcc\x81bce\xcc\x81    \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 104",
                expanded: "#[range=left]bca#[fg=red]#def#[list=on]#[norange]#[range=user|a]e\u{301}#[fg=#445566,us=red,underscore]#\u{4e16}\u{754c}#[align=centre]",
                cols: 29,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nbca\x1b[31m#def\x1b[39m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\nbca\x1b[31m#def\x1b[39m\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 105",
                expanded: "#[push-default]#[noalign]def#[default]\u{4e16}\u{754c}#[range=left]#[range=user|b]#[align=left]#[fg=#445566,us=red,underscore] #[default]#[align=left]#[range=user|b]bcdef",
                cols: 35,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\ndef\xe4\xb8\x96\xe7\x95\x8c\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m \x1b(B\x1b[m\x1b[33m\x1b[44mbcdef\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\ndef\xe4\xb8\x96\xe7\x95\x8c\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m \x1b(B\x1b[m\x1b[33m\x1b[44mbcdef\x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 106",
                expanded: "bc#[align=right]##[range=user|b]#[list=focus]\u{1f44d}<#[nobold,italics]\u{1f44d}\u{1f44d}\u{1f3fd}",
                cols: 4,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[5;1Hbc\x1b[3m\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l    \x1b[2;1H    \x1b[3;1H    \x1b[4;1H    \x1b[5;1Hbc\x1b[3m\xf0\x9f\x91\x8d\xf0\x9f\x8f\xbd\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", ""]),
            },
            Golden {
                name: "random 107",
                expanded: "#[nolist]#[list=left-marker]#[bg=blue]",
                cols: 31,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n \x1b[K\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 108",
                expanded: "bc>#[bold]\u{4e16}##[fg=#445566,us=red,underscore]#[fg=red]#[nolist]e\u{301}#[align=absolute-centre]\u{1f44d}#[ignore]#[noignore]#[range=user|b]",
                cols: 34,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nbc>\x1b[31m\x1b[1m\xf0\x9f\x91\x8d#[noignore]#[range=user|b]\x1b[33mcor\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nbc>\x1b[31m\x1b[1m\xf0\x9f\x91\x8d#[noignore]#[range=user|b]\x1b[33mcor\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 109",
                expanded: "###[bold]##[list=on]#[nolist]#[noignore]#[fg=red]#[nobold,italics]#[fg=red]#[fg=red]#[align=centre]#[bg=blue]<#[noignore]",
                cols: 13,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[1m#[list=on]\x1b(B\x1b[m \x1b[31m\x1b[44m\x1b[3m<\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[?25l\x1b[1;1H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[1m#[list=on]\x1b(B\x1b[m \x1b[31m\x1b[44m\x1b[3m<\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h",
                ranges: None,
            },
            Golden {
                name: "random 110",
                expanded: "#[fg=red]\u{4e16}\u{754c}#[bold]",
                cols: 11,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[44m\r\n\xe4\xb8\x96\xe7\x95\x8c\x1b[33m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[31m\x1b[44m\r\n\xe4\xb8\x96\xe7\x95\x8c\x1b[33m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 111",
                expanded: "bc#[fg=#445566,us=red,underscore]#[list=right-marker]#[align=left]>#[bogus]#[nobold,italics]bc#[bold]#[list=on]",
                cols: 12,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nbc\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m>\x1b[58:5:1m\x1b[3mbc\x1b(B\x1b[m\x1b[33m\x1b[44m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\nbc\x1b[38;2;68;85;102m\x1b[58:5:1m\x1b[4m>\x1b[58:5:1m\x1b[3mbc\x1b(B\x1b[m\x1b[33m\x1b[44m       \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 112",
                expanded: "#defx ya",
                cols: 14,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#defx ya      \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[33m\x1b[44m\r\n#defx ya      \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 113",
                expanded: "a#[range=user|a]def#[range=left]#[range=user|a]>bc#[bogus]def#[noalign]#[align=centre]def\u{4e16}\u{754c}#[range=user|a]#[fg=#445566,us=red,underscore]e\u{301}",
                cols: 8,
                base: "fg=yellow,bg=blue",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1Hadef>bcd\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[33m\x1b[44m\x1b[5;1Hadef>bcd\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "user:a", "user:a", "user:a", "", "", "", ""]),
            },
            Golden {
                name: "random 114",
                expanded: " \u{1f44d}\u{1f3fd}#[noignore]#[ignore]def#[list=left-marker]#[list=right-marker]#[fg=#445566,us=red,underscore]#[list=on]#[list=on]#[align=centre]#[noignore]",
                cols: 7,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1H \xf0\x9f\x91\x8d\xf0\x9f\x8f\xbddef#\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l       \x1b[2;1H       \x1b[3;1H       \x1b[4;1H       \x1b[41m\x1b[5;1H \xf0\x9f\x91\x8d\xf0\x9f\x8f\xbddef#\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 115",
                expanded: "#[list=focus]#[list=focus]\u{1f44d}#[list=right-marker]",
                cols: 34,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\xf0\x9f\x91\x8d\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\xf0\x9f\x91\x8d\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: None,
            },
            Golden {
                name: "random 116",
                expanded: "<#[pop-default]#[bg=blue]#[fg=#445566,us=red,underscore]#[fg=#445566,us=red,underscore]a#[align=centre]#[align=centre]e\u{301}#[nolist]e\u{301}",
                cols: 16,
                base: "bg=red",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n<\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4ma\x1b(B\x1b[m\x1b[41m      \x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4me\xcc\x81e\xcc\x81\x1b(B\x1b[m\x1b[41m      \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[41m\r\n<\x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4ma\x1b(B\x1b[m\x1b[41m      \x1b[38;2;68;85;102m\x1b[44m\x1b[58:5:1m\x1b[4me\xcc\x81e\xcc\x81\x1b(B\x1b[m\x1b[41m      \x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: None,
            },
            Golden {
                name: "random 117",
                expanded: "ghij#[align=left]a #<ghij#[range=user|b]#[bg=blue]",
                cols: 8,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Hghija #<\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l        \x1b[2;1H        \x1b[3;1H        \x1b[4;1H        \x1b[5;1Hghija #<\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 118",
                expanded: "#[push-default]###[align=absolute-centre]bc#[align=left]#[bogus]#[nobold,italics]#[nobold,italics]#[align=right]#[range=left]#[align=right]#[pop-default]",
                cols: 24,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[10X\x1b[10Cbc\x1b[K\x1b[?12l\x1b[?25h\x1b[H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n#\x1b[10X\x1b[10Cbc\x1b[K\x1b[?12l\x1b[?25h\x1b[H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
            Golden {
                name: "random 119",
                expanded: "#[list=focus]#[range=user|b] #[ignore]#[noignore]#[range=user|a]#[bold]#[noignore]#[norange] #[fill=green]",
                cols: 40,
                base: "default",
                bytes: b"\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?996n\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[c\x1b[>c\x1b[>q\x1b[?2026$p\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n #[noignore]#[range=user|a]#[bold]#[noig\x1b[?12l\x1b[?25h\x1b[1;1H\x1b[18t\x1b[14t\x1b(B\x1b[m\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[1;1H\x1b[1;5r\x1b[1;1H\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n #[noignore]#[range=user|a]#[bold]#[noig\x1b[?12l\x1b[?25h\x1b[1;1H",
                ranges: Some(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""]),
            },
        ]
    }
    // END GENERATED styled_cases

    // BEGIN GENERATED cluster_cases
    // A reference pane (40x5) prints TEXT with printf; its #{cursor_x} and the first row of
    // capture-pane -p (trailing blanks trimmed). Pane output goes through the same
    // screen_write_cell / screen_write_combine as status lines.
    const CLUSTERS: &[(&str, &str, usize)] = &[
        ("ab", "ab", 2),
        ("a\u{4e16}b", "a\u{4e16}b", 4),
        ("e\u{301}x", "e\u{301}x", 2),
        ("\u{301}", "", 0),
        ("a\u{301}\u{302}", "a\u{301}\u{302}", 1),
        ("\u{200d}", "", 0),
        ("a\u{200d}b", "a\u{200d}b", 2),
        ("\u{fe0f}", "", 0),
        ("a\u{fe0f}b", "a\u{fe0f}b", 3),
        ("\u{2764}\u{fe0f}", "\u{2764}\u{fe0f}", 2),
        ("a\u{2764}\u{fe0f}b", "a\u{2764}\u{fe0f}b", 4),
        (
            "\u{2764}\u{fe0f}\u{2764}\u{fe0f}",
            "\u{2764}\u{fe0f}\u{2764}\u{fe0f}",
            4,
        ),
        ("\u{263a}\u{fe0f}", "\u{263a}\u{fe0f}", 2),
        ("1\u{fe0f}\u{20e3}", "1\u{fe0f}\u{20e3}", 2),
        ("\u{1f44d}", "\u{1f44d}", 2),
        ("\u{1f44d}\u{1f3fd}", "\u{1f44d}\u{1f3fd}", 2),
        ("a\u{1f44d}\u{1f3fd}b", "a\u{1f44d}\u{1f3fd}b", 4),
        ("\u{1f3fd}", "\u{1f3fd}", 2),
        ("\u{1f600}\u{1f3fd}", "\u{1f600}\u{1f3fd}", 4),
        (
            "\u{1f44d}\u{1f3fd}\u{1f3fd}",
            "\u{1f44d}\u{1f3fd}\u{1f3fd}",
            2,
        ),
        (
            "\u{1f468}\u{200d}\u{1f469}",
            "\u{1f468}\u{200d}\u{1f469}",
            2,
        ),
        (
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            2,
        ),
        (
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}\u{200d}\u{1f466}\u{200d}\u{1f466}",
            "\u{200d}\u{1f466}",
            2,
        ),
        (
            "\u{1f3f3}\u{fe0f}\u{200d}\u{1f308}",
            "\u{1f3f3}\u{fe0f}\u{200d}\u{1f308}",
            2,
        ),
        (
            "a\u{1f468}\u{200d}\u{1f469}b",
            "a\u{1f468}\u{200d}\u{1f469}b",
            4,
        ),
        ("\u{1f1ef}\u{1f1f5}", "\u{1f1ef}\u{1f1f5}", 2),
        (
            "\u{1f1ef}\u{1f1f5}\u{1f1fa}",
            "\u{1f1ef}\u{1f1f5}\u{1f1fa}",
            3,
        ),
        (
            "\u{1f1ef}\u{1f1f5}\u{1f1fa}\u{1f1f8}",
            "\u{1f1ef}\u{1f1f5}\u{1f1fa}\u{1f1f8}",
            4,
        ),
        ("\u{1f1ef}", "\u{1f1ef}", 1),
        ("\u{1f1ef}\u{1f600}", "\u{1f1ef}\u{1f600}", 3),
        ("\u{1100}\u{1161}", "\u{1100}\u{1161}", 2),
        ("\u{1100}\u{1161}\u{11a8}", "\u{1100}\u{1161}\u{11a8}", 2),
        ("\u{1161}", "", 0),
        ("\u{11a8}", "", 0),
        ("a\u{1161}", "a\u{1161}", 1),
        ("\u{1100}\u{1100}", "\u{1100}\u{1100}", 4),
        ("\u{3164}", "", 0),
        ("a\u{3164}b", "ab", 2),
        ("\u{4e16}\u{301}", "\u{4e16}\u{301}", 2),
        ("\u{4e16}\u{200d}\u{4e16}", "\u{4e16}\u{200d}\u{4e16}", 2),
        ("\u{e9}\u{301}", "\u{e9}\u{301}", 1),
        ("\u{200d}\u{200d}", "", 0),
        ("a\u{200d}\u{200d}\u{200d}", "a\u{200d}\u{200d}\u{200d}", 1),
        ("\u{1f44d}\u{200d}", "\u{1f44d}\u{200d}", 2),
        (
            "\u{1f44d}\u{200d}\u{1f3fd}",
            "\u{1f44d}\u{200d}\u{1f3fd}",
            2,
        ),
        ("\u{2b50}\u{fe0f}", "\u{2b50}\u{fe0f}", 2),
        (
            "xxxxx\u{4e16}\u{4e16}\u{4e16}",
            "xxxxx\u{4e16}\u{4e16}\u{4e16}",
            11,
        ),
    ];
    // END GENERATED cluster_cases

    #[test]
    fn clusters_match_a_reference_pane() {
        for &(text, shown, cursor) in CLUSTERS {
            let mut row = Vec::new();
            for c in text.chars() {
                write_char(&mut row, c, &CellStyle::default());
            }
            let got: String = row.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(got, shown, "text of {text:?}");
            assert_eq!(row.len(), cursor, "columns of {text:?}");
        }
    }

    // BEGIN GENERATED format_width
    // set -g @w S; set -gF @out '#{w:@w}'; show -gv @out  (format_width of the raw text)
    const FORMAT_WIDTHS: &[(&str, usize)] = &[
        ("ab", 2),
        ("#[fg=red]ab", 2),
        ("a#[bold]b#[default]c", 3),
        ("#[fg=red]#[bg=blue]", 0),
        ("##", 1),
        ("###", 2),
        ("####", 2),
        ("#####", 3),
        ("a##b", 3),
        ("a###b", 4),
        ("##[x", 3),
        ("####[x", 4),
        ("####[fg=red]x", 11),
        ("#####[fg=red]x", 3),
        ("#[unterminated", 0),
        ("ab#[unterminated", 0),
        ("#", 1),
        ("a#", 2),
        ("#x", 2),
        ("##x", 2),
        ("#[a]#[b]#[c]", 0),
        ("\u{4e16}#[bold]\u{754c}", 4),
        ("a#[fg=#ff0000]b", 2),
        ("#[range=user|a#,b]x", 1),
        ("#[align=right]ab#[default]", 2),
        ("#[x#]y]z", 3),
        ("tab\u{9}here", 7),
        ("e\u{301}#[bold]e\u{301}", 2),
        ("#[a#{b}c]d", 1),
        ("#[a#}b]c", 1),
        ("#[a##]b", 1),
    ];
    // END GENERATED format_width

    /// A cell as the terminal shows it, with colours as the client's terminal
    /// resolves them. The reference runs on a 256-colour/RGB terminal with a
    /// dark theme, so the theme and default colours become fixed values here.
    #[expect(clippy::struct_excessive_bools, reason = "one flag per SGR attribute")]
    #[derive(Debug, PartialEq, Eq, Clone, Default)]
    struct Seen {
        text: String,
        width: u8,
        fg: Color,
        bg: Color,
        us: Color,
        bold: bool,
        faint: bool,
        italic: bool,
        blink: bool,
        inverse: bool,
        invisible: bool,
        strike: bool,
        overline: bool,
        underline: vtmux_vt::Underline,
    }

    impl Seen {
        /// The second column of a wide character: the terminal keeps whatever colour
        /// was current there, which says nothing about tmux's model; only the fact
        /// that it is a spacer is compared.
        fn padding() -> Self {
            Self {
                width: 0,
                ..Self::default()
            }
        }
    }

    /// Dark theme with 256 colours (options-table.c:578-690 defaults).
    fn theme_colour(slot: u8) -> Color {
        let palette = [
            // black, white, light grey, dark grey, green, yellow, red, blue, cyan, magenta
            Colour::parse("gray5"),
            Colour::parse("gray90"),
            Colour::parse("gray70"),
            Colour::parse("gray15"),
            Colour::parse("yellowgreen"),
            Colour::parse("darkgoldenrod"),
            Colour::parse("indianred"),
            Colour::parse("skyblue3"),
            Colour::parse("cadetblue"),
            Colour::parse("mediumpurple"),
        ];
        let (r, g, b) = palette[usize::from(slot)].and_then(Colour::rgb).unwrap();
        Color::Rgb(r, g, b)
    }

    /// The colour as the terminal's cell would carry it. `terminal` and
    /// `default` are the terminal default, shown as `Color::Default`.
    fn to_vt(c: Colour) -> Color {
        match c {
            Colour::Default | Colour::Terminal => Color::Default,
            Colour::Theme(n) => theme_colour(n),
            Colour::Ansi(n) | Colour::Palette(n) => Color::Palette(n),
            Colour::Bright(n) => Color::Palette(8 + n),
            Colour::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }

    fn underline_of(a: Attrs) -> vtmux_vt::Underline {
        use vtmux_vt::Underline as U;
        // tmux sends the last underline kind it is asked for; when several are
        // set the order in tty_attributes decides (single wins).
        if a.contains(Attrs::UNDERSCORE) {
            U::Single
        } else if a.contains(Attrs::DOUBLE_UNDERSCORE) {
            U::Double
        } else if a.contains(Attrs::CURLY_UNDERSCORE) {
            U::Curly
        } else if a.contains(Attrs::DOTTED_UNDERSCORE) {
            U::Dotted
        } else if a.contains(Attrs::DASHED_UNDERSCORE) {
            U::Dashed
        } else {
            U::None
        }
    }

    /// What the terminal would show for one of our cells.
    fn render(cell: &Cell, bg_default: Colour) -> Seen {
        let st = &cell.style;
        let bg = if st.bg == Colour::Default {
            bg_default
        } else {
            st.bg
        };
        // A space with nothing but a background is an erased cell to the terminal
        // (tty_draw_line_get_empty): no text, no attributes, no foreground.
        let blank = cell.text == " " && st.attrs.is_empty() && st.link.is_none();
        if cell.width == 0 {
            return Seen::padding();
        }
        Seen {
            text: if blank {
                String::new()
            } else {
                cell.text.clone()
            },
            width: cell.width,
            fg: if blank { Color::Default } else { to_vt(st.fg) },
            bg: to_vt(bg),
            us: if blank { Color::Default } else { to_vt(st.us) },
            bold: !blank && st.attrs.contains(Attrs::BRIGHT),
            faint: !blank && st.attrs.contains(Attrs::DIM),
            italic: !blank && st.attrs.contains(Attrs::ITALICS),
            blink: !blank && st.attrs.contains(Attrs::BLINK),
            inverse: !blank && st.attrs.contains(Attrs::REVERSE),
            invisible: !blank && st.attrs.contains(Attrs::HIDDEN),
            strike: !blank && st.attrs.contains(Attrs::STRIKETHROUGH),
            overline: !blank && st.attrs.contains(Attrs::OVERLINE),
            underline: if blank {
                vtmux_vt::Underline::None
            } else {
                underline_of(st.attrs)
            },
        }
    }

    fn seen_from_vt(cell: &vtmux_vt::Cell) -> Seen {
        let s = &cell.style;
        if cell.width == 0 {
            return Seen::padding();
        }
        // tmux paints some blanks as erased cells and some as written spaces
        // (tty_draw_line_clear: EL/ECH or spaces); they look the same, so a
        // blank is a blank, without foreground or text.
        let blank = (cell.text.is_empty() || cell.text == " ") && cell.width == 1;
        let plain = blank
            && !s.bold
            && !s.faint
            && !s.italic
            && !s.blink
            && !s.inverse
            && !s.invisible
            && !s.strikethrough
            && !s.overline
            && s.underline == vtmux_vt::Underline::None;
        Seen {
            text: if plain {
                String::new()
            } else {
                cell.text.clone()
            },
            width: cell.width,
            fg: if plain { Color::Default } else { s.fg },
            bg: s.bg,
            us: if plain {
                Color::Default
            } else {
                s.underline_color
            },
            bold: s.bold,
            faint: s.faint,
            italic: s.italic,
            blink: s.blink,
            inverse: s.inverse,
            invisible: s.invisible,
            strike: s.strikethrough,
            overline: s.overline,
            underline: s.underline,
        }
    }

    /// The status row as the reference client's terminal drew it: the last row
    /// of the screen after replaying everything the client wrote.
    fn reference_row(g: &Golden) -> Vec<Seen> {
        let mut term = PaneTerm::new(g.cols, 5, 0);
        // tmux writes a ZWJ sequence or a skin-toned emoji as one cell's bytes and
        // relies on the terminal clustering them (mode 2027, grapheme clusters), as
        // terminals that support it do.
        term.feed(b"\x1b[?2027h");
        term.feed(g.bytes);
        let snap = snapshot(&term);
        (0..g.cols)
            .map(|x| seen_from_vt(snap.cell(x, 4).unwrap()))
            .collect()
    }

    /// `status_redraw`: `style_apply` over the default cell, so the base is just
    /// the `status-style` option the golden set (`default` is the terminal's own).
    fn base_style(g: &Golden) -> CellStyle {
        let mut sy = Style::default();
        sy.parse(&CellStyle::default(), g.base).unwrap();
        sy.cell
    }

    fn range_labels(line: &Line, cols: usize) -> Vec<String> {
        (0..cols)
            .map(|x| {
                line.ranges
                    .iter()
                    .find(|r| x >= r.start && x < r.end)
                    .map_or_else(String::new, |r| match &r.kind {
                        RangeKind::Left => "left".to_owned(),
                        RangeKind::Right => "right".to_owned(),
                        RangeKind::Control(_) => "control".to_owned(),
                        RangeKind::Pane(n) => format!("pane:%{n}"),
                        RangeKind::Window(n) => format!("window:{n}"),
                        RangeKind::Session(n) => format!("session:${n}"),
                        RangeKind::User(s) => format!("user:{s}"),
                    })
            })
            .collect()
    }

    #[test]
    fn styled_rows_match_the_reference_screen() {
        let mut failures = Vec::new();
        for g in golden_cases() {
            let base = base_style(&g);
            let line = draw(
                &base,
                usize::from(g.cols),
                g.expanded,
                g.ranges.is_some(),
                false,
            );
            let want = reference_row(&g);
            // The reference paints the row with the status style's colours; a
            // cell with the default background shows the style's own.
            let got: Vec<Seen> = line.cells.iter().map(|c| render(c, base.bg)).collect();
            if got != want {
                let at = got.iter().zip(&want).position(|(a, b)| a != b).unwrap();
                failures.push(format!(
                    "{}: first difference at column {at}\n  got  {:?}\n  want {:?}",
                    g.name, got[at], want[at]
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn range_goldens_match_the_reference() {
        let mut failures = Vec::new();
        let mut cases = 0;
        for g in golden_cases() {
            let Some(want) = g.ranges else { continue };
            cases += 1;
            let line = draw(
                &base_style(&g),
                usize::from(g.cols),
                g.expanded,
                true,
                false,
            );
            let got = range_labels(&line, usize::from(g.cols));
            let want: Vec<String> = want.iter().map(|l| (*l).to_owned()).collect();
            if got != want {
                failures.push(format!("{}:\n  got  {got:?}\n  want {want:?}", g.name));
            }
        }
        assert!(cases >= 10, "{cases}");
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn format_width_matches_reference() {
        for &(s, want) in FORMAT_WIDTHS {
            assert_eq!(format_width(s), want, "{s:?}");
        }
    }

    #[test]
    fn spans_group_equal_styles_and_keep_text() {
        let base = CellStyle::default();
        let line = draw(&base, 8, "#[fg=red]ab#[fg=blue]c", false, false);
        let spans = line.spans();
        assert_eq!(spans[0].text, "ab");
        assert_eq!(spans[0].style.fg, Colour::Ansi(1));
        assert_eq!(spans[1].text, "c");
        assert_eq!(spans.iter().map(|s| s.width).sum::<usize>(), 8);
    }

    #[test]
    fn golden_set_is_broad_enough() {
        let cases = golden_cases();
        assert!(cases.len() >= 80, "{}", cases.len());
        assert!(cases.iter().filter(|c| c.ranges.is_some()).count() >= 10);
    }

    /// Every `#[...]` of the owner's two configs (comments skipped, `#{@color_*}`
    /// and friends replaced by their literal hex) is accepted by `style_parse`;
    /// the generator checked each against the reference with `set -g status-style`.
    #[test]
    fn owner_config_styles_are_accepted() {
        assert!(OWNER_STYLES.len() >= 15, "{}", OWNER_STYLES.len());
        for style in OWNER_STYLES {
            let mut sy = Style::default();
            assert!(
                sy.parse(&CellStyle::default(), style).is_ok(),
                "style {style:?} rejected"
            );
            assert!(
                crate::style::validate_style_option(style).is_ok(),
                "{style:?}"
            );
        }
    }

    // BEGIN GENERATED owner_styles
    /// The distinct `#[...]` bodies of ~/dotfiles/tmux/.tmux.conf and mu-crew.conf; each was
    /// accepted by `set -g status-style` on the reference (`style_cases.sh`, `gen_owner`).
    const OWNER_STYLES: &[&str] = &[
        "fg=#11111b",
        "bg=#a6adc8",
        "bold",
        "fg=#a6adc8",
        "bg=#313244",
        "default",
        "fg=#89dceb,bg=#313244,bold",
        "fg=#6c7086",
        "fg=#b4befe",
        "bg=#585b70",
        "bg=#f38ba8",
        "fg=#fab387",
        "fg=#f9e2af",
        "fg=#f38ba8",
        "nobold",
        "fg=#eba0ac",
        "fg=#94e2d5",
        "fg=#74c7ec",
    ];
    // END GENERATED owner_styles
}
