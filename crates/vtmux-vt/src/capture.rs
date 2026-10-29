//! tmux-shaped capture of terminal contents (`capture-pane`).
//!
//! Mirrors `cmd_capture_pane_history` (cmd-capture-pane.c:315-413) and
//! `grid_string_cells` / `grid_string_cells_code` (grid.c:859-1260).

use libghostty_vt::Terminal;
use libghostty_vt::error::Error;
use libghostty_vt::screen::{CellContentTag, CellWide, GridRef};
use libghostty_vt::terminal::{Point, PointCoordinate};

use crate::snapshot::{Color, Style, Underline};
use crate::term::PaneTerm;

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
pub struct CaptureOpts {
    /// `-e`: emit SGR sequences for styles.
    pub escapes: bool,
    /// `-J`: join soft-wrapped rows and keep trailing spaces.
    pub join: bool,
    pub start: LineRef,
    pub end: LineRef,
}

impl Default for CaptureOpts {
    fn default() -> Self {
        Self {
            escapes: false,
            join: false,
            start: LineRef::Visible,
            end: LineRef::Visible,
        }
    }
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
    for y in top..=bottom {
        let (cells, wrapped) = read_row(term, y, hsize, cols);
        emit_row(&mut out, &cells, opts, &mut last_style);
        if !opts.join || !wrapped {
            out.push(b'\n');
        }
    }
    out
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

#[derive(Clone, Debug)]
pub(crate) struct RowCell {
    text: String,
    style: Style,
    /// Blank cell with no text (tmux stores a space).
    empty: bool,
}

impl RowCell {
    fn is_default_blank(&self) -> bool {
        self.empty && self.style == Style::default()
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
                text: String::new(),
                style: Style::default(),
                empty: true,
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
    // Ghostty may retain more history than tmux has: skip the oldest rows.
    let retained = term.term.scrollback_rows().unwrap_or(0);
    read_screen_row(&term.term, y + retained - hsize, cols)
}

/// Read one row of the active screen as tmux sees it: padding cells removed.
fn read_screen_row(term: &Terminal<'_, '_>, y: usize, cols: usize) -> (Vec<RowCell>, bool) {
    let mut cells = Vec::with_capacity(cols);
    let mut wrapped = false;
    let y = u32::try_from(y).unwrap_or(u32::MAX);
    for x in 0..cols {
        let Ok(gr) = term.grid_ref(Point::Screen(PointCoordinate {
            x: u16::try_from(x).unwrap_or(u16::MAX),
            y,
        })) else {
            break;
        };
        if x == 0 {
            wrapped = gr
                .row()
                .and_then(libghostty_vt::screen::Row::is_wrapped)
                .unwrap_or(false);
        }
        let Ok(cell) = gr.cell() else { continue };
        // Spacer cells are tmux's PADDING cells: skipped (grid.c:1219).
        if matches!(cell.wide(), Ok(CellWide::SpacerTail | CellWide::SpacerHead)) {
            continue;
        }
        let mut style = gr.style().map(Style::from).unwrap_or_default();
        match cell.content_tag() {
            Ok(CellContentTag::BgColorPalette) => {
                if let Ok(i) = cell.bg_color_palette() {
                    style.bg = Color::Palette(i.0);
                }
            }
            Ok(CellContentTag::BgColorRgb) => {
                if let Ok(c) = cell.bg_color_rgb() {
                    style.bg = Color::Rgb(c.r, c.g, c.b);
                }
            }
            _ => {}
        }
        let text = cell_text(&gr);
        cells.push(RowCell {
            empty: text.is_empty(),
            text,
            style,
        });
    }
    (cells, wrapped)
}

/// tmux's cell data size: `UTF8_SIZE` (tmux.h:721).
const UTF8_SIZE: usize = 32;

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
    let end = if opts.join {
        used
    } else if opts.escapes {
        tmux_cellsize(cells, used)
    } else {
        cells.len()
    };

    let mut line = Vec::new();
    for c in &cells[..end.min(cells.len())] {
        if opts.escapes {
            sgr_code(&mut line, last, &c.style);
            *last = c.style;
        }
        if c.empty {
            line.push(b' ');
        } else {
            line.extend_from_slice(c.text.as_bytes());
        }
    }
    if !opts.join {
        // GRID_STRING_TRIM_SPACES (grid.c:1263).
        while line.last() == Some(&b' ') {
            line.pop();
        }
    }
    out.extend_from_slice(&line);
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
    let extent = cells
        .iter()
        .rposition(|c| !c.empty || c.style.bg != Color::Default)
        .map_or(0, |i| i + 1)
        .max(used);
    let written = |c: &RowCell| !c.empty || c.style.bg != Color::Default;
    let mut size = 0;
    let mut i = 0;
    while i < extent {
        if !written(&cells[i]) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < extent && written(&cells[j]) && cells[j].style == cells[i].style {
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
