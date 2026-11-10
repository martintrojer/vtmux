//! A pane's terminal state to and from the snapshot format (D27, live
//! upgrade spec `docs/specs/2026-11-06-live-upgrade.md`, decisions 5, 11
//! and 23).
//!
//! [`term_to_snapshot`] exports a [`PaneTerm`] one Ghostty page at a time
//! and encodes each page into the vtmux row format as it comes
//! ([`vtmux_snapshot::PageBuilder`]), so a pane's history never exists
//! whole in the rows form (12 bytes a cell). [`Form::Rows`] writes every
//! page as rows: checkpoints, and a reader of any build. [`Form::SamePin`]
//! lets pages Ghostty holds compressed go in Ghostty's own form
//! ([`vtmux_snapshot::HistoryPage::ghostty`]): upgrade streams to a binary
//! of the same Ghostty patch stack only, behind
//! [`ghostty_pages_requirement`] in `Meta.requires`.
//!
//! [`term_from_snapshot`] builds the pane back, again one page at a time
//! ([`pages_of`] assembles Ghostty pages from the snapshot's history pages
//! and visible rows). A pane imported from rows has its history resident;
//! `Server::from_snapshot` queues it for the compressor at once
//! (`PaneCompress::restored`, decision 23).
//!
//! Exporting reads no event the server drains each loop turn: replies,
//! bells, title changes, passthrough and renames must have been taken
//! (the end of a loop turn) before a pane is exported.

use std::collections::HashMap;
use std::time::Duration;

use vtmux_snapshot::{self as snap, PageBuilder};
use vtmux_vt::{
    self as vt, CellState, CompressedPages, FrozenCell, FrozenLinkId, FrozenRowState,
    GhosttyCharset, GhosttyCursor, GhosttyState, ImportCheck, LinkId, LinkState, ModeState,
    PageBlob, PageCapacity, PageRows, PageState, PaneState, PaneTerm, RowState, SavedCursor,
    ScreenKind, ScreenState, StateError, StateImport, StyleState, TermState,
};

/// What a pane's history pages become in a snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Form {
    /// The vtmux row format, every page: checkpoints, any reader.
    #[default]
    Rows,
    /// Pages Ghostty holds compressed in its own form (decision 23): only
    /// for an upgrade stream to a binary of the same patch stack.
    SamePin,
}

/// This binary's Ghostty patch-stack hash: pages in Ghostty's form go
/// only to a binary that reports the same (decision 23,
/// `sys::upgrade::patch_stack`).
pub const PATCH_STACK: &str = vt::PATCH_STACK;

/// The `Meta.requires` entry a stream with pages in Ghostty's form needs.
#[must_use]
pub fn ghostty_pages_requirement() -> String {
    snap::ghostty_pages_feature(vt::PATCH_STACK)
}

/// What this binary reads beyond the format's own features: pages in its
/// own Ghostty form.
#[must_use]
pub fn reader_features() -> snap::Features {
    snap::Features {
        ghostty_patches: Some(vt::PATCH_STACK.to_owned()),
    }
}

/// The terminal holds a very visible cursor
/// ([`snap::CURSOR_VERY_VISIBLE_FEATURE`]).
#[must_use]
pub fn has_very_visible_cursor(t: &snap::TermState) -> bool {
    t.cursor_style.very_visible
}

/// Whether `t` holds a page in Ghostty's form (the stream then requires
/// [`ghostty_pages_requirement`]).
#[must_use]
pub fn has_ghostty_pages(t: &snap::TermState) -> bool {
    std::iter::once(&t.primary)
        .chain(&t.alternate)
        .any(|s| s.history.iter().any(snap::HistoryPage::is_ghostty))
}

/// tmux's synchronized-output timeout (screen-write.c
/// `screen_write_start_sync`: one second), which `PaneState::sync` counts
/// down and the snapshot counts up.
const SYNC_TIMEOUT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------
// Cells, styles and links.

fn color_out(c: vt::Color, long: bool) -> snap::Color {
    match c {
        vt::Color::Default => snap::Color::Default,
        vt::Color::Palette(n) | vt::Color::Palette256(n) if long => snap::Color::Palette256(n),
        vt::Color::Palette(n) => snap::Color::Palette(n),
        vt::Color::Palette256(n) => snap::Color::Palette256(n),
        vt::Color::Rgb(r, g, b) => snap::Color::Rgb(r, g, b),
    }
}

fn color_in(c: snap::Color) -> vt::Color {
    match c {
        snap::Color::Default => vt::Color::Default,
        snap::Color::Palette(n) => vt::Color::Palette(n),
        snap::Color::Palette256(n) => vt::Color::Palette256(n),
        snap::Color::Rgb(r, g, b) => vt::Color::Rgb(r, g, b),
    }
}

fn underline_out(u: vt::Underline) -> snap::Underline {
    match u {
        vt::Underline::None => snap::Underline::None,
        vt::Underline::Single => snap::Underline::Single,
        vt::Underline::Double => snap::Underline::Double,
        vt::Underline::Curly => snap::Underline::Curly,
        vt::Underline::Dotted => snap::Underline::Dotted,
        vt::Underline::Dashed => snap::Underline::Dashed,
    }
}

fn underline_in(u: snap::Underline) -> vt::Underline {
    match u {
        snap::Underline::None => vt::Underline::None,
        snap::Underline::Single => vt::Underline::Single,
        snap::Underline::Double => vt::Underline::Double,
        snap::Underline::Curly => vt::Underline::Curly,
        snap::Underline::Dotted => vt::Underline::Dotted,
        snap::Underline::Dashed => vt::Underline::Dashed,
    }
}

/// A compositor style as the snapshot's, colours in the form they have.
fn vt_style_out(s: &vt::Style) -> snap::Style {
    use snap::row::attr;
    let mut attrs = 0;
    for (bit, on) in [
        (attr::BOLD, s.bold),
        (attr::FAINT, s.faint),
        (attr::ITALIC, s.italic),
        (attr::BLINK, s.blink),
        (attr::INVERSE, s.inverse),
        (attr::INVISIBLE, s.invisible),
        (attr::STRIKETHROUGH, s.strikethrough),
        (attr::OVERLINE, s.overline),
    ] {
        if on {
            attrs |= bit;
        }
    }
    snap::Style {
        fg: color_out(s.fg, false),
        bg: color_out(s.bg, false),
        underline_color: color_out(s.underline_color, false),
        attrs,
        underline: underline_out(s.underline),
    }
}

fn vt_style_in(s: &snap::Style) -> vt::Style {
    use snap::row::attr;
    let on = |bit| s.attrs & bit != 0;
    vt::Style {
        fg: color_in(s.fg),
        bg: color_in(s.bg),
        underline_color: color_in(s.underline_color),
        bold: on(attr::BOLD),
        faint: on(attr::FAINT),
        italic: on(attr::ITALIC),
        blink: on(attr::BLINK),
        inverse: on(attr::INVERSE),
        invisible: on(attr::INVISIBLE),
        strikethrough: on(attr::STRIKETHROUGH),
        overline: on(attr::OVERLINE),
        underline: underline_in(s.underline),
    }
}

/// A Ghostty style: a palette colour set by `38;5;N` / `48;5;N` is the
/// snapshot's `Palette256`, whatever its index (Ghostty patch 0010).
fn style_out(s: &StyleState) -> snap::Style {
    snap::Style {
        fg: color_out(s.style.fg, s.fg_256),
        bg: color_out(s.style.bg, s.bg_256),
        ..vt_style_out(&s.style)
    }
}

fn style_in(s: &snap::Style) -> StyleState {
    let long = |c| matches!(c, snap::Color::Palette256(_));
    let base = |c| match c {
        snap::Color::Palette256(n) => vt::Color::Palette(n).in_form(true),
        other => color_in(other),
    };
    StyleState {
        style: vt::Style {
            fg: base(s.fg),
            bg: base(s.bg),
            ..vt_style_in(s)
        },
        fg_256: long(s.fg),
        bg_256: long(s.bg),
    }
}

fn link_out(l: &LinkState) -> snap::Hyperlink {
    match &l.id {
        LinkId::Explicit(id) => snap::Hyperlink {
            id: Some(id.clone()),
            uri: l.uri.clone(),
            implicit: 0,
        },
        LinkId::Implicit(n) => snap::Hyperlink {
            id: None,
            uri: l.uri.clone(),
            implicit: *n,
        },
    }
}

fn link_in(l: &snap::Hyperlink) -> LinkState {
    LinkState {
        uri: l.uri.clone(),
        id: l.id.as_ref().map_or(LinkId::Implicit(l.implicit), |id| {
            LinkId::Explicit(id.clone())
        }),
    }
}

fn tab_out(t: u8) -> snap::Tab {
    match t {
        0 => snap::Tab::None,
        255 => snap::Tab::Covered,
        n => snap::Tab::Start(n),
    }
}

fn tab_in(t: snap::Tab) -> u8 {
    match t {
        snap::Tab::None => 0,
        snap::Tab::Covered => 255,
        snap::Tab::Start(n) => n,
    }
}

const WIDES: [snap::Wide; 4] = [
    snap::Wide::Narrow,
    snap::Wide::Wide,
    snap::Wide::SpacerTail,
    snap::Wide::SpacerHead,
];

fn wide_bits(w: snap::Wide) -> u16 {
    match w {
        snap::Wide::Narrow => 0,
        snap::Wide::Wide => 1,
        snap::Wide::SpacerTail => 2,
        snap::Wide::SpacerHead => 3,
    }
}

const SEMANTICS: [snap::Semantic; 3] = [
    snap::Semantic::Output,
    snap::Semantic::Input,
    snap::Semantic::Prompt,
];

fn semantic_bits(s: snap::Semantic) -> u16 {
    match s {
        snap::Semantic::Output => 0,
        snap::Semantic::Input => 1,
        snap::Semantic::Prompt => 2,
    }
}

// Ghostty's cell flags (`vtmux_state.h`, `GhosttyVtmuxCell`).
const KIND_GRAPHEME: u16 = 1;
const KIND_BG_PALETTE: u16 = 2;
const KIND_BG_RGB: u16 = 3;
const PROTECTED: u16 = 1 << 4;
const BG_256: u16 = 1 << 5;

/// Why a snapshot's terminal did not import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermError {
    /// The snapshot's record is out of range or a history page does not
    /// decode: what is wrong.
    Snapshot(String),
    /// The terminal state it decodes to was refused (`vtmux-vt`).
    State(StateError),
}

impl std::fmt::Display for TermError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snapshot(e) => write!(f, "terminal state unreadable: {e}"),
            Self::State(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TermError {}

impl From<StateError> for TermError {
    fn from(e: StateError) -> Self {
        Self::State(e)
    }
}

fn bad(what: &str) -> TermError {
    TermError::Snapshot(what.to_owned())
}

fn narrow<T: TryFrom<u64>>(v: impl Into<u64>, what: &str) -> Result<T, TermError> {
    T::try_from(v.into()).map_err(|_| bad(what))
}

// ---------------------------------------------------------------------
// Ghostty pages to snapshot rows.

/// A page's tables, converted once for all its rows.
struct PageOut<'a> {
    page: &'a PageRows,
    styles: Vec<snap::Style>,
    /// Each cell of more than one codepoint: its index and where its
    /// count is in `page.graphemes`.
    graphemes: Vec<(u32, usize)>,
}

impl<'a> PageOut<'a> {
    fn new(page: &'a PageRows) -> Self {
        let mut graphemes = Vec::new();
        let g = &page.graphemes;
        let mut i = 0;
        while i + 1 < g.len() {
            graphemes.push((g[i], i + 1));
            i += 2 + usize::try_from(g[i + 1]).unwrap_or(usize::MAX - i);
        }
        Self {
            page,
            styles: page.styles.iter().map(style_out).collect(),
            graphemes,
        }
    }

    fn extra(&self, cell: u32) -> &[u32] {
        let Ok(at) = self.graphemes.binary_search_by_key(&cell, |&(c, _)| c) else {
            return &[];
        };
        let at = self.graphemes[at].1;
        let g = &self.page.graphemes;
        let n = usize::try_from(g[at]).unwrap_or(0);
        g.get(at + 1..at + 1 + n).unwrap_or_default()
    }

    /// Row `i` of the page into `out`.
    fn row(&self, index: usize, out: &mut snap::Row) {
        let p = self.page;
        let r = &p.rows[index];
        out.cells.clear();
        out.graphemes.clear();
        out.links.clear();
        out.wrapped = r.flags & RowState::WRAPPED != 0;
        out.continuation = r.flags & RowState::CONTINUATION != 0;
        out.prompt = match (r.flags >> 2) & 3 {
            1 => snap::RowPrompt::Prompt,
            2 => snap::RowPrompt::Continuation,
            _ => snap::RowPrompt::None,
        };
        out.cells_used = r.used;
        out.mark = None;
        out.time = 0;
        let start = r.cell_start as usize;
        let cells = p
            .cells
            .get(start..start + usize::from(r.cell_count))
            .unwrap_or_default();
        let mut links: Vec<u16> = Vec::new();
        for (col, c) in cells.iter().enumerate() {
            let mut style = self
                .styles
                .get(usize::from(c.style))
                .copied()
                .unwrap_or_default();
            let mut flags = 0;
            let mut codepoint = c.content;
            match c.flags & 3 {
                KIND_BG_PALETTE => {
                    let n = c.content.to_le_bytes()[0];
                    style.bg = if c.flags & BG_256 != 0 {
                        snap::Color::Palette256(n)
                    } else {
                        snap::Color::Palette(n)
                    };
                    flags |= snap::row::cell_flag::BG_ONLY;
                    codepoint = 0;
                }
                KIND_BG_RGB => {
                    let [blue, green, red, _] = c.content.to_le_bytes();
                    style.bg = snap::Color::Rgb(red, green, blue);
                    flags |= snap::row::cell_flag::BG_ONLY;
                    codepoint = 0;
                }
                KIND_GRAPHEME => {
                    let extra = self.extra(u32::try_from(start + col).unwrap_or(u32::MAX));
                    let rest: String = extra.iter().filter_map(|&u| char::from_u32(u)).collect();
                    if !rest.is_empty() {
                        out.graphemes.push(snap::Grapheme {
                            col: u16::try_from(col).unwrap_or(u16::MAX),
                            rest,
                        });
                    }
                }
                _ => {}
            }
            // vtmux marks line drawing with Ghostty's protected bit
            // (`PaneTerm::sync_charset`).
            if c.flags & PROTECTED != 0 {
                flags |= snap::row::cell_flag::ACS;
            }
            let link = if c.link == 0 {
                0
            } else {
                let at = links.iter().position(|&l| l == c.link).unwrap_or_else(|| {
                    links.push(c.link);
                    out.links.push(
                        p.links
                            .get(usize::from(c.link))
                            .and_then(Option::as_ref)
                            .map(link_out)
                            .unwrap_or_default(),
                    );
                    links.len() - 1
                });
                u16::try_from(at + 1).unwrap_or(u16::MAX)
            };
            out.cells.push(snap::Cell {
                codepoint,
                wide: WIDES[usize::from((c.flags >> 2) & 3)],
                style,
                flags,
                link,
                semantic: SEMANTICS
                    .get(usize::from((c.flags >> 6) & 3))
                    .copied()
                    .unwrap_or_default(),
                tab: tab_out(c.tab),
            });
        }
        line_flags_out(
            out,
            r.flags & RowState::TMUX_EXTENDED != 0,
            r.flags & RowState::TMUX_HYPERLINK != 0,
        );
    }
}

/// What a row's cells say of tmux's line flags: a cell in tmux's extended
/// form (grid.c `grid_need_extended_cell`: a wide or non-ASCII character,
/// more than one codepoint, an RGB colour, an underline colour, an
/// attribute past the first eight, a tab, a link) and a cell with a link.
/// A spacer head is a cell tmux leaves unwritten.
fn line_flags_of_cells(row: &snap::Row) -> (bool, bool) {
    use snap::row::attr;
    let written = || {
        row.cells
            .iter()
            .filter(|c| c.wide != snap::Wide::SpacerHead)
    };
    let link = written().any(|c| c.link != 0);
    let extended = link
        || !row.graphemes.is_empty()
        || written().any(|c| {
            let st = &c.style;
            c.wide == snap::Wide::Wide
                || (c.codepoint >= 0x80 && c.flags & snap::row::cell_flag::ACS == 0)
                || c.tab != snap::Tab::None
                || matches!(st.fg, snap::Color::Rgb(..))
                || matches!(st.bg, snap::Color::Rgb(..))
                || st.underline_color != snap::Color::Default
                || st.attrs & (attr::STRIKETHROUGH | attr::OVERLINE) != 0
                || !matches!(
                    st.underline,
                    snap::Underline::None | snap::Underline::Single
                )
        });
    (extended, link)
}

/// The line flags a row keeps (Ghostty patch 0034) go in the snapshot only
/// where its cells don't say so ([`snap::Row::kept`]), beside the rows
/// ([`snap::KeptRows`]): what a reader before them derives from the cells
/// is all it misses.
fn keep_line_flags(row: &mut snap::Row, extended: bool, hyperlink: bool) {
    let (by_cells, linked) = line_flags_of_cells(row);
    row.kept = snap::KeptLineFlags {
        extended: extended && !by_cells,
        hyperlink: hyperlink && !linked,
    };
}

/// A pane row's line flags as the emulator holds them, which a row can
/// lack though its cells say them (Ghostty moved or cleared the cells
/// where tmux sets the flag): [`keep_line_flags`], and those it lacks
/// ([`snap::Row::lacks`]), so the flags come back as they were and later
/// output that overwrites the cells finds the same flags.
fn line_flags_out(row: &mut snap::Row, extended: bool, hyperlink: bool) {
    keep_line_flags(row, extended, hyperlink);
    let (by_cells, linked) = line_flags_of_cells(row);
    row.lacks = snap::KeptLineFlags {
        extended: by_cells && !extended,
        hyperlink: linked && !hyperlink,
    };
}

/// The line flags a snapshot row puts back: kept, or said by its cells
/// (which the row held as it was written), unless the row lacks them.
fn line_flags_in(row: &snap::Row) -> (bool, bool) {
    let (by_cells, linked) = line_flags_of_cells(row);
    (
        (row.kept.extended || by_cells) && !row.lacks.extended,
        (row.kept.hyperlink || linked) && !row.lacks.hyperlink,
    )
}

fn layout_of(p: &PageRows) -> snap::term::PageLayout {
    snap::term::PageLayout {
        rows: u32::try_from(p.rows.len()).unwrap_or(u32::MAX),
        cols: u32::from(p.cols),
        capacity_rows: p.capacity.rows,
        capacity_styles: p.capacity.styles,
        capacity_hyperlink_bytes: p.capacity.hyperlink_bytes,
        capacity_grapheme_bytes: p.capacity.grapheme_bytes,
        capacity_string_bytes: p.capacity.string_bytes,
    }
}

/// One screen being exported: pages arrive oldest first; all but the last
/// `visible` rows are history, encoded into history pages as soon as they
/// are known to be history, so at most a screenful and a page are held.
struct ScreenOut {
    visible: usize,
    pending: std::collections::VecDeque<PageRows>,
    pending_rows: usize,
    history: Vec<snap::HistoryPage>,
    rows: Vec<snap::Row>,
    layout: Vec<snap::term::PageLayout>,
    scratch: snap::Row,
}

impl ScreenOut {
    fn new(visible: u16) -> Self {
        Self {
            visible: usize::from(visible),
            pending: std::collections::VecDeque::new(),
            pending_rows: 0,
            history: Vec::new(),
            rows: Vec::new(),
            layout: Vec::new(),
            scratch: snap::Row::default(),
        }
    }

    fn page(&mut self, page: PageState) {
        match page {
            PageState::Blob(blob) => {
                // Only history is ever compressed: what came before is too.
                while let Some(p) = self.pending.pop_front() {
                    self.history_rows(&p, p.rows.len());
                }
                self.pending_rows = 0;
                self.history
                    .push(snap::HistoryPage::from_ghostty(blob.bytes));
            }
            PageState::Rows(p) => {
                self.layout.push(layout_of(&p));
                self.pending_rows += p.rows.len();
                self.pending.push_back(p);
                while let Some(front) = self.pending.front() {
                    let n = front.rows.len();
                    if self.pending_rows - n < self.visible {
                        break;
                    }
                    let front = self.pending.pop_front().expect("front");
                    self.pending_rows -= n;
                    self.history_rows(&front, n);
                }
            }
        }
    }

    /// The first `n` rows of `p` as history pages: one per Ghostty page,
    /// more only where a page would pass the snapshot's page limits.
    fn history_rows(&mut self, p: &PageRows, n: usize) {
        if n == 0 {
            return;
        }
        let out = PageOut::new(p);
        let mut b = PageBuilder::new();
        for i in 0..n {
            out.row(i, &mut self.scratch);
            if b.rows() > 0 && !b.fits(&self.scratch) {
                self.history.push(std::mem::take(&mut b).finish(false));
            }
            b.push(&self.scratch);
        }
        self.history.push(b.finish(false));
    }

    fn finish(mut self, mut out: snap::term::Screen) -> snap::term::Screen {
        let mut history = self.pending_rows.saturating_sub(self.visible);
        while let Some(p) = self.pending.pop_front() {
            let h = history.min(p.rows.len());
            history -= h;
            self.history_rows(&p, h);
            let page = PageOut::new(&p);
            for i in h..p.rows.len() {
                let mut row = snap::Row::default();
                page.row(i, &mut row);
                self.rows.push(row);
            }
        }
        out.history = self.history;
        out.rows = self.rows;
        out.pages = self.layout;
        out
    }
}

// ---------------------------------------------------------------------
// Snapshot rows to Ghostty pages.

/// A Ghostty page being filled from snapshot rows.
struct PageIn {
    page: PageRows,
    /// Rows it is to hold (its layout entry), or `None`: up to the end of
    /// the current history page or visible rows.
    target: Option<usize>,
    styles: HashMap<snap::Style, u16>,
    /// The style looked up last: runs of cells share one.
    last_style: Option<(snap::Style, u16)>,
    links: HashMap<LinkKey, u16>,
}

/// A hyperlink as a page's link table keys it: id, URI, implicit id.
type LinkKey = (Option<Vec<u8>>, Vec<u8>, u32);

impl PageIn {
    fn new(layout: Option<&snap::term::PageLayout>, cols: u16) -> Result<Self, TermError> {
        let (target, cols, capacity) = match layout {
            Some(l) => (
                Some(narrow::<usize>(l.rows, "page rows")?),
                narrow::<u16>(l.cols, "page width")?,
                PageCapacity {
                    rows: l.capacity_rows,
                    styles: l.capacity_styles,
                    hyperlink_bytes: l.capacity_hyperlink_bytes,
                    grapheme_bytes: l.capacity_grapheme_bytes,
                    string_bytes: l.capacity_string_bytes,
                },
            ),
            None => (None, cols, PageCapacity::default()),
        };
        Ok(Self {
            page: PageRows {
                cols,
                styles: vec![StyleState::default()],
                links: vec![None],
                capacity,
                ..PageRows::default()
            },
            target,
            styles: HashMap::new(),
            last_style: None,
            links: HashMap::new(),
        })
    }

    fn full(&self, default_rows: usize) -> bool {
        self.page.rows.len() >= self.target.unwrap_or(default_rows)
    }

    fn style(&mut self, s: &snap::Style) -> Result<u16, TermError> {
        if *s == snap::Style::default() {
            return Ok(0);
        }
        if let Some((last, id)) = self.last_style
            && last == *s
        {
            return Ok(id);
        }
        let id = if let Some(&id) = self.styles.get(s) {
            id
        } else {
            let id = narrow::<u16>(self.page.styles.len() as u64, "styles in a page")?;
            self.page.styles.push(style_in(s));
            self.styles.insert(*s, id);
            id
        };
        self.last_style = Some((*s, id));
        Ok(id)
    }

    fn link(&mut self, l: &snap::Hyperlink) -> Result<u16, TermError> {
        let key = (l.id.clone(), l.uri.clone(), l.implicit);
        if let Some(&id) = self.links.get(&key) {
            return Ok(id);
        }
        let id = narrow::<u16>(self.page.links.len() as u64, "links in a page")?;
        self.page.links.push(Some(link_in(l)));
        self.links.insert(key, id);
        Ok(id)
    }

    fn push(&mut self, row: &snap::Row) -> Result<(), TermError> {
        if row.cells.len() > usize::from(self.page.cols) {
            return Err(bad("row wider than its page"));
        }
        let start = narrow::<u32>(self.page.cells.len() as u64, "cells in a page")?;
        for (col, c) in row.cells.iter().enumerate() {
            let mut flags = (wide_bits(c.wide) << 2) | (semantic_bits(c.semantic) << 6);
            if c.flags & snap::row::cell_flag::ACS != 0 {
                flags |= PROTECTED;
            }
            let mut style = c.style;
            let content = if c.flags & snap::row::cell_flag::BG_ONLY != 0 {
                let bg = std::mem::take(&mut style.bg);
                match bg {
                    snap::Color::Palette(n) => {
                        flags |= KIND_BG_PALETTE;
                        u32::from(n)
                    }
                    snap::Color::Palette256(n) => {
                        flags |= KIND_BG_PALETTE | BG_256;
                        u32::from(n)
                    }
                    snap::Color::Rgb(r, g, b) => {
                        flags |= KIND_BG_RGB;
                        u32::from_le_bytes([b, g, r, 0])
                    }
                    snap::Color::Default => 0,
                }
            } else {
                let extra = if row.graphemes.is_empty() {
                    None
                } else {
                    row.graphemes
                        .iter()
                        .find(|g| usize::from(g.col) == col && !g.rest.is_empty())
                };
                if let Some(g) = extra {
                    flags |= KIND_GRAPHEME;
                    let index = start + narrow::<u32>(col as u64, "column")?;
                    let n = g.rest.chars().count();
                    self.page.graphemes.push(index);
                    self.page
                        .graphemes
                        .push(narrow::<u32>(n as u64, "grapheme length")?);
                    self.page.graphemes.extend(g.rest.chars().map(u32::from));
                }
                c.codepoint
            };
            let link = match c.link {
                0 => 0,
                n => {
                    let l = row
                        .links
                        .get(usize::from(n) - 1)
                        .ok_or_else(|| bad("link outside its row"))?;
                    self.link(l)?
                }
            };
            let style = self.style(&style)?;
            self.page.cells.push(CellState {
                content,
                style,
                link,
                flags,
                tab: tab_in(c.tab),
                reserved: 0,
            });
        }
        let prompt = match row.prompt {
            snap::RowPrompt::None => 0,
            snap::RowPrompt::Prompt => 1,
            snap::RowPrompt::Continuation => 2,
        };
        let (extended, hyperlink) = line_flags_in(row);
        let mut flags = u8::from(row.wrapped) | (u8::from(row.continuation) << 1) | (prompt << 2);
        if extended {
            flags |= RowState::TMUX_EXTENDED;
        }
        if hyperlink {
            flags |= RowState::TMUX_HYPERLINK;
        }
        self.page.rows.push(RowState {
            cell_start: start,
            cell_count: narrow::<u16>(row.cells.len() as u64, "row width")?,
            used: row.cells_used,
            flags,
            reserved: [0; 3],
        });
        Ok(())
    }
}

/// Whether `s.pages` describes its rows: every row page adds up, and no
/// page spans a page in Ghostty's form. A layout that doesn't is ignored.
fn layout_fits(s: &snap::term::Screen) -> bool {
    if s.pages.is_empty()
        || s.pages.iter().any(|l| {
            let most = u32::from(u16::MAX);
            l.rows == 0 || l.rows > most || l.cols == 0 || l.cols > most || l.capacity_rows > most
        })
    {
        return false;
    }
    // The page boundaries, as rows before each; 0 too.
    let bounds: Vec<u64> = std::iter::once(0)
        .chain(s.pages.iter().scan(0u64, |at, l| {
            *at += u64::from(l.rows);
            Some(*at)
        }))
        .collect();
    let mut at = 0u64;
    for p in &s.history {
        if p.is_ghostty() {
            // A row page may not straddle a page in Ghostty's form.
            if bounds.binary_search(&at).is_err() {
                return false;
            }
        } else {
            at += u64::from(p.rows);
        }
    }
    at += s.rows.len() as u64;
    bounds.last() == Some(&at)
}

/// Ghostty pages from a snapshot screen, oldest first, each handed to
/// `emit` as soon as it is complete: the history pages' rows (decoded one
/// page at a time), the pages in Ghostty's form as they are, then the
/// visible rows, split as [`snap::term::Screen::pages`] says when it adds
/// up, else at every history page and every `vt::page_rows(cols)` rows.
///
/// # Errors
/// A history page that does not decode, rows that don't fit their pages,
/// or what `emit` returns.
pub fn pages_of(
    s: &snap::term::Screen,
    cols: u16,
    stack: &str,
    emit: &mut dyn FnMut(&PageState) -> Result<(), TermError>,
) -> Result<(), TermError> {
    struct Asm<'a> {
        layout: Option<std::slice::Iter<'a, snap::term::PageLayout>>,
        cols: u16,
        default_rows: usize,
        cur: Option<PageIn>,
    }
    impl Asm<'_> {
        fn push(
            &mut self,
            row: &snap::Row,
            emit: &mut dyn FnMut(&PageState) -> Result<(), TermError>,
        ) -> Result<(), TermError> {
            if self.cur.is_none() {
                let entry = self.layout.as_mut().and_then(Iterator::next);
                self.cur = Some(PageIn::new(entry, self.cols)?);
            }
            let cur = self.cur.as_mut().expect("started");
            cur.push(row)?;
            if cur.full(self.default_rows) {
                self.flush(emit)?;
            }
            Ok(())
        }

        fn flush(
            &mut self,
            emit: &mut dyn FnMut(&PageState) -> Result<(), TermError>,
        ) -> Result<(), TermError> {
            match self.cur.take() {
                Some(p) if !p.page.rows.is_empty() => emit(&PageState::Rows(p.page)),
                _ => Ok(()),
            }
        }
    }
    let mut asm = Asm {
        layout: layout_fits(s).then(|| s.pages.iter()),
        cols,
        default_rows: vt::page_rows(cols),
        cur: None,
    };
    for page in &s.history {
        if page.is_ghostty() {
            asm.flush(emit)?;
            emit(&PageState::Blob(PageBlob {
                stack: stack.to_owned(),
                bytes: page.ghostty.clone(),
            }))?;
            continue;
        }
        let mut failed = Ok(());
        page.each_row(|row| {
            if failed.is_ok() {
                failed = asm.push(row, emit);
            }
        })
        .map_err(|e| TermError::Snapshot(e.to_string()))?;
        failed?;
        if asm.layout.is_none() {
            asm.flush(emit)?;
        }
    }
    for row in &s.rows {
        asm.push(row, emit)?;
    }
    asm.flush(emit)
}

// ---------------------------------------------------------------------
// Rows PaneTerm keeps beside Ghostty: frozen history, the sync hold.

fn text_cell(text: &str, col: usize, graphemes: &mut Vec<snap::Grapheme>) -> u32 {
    let mut chars = text.chars();
    let first = chars.next().map_or(0, u32::from);
    let rest = chars.as_str();
    if !rest.is_empty() {
        graphemes.push(snap::Grapheme {
            col: u16::try_from(col).unwrap_or(u16::MAX),
            rest: rest.to_owned(),
        });
    }
    first
}

fn cell_text(row: &snap::Row, col: usize, c: &snap::Cell) -> String {
    let mut text = String::new();
    if let Some(ch) = char::from_u32(c.codepoint).filter(|&ch| ch != '\0') {
        text.push(ch);
        if let Some(g) = row.graphemes.iter().find(|g| usize::from(g.col) == col) {
            text.push_str(&g.rest);
        }
    }
    text
}

fn frozen_out(r: &FrozenRowState) -> snap::Row {
    let mut row = snap::Row {
        wrapped: r.wrapped,
        ..snap::Row::default()
    };
    for (col, c) in r.cells.iter().enumerate() {
        let codepoint = text_cell(&c.text, col, &mut row.graphemes);
        let link = match (&c.link, &c.target) {
            (false, _) => 0,
            (true, target) => {
                let (uri, id) = target.clone().unwrap_or((None, None));
                let mut l = snap::Hyperlink {
                    uri: uri.unwrap_or_default().into_bytes(),
                    ..snap::Hyperlink::default()
                };
                match id {
                    Some(FrozenLinkId::Explicit(id)) => l.id = Some(id),
                    Some(FrozenLinkId::Implicit(n)) => l.implicit = n,
                    Some(FrozenLinkId::None) | None => {}
                }
                let at = row.links.iter().position(|x| *x == l).unwrap_or_else(|| {
                    row.links.push(l);
                    row.links.len() - 1
                });
                u16::try_from(at + 1).unwrap_or(u16::MAX)
            }
        };
        row.cells.push(snap::Cell {
            codepoint,
            wide: WIDES[usize::from(c.wide & 3)],
            style: vt_style_out(&c.style),
            flags: if c.acs { snap::row::cell_flag::ACS } else { 0 },
            link,
            semantic: snap::Semantic::Output,
            tab: tab_out(c.tab),
        });
    }
    // The row's width: trailing default blanks were not kept.
    row.cells
        .resize(r.len.max(r.cells.len()), snap::Cell::default());
    keep_line_flags(&mut row, r.kept_extended, r.kept_hyperlink);
    row
}

fn frozen_in(row: &snap::Row) -> FrozenRowState {
    let blank = |c: &snap::Cell| {
        c.codepoint == 0
            && c.style == snap::Style::default()
            && c.wide == snap::Wide::Narrow
            && c.link == 0
            && c.tab == snap::Tab::None
    };
    let kept = row.cells.len() - row.cells.iter().rev().take_while(|c| blank(c)).count();
    let cells = row.cells[..kept]
        .iter()
        .enumerate()
        .map(|(col, c)| {
            let text = cell_text(row, col, c);
            let target = (c.link != 0).then(|| {
                let l = row
                    .links
                    .get(usize::from(c.link) - 1)
                    .cloned()
                    .unwrap_or_default();
                let id = match l.id {
                    Some(id) => FrozenLinkId::Explicit(id),
                    None => FrozenLinkId::Implicit(l.implicit),
                };
                (Some(String::from_utf8_lossy(&l.uri).into_owned()), Some(id))
            });
            FrozenCell {
                empty: text.is_empty(),
                text,
                style: vt_style_in(&c.style),
                wide: u8::try_from(wide_bits(c.wide)).unwrap_or(0),
                acs: c.flags & snap::row::cell_flag::ACS != 0,
                link: c.link != 0,
                target,
                tab: tab_in(c.tab),
            }
        })
        .collect();
    let (kept_extended, kept_hyperlink) = line_flags_in(row);
    FrozenRowState {
        cells,
        len: row.cells.len(),
        wrapped: row.wrapped,
        kept_extended,
        kept_hyperlink,
    }
}

fn sync_out(screen: &vt::Snapshot, left: Duration) -> snap::term::SyncHold {
    let cols = usize::from(screen.cols);
    let rows = screen
        .cells
        .chunks(cols.max(1))
        .take(usize::from(screen.rows))
        .enumerate()
        .map(|(y, cells)| {
            let mut row = snap::Row {
                wrapped: screen.wrapped.get(y).copied().unwrap_or(false),
                ..snap::Row::default()
            };
            // Each link as it was drawn: its number as the id, its URI as
            // the link keeps it.
            let mut links: Vec<&std::sync::Arc<vt::Link>> = Vec::new();
            for (col, c) in cells.iter().enumerate() {
                let link = c.link.as_ref().map_or(0, |l| {
                    let at = links.iter().position(|k| k == &l).unwrap_or_else(|| {
                        links.push(l);
                        row.links.push(snap::Hyperlink {
                            id: Some(l.number().to_string().into_bytes()),
                            uri: l.uri().as_bytes().to_vec(),
                            implicit: 0,
                        });
                        links.len() - 1
                    });
                    u16::try_from(at + 1).unwrap_or(u16::MAX)
                });
                let codepoint = text_cell(c.text.as_str(), col, &mut row.graphemes);
                row.cells.push(snap::Cell {
                    codepoint,
                    wide: match c.width {
                        0 => snap::Wide::SpacerTail,
                        2 => snap::Wide::Wide,
                        _ => snap::Wide::Narrow,
                    },
                    style: vt_style_out(&c.style),
                    flags: if c.acs { snap::row::cell_flag::ACS } else { 0 },
                    link,
                    semantic: snap::Semantic::Output,
                    tab: if c.tab {
                        snap::Tab::Covered
                    } else {
                        snap::Tab::None
                    },
                });
            }
            row
        })
        .collect();
    snap::term::SyncHold {
        rows,
        since_ms: u64::try_from(SYNC_TIMEOUT.saturating_sub(left).as_millis()).unwrap_or(0),
        cols: u32::from(screen.cols),
        cursor: screen.cursor.map(|(x, y)| (u32::from(x), u32::from(y))),
        cursor_visible: screen.cursor_visible,
    }
}

/// The link of a held screen's cell, as [`sync_out`] wrote it; none for
/// one a build before it wrote (no number).
fn held_link(row: &snap::Row, link: u16) -> Option<std::sync::Arc<vt::Link>> {
    let l = row.links.get(usize::from(link).checked_sub(1)?)?;
    let number = std::str::from_utf8(l.id.as_deref()?).ok()?.parse().ok()?;
    Some(vt::Link::restored(number, &String::from_utf8_lossy(&l.uri)))
}

fn sync_in(h: &snap::term::SyncHold) -> Result<(vt::Snapshot, Duration), TermError> {
    let cols = narrow::<u16>(h.cols, "held screen width")?;
    let mut cells = Vec::with_capacity(usize::from(cols) * h.rows.len());
    for row in &h.rows {
        for col in 0..usize::from(cols) {
            let c = row.cells.get(col).copied().unwrap_or_default();
            cells.push(vt::Cell {
                text: vt::CellText::from(cell_text(row, col, &c).as_str()),
                width: match c.wide {
                    snap::Wide::SpacerTail => 0,
                    snap::Wide::Wide => 2,
                    _ => 1,
                },
                style: vt_style_in(&c.style),
                acs: c.flags & snap::row::cell_flag::ACS != 0,
                link: held_link(row, c.link),
                tab: c.tab != snap::Tab::None,
            });
        }
    }
    let cursor = match h.cursor {
        Some((x, y)) => Some((narrow(x, "held cursor")?, narrow(y, "held cursor")?)),
        None => None,
    };
    let screen = vt::Snapshot {
        cols,
        rows: narrow::<u16>(h.rows.len() as u64, "held screen height")?,
        cells,
        wrapped: h.rows.iter().map(|r| r.wrapped).collect(),
        cursor,
        cursor_visible: h.cursor_visible,
    };
    let since = Duration::from_millis(h.since_ms);
    Ok((screen, SYNC_TIMEOUT.saturating_sub(since)))
}

// ---------------------------------------------------------------------
// The state outside the grid.

const CHARSETS: [snap::term::Charset; 4] = [
    snap::term::Charset::Utf8,
    snap::term::Charset::Ascii,
    snap::term::Charset::British,
    snap::term::Charset::DecSpecial,
];

fn charsets_out(c: GhosttyCharset) -> snap::term::Charsets {
    let g = |i: usize| {
        CHARSETS
            .get(usize::from(c.g[i]))
            .copied()
            .unwrap_or(snap::term::Charset::Utf8)
    };
    snap::term::Charsets {
        g0: g(0),
        g1: g(1),
        g2: g(2),
        g3: g(3),
        gl: u32::from(c.gl),
        gr: u32::from(c.gr),
        single_shift: c.single_shift.map(u32::from),
    }
}

fn charsets_in(c: &snap::term::Charsets) -> Result<GhosttyCharset, TermError> {
    let g = |cs: snap::term::Charset| -> Result<u8, TermError> {
        CHARSETS
            .iter()
            .position(|&x| x == cs)
            .and_then(|i| u8::try_from(i).ok())
            .ok_or_else(|| bad("charset the emulator lacks"))
    };
    Ok(GhosttyCharset {
        g: [g(c.g0)?, g(c.g1)?, g(c.g2)?, g(c.g3)?],
        gl: narrow(c.gl, "charset slot")?,
        gr: narrow(c.gr, "charset slot")?,
        single_shift: c
            .single_shift
            .map(|s| narrow(s, "charset slot"))
            .transpose()?,
    })
}

fn rgb_out((r, g, b): (u8, u8, u8)) -> u32 {
    u32::from_be_bytes([0, r, g, b])
}

fn rgb_in(c: u32) -> (u8, u8, u8) {
    let [_, r, g, b] = c.to_be_bytes();
    (r, g, b)
}

fn osc133_out(m: &vt::Osc133) -> snap::term::Osc133Rec {
    snap::term::Osc133Rec {
        flags: m.flags,
        prompt_col: m.prompt_col.into(),
        cmd_col: m.cmd_col.into(),
        out_start_col: m.out_start_col.into(),
        out_end_col: m.out_end_col.into(),
        exit_status: m.exit_status.into(),
    }
}

fn osc133_in(m: &snap::term::Osc133Rec) -> Result<vt::Osc133, TermError> {
    Ok(vt::Osc133 {
        flags: m.flags,
        prompt_col: narrow(m.prompt_col, "mark column")?,
        cmd_col: narrow(m.cmd_col, "mark column")?,
        out_start_col: narrow(m.out_start_col, "mark column")?,
        out_end_col: narrow(m.out_end_col, "mark column")?,
        exit_status: narrow(m.exit_status, "exit status")?,
    })
}

fn mark_out(row: u64, m: &vt::Osc133) -> snap::term::FrozenMark {
    snap::term::FrozenMark {
        row,
        mark: osc133_out(m),
    }
}

/// The primary screen's OSC 133 marks, each with its row.
fn marks_out(marks: &[(u32, vt::Osc133)]) -> Vec<snap::term::FrozenMark> {
    marks
        .iter()
        .map(|(y, m)| mark_out(u64::from(*y), m))
        .collect()
}

fn screen_out(s: &ScreenState, visible: ScreenOut) -> snap::term::Screen {
    let c = &s.cursor;
    let screen = snap::term::Screen {
        cursor: snap::term::Cursor {
            x: c.x.into(),
            y: c.y.into(),
            pending_wrap: c.pending_wrap,
            style: style_out(&c.pen),
            protected: c.protected,
            link: c.link.as_ref().map(link_out),
            semantic: c.semantic_content.into(),
            visual: c.visual.into(),
            semantic_clear_eol: c.semantic_clear_eol,
        },
        saved_cursor: s.saved_cursor.as_ref().map(|v| snap::term::SavedCursor {
            x: v.x.into(),
            y: v.y.into(),
            pending_wrap: v.pending_wrap,
            style: style_out(&v.pen),
            protected: v.protected,
            origin: v.origin,
            charsets: charsets_out(v.charset),
        }),
        charsets: charsets_out(s.charset),
        kitty_keyboard: s.kitty_flags.iter().map(|&f| u32::from(f)).collect(),
        kitty_index: s.kitty_index.into(),
        protected_mode: s.protected_mode.into(),
        semantic_click_kind: s.semantic_click.0.into(),
        semantic_click_value: s.semantic_click.1.into(),
        hyperlink_counter: s.hyperlink_counter,
        ..snap::term::Screen::default()
    };
    visible.finish(screen)
}

fn saved_cursor_in(v: &snap::term::SavedCursor) -> Result<SavedCursor, TermError> {
    Ok(SavedCursor {
        x: narrow(v.x, "saved cursor")?,
        y: narrow(v.y, "saved cursor")?,
        pen: style_in(&v.style),
        protected: v.protected,
        pending_wrap: v.pending_wrap,
        origin: v.origin,
        charset: charsets_in(&v.charsets)?,
    })
}

/// What `?1049h` saved. A writer from before
/// [`snap::TermState::alternate_saved`] (no
/// [`snap::TermState::alternate_saved_layout`]) kept it in the primary
/// screen's DECSC slot while the alternate screen was up with mode 1049,
/// where its `?1049l` restored it from (upstream Ghostty's `saveCursor`).
fn alternate_saved_in(
    t: &snap::TermState,
    primary: &snap::term::Screen,
) -> Result<Option<vt::AltSaved>, TermError> {
    let saved = |x: u32, y: u32, style: &snap::Style| -> Result<vt::AltSaved, TermError> {
        Ok(vt::AltSaved {
            x: narrow(x, "alternate saved cursor")?,
            y: narrow(y, "alternate saved cursor")?,
            pen: style_in(style),
        })
    };
    if t.alternate_saved_layout.is_some() {
        return t
            .alternate_saved
            .as_ref()
            .map(|a| saved(a.x, a.y, &a.style))
            .transpose();
    }
    let in_1049 = t.active == snap::term::ActiveScreen::Alternate
        && t.modes.iter().any(|m| m.number == 1049 && !m.ansi && m.on);
    match &primary.saved_cursor {
        // tmux's `cx` past the last column while a wrap is pending.
        Some(c) if in_1049 => {
            saved(c.x.saturating_add(c.pending_wrap.into()), c.y, &c.style).map(Some)
        }
        _ => Ok(None),
    }
}

fn screen_in(s: &snap::term::Screen) -> Result<ScreenState, TermError> {
    let c = &s.cursor;
    let mut kitty = [0u8; 8];
    for (slot, &f) in kitty.iter_mut().zip(&s.kitty_keyboard) {
        *slot = narrow(f, "kitty keyboard flags")?;
    }
    Ok(ScreenState {
        pages: Vec::new(),
        cursor: GhosttyCursor {
            x: narrow(c.x, "cursor")?,
            y: narrow(c.y, "cursor")?,
            pending_wrap: c.pending_wrap,
            visual: narrow(c.visual, "cursor shape")?,
            protected: c.protected,
            pen: style_in(&c.style),
            link: c.link.as_ref().map(link_in),
            semantic_content: narrow(c.semantic, "content kind")?,
            semantic_clear_eol: c.semantic_clear_eol,
        },
        saved_cursor: s.saved_cursor.as_ref().map(saved_cursor_in).transpose()?,
        charset: charsets_in(&s.charsets)?,
        protected_mode: narrow(s.protected_mode, "protected mode")?,
        kitty_flags: kitty,
        kitty_index: narrow(s.kitty_index, "kitty keyboard index")?,
        semantic_click: (
            narrow(s.semantic_click_kind, "click kind")?,
            narrow(s.semantic_click_value, "click value")?,
        ),
        hyperlink_counter: s.hyperlink_counter,
    })
}

fn times_out(times: &[i64]) -> Vec<snap::term::TimeRun> {
    let mut out: Vec<snap::term::TimeRun> = Vec::new();
    for &t in times {
        match out.last_mut() {
            Some(run) if run.time == t => run.count += 1,
            _ => out.push(snap::term::TimeRun { time: t, count: 1 }),
        }
    }
    out
}

fn times_in(runs: &[snap::term::TimeRun], limit: usize) -> Result<Vec<i64>, TermError> {
    let mut out = Vec::new();
    for r in runs {
        let n = narrow::<usize>(r.count, "history times")?;
        if out.len() + n > limit {
            return Err(bad("more history times than history"));
        }
        out.extend(std::iter::repeat_n(r.time, n));
    }
    Ok(out)
}

/// `term`'s state and its screens' pages: the primary's, and the
/// alternate's unless `primary_only`.
fn export_screens(
    term: &PaneTerm,
    compressed: CompressedPages,
    primary_only: bool,
) -> (TermState, ScreenOut, ScreenOut) {
    let v = term.export_state_head();
    let mut primary = ScreenOut::new(v.primary_size.map_or(v.rows, |(_, rows)| rows));
    let mut alternate = ScreenOut::new(v.rows);
    let done: Result<(), std::convert::Infallible> =
        term.export_pages(compressed, |screen, page| {
            match screen {
                ScreenKind::Primary => primary.page(page),
                ScreenKind::Alternate if !primary_only => alternate.page(page),
                ScreenKind::Alternate => {}
            }
            Ok(())
        });
    let Ok(()) = done;
    (v, primary, alternate)
}

/// `term`'s primary screen alone, with its OSC 133 marks and the
/// alternate screen's entry row in its rows.
fn primary_of(
    term: &PaneTerm,
    compressed: CompressedPages,
) -> (snap::term::Screen, Vec<snap::term::FrozenMark>, Option<u64>) {
    let (v, out, _) = export_screens(term, compressed, true);
    let p = &v.pane;
    (
        screen_out(&v.primary, out),
        marks_out(&p.marks),
        p.alt_entry.map(|(y, _)| u64::from(y)),
    )
}

/// What a reader from before [`snap::TermState::sized_primary`],
/// [`snap::TermState::alternate_saved`] and Ghostty patch 0038 (a
/// downgrade, or an older binary restoring a checkpoint) needs where it
/// reads: the copy of tmux's history it shows under the alternate screen,
/// the primary screen at the terminal's size, as its emulator kept it, and
/// `?1049h`'s save in the primary screen's DECSC slot, where its `?1049l`
/// restores from. Only for [`Form::Rows`]: [`Form::SamePin`] goes to a
/// binary of this patch stack, which reads all three where they are (an
/// older one has another stack), so `?1049h` and a same-binary upgrade
/// never pay for the copy.
fn for_older_readers(term: &PaneTerm, compressed: CompressedPages, t: &mut snap::TermState) {
    // A reader from before Ghostty patch 0038 shows, under the alternate
    // screen, the copy of tmux's history it took on `?1049h`: the newest
    // `hsize` rows of the primary screen as it was left (not reflowed).
    if t.active == snap::term::ActiveScreen::Alternate
        && let Some(alt) = t.alt_entry.as_mut()
    {
        let hsize = usize::try_from(t.history.hsize).unwrap_or(usize::MAX);
        alt.history = history_tail(&t.primary.history, hsize);
        alt.has_history = true;
    }
    if let Some(sized) = t.sized_primary.as_mut() {
        let copy = term
            .with_primary_resized()
            .expect("a primary screen at another size");
        let (screen, marks, entry_row) = primary_of(&copy, compressed);
        sized.screen = Some(std::mem::replace(&mut t.primary, screen));
        sized.marks = std::mem::replace(&mut t.marks, marks);
        if let Some(alt) = t.alt_entry.as_mut() {
            sized.entry_row = std::mem::replace(&mut alt.entry_row, entry_row);
        }
    }
    let layout = t
        .alternate_saved_layout
        .as_mut()
        .expect("written with the slot");
    if t.active != snap::term::ActiveScreen::Alternate {
        return;
    }
    let Some(a) = &t.alternate_saved else {
        return;
    };
    // `t.primary` is at the terminal's size now.
    let cols = t.cols.max(1);
    let origin = t.modes.iter().any(|m| m.number == 6 && !m.ansi && m.on);
    let save = snap::term::SavedCursor {
        x: a.x.min(cols - 1),
        y: a.y,
        pending_wrap: a.x >= cols,
        style: a.style,
        protected: false,
        origin,
        // tmux's ?1049l keeps the charsets, which both screens share.
        charsets: t
            .alternate
            .as_ref()
            .map_or_else(|| t.primary.charsets.clone(), |alt| alt.charsets.clone()),
    };
    let own = t.primary.saved_cursor.replace(save);
    // Unless the screen a reader with the field takes is another one.
    if t.sized_primary.is_none() {
        layout.moved = true;
        layout.primary_saved = own;
    }
}

/// The newest `n` rows of `pages`, as pages: whole pages are the same
/// pages (a checkpoint stores their chunk once), the one the cut falls in
/// is encoded again from its cut.
fn history_tail(pages: &[snap::HistoryPage], n: usize) -> Vec<snap::HistoryPage> {
    let mut left = n;
    let mut from = pages.len();
    while let Some(rows) = from.checked_sub(1).map(|i| pages[i].rows as usize)
        && rows <= left
    {
        from -= 1;
        left -= rows;
    }
    let mut out = Vec::with_capacity(pages.len() - from + 1);
    if left > 0
        && let Some(cut) = from.checked_sub(1).map(|i| &pages[i])
    {
        let skip = cut.rows as usize - left;
        let mut b = PageBuilder::new();
        let mut i = 0;
        cut.each_row(|row| {
            if i >= skip {
                b.push(row);
            }
            i += 1;
        })
        .expect("a page this export encoded");
        out.push(b.finish(false));
    }
    out.extend_from_slice(&pages[from..]);
    out
}

/// Export `term` as a snapshot record, one page at a time (see the module
/// docs). The server's per-turn events must have been drained.
#[must_use]
#[expect(clippy::too_many_lines, reason = "one line per field carried")]
pub fn term_to_snapshot(term: &PaneTerm, form: Form) -> snap::TermState {
    let compressed = match form {
        Form::Rows => CompressedPages::Rows,
        Form::SamePin => CompressedPages::Blob,
    };
    let (v, primary, alternate) = export_screens(term, compressed, false);
    let (g, p) = (&v.ghostty, &v.pane);
    let modes = g
        .modes
        .iter()
        .map(|m| snap::term::Mode {
            number: m.number.into(),
            ansi: m.ansi,
            on: m.current,
            saved: m.saved,
            default: Some(m.default),
        })
        .collect();
    let alt = p.saved.is_some()
        || p.alt_entry.is_some()
        || !p.frozen_marks.is_empty()
        || p.alt_collected != 0
        || p.clear_primary_history;
    let alt_entry = alt.then(|| snap::term::AltEntry {
        saved: p
            .saved
            .as_ref()
            .map(|rows| rows.iter().map(frozen_out).collect())
            .unwrap_or_default(),
        // The primary screen's own history, in `primary`: the alternate
        // screen leaves it as it was (Ghostty patch 0038). A copy for an
        // older reader goes with rows (`for_older_readers`).
        history: Vec::new(),
        marks: p
            .frozen_marks
            .iter()
            .map(|(y, m)| mark_out(*y as u64, m))
            .collect(),
        ghostty_history: p.alt_entry.map_or(0, |(_, r)| r as u64),
        clear_primary_history: p.clear_primary_history,
        collected: p.alt_collected as u64,
        entry_row: p.alt_entry.map(|(y, _)| u64::from(y)),
        has_saved: p.saved.is_some(),
        has_history: false,
    });
    let (g0, g1, shifted, (s0, s1, ss)) = p.charset;
    let (shape, blinking, blinking_set, option) = p.cursor_style;
    let mut t = snap::TermState {
        cols: v.cols.into(),
        rows: v.rows.into(),
        primary: screen_out(&v.primary, primary),
        alternate: v.alternate.as_ref().map(|alt| screen_out(alt, alternate)),
        active: if v.alternate_active {
            snap::term::ActiveScreen::Alternate
        } else {
            snap::term::ActiveScreen::Primary
        },
        modes,
        tab_stops: g.tab_stops.iter().map(|&c| u32::from(c)).collect(),
        scroll_region: snap::term::ScrollRegion {
            top: g.scroll_region.0.into(),
            bottom: g.scroll_region.1.into(),
            left: g.scroll_region.2.into(),
            right: g.scroll_region.3.into(),
        },
        title: g.title.clone(),
        server_title: p.title.clone(),
        title_stack: p.title_stack.clone(),
        pwd: g.pwd.clone(),
        palette: palette_out(&p.palette.entries),
        fg: p.palette.fg.map(rgb_out),
        bg: p.palette.bg.map(rgb_out),
        cursor_colour: p.palette.cursor.map(rgb_out),
        cursor_style: snap::term::CursorStyle {
            shape: shape.into(),
            blinking,
            blinking_set,
            option: option.into(),
            very_visible: p.cursor_very_visible,
        },
        mouse: snap::term::MouseModes {
            tracking: match p.mouse.tracking {
                vt::MouseTracking::None => 0,
                vt::MouseTracking::Standard => 1000,
                vt::MouseTracking::Button => 1002,
                vt::MouseTracking::All => 1003,
            },
            utf8: p.mouse.utf8,
            sgr: p.mouse.sgr,
        },
        modify_other_keys: match p.key_mode {
            vt::KeyMode::Standard => 0,
            vt::KeyMode::Extended1 => 1,
            vt::KeyMode::Extended2 => 2,
        },
        history: snap::term::HistoryState {
            limit: p.history_limit as u64,
            enabled: p.keeps_history,
            hsize: p.hsize as u64,
            scrolled: p.scrolled,
            generation: p.generation,
            line_limit: g.max_lines.map(|n| n as u64),
            reads: 0,
            times_base: p.times_base,
            times: times_out(&p.times),
            max_bytes: g.max_bytes.map(|n| n as u64),
            hscrolled: Some(p.hscrolled as u64),
        },
        alt_entry,
        sync: p.sync.as_ref().map(|(s, left)| sync_out(s, *left)),
        replies: Vec::new(),
        pending: p.continuation.clone(),
        dcs: Vec::new(),
        dcs_since_ms: None,
        rename: None,
        input_buffer_size: p.input_buffer_size as u64,
        scroll_on_clear: p.scroll_on_clear,
        bells: 0,
        redraw: p.redraw,
        marks: marks_out(&p.marks),
        tmux_charset: snap::term::TmuxCharset {
            g0_acs: g0,
            g1_acs: g1,
            shifted,
            saved_g0_acs: s0,
            saved_g1_acs: s1,
            saved_shifted: ss,
        },
        extended_keys: match p.extended_keys {
            vt::ExtendedKeys::Off => 0,
            vt::ExtendedKeys::On => 1,
            vt::ExtendedKeys::Always => 2,
        },
        input_last: p.last_print,
        sized_primary: v.primary_size.map(|(c, r)| snap::term::SizedPrimary {
            cols: c.into(),
            rows: r.into(),
            ..snap::term::SizedPrimary::default()
        }),
        alternate_saved: g.alternate_saved.map(|a| snap::term::AltSaved {
            x: a.x.into(),
            y: a.y.into(),
            style: style_out(&a.pen),
        }),
        alternate_saved_layout: Some(snap::term::AltSaveLayout::default()),
        progress_bar: snap::term::ProgressBar {
            state: p.progress_bar.state.number().into(),
            progress: p.progress_bar.progress.into(),
        },
        drawn_links: p
            .drawn_links
            .iter()
            .map(|(uri, id, number)| {
                let (id, implicit, alternate) = match id {
                    vt::LinkKeyId::Explicit(id) => (Some(id.clone()), 0, false),
                    vt::LinkKeyId::Implicit { alternate, n } => (None, *n, *alternate),
                };
                snap::term::DrawnLink {
                    uri: uri.clone().into_bytes(),
                    id,
                    implicit,
                    alternate,
                    number: *number,
                }
            })
            .collect(),
        pen_links: (p.links != vt::PenLinkState::default()).then(|| snap::term::PenLinks {
            pen: p.links.pen.clone(),
            saved: p.links.saved.clone(),
            alternate: p.links.alternate.clone(),
            emulator: p.links.emulator.clone(),
        }),
        inband_resize: p.inband_resize,
        graphics_primary: p.graphics[0],
        graphics_alternate: p.graphics[1],
        emulator: snap::term::Emulator {
            status_display: g.status_display.into(),
            previous_char: g.previous_char,
            cursor_is_default: g.cursor_is_default,
            cursor_default_style: g.cursor_default_style.into(),
            cursor_default_blink: g.cursor_default_blink,
            shell_redraw: g.shell_redraw.into(),
            modify_other_keys_2: g.modify_other_keys_2,
            mouse_event: g.mouse_event.into(),
            mouse_format: g.mouse_format.into(),
            mouse_shift_capture: g.mouse_shift_capture,
            mouse_shape: g.mouse_shape.into(),
            password_input: g.password_input,
            xt_checksum: g.xt_checksum.into(),
            palette: palette_out(&g.palette),
            fg: g.fg.map(rgb_out),
            bg: g.bg.map(rgb_out),
            cursor_colour: g.cursor_color.map(rgb_out),
        },
    };
    if form == Form::Rows {
        for_older_readers(term, compressed, &mut t);
    }
    t
}

fn palette_out(entries: &[(u8, vt::Rgb)]) -> Vec<snap::term::PaletteEntry> {
    entries
        .iter()
        .map(|&(i, c)| snap::term::PaletteEntry {
            index: i.into(),
            rgb: rgb_out(c),
        })
        .collect()
}

fn palette_in(entries: &[snap::term::PaletteEntry]) -> Result<Vec<(u8, vt::Rgb)>, TermError> {
    entries
        .iter()
        .map(|e| Ok((narrow(e.index, "palette index")?, rgb_in(e.rgb))))
        .collect()
}

/// The modes of a fresh terminal (number, ANSI) with `recorded` over them:
/// a mode the writer didn't list keeps this build's value.
fn modes_in(cols: u16, rows: u16, recorded: &[snap::term::Mode]) -> Vec<ModeState> {
    let mut modes = PaneTerm::new(cols, rows, 0).export_state().ghostty.modes;
    let mut extra = Vec::new();
    for r in recorded {
        let Ok(number) = u16::try_from(r.number) else {
            continue;
        };
        let slot = modes
            .iter_mut()
            .find(|m| m.number == number && m.ansi == r.ansi);
        let m = if let Some(m) = slot {
            m
        } else {
            extra.push(ModeState {
                number,
                ansi: r.ansi,
                ..ModeState::default()
            });
            extra.last_mut().expect("pushed")
        };
        m.current = r.on;
        m.saved = r.saved;
        if let Some(d) = r.default {
            m.default = d;
        }
    }
    modes.extend(extra);
    modes
}

/// A pane that is the terminal `t` was exported from. `ghostty_patches`
/// is the writer's patch stack (`Meta.ghostty_patches`): pages in
/// Ghostty's form are read only if it is this binary's, checked as
/// [`ImportCheck::Lazy`] says (the restore after an upgrade's exec).
///
/// # Errors
/// A record out of range, a page that does not decode, pages from another
/// Ghostty build, or a state Ghostty refuses.
pub fn term_from_snapshot(
    t: &snap::TermState,
    ghostty_patches: &str,
) -> Result<PaneTerm, TermError> {
    term_from_snapshot_with(t, ghostty_patches, ImportCheck::Lazy)
}

/// [`term_from_snapshot`], pages in Ghostty's form checked as `check`
/// says. The server imports only with [`ImportCheck::Lazy`], the
/// upgrade's dry run (live upgrade spec, decision 7) included: it checks
/// each page's checksum and layout and decodes none (`lu_upgrade_perf`).
/// [`ImportCheck::Full`], which also decodes each one, is for the tests
/// and the fuzzer.
///
/// # Errors
/// As [`term_from_snapshot`].
#[expect(clippy::too_many_lines, reason = "one line per field carried")]
pub fn term_from_snapshot_with(
    t: &snap::TermState,
    ghostty_patches: &str,
    check: ImportCheck,
) -> Result<PaneTerm, TermError> {
    let cols = narrow::<u16>(t.cols, "terminal width")?;
    let rows = narrow::<u16>(t.rows, "terminal height")?;
    let primary_size = t
        .sized_primary
        .as_ref()
        .map(|p| {
            Ok::<_, TermError>((
                narrow::<u16>(p.cols, "primary width")?,
                narrow::<u16>(p.rows, "primary height")?,
            ))
        })
        .transpose()?;
    let (primary_cols, primary_rows) = primary_size.unwrap_or((cols, rows));
    let alt = t.alt_entry.clone().unwrap_or_default();
    // The primary screen at its size, when the writer also put it at the
    // terminal's for an older reader (`for_older_readers`).
    let (primary_screen, marks, entry_row) = match &t.sized_primary {
        Some(snap::term::SizedPrimary {
            screen: Some(screen),
            marks,
            entry_row,
            ..
        }) => (screen, marks, *entry_row),
        _ => (&t.primary, &t.marks, alt.entry_row),
    };
    // Every screen shows exactly its visible rows; history is before them.
    if primary_screen.rows.len() != usize::from(primary_rows)
        || t.alternate
            .as_ref()
            .is_some_and(|alt| alt.rows.len() != usize::from(rows))
    {
        return Err(bad("a screen's visible rows are not its height"));
    }
    let h = &t.history;
    let usize_of = |v: u64, what| narrow::<usize>(v, what);
    // `alt.history` is the copy a reader from before Ghostty patch 0038
    // shows (`for_older_readers`); the primary screen has those rows.
    let hsize = usize_of(h.hsize, "history size")?;
    let saved: Option<Vec<FrozenRowState>> = alt
        .has_saved
        .then(|| alt.saved.iter().map(frozen_in).collect());
    let scrolled_rows = usize_of(h.scrolled, "history rows")?;
    let times = times_in(&h.times, scrolled_rows)?;
    // The times end at the newest row. A writer from before `hscrolled`
    // was kept left their start past it after a resize pulled rows back
    // (it kept the start of the rows that went): it starts where they end.
    let times_base = if h.hscrolled.is_some() {
        h.times_base
    } else {
        h.times_base
            .min(h.scrolled.saturating_sub(times.len() as u64))
    };
    let ghostty = GhosttyState {
        scroll_region: (
            narrow(t.scroll_region.top, "scroll region")?,
            narrow(t.scroll_region.bottom, "scroll region")?,
            narrow(t.scroll_region.left, "scroll region")?,
            narrow(t.scroll_region.right, "scroll region")?,
        ),
        status_display: narrow(t.emulator.status_display, "status display")?,
        previous_char: t.emulator.previous_char,
        cursor_is_default: t.emulator.cursor_is_default,
        cursor_default_style: narrow(t.emulator.cursor_default_style, "cursor style")?,
        cursor_default_blink: t.emulator.cursor_default_blink,
        shell_redraw: narrow(t.emulator.shell_redraw, "shell redraw")?,
        modify_other_keys_2: t.emulator.modify_other_keys_2,
        mouse_event: narrow(t.emulator.mouse_event, "mouse event")?,
        mouse_format: narrow(t.emulator.mouse_format, "mouse format")?,
        mouse_shift_capture: t.emulator.mouse_shift_capture,
        mouse_shape: narrow(t.emulator.mouse_shape, "mouse shape")?,
        password_input: t.emulator.password_input,
        xt_checksum: narrow(t.emulator.xt_checksum, "checksum")?,
        modes: modes_in(cols, rows, &t.modes),
        tab_stops: t
            .tab_stops
            .iter()
            .map(|&c| narrow(c, "tab stop"))
            .collect::<Result<_, _>>()?,
        palette: palette_in(&t.emulator.palette)?,
        fg: t.emulator.fg.map(rgb_in),
        bg: t.emulator.bg.map(rgb_in),
        cursor_color: t.emulator.cursor_colour.map(rgb_in),
        title: t.title.clone(),
        pwd: t.pwd.clone(),
        max_bytes: h
            .max_bytes
            .map(|n| usize_of(n, "history budget"))
            .transpose()?,
        max_lines: h
            .line_limit
            .map(|n| usize_of(n, "line limit"))
            .transpose()?,
        alternate_saved: alternate_saved_in(t, primary_screen)?,
    };
    let tc = &t.tmux_charset;
    let cs = &t.cursor_style;
    let pane = PaneState {
        history_limit: usize_of(h.limit, "history limit")?,
        keeps_history: h.enabled,
        hsize,
        hscrolled: h
            .hscrolled
            .map(|n| usize_of(n, "scrolled history rows"))
            .transpose()?
            .unwrap_or(hsize),
        scrolled: h.scrolled,
        generation: h.generation,
        times_base,
        // Times are kept for retained rows only; bound by what history the
        // pane could hold.
        times,
        alt_collected: usize_of(alt.collected, "collected rows")?,
        clear_primary_history: alt.clear_primary_history,
        redraw: t.redraw,
        marks: marks
            .iter()
            .map(|m| Ok((narrow(m.row, "mark row")?, osc133_in(&m.mark)?)))
            .collect::<Result<_, TermError>>()?,
        frozen_marks: alt
            .marks
            .iter()
            .map(|m| Ok((usize_of(m.row, "mark row")?, osc133_in(&m.mark)?)))
            .collect::<Result<_, TermError>>()?,
        saved,
        alt_entry: entry_row
            .map(|y| {
                Ok::<_, TermError>((
                    narrow(y, "alternate entry row")?,
                    usize_of(alt.ghostty_history, "retained rows")?,
                ))
            })
            .transpose()?,
        charset: (
            tc.g0_acs,
            tc.g1_acs,
            tc.shifted,
            (tc.saved_g0_acs, tc.saved_g1_acs, tc.saved_shifted),
        ),
        cursor_style: (
            narrow(cs.shape, "cursor shape")?,
            cs.blinking,
            cs.blinking_set,
            narrow(cs.option, "cursor-style")?,
        ),
        cursor_very_visible: cs.very_visible,
        mouse: vt::MouseModes {
            tracking: match t.mouse.tracking {
                0 => vt::MouseTracking::None,
                1000 => vt::MouseTracking::Standard,
                1002 => vt::MouseTracking::Button,
                1003 => vt::MouseTracking::All,
                _ => return Err(bad("mouse mode")),
            },
            utf8: t.mouse.utf8,
            sgr: t.mouse.sgr,
        },
        key_mode: match t.modify_other_keys {
            0 => vt::KeyMode::Standard,
            1 => vt::KeyMode::Extended1,
            2 => vt::KeyMode::Extended2,
            _ => return Err(bad("key mode")),
        },
        extended_keys: match t.extended_keys {
            0 => vt::ExtendedKeys::Off,
            1 => vt::ExtendedKeys::On,
            2 => vt::ExtendedKeys::Always,
            _ => return Err(bad("extended-keys")),
        },
        last_print: t.input_last,
        palette: vt::PaletteState {
            entries: palette_in(&t.palette)?,
            fg: t.fg.map(rgb_in),
            bg: t.bg.map(rgb_in),
            cursor: t.cursor_colour.map(rgb_in),
        },
        progress_bar: vt::ProgressBar {
            state: u8::try_from(t.progress_bar.state)
                .ok()
                .and_then(vt::ProgressState::from_number)
                .ok_or_else(|| bad("progress bar state"))?,
            progress: narrow(t.progress_bar.progress, "progress bar")?,
        },
        title: t.server_title.clone(),
        title_stack: t.title_stack.clone(),
        continuation: t.pending.clone(),
        input_buffer_size: usize_of(t.input_buffer_size, "input-buffer-size")?,
        scroll_on_clear: t.scroll_on_clear,
        sync: t.sync.as_ref().map(sync_in).transpose()?,
        links: t
            .pen_links
            .as_ref()
            .map(|l| vt::PenLinkState {
                pen: l.pen.clone(),
                saved: l.saved.clone(),
                alternate: l.alternate.clone(),
                emulator: l.emulator.clone(),
            })
            .unwrap_or_default(),
        inband_resize: t.inband_resize,
        graphics: [t.graphics_primary, t.graphics_alternate],
        drawn_links: t
            .drawn_links
            .iter()
            .map(|l| {
                let id = match &l.id {
                    Some(id) => vt::LinkKeyId::Explicit(id.clone()),
                    None => vt::LinkKeyId::Implicit {
                        alternate: l.alternate,
                        n: l.implicit,
                    },
                };
                (String::from_utf8_lossy(&l.uri).into_owned(), id, l.number)
            })
            .collect(),
    };
    let mut primary = screen_in(primary_screen)?;
    if let Some(layout) = &t.alternate_saved_layout
        && layout.moved
    {
        primary.saved_cursor = layout
            .primary_saved
            .as_ref()
            .map(saved_cursor_in)
            .transpose()?;
    }
    let state = TermState {
        cols,
        rows,
        primary_size,
        primary,
        alternate: t.alternate.as_ref().map(screen_in).transpose()?,
        alternate_active: t.active == snap::term::ActiveScreen::Alternate,
        ghostty,
        pane,
    };
    let mut import = StateImport::new(&state, check)?;
    pages_of(primary_screen, primary_cols, ghostty_patches, &mut |page| {
        Ok(import.page(ScreenKind::Primary, page)?)
    })?;
    if let Some(alt) = &t.alternate {
        pages_of(alt, cols, ghostty_patches, &mut |page| {
            Ok(import.page(ScreenKind::Alternate, page)?)
        })?;
    }
    Ok(import.finish()?)
}
