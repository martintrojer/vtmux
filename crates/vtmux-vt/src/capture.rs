//! tmux-shaped capture of terminal contents (`capture-pane`).
//!
//! Mirrors `cmd_capture_pane_history` (cmd-capture-pane.c:315-413) and
//! `grid_string_cells` / `grid_string_cells_code` (grid.c:859-1260).

use libghostty_vt::Terminal;
use libghostty_vt::error::Error;
use libghostty_vt::screen::{CellContentTag, CellWide, GridRef, HyperlinkId};
use libghostty_vt::terminal::{Point, PointCoordinate};

use crate::link::Links;
use crate::snapshot::{Cell, Color, Style, Underline};
use crate::text::CellText;
use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;
use std::sync::Arc;

use crate::term::{HistoryRow, LineMark, PaneTerm};

mod raw;
pub use raw::capture_grid;
pub(crate) use raw::dec_special_source;

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
    /// Where line `y` still reads from a copy of the pane's grid taken at
    /// this grid's width (copy mode's entry copy, for a line it has not
    /// read): the copy and its row. A capture reads a row of plain cells
    /// from there without building or keeping the line's [`Row`].
    fn entry_row(&self, _y: usize) -> Option<(&ScreenCopy, usize)> {
        None
    }
}

/// Where a capture reads (cmd-capture-pane.c:265-291).
enum Source<'a> {
    /// The pane's grid, history included, and its sizes.
    Pane(&'a PaneTerm, PaneRows),
    /// `-a`: the primary screen saved under the alternate one, no history,
    /// read `sx` columns wide (the pane's width now).
    Saved(Rc<Vec<FrozenRow>>, usize),
    /// `-M`: a mode's screen, `sx` columns.
    Mode(&'a dyn Lines, usize),
}

/// A pane's sizes, which hold while a capture borrows it: `hsize`, `sy`,
/// `sx`, the history rows Ghostty retains, and whether the alternate screen
/// is up.
#[derive(Clone, Copy)]
struct PaneRows {
    hsize: usize,
    rows: usize,
    cols: usize,
    retained: usize,
    alternate: bool,
}

impl PaneRows {
    fn of(term: &PaneTerm) -> Self {
        Self {
            hsize: term.history_size(),
            rows: usize::from(term.rows()),
            cols: usize::from(term.cols()),
            retained: term.term.scrollback_rows().unwrap_or(0),
            alternate: term.alternate_on(),
        }
    }

    /// Line `y`'s cells in `cells`, and whether it wraps; `last` is the
    /// row read before, which the next row is stepped to from (see
    /// [`row_ref_near`]); it becomes this row. `raw` and `cells` are kept
    /// from line to line, so a line costs no allocation.
    ///
    /// While the alternate screen is up, tmux keeps showing the primary
    /// history (screen.c:700-721 saves only the visible rows), but Ghostty
    /// can only read the active screen, so history comes from the copy
    /// taken on entry.
    fn read<'t>(
        &self,
        t: &'t PaneTerm,
        y: usize,
        last: &mut LastRow<'t>,
        raw: &mut Vec<libghostty_vt::screen::Cell>,
        cells: &mut Vec<RowCell>,
    ) -> bool {
        let Some((at, walk)) = self.screen_row(y) else {
            let wrapped;
            (*cells, wrapped) = t.frozen_row(y).map(FrozenRow::thaw).unwrap_or_default();
            return wrapped;
        };
        if y < self.hsize {
            t.note_history_read();
        }
        let Some(row_ref) = row_ref_stepped(&t.term, at, walk, last.as_ref()) else {
            cells.clear();
            return false;
        };
        let wrapped = read_cells_into(&row_ref, self.cols, false, raw, cells);
        *last = Some((at, row_ref));
        wrapped
    }

    /// [`PaneRows::read`] building its own cells.
    fn row<'t>(&self, t: &'t PaneTerm, y: usize, last: &mut LastRow<'t>) -> (Vec<RowCell>, bool) {
        let mut cells = Vec::new();
        let wrapped = self.read(t, y, last, &mut Vec::new(), &mut cells);
        (cells, wrapped)
    }

    /// The line flags line `y` keeps ([`LineKept`]); `last` is the row just
    /// read for it ([`PaneRows::read`]), which spares a lookup of a row of a
    /// long history.
    fn kept(&self, t: &PaneTerm, y: usize, last: &LastRow<'_>) -> LineKept {
        match self.screen_row(y) {
            None => t.frozen_row(y).map(|f| f.kept).unwrap_or_default(),
            Some((at, _)) => match last {
                Some((from, row_ref)) if *from == at => ref_kept(row_ref),
                _ => row_ref_at(&t.term, at)
                    .map(|r| ref_kept(&r))
                    .unwrap_or_default(),
            },
        }
    }

    /// The Ghostty screen row line `y` is (`y + retained - hsize`: Ghostty
    /// may retain more history than tmux has) and the rows a lookup of it
    /// walks (see [`row_ref_near`]); `None` for the primary history under
    /// the alternate screen, which is read from its copy.
    fn screen_row(&self, y: usize) -> Option<(usize, usize)> {
        if self.alternate && y < self.hsize {
            return None;
        }
        let at = y + self.retained - self.hsize;
        let walk = at
            .checked_sub(self.retained)
            .map_or(at, |active| active + self.rows);
        Some((at, walk))
    }
}

/// The cells a capture writes from their text, and their style alone, with
/// nothing else of them to look at (see [`CellBits::plain`]): one
/// codepoint or none, one column, no tab or line drawing.
#[derive(Clone, Copy)]
struct Plain {
    /// Any style and link, and a blank with only a colour, are plain too:
    /// without `-e`, `-N`, `-F` or `-H` they change nothing written.
    any_style: bool,
    /// Any style, and a blank with only a colour, are plain, and each
    /// cell's style is read with its text ([`StyleRow`]): `-e` or `-N`
    /// write or count them, but without `-F` or `-H` nothing else of a
    /// cell but its link is looked at.
    styled: bool,
    /// Only ASCII is plain: `-F` flags a character of more than one byte
    /// (`X`, [`row_extended`]).
    ascii: bool,
}

/// The style id bits of a Ghostty cell (see [`CellBits`]).
const STYLE_BITS: u64 = 0xffff << 26;

impl Plain {
    /// The bits of a Ghostty cell that change nothing written, or are read
    /// apart: the OSC 133 semantic bits, the style when it is plain, and
    /// the link when any is.
    fn ignored(self) -> u64 {
        const SEMANTIC: u64 = 0b11 << 46;
        const LINK: u64 = 1 << 45;
        SEMANTIC
            | if self.any_style { STYLE_BITS | LINK } else { 0 }
            | if self.styled { STYLE_BITS } else { 0 }
    }

    fn of(opts: CaptureOpts) -> Self {
        let looked_at = opts.flags || opts.hyperlinks;
        Self {
            any_style: !(opts.escapes || opts.keep_spaces || looked_at),
            styled: (opts.escapes || opts.keep_spaces) && !looked_at,
            ascii: opts.flags,
        }
    }

    /// Whether a line of this kind's plain cells can follow `last`, the
    /// cell written before: a style it writes from there, but no charset
    /// or link it would have to end (and, unless it keeps styles, no
    /// style).
    fn follows(self, last: &Gc, opts: CaptureOpts) -> bool {
        !opts.escapes
            || if self.styled {
                !last.acs && last.link.is_none()
            } else {
                *last == Gc::default()
            }
    }
}

/// The styles of a line of plain cells (`Plain::styled`): each cell's
/// index in `styles`, which holds each run of one style once.
#[derive(Default)]
struct StyleRow {
    styles: Vec<Style>,
    of: Vec<u16>,
}

impl StyleRow {
    fn clear(&mut self) {
        self.styles.clear();
        self.of.clear();
    }

    /// Add a cell of style `style`.
    fn push(&mut self, style: Style) {
        if self.styles.last() != Some(&style) {
            self.styles.push(style);
        }
        self.of
            .push(u16::try_from(self.styles.len() - 1).unwrap_or(u16::MAX));
    }

    fn style(&self, x: usize) -> &Style {
        &self.styles[usize::from(self.of[x])]
    }
}

/// tmux's `GRID_LINE_EXTENDED` and `GRID_LINE_HYPERLINK` as a row keeps them
/// (Ghostty patch 0034): a cell on the line needed tmux's extended form, or
/// had a link, since the line was last emptied. They stay set when that
/// cell is overwritten (grid.c `grid_extended_cell`), so readers OR them
/// with what the cells say now ([`row_extended`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LineKept {
    pub(crate) extended: bool,
    pub(crate) hyperlink: bool,
}

/// [`Source::plain_line`] of a mode's [`Row`].
fn mode_plain_cells(
    row: &Row,
    kind: Plain,
    out: &mut Vec<u32>,
    styles: &mut StyleRow,
) -> Option<bool> {
    out.clear();
    styles.clear();
    for c in &row.cells {
        let plain = !c.tab
            && c.width == 1
            && !c.acs
            && (kind.any_style
                || (c.link.is_none() && (kind.styled || c.style == Style::default())));
        if kind.styled {
            styles.push(c.style);
        }
        let mut chars = c.text.chars();
        out.push(match (plain, chars.next(), chars.next()) {
            (true, None, _) => 0,
            (true, Some(c), None) if !kind.ascii || c.is_ascii() => u32::from(c),
            _ => return None,
        });
    }
    Some(row.wrapped)
}

impl Source<'_> {
    /// Line `y` as one codepoint per cell when every cell of it is plain
    /// (`kind`); its cells as [`Source::line`] reads them, 0 for a blank,
    /// and whether it wraps. `None` when a cell is anything else, or the
    /// line is not read from a Ghostty row or a [`Row`]: the line then has
    /// to be read with [`Source::line`]. Builds no cells: a capture of a
    /// long history of text reads every line.
    fn plain_line<'t>(
        &'t self,
        y: usize,
        kind: Plain,
        last: &mut LastRow<'t>,
        raw: &mut Vec<libghostty_vt::screen::Cell>,
        out: &mut Vec<u32>,
        styles: &mut StyleRow,
    ) -> Option<bool> {
        match self {
            Self::Pane(t, p) => {
                let (at, walk) = p.screen_row(y)?;
                let row_ref = row_ref_stepped(&t.term, at, walk, last.as_ref())?;
                *last = Some((at, row_ref.clone()));
                let wrapped = plain_cells(&row_ref, p.cols, kind, raw, out, styles)?;
                if y < p.hsize {
                    t.note_history_read();
                }
                Some(wrapped)
            }
            Self::Saved(..) => None,
            Self::Mode(l, sx) => {
                let wrapped = match l.entry_row(y) {
                    Some((copy, at)) => copy.plain_cells(at, kind, raw, out, styles)?,
                    None => mode_plain_cells(l.line(y), kind, out, styles)?,
                };
                if kind.styled {
                    for _ in out.len()..*sx {
                        styles.push(Style::default());
                    }
                }
                out.resize((*sx).max(out.len()), 0);
                Some(wrapped)
            }
        }
    }

    /// Line `y`'s history time and OSC 133 prompt and output marks, as
    /// `-I` and `-F` need them.
    fn line_meta(
        &self,
        y: usize,
        marks: &[(usize, LineMark)],
        opts: CaptureOpts,
    ) -> (i64, bool, bool) {
        let marked = |m: LineMark| marks.iter().any(|&(my, mm)| my == y && mm == m);
        match self {
            Self::Pane(t, p) => {
                let time = if opts.times && y < p.hsize {
                    t.history_time(t.history_id(y))
                } else {
                    0
                };
                (time, marked(LineMark::Prompt), marked(LineMark::Output))
            }
            Self::Saved(..) => (0, false, false),
            Self::Mode(l, _) => (l.time(y), l.starts(y, false), l.starts(y, true)),
        }
    }

    /// The line flags line `y` keeps ([`LineKept`]); `last` is the row just
    /// read for it ([`Source::line`], [`Source::plain_line`]), which spares
    /// a lookup of a row of a long history.
    fn line_kept(&self, y: usize, last: &LastRow<'_>) -> LineKept {
        match self {
            Self::Pane(t, p) => p.kept(t, y, last),
            Self::Saved(rows, _) => rows.get(y).map(|r| r.kept).unwrap_or_default(),
            Self::Mode(..) => LineKept::default(),
        }
    }

    /// `hsize` and `sy`.
    fn size(&self) -> (usize, usize) {
        match self {
            Self::Pane(_, p) => (p.hsize, p.rows),
            Self::Saved(rows, _) => (0, rows.len()),
            Self::Mode(l, _) => (l.history_size(), l.screen_rows()),
        }
    }

    /// Line `y`'s cells in `cells`, and whether it wraps, as
    /// [`PaneRows::read`] reads a pane's.
    fn line<'t>(
        &'t self,
        y: usize,
        last: &mut LastRow<'t>,
        raw: &mut Vec<libghostty_vt::screen::Cell>,
        cells: &mut Vec<RowCell>,
    ) -> bool {
        match self {
            Self::Pane(t, p) => p.read(t, y, last, raw, cells),
            Self::Saved(rows, sx) => {
                let wrapped;
                (*cells, wrapped) = rows.get(y).map(FrozenRow::thaw).unwrap_or_default();
                // grid_string_cells reads the pane's width of a line
                // whatever its own, so a narrower pane cuts it.
                cells.truncate(*sx);
                wrapped
            }
            Self::Mode(l, sx) => {
                let row = l.line(y);
                cells.clear();
                for c in &row.cells {
                    let after_tab = cells.last().is_some_and(|p: &RowCell| p.tab != 0);
                    cells.push(RowCell::from_cell(c, after_tab));
                }
                let blank = RowCell {
                    empty: true,
                    ..RowCell::default()
                };
                cells.resize((*sx).max(cells.len()), blank);
                row.wrapped
            }
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
    capture_from(&Source::Pane(term, PaneRows::of(term)), opts)
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
        Source::Pane(t, _) if opts.flags => t.line_marks(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    // tmux threads one `lastgc` through every line of a capture
    // (cmd-capture-pane.c:333), and one list of the links -H has listed.
    let mut last = Gc::default();
    let mut seen: Vec<LinkKey> = Vec::new();
    let mut at = None;
    let (mut raw, mut plain, mut cells, mut links) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut styles = StyleRow::default();
    let kind = Plain::of(opts);
    for y in top..=bottom {
        // A row of plain cells needs no sequence but its styles' when the
        // cell before had no charset or link, and lists no link.
        let plain_wrapped = if kind.follows(&last, opts) && plain_lines() {
            src.plain_line(y, kind, &mut at, &mut raw, &mut plain, &mut styles)
        } else {
            None
        };
        #[cfg(test)]
        if plain_wrapped.is_some() {
            tests::PLAIN_ROWS.with(|n| n.set(n.get() + 1));
        }
        let wrapped = match plain_wrapped {
            Some(_) if opts.hyperlinks => continue,
            Some(wrapped) => wrapped,
            None => {
                let wrapped = src.line(y, &mut at, &mut raw, &mut cells);
                if opts.hyperlinks {
                    links.clear();
                    line_links(&mut links, &cells, &mut seen);
                    if links.is_empty() {
                        continue;
                    }
                }
                wrapped
            }
        };
        // cmd-capture-pane.c:363-401: the line's number, history time and
        // flags first.
        if opts.numbers || opts.times || opts.flags {
            let (time, prompt, output) = src.line_meta(y, &marks, opts);
            if opts.numbers {
                let n = i64::try_from(y).unwrap_or(0) - i64::try_from(hsize).unwrap_or(0);
                let _ = write!(out, "{n} ");
            }
            if opts.times {
                let _ = write!(out, "{time} ");
            }
            if opts.flags {
                let cells: &[RowCell] = if plain_wrapped.is_some() { &[] } else { &cells };
                let kept = src.line_kept(y, &at);
                out.extend_from_slice(row_flags(cells, wrapped, kept, prompt, output).as_bytes());
            }
        }
        if plain_wrapped.is_some() && kind.styled {
            emit_styled(&mut out, &plain, &styles, opts, &mut last);
        } else if plain_wrapped.is_some() {
            emit_plain(&mut out, &plain, opts);
        } else if opts.hyperlinks {
            out.extend_from_slice(&links);
        } else {
            emit_row(&mut out, &cells, opts, &mut last);
        }
        if !opts.join || !wrapped {
            out.push(b'\n');
        }
    }
    out
}

/// Whether a capture writes lines of plain cells without building their
/// cells; tests turn it off to compare with the general path.
fn plain_lines() -> bool {
    #[cfg(test)]
    return !tests::CELLS_ONLY.with(std::cell::Cell::get);
    #[cfg(not(test))]
    true
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
        out.extend_from_slice(vis_link(&key.0).as_bytes());
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
    let p = PaneRows::of(term);
    let rows = p.hsize + p.rows;
    let mut counts = HistoryCounts {
        lines: rows,
        ..HistoryCounts::default()
    };
    // A line of unstyled ASCII without a link (most of a long history) is
    // counted from Ghostty's cells without building any: tmux sums sizes
    // it keeps per line (format.c `format_cb_history_bytes`), and building
    // every cell of a 100000-line history took longer than a format may
    // run (format.c `format_check_time`), cutting short the rest of the
    // line `list-panes` prints.
    let kind = Plain {
        any_style: false,
        styled: false,
        ascii: true,
    };
    let src = Source::Pane(term, p);
    let (mut raw, mut plain) = (Vec::new(), Vec::new());
    let mut styles = StyleRow::default();
    let mut last = None;
    for y in 0..rows {
        if plain_lines()
            && src
                .plain_line(y, kind, &mut last, &mut raw, &mut plain, &mut styles)
                .is_some()
        {
            #[cfg(test)]
            tests::PLAIN_ROWS.with(|n| n.set(n.get() + 1));
            // No cell needs the extended form ([`cell_extended`]).
            let used = plain.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
            counts.cells += plain_cellsize(&plain, used).max(used);
            continue;
        }
        let (cells, _) = p.row(term, y, &mut last);
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
/// space (`X`: [`row_extended`] or kept, `H` likewise). vtmux has no dead
/// lines (`D`).
fn row_flags(
    cells: &[RowCell],
    wrapped: bool,
    kept: LineKept,
    prompt: bool,
    output: bool,
) -> String {
    let mut f = String::new();
    for (on, c) in [
        (kept.hyperlink || cells.iter().any(|c| c.link), 'H'),
        (output, 'O'),
        (prompt, 'P'),
        (wrapped, 'W'),
        (kept.extended || row_extended(cells), 'X'),
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
    /// The link drawn for this one, from `links`; read on the alternate
    /// screen or not.
    fn link(&self, links: &RefCell<Links>, alternate: bool) -> Option<Arc<crate::Link>> {
        links
            .borrow_mut()
            .get(self.uri.as_deref()?, self.id.as_ref()?, alternate)
    }

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
            link: c.link.is_some(),
            target: None,
            tab,
        }
    }

    /// The cell as the renderer sees it (as [`crate::snapshot`] reads it),
    /// its link from `links` (read on the alternate screen or not).
    fn to_cell(&self, links: &RefCell<Links>, alternate: bool) -> Cell {
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
            link: self
                .target
                .as_deref()
                .and_then(|t| t.link(links, alternate)),
            tab,
        }
    }
}

/// [`RowCell::to_cell`] of each of `cells`.
fn to_cells(cells: &[RowCell], links: &RefCell<Links>, alternate: bool) -> Vec<Cell> {
    cells.iter().map(|c| c.to_cell(links, alternate)).collect()
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
            cells: to_cells(&cells, &self.links, false),
            wrapped,
        })
    }

    /// Row `y` of tmux's grid: `0..history_size()` is history, oldest
    /// first, then the visible rows. While the alternate screen is up the
    /// history is the primary screen's, copied on entry. Out of range is an
    /// empty row.
    #[must_use]
    pub fn row(&self, y: usize) -> Row {
        self.rows_in(y..y + 1).next().unwrap_or_default()
    }

    /// Rows `range` of tmux's grid as [`PaneTerm::row`] gives them, each
    /// stepped to from the one before (see [`row_ref_near`]): reading all
    /// of a long history this way costs a page-list step per row, not a
    /// walk from the oldest page.
    pub fn rows_in(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = Row> + '_ {
        let p = PaneRows::of(self);
        let mut last = None;
        range.map(move |y| {
            if y >= p.hsize + p.rows {
                return Row::default();
            }
            let (cells, wrapped) = p.row(self, y, &mut last);
            Row {
                cells: to_cells(&cells, &self.links, y >= p.hsize && p.alternate),
                wrapped,
            }
        })
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
    /// `y + retained - hsize` (see [`PaneRows::screen_row`]).
    retained: usize,
    /// The primary history copied when the alternate screen came up, which
    /// is tmux's history while it is up (see [`PaneRows::read`]).
    frozen: Option<Rc<Vec<FrozenRow>>>,
    hsize: usize,
    rows: usize,
    cols: usize,
    /// `grid_line_time` of each history row.
    times: Vec<i64>,
    marks: Vec<(usize, LineMark)>,
    /// The screen row of `term` read last, which the next read steps from
    /// (see [`row_ref_near`]). Read only through [`ScreenCopy::row_ref`].
    last: RefCell<LastRow<'static>>,
    /// The cells and codepoints of the row [`ScreenCopy::text`] read
    /// last, kept for the next: a search reads every row.
    scratch: RefCell<(Vec<libghostty_vt::screen::Cell>, Vec<u32>)>,
    /// The pane's links ([`PaneTerm::drawn_link`]).
    links: Rc<RefCell<Links>>,
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
            cells: to_cells(&cells, &self.links, self.alternate_at(y)),
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
            cells: to_cells(&cells[..used], &self.links, self.alternate_at(y)),
            wrapped,
        }
    }

    /// Row `y` is a row of the alternate screen (history is the primary's).
    fn alternate_at(&self, y: usize) -> bool {
        self.frozen.is_some() && y >= self.hsize
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
            (_, Some(_)) => match self.row_ref(y + self.retained - self.hsize) {
                Some(row_ref) => read_cells(&row_ref, self.cols, used_only),
                None => (Vec::new(), false),
            },
            (_, None) => (Vec::new(), false),
        }
    }

    /// The reference to screen row `y` of the copy's terminal, stepped to
    /// from the row read before it when that is nearer than a lookup
    /// ([`row_ref_near`]): a search or a reflow reads the rows in order.
    fn row_ref(&self, y: usize) -> Option<GridRef<'_>> {
        let term = self.term.as_ref()?;
        let last = self.last.borrow().clone();
        let row_ref = row_ref_near(term, y, last.as_ref())?;
        // SAFETY: a `GridRef` is a plain pointer into `term`'s page list
        // (no drop); the lifetime is only a borrow of `term`. `term` is
        // owned by this copy and never changed after it is taken (it is
        // only read, through `&self`; reading a compressed page restores it
        // in place, keeping its node), so the reference stays valid for as
        // long as the copy lives, and it leaves `last` only through this
        // method, shortened to a borrow of `self`.
        let keep = unsafe { std::mem::transmute::<GridRef<'_>, GridRef<'static>>(row_ref.clone()) };
        *self.last.borrow_mut() = Some((y, keep));
        Some(row_ref)
    }

    /// Row `y` as the text a search reads (`window_copy_stringify`: a
    /// space for a blank, the character of any other cell), appended to
    /// `out` (cleared first), and whether the row wraps onto the next.
    /// Every cell is one character of one column, so `out` holds `cols`
    /// characters, and is ASCII exactly when it is `cols` bytes long.
    /// `None` when a cell is anything else (a wide, grapheme, tab or
    /// line-drawing cell: [`CellBits::plain`]), or the row cannot be read
    /// from the copy; the row then has to be read with
    /// [`ScreenCopy::used_row`]. Styles and links are not looked at. It
    /// costs one step along the page list and one call for the cells, and
    /// builds no cell structures: a search over a long history reads every
    /// line.
    pub fn text(&self, y: usize, out: &mut String) -> Option<bool> {
        out.clear();
        if y >= self.hsize + self.rows || (self.frozen.is_some() && y < self.hsize) {
            return None;
        }
        let row_ref = self.row_ref(y + self.retained - self.hsize)?;
        let wrapped = row_ref.row().ok()?.is_wrapped().ok()?;
        let (raw, codepoints) = &mut *self.scratch.borrow_mut();
        row_ref.row_cells(raw).ok()?;
        // An incomplete reflow can leave the page narrower than the screen.
        raw.truncate(self.cols);
        let kind = Plain {
            any_style: true,
            styled: false,
            ascii: false,
        };
        plain_codepoints(raw, kind, codepoints)?;
        out.reserve(self.cols);
        // A blank is a space; any other codepoint is a character.
        out.extend(codepoints.iter().map(|&cp| match cp {
            0 => ' ',
            cp => char::from_u32(cp).unwrap_or(' '),
        }));
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

    /// Row `y` as [`Source::plain_line`] reads a pane's.
    fn plain_cells(
        &self,
        y: usize,
        kind: Plain,
        raw: &mut Vec<libghostty_vt::screen::Cell>,
        out: &mut Vec<u32>,
        styles: &mut StyleRow,
    ) -> Option<bool> {
        if y >= self.hsize + self.rows || (self.frozen.is_some() && y < self.hsize) {
            return None;
        }
        let row_ref = self.row_ref(y + self.retained - self.hsize)?;
        plain_cells(&row_ref, self.cols, kind, raw, out, styles)
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

    /// The link of the cell at `(x, y)` of the visible screen, which has
    /// one, as the outer terminal is given it ([`crate::Link`]).
    pub(crate) fn drawn_link(&self, (x, y): (usize, usize)) -> Option<Arc<crate::Link>> {
        let gr = self
            .term
            .grid_ref(Point::Viewport(PointCoordinate {
                x: u16::try_from(x).ok()?,
                y: u32::try_from(y).ok()?,
            }))
            .ok()?;
        let target = LinkTarget::read(&gr)?;
        target.link(&self.links, self.alternate_on())
    }

    /// The link this pane's cells with `uri` were drawn with, when only one
    /// link has it: for lines that keep only the URI (a copy mode restored
    /// from a snapshot).
    #[must_use]
    pub fn drawn_link_of(&self, uri: &str) -> Option<Arc<crate::Link>> {
        self.links.borrow().by_uri(uri)
    }

    /// The counter this pane's links take their numbers from.
    #[must_use]
    pub fn link_numbers(&self) -> crate::LinkNumbers {
        self.links.borrow().numbers.clone()
    }

    /// Number this pane's links from `numbers` (the server's, tmux's one
    /// counter), which moves past every number the pane has given.
    pub fn number_links_from(&mut self, numbers: &crate::LinkNumbers) {
        let mut links = self.links.borrow_mut();
        numbers.reach(links.numbers.next());
        links.numbers = numbers.clone();
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
            last: RefCell::new(None),
            scratch: RefCell::default(),
            links: Rc::clone(&self.links),
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
    pub(crate) kept: LineKept,
}

impl FrozenRow {
    /// Copy screen row `y` (history included) of the active screen;
    /// `last` as for [`read_screen_row_near`].
    pub(crate) fn capture<'t>(
        term: &'t Terminal<'_, '_>,
        y: usize,
        cols: usize,
        last: &mut LastRow<'t>,
    ) -> Self {
        let (mut cells, wrapped) = read_screen_row_near(term, y, cols, last);
        let kept = match last {
            Some((at, row_ref)) if *at == y => ref_kept(row_ref),
            _ => LineKept::default(),
        };
        let len = cells.len();
        while cells.last().is_some_and(RowCell::is_default_blank) {
            cells.pop();
        }
        Self {
            cells,
            len,
            wrapped,
            kept,
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
            kept_extended: self.kept.extended,
            kept_hyperlink: self.kept.hyperlink,
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
            kept: LineKept {
                extended: state.kept_extended,
                hyperlink: state.kept_hyperlink,
            },
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

/// [`read_screen_row`], the row stepped to from `last` when nearer
/// ([`row_ref_near`]); `last` becomes this row.
fn read_screen_row_near<'t>(
    term: &'t Terminal<'_, '_>,
    y: usize,
    cols: usize,
    last: &mut LastRow<'t>,
) -> (Vec<RowCell>, bool) {
    let Some(row_ref) = row_ref_near(term, y, last.as_ref()) else {
        return (Vec::new(), false);
    };
    let row = read_cells(&row_ref, cols, false);
    *last = Some((y, row_ref));
    row
}

/// A screen row of a terminal (history first, counted from the oldest row
/// Ghostty keeps) and the reference to it: the row a reader resolved last.
pub(crate) type LastRow<'t> = Option<(usize, GridRef<'t>)>;

/// The reference to row `y` of a screen, as [`row_ref_at`] gives it, but
/// stepped to from `last` (patch 0033's `ghostty_grid_ref_down` and `_up`)
/// when that crosses no more rows than the lookup's walk would: a lookup
/// walks the page list from the oldest page (or, for the active area, from
/// the newest), so a reader going through a long history in order with
/// lookups alone took time quadratic in its length. `last` must come from
/// the same terminal, unchanged since.
fn row_ref_near<'t>(
    term: &'t Terminal<'_, '_>,
    y: usize,
    last: Option<&(usize, GridRef<'t>)>,
) -> Option<GridRef<'t>> {
    // The rows the lookup's walk crosses: the history above a history
    // row, the screen and the screen rows above an active row.
    let walk = y
        .checked_sub(term.scrollback_rows().unwrap_or(0))
        .map_or(y, |active| active + usize::from(term.rows().unwrap_or(0)));
    row_ref_stepped(term, y, walk, last)
}

/// [`row_ref_near`], `walk` the rows a lookup of row `y` crosses.
fn row_ref_stepped<'t>(
    term: &'t Terminal<'_, '_>,
    y: usize,
    walk: usize,
    last: Option<&(usize, GridRef<'t>)>,
) -> Option<GridRef<'t>> {
    if let Some((from, from_ref)) = last {
        let rows = y.abs_diff(*from);
        if rows <= walk {
            #[cfg(test)]
            tests::STEPS.with(|n| n.set(n.get() + 1));
            let stepped = if y >= *from {
                from_ref.down(rows)
            } else {
                from_ref.up(rows)
            };
            if let Ok(row_ref) = stepped {
                return Some(row_ref);
            }
        }
    }
    row_ref_at(term, y)
}

/// The line flags the row `row_ref` is of keeps.
fn ref_kept(row_ref: &GridRef<'_>) -> LineKept {
    row_ref
        .row()
        .ok()
        .and_then(|row| row.tmux_line_flags().ok())
        .map(|(extended, hyperlink)| LineKept {
            extended,
            hyperlink,
        })
        .unwrap_or_default()
}

/// The reference to row `y` of a screen (history first, `y` counted from
/// the oldest row Ghostty keeps). Ghostty resolves a screen point by
/// walking its pages from the oldest and an active point from the newest:
/// a row of the active area is read as one, at a cost that does not grow
/// with the history.
fn row_ref_at<'t>(term: &'t Terminal<'_, '_>, y: usize) -> Option<GridRef<'t>> {
    #[cfg(test)]
    tests::LOOKUPS.with(|n| n.set(n.get() + 1));
    term.grid_ref(screen_point(term, 0, y)).ok()
}

/// Ghostty's point for cell `x` of screen row `y`, as [`row_ref_at`]
/// looks it up: a history row as a screen point, an active row as an
/// active one.
fn screen_point(term: &Terminal<'_, '_>, x: u16, y: usize) -> Point {
    let (at, y): (fn(PointCoordinate) -> Point, usize) =
        match y.checked_sub(term.scrollback_rows().unwrap_or(0)) {
            Some(active) => (Point::Active, active),
            None => (Point::Screen, y),
        };
    at(PointCoordinate {
        x,
        y: u32::try_from(y).unwrap_or(u32::MAX),
    })
}

/// Read one row of the active screen, spacer cells included: one page-list
/// walk for the row and one call for its cells; a cell's style and
/// grapheme are looked up only when it has them.
fn read_screen_row(term: &Terminal<'_, '_>, y: usize, cols: usize) -> (Vec<RowCell>, bool) {
    match row_ref_at(term, y) {
        Some(row_ref) => read_cells(&row_ref, cols, false),
        None => (Vec::new(), false),
    }
}

/// The cells of the row `row_ref` is on; with `used_only`, the cells
/// Ghostty has nothing in after the last it has something in are not read
/// (they are blanks, and a tab is judged by the whole row's width all the
/// same).
fn read_cells(row_ref: &GridRef<'_>, cols: usize, used_only: bool) -> (Vec<RowCell>, bool) {
    let mut cells = Vec::new();
    let wrapped = read_cells_into(row_ref, cols, used_only, &mut Vec::new(), &mut cells);
    (cells, wrapped)
}

/// [`read_cells`] into `cells`, Ghostty's cells read into `raw`: a reader
/// of many rows keeps both.
fn read_cells_into(
    row_ref: &GridRef<'_>,
    cols: usize,
    used_only: bool,
    raw: &mut Vec<libghostty_vt::screen::Cell>,
    cells: &mut Vec<RowCell>,
) -> bool {
    #[cfg(test)]
    tests::CELL_ROWS.with(|n| n.set(n.get() + 1));
    cells.clear();
    let (wrapped, styled, linked) = row_ref.row().map_or((false, true, true), |row| {
        (
            row.is_wrapped().unwrap_or(false),
            // Per-row flags let most cells skip the style and link lookups.
            row.is_styled().unwrap_or(true),
            row.has_hyperlink().unwrap_or(true),
        )
    });
    if row_ref.row_cells(raw).is_err() {
        return wrapped;
    }
    // An incomplete reflow can leave the page narrower than the screen.
    raw.truncate(cols);
    let width = raw.len();
    if used_only {
        raw.truncate(raw.iter().rposition(|c| c.bits() != 0).map_or(0, |x| x + 1));
    }
    cells.reserve(raw.len());
    // The last style looked up, by its id in the row's page: a run of
    // cells of one style costs one lookup.
    let mut last_style: Option<(u32, Style)> = None;
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
            let id = cell.field(26, 16);
            match last_style {
                Some((last, style)) if last == id => style,
                _ => {
                    let style = gr.style().map(Style::from).unwrap_or_default();
                    last_style = Some((id, style));
                    style
                }
            }
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
    whole_tabs(cells, width, wrapped);
    wrapped
}

/// The cells of the row at `row_ref`, `cols` at most, as one codepoint
/// each ([`CellBits::plain`]) in `out`, and whether the row wraps; `None`
/// when a cell is not plain or the row does not read.
fn plain_cells(
    row_ref: &GridRef<'_>,
    cols: usize,
    kind: Plain,
    raw: &mut Vec<libghostty_vt::screen::Cell>,
    out: &mut Vec<u32>,
    styles: &mut StyleRow,
) -> Option<bool> {
    let row = row_ref.row().ok()?;
    let wrapped = row.is_wrapped().ok()?;
    row_ref.row_cells(raw).ok()?;
    // An incomplete reflow can leave the page narrower than the screen.
    raw.truncate(cols);
    plain_codepoints(raw, kind, out)?;
    if kind.styled {
        // As read_cells reads them: per-row flags let most cells skip
        // the style lookup.
        cell_styles(row_ref, raw, row.is_styled().unwrap_or(true), styles);
    }
    Some(wrapped)
}

/// The codepoint of each of the cells `raw`, 0 for a blank, in `out`, when
/// every one is plain for `kind` ([`CellBits::plain`]); `None` when one is
/// not. The usual rows go in two passes without a branch per cell: every
/// cell the codepoint tag and a codepoint (ASCII, or any if `kind` takes
/// any) or none, with nothing else that `kind` looks at; only another row
/// is looked at cell by cell.
fn plain_codepoints(
    raw: &[libghostty_vt::screen::Cell],
    kind: Plain,
    out: &mut Vec<u32>,
) -> Option<()> {
    let looked_at = !kind.ignored();
    // Used only once every cell's bits past the codepoint are known clear.
    #[expect(clippy::cast_possible_truncation, reason = "21 bits")]
    let codepoint = |c: &libghostty_vt::screen::Cell| ((c.bits() & looked_at) >> 2) as u32;
    out.clear();
    let bits = raw.iter().fold(0, |acc, c| acc | (c.bits() & looked_at));
    let max: u64 = if kind.ascii { 0x7f } else { 0x1f_ffff };
    if bits & !(max << 2) == 0
        && (bits < 0x80 << 2
            || raw
                .iter()
                .fold(true, |ok, c| ok & char::from_u32(codepoint(c)).is_some()))
    {
        out.extend(raw.iter().map(codepoint));
        return Some(());
    }
    out.resize(raw.len(), 0);
    for (o, cell) in out.iter_mut().zip(raw) {
        *o = CellBits(cell.bits()).plain(kind)?;
    }
    Some(())
}

/// The style of each of the plain cells `raw` of the row at `row_ref`, as
/// [`read_cells`] gives it, in `styles`: a run of cells of one style id
/// (and one colour, for a blank with only a colour) costs one lookup.
fn cell_styles(
    row_ref: &GridRef<'_>,
    raw: &[libghostty_vt::screen::Cell],
    row_styled: bool,
    styles: &mut StyleRow,
) {
    styles.clear();
    if raw
        .iter()
        .fold(0, |acc, c| acc | (c.bits() & (STYLE_BITS | 0b11)))
        == 0
    {
        // No style and no colour-only blank: the usual row.
        styles.styles.push(Style::default());
        styles.of.resize(raw.len(), 0);
        return;
    }
    let mut last: Option<(u64, Style)> = None;
    for (x, cell) in raw.iter().enumerate() {
        let cell = CellBits(cell.bits());
        let colour_only = !matches!(cell.content_tag(), CellContentTag::Codepoint);
        let key = cell.0 & (STYLE_BITS | if colour_only { (1 << 26) - 1 } else { 0 });
        let style = match last {
            Some((k, style)) if k == key => style,
            _ => {
                let mut style = if row_styled && cell.has_styling() {
                    row_ref
                        .with_x(u16::try_from(x).unwrap_or(u16::MAX))
                        .style()
                        .map(Style::from)
                        .unwrap_or_default()
                } else {
                    Style::default()
                };
                match cell.content_tag() {
                    CellContentTag::BgColorPalette => {
                        style.bg = Color::Palette(cell.palette()).in_form(cell.palette_256());
                    }
                    CellContentTag::BgColorRgb => {
                        let (r, g, b) = cell.rgb();
                        style.bg = Color::Rgb(r, g, b);
                    }
                    CellContentTag::Codepoint | CellContentTag::CodepointGrapheme => {}
                }
                last = Some((key, style));
                style
            }
        };
        styles.push(style);
    }
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

    /// The cell's codepoint, 0 for none (a blank), when [`read_cells`]
    /// makes it a narrow cell of one codepoint or none, no charset or tab,
    /// of the default style and no link unless `kind` takes any (and then
    /// a cell of only a background colour is a blank), and ASCII if `kind`
    /// says. `None` for any other cell. The OSC 133 semantic bits are
    /// ignored, as `read_cells` ignores them.
    fn plain(self, kind: Plain) -> Option<u32> {
        let bits = self.0 & !kind.ignored();
        // The usual cell first: the codepoint tag (0), an ASCII codepoint
        // or none, nothing else.
        if bits < 0x80 << 2 && bits.trailing_zeros() >= 2 {
            return Some(u32::try_from(bits >> 2).expect("below 0x80"));
        }
        match self.content_tag() {
            // Past the 21-bit codepoint: the rest of the content, the
            // width, the charset mark and the tab.
            CellContentTag::Codepoint if bits >> 23 == 0 => {
                let cp = u32::try_from(bits >> 2).expect("21 bits");
                let c = char::from_u32(cp)?;
                (!kind.ascii || c.is_ascii()).then_some(cp)
            }
            CellContentTag::BgColorPalette | CellContentTag::BgColorRgb
                if (kind.any_style || kind.styled) && bits >> 26 == 0 =>
            {
                Some(0)
            }
            _ => None,
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
    let gr = term
        .grid_ref(screen_point(term, u16::try_from(x).ok()?, y))
        .ok()?;
    hyperlink_uri(&gr)
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

/// Line `cells` appended to `line` (`grid_string_cells`).
fn emit_row(line: &mut Vec<u8>, cells: &[RowCell], opts: CaptureOpts, last: &mut Gc) {
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

    let start = line.len();
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
            sgr_code(line, &last.style, &gc.style);
            if opts.octal {
                escape_esc(line, at);
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
                    has_link = add_hyperlink(
                        line,
                        vis_link(&String::from_utf8_lossy(id)).as_bytes(),
                        vis_link(uri).as_bytes(),
                        opts.octal,
                    );
                } else if has_link {
                    add_hyperlink(line, b"", b"", opts.octal);
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
        line.extend_from_within(code_at..code_at + code_len);
        add_hyperlink(line, b"", b"", opts.octal);
    }
    if !opts.join && !opts.keep_spaces {
        // GRID_STRING_TRIM_SPACES (grid.c:1263).
        while line.len() > start && line.last() == Some(&b' ') {
            line.pop();
        }
    }
}

/// [`emit_row`] for a line of plain cells (one codepoint each, 0 a blank:
/// see [`Source::plain_line`]) when the cell written before had no
/// sequence (the default style, no charset, no link) or `-e` is off: no
/// cell needs one, so the line is its characters.
fn emit_plain(out: &mut Vec<u8>, cells: &[u32], opts: CaptureOpts) {
    let used = cells.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let end = if opts.join || opts.used_only {
        used
    } else if opts.escapes || opts.keep_spaces {
        plain_cellsize(cells, used)
    } else {
        cells.len()
    };
    let start = out.len();
    push_text(out, &cells[..end.min(cells.len())], opts.octal);
    if !opts.join && !opts.keep_spaces {
        // GRID_STRING_TRIM_SPACES (grid.c:1263).
        let kept = out[start..]
            .iter()
            .rposition(|&b| b != b' ')
            .map_or(start, |i| start + i + 1);
        out.truncate(kept);
    }
}

/// The text of plain cells (one codepoint each, 0 a blank) as
/// [`emit_row`] writes it: a blank as a space, a backslash doubled under
/// `-C` (`octal`).
fn push_text(out: &mut Vec<u8>, cells: &[u32], octal: bool) {
    let special = |c: u32| c >= 0x80 || (octal && c == u32::from(b'\\'));
    let mut rest = cells;
    loop {
        // A run of ASCII written as it is, a blank as a space.
        let n = rest.iter().position(|&c| special(c)).unwrap_or(rest.len());
        #[expect(clippy::cast_possible_truncation, reason = "below 0x80")]
        out.extend(
            rest[..n]
                .iter()
                .map(|&c| if c == 0 { b' ' } else { c as u8 }),
        );
        let Some(&c) = rest.get(n) else { break };
        if c == u32::from(b'\\') {
            out.extend_from_slice(b"\\\\");
        } else {
            let c = char::from_u32(c).expect("read as a char");
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
        rest = &rest[n + 1..];
    }
}

/// [`tmux_cellsize`] of a line of plain cells: every written cell (not a
/// blank) has the same style, so each run of ASCII is one flush, and a
/// character past ASCII is written alone.
fn plain_cellsize(cells: &[u32], used: usize) -> usize {
    let mut size = 0;
    let mut i = 0;
    while i < used {
        if cells[i] == 0 {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < used && cells[j] != 0 && cells[i] < 0x80 && cells[j] < 0x80 {
            j += 1;
        }
        if j > size {
            size = expanded_size(j, cells.len());
        }
        i = j;
    }
    size
}

/// [`emit_row`] for a line of plain cells with their styles
/// ([`Plain::styled`]) after a cell with no charset or link: no cell
/// changes the charset or a link, so a cell needs only its style's
/// sequence.
fn emit_styled(
    out: &mut Vec<u8>,
    cells: &[u32],
    styles: &StyleRow,
    opts: CaptureOpts,
    last: &mut Gc,
) {
    let used = cells.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
    let end = if opts.join || opts.used_only {
        used
    } else if opts.escapes || opts.keep_spaces {
        styled_cellsize(cells, styles, used)
    } else {
        cells.len()
    };
    let start = out.len();
    let cells = &cells[..end.min(cells.len())];
    if opts.escapes {
        // A run of cells of one style: the first writes its sequence, the
        // rest none (each follows a cell of its style).
        let mut x = 0;
        while x < cells.len() {
            let run = styles.of[x];
            let n = styles.of[x..cells.len()]
                .iter()
                .position(|&i| i != run)
                .unwrap_or(cells.len() - x);
            let style = styles.style(x);
            let at = out.len();
            sgr_code(out, &last.style, style);
            if opts.octal && out.len() > at {
                escape_esc(out, at);
            }
            last.style = *style;
            push_text(out, &cells[x..x + n], opts.octal);
            x += n;
        }
    } else {
        push_text(out, cells, opts.octal);
    }
    if !opts.join && !opts.keep_spaces {
        // GRID_STRING_TRIM_SPACES (grid.c:1263).
        while out.len() > start && out.last() == Some(&b' ') {
            out.pop();
        }
    }
}

/// [`tmux_cellsize`] of a line of plain cells with their styles: a cell is
/// written when it has text or a background colour, and a run is written
/// cells of one style.
fn styled_cellsize(cells: &[u32], styles: &StyleRow, used: usize) -> usize {
    let written = |x: usize| cells[x] != 0 || styles.style(x).bg != Color::Default;
    // screen_write_collect_add: a character past ASCII is written alone.
    let ascii = |x: usize| cells[x] < 0x80;
    let extent = (0..cells.len())
        .rposition(written)
        .map_or(0, |i| i + 1)
        .max(used);
    let mut size = 0;
    let mut i = 0;
    while i < extent {
        if !written(i) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < extent && written(j) && styles.style(j) == styles.style(i) && ascii(i) && ascii(j)
        {
            j += 1;
        }
        if j > size {
            size = expanded_size(j, cells.len());
        }
        i = j;
    }
    size
}

/// The size `grid_expand_line` grows a line `width` wide to for a flush
/// that ends at cell `j` (grid.c:564-589).
fn expanded_size(j: usize, width: usize) -> usize {
    if j < width / 4 {
        width / 4
    } else if j < width / 2 {
        width / 2
    } else if width > j {
        width
    } else {
        j
    }
}

/// `grid_string_cells_add_hyperlink` (grid.c:1035-1060): OSC 8 with `id=`
/// when there is one; ST is `ESC \` (`\033\\` under `-C`).
/// A link's URI or id as tmux keeps it: `utf8_stravis` with
/// `VIS_OCTAL|VIS_CSTYLE` (hyperlinks.c `hyperlinks_put`), which doubles a
/// backslash and writes DEL and the C0 controls other than tab and newline
/// in octal (`\a`, `\b`, `\f`, `\r`, `\v` and `\0` C-style); UTF-8 is
/// kept. What capture-pane and the `*_hyperlink` formats show.
#[must_use]
pub fn vis_link(s: &str) -> std::borrow::Cow<'_, str> {
    use std::fmt::Write as _;
    let plain = |c: char| c != '\\' && c != '\x7f' && (c >= ' ' || c == '\t' || c == '\n');
    if s.chars().all(plain) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\x0b' => out.push_str("\\v"),
            '\0' => out.push_str("\\0"),
            c if !plain(c) => {
                let _ = write!(out, "\\{:03o}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

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
/// tmux grows a line with `grid_expand_line` (grid.c:564-589): to a quarter
/// of the width if the write ends below it, to a half if below that, else
/// to the full width, and not at all if the line already reaches the end.
/// Plain ASCII is collected and written a run at a time
/// (`screen_write_collect_add`, `grid_set_cells`), anything else one cell
/// at a time (`screen_write_cell`: a wide or non-ASCII character, a tab, a
/// line-drawing character), its padding first (`grid_view_set_padding`).
/// Reproduced by treating each maximal run of collectable cells with equal
/// style, charset and link as one write, and every other cell as its own.
/// Approximation: it does not know about rows that were written and later
/// erased or rewritten out of order, which keep their old allocation in
/// tmux, nor where tmux ended a run early (an escape sequence that changes
/// nothing, the end of a read).
fn tmux_cellsize(cells: &[RowCell], used: usize) -> usize {
    let width = cells.len();
    // A wide character's padding and a tab's columns are written with it.
    let written = |c: &RowCell| !c.empty || c.style.bg != Color::Default || c.padding();
    // screen_write_collect_add: one column, one byte, no tab, no charset.
    let alone = |c: &RowCell| c.acs || c.tab != 0 || c.wide == Width::Double || c.data().len() > 1;
    let extent = cells
        .iter()
        .rposition(written)
        .map_or(0, |i| i + 1)
        .max(used);
    let mut size = 0;
    let mut expand = |end: usize| {
        if end > size {
            size = expanded_size(end, width);
        }
    };
    let mut i = 0;
    while i < extent {
        if !written(&cells[i]) {
            i += 1;
            continue;
        }
        if alone(&cells[i]) {
            // The padding cells, then the character (screen_write_cell).
            let mut j = i + 1;
            if cells[i].wide == Width::Double {
                if cells.get(j).is_some_and(|c| c.wide == Width::SpacerTail) {
                    j += 1;
                }
            } else if matches!(cells[i].tab, 1..=32) {
                while cells.get(j).is_some_and(|c| c.tab == TAB_CONTINUATION) {
                    j += 1;
                }
            }
            for end in i + 2..=j {
                expand(end);
            }
            expand(i + 1);
            i = j;
            continue;
        }
        // A run is what one grid_set_cells writes: same style, charset
        // and link.
        let same =
            |a: &RowCell, b: &RowCell| a.style == b.style && a.acs == b.acs && a.target == b.target;
        let mut j = i + 1;
        while j < extent && written(&cells[j]) && !alone(&cells[j]) && same(&cells[j], &cells[i]) {
            j += 1;
        }
        expand(j);
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
    for (i, v) in new.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        let _ = write!(out, "{v}");
    }
    out.push(b'm');
}

fn sgr_code(out: &mut Vec<u8>, last: &Style, cur: &Style) {
    // Nothing changed: no attribute is removed or added and every colour
    // is the last one, so tmux writes nothing; the usual case, a cell
    // like the one before.
    if last == cur {
        return;
    }
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
            let _ = if *v < 10 {
                write!(out, "{v}")
            } else {
                write!(out, "{}:{}", v / 10, v % 10)
            };
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
        /// Rows [`row_ref_near`] stepped to from the last instead.
        pub(super) static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// Lines a capture wrote from [`Source::plain_line`].
        pub(super) static PLAIN_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// Rows [`read_cells`] built cells for.
        pub(super) static CELL_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// Captures read every line as cells ([`plain_lines`] off).
        pub(super) static CELLS_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn lookups() -> usize {
        LOOKUPS.with(std::cell::Cell::get)
    }

    /// A pane whose history spans many Ghostty pages, with a line of each
    /// kind a row read has to get right every so often: styles, a wide
    /// character, a cluster, a hyperlink, a tab, a coloured erase, a line
    /// wrapped onto the next row.
    fn long_history() -> PaneTerm {
        use std::fmt::Write as _;
        let mut t = PaneTerm::new(40, 5, 30_000);
        let mut out = String::new();
        for i in 0..20_000 {
            let _ = match i % 7 {
                0 => write!(out, "\x1b[1;3{}mline {i}\x1b[m", i % 8),
                1 => write!(out, "{i} \u{4e2d}\u{6587} e\u{301}"),
                2 => write!(out, "\x1b]8;;http://h/{i}\x1b\\link\x1b]8;;\x1b\\ {i}"),
                3 => write!(out, "a\tb\t{i}"),
                4 => write!(out, "{i}\x1b[44m\x1b[K\x1b[m"),
                5 => write!(out, "{i:-<45}"),
                _ => write!(out, "plain {i}"),
            };
            out.push_str("\r\n");
        }
        t.feed(out.as_bytes());
        t
    }

    /// Reading a copy's rows in order, either way, is a lookup for the
    /// first and a step for each after it: a lookup walks Ghostty's page
    /// list from the oldest page, so a search over a long history (which
    /// reads every row) took time quadratic in its length.
    #[test]
    fn a_copy_read_in_order_steps_from_row_to_row() {
        let t = long_history();
        let copy = t.copy_screen();
        let n = copy.history_size() + 5;
        assert!(n > 20_000);
        let before = lookups();
        for y in 0..n {
            let _ = copy.used_row(y);
        }
        let mut text = String::new();
        for y in (0..n).rev() {
            let _ = copy.text(y, &mut text);
        }
        // The first row, and row 0 again at the end (a lookup of it walks
        // nothing).
        assert_eq!(lookups() - before, 2);
    }

    /// `PaneTerm::rows_in`, a whole-history capture, `#{history_bytes}`'s
    /// count and `capture-pane -R` read the pane's rows with a step each
    /// too.
    #[test]
    fn rows_and_capture_step_from_row_to_row() {
        let t = long_history();
        let n = t.history_size() + 5;
        let before = lookups();
        assert_eq!(t.rows_in(0..n + 2).count(), n + 2);
        let _ = capture(
            &t,
            CaptureOpts {
                start: LineRef::Start,
                end: LineRef::End,
                ..CaptureOpts::default()
            },
        );
        let _ = history_counts(&t);
        let _ = capture_grid(&t, 30_000);
        assert!(lookups() - before <= 8, "{} lookups", lookups() - before);
    }

    /// The alternate screen coming up copies the primary history (tmux
    /// goes on showing it) a step per row, and the copy reads as the rows
    /// did: a lookup per row made `vim` or `less` take seconds to start
    /// over a long history.
    #[test]
    fn alternate_screen_copies_history_a_step_per_row() {
        let mut t = long_history();
        let hsize = t.history_size();
        let want: Vec<Row> = (0..hsize).map(|y| t.row(y)).collect();
        let before = lookups();
        t.feed(b"\x1b[?1049h");
        assert!(t.alternate_on());
        assert!(lookups() - before <= 4, "{} lookups", lookups() - before);
        assert_eq!(t.history_size(), hsize);
        assert!((0..hsize).all(|y| t.row(y) == want[y]));
    }

    /// Rows stepped to read the same as rows looked up, in any order:
    /// row by row both ways, jumps across pages, the same row again, the
    /// active area and back, the first and last rows, over resident and
    /// compressed history.
    #[test]
    fn stepped_rows_read_as_looked_up() {
        let mut t = long_history();
        for compressed in [false, true] {
            if compressed {
                t.compress_all();
                assert!(t.compressed_pages() > 0);
            }
            let n = t.history_size() + 5;
            let want: Vec<Row> = (0..n).map(|y| t.row(y)).collect();
            let copy = t.copy_screen();
            let mut order: Vec<usize> = (0..n).chain((0..n).rev()).collect();
            let mut x: usize = 12345;
            for _ in 0..3000 {
                x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let y = (x >> 33) % n;
                order.extend([y, y, y.saturating_sub(1), (y + 700).min(n - 1), n - 1, 0]);
            }
            for &y in &order {
                assert_eq!(copy.row(y), want[y], "row {y}, compressed {compressed}");
            }
            // A fresh copy's first read is a lookup.
            let (mut text, mut fresh) = (String::new(), String::new());
            for &y in order.iter().step_by(97) {
                let got = copy.text(y, &mut text);
                let want = t.copy_screen().text(y, &mut fresh);
                assert_eq!((got, &text), (want, &fresh), "row {y}");
            }
            assert_eq!(t.rows_in(0..n).collect::<Vec<_>>(), want);
            let jumpy: Vec<Row> = order
                .iter()
                .take(5000)
                .map(|&y| t.rows_in(y..y + 1).next().unwrap())
                .collect();
            assert!(jumpy.iter().zip(&order).all(|(r, &y)| *r == want[y]));
        }
    }

    /// A copy of a history of many pages (each copied whole, as its
    /// memory, patch 0035) reads as the pane read when it was taken, with
    /// styles, links, clusters, wide characters and tabs, whatever the
    /// pane does after.
    #[test]
    fn a_copy_of_whole_pages_reads_as_the_pane_did() {
        let mut t = long_history();
        let n = t.history_size() + 5;
        let want: Vec<Row> = (0..n).map(|y| t.row(y)).collect();
        let links: Vec<Option<String>> = (0..n).map(|y| t.hyperlink(0, y)).collect();
        let copy = t.copy_screen();
        t.feed(b"\x1b[3J\x1b[2J\x1b[H\x1b[1;31mgone\x1b]8;;http://x/\x1b\\x\r\n");
        for y in 0..n {
            assert_eq!(copy.row(y), want[y], "row {y}");
            assert_eq!(copy.hyperlink(0, y), links[y], "row {y}");
        }
        assert!(links.iter().flatten().count() > 2000);
    }

    /// A copy's row as text ([`ScreenCopy::text`]) is what a search makes
    /// of its cells (`window_copy_stringify`: a blank, or a column past the
    /// row's last cell, a space; any other cell its character) for every
    /// row whose cells are all one character of one column, of any style,
    /// colour or link, past ASCII or not; and nothing for any other row.
    /// A few rows of such cells are read from cells too: a tab a resize cut
    /// and the column a wrapped wide character left read as blanks, but
    /// are not plain in Ghostty's cell. Over random content: resident,
    /// compressed, and the alternate screen (the primary history it hides
    /// is read from cells).
    #[test]
    fn a_copy_row_as_text_is_its_cells() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let (mut texts, mut others, mut past_ascii, mut declined) = (0, 0, 0, 0);
        for round in 0..3 {
            let cols = 12 + rng.below(30);
            let mut t = PaneTerm::new(u16::try_from(cols).unwrap(), 6, 3000);
            t.feed(&random_output(&mut rng, 2500, cols));
            if round == 1 {
                t.compress_all();
                assert!(t.compressed_pages() > 0);
            }
            if round == 2 {
                t.feed(b"\x1b[?1049h");
                t.feed(&random_output(&mut rng, 4, cols));
            }
            let copy = t.copy_screen();
            let cols = usize::try_from(cols).unwrap();
            let mut text = String::new();
            for y in 0..copy.history_size() + 6 {
                let got = copy.text(y, &mut text);
                if round == 2 && y < copy.history_size() {
                    assert_eq!(got, None, "row {y}");
                    continue;
                }
                let row = copy.used_row(y);
                let narrow = row
                    .cells
                    .iter()
                    .all(|c| c.width == 1 && !c.tab && !c.acs && c.text.chars().nth(1).is_none());
                if !narrow {
                    assert_eq!(got, None, "round {round} row {y}: {row:?}");
                    others += 1;
                    continue;
                }
                let want: String = (0..cols)
                    .map(|x| match row.cells.get(x) {
                        Some(c) if !c.text.is_empty() => c.text.as_str(),
                        _ => " ",
                    })
                    .collect();
                let Some(wrapped) = got else {
                    declined += 1;
                    continue;
                };
                assert_eq!(wrapped, row.wrapped, "round {round} row {y}");
                assert_eq!(text, want, "round {round} row {y}");
                texts += 1;
                past_ascii += usize::from(!text.is_ascii());
            }
        }
        assert!(
            texts > 1000 && others > 1000 && past_ascii > 100 && declined < texts / 20,
            "{texts} as text ({past_ascii} past ASCII), {others} not, {declined} declined"
        );
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

    /// A wide character that does not fit the rest of the row wraps and
    /// leaves the last cell as it was (screen-write.c `screen_write_cell`,
    /// patch 0037): written text stays, an empty cell stays unwritten.
    /// Expected: tmux 3.8's capture-pane, then `-J`.
    #[test]
    fn wide_wrap_keeps_last_cell() {
        let mut t = PaneTerm::new(10, 4, 0);
        t.feed("012345678x\x1b[10G\u{4e00}a\r\n0123\x1b[10G\u{4e00}b".as_bytes());
        let text = String::from_utf8_lossy(&capture(&t, CaptureOpts::default())).into_owned();
        assert_eq!(text, "012345678x\n\u{4e00}a\n0123\n\u{4e00}b\n", "{text:?}");
        let opts = CaptureOpts {
            join: true,
            ..Default::default()
        };
        let text = String::from_utf8_lossy(&capture(&t, opts)).into_owned();
        assert_eq!(text, "012345678x\u{4e00}a\n0123\u{4e00}b\n", "{text:?}");
    }

    /// In insert mode tmux makes room before a wide character wraps, which
    /// clears the last cell (`screen_write_cell`, `grid_view_insert_cells`);
    /// a blank with only the pen's background, as a wrap scrolls in, is
    /// tmux's default cell (`screen_write_linefeed` with bg 8). Expected:
    /// tmux 3.8's capture-pane -e (oracle `wide_wrap_insert_mode`,
    /// `wide_wrap_scrolled_fill`).
    #[test]
    fn wide_wrap_clears_last_cell() {
        let opts = CaptureOpts {
            escapes: true,
            ..Default::default()
        };
        let mut t = PaneTerm::new(11, 2, 0);
        t.feed("\x1b[4habcdefghij\x1b[1mk\x1b[m\x1b[11G\x1b[44m\u{4e00}\x1b[m".as_bytes());
        let text = String::from_utf8_lossy(&capture(&t, opts)).into_owned();
        assert_eq!(text, "abcdefghij\n\x1b[44m\u{4e00}\x1b[49m\n", "{text:?}");
        // The last two lines are ones a wrap scrolled in.
        let mut t = PaneTerm::new(5, 2, 100);
        t.feed("\x1b[41mab\u{4e00}\u{4e00}\u{4e00}\u{4e00}\u{4e00}\u{4e00}\x1b[mxyz".as_bytes());
        let text = String::from_utf8_lossy(&capture(&t, opts)).into_owned();
        assert_eq!(
            text, "\x1b[41m\u{4e00}\u{4e00}\x1b[49m\n\x1b[41m\u{4e00}\x1b[49mxyz\n",
            "{text:?}"
        );
    }

    /// tmux gives the line-drawing charset only to an ASCII character
    /// (input.c `input_print`): a character from UTF-8 printed under it is
    /// a plain one (`input_top_bit_set`). Expected: tmux 3.8's
    /// capture-pane -e.
    #[test]
    fn utf8_under_line_drawing_is_plain() {
        let mut t = PaneTerm::new(20, 2, 0);
        t.feed("\x1b(0q\u{3042}\u{2500}x\x1b(B".as_bytes());
        let opts = CaptureOpts {
            escapes: true,
            ..Default::default()
        };
        let text = String::from_utf8_lossy(&capture(&t, opts)).into_owned();
        assert!(
            text.starts_with("\x0eq\x0f\u{3042}\u{2500}\x0ex\n"),
            "{text:?}"
        );
        let s = crate::snapshot(&t);
        let acs: Vec<bool> = [0, 1, 3, 4].map(|x| s.cell(x, 0).unwrap().acs).to_vec();
        assert_eq!(acs, [true, false, false, true]);
    }

    /// tmux keeps a link's URI and id through `utf8_stravis` with
    /// `VIS_OCTAL|VIS_CSTYLE` (hyperlinks.c `hyperlinks_put`), so
    /// capture-pane shows a backslash doubled and DEL in octal.
    #[test]
    fn link_uri_and_id_are_kept_vis_encoded() {
        let mut t = PaneTerm::new(20, 2, 0);
        t.feed(b"\x1b]8;id=i\\d;a\\b\x7fc\x1b\\x\x1b]8;;\x1b\\");
        let opts = CaptureOpts {
            escapes: true,
            ..Default::default()
        };
        let text = String::from_utf8_lossy(&capture(&t, opts)).into_owned();
        assert!(
            text.starts_with("\x1b]8;id=i\\\\d;a\\\\b\\177c\x1b\\x\x1b]8;;\x1b\\\n"),
            "{text:?}"
        );
        assert_eq!(vis_link("a\\b\x7f\u{e9}").as_ref(), "a\\\\b\\177\u{e9}");
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
                    && c.link.is_none()
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

    /// A small deterministic generator (xorshift64*) for random content.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Random output for a pane `cols` wide: plain text and spaces, most
    /// lines, between lines of every kind a capture writes differently:
    /// styles left on across lines, coloured erases, wide characters,
    /// clusters, tabs, links, line drawing, backslashes, gaps written out
    /// of order, erased cells, lines that wrap and empty lines.
    fn random_output(rng: &mut Rng, lines: usize, cols: u64) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut out = String::new();
        for i in 0..lines {
            let pieces = rng.below(4);
            for _ in 0..pieces {
                let _ = match rng.below(24) {
                    0 => write!(out, "\x1b[3{}m", rng.below(8)),
                    1 => write!(out, "\x1b[1;4{}m", rng.below(8)),
                    2 => write!(out, "\x1b[38;5;{}m", rng.below(256)),
                    3 => write!(out, "\x1b[48;2;{};2;3m", rng.below(256)),
                    4 => write!(out, "\x1b[m"),
                    5 => write!(out, "\x1b[4{}m\x1b[K\x1b[m", rng.below(8)),
                    6 => write!(out, "\u{4e2d}\u{6587}"),
                    7 => write!(out, "e\u{301}\u{1f600}"),
                    8 => write!(out, "a\tb"),
                    9 => write!(
                        out,
                        "\x1b]8;id=x{};http://h/{i}\x1b\\ln\x1b]8;;\x1b\\",
                        rng.below(3)
                    ),
                    10 => write!(out, "\x1b(0qx\x1b(B"),
                    11 => write!(out, "a\\b"),
                    12 => write!(out, "\x1b[{}G#", 1 + rng.below(cols)),
                    13 => write!(out, "\x1b[{}X", 1 + rng.below(5)),
                    14 => write!(
                        out,
                        "{:x<1$}",
                        "",
                        usize::try_from(cols + rng.below(cols)).unwrap()
                    ),
                    15 => write!(out, "\x1b[4:3m~\x1b[59m"),
                    16 => write!(out, "\u{e9}\u{2500}"),
                    17..=18 => write!(out, "{:1$}", "", usize::try_from(rng.below(6)).unwrap()),
                    _ => write!(out, "w{} ", rng.below(1000)),
                };
            }
            if rng.below(8) == 0 {
                // A line of plain text after a style left on.
                out.push_str("plain text");
            }
            out.push_str(if rng.below(5) == 0 { "  \r\n" } else { "\r\n" });
        }
        out.into_bytes()
    }

    /// Every combination of the flags a line of plain cells is written
    /// differently under, and a few ranges.
    fn capture_opts() -> Vec<CaptureOpts> {
        let mut all = Vec::new();
        for bits in 0..32u32 {
            for extra in 0..5 {
                for (start, end) in [
                    (LineRef::Start, LineRef::End),
                    (LineRef::Line(-7), LineRef::Line(2)),
                ] {
                    all.push(CaptureOpts {
                        escapes: bits & 1 != 0,
                        join: bits & 2 != 0,
                        octal: bits & 4 != 0,
                        keep_spaces: bits & 8 != 0,
                        used_only: bits & 16 != 0,
                        numbers: extra == 1,
                        times: extra == 2,
                        flags: extra == 3,
                        hyperlinks: extra == 4,
                        start,
                        end,
                    });
                }
            }
        }
        all
    }

    /// `f` with lines of plain cells read as cells, as every line was
    /// before captures wrote them straight from Ghostty's cells.
    fn cells_only<R>(f: impl FnOnce() -> R) -> R {
        CELLS_ONLY.with(|c| c.set(true));
        let r = f();
        CELLS_ONLY.with(|c| c.set(false));
        r
    }

    /// Copy mode's grid as `capture-pane -M` reads it: the copy's rows,
    /// each still the entry copy's (`entry`) or read as a [`Row`].
    struct CopyLines {
        copy: ScreenCopy,
        rows: Vec<Row>,
        sy: usize,
        entry: bool,
    }

    impl Lines for CopyLines {
        fn history_size(&self) -> usize {
            self.copy.history_size()
        }

        fn screen_rows(&self) -> usize {
            self.sy
        }

        fn line(&self, y: usize) -> &Row {
            &self.rows[y]
        }

        fn time(&self, y: usize) -> i64 {
            self.copy.history_time(y)
        }

        fn starts(&self, y: usize, output: bool) -> bool {
            let mark = if output {
                LineMark::Output
            } else {
                LineMark::Prompt
            };
            self.copy.line_marks().contains(&(y, mark))
        }

        fn entry_row(&self, y: usize) -> Option<(&ScreenCopy, usize)> {
            self.entry.then_some((&self.copy, y))
        }
    }

    /// Lines of plain cells written straight from Ghostty's cells (or a
    /// copy mode row's) capture byte for byte as the general path writes
    /// them, under every flag, over random content: resident and
    /// compressed history, the alternate screen (`-a` and the primary
    /// history it hides), and copy mode's grid (`-M`).
    #[test]
    fn plain_lines_capture_as_cells_do() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let opts = capture_opts();
        let (mut plain, mut cells) = (0, 0);
        for round in 0..4 {
            let cols = 12 + rng.below(30);
            let mut t = PaneTerm::new(u16::try_from(cols).unwrap(), 6, 2000);
            // Round 2's history spans pages, and full ones compress.
            let lines = if round == 2 { 8000 } else { 300 + round * 200 };
            t.feed(&random_output(&mut rng, lines, cols));
            if round == 2 {
                t.compress_all();
                assert!(t.compressed_pages() > 0);
            }
            if round == 3 {
                t.feed(b"\x1b[?1049h");
                t.feed(&random_output(&mut rng, 3, cols));
            }
            let copy = t.copy_screen();
            let n = copy.history_size() + 6;
            let lines = |entry| CopyLines {
                rows: (0..n).map(|y| copy.used_row(y)).collect(),
                copy: t.copy_screen(),
                sy: 6,
                entry,
            };
            let (entry, kept) = (lines(true), lines(false));
            let sx = usize::try_from(cols).unwrap();
            for &o in &opts {
                let want = cells_only(|| capture(&t, o));
                let (p, c) = (
                    PLAIN_ROWS.with(std::cell::Cell::get),
                    CELL_ROWS.with(std::cell::Cell::get),
                );
                assert_eq!(capture(&t, o), want, "round {round} {o:?}");
                plain += PLAIN_ROWS.with(std::cell::Cell::get) - p;
                cells += CELL_ROWS.with(std::cell::Cell::get) - c;
                assert_eq!(
                    capture_alternate(&t, o),
                    cells_only(|| capture_alternate(&t, o)),
                    "round {round} -a {o:?}"
                );
                let want = cells_only(|| capture_lines(&kept, sx, o));
                assert_eq!(capture_lines(&kept, sx, o), want, "round {round} -M {o:?}");
                assert_eq!(
                    capture_lines(&entry, sx, o),
                    want,
                    "round {round} -M entry {o:?}"
                );
            }
        }
        // Both paths ran.
        assert!(
            plain > 5_000 && cells > 5_000,
            "{plain} plain, {cells} as cells"
        );
    }

    /// `#{history_bytes}`'s count of lines of plain cells read straight
    /// from Ghostty's cells is the general path's, over random content:
    /// resident and compressed history, and the primary history under the
    /// alternate screen.
    #[test]
    fn plain_lines_count_as_cells_do() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let (mut plain, mut cells) = (0, 0);
        for round in 0..4 {
            let cols = 12 + rng.below(30);
            let mut t = PaneTerm::new(u16::try_from(cols).unwrap(), 6, 2000);
            let lines = if round == 2 { 8000 } else { 300 + round * 200 };
            t.feed(&random_output(&mut rng, lines, cols));
            if round == 2 {
                t.compress_all();
                assert!(t.compressed_pages() > 0);
            }
            if round == 3 {
                t.feed(b"\x1b[?1049h");
                t.feed(&random_output(&mut rng, 3, cols));
            }
            let want = cells_only(|| history_counts(&t));
            let (p, c) = (
                PLAIN_ROWS.with(std::cell::Cell::get),
                CELL_ROWS.with(std::cell::Cell::get),
            );
            assert_eq!(history_counts(&t), want, "round {round}");
            plain += PLAIN_ROWS.with(std::cell::Cell::get) - p;
            cells += CELL_ROWS.with(std::cell::Cell::get) - c;
        }
        // Both paths ran.
        assert!(
            plain > 500 && cells > 500,
            "{plain} plain, {cells} as cells"
        );
    }

    /// `#{history_bytes}` over a long history of plain text (`seq`) builds
    /// no cells: building every one made a 70000-line pane's count take
    /// longer than a format may run (format.c `format_check_time`), so
    /// `list-panes` printed its line without the ` (active)` after it.
    #[test]
    fn a_plain_history_counts_without_building_cells() {
        let mut t = PaneTerm::new(80, 24, 100_000);
        for i in 1..=20_000 {
            t.feed(format!("{i}\r\n").as_bytes());
        }
        t.compress_all();
        assert!(t.compressed_pages() > 0);
        let before = CELL_ROWS.with(std::cell::Cell::get);
        let counts = history_counts(&t);
        assert_eq!(CELL_ROWS.with(std::cell::Cell::get), before);
        assert_eq!(counts, cells_only(|| history_counts(&t)));
        assert_eq!(counts.lines, t.history_size() + 24);
    }

    /// A capture of a long history of plain text builds no cells for its
    /// lines: every line used to be read into cells with their text,
    /// style and link, which made `capture-pane -S -` of a 1M-line
    /// history several times slower than tmux. Copy mode's (`-M`) unread
    /// lines too.
    #[test]
    fn a_plain_history_captures_without_building_cells() {
        let mut t = PaneTerm::new(60, 5, 5000);
        for i in 0..1500 {
            t.feed(format!("2026-11-01 INFO worker[{i}] handled  \\ request\r\n").as_bytes());
        }
        let copy = CopyLines {
            rows: Vec::new(),
            copy: t.copy_screen(),
            sy: 5,
            entry: true,
        };
        let before = CELL_ROWS.with(std::cell::Cell::get);
        for o in capture_opts()
            .into_iter()
            .filter(|o| o.start == LineRef::Start)
        {
            let _ = capture(&t, o);
            let _ = capture_lines(&copy, 60, o);
        }
        assert_eq!(CELL_ROWS.with(std::cell::Cell::get), before);
    }

    /// Nor does one of styled text (colours, attributes, a coloured erase,
    /// characters past ASCII), with or without its styles (`-e`, `-N`),
    /// unless the capture looks at more of each cell (`-F`, `-H`).
    #[test]
    fn a_styled_history_captures_without_building_cells_unless_flags_or_links() {
        let mut t = PaneTerm::new(60, 5, 5000);
        for i in 0..1500 {
            t.feed(
                format!("\x1b[32m2026\x1b[m \x1b[1;7mINFO\x1b[m {i} caf\u{e9} \u{2500}\x1b[44m\x1b[K\x1b[m\r\n")
                    .as_bytes(),
            );
        }
        let entry = t.copy_screen();
        let copy = CopyLines {
            rows: (0..entry.history_size() + 5)
                .map(|y| entry.used_row(y))
                .collect(),
            copy: t.copy_screen(),
            sy: 5,
            entry: true,
        };
        for o in capture_opts()
            .into_iter()
            .filter(|o| o.start == LineRef::Start)
        {
            let before = CELL_ROWS.with(std::cell::Cell::get);
            let _ = capture(&t, o);
            let _ = capture_lines(&copy, 60, o);
            let built = CELL_ROWS.with(std::cell::Cell::get) - before;
            let more = o.flags || o.hyperlinks;
            assert_eq!(built == 0, !more, "{built} rows built for {o:?}");
        }
    }

    /// The flag letters `-F` gives each line.
    fn flag_letters(t: &PaneTerm) -> Vec<String> {
        let o = CaptureOpts {
            flags: true,
            start: LineRef::Start,
            ..Default::default()
        };
        String::from_utf8(capture(t, o))
            .unwrap()
            .lines()
            .map(|l| l.split(' ').next().unwrap_or_default().to_owned())
            .collect()
    }

    /// tmux keeps `GRID_LINE_EXTENDED` and `GRID_LINE_HYPERLINK` once a cell
    /// needed them (grid.c `grid_extended_cell`; Ghostty patch 0034): a
    /// wide character or a link overwritten still flags its line, the
    /// other half of the wide character is a written space, and a line
    /// emptied loses them. The history an alternate screen hides keeps them.
    #[test]
    fn line_flags_outlive_the_cells_that_set_them() {
        let mut t = PaneTerm::new(10, 5, 100);
        t.feed("\u{3042}B\rX\r\n".as_bytes());
        t.feed(b"\x1b]8;;http://e\x1b\\a\x1b]8;;\x1b\\\rb\r\n");
        t.feed("\u{3042}\x1b[2Kc\r\nplain".as_bytes());
        assert_eq!(flag_letters(&t), ["X", "HX", "-", "-", "-"]);
        assert_eq!(t.row(0).text(), "X B");
        let raw = String::from_utf8(raw::capture_grid(&t, 100)).unwrap();
        assert!(raw.contains("\tL 0 (0) flags=EXTENDED[2] 3/5\n"), "{raw}");
        assert!(
            raw.contains("\t\tC 0,1 data=(1,1, ) flags=NONE[0]"),
            "{raw}"
        );
        assert!(
            raw.contains("\tL 1 (1) flags=EXTENDED,HYPERLINK[102]"),
            "{raw}"
        );
        t.feed(b"\r\n\r\n\r\n\r\n\x1b[?1049h");
        assert_eq!(t.history_size(), 3);
        assert_eq!(flag_letters(&t)[..3], ["X", "HX", "-"]);
    }

    /// A line a scroll adds under an RGB background is cleared with
    /// extended cells (grid.c `grid_empty_line`, `grid_clear_cell`), so it
    /// is flagged `X`: on the alternate screen (no history), under a
    /// scrolling region, and into history. Ghostty's blank fill of the new
    /// line took the flag with it (Ghostty patch 0034; the reference gives
    /// `X x` and `X ` for the first).
    #[test]
    fn a_line_scrolled_in_under_an_rgb_background_is_extended() {
        // Each with the flags the reference gives (capture-pane -pF -S -).
        for (name, feed, expected) in [
            (
                "alternate",
                "\x1b[?1049h\x1b[4;1H\x1b[48;2;9;8;7mx\n",
                "--XX",
            ),
            ("primary", "\x1b[4;1H\x1b[48;2;9;8;7mx\n", "---XX"),
            (
                "region",
                "\x1b[2;3r\x1b[3;1H\x1b[48;2;9;8;7mx\n\x1b[r",
                "--XX-",
            ),
            (
                "alternate region",
                "\x1b[?1049h\x1b[2;3r\x1b[3;1H\x1b[48;2;9;8;7mx\n\x1b[r",
                "-XX-",
            ),
        ] {
            let mut t = PaneTerm::new(10, 4, 100);
            t.feed(feed.as_bytes());
            assert_eq!(flag_letters(&t).concat(), expected, "{name}");
        }
    }
}
