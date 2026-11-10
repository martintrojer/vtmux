//! D33: kitty graphics (spec *Protocol extensions (D28)*, D33). A granted
//! pane screen keeps its images and placements in Ghostty's storage
//! (`graphics_storage.zig`); this module reads them for the server, which
//! draws them as unicode placeholders through its cell compositor and
//! uploads each image to the clients whose frames show it.
//!
//! The pane side (the grant, the storage limit, the replies) is in
//! `term.rs`; here are what the server reads and what it writes to an
//! outer terminal: uploads, virtual placements and deletes, in the form
//! the kitty graphics protocol gives them
//! (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>).

use std::fmt::Write as _;

use libghostty_vt::alloc::{Allocator, Bytes};
use libghostty_vt::kitty::graphics as kg;

use crate::snapshot::{Cell, Color};
use crate::term::PaneTerm;
use crate::text::CellText;

/// The unicode placeholder (`graphics_unicode.zig`:16): a cell of an image.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// The row, column and image id high byte diacritics, in order: the
/// number is the index (`graphics_unicode.zig` `diacritics`, kitty's
/// `rowcolumn-diacritics.txt`). Sorted.
pub static DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

/// The diacritic for `n`, if there is one.
#[must_use]
pub fn diacritic(n: u32) -> Option<char> {
    DIACRITICS.get(usize::try_from(n).ok()?).copied()
}

/// The number diacritic `c` stands for.
#[must_use]
pub fn diacritic_index(c: char) -> Option<u32> {
    DIACRITICS
        .binary_search(&c)
        .ok()
        .and_then(|i| u32::try_from(i).ok())
}

/// The kitty image id a colour encodes (`colorToId`,
/// `graphics_unicode.zig`): the 24 bits of an RGB colour, a palette index.
fn colour_id(c: Color) -> u32 {
    match c {
        Color::Default => 0,
        Color::Palette(n) | Color::Palette256(n) => u32::from(n),
        Color::Rgb(r, g, b) => u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
    }
}

/// The colour that encodes the low 24 bits of `id`.
#[must_use]
pub fn id_colour(id: u32) -> Color {
    let [_, r, g, b] = id.to_be_bytes();
    Color::Rgb(r, g, b)
}

/// One placeholder cell, resolved: which image (its full id, the high byte
/// included) and placement it shows, and which cell of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceholderCell {
    pub image: u32,
    pub placement: u32,
    pub row: u32,
    pub col: u32,
}

/// What one placeholder cell says of itself (`IncompletePlacement.init`).
#[derive(Clone, Copy)]
struct Partial {
    low: u32,
    placement: u32,
    row: Option<u32>,
    col: Option<u32>,
    high: Option<u32>,
}

fn partial(cell: &Cell) -> Option<Partial> {
    let mut chars = cell.text.chars();
    if chars.next() != Some(PLACEHOLDER) {
        return None;
    }
    let mut marks = chars.map(diacritic_index);
    let row = marks.next().flatten();
    let col = marks.next().flatten();
    let high = marks.next().flatten().filter(|&h| h <= 0xff);
    Some(Partial {
        low: colour_id(cell.style.fg),
        placement: colour_id(cell.style.underline_color),
        row,
        col,
        high,
    })
}

/// Whether `cell` is a placeholder cell.
#[must_use]
pub fn is_placeholder(cell: &Cell) -> bool {
    cell.text.starts_with(PLACEHOLDER)
}

/// A row's placeholder cells resolved as Ghostty's renderer and kitty
/// resolve them (`IncompletePlacement.start`, `append`): a cell that
/// leaves out its row, column or high byte continues the run to its left
/// when it can, and starts a run at row and column 0 when it cannot.
#[must_use]
pub fn resolve_row(cells: &[Cell]) -> Vec<Option<PlaceholderCell>> {
    let mut out = Vec::with_capacity(cells.len());
    // The run so far: its first cell, filled in, and its width.
    let mut run: Option<(Partial, u32)> = None;
    for cell in cells {
        let Some(p) = partial(cell) else {
            if cell.width != 0 {
                run = None;
            }
            out.push(None);
            continue;
        };
        let joins = run.is_some_and(|(r, width)| {
            r.low == p.low
                && r.placement == p.placement
                && p.row.is_none_or(|row| Some(row) == r.row)
                && p.col
                    .is_none_or(|col| Some(col) == r.col.map(|c| c + width))
                && p.high.is_none_or(|h| Some(h) == r.high)
        });
        let (first, at) = match run {
            Some((r, width)) if joins => {
                run = Some((r, width + 1));
                (r, width)
            }
            _ => {
                let start = Partial {
                    row: Some(p.row.unwrap_or(0)),
                    col: Some(p.col.unwrap_or(0)),
                    ..p
                };
                run = Some((start, 1));
                (start, 0)
            }
        };
        out.push(Some(PlaceholderCell {
            image: first.low | first.high.unwrap_or(0) << 24,
            placement: first.placement,
            row: first.row.unwrap_or(0),
            col: first.col.unwrap_or(0) + at,
        }));
    }
    out
}

/// A placeholder cell's text for cell (`row`, `col`) of image `id`: all
/// three diacritics, so it stands on its own.
#[must_use]
pub fn placeholder_text(id: u32, row: u32, col: u32) -> Option<CellText> {
    let mut t = CellText::default();
    t.push(PLACEHOLDER);
    t.push(diacritic(row)?);
    t.push(diacritic(col)?);
    t.push(diacritic(id >> 24)?);
    Some(t)
}

/// One placement of the active screen, as the server draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The image's id in the screen's storage.
    pub image: u32,
    /// Its generation: a new one when the image was sent again.
    pub generation: u64,
    pub placement: u32,
    /// A virtual placement (`U=1`): the program writes its cells itself.
    pub is_virtual: bool,
    /// A direct placement's top-left cell, `(col, row)`, the row relative
    /// to the active area's top (negative: in history).
    pub at: Option<(i32, i32)>,
    /// The cells it covers.
    pub cols: u32,
    pub rows: u32,
    /// The part of the image shown, in pixels: `(x, y, width, height)`.
    pub source: (u32, u32, u32, u32),
    /// The image's size in pixels.
    pub size: (u32, u32),
    pub z: i32,
}

/// An image's pixels as an outer terminal takes them: 8-bit RGB or RGBA
/// (`f=24`, `f=32`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub alpha: bool,
    pub data: Vec<u8>,
}

impl PaneTerm {
    /// The active screen's storage generation (`Graphics::generation`): a
    /// new number whenever an image or placement is added or deleted. 0
    /// for a storage never used.
    #[must_use]
    pub fn graphics_generation(&self) -> u64 {
        self.term
            .kitty_graphics()
            .and_then(|g| g.generation())
            .unwrap_or(0)
    }

    /// The active screen's placements, in no order.
    #[must_use]
    pub fn graphics_placements(&self) -> Vec<Placement> {
        let mut out = Vec::new();
        let Ok(g) = self.term.kitty_graphics() else {
            return out;
        };
        if g.generation().unwrap_or(0) == 0 {
            return out;
        }
        let Ok(mut iter) = kg::PlacementIterator::new() else {
            return out;
        };
        let Ok(mut each) = iter.update(&g) else {
            return out;
        };
        while let Some(p) = each.next() {
            let Ok(id) = p.image_id() else { continue };
            let Some(image) = g.image(id) else { continue };
            let is_virtual = p.is_virtual().unwrap_or(false);
            let at = if is_virtual {
                None
            } else {
                match p.vtmux_active_pos(&self.term) {
                    Ok(Some(pos)) => Some((pos.col, pos.row)),
                    // A placement with no place (a chain rooted at a
                    // virtual one) draws nothing.
                    _ => continue,
                }
            };
            let grid = p.grid_size(&image, &self.term).unwrap_or_default();
            let source = p.source_rect(&image).unwrap_or_default();
            out.push(Placement {
                image: id,
                generation: image.generation().unwrap_or(0),
                placement: p.placement_id().unwrap_or(0),
                is_virtual,
                at,
                cols: grid.cols,
                rows: grid.rows,
                source: (source.x, source.y, source.width, source.height),
                size: (image.width().unwrap_or(0), image.height().unwrap_or(0)),
                z: p.z().unwrap_or(0),
            });
        }
        out
    }

    /// The generation of image `id` of the active screen, if it has one.
    #[must_use]
    pub fn graphics_image_generation(&self, id: u32) -> Option<u64> {
        let g = self.term.kitty_graphics().ok()?;
        g.image(id)?.generation().ok()
    }

    /// Image `id` of the active screen: its generation and pixels, as
    /// RGB or RGBA. `None` for an image not loaded yet (a PNG is decoded
    /// to RGBA when it loads).
    #[must_use]
    pub fn graphics_image(&self, id: u32) -> Option<(u64, Pixels)> {
        let g = self.term.kitty_graphics().ok()?;
        let image = g.image(id)?;
        if image.compression().ok()? != kg::Compression::None {
            return None;
        }
        let (width, height) = (image.width().ok()?, image.height().ok()?);
        let data = image.data().ok()?;
        let (alpha, data) = match image.format().ok()? {
            kg::ImageFormat::Rgb => (false, data.to_vec()),
            kg::ImageFormat::Rgba => (true, data.to_vec()),
            kg::ImageFormat::Gray => (false, data.iter().flat_map(|&v| [v, v, v]).collect()),
            kg::ImageFormat::GrayAlpha => (
                true,
                data.as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|&[v, a]| [v, v, v, a])
                    .collect(),
            ),
            _ => return None,
        };
        let want = usize::try_from(width).ok()?
            * usize::try_from(height).ok()?
            * if alpha { 4 } else { 3 };
        if data.len() != want {
            return None;
        }
        Some((
            image.generation().ok()?,
            Pixels {
                width,
                height,
                alpha,
                data,
            },
        ))
    }
}

/// Base64 (RFC 4648, padded), what the protocol's payloads are in.
fn base64(data: &[u8], out: &mut String) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(T[(n >> shift & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
}

/// The base64 payload bytes one escape carries (kitty's limit for a chunk).
const CHUNK: usize = 4096;

/// An upload of `px` as image `id` for an outer terminal: zlib-compressed
/// (`o=z`), in chunks (`m=1` until the last), `q=2` so it never replies.
#[must_use]
pub fn upload(id: u32, px: &Pixels) -> Vec<u8> {
    use std::io::Write as _;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = z.write_all(&px.data);
    let packed = z.finish().unwrap_or_default();
    let mut payload = String::new();
    base64(&packed, &mut payload);
    let f = if px.alpha { 32 } else { 24 };
    let mut out = String::with_capacity(payload.len() + payload.len() / CHUNK * 16 + 64);
    let chunks: Vec<&str> = payload
        .as_bytes()
        .chunks(CHUNK)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    let last = chunks.len().saturating_sub(1);
    for (i, c) in chunks.iter().enumerate() {
        let more = u8::from(i < last);
        if i == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=t,f={f},s={},v={},o=z,i={id},q=2,m={more};{c}\x1b\\",
                px.width, px.height
            );
        } else {
            let _ = write!(out, "\x1b_Gm={more},q=2;{c}\x1b\\");
        }
    }
    if chunks.is_empty() {
        let _ = write!(
            out,
            "\x1b_Ga=t,f={f},s={},v={},o=z,i={id},q=2;\x1b\\",
            px.width, px.height
        );
    }
    out.into_bytes()
}

/// A virtual placement (`a=p,U=1`) of image `id` over `cols` x `rows`
/// cells, showing `source` of it when that is not the whole image.
#[must_use]
pub fn place(
    id: u32,
    placement: u32,
    cols: u32,
    rows: u32,
    source: Option<(u32, u32, u32, u32)>,
) -> Vec<u8> {
    let mut out = format!("\x1b_Ga=p,U=1,i={id}");
    if placement != 0 {
        let _ = write!(out, ",p={placement}");
    }
    let _ = write!(out, ",c={cols},r={rows}");
    if let Some((x, y, w, h)) = source {
        let _ = write!(out, ",x={x},y={y},w={w},h={h}");
    }
    out.push_str(",q=2\x1b\\");
    out.into_bytes()
}

/// Delete image `id` and its placements, freeing its data (`d=I`).
#[must_use]
pub fn free(id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\").into_bytes()
}

/// Delete every image with an id from `first` to `last` (`d=R`,
/// `graphics_command.zig`:1172).
#[must_use]
pub fn free_range(first: u32, last: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=R,x={first},y={last},q=2\x1b\\").into_bytes()
}

/// Ghostty's PNG decoder hook (graphics.rs `set_png_decoder`): PNGs to
/// 8-bit RGBA, with the `png` crate.
struct PngDecoder;

/// The largest image a PNG may decode to, in bytes (kitty's own cap is
/// 400 MB; Ghostty's storage limit applies after).
const PNG_LIMIT: usize = 400_000_000;

impl kg::DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc Allocator<'_>,
        data: &[u8],
    ) -> Option<kg::DecodedImage<'alloc>> {
        let rgba = decode_png(data)?;
        let mut bytes = Bytes::new_with_alloc(alloc, rgba.data.len()).ok()?;
        bytes.copy_from_slice(&rgba.data);
        Some(kg::DecodedImage {
            width: rgba.width,
            height: rgba.height,
            data: bytes,
        })
    }
}

/// A PNG as 8-bit RGBA.
fn decode_png(data: &[u8]) -> Option<Pixels> {
    use png::{ColorType, Transformations};
    let mut d =
        png::Decoder::new_with_limits(std::io::Cursor::new(data), png::Limits { bytes: PNG_LIMIT });
    d.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut r = d.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let data = match info.color_type {
        ColorType::Rgba => buf,
        ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[v, a]| [v, v, v, a])
            .collect(),
        ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        ColorType::Indexed => return None,
    };
    Some(Pixels {
        width: info.width,
        height: info.height,
        alpha: true,
        data,
    })
}

thread_local! {
    static DECODER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install the PNG decoder on this thread, once (Ghostty keeps it per
/// thread, and only a granted screen decodes).
pub(crate) fn install_png_decoder() {
    if !DECODER.get() {
        DECODER.set(kg::set_png_decoder(Some(Box::new(PngDecoder))).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, fg: Color) -> Cell {
        Cell {
            text: text.into(),
            width: 1,
            style: crate::Style {
                fg,
                ..crate::Style::default()
            },
            ..Cell::default()
        }
    }

    /// kitty's rules (`graphics_unicode.zig` `IncompletePlacement`): a cell
    /// with no diacritics continues the run to its left, one whose row
    /// differs starts a new run, the third diacritic is the high byte.
    #[test]
    fn placeholder_runs_resolve_as_kitty_does() {
        let d = |n: u32| diacritic(n).unwrap();
        let p = PLACEHOLDER;
        let red = Color::Rgb(0, 0, 5);
        let row = [
            cell(&format!("{p}{}{}{}", d(2), d(3), d(1)), red),
            cell(&p.to_string(), red),
            cell(&format!("{p}{}", d(2)), red),
            cell("x", Color::Default),
            cell(&p.to_string(), red),
            cell(&format!("{p}{}", d(4)), red),
        ];
        let got = resolve_row(&row);
        let at = |i: usize| got[i].map(|c| (c.image, c.row, c.col));
        assert_eq!(at(0), Some((0x0100_0005, 2, 3)));
        assert_eq!(at(1), Some((0x0100_0005, 2, 4)));
        assert_eq!(at(2), Some((0x0100_0005, 2, 5)));
        assert_eq!(at(3), None);
        assert_eq!(at(4), Some((5, 0, 0)), "a run starts at 0, 0");
        assert_eq!(at(5), Some((5, 4, 0)), "another row starts a run");
        assert_eq!(diacritic_index(d(296)), Some(296));
        assert_eq!(diacritic(297), None);
    }

    #[test]
    fn base64_pads_as_rfc_4648() {
        for (raw, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
        ] {
            let mut out = String::new();
            base64(raw.as_bytes(), &mut out);
            assert_eq!(out, want);
        }
    }

    /// An upload is chunked at 4096 payload bytes, the first chunk with
    /// the image's keys, the rest with `m` and `q` alone.
    #[test]
    fn an_upload_is_chunked_quiet_and_compressed() {
        let px = Pixels {
            width: 64,
            height: 64,
            alpha: true,
            // Noise, so compression leaves it over one chunk.
            data: (0..64 * 64 * 4)
                .scan(1u64, |x, _| {
                    *x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    Some(x.to_be_bytes()[0])
                })
                .collect(),
        };
        let up = String::from_utf8(upload(0xEE00_0001, &px)).unwrap();
        let escapes: Vec<&str> = up.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert!(escapes.len() > 1, "{}", escapes.len());
        assert!(escapes[0].starts_with("\x1b_Ga=t,f=32,s=64,v=64,o=z,i=3992977409,q=2,m=1;"));
        for e in &escapes[1..escapes.len() - 1] {
            assert!(e.starts_with("\x1b_Gm=1,q=2;"), "{e:?}");
        }
        assert!(escapes.last().unwrap().starts_with("\x1b_Gm=0,q=2;"));
        assert_eq!(
            String::from_utf8(place(7, 0, 3, 2, None)).unwrap(),
            "\x1b_Ga=p,U=1,i=7,c=3,r=2,q=2\x1b\\"
        );
    }

    /// A PNG of each colour type decodes to RGBA.
    #[test]
    fn pngs_decode_to_rgba() {
        let encode = |ct: png::ColorType, data: &[u8]| {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, 2, 1);
            e.set_color(ct);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(data).unwrap();
            out
        };
        let rgb = decode_png(&encode(png::ColorType::Rgb, &[1, 2, 3, 4, 5, 6])).unwrap();
        assert_eq!(rgb.data, [1, 2, 3, 255, 4, 5, 6, 255]);
        let gray = decode_png(&encode(png::ColorType::Grayscale, &[9, 8])).unwrap();
        assert_eq!(gray.data, [9, 9, 9, 255, 8, 8, 8, 255]);
        let rgba = decode_png(&encode(png::ColorType::Rgba, &[1, 2, 3, 4, 5, 6, 7, 8])).unwrap();
        assert_eq!((rgba.width, rgba.height, rgba.data.len()), (2, 1, 8));
        assert!(decode_png(b"not a png").is_none());
    }

    /// One RGB pixel, red, as base64.
    const RED: &str = "/wAA";

    fn term_with(option: bool) -> PaneTerm {
        let mut t = PaneTerm::new(20, 5, 100);
        t.set_graphics_share(1_000_000);
        t.set_graphics(option);
        t
    }

    /// D33 off: a graphics command is an APC as tmux takes it, a title
    /// (`input_exit_apc`), and gets no reply; Ghostty stores nothing.
    #[test]
    fn off_a_graphics_command_is_a_title() {
        let mut t = term_with(false);
        t.feed(
            format!("\x1b_Ga=T,f=24,s=1,v=1,i=1;{RED}\x1b\\\x1b_Ga=q,i=2,s=1,v=1,f=24;{RED}\x1b\\")
                .as_bytes(),
        );
        assert_eq!(t.take_replies(), b"");
        assert_eq!(t.title(), format!("Ga=q,i=2,s=1,v=1,f=24;{RED}"));
        assert_eq!(t.graphics_grants(), [false; 2]);
        assert_eq!(t.graphics_storage(false), (0, 0));
        assert_eq!(t.graphics_placements(), []);
    }

    /// D33 on: the first command grants the screen; Ghostty stores the
    /// image, places it and answers, honouring `q`; the title is left.
    #[test]
    fn on_a_graphics_command_is_ghosttys_and_answered() {
        let mut t = term_with(true);
        t.feed(b"\x1b]2;kept\x07");
        t.feed(format!("\x1b_Ga=T,f=24,s=1,v=1,i=1;{RED}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"\x1b_Gi=1;OK\x1b\\");
        assert_eq!(t.graphics_grants(), [true, false]);
        assert_eq!(t.take_graphics_grants(), [true, false]);
        assert_eq!(t.take_graphics_grants(), [false; 2]);
        t.feed(format!("\x1b_Ga=t,f=24,s=1,v=1,i=2,q=2;{RED}\x1b\\").as_bytes());
        t.feed(format!("\x1b_Ga=t,f=24,s=1,v=1,i=3,q=1;{RED}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"", "q=2 says nothing, q=1 no OK");
        t.feed(b"\x1b_Ga=t,f=24,s=1,v=1,i=4,q=0;AA\x1b\\");
        let err = String::from_utf8(t.take_replies()).unwrap();
        assert!(err.starts_with("\x1b_Gi=4;E"), "{err:?}");
        assert_eq!(t.title(), "kept");
        assert_eq!(t.graphics_storage(false), (9, 1_000_000));
        let p = t.graphics_placements();
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].image, p[0].at, p[0].cols, p[0].rows),
            (1, Some((0, 0)), 1, 1)
        );
        let (_, px) = t.graphics_image(1).unwrap();
        assert_eq!((px.width, px.alpha, px.data), (1, false, vec![255, 0, 0]));
    }

    /// D28 rule 5: a granted screen keeps its grant with the option off;
    /// a screen not granted gets tmux's behaviour. RIS ends both grants and
    /// frees the images.
    #[test]
    fn a_grant_outlives_the_option_and_ends_at_ris() {
        let mut t = term_with(true);
        t.feed(format!("\x1b_Ga=T,f=24,s=1,v=1,i=1;{RED}\x1b\\").as_bytes());
        t.take_replies();
        t.set_graphics(false);
        t.feed(b"\x1b_Ga=q,i=9,s=1,v=1,f=24;AAAA\x1b\\");
        assert_eq!(t.take_replies(), b"\x1b_Gi=9;OK\x1b\\", "still granted");
        t.feed(b"\x1b[?1049h\x1b_Ga=q,i=9,s=1,v=1,f=24;AAAA\x1b\\");
        assert_eq!(t.take_replies(), b"", "the alternate screen is not");
        assert_eq!(t.title(), "Ga=q,i=9,s=1,v=1,f=24;AAAA");
        t.feed(b"\x1b[?1049l\x1bc");
        assert_eq!(t.graphics_grants(), [false; 2]);
        assert_eq!(t.graphics_storage(false).0, 0, "RIS frees the images");
        t.feed(b"\x1b_Ga=q,i=9,s=1,v=1,f=24;AAAA\x1b\\");
        assert_eq!(t.take_replies(), b"", "an APC is a title again");
    }

    /// A private directory for a test's files, removed by the caller.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vtmux-gfx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// D33: images arrive only as data in the escape. A file, temporary
    /// file or shared memory transmission is refused with one answer
    /// whatever its path names, a readable file of the right size (`S`),
    /// none, a directory or a FIFO, so no reply says which files exist;
    /// no path is opened (a FIFO would block the server until a writer
    /// came), and a temporary file is not deleted.
    #[test]
    fn only_direct_media_load_and_no_path_is_opened() {
        let dir = scratch("media");
        let file = dir.join("tty-graphics-protocol-red");
        std::fs::write(&file, [255, 0, 0]).unwrap();
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let missing = dir.join("missing");
        let (tx, rx) = std::sync::mpsc::channel();
        let paths = [&file, &missing, &dir, &fifo].map(|p| p.to_str().unwrap().to_owned());
        std::thread::spawn(move || {
            let mut t = term_with(true);
            let mut replies = Vec::new();
            let mut slowest = std::time::Duration::ZERO;
            for (n, path) in (1..).zip(&paths) {
                for medium in ['f', 't', 's'] {
                    let mut b64 = String::new();
                    base64(path.as_bytes(), &mut b64);
                    let start = std::time::Instant::now();
                    t.feed(
                        format!("\x1b_Ga=t,t={medium},f=24,s=1,v=1,S=3,i={n};{b64}\x1b\\")
                            .as_bytes(),
                    );
                    slowest = slowest.max(start.elapsed());
                    let reply = String::from_utf8(t.take_replies()).unwrap();
                    replies.push(reply.replacen(&format!("i={n};"), "i=N;", 1));
                }
            }
            let _ = tx.send((replies, slowest, t.graphics_storage(false).0));
        });
        let (replies, slowest, stored) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a FIFO path blocked the pane's parse");
        assert!(
            slowest < std::time::Duration::from_millis(100),
            "{slowest:?}"
        );
        assert_eq!(replies.len(), 12);
        for reply in &replies {
            assert_eq!(reply, "\x1b_Gi=N;EINVAL: unsupported medium\x1b\\");
        }
        assert_eq!(stored, 0);
        assert!(file.exists(), "a temporary file is never read, nor deleted");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `n` zero bytes of image data, as base64 chunks of kitty's size.
    fn zero_chunks(n: usize) -> Vec<String> {
        let mut b64 = String::new();
        base64(&vec![0; n], &mut b64);
        b64.as_bytes()
            .chunks(CHUNK)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect()
    }

    /// D33: a pending (chunked, unfinished) load never holds more than its
    /// screen's limit: at the chunk past it the load lets go of its data,
    /// its other chunks go unanswered, and its last one fails as Ghostty
    /// fails an image over the limit (`ENOMEM`, honouring `q`). One command
    /// longer than the limit could store is dropped unparsed.
    #[test]
    fn a_pending_load_is_held_to_the_screens_limit() {
        let mut t = term_with(true);
        // 1000x1000 RGB is 3 MB; the limit is 1 MB.
        let chunks = zero_chunks(3_000_000);
        t.feed(format!("\x1b_Ga=t,f=24,s=1000,v=1000,i=1,m=1;{}\x1b\\", chunks[0]).as_bytes());
        let mut most = 0;
        for chunk in &chunks[1..chunks.len() - 1] {
            t.feed(format!("\x1b_Gm=1;{chunk}\x1b\\").as_bytes());
            assert_eq!(t.take_replies(), b"");
            most = most.max(t.graphics_pending(false));
            assert!(most <= 1_000_000, "{most}");
        }
        assert!(most > 900_000, "it held what fits: {most}");
        assert_eq!(t.graphics_pending(false), 0, "then let go");
        t.feed(format!("\x1b_Gm=0;{}\x1b\\", chunks[chunks.len() - 1]).as_bytes());
        assert_eq!(t.take_replies(), b"\x1b_Gi=1;ENOMEM: out of memory\x1b\\");
        assert_eq!(
            (t.graphics_pending(false), t.graphics_storage(false).0),
            (0, 0)
        );
        // A load that fits still loads after.
        t.feed(format!("\x1b_Ga=t,f=24,s=1,v=1,i=2;{RED}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"\x1b_Gi=2;OK\x1b\\");
        // One unchunked command of 2 MB: dropped as it is read.
        let big = zero_chunks(2_000_000).concat();
        t.feed(format!("\x1b_Ga=t,f=24,s=1000,v=666,i=3;{big}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"");
        assert_eq!(t.graphics_storage(false).0, 3, "only image 2");
        // A dropped pending load: the next chunk starts anew.
        t.feed(
            format!(
                "\x1b_Ga=t,f=24,s=10,v=10,i=4,m=1;{}\x1b\\",
                zero_chunks(150)[0]
            )
            .as_bytes(),
        );
        assert!(t.graphics_pending(false) >= 150);
        t.drop_graphics_pending(false);
        assert_eq!(t.graphics_pending(false), 0);
    }

    /// D33: a load's first chunk is held to its screen's limit too, though
    /// one command may carry more (another screen's limit, or the share, is
    /// larger). Past it, it is not kept: alone it fails at once, the first
    /// of several fails at the last; one that fits costs its size.
    #[test]
    fn a_first_chunk_past_the_limit_is_not_held() {
        let mut t = term_with(true);
        t.feed(format!("\x1b_Ga=t,f=24,s=1,v=1,i=1;{RED}\x1b\\").as_bytes());
        t.set_graphics_limit(false, 100_000);
        t.take_replies();
        let big = zero_chunks(300_000).concat();
        t.feed(format!("\x1b_Ga=t,f=24,s=1000,v=100,i=2,m=1;{big}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"");
        assert_eq!(t.graphics_pending(false), 0, "nothing held");
        t.feed(format!("\x1b_Gm=1;{RED}\x1b\\").as_bytes());
        assert_eq!((t.take_replies(), t.graphics_pending(false)), (vec![], 0));
        t.feed(format!("\x1b_Gm=0;{RED}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"\x1b_Gi=2;ENOMEM: out of memory\x1b\\");
        t.feed(format!("\x1b_Ga=t,f=24,s=1000,v=100,i=3;{big}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"\x1b_Gi=3;ENOMEM: out of memory\x1b\\");
        assert_eq!(t.graphics_storage(false), (3, 100_000), "only image 1");
        let fits = zero_chunks(50_001).concat();
        t.feed(format!("\x1b_Ga=t,f=24,s=1000,v=100,i=4,m=1;{fits}\x1b\\").as_bytes());
        assert_eq!(t.graphics_pending(false), 50_001, "a buffer of its size");
    }

    /// A PNG (`f=100`) loads through the decoder vtmux installs.
    #[test]
    fn a_png_is_decoded() {
        let mut png = Vec::new();
        let mut e = png::Encoder::new(&mut png, 2, 1);
        e.set_color(png::ColorType::Rgb);
        e.write_header()
            .unwrap()
            .write_image_data(&[1, 2, 3, 4, 5, 6])
            .unwrap();
        let mut b64 = String::new();
        base64(&png, &mut b64);
        let mut t = term_with(true);
        t.feed(format!("\x1b_Ga=t,f=100,i=5;{b64}\x1b\\").as_bytes());
        assert_eq!(t.take_replies(), b"\x1b_Gi=5;OK\x1b\\");
        let (_, px) = t.graphics_image(5).unwrap();
        assert_eq!((px.width, px.height, px.alpha), (2, 1, true));
        assert_eq!(px.data, [1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
