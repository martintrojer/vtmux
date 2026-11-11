//! D33's client side: panes' kitty graphics drawn as unicode placeholders
//! through the cell compositor (spec, *Protocol extensions (D28)*, D33).
//!
//! Each turn the server reads every graphics pane's placements once
//! ([`Tick::build`]). A pane's content, for a client that draws graphics,
//! is then its cells with two changes ([`overlay`]): the program's own
//! placeholder cells (virtual placements, `U=1`) carry vtmux's image ids,
//! and a direct placement becomes placeholder cells over its grid rect.
//! Everything else (scrolling, clipping at pane edges, zoom, popups and
//! menus over an image, window switches) is the compositor's as for any
//! cell. Ids number each (pane, screen, image, generation) from one range
//! with the high byte set, the same for every client, so clients with one
//! cell size still share frames.
//!
//! A client keeps its own record of what its terminal holds
//! ([`ClientGraphics`]): before a frame, the images and virtual placements
//! its screen shows that it lacks are uploaded; after it, an image its
//! screen no longer shows is freed. At attach (the re-exec of a live
//! upgrade included) the whole range is freed first.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash as _, Hasher as _};

use vtmux_core::ids::PaneId;
use vtmux_vt::graphics::{self as g, Placement};
use vtmux_vt::{Cell, Snapshot};

use crate::model::Model;

/// vtmux's image ids: the high byte is 0xEE (the third diacritic), so
/// they are far from the small ids programs pick and from a 24-bit colour.
pub const FIRST: u32 = 0xEE00_0000;
pub const LAST: u32 = 0xEEFF_FFFF;

/// The placement ids vtmux gives a direct placement's cells: high in the
/// 24 bits an underline colour carries, away from the small ids programs
/// give virtual placements.
const DIRECT_PLACEMENT: u32 = 0x00F0_0000;

/// Where the pixels of one of vtmux's image ids are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageKey {
    pub pane: PaneId,
    pub alternate: bool,
    pub image: u32,
    pub generation: u64,
}

/// The server's numbering of images, kept across turns: one id per
/// [`ImageKey`] while the image is placed.
#[derive(Debug, Default)]
pub struct Ids {
    next: u32,
    by_key: HashMap<ImageKey, u32>,
    by_id: HashMap<u32, ImageKey>,
}

impl Ids {
    fn id(&mut self, key: ImageKey) -> u32 {
        if let Some(&id) = self.by_key.get(&key) {
            return id;
        }
        let span = LAST - FIRST + 1;
        loop {
            let id = FIRST + self.next % span;
            self.next = self.next.wrapping_add(1);
            if !self.by_id.contains_key(&id) {
                self.by_key.insert(key, id);
                self.by_id.insert(id, key);
                return id;
            }
        }
    }

    fn find(&self, key: ImageKey) -> Option<u32> {
        self.by_key.get(&key).copied()
    }

    /// Where image `id`'s pixels are.
    #[must_use]
    pub fn key(&self, id: u32) -> Option<ImageKey> {
        self.by_id.get(&id).copied()
    }

    /// Forget the images not placed this turn.
    fn keep(&mut self, seen: &BTreeSet<ImageKey>) {
        self.by_key.retain(|k, _| seen.contains(k));
        self.by_id.retain(|_, k| seen.contains(k));
    }
}

/// A virtual placement a client's terminal must hold for the cells of an
/// image and placement id: its cells and the part of the image shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub cols: u32,
    pub rows: u32,
    pub source: Option<(u32, u32, u32, u32)>,
}

/// A direct placement drawn as placeholder cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Direct {
    pub id: u32,
    pub placement: u32,
    /// Top-left cell; the row relative to the active area's top.
    pub col: i32,
    pub row: i32,
    pub cols: u32,
    pub rows: u32,
    /// Below the text (negative `z`): only in cells with no text.
    pub below: bool,
}

/// One pane's graphics this turn.
#[derive(Debug, Default)]
pub struct PaneGraphics {
    /// The program's image id -> vtmux's, for its placeholder cells.
    pub ids: HashMap<u32, u32>,
    pub direct: Vec<Direct>,
    /// Copy mode's: the direct placements when the copy was taken, over
    /// the copy's rows.
    pub copy: Vec<Direct>,
}

/// Every pane's graphics this turn, and every virtual placement a client
/// may need.
#[derive(Debug, Default)]
pub struct Tick {
    pub panes: BTreeMap<PaneId, PaneGraphics>,
    pub places: BTreeMap<(u32, u32), Place>,
}

/// The source rect of `p` when it is not the whole image.
fn crop(p: &Placement) -> Option<(u32, u32, u32, u32)> {
    let whole = (0, 0, p.size.0, p.size.1);
    (p.source != whole && p.source.2 > 0 && p.source.3 > 0).then_some(p.source)
}

impl Tick {
    /// Read every graphics pane's placements on its active screen.
    #[must_use]
    pub fn build(model: &Model, ids: &mut Ids) -> Self {
        let mut tick = Self::default();
        let mut seen = BTreeSet::new();
        for (&pane, p) in model.panes.iter().filter(|(_, p)| p.graphics.held) {
            let alternate = p.term.alternate_on();
            let mut pg = PaneGraphics::default();
            let mut placements = p.term.graphics_placements();
            // One order, so a direct placement keeps its cells' id.
            placements.sort_by_key(|x| (x.image, x.placement, x.at));
            let mut unnamed = 0;
            for x in &placements {
                let key = ImageKey {
                    pane,
                    alternate,
                    image: x.image,
                    generation: x.generation,
                };
                seen.insert(key);
                let id = ids.id(key);
                let place = Place {
                    cols: x.cols,
                    rows: x.rows,
                    source: crop(x),
                };
                if x.is_virtual {
                    pg.ids.insert(x.image, id);
                    tick.places.insert((id, x.placement & 0xFF_FFFF), place);
                    continue;
                }
                let Some((col, row)) = x.at else { continue };
                if x.cols == 0 || x.rows == 0 {
                    continue;
                }
                let placement = if x.placement == 0 {
                    unnamed += 1;
                    DIRECT_PLACEMENT | 0x8_0000 | (unnamed & 0x7_FFFF)
                } else {
                    DIRECT_PLACEMENT | (x.placement & 0x7_FFFF)
                };
                tick.places.insert((id, placement), place);
                pg.direct.push(Direct {
                    id,
                    placement,
                    col,
                    row,
                    cols: x.cols,
                    rows: x.rows,
                    below: x.z < 0,
                });
            }
            if let Some(m) = p.modes.first().and_then(crate::mode::Mode::as_copy) {
                let (alternate, placed) = m.graphics();
                for (n, x) in (0u32..).zip(placed) {
                    let key = ImageKey {
                        pane,
                        alternate,
                        image: x.image,
                        generation: x.generation,
                    };
                    let (Some(id), Some((col, row))) = (ids.find(key), x.at) else {
                        continue;
                    };
                    let placement = DIRECT_PLACEMENT | 0x4_0000 | (n & 0x3_FFFF);
                    tick.places.insert(
                        (id, placement),
                        Place {
                            cols: x.cols,
                            rows: x.rows,
                            source: crop(x),
                        },
                    );
                    pg.copy.push(Direct {
                        id,
                        placement,
                        col,
                        row,
                        cols: x.cols,
                        rows: x.rows,
                        below: x.z < 0,
                    });
                }
            }
            tick.panes.insert(pane, pg);
        }
        ids.keep(&seen);
        tick
    }
}

/// Whether a cell shows text a placement below the text leaves alone.
fn has_text(c: &Cell) -> bool {
    !c.text.is_empty() && c.text.as_str() != " "
}

/// A placeholder cell of image `id`, placement `placement`, for the
/// image's cell (`row`, `col`), in `base`'s background.
fn placeholder(base: &Cell, id: u32, placement: u32, row: u32, col: u32) -> Option<Cell> {
    let mut style = vtmux_vt::Style {
        bg: base.style.bg,
        ..vtmux_vt::Style::default()
    };
    style.fg = g::id_colour(id);
    if placement != 0 {
        style.underline_color = g::id_colour(placement);
    }
    Some(Cell {
        text: g::placeholder_text(id, row, col)?,
        width: 1,
        style,
        acs: false,
        link: None,
        tab: false,
    })
}

/// A cell blanked in its background: a placeholder of an image the
/// client cannot be given, or half of a wide character cut by one.
fn blank(base: &Cell) -> Cell {
    let mut style = vtmux_vt::Style::default();
    style.bg = base.style.bg;
    Cell {
        text: " ".into(),
        width: 1,
        style,
        ..Cell::default()
    }
}

/// `content` as a client that draws graphics sees it, its row 0 being
/// row `top` relative to the active area (copy mode shows history), with
/// its row stamps: `None` when nothing changes.
#[must_use]
pub fn overlay(
    content: &Snapshot,
    stamps: &[u64],
    pg: &PaneGraphics,
    direct: &[Direct],
    top: i32,
) -> Option<(Snapshot, Vec<u64>)> {
    let cols = usize::from(content.cols);
    if cols == 0 {
        return None;
    }
    let rows_with = |y: usize| {
        content.cells[y * cols..(y + 1) * cols]
            .iter()
            .any(g::is_placeholder)
    };
    let placeholder_rows: Vec<usize> = (0..usize::from(content.rows))
        .filter(|&y| rows_with(y))
        .collect();
    let visible = |d: &Direct| {
        let first = i64::from(d.row) - i64::from(top);
        first < i64::from(content.rows) && first + i64::from(d.rows) > 0
    };
    if placeholder_rows.is_empty() && !direct.iter().any(visible) {
        return None;
    }
    let mut out = content.clone();
    let mut changed = vec![false; usize::from(content.rows)];
    // The program's own placeholder cells: vtmux's id, every diacritic.
    for &y in &placeholder_rows {
        let row = &mut out.cells[y * cols..(y + 1) * cols];
        let resolved = g::resolve_row(row);
        for (cell, r) in row.iter_mut().zip(resolved) {
            let Some(r) = r else { continue };
            *cell = pg
                .ids
                .get(&r.image)
                .and_then(|&id| placeholder(cell, id, r.placement, r.row, r.col))
                .unwrap_or_else(|| blank(cell));
        }
        changed[y] = true;
    }
    for d in direct.iter().filter(|d| visible(d)) {
        for r in 0..d.rows {
            let Ok(y) = usize::try_from(i64::from(d.row) + i64::from(r) - i64::from(top)) else {
                continue;
            };
            if y >= usize::from(content.rows) {
                break;
            }
            let row = &mut out.cells[y * cols..(y + 1) * cols];
            let x0 = i64::from(d.col);
            let x1 = (x0 + i64::from(d.cols)).min(i64::try_from(cols).unwrap_or(i64::MAX));
            for x in x0.max(0)..x1 {
                let (Ok(x), Ok(c)) = (usize::try_from(x), u32::try_from(x - x0)) else {
                    continue;
                };
                // A wide character's spacer is part of its text: covering
                // it would make the terminal erase the character.
                if d.below
                    && (has_text(&row[x])
                        || (row[x].width == 0
                            && x > 0
                            && row[x - 1].width == 2
                            && has_text(&row[x - 1])))
                {
                    continue;
                }
                let Some(cell) = placeholder(&row[x], d.id, d.placement, r, c) else {
                    continue;
                };
                // A wide character cut by the rect leaves a blank half.
                if row[x].width == 2 && x + 1 < cols && x + 1 >= usize::try_from(x1).unwrap_or(0) {
                    row[x + 1] = blank(&row[x + 1]);
                }
                if row[x].width == 0 && x > 0 && i64::try_from(x).unwrap_or(0) == x0.max(0) {
                    row[x - 1] = blank(&row[x - 1]);
                }
                row[x] = cell;
            }
            changed[y] = true;
        }
    }
    let stamps = restamp(stamps, &changed, &out);
    Some((out, stamps))
}

/// The row stamps of `out`, from `content`'s `stamps`: a row `changed`
/// gets one of its own.
fn restamp(stamps: &[u64], changed: &[bool], out: &Snapshot) -> Vec<u64> {
    if stamps.len() != changed.len() {
        return Vec::new();
    }
    let cols = usize::from(out.cols);
    stamps
        .iter()
        .zip(changed)
        .enumerate()
        .map(|(y, (&stamp, &c))| {
            if !c {
                return stamp;
            }
            let mut h = std::hash::DefaultHasher::new();
            stamp.hash(&mut h);
            for c in &out.cells[y * cols..(y + 1) * cols] {
                c.text.as_str().hash(&mut h);
                format!("{:?}{:?}", c.style.fg, c.style.underline_color).hash(&mut h);
            }
            h.finish() | 1 << 63
        })
        .collect()
}

/// `content` with the placeholder cells that name one of vtmux's image
/// ids blanked, with its row stamps: `None` when it has none. For what
/// reaches a client that draws graphics without [`overlay`] (a pane
/// holding no images, a mode's screen, a popup): those ids are vtmux's
/// alone, and a program printing one would show another pane's image,
/// another session's even. A program's own ids (through passthrough) are
/// left as written.
#[must_use]
pub fn scrub(content: &Snapshot, stamps: &[u64]) -> Option<(Snapshot, Vec<u64>)> {
    let cols = usize::from(content.cols);
    if cols == 0 || !content.cells.iter().any(g::is_placeholder) {
        return None;
    }
    let mut out = content.clone();
    let mut changed = vec![false; usize::from(content.rows)];
    for (y, row) in out.cells.chunks_mut(cols).enumerate() {
        if !row.iter().any(g::is_placeholder) {
            continue;
        }
        let resolved = g::resolve_row(row);
        for (cell, r) in row.iter_mut().zip(resolved) {
            if r.is_some_and(|r| (FIRST..=LAST).contains(&r.image)) {
                *cell = blank(cell);
                changed[y] = true;
            }
        }
    }
    if !changed.contains(&true) {
        return None;
    }
    let stamps = restamp(stamps, &changed, &out);
    Some((out, stamps))
}

/// What one placeholder cell of a composed screen shows: vtmux's image
/// id (full, from its colour and third diacritic) and placement id.
fn shown(cell: &Cell) -> Option<(u32, u32)> {
    let mut chars = cell.text.chars();
    if chars.next() != Some(g::PLACEHOLDER) {
        return None;
    }
    let high = chars.nth(2).and_then(g::diacritic_index)?;
    let low = match cell.style.fg {
        vtmux_vt::Color::Rgb(r, gr, b) => u32::from(r) << 16 | u32::from(gr) << 8 | u32::from(b),
        _ => return None,
    };
    let placement = match cell.style.underline_color {
        vtmux_vt::Color::Rgb(r, gr, b) => u32::from(r) << 16 | u32::from(gr) << 8 | u32::from(b),
        _ => 0,
    };
    let id = low | high << 24;
    (FIRST..=LAST).contains(&id).then_some((id, placement))
}

/// What one client's terminal holds of vtmux's images.
#[derive(Debug, Default)]
pub struct ClientGraphics {
    /// Its range was freed (at attach): what it held before is gone.
    cleared: bool,
    uploaded: BTreeSet<u32>,
    placed: BTreeMap<(u32, u32), Place>,
}

impl ClientGraphics {
    /// The bytes to write before and after a frame of `screen`: uploads
    /// and virtual placements it lacks, then frees of what it no longer
    /// shows. `mine` says whether the client may be shown an image (one
    /// of a pane it shows), `pixels` gives its pixels, both by its key.
    pub fn frame(
        &mut self,
        screen: &Snapshot,
        tick: &Tick,
        ids: &Ids,
        mine: impl Fn(ImageKey) -> bool,
        pixels: impl Fn(ImageKey) -> Option<g::Pixels>,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut before = Vec::new();
        if !self.cleared {
            self.cleared = true;
            before.extend(g::free_range(FIRST, LAST));
        }
        let shows: BTreeSet<(u32, u32)> = screen
            .cells
            .iter()
            .filter_map(shown)
            .filter(|s| ids.key(s.0).is_some_and(&mine))
            .collect();
        let images: BTreeSet<u32> = shows.iter().map(|s| s.0).collect();
        for &id in &images {
            if self.uploaded.contains(&id) {
                continue;
            }
            let Some(px) = ids.key(id).and_then(&pixels) else {
                continue;
            };
            before.extend(g::upload(id, &px));
            self.uploaded.insert(id);
        }
        for &(id, cell_placement) in &shows {
            // A cell naming no placement shows any of the image's
            // (`placeholderTarget`, graphics_storage.zig).
            let found = tick
                .places
                .get_key_value(&(id, cell_placement))
                .or_else(|| {
                    (cell_placement == 0)
                        .then(|| tick.places.range((id, 0)..=(id, u32::MAX)).next())
                        .flatten()
                });
            let Some((&(_, placement), &place)) = found else {
                continue;
            };
            if !self.uploaded.contains(&id) || self.placed.get(&(id, placement)) == Some(&place) {
                continue;
            }
            before.extend(g::place(
                id,
                placement,
                place.cols,
                place.rows,
                place.source,
            ));
            self.placed.insert((id, placement), place);
        }
        let mut after = Vec::new();
        let gone: Vec<u32> = self.uploaded.difference(&images).copied().collect();
        for id in gone {
            after.extend(g::free(id));
            self.uploaded.remove(&id);
            self.placed.retain(|k, _| k.0 != id);
        }
        (before, after)
    }

    /// Whether its terminal holds any of vtmux's images.
    #[must_use]
    pub fn holds_any(&self) -> bool {
        !self.uploaded.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pane: u32, image: u32, generation: u64) -> ImageKey {
        ImageKey {
            pane: PaneId(pane),
            alternate: false,
            image,
            generation,
        }
    }

    /// One id per (pane, screen, image, generation), from vtmux's range:
    /// two panes' image 1 are two images, a new generation a new id, and
    /// an id is kept while its image is placed and forgotten after.
    #[test]
    fn ids_are_per_pane_image_and_generation() {
        let mut ids = Ids::default();
        let a = ids.id(key(0, 1, 5));
        let b = ids.id(key(1, 1, 5));
        let c = ids.id(key(0, 1, 6));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!([a, b, c].iter().all(|id| (FIRST..=LAST).contains(id)));
        assert_eq!(ids.id(key(0, 1, 5)), a, "the same image keeps its id");
        assert_eq!(ids.key(b), Some(key(1, 1, 5)));
        ids.keep(&[key(0, 1, 6)].into_iter().collect());
        assert_eq!((ids.key(a), ids.key(c)), (None, Some(key(0, 1, 6))));
    }

    fn row(text: &str) -> Snapshot {
        let mut s = crate::render::blank(None, 6, 1);
        for (cell, ch) in s.cells.iter_mut().zip(text.chars()) {
            cell.text = ch.to_string().as_str().into();
        }
        s
    }

    /// A direct placement covers its cells, cut at the content's edge; one
    /// below the text leaves the cells with text; a placement scrolled off
    /// above draws nothing.
    #[test]
    fn a_direct_placement_covers_its_cells() {
        let pg = PaneGraphics::default();
        let at = |col, below| Direct {
            id: FIRST,
            placement: 7,
            col,
            row: 0,
            cols: 3,
            rows: 1,
            below,
        };
        let content = row("ab");
        let (over, _) = overlay(&content, &[], &pg, &[at(4, false)], 0).unwrap();
        let shown: Vec<_> = over.cells.iter().map(shown).collect();
        assert_eq!(shown[..4], [None; 4]);
        assert_eq!(shown[4..], [Some((FIRST, 7)); 2], "cut at the edge");
        let (under, _) = overlay(&content, &[], &pg, &[at(0, true)], 0).unwrap();
        assert_eq!(under.cells[0].text.as_str(), "a", "below the text");
        assert_eq!(shown_of(&under, 2), Some((FIRST, 7)));
        assert!(
            overlay(&content, &[], &pg, &[at(0, false)], 1).is_none(),
            "above the top"
        );
    }

    /// A placement below the text leaves a wide character whole: its
    /// spacer is part of the text, so the head keeps its width.
    #[test]
    fn a_placement_below_the_text_leaves_a_wide_character_whole() {
        let pg = PaneGraphics::default();
        let mut content = row("");
        content.cells[0].text = "中".into();
        content.cells[0].width = 2;
        content.cells[1].width = 0;
        content.cells[2].text = "x".into();
        content.cells[2].width = 1;
        let d = Direct {
            id: FIRST,
            placement: 7,
            col: 0,
            row: 0,
            cols: 4,
            rows: 1,
            below: true,
        };
        let (under, _) = overlay(&content, &[], &pg, &[d], 0).unwrap();
        let cells: Vec<_> = under.cells[..4]
            .iter()
            .map(|c| (c.text.as_str().to_owned(), c.width, shown(c)))
            .collect();
        assert_eq!(cells[0], ("中".to_owned(), 2, None));
        assert_eq!((cells[1].1, cells[1].2), (0, None), "the spacer stays");
        assert_eq!(cells[2], ("x".to_owned(), 1, None));
        assert_eq!(cells[3].2, Some((FIRST, 7)));
    }

    fn shown_of(s: &Snapshot, x: usize) -> Option<(u32, u32)> {
        shown(&s.cells[x])
    }

    /// What reaches a client without [`overlay`] loses the placeholder
    /// cells naming vtmux's ids, a run's bare cells too (they take the
    /// id of the cell before them); a program's own ids stay.
    #[test]
    fn scrub_blanks_placeholder_cells_of_vtmuxs_ids_only() {
        let d = |n: usize| g::DIACRITICS[n];
        let p = g::PLACEHOLDER;
        let mut content = row("ab");
        let mut put = |x: usize, text: String, fg: u32| {
            content.cells[x].text = text.as_str().into();
            content.cells[x].style.fg = g::id_colour(fg);
        };
        put(1, format!("{p}{}{}{}", d(0), d(0), d(0xEE)), FIRST);
        put(2, p.to_string(), FIRST);
        put(4, format!("{p}{}{}{}", d(0), d(0), d(1)), 0x0100_0005);
        let (out, stamps) = scrub(&content, &[9]).unwrap();
        let text: Vec<&str> = out.cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(text[..4], ["a", " ", " ", ""]);
        assert_eq!(out.cells[4], content.cells[4], "the program's own id");
        assert_ne!(stamps, [9], "the row changed");
        assert!(scrub(&row("ab"), &[]).is_none());
        assert!(scrub(&out, &[]).is_none(), "nothing left to blank");
    }
}
