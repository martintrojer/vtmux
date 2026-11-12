//! The vtmux row format (D27 decision 11): one terminal row, cell for cell,
//! independent of Ghostty's page memory, and the history pages that hold
//! rows LZ4-compressed under a content hash.
//!
//! A page is named by the BLAKE3 hash of its rows' encoding before
//! compression, so the name and the snapshot's meaning depend on the row
//! codec alone: another LZ4 encoder (an `lz4_flex` update) may store the
//! same page in other bytes under the same name.
//!
//! A row is written compactly, not as a tagged record: rows are most of a
//! snapshot's bytes. Cells are run-length encoded: a run of cells sharing
//! their attributes writes the attributes once, and a run of identical cells
//! writes the cell once.
//!
//! ```text
//! row    := flags ncells [cells_used] [time] [mark] [links] [graphemes] run*
//! run    := (count << 2 | uniform << 1 | same_attrs) [attrs] text+
//! text   := codepoint << 2 | wide          (codepoint 0: an empty cell)
//! attrs  := mask [fg] [bg] [ul] [attrs] [underline] [flags] [link] [semantic] [tab]
//! colour := 1 index | 2 index | 3 r g b     (palette, 256-form palette, RGB)
//! ```
//!
//! A `uniform` run carries one `text` for all its cells, otherwise one per
//! cell; `same_attrs` reuses the previous run's attributes.

use crate::Error;
use crate::wire::{Ctx, Reader, Value, put_varint, unzigzag, zigzag};

/// Widest row a decoder accepts (tmux's pane sizes are well below it).
pub const MAX_COLS: usize = 10_000;

/// A colour as the grid holds it; palette indices stay unresolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    #[default]
    Default,
    /// Palette index 0-255, 0-15 as `30-37`/`90-97` set them.
    Palette(u8),
    /// Palette index 0-15 set in its 256-colour form (`38;5;N`): tmux keeps
    /// `COLOUR_FLAG_256` on it (input.c `input_csi_dispatch_sgr_256_do`).
    Palette256(u8),
    Rgb(u8, u8, u8),
}

/// Underline shape (SGR 4:n).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl Underline {
    const ALL: [Self; 6] = [
        Self::None,
        Self::Single,
        Self::Double,
        Self::Curly,
        Self::Dotted,
        Self::Dashed,
    ];
}

/// [`Style::attrs`] bits.
pub mod attr {
    pub const BOLD: u16 = 1 << 0;
    pub const FAINT: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const BLINK: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const INVISIBLE: u16 = 1 << 5;
    pub const STRIKETHROUGH: u16 = 1 << 6;
    pub const OVERLINE: u16 = 1 << 7;
}

/// A cell's SGR state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    /// [`attr`] bits.
    pub attrs: u16,
    pub underline: Underline,
}

/// [`Cell::flags`] bits.
pub mod cell_flag {
    /// Printed under DEC Special Graphics (tmux's `GRID_ATTR_CHARSET`).
    pub const ACS: u8 = 1 << 0;
    /// DECSCA protected.
    pub const PROTECTED: u8 = 1 << 1;
    /// A blank holding only a background colour, as an erase with a
    /// coloured background leaves it (Ghostty's `bg_color_*` content).
    pub const BG_ONLY: u8 = 1 << 2;
}

/// What a cell covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Wide {
    #[default]
    Narrow,
    /// The first column of a wide character.
    Wide,
    /// The column after a wide character.
    SpacerTail,
    /// The last column of a row whose wide character wrapped to the next.
    SpacerHead,
}

impl Wide {
    const ALL: [Self; 4] = [Self::Narrow, Self::Wide, Self::SpacerTail, Self::SpacerHead];
}

/// OSC 133 content kind of a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Semantic {
    #[default]
    Output,
    Input,
    Prompt,
}

/// A TAB cell (tmux's `GRID_FLAG_TAB`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tab {
    #[default]
    None,
    /// The first cell of a tab, and how many columns it covers.
    Start(u8),
    /// A later cell of a tab.
    Covered,
}

/// One grid cell. Its text is `codepoint` (0 for an empty cell) followed
/// by the row's [`Grapheme`] for its column, if any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub codepoint: u32,
    pub wide: Wide,
    pub style: Style,
    /// [`cell_flag`] bits.
    pub flags: u8,
    /// 0: no hyperlink; otherwise `n - 1` indexes [`Row::links`].
    pub link: u16,
    pub semantic: Semantic,
    pub tab: Tab,
}

/// The codepoints after the first of a multi-codepoint cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grapheme {
    pub col: u16,
    pub rest: String,
}

/// An OSC 8 hyperlink. URI and id are bytes: a terminal keeps what the
/// program sent, UTF-8 or not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hyperlink {
    /// The `id=` parameter, if the program gave one.
    pub id: Option<Vec<u8>>,
    pub uri: Vec<u8>,
    /// Without an `id=`: the emulator's own id for the OSC 8 that opened
    /// the link, which keeps two links to one URI apart.
    pub implicit: u32,
}

/// Ghostty's row prompt state from OSC 133.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowPrompt {
    #[default]
    None,
    Prompt,
    Continuation,
}

/// tmux's OSC 133 line flags and columns (`struct grid_line`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Osc133 {
    /// `GRID_LINE_START_PROMPT` 0x8, `SECOND_PROMPT` 0x10, `START_COMMAND`
    /// 0x20, `START_OUTPUT` 0x40, `END_OUTPUT` 0x80 (tmux.h).
    pub flags: u32,
    pub prompt_col: u16,
    pub cmd_col: u16,
    pub out_start_col: u16,
    pub out_end_col: u16,
    pub exit_status: u8,
}

/// One row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub graphemes: Vec<Grapheme>,
    pub links: Vec<Hyperlink>,
    /// Soft-wrapped into the next row (tmux's `GRID_LINE_WRAPPED`).
    pub wrapped: bool,
    /// Continues the row above (Ghostty's `wrap_continuation`).
    pub continuation: bool,
    /// tmux's `cellused`.
    pub cells_used: u16,
    pub prompt: RowPrompt,
    pub mark: Option<Osc133>,
    /// When the row scrolled into history (`grid_line_time`), epoch
    /// seconds; 0 for none.
    pub time: i64,
    /// tmux's line flags the row keeps where its cells no longer say so.
    /// Not in the row's encoding: beside the rows that hold it
    /// ([`KeptRows`]).
    pub kept: KeptLineFlags,
    /// tmux's line flags the row lacks though its cells say so (Ghostty
    /// moved or cleared the cells where tmux sets the flag). Beside the
    /// rows like [`Row::kept`], under a tag of its own where a reader has
    /// one: a reader before it, or a run without the tag, gives the row
    /// the flags its cells say.
    pub lacks: KeptLineFlags,
    /// The cells the row keeps past the width, from the column after the
    /// last one on: tmux's line that fitted a narrower width keeps its
    /// cells there (`cellsize`, grid.c `grid_reflow_move`), erased ones,
    /// each its background (`Color::Default` for none), shown again when
    /// the pane widens. Beside the rows like [`Row::kept`] ([`PastRows`]):
    /// a reader before it keeps none, as its own build did.
    pub past: Vec<Color>,
}

/// tmux's `GRID_LINE_EXTENDED` and `GRID_LINE_HYPERLINK` as a row keeps
/// them where its cells no longer say so (a wide character since
/// overwritten): tmux keeps both once a cell needed them until the line is
/// emptied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeptLineFlags {
    pub extended: bool,
    pub hyperlink: bool,
}

impl KeptLineFlags {
    /// Either flag.
    #[must_use]
    pub fn any(self) -> bool {
        self.extended || self.hyperlink
    }
}

/// The rows of a run (a record's rows, a history page's) that keep line
/// flags ([`Row::kept`]), or lack them ([`Row::lacks`]), by index in the
/// run, ascending: a field under
/// its own tag beside the rows, not in the row codec, whose readers of the
/// version refuse a row flag they don't know. A reader before it skips the
/// field and derives the flags from the cells, as its own build held them
/// (crate docs, *Changing the format*). Written as `gap flags` varint
/// pairs, `gap` the rows skipped since the last entry, `flags` 1 extended,
/// 2 hyperlink.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeptRows(Vec<(u32, KeptLineFlags)>);

impl KeptRows {
    /// The flags `which` takes of each of `rows` ([`Row::kept`] or
    /// [`Row::lacks`]).
    #[must_use]
    pub fn of(rows: &[Row], which: fn(&Row) -> KeptLineFlags) -> Self {
        let mut k = Self::default();
        for (i, r) in rows.iter().enumerate() {
            k.note(u32::try_from(i).unwrap_or(u32::MAX), which(r));
        }
        k
    }

    /// Row `index`, past every row noted so far, keeps `flags`.
    fn note(&mut self, index: u32, flags: KeptLineFlags) {
        if flags.any() {
            self.0.push((index, flags));
        }
    }

    /// Whether no row keeps a flag.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One past the last row with flags: the rows the run must have.
    fn needs(&self) -> u64 {
        self.0.last().map_or(0, |&(i, _)| u64::from(i) + 1)
    }

    /// Put the flags back on `rows` with `set`; refuses a row they don't
    /// have.
    fn apply(&self, rows: &mut [Row], set: fn(&mut Row, KeptLineFlags)) -> Result<(), Error> {
        if self.needs() > rows.len() as u64 {
            return Err(Error::Malformed("kept line flags past the rows"));
        }
        for &(i, k) in &self.0 {
            set(&mut rows[i as usize], k);
        }
        Ok(())
    }
}

impl Value for KeptRows {
    fn put(&self, out: &mut Vec<u8>) {
        let mut next = 0;
        for &(i, k) in &self.0 {
            put_varint(out, u64::from(i) - next);
            put_varint(out, u64::from(k.extended) | (u64::from(k.hyperlink) << 1));
            next = u64::from(i) + 1;
        }
    }
    fn get(buf: &[u8], cx: &Ctx) -> Result<Self, Error> {
        let mut r = Reader::new(buf);
        let mut k = Self::default();
        let mut next = 0u64;
        while !r.is_empty() {
            let i = next
                .checked_add(r.varint()?)
                .and_then(|i| u32::try_from(i).ok())
                .ok_or(Error::Malformed("kept line flags past the rows"))?;
            let flags = match r.varint()? {
                1 => KeptLineFlags {
                    extended: true,
                    hyperlink: false,
                },
                2 => KeptLineFlags {
                    extended: false,
                    hyperlink: true,
                },
                3 => KeptLineFlags {
                    extended: true,
                    hyperlink: true,
                },
                _ => return Err(Error::Malformed("bad kept line flags")),
            };
            cx.charge(size_of::<(u32, KeptLineFlags)>())?;
            k.0.push((i, flags));
            next = u64::from(i) + 1;
        }
        Ok(k)
    }
}

/// The cells rows of a run keep past the width ([`Row::past`]), by index in
/// the run, ascending: beside the rows as [`KeptRows`], which a reader
/// before it skips. Written per row as varints: `gap` (the rows skipped
/// since the last entry), the cell count, then each cell's colour (0
/// default, 1 + n palette n, 257 + n palette n in its 256-colour form, 513
/// + 0xRRGGBB RGB).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PastRows(Vec<(u32, Vec<Color>)>);

impl PastRows {
    /// The cells each of `rows` keeps past the width.
    #[must_use]
    pub fn of(rows: &[Row]) -> Self {
        let mut p = Self::default();
        for (i, r) in rows.iter().enumerate() {
            p.note(u32::try_from(i).unwrap_or(u32::MAX), &r.past);
        }
        p
    }

    /// Row `index`, past every row noted so far, keeps `past`.
    fn note(&mut self, index: u32, past: &[Color]) {
        if !past.is_empty() {
            self.0.push((index, past.to_vec()));
        }
    }

    /// Whether no row keeps a cell.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One past the last row with cells: the rows the run must have.
    fn needs(&self) -> u64 {
        self.0.last().map_or(0, |&(i, _)| u64::from(i) + 1)
    }

    /// Put the cells back on `rows`; refuses a row they don't have.
    fn apply(&self, rows: &mut [Row]) -> Result<(), Error> {
        if self.needs() > rows.len() as u64 {
            return Err(Error::Malformed("cells past the width past the rows"));
        }
        for (i, past) in &self.0 {
            rows[*i as usize].past.clone_from(past);
        }
        Ok(())
    }
}

/// A cell's colour in [`PastRows`]' encoding.
fn past_code(colour: Color) -> u64 {
    match colour {
        Color::Default => 0,
        Color::Palette(index) => 1 + u64::from(index),
        Color::Palette256(index) => 257 + u64::from(index),
        Color::Rgb(red, green, blue) => {
            513 + (u64::from(red) << 16 | u64::from(green) << 8 | u64::from(blue))
        }
    }
}

/// [`past_code`] read back; `None` for a number of no colour.
fn past_colour(code: u64) -> Option<Color> {
    Some(match code {
        0 => Color::Default,
        1..=256 => Color::Palette(u8::try_from(code - 1).ok()?),
        257..=512 => Color::Palette256(u8::try_from(code - 257).ok()?),
        513..=0x0100_0200 => {
            let [_, red, green, blue] = u32::try_from(code - 513).ok()?.to_be_bytes();
            Color::Rgb(red, green, blue)
        }
        _ => return None,
    })
}

impl Value for PastRows {
    fn put(&self, out: &mut Vec<u8>) {
        let mut next = 0;
        for (index, past) in &self.0 {
            put_varint(out, u64::from(*index) - next);
            put_varint(out, past.len() as u64);
            for &colour in past {
                put_varint(out, past_code(colour));
            }
            next = u64::from(*index) + 1;
        }
    }
    fn get(buf: &[u8], cx: &Ctx) -> Result<Self, Error> {
        let mut reader = Reader::new(buf);
        let mut rows = Self::default();
        let mut next = 0u64;
        while !reader.is_empty() {
            let index = next
                .checked_add(reader.varint()?)
                .and_then(|at| u32::try_from(at).ok())
                .ok_or(Error::Malformed("cells past the width past the rows"))?;
            let count = usize::try_from(reader.varint()?)
                .ok()
                .filter(|&count| count > 0 && count <= MAX_COLS)
                .ok_or(Error::Malformed("bad cells past the width"))?;
            cx.charge(size_of::<(u32, Vec<Color>)>() + count * size_of::<Color>())?;
            let mut past = Vec::with_capacity(count);
            for _ in 0..count {
                past.push(
                    past_colour(reader.varint()?)
                        .ok_or(Error::Malformed("bad cell colour past the width"))?,
                );
            }
            rows.0.push((index, past));
            next = u64::from(index) + 1;
        }
        Ok(rows)
    }
}

/// Field mode `past` (`record!`'s `tag / kept_tag / lacks_tag / past_tag`):
/// the [`PastRows`] of a `many` field of rows, after them under its own tag.
pub(crate) mod past {
    use super::{PastRows, Row};
    use crate::Error;
    use crate::wire::{Ctx, Value, one};

    pub fn put(out: &mut Vec<u8>, tag: u64, rows: &[Row]) {
        one::put(out, tag, &PastRows::of(rows));
    }

    pub fn get(rows: &mut [Row], buf: &[u8], cx: &Ctx) -> Result<(), Error> {
        PastRows::get(buf, cx)?.apply(rows)
    }
}

/// Field mode `kept` (`record!`'s `tag / kept_tag`): the [`KeptRows`] of
/// a `many` field of rows, written after them under its own tag.
pub(crate) mod kept {
    use super::{KeptRows, Row};
    use crate::Error;
    use crate::wire::{Ctx, Value, one};

    pub fn put(out: &mut Vec<u8>, tag: u64, rows: &[Row]) {
        one::put(out, tag, &KeptRows::of(rows, |r| r.kept));
    }

    pub fn get(rows: &mut [Row], buf: &[u8], cx: &Ctx) -> Result<(), Error> {
        KeptRows::get(buf, cx)?.apply(rows, |r, k| r.kept = k)
    }
}

/// Field mode `lacks` (`record!`'s `tag / kept_tag / lacks_tag`): the
/// [`Row::lacks`] of a `many` field of rows, after them under its own tag.
pub(crate) mod lacks {
    use super::{KeptRows, Row};
    use crate::Error;
    use crate::wire::{Ctx, Value, one};

    pub fn put(out: &mut Vec<u8>, tag: u64, rows: &[Row]) {
        one::put(out, tag, &KeptRows::of(rows, |r| r.lacks));
    }

    pub fn get(rows: &mut [Row], buf: &[u8], cx: &Ctx) -> Result<(), Error> {
        KeptRows::get(buf, cx)?.apply(rows, |r, k| r.lacks = k)
    }
}

const ROW_WRAPPED: u64 = 1 << 0;
const ROW_CONTINUATION: u64 = 1 << 1;
const ROW_PROMPT_SHIFT: u32 = 2; // two bits
const ROW_USED: u64 = 1 << 4;
const ROW_TIME: u64 = 1 << 5;
const ROW_MARK: u64 = 1 << 6;
const ROW_LINKS: u64 = 1 << 7;
const ROW_GRAPHEMES: u64 = 1 << 8;

const A_FG: u64 = 1 << 0;
const A_BG: u64 = 1 << 1;
const A_UL: u64 = 1 << 2;
const A_ATTRS: u64 = 1 << 3;
const A_UNDERLINE: u64 = 1 << 4;
const A_FLAGS: u64 = 1 << 5;
const A_LINK: u64 = 1 << 6;
const A_SEMANTIC: u64 = 1 << 7;
const A_TAB: u64 = 1 << 8;

/// The part of a cell a run shares.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Attrs {
    style: Style,
    flags: u8,
    link: u16,
    semantic: Semantic,
    tab: Tab,
}

impl Attrs {
    fn of(c: &Cell) -> Self {
        Self {
            style: c.style,
            flags: c.flags,
            link: c.link,
            semantic: c.semantic,
            tab: c.tab,
        }
    }
}

#[inline]
fn same_attrs(a: &Cell, b: &Cell) -> bool {
    let (x, y) = (&a.style, &b.style);
    x.attrs == y.attrs
        && x.fg == y.fg
        && x.bg == y.bg
        && x.underline_color == y.underline_color
        && x.underline == y.underline
        && a.flags == b.flags
        && a.link == b.link
        && a.semantic == b.semantic
        && a.tab == b.tab
}

fn text_of(c: &Cell) -> u64 {
    (u64::from(c.codepoint) << 2) | (c.wide as u64)
}

fn put_color(out: &mut Vec<u8>, c: Color) {
    match c {
        Color::Default => out.push(0),
        Color::Palette(n) => out.extend_from_slice(&[1, n]),
        Color::Palette256(n) => out.extend_from_slice(&[2, n]),
        Color::Rgb(r, g, b) => out.extend_from_slice(&[3, r, g, b]),
    }
}

fn get_color(r: &mut Reader<'_>) -> Result<Color, Error> {
    Ok(match r.byte()? {
        0 => Color::Default,
        1 => Color::Palette(r.byte()?),
        2 => Color::Palette256(r.byte()?),
        3 => {
            let b = r.bytes(3)?;
            Color::Rgb(b[0], b[1], b[2])
        }
        _ => return Err(Error::Malformed("unknown colour kind")),
    })
}

fn put_attrs(out: &mut Vec<u8>, a: &Attrs) {
    let s = &a.style;
    let mut mask = 0;
    for (bit, on) in [
        (A_FG, s.fg != Color::Default),
        (A_BG, s.bg != Color::Default),
        (A_UL, s.underline_color != Color::Default),
        (A_ATTRS, s.attrs != 0),
        (A_UNDERLINE, s.underline != Underline::None),
        (A_FLAGS, a.flags != 0),
        (A_LINK, a.link != 0),
        (A_SEMANTIC, a.semantic != Semantic::Output),
        (A_TAB, a.tab != Tab::None),
    ] {
        if on {
            mask |= bit;
        }
    }
    put_varint(out, mask);
    if mask & A_FG != 0 {
        put_color(out, s.fg);
    }
    if mask & A_BG != 0 {
        put_color(out, s.bg);
    }
    if mask & A_UL != 0 {
        put_color(out, s.underline_color);
    }
    if mask & A_ATTRS != 0 {
        put_varint(out, u64::from(s.attrs));
    }
    if mask & A_UNDERLINE != 0 {
        out.push(s.underline as u8);
    }
    if mask & A_FLAGS != 0 {
        out.push(a.flags);
    }
    if mask & A_LINK != 0 {
        put_varint(out, u64::from(a.link));
    }
    if mask & A_SEMANTIC != 0 {
        out.push(a.semantic as u8);
    }
    match a.tab {
        Tab::None => {}
        Tab::Covered => out.push(0),
        Tab::Start(n) => {
            out.push(1);
            out.push(n);
        }
    }
}

fn get_attrs(r: &mut Reader<'_>, links: usize) -> Result<Attrs, Error> {
    let mask = r.varint()?;
    if mask >> 9 != 0 {
        return Err(Error::Malformed("unknown cell attribute"));
    }
    let mut a = Attrs {
        style: Style::default(),
        flags: 0,
        link: 0,
        semantic: Semantic::Output,
        tab: Tab::None,
    };
    if mask & A_FG != 0 {
        a.style.fg = get_color(r)?;
    }
    if mask & A_BG != 0 {
        a.style.bg = get_color(r)?;
    }
    if mask & A_UL != 0 {
        a.style.underline_color = get_color(r)?;
    }
    if mask & A_ATTRS != 0 {
        a.style.attrs =
            u16::try_from(r.varint()?).map_err(|_| Error::Malformed("bad attribute bits"))?;
    }
    if mask & A_UNDERLINE != 0 {
        a.style.underline = *Underline::ALL
            .get(usize::from(r.byte()?))
            .ok_or(Error::Malformed("unknown underline"))?;
    }
    if mask & A_FLAGS != 0 {
        a.flags = r.byte()?;
    }
    if mask & A_LINK != 0 {
        a.link = u16::try_from(r.varint()?).map_err(|_| Error::Malformed("bad link index"))?;
        if a.link == 0 || usize::from(a.link) > links {
            return Err(Error::Malformed("link index outside the row's links"));
        }
    }
    if mask & A_SEMANTIC != 0 {
        a.semantic = match r.byte()? {
            1 => Semantic::Input,
            2 => Semantic::Prompt,
            _ => return Err(Error::Malformed("unknown semantic content")),
        };
    }
    if mask & A_TAB != 0 {
        a.tab = match r.byte()? {
            0 => Tab::Covered,
            1 => Tab::Start(r.byte()?),
            _ => return Err(Error::Malformed("unknown tab mark")),
        };
    }
    Ok(a)
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn get_string(r: &mut Reader<'_>) -> Result<String, Error> {
    std::str::from_utf8(r.chunk()?)
        .map(str::to_owned)
        .map_err(|_| Error::Malformed("string is not UTF-8"))
}

fn get_u16(r: &mut Reader<'_>) -> Result<u16, Error> {
    u16::try_from(r.varint()?).map_err(|_| Error::Malformed("column out of range"))
}

/// Identical cells in a row of at least this many become one uniform run.
const UNIFORM_MIN: usize = 3;

impl Row {
    /// Append the row's encoding to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let mut flags = (self.prompt as u64) << ROW_PROMPT_SHIFT;
        for (bit, on) in [
            (ROW_WRAPPED, self.wrapped),
            (ROW_CONTINUATION, self.continuation),
            (ROW_USED, self.cells_used != 0),
            (ROW_TIME, self.time != 0),
            (ROW_MARK, self.mark.is_some()),
            (ROW_LINKS, !self.links.is_empty()),
            (ROW_GRAPHEMES, !self.graphemes.is_empty()),
        ] {
            if on {
                flags |= bit;
            }
        }
        put_varint(out, flags);
        put_varint(out, self.cells.len() as u64);
        if self.cells_used != 0 {
            put_varint(out, u64::from(self.cells_used));
        }
        if self.time != 0 {
            put_varint(out, zigzag(self.time));
        }
        if let Some(m) = &self.mark {
            put_varint(out, u64::from(m.flags));
            for c in [m.prompt_col, m.cmd_col, m.out_start_col, m.out_end_col] {
                put_varint(out, u64::from(c));
            }
            out.push(m.exit_status);
        }
        if !self.links.is_empty() {
            put_varint(out, self.links.len() as u64);
            for l in &self.links {
                match &l.id {
                    None if l.implicit == 0 => out.push(0),
                    None => {
                        out.push(2);
                        put_varint(out, u64::from(l.implicit));
                    }
                    Some(id) => {
                        out.push(1);
                        put_bytes(out, id);
                    }
                }
                put_bytes(out, &l.uri);
            }
        }
        if !self.graphemes.is_empty() {
            put_varint(out, self.graphemes.len() as u64);
            for g in &self.graphemes {
                put_varint(out, u64::from(g.col));
                put_string(out, &g.rest);
            }
        }
        self.encode_runs(out);
    }

    fn encode_runs(&self, out: &mut Vec<u8>) {
        let cells = &self.cells;
        let n = cells.len();
        out.reserve(n * 2 + 8);
        // A uniform span starts at `k`: `UNIFORM_MIN` identical cells.
        let uniform_at = |k: usize| {
            k + UNIFORM_MIN <= n && cells[k + 1..k + UNIFORM_MIN].iter().all(|c| *c == cells[k])
        };
        let mut prev: Option<Attrs> = None;
        let mut j = 0;
        while j < n {
            let first = &cells[j];
            let (count, uniform) = if uniform_at(j) {
                let mut end = j + UNIFORM_MIN;
                while end < n && cells[end] == *first {
                    end += 1;
                }
                (end - j, true)
            } else {
                // Literal until the attributes change or a uniform span
                // begins.
                let mut end = j + 1;
                while end < n && same_attrs(&cells[end], first) && !uniform_at(end) {
                    end += 1;
                }
                (end - j, false)
            };
            let attrs = Attrs::of(first);
            let same_attrs = prev == Some(attrs);
            put_varint(
                out,
                ((count as u64) << 2) | (u64::from(uniform) << 1) | u64::from(same_attrs),
            );
            if !same_attrs {
                put_attrs(out, &attrs);
                prev = Some(attrs);
            }
            if uniform {
                put_varint(out, text_of(first));
            } else {
                for c in &cells[j..j + count] {
                    put_varint(out, text_of(c));
                }
            }
            j += count;
        }
    }

    /// Read one row from `r` into `self`, reusing its buffers.
    pub fn decode_from(&mut self, r: &mut Reader<'_>) -> Result<(), Error> {
        let flags = r.varint()?;
        if flags >> 9 != 0 {
            return Err(Error::Malformed("unknown row flag"));
        }
        let ncells = usize::try_from(r.varint()?).map_err(|_| Error::Malformed("row too wide"))?;
        if ncells > MAX_COLS {
            return Err(Error::Malformed("row too wide"));
        }
        self.wrapped = flags & ROW_WRAPPED != 0;
        self.continuation = flags & ROW_CONTINUATION != 0;
        // Beside the row, not in it ([`KeptRows`], [`PastRows`]).
        self.kept = KeptLineFlags::default();
        self.lacks = KeptLineFlags::default();
        self.past.clear();
        self.prompt = match (flags >> ROW_PROMPT_SHIFT) & 3 {
            0 => RowPrompt::None,
            1 => RowPrompt::Prompt,
            2 => RowPrompt::Continuation,
            _ => return Err(Error::Malformed("unknown row prompt")),
        };
        self.cells_used = if flags & ROW_USED != 0 {
            get_u16(r)?
        } else {
            0
        };
        self.time = if flags & ROW_TIME != 0 {
            unzigzag(r.varint()?)
        } else {
            0
        };
        self.mark = if flags & ROW_MARK != 0 {
            Some(Osc133 {
                flags: r.u32()?,
                prompt_col: get_u16(r)?,
                cmd_col: get_u16(r)?,
                out_start_col: get_u16(r)?,
                out_end_col: get_u16(r)?,
                exit_status: r.byte()?,
            })
        } else {
            None
        };
        self.links.clear();
        if flags & ROW_LINKS != 0 {
            let n = r.varint()?;
            if n > ncells as u64 {
                return Err(Error::Malformed("more links than cells"));
            }
            for _ in 0..n {
                let (id, implicit) = match r.byte()? {
                    0 => (None, 0),
                    1 => (Some(r.chunk()?.to_vec()), 0),
                    2 => (None, r.u32()?),
                    _ => return Err(Error::Malformed("bad hyperlink")),
                };
                let uri = r.chunk()?.to_vec();
                self.links.push(Hyperlink { id, uri, implicit });
            }
        }
        self.graphemes.clear();
        if flags & ROW_GRAPHEMES != 0 {
            let n = r.varint()?;
            if n > ncells as u64 {
                return Err(Error::Malformed("more graphemes than cells"));
            }
            for _ in 0..n {
                let col = get_u16(r)?;
                if usize::from(col) >= ncells {
                    return Err(Error::Malformed("grapheme outside the row"));
                }
                let rest = get_string(r)?;
                self.graphemes.push(Grapheme { col, rest });
            }
        }
        self.cells.clear();
        self.cells.reserve(ncells);
        let mut attrs = None;
        while self.cells.len() < ncells {
            let h = r.varint()?;
            let count = usize::try_from(h >> 2).map_err(|_| Error::Malformed("run too long"))?;
            if count == 0 || count > ncells - self.cells.len() {
                return Err(Error::Malformed("run outside the row"));
            }
            if h & 1 == 0 {
                attrs = Some(get_attrs(r, self.links.len())?);
            }
            let a = attrs.ok_or(Error::Malformed("run reuses attributes it has not seen"))?;
            if h & 2 != 0 {
                let cell = make_cell(&a, r.varint()?)?;
                self.cells.extend(std::iter::repeat_n(cell, count));
            } else {
                for _ in 0..count {
                    self.cells.push(make_cell(&a, r.varint()?)?);
                }
            }
        }
        Ok(())
    }

    /// The row's encoding.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// A row from exactly `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes);
        let mut row = Self::default();
        row.decode_from(&mut r)?;
        if !r.is_empty() {
            return Err(Error::Malformed("trailing bytes after a row"));
        }
        Ok(row)
    }
}

fn make_cell(a: &Attrs, text: u64) -> Result<Cell, Error> {
    let cp = u32::try_from(text >> 2).map_err(|_| Error::Malformed("bad codepoint"))?;
    if cp != 0 && char::from_u32(cp).is_none() {
        return Err(Error::Malformed("bad codepoint"));
    }
    Ok(Cell {
        codepoint: cp,
        wide: Wide::ALL[usize::from(text.to_le_bytes()[0] & 3)],
        style: a.style,
        flags: a.flags,
        link: a.link,
        semantic: a.semantic,
        tab: a.tab,
    })
}

impl Row {
    /// The memory the row holds beyond its own size.
    fn heap_size(&self) -> usize {
        self.cells.len() * size_of::<Cell>()
            + self.graphemes.len() * size_of::<Grapheme>()
            + self.links.len() * size_of::<Hyperlink>()
    }

    /// An upper bound on the length of the row's encoding.
    fn encoded_bound(&self) -> u64 {
        // A cell takes at most 32 bytes (run header, every attribute, a
        // four-byte codepoint), the row's fixed fields under 64.
        let strings: usize = self
            .links
            .iter()
            .map(|l| 21 + l.uri.len() + l.id.as_ref().map_or(0, Vec::len))
            .chain(self.graphemes.iter().map(|g| 13 + g.rest.len()))
            .sum();
        (64 + 40 * self.cells.len() + strings) as u64
    }
}

impl Value for Row {
    fn put(&self, out: &mut Vec<u8>) {
        self.encode_into(out);
    }
    fn get(buf: &[u8], cx: &Ctx) -> Result<Self, Error> {
        let row = Self::decode(buf)?;
        cx.charge(row.heap_size())?;
        Ok(row)
    }
}

/// A pen (a cursor's style) as its row-attribute encoding.
impl Value for Style {
    fn put(&self, out: &mut Vec<u8>) {
        put_attrs(
            out,
            &Attrs {
                style: *self,
                flags: 0,
                link: 0,
                semantic: Semantic::Output,
                tab: Tab::None,
            },
        );
    }
    fn get(buf: &[u8], _: &Ctx) -> Result<Self, Error> {
        let mut r = Reader::new(buf);
        let a = get_attrs(&mut r, 0)?;
        if !r.is_empty() || a.flags != 0 || a.semantic != Semantic::Output || a.tab != Tab::None {
            return Err(Error::Malformed("not a style"));
        }
        Ok(a.style)
    }
}

impl Value for Hyperlink {
    fn put(&self, out: &mut Vec<u8>) {
        crate::wire::opt::put(out, 1, &self.id);
        crate::wire::one::put(out, 2, &self.uri);
        crate::wire::one::put(out, 3, &self.implicit);
    }
    fn get(buf: &[u8], cx: &Ctx) -> Result<Self, Error> {
        let mut h = Self::default();
        let mut r = Reader::new(buf);
        while let Some((tag, body)) = r.field()? {
            match tag {
                1 => crate::wire::opt::get(&mut h.id, body, cx)?,
                2 => crate::wire::one::get(&mut h.uri, body, cx)?,
                3 => crate::wire::one::get(&mut h.implicit, body, cx)?,
                _ => {}
            }
        }
        Ok(h)
    }
}

/// A page's content hash (BLAKE3 of its rows' encoding, before
/// compression): the chunk name on disk.
pub type Hash = [u8; 32];

/// Largest uncompressed page a decoder accepts.
pub const MAX_PAGE_BYTES: u64 = 64 << 20;

/// Most rows in a page a decoder accepts.
pub const MAX_PAGE_ROWS: u32 = 1 << 16;

/// Most cells in a page a decoder accepts: what bounds the work one page
/// costs, since LZ4 and run-length encoding together let a few hundred
/// bytes stand for millions of blank cells. [`PageBuilder::fits`] keeps a
/// writer under every page limit.
pub const MAX_PAGE_CELLS: u64 = 1 << 22;

/// A run of history rows, LZ4-compressed, named by the hash of their
/// encoding. `data` was empty in a `vtmux-persist` manifest (3.8-beta3
/// and before), whose pages lived in files of their own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryPage {
    pub rows: u32,
    /// Length of the rows' encoding before compression.
    pub raw_len: u64,
    /// BLAKE3 of the rows' encoding.
    pub hash: Hash,
    /// Ghostty held the page compressed in memory when it was exported.
    pub compressed: bool,
    /// The LZ4 block.
    pub data: Vec<u8>,
    /// The page in Ghostty's own compressed form instead of rows (decision
    /// 23): only a binary of the same Ghostty patch stack reads it, so a
    /// stream with one requires [`crate::ghostty_pages_feature`]. Upgrade
    /// streams only, never a checkpoint. `rows`, `raw_len`, `hash` and
    /// `data` are then unset.
    pub ghostty: Vec<u8>,
    /// The page's rows that keep line flags ([`Row::kept`]): in the
    /// record, not in `data`, so the page's name doesn't depend on them.
    pub kept: KeptRows,
    /// The page's rows that lack line flags ([`Row::lacks`]), as `kept`.
    pub lacks: KeptRows,
    /// The cells the page's rows keep past the width ([`Row::past`]), as
    /// `kept`.
    pub past: PastRows,
}

/// Builds a [`HistoryPage`] one row at a time, so a pane's history never
/// needs to exist as typed rows all at once.
#[derive(Debug, Default)]
pub struct PageBuilder {
    raw: Vec<u8>,
    rows: u32,
    cells: u64,
    kept: KeptRows,
    lacks: KeptRows,
    past: PastRows,
}

impl PageBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a row. Check [`PageBuilder::fits`] first: a page past the
    /// limits encodes, but no reader accepts it.
    pub fn push(&mut self, row: &Row) {
        row.encode_into(&mut self.raw);
        self.kept.note(self.rows, row.kept);
        self.lacks.note(self.rows, row.lacks);
        self.past.note(self.rows, &row.past);
        self.rows += 1;
        self.cells += row.cells.len() as u64;
    }

    /// Whether `row` can join the page without taking it past
    /// [`MAX_PAGE_ROWS`], [`MAX_PAGE_CELLS`] or [`MAX_PAGE_BYTES`].
    #[must_use]
    pub fn fits(&self, row: &Row) -> bool {
        self.rows < MAX_PAGE_ROWS
            && self.cells + row.cells.len() as u64 <= MAX_PAGE_CELLS
            && self.raw.len() as u64 + row.encoded_bound() <= MAX_PAGE_BYTES
    }

    #[must_use]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Cells pushed so far.
    #[must_use]
    pub fn cells(&self) -> u64 {
        self.cells
    }

    /// Bytes encoded so far, before compression.
    #[must_use]
    pub fn raw_len(&self) -> usize {
        self.raw.len()
    }

    /// Compress the rows pushed so far into a page.
    #[must_use]
    pub fn finish(self, compressed: bool) -> HistoryPage {
        let data = lz4_flex::block::compress(&self.raw);
        HistoryPage {
            rows: self.rows,
            raw_len: self.raw.len() as u64,
            hash: *blake3::hash(&self.raw).as_bytes(),
            compressed,
            data,
            ghostty: Vec::new(),
            kept: self.kept,
            lacks: self.lacks,
            past: self.past,
        }
    }
}

impl HistoryPage {
    #[must_use]
    pub fn from_rows(rows: &[Row], compressed: bool) -> Self {
        let mut b = PageBuilder::new();
        for r in rows {
            b.push(r);
        }
        b.finish(compressed)
    }

    /// Whether `data` is present, decompresses within the page limits and
    /// matches `hash`.
    #[must_use]
    pub fn verified(&self) -> bool {
        self.raw().is_ok()
    }

    /// The rows' encoding: `data` decompressed, checked against the page's
    /// sizes, its limits and its hash.
    pub fn raw(&self) -> Result<Vec<u8>, Error> {
        let raw = self.inflate()?;
        if *blake3::hash(&raw).as_bytes() != self.hash {
            return Err(Error::MissingChunk(self.hash));
        }
        Ok(raw)
    }

    /// [`HistoryPage::raw`] without the hash check.
    fn inflate(&self) -> Result<Vec<u8>, Error> {
        if self.data.is_empty() {
            return Err(Error::MissingChunk(self.hash));
        }
        // LZ4 expands at most 255 times; every row takes at least 2 bytes.
        if self.raw_len > MAX_PAGE_BYTES
            || self.rows > MAX_PAGE_ROWS
            || self.raw_len > self.data.len() as u64 * 255
            || u64::from(self.rows) * 2 > self.raw_len
        {
            return Err(Error::Malformed("history page sizes don't add up"));
        }
        let mut raw = vec![0; usize::try_from(self.raw_len).expect("checked above")];
        let n = lz4_flex::block::decompress_into(&self.data, &mut raw)
            .map_err(|_| Error::Malformed("history page does not decompress"))?;
        if n != raw.len() {
            return Err(Error::Malformed("history page shorter than its length"));
        }
        Ok(raw)
    }

    /// Call `f` with each row in order, reusing one [`Row`]. Checks the
    /// page's sizes and limits ([`MAX_PAGE_CELLS`] too), but not its hash:
    /// decoding checked that, against the stream's hash for an inline page
    /// and against the page's own for a chunk ([`HistoryPage::each_row_verified`] does
    /// both).
    pub fn each_row(&self, f: impl FnMut(&Row)) -> Result<(), Error> {
        self.rows_of(&self.inflate()?, f)
    }

    /// [`HistoryPage::each_row`] with the hash checked too.
    pub fn each_row_verified(&self, f: impl FnMut(&Row)) -> Result<(), Error> {
        self.rows_of(&self.raw()?, f)
    }

    fn rows_of(&self, raw: &[u8], mut f: impl FnMut(&Row)) -> Result<(), Error> {
        let mut r = Reader::new(raw);
        let mut row = Row::default();
        let mut cells = 0u64;
        if self.kept.needs().max(self.lacks.needs()) > u64::from(self.rows) {
            return Err(Error::Malformed("kept line flags past the rows"));
        }
        if self.past.needs() > u64::from(self.rows) {
            return Err(Error::Malformed("cells past the width past the rows"));
        }
        let mut kept = self.kept.0.iter().peekable();
        let mut lacks = self.lacks.0.iter().peekable();
        let mut past = self.past.0.iter().peekable();
        for i in 0..self.rows {
            row.decode_from(&mut r)?;
            if let Some(&(_, k)) = kept.next_if(|&&(at, _)| at == i) {
                row.kept = k;
            }
            if let Some(&(_, k)) = lacks.next_if(|&&(at, _)| at == i) {
                row.lacks = k;
            }
            if let Some((_, p)) = past.next_if(|(at, _)| *at == i) {
                row.past.clone_from(p);
            }
            cells += row.cells.len() as u64;
            if cells > MAX_PAGE_CELLS {
                return Err(Error::Malformed("history page has too many cells"));
            }
            f(&row);
        }
        if !r.is_empty() {
            return Err(Error::Malformed("history page longer than its rows"));
        }
        Ok(())
    }

    /// Every row of the page.
    pub fn decode_rows(&self) -> Result<Vec<Row>, Error> {
        // `rows` is the page's own claim: grow as rows arrive.
        let mut out = Vec::new();
        self.each_row(|r| out.push(r.clone()))?;
        Ok(out)
    }
}

impl Value for HistoryPage {
    fn put(&self, out: &mut Vec<u8>) {
        self.put_with(out, true);
    }

    fn get(buf: &[u8], cx: &Ctx) -> Result<Self, Error> {
        use crate::wire::one;
        let mut p = Self::default();
        let mut r = Reader::new(buf);
        while let Some((tag, body)) = r.field()? {
            match tag {
                1 => one::get(&mut p.rows, body, cx)?,
                2 => one::get(&mut p.raw_len, body, cx)?,
                3 => one::get(&mut p.hash, body, cx)?,
                4 => one::get(&mut p.compressed, body, cx)?,
                5 => one::get(&mut p.data, body, cx)?,
                // Written only behind the `requires` entry
                // `crate::ghostty_pages_feature` (decision 23).
                6 => one::get(&mut p.ghostty, body, cx)?,
                // Skipped by a reader before it ([`KeptRows`]).
                7 => one::get(&mut p.kept, body, cx)?,
                // Skipped by a reader before it, as 7 ([`Row::lacks`]).
                8 => one::get(&mut p.lacks, body, cx)?,
                // Skipped by a reader before it, as 7 ([`Row::past`]).
                9 => one::get(&mut p.past, body, cx)?,
                _ => {}
            }
        }
        Ok(p)
    }
}

impl HistoryPage {
    /// The page as a record; without `data` when `inline` is false (a
    /// manifest names its pages, the chunks hold them).
    pub(crate) fn put_with(&self, out: &mut Vec<u8>, inline: bool) {
        use crate::wire::one;
        one::put(out, 1, &self.rows);
        one::put(out, 2, &self.raw_len);
        one::put(out, 3, &self.hash);
        one::put(out, 4, &self.compressed);
        if inline {
            one::put(out, 5, &self.data);
        }
        one::put(out, 6, &self.ghostty);
        one::put(out, 7, &self.kept);
        one::put(out, 8, &self.lacks);
        one::put(out, 9, &self.past);
    }

    /// A page in Ghostty's own compressed form ([`HistoryPage::ghostty`]).
    #[must_use]
    pub fn from_ghostty(bytes: Vec<u8>) -> Self {
        Self {
            compressed: true,
            ghostty: bytes,
            ..Self::default()
        }
    }

    /// Whether the page is in Ghostty's own form rather than rows.
    #[must_use]
    pub fn is_ghostty(&self) -> bool {
        !self.ghostty.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn ch(c: char) -> u32 {
        c as u32
    }

    /// A row with every row-level field and every kind of cell.
    pub(crate) fn sample_row() -> Row {
        let mut cells = vec![Cell::default(); 20];
        for (i, c) in "hello".chars().enumerate() {
            cells[i].codepoint = ch(c);
            cells[i].style.fg = Color::Palette(1);
            cells[i].style.attrs = attr::BOLD | attr::ITALIC;
        }
        cells[5] = Cell {
            codepoint: ch('世'),
            wide: Wide::Wide,
            style: Style {
                fg: Color::Rgb(1, 2, 3),
                bg: Color::Palette256(4),
                underline_color: Color::Palette(200),
                attrs: attr::INVERSE | attr::OVERLINE | attr::STRIKETHROUGH,
                underline: Underline::Curly,
            },
            flags: cell_flag::PROTECTED,
            link: 1,
            semantic: Semantic::Input,
            tab: Tab::None,
        };
        cells[6] = Cell {
            wide: Wide::SpacerTail,
            ..cells[5]
        };
        cells[7].codepoint = ch('e');
        cells[8] = Cell {
            codepoint: ch('q'),
            flags: cell_flag::ACS,
            semantic: Semantic::Prompt,
            ..Cell::default()
        };
        for (i, c) in cells.iter_mut().enumerate().skip(9).take(4) {
            c.codepoint = ch(' ');
            c.tab = if i == 9 { Tab::Start(4) } else { Tab::Covered };
        }
        for c in &mut cells[13..19] {
            c.style.bg = Color::Palette(4);
            c.flags = cell_flag::BG_ONLY;
        }
        cells[19].wide = Wide::SpacerHead;
        Row {
            cells,
            graphemes: vec![Grapheme {
                col: 7,
                rest: "\u{301}".into(),
            }],
            links: vec![Hyperlink {
                id: Some("x1".into()),
                uri: "https://example.com/".into(),
                implicit: 0,
            }],
            wrapped: true,
            continuation: true,
            cells_used: 19,
            prompt: RowPrompt::Continuation,
            mark: Some(Osc133 {
                flags: 0x8 | 0x40,
                prompt_col: 1,
                cmd_col: 2,
                out_start_col: 3,
                out_end_col: 4,
                exit_status: 7,
            }),
            time: 1_790_000_000,
            // Added within v1, beside the row (kept_line_flags_go_beside_the_row_codec).
            kept: KeptLineFlags::default(),
            lacks: KeptLineFlags::default(),
            past: Vec::new(),
        }
    }

    #[test]
    fn row_round_trips_every_field() {
        let row = sample_row();
        assert_eq!(Row::decode(&row.encode()).unwrap(), row);
        let blank = Row {
            cells: vec![Cell::default(); 80],
            ..Row::default()
        };
        assert_eq!(Row::decode(&blank.encode()).unwrap(), blank);
        assert_eq!(
            Row::decode(&Row::default().encode()).unwrap(),
            Row::default()
        );
    }

    /// The kept line flags, added within v1, stay out of the row codec
    /// that every reader of the version shares: a row's encoding and a
    /// page's data and name are the same without them, so a reader before
    /// them (whose codec refuses row flags from bit 9 on) reads the page.
    /// They round-trip beside the rows, in the page's record, inline and in
    /// a manifest.
    #[test]
    fn kept_line_flags_go_beside_the_row_codec() {
        let cx = Ctx::new(1);
        for (extended, hyperlink) in [(true, false), (false, true), (true, true)] {
            let row = Row {
                kept: KeptLineFlags {
                    extended,
                    hyperlink,
                },
                ..sample_row()
            };
            assert_eq!(row.encode(), sample_row().encode());
            let rows = [sample_row(), row.clone(), sample_row(), row];
            let page = HistoryPage::from_rows(&rows, false);
            let plain = HistoryPage::from_rows(
                &[sample_row(), sample_row(), sample_row(), sample_row()],
                false,
            );
            assert_eq!((&page.data, page.hash), (&plain.data, plain.hash));
            assert_eq!(page.decode_rows().unwrap(), rows);
            for inline in [true, false] {
                let mut out = Vec::new();
                page.put_with(&mut out, inline);
                let mut back = HistoryPage::get(&out, &cx).unwrap();
                back.data.clone_from(&page.data);
                assert_eq!(back, page, "inline {inline}");
            }
        }
        let mut past = Vec::new();
        put_varint(&mut past, 1 << 9);
        put_varint(&mut past, 0);
        assert!(Row::decode(&past).is_err());
    }

    /// Kept line flags for a row the run doesn't have, or flags of no
    /// meaning, are refused.
    #[test]
    fn kept_line_flags_past_the_rows_are_refused() {
        let cx = Ctx::new(1);
        let kept = |pairs: &[u64]| {
            let mut b = Vec::new();
            for &v in pairs {
                put_varint(&mut b, v);
            }
            b
        };
        assert!(KeptRows::get(&kept(&[0, 0]), &cx).is_err());
        assert!(KeptRows::get(&kept(&[0, 4]), &cx).is_err());
        assert!(KeptRows::get(&kept(&[1 << 32, 1]), &cx).is_err());
        let last = KeptRows::get(&kept(&[u64::from(u32::MAX), 1]), &cx).unwrap();
        let mut again = Vec::new();
        last.put(&mut again);
        assert_eq!(KeptRows::get(&again, &cx).unwrap(), last);
        assert!(last.apply(&mut [sample_row()], |r, k| r.kept = k).is_err());
        let mut page = HistoryPage::from_rows(&[sample_row(), sample_row()], false);
        page.kept = KeptRows::get(&kept(&[1, 1]), &cx).unwrap();
        assert!(page.each_row(|_| {}).is_ok());
        page.kept = KeptRows::get(&kept(&[2, 1]), &cx).unwrap();
        assert!(page.each_row(|_| {}).is_err());
        let mut rows = vec![sample_row(), sample_row()];
        assert!(kept::get(&mut rows, &kept(&[1, 3]), &cx).is_ok());
        assert!(rows[1].kept.extended && rows[1].kept.hyperlink);
        assert!(kept::get(&mut rows, &kept(&[2, 3]), &cx).is_err());
    }

    #[test]
    fn blank_rows_are_run_length_encoded() {
        let blank = Row {
            cells: vec![Cell::default(); 200],
            ..Row::default()
        };
        // flags, ncells (2 bytes), one run header (2), attrs mask, text.
        assert!(blank.encode().len() <= 8, "{:?}", blank.encode());
        let mut text = blank.clone();
        for (c, ch) in text.cells.iter_mut().zip("abcdefgh".chars()) {
            c.codepoint = ch as u32;
        }
        // Two bytes a letter, then the blanks as one run.
        assert!(text.encode().len() <= 8 + 2 * 8 + 4, "{:?}", text.encode());
        assert_eq!(Row::decode(&text.encode()).unwrap(), text);
    }

    #[test]
    fn runs_alternate_literal_and_uniform_spans() {
        let mut row = Row {
            cells: vec![Cell::default(); 12],
            ..Row::default()
        };
        // a b b b c d e e e e f g
        for (c, ch) in row.cells.iter_mut().zip("abbbcdeeeefg".chars()) {
            c.codepoint = ch as u32;
        }
        assert_eq!(Row::decode(&row.encode()).unwrap(), row);
    }

    #[test]
    fn rows_reject_corruption() {
        let bytes = sample_row().encode();
        for cut in 0..bytes.len() {
            assert!(Row::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(Row::decode(&long).is_err());
        // A width above MAX_COLS.
        let mut wide = Vec::new();
        put_varint(&mut wide, 0);
        put_varint(&mut wide, MAX_COLS as u64 + 1);
        assert!(Row::decode(&wide).is_err());
        // A surrogate codepoint.
        let mut bad = Vec::new();
        put_varint(&mut bad, 0);
        put_varint(&mut bad, 1);
        put_varint(&mut bad, 1 << 2);
        put_varint(&mut bad, 0);
        put_varint(&mut bad, 0xD800 << 2);
        assert!(Row::decode(&bad).is_err());
        // A link index with no links.
        let mut link = Vec::new();
        put_varint(&mut link, 0);
        put_varint(&mut link, 1);
        put_varint(&mut link, 1 << 2);
        put_varint(&mut link, A_LINK);
        put_varint(&mut link, 1);
        put_varint(&mut link, 0);
        assert!(Row::decode(&link).is_err());
    }

    #[test]
    fn page_round_trips_and_names_its_content() {
        let rows: Vec<Row> = (0..100)
            .map(|i| {
                let mut r = sample_row();
                r.time = i;
                r
            })
            .collect();
        let page = HistoryPage::from_rows(&rows, true);
        assert_eq!(page.rows, 100);
        assert!(page.compressed);
        assert!(page.verified());
        assert!(page.data.len() < usize::try_from(page.raw_len).unwrap());
        assert_eq!(page.decode_rows().unwrap(), rows);
        // Same rows, same name; different rows, different name.
        assert_eq!(HistoryPage::from_rows(&rows, true).hash, page.hash);
        assert_ne!(HistoryPage::from_rows(&rows[1..], true).hash, page.hash);

        let cx = Ctx::new(1);
        let mut out = Vec::new();
        page.put(&mut out);
        assert_eq!(HistoryPage::get(&out, &cx).unwrap(), page);
        let mut by_ref = Vec::new();
        page.put_with(&mut by_ref, false);
        let named = HistoryPage::get(&by_ref, &cx).unwrap();
        assert!(named.data.is_empty() && named.hash == page.hash);
        assert!(matches!(named.decode_rows(), Err(Error::MissingChunk(_))));
    }

    #[test]
    fn page_rejects_a_bad_block() {
        let mut page = HistoryPage::from_rows(&[sample_row()], false);
        page.data[0] ^= 0xff;
        assert!(!page.verified());
        let mut short = HistoryPage::from_rows(&[sample_row()], false);
        short.rows = 2;
        assert!(short.decode_rows().is_err());
        let mut huge = HistoryPage::from_rows(&[sample_row()], false);
        huge.raw_len = MAX_PAGE_BYTES + 1;
        assert!(huge.decode_rows().is_err());
    }

    #[test]
    fn a_page_claiming_endless_rows_fails_before_allocating_for_them() {
        // `rows` is the manifest's claim, not covered by the page's hash.
        let mut page = HistoryPage::from_rows(&[Row::default()], false);
        page.rows = u32::MAX;
        assert!(page.decode_rows().is_err());
        let many = vec![Row::default(); MAX_PAGE_ROWS as usize + 1];
        assert!(HistoryPage::from_rows(&many, false).decode_rows().is_err());
        let most = HistoryPage::from_rows(&many[1..], false);
        assert_eq!(most.decode_rows().unwrap().len(), MAX_PAGE_ROWS as usize);
    }

    #[test]
    fn a_page_of_too_many_cells_is_refused_and_the_builder_stops_short_of_it() {
        // A few hundred bytes compressed, billions of cells unbounded.
        let blank = Row {
            cells: vec![Cell::default(); MAX_COLS],
            ..Row::default()
        };
        let mut b = PageBuilder::new();
        while b.fits(&blank) {
            b.push(&blank);
        }
        assert_eq!(
            b.cells(),
            MAX_PAGE_CELLS / MAX_COLS as u64 * MAX_COLS as u64
        );
        let full = b.finish(false);
        assert_eq!(full.decode_rows().unwrap().len(), full.rows as usize);
        let mut over = PageBuilder::new();
        for _ in 0..=full.rows {
            over.push(&blank);
        }
        let over = over.finish(false);
        assert!(over.data.len() < 4096);
        assert_eq!(
            over.each_row(|_| {}),
            Err(Error::Malformed("history page has too many cells"))
        );
        // The rows limit, and the bytes limit for rows that encode long.
        let mut b = PageBuilder::new();
        while b.fits(&Row::default()) {
            b.push(&Row::default());
        }
        assert_eq!(b.rows(), MAX_PAGE_ROWS);
        let mut b = PageBuilder::new();
        let long = Row {
            links: vec![Hyperlink {
                id: None,
                uri: "x".repeat(1 << 20).into(),
                implicit: 0,
            }],
            ..sample_row()
        };
        while b.fits(&long) {
            b.push(&long);
        }
        assert!(b.raw_len() as u64 <= MAX_PAGE_BYTES);
        assert!(b.finish(false).decode_rows().is_ok());
    }
}
