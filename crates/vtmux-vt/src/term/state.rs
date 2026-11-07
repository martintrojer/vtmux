//! A pane's whole terminal state, exported and imported exactly (live
//! upgrade spec, `docs/specs/2026-11-06-live-upgrade.md`, decisions 5, 11
//! and 23).
//!
//! [`PaneTerm::export_state`] copies everything a pane's terminal holds:
//! Ghostty's grids (both screens, every page of history) as rows of cells
//! with the tmux row flags, Ghostty's state outside them (cursor, saved
//! cursor, modes, charsets, tab stops, scroll region, colours), and what
//! [`PaneTerm`] keeps beside Ghostty (tmux's history model, OSC 133
//! marks, the history copied for an alternate screen, title stack,
//! palette, charset, mouse and key modes, the input since the parser left
//! the ground state). [`PaneTerm::import_state`] builds a pane that is
//! then the same terminal: every capture, cursor, mode and row flag reads
//! alike, and the same input changes both alike.
//!
//! The rows and cells are vtmux's own types ([`RowState`], [`CellState`]),
//! filled through Ghostty's page accessors (vendored patch 0017), so they
//! survive a Ghostty pin bump (decision 11). A page Ghostty holds
//! compressed can instead come out as its compressed bytes
//! ([`PageState::Blob`], decision 23): fast, but only a binary built from
//! the same Ghostty patch stack ([`PATCH_STACK`]) reads it back.
//!
//! The same state streams a page at a time: [`PaneTerm::export_state_head`]
//! (everything but the pages) and [`PaneTerm::export_pages`] on the way
//! out, [`StateImport`] on the way in, so a pane's history is never held
//! whole outside Ghostty. `export_state` and `import_state` are those over
//! a whole [`TermState`].
//!
//! An import trusts nothing of the rows and the state around them: every
//! count, index, width, position and bound is checked (`validate.rs`,
//! including every table size Ghostty's parser and pages bound, then
//! Ghostty again) and a malformed state is a [`StateError`], never a crash
//! later. A blob is another matter: it is Ghostty's own page memory, and it
//! is trusted for where it comes from, an upgrade's memory file, private to
//! the user, written by a build of the same [`PATCH_STACK`] (it never goes
//! to disk: decision 23). Its checksum finds a damaged blob, not a forged
//! one. See [`ImportCheck`].

use std::rc::Rc;

use libghostty_vt::ffi;
use libghostty_vt::screen::HyperlinkId;
use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace};
use libghostty_vt::vtmux as gv;

use super::{
    Charset, CursorState, ExtendedKeys, HeldScreen, KeyMode, MouseModes, Osc133, PaneTerm,
    ScanState,
};
use crate::capture::FrozenRow;
use crate::palette::PaletteState;
use crate::snapshot::{Color, Snapshot, Style, Underline};

mod validate;

/// Identifies the Ghostty build a [`PageState::Blob`] can be read by:
/// its commit, local patches, optimize mode and target.
pub const PATCH_STACK: &str = ffi::PATCH_STACK;

/// What a compressed history page becomes in an export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompressedPages {
    /// Rows of cells, like every other page: any build reads them.
    #[default]
    Rows,
    /// The page's compressed bytes, as Ghostty holds them: no decoding,
    /// but only a build with the same [`PATCH_STACK`] reads them.
    Blob,
}

/// A pane's whole terminal state (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermState {
    pub cols: u16,
    pub rows: u16,
    pub primary: ScreenState,
    pub alternate: Option<ScreenState>,
    pub alternate_active: bool,
    pub ghostty: GhosttyState,
    pub pane: PaneState,
}

/// One of Ghostty's screens: every page, oldest first (the last `rows`
/// rows are the visible ones), and the state outside the grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenState {
    pub pages: Vec<PageState>,
    pub cursor: GhosttyCursor,
    pub saved_cursor: Option<SavedCursor>,
    pub charset: GhosttyCharset,
    /// Ghostty's `ProtectedMode` (0 off, 1 ISO, 2 DEC).
    pub protected_mode: u8,
    /// The Kitty keyboard flag stack and its index.
    pub kitty_flags: [u8; 8],
    pub kitty_index: u8,
    /// OSC 133 click handling: kind and value.
    pub semantic_click: (u8, u8),
    /// The next implicit hyperlink id.
    pub hyperlink_counter: u32,
}

/// Ghostty's cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GhosttyCursor {
    pub x: u16,
    pub y: u16,
    /// A wrap is pending: tmux's `cx` past the last column.
    pub pending_wrap: bool,
    /// Ghostty's visual cursor style (bar, block, underline, ...).
    pub visual: u8,
    pub protected: bool,
    pub pen: StyleState,
    /// The OSC 8 link open at the cursor.
    pub link: Option<LinkState>,
    pub semantic_content: u8,
    pub semantic_clear_eol: bool,
}

/// DECSC's saved cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SavedCursor {
    pub x: u16,
    pub y: u16,
    pub pen: StyleState,
    pub protected: bool,
    pub pending_wrap: bool,
    pub origin: bool,
    pub charset: GhosttyCharset,
}

/// Ghostty's charset state: G0-G3, GL, GR and a single shift.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GhosttyCharset {
    /// 0 UTF-8, 1 ASCII, 2 British, 3 DEC special graphics.
    pub g: [u8; 4],
    pub gl: u8,
    pub gr: u8,
    pub single_shift: Option<u8>,
}

/// A cell style: the compositor's [`Style`], and whether a palette colour
/// was set in its 256-colour form for any index (Ghostty patch 0010 keeps
/// the form for every index, [`Style`] only for 0-15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StyleState {
    pub style: Style,
    pub fg_256: bool,
    pub bg_256: bool,
}

/// An OSC 8 hyperlink: its URI and id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LinkState {
    pub uri: Vec<u8>,
    pub id: LinkId,
}

/// A hyperlink's OSC 8 id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LinkId {
    /// `id=` given by the program.
    Explicit(Vec<u8>),
    /// No `id=`: Ghostty's counter when the link opened.
    Implicit(u32),
}

/// One Ghostty page: rows of one width, or a compressed page's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageState {
    Rows(PageRows),
    Blob(PageBlob),
}

impl PageState {
    /// Rows in the page; a blob's are only known to Ghostty.
    #[must_use]
    pub fn rows(&self) -> Option<usize> {
        match self {
            Self::Rows(p) => Some(p.rows.len()),
            Self::Blob(_) => None,
        }
    }
}

/// A compressed page as Ghostty holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageBlob {
    /// [`PATCH_STACK`] of the build that wrote it.
    pub stack: String,
    pub bytes: Vec<u8>,
}

/// Rows of one width, in vtmux's row/cell format.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageRows {
    pub cols: u16,
    pub rows: Vec<RowState>,
    /// Every row's cells, a row's trailing default cells left out.
    pub cells: Vec<CellState>,
    /// For each cell of more than one codepoint, in cell order: its index
    /// in `cells`, the count of codepoints after the first, and those.
    pub graphemes: Vec<u32>,
    /// The styles cells index; entry 0 is the default.
    pub styles: Vec<StyleState>,
    /// The hyperlinks cells index; entry 0 is none.
    pub links: Vec<Option<LinkState>>,
    /// Ghostty's allocation sizes for the page, a hint for the import.
    pub capacity: PageCapacity,
}

/// A page's allocation sizes (Ghostty's `Capacity`), zero when unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageCapacity {
    pub rows: u32,
    pub styles: u32,
    pub hyperlink_bytes: u32,
    pub grapheme_bytes: u32,
    pub string_bytes: u32,
}

/// One row: its cells are `cells[cell_start..cell_start + cell_count]`
/// of the page, the rest of its columns default cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowState {
    pub cell_start: u32,
    pub cell_count: u16,
    /// tmux's `cellused` (Ghostty patch 0013).
    pub used: u16,
    /// [`RowState::WRAPPED`], [`RowState::CONTINUATION`], and in bits 2-3
    /// Ghostty's semantic prompt mark.
    pub flags: u8,
    pub reserved: [u8; 3],
}

impl RowState {
    /// tmux's `GRID_LINE_WRAPPED`.
    pub const WRAPPED: u8 = 0x01;
    /// The row continues a wrapped one.
    pub const CONTINUATION: u8 = 0x02;

    #[must_use]
    pub fn wrapped(self) -> bool {
        self.flags & Self::WRAPPED != 0
    }
}

/// One cell. `flags`: bits 0-1 the content kind (0 a
/// codepoint, 1 a codepoint with more in the page's graphemes, 2 a
/// palette background and no text, 3 an RGB background and no text),
/// bits 2-3 the width (0 narrow, 1 wide, 2 spacer tail, 3 spacer head),
/// bit 4 protected (tmux's ACS, see `sync_charset`), bit 5 the palette
/// background's 256-colour form, bits 6-7 the semantic content.
/// `content`: the codepoint, palette index or `0xRRGGBB`. `style` and
/// `link` index the page's tables. `tab`: Ghostty's tab mark (patch 0002).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellState {
    pub content: u32,
    pub style: u16,
    pub link: u16,
    pub flags: u16,
    pub tab: u8,
    pub reserved: u8,
}

/// Ghostty's terminal state outside its screens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhosttyState {
    /// DECSTBM and DECSLRM: top, bottom, left, right.
    pub scroll_region: (u16, u16, u16, u16),
    pub status_display: u8,
    /// The last printed character, for REP.
    pub previous_char: Option<u32>,
    pub cursor_is_default: bool,
    pub cursor_default_style: u8,
    pub cursor_default_blink: Option<bool>,
    pub shell_redraw: u8,
    pub modify_other_keys_2: bool,
    pub mouse_event: u8,
    pub mouse_format: u8,
    pub mouse_shift_capture: Option<bool>,
    pub mouse_shape: u8,
    pub password_input: bool,
    pub xt_checksum: u8,
    /// Every mode Ghostty knows.
    pub modes: Vec<ModeState>,
    /// The columns with a tab stop.
    pub tab_stops: Vec<u16>,
    /// Palette entries changed (OSC 4), and the OSC 10/11/12 colours.
    pub palette: Vec<(u8, (u8, u8, u8))>,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub cursor_color: Option<(u8, u8, u8)>,
    pub title: String,
    pub pwd: String,
    /// Ghostty's scrollback limits.
    pub max_bytes: Option<usize>,
    pub max_lines: Option<usize>,
}

/// One mode: its number, ANSI or DEC, and its current, XTSAVE'd and reset
/// values.
#[allow(clippy::struct_excessive_bools)] // one per value Ghostty keeps
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeState {
    pub number: u16,
    pub ansi: bool,
    pub current: bool,
    pub saved: bool,
    pub default: bool,
}

/// What [`PaneTerm`] keeps beside Ghostty.
#[allow(clippy::struct_excessive_bools)] // independent pane flags
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneState {
    pub history_limit: usize,
    /// tmux's `GRID_HISTORY` (see `PaneTerm::history`).
    pub keeps_history: bool,
    pub hsize: usize,
    pub scrolled: u64,
    pub generation: u64,
    pub times_base: u64,
    pub times: Vec<i64>,
    pub alt_collected: usize,
    pub clear_primary_history: bool,
    pub redraw: bool,
    /// OSC 133 marks: each line's row in the primary screen (Ghostty's
    /// screen coordinates) and what was recorded.
    pub marks: Vec<(u32, Osc133)>,
    /// The primary history and screen copied when the alternate screen
    /// came up, and the history's marks.
    pub frozen: Option<Vec<FrozenRowState>>,
    pub frozen_marks: Vec<(usize, Osc133)>,
    pub saved: Option<Vec<FrozenRowState>>,
    /// The primary row tmux's history started at when the alternate screen
    /// came up, and the history rows retained then.
    pub alt_entry: Option<(u32, usize)>,
    /// tmux's charset: G0 and G1 ACS, shifted, and the saved three.
    pub charset: (bool, bool, bool, (bool, bool, bool)),
    /// tmux's cursor style: shape, blinking, blinking set, option.
    pub cursor_style: (u8, bool, bool, u8),
    pub mouse: MouseModes,
    pub key_mode: KeyMode,
    pub extended_keys: ExtendedKeys,
    /// tmux's `INPUT_LAST`: the last thing parsed was a printed character,
    /// so a REP repeats it (input.c `INPUT_CSI_REP`).
    pub last_print: bool,
    pub palette: PaletteState,
    /// The server's title over the program's, and tmux's title stack.
    pub title: Option<String>,
    pub title_stack: Vec<String>,
    /// The input since the parser left the ground state (and a UTF-8
    /// character begun, a window name being read): fed again on import,
    /// it puts both parsers where they were.
    pub continuation: Vec<u8>,
    pub input_buffer_size: usize,
    pub scroll_on_clear: bool,
    /// Synchronized output held: the screen held and the time left.
    pub sync: Option<(Snapshot, std::time::Duration)>,
}

/// A row of history copied out of Ghostty (`FrozenRow`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenRowState {
    pub cells: Vec<FrozenCell>,
    pub len: usize,
    pub wrapped: bool,
}

/// A cell of a [`FrozenRowState`], as `capture.rs` reads cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenCell {
    pub text: String,
    pub style: Style,
    pub empty: bool,
    /// 0 narrow, 1 wide, 2 spacer tail, 3 spacer head.
    pub wide: u8,
    pub acs: bool,
    pub link: bool,
    /// The link's URI and id, when they read.
    pub target: Option<(Option<String>, Option<FrozenLinkId>)>,
    pub tab: u8,
}

/// A frozen cell's link id, as Ghostty reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrozenLinkId {
    None,
    Explicit(Vec<u8>),
    Implicit(u32),
}

impl From<&HyperlinkId> for FrozenLinkId {
    fn from(id: &HyperlinkId) -> Self {
        match id {
            HyperlinkId::None => Self::None,
            HyperlinkId::Explicit(b) => Self::Explicit(b.clone()),
            HyperlinkId::Implicit(n) => Self::Implicit(*n),
        }
    }
}

impl From<&FrozenLinkId> for HyperlinkId {
    fn from(id: &FrozenLinkId) -> Self {
        match id {
            FrozenLinkId::None => Self::None,
            FrozenLinkId::Explicit(b) => Self::Explicit(b.clone()),
            FrozenLinkId::Implicit(n) => Self::Implicit(*n),
        }
    }
}

/// Why an import failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    /// A blob from a build of another Ghostty patch stack.
    ForeignBlob,
    /// The state is malformed: what is wrong with it.
    Invalid(&'static str),
    /// Ghostty refused the state (a blob that fails its checksum or does
    /// not decode, or out of memory).
    Ghostty(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForeignBlob => f.write_str("compressed page from another Ghostty build"),
            Self::Invalid(what) => write!(f, "malformed terminal state: {what}"),
            Self::Ghostty(e) => write!(f, "terminal state rejected: {e}"),
        }
    }
}

impl std::error::Error for StateError {}

fn ghostty_err(e: libghostty_vt::Error) -> StateError {
    StateError::Ghostty(e.to_string())
}

// ---------------------------------------------------------------------
// Conversions between Ghostty's C records and the state types.

fn color_from(c: ffi::StyleColor) -> Color {
    // SAFETY: the tag says which member of the union is set.
    unsafe {
        match c.tag {
            ffi::StyleColorTag::PALETTE => Color::Palette(c.value.palette),
            ffi::StyleColorTag::RGB => Color::Rgb(c.value.rgb.r, c.value.rgb.g, c.value.rgb.b),
            _ => Color::Default,
        }
    }
}

fn color_to(c: Color) -> ffi::StyleColor {
    let mut out = ffi::StyleColor {
        tag: ffi::StyleColorTag::NONE,
        value: ffi::StyleColorValue { _padding: 0 },
    };
    match c {
        Color::Default => {}
        Color::Palette(n) | Color::Palette256(n) => {
            out.tag = ffi::StyleColorTag::PALETTE;
            out.value = ffi::StyleColorValue { palette: n };
        }
        Color::Rgb(r, g, b) => {
            out.tag = ffi::StyleColorTag::RGB;
            out.value = ffi::StyleColorValue {
                rgb: ffi::ColorRgb { r, g, b },
            };
        }
    }
    out
}

fn underline_from(n: i32) -> Underline {
    match n {
        1 => Underline::Single,
        2 => Underline::Double,
        3 => Underline::Curly,
        4 => Underline::Dotted,
        5 => Underline::Dashed,
        _ => Underline::None,
    }
}

fn underline_to(u: Underline) -> i32 {
    match u {
        Underline::None => 0,
        Underline::Single => 1,
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
    }
}

impl StyleState {
    fn from_ffi(s: &ffi::Style) -> Self {
        Self {
            style: Style {
                fg: color_from(s.fg_color).in_form(s.fg_palette_256),
                bg: color_from(s.bg_color).in_form(s.bg_palette_256),
                underline_color: color_from(s.underline_color),
                bold: s.bold,
                faint: s.faint,
                italic: s.italic,
                blink: s.blink,
                inverse: s.inverse,
                invisible: s.invisible,
                strikethrough: s.strikethrough,
                overline: s.overline,
                underline: underline_from(s.underline),
            },
            fg_256: s.fg_palette_256,
            bg_256: s.bg_palette_256,
        }
    }

    fn to_ffi(self) -> ffi::Style {
        let s = self.style;
        ffi::Style {
            size: std::mem::size_of::<ffi::Style>(),
            fg_color: color_to(s.fg),
            bg_color: color_to(s.bg),
            underline_color: color_to(s.underline_color),
            bold: s.bold,
            italic: s.italic,
            faint: s.faint,
            blink: s.blink,
            inverse: s.inverse,
            invisible: s.invisible,
            strikethrough: s.strikethrough,
            overline: s.overline,
            underline: underline_to(s.underline),
            fg_palette_256: self.fg_256,
            bg_palette_256: self.bg_256,
        }
    }
}

impl GhosttyCharset {
    fn from_ffi(c: gv::Charset) -> Self {
        Self {
            g: c.g,
            gl: c.gl,
            gr: c.gr,
            single_shift: (c.single_shift != 0xff).then_some(c.single_shift),
        }
    }

    fn to_ffi(self) -> gv::Charset {
        gv::Charset {
            g: self.g,
            gl: self.gl,
            gr: self.gr,
            single_shift: self.single_shift.unwrap_or(0xff),
            reserved: 0,
        }
    }
}

fn link_from(bytes: &[u8], uri: (u32, u32), id: Option<(u32, u32)>, implicit: u32) -> LinkState {
    let slice = |(o, l): (u32, u32)| {
        bytes
            .get(o as usize..(o as usize).saturating_add(l as usize))
            .unwrap_or_default()
            .to_vec()
    };
    LinkState {
        uri: slice(uri),
        id: id.map_or(LinkId::Implicit(implicit), |id| LinkId::Explicit(slice(id))),
    }
}

/// Append a link's strings to `bytes`: its C record.
fn link_to(link: &LinkState, bytes: &mut Vec<u8>) -> gv::Link {
    let mut out = gv::Link {
        uri_offset: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
        uri_len: u32::try_from(link.uri.len()).unwrap_or(u32::MAX),
        ..gv::Link::default()
    };
    bytes.extend_from_slice(&link.uri);
    match &link.id {
        LinkId::Implicit(n) => out.implicit_id = *n,
        LinkId::Explicit(id) => {
            out.explicit_id = 1;
            out.id_offset = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            out.id_len = u32::try_from(id.len()).unwrap_or(u32::MAX);
            bytes.extend_from_slice(id);
        }
    }
    out
}

impl PageRows {
    fn from_ffi(p: &gv::Page) -> Self {
        let mut page = Self {
            cols: p.cols,
            rows: p
                .rows
                .iter()
                .map(|r| RowState {
                    cell_start: r.cell_start,
                    cell_count: r.cell_count,
                    used: r.used,
                    flags: r.flags,
                    reserved: [0; 3],
                })
                .collect(),
            cells: p
                .cells
                .iter()
                .map(|c| CellState {
                    content: c.content,
                    style: c.style,
                    link: c.link,
                    flags: c.flags,
                    tab: c.tab,
                    reserved: 0,
                })
                .collect(),
            graphemes: p.graphemes.clone(),
            styles: p.styles.iter().map(StyleState::from_ffi).collect(),
            links: p
                .links
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    (i > 0).then(|| {
                        link_from(
                            &p.bytes,
                            (l.uri_offset, l.uri_len),
                            (l.explicit_id != 0).then_some((l.id_offset, l.id_len)),
                            l.implicit_id,
                        )
                    })
                })
                .collect(),
            capacity: PageCapacity {
                rows: p.capacity.rows,
                styles: p.capacity.styles,
                hyperlink_bytes: p.capacity.hyperlink_bytes,
                grapheme_bytes: p.capacity.grapheme_bytes,
                string_bytes: p.capacity.string_bytes,
            },
        };
        // A live page's capacity outgrows what it holds once that is
        // erased; the hint goes out within what an import takes.
        let most = validate::capacity_bound(&page);
        let cap = &mut page.capacity;
        cap.styles = cap.styles.min(most.styles);
        cap.grapheme_bytes = cap.grapheme_bytes.min(most.grapheme_bytes);
        cap.string_bytes = cap.string_bytes.min(most.string_bytes);
        page
    }

    /// The C records for an import, into `scratch`.
    fn to_ffi(&self, scratch: &mut gv::Page) {
        scratch.rows.clear();
        scratch.rows.extend(self.rows.iter().map(|r| gv::Row {
            cell_start: r.cell_start,
            cell_count: r.cell_count,
            used: r.used,
            flags: r.flags,
            reserved: [0; 3],
        }));
        scratch.cells.clear();
        scratch.cells.extend(self.cells.iter().map(|c| gv::Cell {
            content: c.content,
            style: c.style,
            link: c.link,
            flags: c.flags,
            tab: c.tab,
            reserved: 0,
        }));
        scratch.graphemes.clone_from(&self.graphemes);
        scratch.styles.clear();
        scratch
            .styles
            .extend(self.styles.iter().map(|s| s.to_ffi()));
        scratch.links.clear();
        scratch.bytes.clear();
        for l in &self.links {
            let raw = l
                .as_ref()
                .map_or_else(gv::Link::default, |l| link_to(l, &mut scratch.bytes));
            scratch.links.push(raw);
        }
        scratch.cols = self.cols;
        scratch.blob = false;
        scratch.capacity = gv::Capacity {
            rows: self.capacity.rows,
            styles: self.capacity.styles,
            hyperlink_bytes: self.capacity.hyperlink_bytes,
            grapheme_bytes: self.capacity.grapheme_bytes,
            string_bytes: self.capacity.string_bytes,
        };
    }
}

impl ScreenState {
    fn from_ffi(s: &gv::ScreenState, bytes: &[u8], pages: Vec<PageState>) -> Self {
        let link = match s.link_kind {
            1 => Some(link_from(
                bytes,
                (s.link_uri_offset, s.link_uri_len),
                None,
                s.link_implicit_id,
            )),
            2 => Some(link_from(
                bytes,
                (s.link_uri_offset, s.link_uri_len),
                Some((s.link_id_offset, s.link_id_len)),
                0,
            )),
            _ => None,
        };
        Self {
            pages,
            cursor: GhosttyCursor {
                x: s.cursor_x,
                y: s.cursor_y,
                pending_wrap: s.pending_wrap,
                visual: s.cursor_style,
                protected: s.protected_,
                pen: StyleState::from_ffi(&s.pen),
                link,
                semantic_content: s.semantic_content,
                semantic_clear_eol: s.semantic_content_clear_eol,
            },
            saved_cursor: s.saved_present.then(|| SavedCursor {
                x: s.saved_x,
                y: s.saved_y,
                pen: StyleState::from_ffi(&s.saved_pen),
                protected: s.saved_protected,
                pending_wrap: s.saved_pending_wrap,
                origin: s.saved_origin,
                charset: GhosttyCharset::from_ffi(s.saved_charset),
            }),
            charset: GhosttyCharset::from_ffi(s.charset),
            protected_mode: s.protected_mode,
            kitty_flags: s.kitty_flags,
            kitty_index: s.kitty_index,
            semantic_click: (s.semantic_click_kind, s.semantic_click_value),
            hyperlink_counter: s.hyperlink_implicit_counter,
        }
    }

    fn to_ffi(&self, out: &mut gv::ScreenState, bytes: &mut Vec<u8>) {
        let c = &self.cursor;
        out.present = true;
        out.cursor_x = c.x;
        out.cursor_y = c.y;
        out.cursor_style = c.visual;
        out.pending_wrap = c.pending_wrap;
        out.protected_ = c.protected;
        out.semantic_content = c.semantic_content;
        out.semantic_content_clear_eol = c.semantic_clear_eol;
        out.pen = c.pen.to_ffi();
        out.link_kind = 0;
        if let Some(link) = &c.link {
            let raw = link_to(link, bytes);
            out.link_kind = if raw.explicit_id != 0 { 2 } else { 1 };
            out.link_implicit_id = raw.implicit_id;
            out.link_uri_offset = raw.uri_offset;
            out.link_uri_len = raw.uri_len;
            out.link_id_offset = raw.id_offset;
            out.link_id_len = raw.id_len;
        }
        out.hyperlink_implicit_counter = self.hyperlink_counter;
        out.charset = self.charset.to_ffi();
        out.protected_mode = self.protected_mode;
        out.kitty_flags = self.kitty_flags;
        out.kitty_index = self.kitty_index;
        (out.semantic_click_kind, out.semantic_click_value) = self.semantic_click;
        out.saved_present = self.saved_cursor.is_some();
        out.saved_pen = StyleState::default().to_ffi();
        if let Some(s) = &self.saved_cursor {
            out.saved_x = s.x;
            out.saved_y = s.y;
            out.saved_pen = s.pen.to_ffi();
            out.saved_protected = s.protected;
            out.saved_pending_wrap = s.pending_wrap;
            out.saved_origin = s.origin;
            out.saved_charset = s.charset.to_ffi();
        }
    }
}

fn rgb_opt(set: bool, c: [u8; 3]) -> Option<(u8, u8, u8)> {
    set.then_some((c[0], c[1], c[2]))
}

fn rgb_bytes(c: Option<(u8, u8, u8)>) -> (bool, [u8; 3]) {
    c.map_or((false, [0; 3]), |(r, g, b)| (true, [r, g, b]))
}

impl GhosttyState {
    fn from_ffi(s: &gv::RawState) -> Self {
        let modes = s.modes[..s.modes_len.min(s.modes.len())]
            .iter()
            .map(|m| ModeState {
                number: m.value,
                ansi: m.ansi,
                current: m.current,
                saved: m.saved,
                default: m.default_,
            })
            .collect();
        let tab_stops = (0..s.cols)
            .filter(|&c| {
                s.tabstops
                    .get(usize::from(c / 8))
                    .is_some_and(|b| b & (1 << (c % 8)) != 0)
            })
            .collect();
        let palette = (0..=255u8)
            .filter(|&i| s.palette_mask[usize::from(i / 8)] & (1 << (i % 8)) != 0)
            .map(|i| {
                let at = usize::from(i) * 3;
                (i, (s.palette[at], s.palette[at + 1], s.palette[at + 2]))
            })
            .collect();
        Self {
            scroll_region: (s.scroll_top, s.scroll_bottom, s.scroll_left, s.scroll_right),
            status_display: s.status_display,
            previous_char: (s.previous_char != u32::MAX).then_some(s.previous_char),
            cursor_is_default: s.cursor_is_default,
            cursor_default_style: s.cursor_default_style,
            cursor_default_blink: match s.cursor_default_blink {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            },
            shell_redraw: s.shell_redraw,
            modify_other_keys_2: s.modify_other_keys_2,
            mouse_event: s.mouse_event,
            mouse_format: s.mouse_format,
            mouse_shift_capture: match s.mouse_shift_capture {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            },
            mouse_shape: s.mouse_shape,
            password_input: s.password_input,
            xt_checksum: s.xt_checksum,
            modes,
            tab_stops,
            palette,
            fg: rgb_opt(s.fg_set, s.fg),
            bg: rgb_opt(s.bg_set, s.bg),
            cursor_color: rgb_opt(s.cursor_color_set, s.cursor_color),
            title: String::new(),
            pwd: String::new(),
            max_bytes: None,
            max_lines: None,
        }
    }

    fn to_ffi(&self, out: &mut gv::RawState) {
        (
            out.scroll_top,
            out.scroll_bottom,
            out.scroll_left,
            out.scroll_right,
        ) = self.scroll_region;
        out.status_display = self.status_display;
        out.previous_char = self.previous_char.unwrap_or(u32::MAX);
        out.cursor_is_default = self.cursor_is_default;
        out.cursor_default_style = self.cursor_default_style;
        out.cursor_default_blink = match self.cursor_default_blink {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        out.shell_redraw = self.shell_redraw;
        out.modify_other_keys_2 = self.modify_other_keys_2;
        out.mouse_event = self.mouse_event;
        out.mouse_format = self.mouse_format;
        out.mouse_shift_capture = match self.mouse_shift_capture {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        out.mouse_shape = self.mouse_shape;
        out.password_input = self.password_input;
        out.xt_checksum = self.xt_checksum;
        out.modes_len = self.modes.len().min(out.modes.len());
        for (slot, m) in out.modes.iter_mut().zip(&self.modes) {
            *slot = gv::Mode {
                value: m.number,
                ansi: m.ansi,
                current: m.current,
                saved: m.saved,
                default_: m.default,
            };
        }
        out.tabstops.fill(0);
        for &c in &self.tab_stops {
            if let Some(b) = out.tabstops.get_mut(usize::from(c / 8)) {
                *b |= 1 << (c % 8);
            }
        }
        out.palette_mask.fill(0);
        for &(i, (r, g, b)) in &self.palette {
            out.palette_mask[usize::from(i / 8)] |= 1 << (i % 8);
            let at = usize::from(i) * 3;
            out.palette[at..at + 3].copy_from_slice(&[r, g, b]);
        }
        (out.fg_set, out.fg) = rgb_bytes(self.fg);
        (out.bg_set, out.bg) = rgb_bytes(self.bg);
        (out.cursor_color_set, out.cursor_color) = rgb_bytes(self.cursor_color);
    }
}

// ---------------------------------------------------------------------
// PaneTerm

/// One of a pane's two screens, for pages exported or imported one at a
/// time ([`PaneTerm::export_pages`], [`StateImport::page`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenKind {
    Primary,
    Alternate,
}

impl ScreenKind {
    fn key(self) -> u8 {
        match self {
            Self::Primary => gv::PRIMARY,
            Self::Alternate => gv::ALTERNATE,
        }
    }
}

fn page_state(buf: &gv::Page) -> PageState {
    if buf.blob {
        PageState::Blob(PageBlob {
            stack: PATCH_STACK.to_owned(),
            bytes: buf.bytes.clone(),
        })
    } else {
        PageState::Rows(PageRows::from_ffi(buf))
    }
}

impl PaneTerm {
    /// The pane's whole terminal state, every compressed page as rows (see
    /// [`TermState`]). Reads history without restoring compressed pages.
    /// Holds every page at once: [`PaneTerm::export_pages`] streams them.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    #[must_use]
    pub fn export_state(&self) -> TermState {
        self.export_state_with(CompressedPages::Rows)
    }

    /// [`PaneTerm::export_state`], with compressed pages as `compressed`
    /// says.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    #[must_use]
    pub fn export_state_with(&self, compressed: CompressedPages) -> TermState {
        let mut state = self.export_state_head();
        let done: Result<(), std::convert::Infallible> =
            self.export_pages(compressed, |screen, page| {
                match (screen, state.alternate.as_mut()) {
                    (ScreenKind::Alternate, Some(alt)) => alt.pages.push(page),
                    _ => state.primary.pages.push(page),
                }
                Ok(())
            });
        let Ok(()) = done;
        state
    }

    /// The pane's terminal state but its pages: every screen's `pages` is
    /// empty, `alternate` is there when the pane has an alternate screen.
    /// With [`PaneTerm::export_pages`] it is the whole state, a page at a
    /// time; [`StateImport`] reads it back the same way.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    #[must_use]
    pub fn export_state_head(&self) -> TermState {
        let term = &self.term;
        let raw = term.vtmux_state_export().expect("terminal state export");
        let has_alternate = term
            .vtmux_page_count(gv::ALTERNATE)
            .expect("page count")
            .is_some();
        let mut ghostty = GhosttyState::from_ffi(&raw.raw);
        term.title()
            .unwrap_or_default()
            .clone_into(&mut ghostty.title);
        term.pwd().unwrap_or_default().clone_into(&mut ghostty.pwd);
        ghostty.max_bytes = term.scrollback_max_bytes().ok().flatten();
        ghostty.max_lines = self.line_limit;
        TermState {
            cols: raw.raw.cols,
            rows: raw.raw.rows,
            primary: ScreenState::from_ffi(&raw.raw.screens[0], &raw.bytes, Vec::new()),
            alternate: has_alternate
                .then(|| ScreenState::from_ffi(&raw.raw.screens[1], &raw.bytes, Vec::new())),
            alternate_active: raw.raw.active_screen == 1,
            ghostty,
            pane: self.pane_state(),
        }
    }

    /// Every page of the pane's screens, the primary's oldest first, then
    /// the alternate's: each is built, handed to `each`, and dropped, so
    /// only one page is held at a time (a 100k-line history is never in
    /// memory twice). The pages of [`PaneTerm::export_state_with`], in its
    /// order. Stops at the first error `each` returns.
    ///
    /// # Errors
    /// What `each` returns.
    ///
    /// # Panics
    /// Only if libghostty fails to allocate.
    pub fn export_pages<E>(
        &self,
        compressed: CompressedPages,
        mut each: impl FnMut(ScreenKind, PageState) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut buf = gv::Page::default();
        for screen in [ScreenKind::Primary, ScreenKind::Alternate] {
            let Some(n) = self
                .term
                .vtmux_page_count(screen.key())
                .expect("page count")
            else {
                continue;
            };
            for i in 0..n {
                self.term
                    .vtmux_page_export(
                        screen.key(),
                        i,
                        compressed == CompressedPages::Blob,
                        &mut buf,
                    )
                    .expect("page export");
                each(screen, page_state(&buf))?;
            }
        }
        Ok(())
    }
    fn pane_state(&self) -> PaneState {
        let screen_y = |pin: &libghostty_vt::screen::TrackedGridRef| {
            pin.point(PointSpace::Screen).ok().flatten().map(|p| p.y)
        };
        let c = self.scan.charset;
        let cs = self.cursor.get();
        let titles = self.titles.borrow();
        PaneState {
            history_limit: self.history_lines,
            keeps_history: self.history,
            hsize: self.hsize,
            scrolled: self.scrolled,
            generation: self.generation,
            times_base: self.times_base,
            times: self.times.iter().copied().collect(),
            alt_collected: self.alt_collected,
            clear_primary_history: self.clear_primary_history,
            redraw: self.redraw,
            marks: self
                .marks
                .iter()
                .filter_map(|(pin, data)| Some((screen_y(pin)?, *data)))
                .collect(),
            frozen: self
                .frozen
                .as_ref()
                .map(|rows| rows.iter().map(FrozenRow::to_state).collect()),
            frozen_marks: self.frozen_marks.clone(),
            saved: self
                .saved
                .as_ref()
                .map(|rows| rows.iter().map(FrozenRow::to_state).collect()),
            alt_entry: self
                .alt_entry
                .as_ref()
                .and_then(|(pin, retained)| Some((screen_y(pin)?, *retained))),
            charset: (c.g0, c.g1, c.shifted, c.saved),
            cursor_style: (cs.shape, cs.blinking, cs.blinking_set, cs.option),
            mouse: self.mouse.get(),
            key_mode: self.scan.key_mode,
            extended_keys: self.scan.extended_keys,
            last_print: self.scan.last_print,
            palette: self.scan.palette.state(),
            title: titles.server.clone(),
            title_stack: titles.stack.clone(),
            continuation: self.continuation(),
            input_buffer_size: self.input_buffer_size,
            scroll_on_clear: self.scroll_on_clear,
            sync: self.sync.hold.as_ref().map(|(held, since)| {
                let left = (*since + super::SYNC_TIMEOUT)
                    .saturating_duration_since(std::time::Instant::now());
                ((*held.screen).clone(), left)
            }),
        }
    }

    /// The bytes that put a fresh parser where this one is.
    fn continuation(&self) -> Vec<u8> {
        let mut out = self.pending.clone();
        if self.scan.state == ScanState::Rename {
            out.extend_from_slice(&self.rename.buf);
        } else if self.scan.state == ScanState::Ground && self.scan.utf8.started() {
            out.extend_from_slice(&self.scan.utf8.buf[..self.scan.utf8.have]);
        }
        out
    }

    /// A pane that is the terminal `state` was exported from (see
    /// [`TermState`]). Blobs are checked as [`ImportCheck::Lazy`] says.
    ///
    /// # Errors
    /// A blob from a build of another Ghostty patch stack, a malformed
    /// state, or one Ghostty refuses.
    pub fn import_state(state: &TermState) -> Result<Self, StateError> {
        Self::import_state_with(state, ImportCheck::Lazy)
    }

    /// [`PaneTerm::import_state`], checking blobs as `check` says.
    ///
    /// # Errors
    /// As [`PaneTerm::import_state`].
    pub fn import_state_with(state: &TermState, check: ImportCheck) -> Result<Self, StateError> {
        let mut import = StateImport::new(state, check)?;
        for page in &state.primary.pages {
            import.page(ScreenKind::Primary, page)?;
        }
        for page in state.alternate.iter().flat_map(|alt| &alt.pages) {
            import.page(ScreenKind::Alternate, page)?;
        }
        import.finish()
    }
    /// The pane's own part of an import (see [`PaneState`]).
    fn restore_model(&mut self, p: &PaneState, cols: u16) {
        self.hsize = p.hsize;
        self.scrolled = p.scrolled;
        self.generation = p.generation;
        self.times_base = p.times_base;
        self.times = p.times.iter().copied().collect();
        self.alt_collected = p.alt_collected;
        self.clear_primary_history = p.clear_primary_history;
        self.redraw = p.redraw;
        self.frozen = p
            .frozen
            .as_ref()
            .map(|rows| Rc::new(rows.iter().map(FrozenRow::from_state).collect()));
        self.frozen_marks.clone_from(&p.frozen_marks);
        self.saved = p
            .saved
            .as_ref()
            .map(|rows| Rc::new(rows.iter().map(FrozenRow::from_state).collect()));
        let (g0, g1, shifted, saved) = p.charset;
        self.scan.charset = Charset {
            g0,
            g1,
            shifted,
            saved,
        };
        let (shape, blinking, blinking_set, option) = p.cursor_style;
        self.cursor.set(CursorState {
            shape,
            blinking,
            blinking_set,
            option,
        });
        self.mouse.set(p.mouse);
        self.scan.key_mode = p.key_mode;
        self.scan.extended_keys = p.extended_keys;
        // Before the continuation is fed: its bytes clear it again as they
        // did, and set it never (it holds no printed character).
        self.scan.last_print = p.last_print;
        self.scan.palette.restore(&p.palette);
        {
            let mut titles = self.titles.borrow_mut();
            titles.server.clone_from(&p.title);
            titles.stack.clone_from(&p.title_stack);
            titles.events.clear();
        }
        self.set_input_buffer_size(p.input_buffer_size);
        self.scroll_on_clear = p.scroll_on_clear;
        self.page_rows = super::page_rows(cols);
        if let Some((screen, left)) = &p.sync {
            let since = std::time::Instant::now()
                .checked_sub(super::SYNC_TIMEOUT.saturating_sub(*left))
                .unwrap_or_else(std::time::Instant::now);
            let rows = usize::from(screen.rows);
            let stamps: Rc<[u64]> = (0..rows).map(|_| crate::snapshot::fresh_stamp()).collect();
            self.sync.hold = Some((
                HeldScreen {
                    screen: Rc::new(screen.clone()),
                    stamps,
                },
                since,
            ));
        }
    }
}

/// How much an import checks of the compressed pages (blobs) it is given.
/// Rows are always checked whole, whatever this says.
///
/// Neither defends against a forged blob (bytes changed and the checksum
/// computed again): a blob is trusted because only an upgrade's memory
/// file carries one, the same user's, from a build of the same
/// [`PATCH_STACK`]. Ghostty's integrity check reads a page through the
/// page's own offsets, so a forged page can crash it, also under `Full`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImportCheck {
    /// A blob's checksum (over its `Page` value and LZ4 bytes) and its
    /// `Page` value against its capacity's layout are checked; its LZ4
    /// bytes are decoded on the page's first read.
    #[default]
    Lazy,
    /// As `Lazy`, and every blob is also decoded and every page's
    /// integrity verified (`Page.verifyIntegrity`) as it is added. For a
    /// blob that passed its checksum, so is what the writer exported, this
    /// finds LZ4 bytes that do not decode and a page the writer's Ghostty
    /// got wrong. The upgrade's dry run does not use it (it decoded every
    /// page of history once more, the cost `lu_upgrade_perf` removed); the
    /// tests and the fuzzer do.
    Full,
}

/// A pane's terminal state imported a page at a time: the state but its
/// pages ([`PaneTerm::export_state_head`]), then each page in the order
/// [`PaneTerm::export_pages`] gives them, then [`StateImport::finish`].
/// Only the page being added is held outside Ghostty, so a 100k-line
/// history is never in memory twice. [`PaneTerm::import_state`] is this
/// over a whole [`TermState`].
///
/// Every part is checked before Ghostty sees it; any malformed part is an
/// error, from the call that is given it or from `finish`.
pub struct StateImport {
    importer: gv::Importer<'static>,
    pane: PaneTerm,
    /// Ghostty's state outside the grids, applied at `finish`.
    raw: gv::State,
    title: String,
    pwd: String,
    marks: Vec<(u32, Osc133)>,
    alt_entry: Option<(u32, usize)>,
    alternate: bool,
    alternate_active: bool,
    continuation: Vec<u8>,
    scratch: gv::Page,
}

impl std::fmt::Debug for StateImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateImport").finish_non_exhaustive()
    }
}

impl StateImport {
    /// Start importing `head`: everything of it but its pages, which are
    /// not read (add them with [`StateImport::page`]).
    ///
    /// # Errors
    /// A malformed state ([`StateError::Invalid`]).
    pub fn new(head: &TermState, check: ImportCheck) -> Result<Self, StateError> {
        validate::head(head)?;
        let p = &head.pane;
        let mut pane = PaneTerm::new(head.cols, head.rows, p.history_limit);
        pane.history = p.keeps_history;
        // The limits first: the import copies them into the new pages.
        let _ = pane.term.set_scrollback_max_bytes(head.ghostty.max_bytes);
        let _ = pane.term.set_scrollback_max_lines(head.ghostty.max_lines);
        pane.line_limit = head.ghostty.max_lines;
        pane.restore_model(p, head.cols);
        let importer = pane
            .term
            .vtmux_importer(check == ImportCheck::Full)
            .map_err(ghostty_err)?;

        let mut raw = gv::State::default();
        raw.raw.cols = head.cols;
        raw.raw.rows = head.rows;
        // The primary first: marks and the alternate screen's entry pin
        // are pinned there.
        raw.raw.active_screen = 0;
        head.ghostty.to_ffi(&mut raw.raw);
        head.primary.to_ffi(&mut raw.raw.screens[0], &mut raw.bytes);
        if let Some(alt) = &head.alternate {
            alt.to_ffi(&mut raw.raw.screens[1], &mut raw.bytes);
        }
        Ok(Self {
            importer,
            pane,
            raw,
            title: head.ghostty.title.clone(),
            pwd: head.ghostty.pwd.clone(),
            marks: p.marks.clone(),
            alt_entry: p.alt_entry,
            alternate: head.alternate.is_some(),
            alternate_active: head.alternate_active,
            continuation: p.continuation.clone(),
            scratch: gv::Page::default(),
        })
    }

    /// Add a screen's next page (oldest first).
    ///
    /// # Errors
    /// A malformed page ([`StateError::Invalid`]), one for an alternate
    /// screen the head does not have, a blob from another Ghostty build
    /// ([`StateError::ForeignBlob`]), or one Ghostty refuses (a blob that
    /// fails its checksum, or with [`ImportCheck::Full`] does not decode).
    /// The import is as it was before the call.
    pub fn page(&mut self, screen: ScreenKind, page: &PageState) -> Result<(), StateError> {
        if screen == ScreenKind::Alternate && !self.alternate {
            return Err(StateError::Invalid("page of an absent alternate screen"));
        }
        validate::page(page, self.pane.cols())?;
        let term = &mut self.pane.term;
        match page {
            PageState::Rows(rows) => {
                rows.to_ffi(&mut self.scratch);
                self.importer
                    .page(term, screen.key(), &self.scratch.as_ref())
                    .map_err(ghostty_err)
            }
            PageState::Blob(blob) => {
                let blob_page = gv::PageRef {
                    bytes: &blob.bytes,
                    blob: true,
                    ..gv::PageRef::default()
                };
                self.importer
                    .page(term, screen.key(), &blob_page)
                    .map_err(ghostty_err)
            }
        }
    }

    /// The pane, once every page is in.
    ///
    /// # Errors
    /// Too few rows for the screen, a cursor, mark or history size the
    /// pages do not have room for, or a state Ghostty refuses.
    pub fn finish(self) -> Result<PaneTerm, StateError> {
        let Self {
            importer,
            mut pane,
            raw,
            title,
            pwd,
            marks,
            alt_entry,
            alternate_active,
            continuation,
            ..
        } = self;
        importer.finish(&mut pane.term, &raw).map_err(ghostty_err)?;
        let _ = pane.term.vtmux_set_title(&title);
        let _ = pane.term.vtmux_set_pwd(&pwd);

        // tmux's history is the newest `hsize` rows Ghostty retains
        // (`PaneTerm::history`); under an alternate screen it is the copy
        // taken on entry (none: no history), and Ghostty may have
        // reflowed the primary since. Leaving it clamps to what is
        // retained (`clamp_history_to_retained`).
        let history = if alternate_active {
            pane.frozen.as_ref().map_or(usize::MAX, |rows| rows.len())
        } else {
            pane.term.scrollback_rows().unwrap_or(0)
        };
        if pane.hsize > history {
            return Err(StateError::Invalid("history size over the rows retained"));
        }
        let pin = |pane: &PaneTerm, y: u32| {
            pane.term
                .track_grid_ref(Point::Screen(PointCoordinate { x: 0, y }))
                .ok()
                .ok_or(StateError::Invalid("mark off the primary screen"))
        };
        pane.marks = marks
            .iter()
            .map(|&(y, data)| Ok((pin(&pane, y)?, data)))
            .collect::<Result<_, StateError>>()?;
        pane.alt_entry = alt_entry
            .map(|(y, retained)| Ok::<_, StateError>((pin(&pane, y)?, retained)))
            .transpose()?;
        if alternate_active {
            pane.term
                .vtmux_set_active_screen(gv::ALTERNATE)
                .map_err(ghostty_err)?;
        }
        // Back where the parser was, with nothing it says kept.
        pane.feed(&continuation);
        let _ = pane.take_answers();
        let _ = pane.take_title_events();
        Ok(pane)
    }
}
