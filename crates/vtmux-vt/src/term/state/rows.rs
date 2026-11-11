//! Rows copied cell for cell from one place of a pane's screens to another
//! (the live-upgrade spec's decision 31, *Respawning a restored pane*: an
//! editor's alternate screen kept on the main one). The rows go through
//! the state export and import a live upgrade uses, so text, widths,
//! styles (an erase's coloured blanks too), hyperlinks with their ids, the
//! line-drawing mark, tabs and the tmux row flags arrive exactly, where
//! replaying them as `capture-pane -e` text loses charsets, coloured
//! erased cells and links that span rows.

use std::collections::HashMap;

use libghostty_vt::vtmux as gv;

use super::{
    CellState, LinkId, PageCapacity, PageRows, PageState, PaneTerm, RowState, ScreenKind,
    StateError, TermState, page_state,
};

/// Cell flags: the width (bits 2-3) and its spacer head
/// ([`super::CellState`]).
const WIDTH_SHIFT: u16 = 2;
const SPACER_HEAD: u16 = 3;

/// Rows of a screen, cell for cell, apart from the page they came from
/// ([`PaneTerm::copy_rows`], [`PaneTerm::put_rows`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowCopy {
    page: PageRows,
}

impl RowCopy {
    /// How many rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.page.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.page.rows.is_empty()
    }
}

/// A page of rows being built from rows of other pages, each table entry
/// taken once.
struct Builder {
    page: PageRows,
    /// For each source page (by the caller's key), its style and link
    /// indexes as this page has them.
    styles: HashMap<(usize, u16), u16>,
    links: HashMap<(usize, u16), u16>,
}

impl Builder {
    fn new(cols: u16) -> Self {
        Self {
            page: PageRows {
                cols,
                styles: vec![super::StyleState::default()],
                links: vec![None],
                ..PageRows::default()
            },
            styles: HashMap::new(),
            links: HashMap::new(),
        }
    }

    /// Row `r` of `src` (the source page `key`) appended, its flags
    /// `flags`; with `hard`, a spacer head at its end is left out (the row
    /// no longer wraps, and only a wrapping row ends in one).
    fn push(
        &mut self,
        key: usize,
        src: &PageRows,
        graphemes: &HashMap<u32, &[u32]>,
        r: usize,
        flags: u8,
        hard: bool,
    ) -> Result<(), StateError> {
        let row = src.rows[r];
        let start = row.cell_start as usize;
        let mut cells = &src.cells[start..start + usize::from(row.cell_count)];
        if hard
            && cells
                .last()
                .is_some_and(|c| (c.flags >> WIDTH_SHIFT) & 3 == SPACER_HEAD)
        {
            cells = &cells[..cells.len() - 1];
        }
        let first = u32::try_from(self.page.cells.len())
            .map_err(|_| StateError::Invalid("page cell count"))?;
        for (i, c) in cells.iter().enumerate() {
            let style = self.style(key, src, c.style)?;
            let link = self.link(key, src, c.link)?;
            let index = u32::try_from(start + i).map_err(|_| StateError::Invalid("cell"))?;
            if let Some(cps) = graphemes.get(&index) {
                let at = first + u32::try_from(i).map_err(|_| StateError::Invalid("cell"))?;
                self.page.graphemes.push(at);
                self.page.graphemes.extend_from_slice(cps);
            }
            self.page.cells.push(CellState { style, link, ..*c });
        }
        self.page.rows.push(RowState {
            cell_start: first,
            cell_count: u16::try_from(cells.len()).map_err(|_| StateError::Invalid("row"))?,
            flags,
            ..row
        });
        Ok(())
    }

    fn style(&mut self, key: usize, src: &PageRows, i: u16) -> Result<u16, StateError> {
        if i == 0 {
            return Ok(0);
        }
        if let Some(&n) = self.styles.get(&(key, i)) {
            return Ok(n);
        }
        let style = *src
            .styles
            .get(usize::from(i))
            .ok_or(StateError::Invalid("cell style"))?;
        let n = self
            .page
            .styles
            .iter()
            .position(|s| *s == style)
            .unwrap_or_else(|| {
                self.page.styles.push(style);
                self.page.styles.len() - 1
            });
        let n = u16::try_from(n).map_err(|_| StateError::Invalid("page style count"))?;
        self.styles.insert((key, i), n);
        Ok(n)
    }

    fn link(&mut self, key: usize, src: &PageRows, i: u16) -> Result<u16, StateError> {
        if i == 0 {
            return Ok(0);
        }
        if let Some(&n) = self.links.get(&(key, i)) {
            return Ok(n);
        }
        let link = src
            .links
            .get(usize::from(i))
            .ok_or(StateError::Invalid("cell hyperlink"))?;
        let n = self
            .page
            .links
            .iter()
            .position(|l| l == link)
            .unwrap_or_else(|| {
                self.page.links.push(link.clone());
                self.page.links.len() - 1
            });
        let n = u16::try_from(n).map_err(|_| StateError::Invalid("page link count"))?;
        self.links.insert((key, i), n);
        Ok(n)
    }
}

/// A page's grapheme table by cell index.
fn graphemes(p: &PageRows) -> HashMap<u32, &[u32]> {
    let mut out = HashMap::new();
    let mut rest = p.graphemes.as_slice();
    while let [index, n, tail @ ..] = rest {
        let n = (*n as usize).min(tail.len());
        // The count stays with the codepoints, as the table holds them.
        out.insert(*index, &rest[1..2 + n]);
        rest = &tail[n..];
    }
    out
}

impl PaneTerm {
    /// The visible rows `range` (top row 0) of the screen in use, cell for
    /// cell: text, widths, styles, hyperlinks with their ids, the
    /// line-drawing mark, tabs and the row flags; a range past the screen
    /// is cut to it.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    #[must_use]
    pub fn copy_rows(&self, range: std::ops::Range<usize>) -> RowCopy {
        let screen = if self.alternate_on() {
            ScreenKind::Alternate
        } else {
            ScreenKind::Primary
        };
        let rows = usize::from(self.rows());
        // The newest pages back to the screen's top row; the visible rows
        // are the last `rows` of them, never compressed.
        let n = self
            .term
            .vtmux_page_count(screen.key())
            .expect("page count")
            .unwrap_or(0);
        let mut pages = Vec::new();
        let mut have = 0;
        let mut buf = gv::Page::default();
        for i in (0..n).rev() {
            if have >= rows {
                break;
            }
            self.term
                .vtmux_page_export(screen.key(), i, false, &mut buf)
                .expect("page export");
            let PageState::Rows(page) = page_state(&buf) else {
                break;
            };
            have += page.rows.len();
            pages.push(page);
        }
        pages.reverse();
        let mut out = Builder::new(self.cols());
        let skip = have.saturating_sub(rows);
        let wanted = range.start.min(rows)..range.end.min(rows);
        let mut at = 0;
        for (key, page) in pages.iter().enumerate() {
            let table = graphemes(page);
            for r in 0..page.rows.len() {
                let y = (at + r).checked_sub(skip);
                if y.is_some_and(|y| wanted.contains(&y)) {
                    let flags = page.rows[r].flags;
                    out.push(key, page, &table, r, flags, false)
                        .expect("a page's own rows");
                }
            }
            at += page.rows.len();
        }
        RowCopy { page: out.page }
    }

    /// `rows` written over the primary screen's rows, the first on visible
    /// row `top` (negative: that far up in its history; a row past what
    /// the history keeps is left out), cell for cell as [`Self::copy_rows`]
    /// took them, through the state export and import a live upgrade
    /// uses. The first row continues no row above it and the last is
    /// hard, so the rows are lines of their own; hyperlinks without an id
    /// are new ones of the primary screen. Only for a pane on its primary
    /// screen at the terminal's size.
    ///
    /// # Errors
    /// A copy the primary screen's rows cannot take (another width, the
    /// alternate screen in use), or one Ghostty refuses; the pane is as it
    /// was.
    pub fn put_rows(&mut self, rows: &RowCopy, top: isize) -> Result<(), StateError> {
        if rows.is_empty() {
            return Ok(());
        }
        if self.alternate_on() || rows.page.cols != self.cols() {
            return Err(StateError::Invalid("rows for another screen"));
        }
        let mut state = self.export_state_with(super::CompressedPages::Blob);
        if state.primary_size.is_some() {
            return Err(StateError::Invalid("rows for another screen"));
        }
        // A target in a page held compressed: every page as rows.
        if !splice(&mut state, rows, top)? {
            state = self.export_state();
            splice(&mut state, rows, top)?;
        }
        let mut term = Self::import_state(&state)?;
        term.number_links_from(&self.link_numbers());
        term.allow_set_title = self.allow_set_title;
        term.kitty_option = self.kitty_option;
        term.graphics_option = self.graphics_option;
        term.set_cell_pixels(self.cell_px);
        term.set_graphics_share(self.graphics_share);
        *self = term;
        Ok(())
    }
}

/// `n` as a signed row distance.
fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

/// `rows` put over `state`'s primary rows from visible row `top` down
/// ([`PaneTerm::put_rows`]); false, `state` unchanged, when a target row
/// is in a page held compressed.
fn splice(state: &mut TermState, copy: &RowCopy, top: isize) -> Result<bool, StateError> {
    let visible = signed(usize::from(state.rows));
    // Each target as its distance from the primary screen's last row (1
    // for the bottom row).
    let distance = |i: usize| visible - top - signed(i);
    // The pages holding targets, newest first, each with how many rows
    // are below its last.
    let mut spans = Vec::new();
    let mut below = 0isize;
    for (index, page) in state.primary.pages.iter().enumerate().rev() {
        if below >= distance(0) {
            break;
        }
        let Some(n) = page.rows() else {
            return Ok(false);
        };
        spans.push((index, below));
        below += signed(n);
    }
    // The copy's links without an id are new links of the primary screen.
    let mut copy = copy.page.clone();
    let mut fresh = HashMap::new();
    for link in copy.links.iter_mut().flatten() {
        if let LinkId::Implicit(n) = link.id {
            let counter = &mut state.primary.hyperlink_counter;
            let id = *fresh.entry(n).or_insert_with(|| {
                let id = *counter;
                *counter = id.wrapping_add(1);
                id
            });
            link.id = LinkId::Implicit(id);
        }
    }
    let copy_graphemes = graphemes(&copy);
    let last = copy.rows.len() - 1;
    for (index, below) in spans {
        let PageState::Rows(page) = &state.primary.pages[index] else {
            unreachable!("checked above");
        };
        let len = signed(page.rows.len());
        // The copy's row page row `r` takes.
        let target = |r: usize| (0..=last).find(|&i| distance(i) == below + len - signed(r));
        if (0..page.rows.len()).all(|r| target(r).is_none()) {
            continue;
        }
        let page_graphemes = graphemes(page);
        let mut out = Builder::new(page.cols);
        for r in 0..page.rows.len() {
            let Some(i) = target(r) else {
                out.push(0, page, &page_graphemes, r, page.rows[r].flags, false)?;
                continue;
            };
            let mut flags = copy.rows[i].flags;
            if i == 0 {
                flags &= !RowState::CONTINUATION;
            }
            if i == last {
                flags &= !RowState::WRAPPED;
            }
            out.push(1, &copy, &copy_graphemes, i, flags, i == last)?;
        }
        // Ghostty counts the capacity the rows need.
        out.page.capacity = PageCapacity::default();
        state.primary.pages[index] = PageState::Rows(out.page);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use crate::{CaptureOpts, Compression, LineRef, PaneTerm, capture};

    fn lines(t: &PaneTerm) -> Vec<String> {
        let all = capture(
            t,
            CaptureOpts {
                escapes: true,
                start: LineRef::Start,
                ..CaptureOpts::default()
            },
        );
        String::from_utf8_lossy(&all)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Rows put over the primary screen read as the rows copied, styles,
    /// links and wide characters with them, the rows around unchanged; a
    /// row deep in a compressed page is put too (every page then goes as
    /// rows), and one past the history is left out.
    #[test]
    fn rows_put_read_as_the_rows_copied() {
        let mut src = PaneTerm::new(20, 4, 0);
        src.feed(
            b"\x1b[31mred\x1b[m \xe4\xb8\x96\r\n\x1b]8;id=k;http://a\x1b\\link\x1b]8;;\x1b\\\r\n\x1b[42m\x1b[K",
        );
        let copy = src.copy_rows(0..3);
        assert_eq!(copy.len(), 3);
        let want: Vec<String> = lines(&src).into_iter().take(3).collect();

        let mut t = PaneTerm::new(20, 4, 20_000);
        for i in 0..crate::page_rows(20) * 3 {
            t.feed(format!("\x1b[3{}mrow {i}\x1b[m\r\n", i % 8).as_bytes());
        }
        while t.compress_step() == Compression::Pending {}
        // Ghostty reclaims compressed pages' memory on Linux and macOS.
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(t.compressed_pages() > 0, "nothing compressed");
        }
        let before = lines(&t);
        t.put_rows(&copy, 1).unwrap();
        let after = lines(&t);
        let h = t.history_size();
        assert_eq!(after[h + 1..h + 4], want[..], "{after:?}");
        assert_eq!(after[..=h], before[..=h]);
        assert_eq!(t.hyperlink(0, h + 2).as_deref(), Some("http://a"));
        // Deep in history, in a page held compressed.
        let deep = -isize::try_from(h).unwrap() + 3;
        t.put_rows(&copy, deep).unwrap();
        let after = lines(&t);
        assert_eq!(after[3..6], want[..], "{:?}", &after[..8]);
        assert_eq!(after[2], before[2]);
        // Past the oldest row: those rows are left out, the rest put.
        let past = -isize::try_from(h).unwrap() - 1;
        t.put_rows(&copy, past).unwrap();
        assert_eq!(lines(&t)[..2], want[1..3]);
    }
}
