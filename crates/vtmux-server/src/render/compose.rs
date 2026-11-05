//! Turn a [`Scene`] into cells (screen-redraw.c `redraw_draw`): pane content,
//! border glyphs in their pane's style, arrows, pane status lines and the
//! window fill.
//!
//! Styles stay [`CellStyle`]s (theme colours unresolved) until here, where a
//! client's [`Theme`] turns them into terminal colours.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use vtmux_core::colour::Colour;
use vtmux_core::ids::PaneId;
use vtmux_core::style::{Attrs, CellStyle};
use vtmux_core::styled;
use vtmux_vt::{Cell, CellText, Color, Snapshot, Style, Underline};

use super::blend::PaneBlend;
use super::scene::{Lines, Scene, SceneCell, Shape};

/// A client's ten `theme*` colours (`c->theme_colours`,
/// server-client.c `server_client_update_theme_colours`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Theme(pub [Color; 10]);

impl Theme {
    /// Colour as the terminal sees it (colour.c `colour_toescape`).
    #[must_use]
    pub fn colour(&self, c: Colour) -> Color {
        match c {
            Colour::Default | Colour::Terminal => Color::Default,
            Colour::Ansi(n) | Colour::Palette(n) => Color::Palette(n),
            Colour::Bright(n) => Color::Palette(8 + n),
            Colour::Rgb(r, g, b) => Color::Rgb(r, g, b),
            Colour::Theme(i) => self.0.get(usize::from(i)).copied().unwrap_or_default(),
        }
    }

    /// A cell style as terminal attributes.
    #[must_use]
    pub fn style(&self, s: &CellStyle) -> Style {
        let a = s.attrs;
        let underline = if a.contains(Attrs::DOUBLE_UNDERSCORE) {
            Underline::Double
        } else if a.contains(Attrs::CURLY_UNDERSCORE) {
            Underline::Curly
        } else if a.contains(Attrs::DOTTED_UNDERSCORE) {
            Underline::Dotted
        } else if a.contains(Attrs::DASHED_UNDERSCORE) {
            Underline::Dashed
        } else if a.contains(Attrs::UNDERSCORE) {
            Underline::Single
        } else {
            Underline::None
        };
        Style {
            fg: self.colour(s.fg),
            bg: self.colour(s.bg),
            underline_color: self.colour(s.us),
            bold: a.contains(Attrs::BRIGHT),
            faint: a.contains(Attrs::DIM),
            italic: a.contains(Attrs::ITALICS),
            blink: a.contains(Attrs::BLINK),
            inverse: a.contains(Attrs::REVERSE),
            invisible: a.contains(Attrs::HIDDEN),
            strikethrough: a.contains(Attrs::STRIKETHROUGH),
            overline: a.contains(Attrs::OVERLINE),
            underline,
        }
    }

    /// A drawn format cell; `acs` text goes through the UTF-8 line-drawing
    /// table (tty-acs.c `tty_acs_get`).
    #[must_use]
    pub fn cell(&self, c: &styled::Cell) -> Cell {
        let text = if c.style.attrs.contains(Attrs::ACS) {
            c.text.chars().map(acs).collect()
        } else {
            c.text.clone()
        };
        Cell {
            text: text.into(),
            width: c.width,
            style: self.style(&c.style),
            acs: c.style.attrs.contains(Attrs::ACS),
            link: c.style.link.is_some(),
            tab: false,
        }
    }
}

/// tty-acs.c `tty_acs_table`.
fn acs(c: char) -> char {
    match c {
        '+' => '→',
        ',' => '←',
        '-' => '↑',
        '.' => '↓',
        '0' => '▮',
        '`' => '◆',
        'a' => '▒',
        'f' => '°',
        'g' => '±',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        c => c,
    }
}

/// How one visible pane draws.
#[derive(Clone, Debug)]
pub struct PaneDraw {
    /// Shared with the pane's [`vtmux_vt::ScreenView`] when it shows its
    /// own grid, so a frame does not copy it.
    pub content: Rc<Snapshot>,
    /// Per content row, the stamp of the change that last touched it
    /// ([`vtmux_vt::ScreenView::row_stamps`]); empty when unknown (a mode's
    /// screen), and the pane's rows are then composed every frame.
    pub stamps: Vec<u64>,
    /// `pane_index`, for `number` lines.
    pub index: Option<u32>,
    /// The colours a default-coloured content cell draws in
    /// (tty.c `tty_default_colours`): `window-active-style`'s where it
    /// sets them on the active pane, else `window-style`'s. Only `fg` and
    /// `bg` count.
    pub defaults: CellStyle,
    pub lines: Lines,
    /// `pane-active-border-style` for the active pane, else
    /// `pane-border-style` (window-border.c `window_pane_get_border_style`).
    pub border: CellStyle,
    /// The drawn status line, empty when there is none.
    pub status: Vec<styled::Cell>,
    /// The status line's `range=` ranges (`wp->border_status_line.ranges`),
    /// in its own columns.
    pub status_ranges: Vec<styled::StyleRange>,
    /// Per content cell, row-major: in copy mode's selection
    /// (`GRID_FLAG_SELECTED`), so never erased as part of an empty run.
    /// Empty without a selection.
    pub selected: Vec<bool>,
    /// Its scrollbar, when one is drawn.
    pub scrollbar: Option<BarDraw>,
    /// D26: how its content cells are blended, if at all.
    pub blend: Option<PaneBlend>,
}

/// How a pane's scrollbar draws (`redraw_draw_scrollbar_span`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarDraw {
    pub style: crate::scrollbar::BarStyle,
    pub placement: crate::scrollbar::Placement,
    /// The slider's first row and height.
    pub slider: (u32, u32),
}

impl BarDraw {
    /// Column `col`, row `row` of the bar: a padding cell in the pane's
    /// default colours (`defaults`, screen-redraw.c:1337
    /// `tty_default_colours`), else the style's colours, swapped on the
    /// slider.
    fn cell(&self, theme: &Theme, defaults: &CellStyle, col: u32, row: u32) -> Cell {
        let (w, pad) = (self.style.width, self.style.pad);
        let is_pad = if self.placement.left {
            col >= w && col < w + pad
        } else {
            col < pad
        };
        let mut style = if is_pad {
            theme.style(&CellStyle {
                fg: defaults.fg,
                bg: defaults.bg,
                ..CellStyle::default()
            })
        } else {
            theme.style(&self.style.cell)
        };
        let (y, h) = self.slider;
        if !is_pad && row >= y && row < y + h {
            std::mem::swap(&mut style.fg, &mut style.bg);
        }
        Cell {
            text: " ".into(),
            width: 1,
            style,
            ..Cell::default()
        }
    }
}

/// Everything [`compose`] needs besides the scene.
#[derive(Clone, Debug)]
pub struct Draw {
    pub panes: BTreeMap<PaneId, PaneDraw>,
    pub active: Option<PaneId>,
    /// The marked pane when it is in this window of this session
    /// (`dctx->marked`, screen-redraw.c:1611): the borders next to it are
    /// drawn reversed.
    pub marked: Option<PaneId>,
    /// Where the window's origin is in the drawn area: 1 cell in under the
    /// focus ring (D23), else 0.
    pub offset: u16,
    /// The window's `pane-border-style` and `pane-border-lines`
    /// (`redraw_get_default_border_style`).
    pub border: CellStyle,
    pub lines: Lines,
    /// `fill-character` outside the window and inside it
    /// (window-border.c `window_set_fill_cells`).
    pub outside: styled::Cell,
    pub inside: styled::Cell,
    /// How empty runs are cleared ([`Clear`]).
    pub clear: Clear,
}

/// How `tty_draw_line_clear` (tty-draw.c:52-90) clears a run of ten or
/// more empty cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clear {
    /// Genuine background colour erase: any run is erased.
    Bce,
    /// No BCE (`tty_fake_bce`): only a run in the default background is.
    FakeBce,
    /// An overlay is up: never erased, written as spaces.
    Spaces,
}

/// `window_copy_fill_cell`: the fill's text and attributes over `base`, its
/// colours where set.
fn fill(base: &CellStyle, f: &styled::Cell) -> styled::Cell {
    let or = |a: Colour, b: Colour| if a == Colour::Default { b } else { a };
    styled::Cell {
        text: f.text.clone(),
        width: 1,
        style: CellStyle {
            fg: or(f.style.fg, base.fg),
            bg: or(f.style.bg, base.bg),
            us: or(f.style.us, base.us),
            attrs: base.attrs.union(f.style.attrs),
            link: base.link.clone(),
        },
    }
}

/// Draw `scene` into the top `scene.rows` rows of `screen`.
pub fn compose(scene: &Scene, d: &Draw, theme: &Theme, screen: &mut Snapshot) {
    let cols = usize::from(screen.cols);
    // The last row drawn left the terminal cursor past its last column.
    let mut ended_written = false;
    for (y, row) in (0..scene.rows.min(screen.rows)).zip(screen.cells.chunks_mut(cols.max(1))) {
        ended_written = compose_row(scene, d, theme, row, y, ended_written);
    }
}

/// [`compose`] into `cache`, reusing the rows it composed last time whose
/// inputs are unchanged: the same scene, styles and theme, and the same
/// content rows of the panes on them ([`PaneDraw::stamps`]). A key's echo
/// then composes one row, not the window. Returns whether any row was
/// composed again; [`ComposeCache::copy_to`] draws the result.
pub fn compose_cached(
    cache: &mut ComposeCache,
    scene: &Rc<Scene>,
    d: &Draw,
    theme: &Theme,
    cols: u16,
    rows: u16,
) -> bool {
    let rows = scene.rows.min(rows);
    let key = DrawKey::of(d, *theme, cols, rows);
    let same = cache.scene.as_ref().is_some_and(|s| Rc::ptr_eq(s, scene))
        && cache.key.as_ref() == Some(&key);
    if !same {
        cache.rows.clear();
        cache.scene = Some(Rc::clone(scene));
        cache.key = Some(key);
    }
    let mut changed = !same;
    let mut ended_written = false;
    for y in 0..rows {
        let sig = row_signature(scene, d, y);
        let i = usize::from(y);
        if let Some(row) = cache.rows.get(i)
            && sig.is_some()
            && row.sig == sig
            && row.above == ended_written
        {
            ended_written = row.ended;
            continue;
        }
        if cache.rows.len() <= i {
            cache.rows.push(CachedRow {
                sig: None,
                above: false,
                ended: false,
                version: 0,
                cells: vec![Cell::default(); usize::from(cols)],
            });
        }
        let row = &mut cache.rows[i];
        let above = ended_written;
        ended_written = compose_row(scene, d, theme, &mut row.cells, y, above);
        row.sig = sig;
        row.above = above;
        row.ended = ended_written;
        row.version = ROW_VERSION.fetch_add(1, Ordering::Relaxed);
        changed = true;
    }
    changed
}

/// [`CachedRow::version`]s are unique across every cache, so a screen row
/// tagged with one ([`ComposeCache::copy_rows`]) holds exactly that row.
static ROW_VERSION: AtomicU64 = AtomicU64::new(1);

/// The tag of a screen row [`ComposeCache::copy_rows`] left blank; no
/// [`CachedRow::version`] reaches it. Tag 0 is a row anything else drew.
pub const BLANK_ROW: u64 = u64::MAX;

/// What [`compose_cached`] composed last, per client.
#[derive(Debug, Default)]
pub struct ComposeCache {
    scene: Option<Rc<Scene>>,
    key: Option<DrawKey>,
    rows: Vec<CachedRow>,
}

impl ComposeCache {
    /// Draw what [`compose_cached`] composed into the top rows of `screen`.
    pub fn copy_to(&self, screen: &mut Snapshot) {
        let cols = usize::from(screen.cols).max(1);
        for (row, cached) in screen.cells.chunks_mut(cols).zip(&self.rows) {
            row.clone_from_slice(&cached.cells);
        }
    }

    /// [`Self::copy_to`] straight into rows `y..y + rows` of `screen`,
    /// which still holds an earlier frame: `tags` has, per screen row, the
    /// [`CachedRow::version`] it holds (0 when unknown, [`BLANK_ROW`] when
    /// blank), and only rows whose tag differs are copied (or blanked,
    /// below the composed rows). A frame where one pane row changed copies
    /// one row, not the window.
    pub fn copy_rows(&self, screen: &mut Snapshot, y: u16, rows: u16, tags: &mut [u64]) {
        let cols = usize::from(screen.cols).max(1);
        let lines = screen.cells.chunks_mut(cols).zip(tags.iter_mut());
        for (i, (row, tag)) in lines
            .skip(usize::from(y))
            .take(usize::from(rows))
            .enumerate()
        {
            match self.rows.get(i) {
                Some(cached) if *tag != cached.version => {
                    row.clone_from_slice(&cached.cells);
                    *tag = cached.version;
                }
                None if *tag != BLANK_ROW => {
                    super::clear_cells(row);
                    *tag = BLANK_ROW;
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug)]
struct CachedRow {
    /// [`row_signature`] when composed.
    sig: Option<Vec<(PaneId, u32, u64, u64)>>,
    /// The row above ended written (see [`compose`]'s `wrapped`).
    above: bool,
    ended: bool,
    /// Unique per composition of the row ([`ROW_VERSION`]).
    version: u64,
    cells: Vec<Cell>,
}

/// A [`PaneDraw`] but its content.
#[derive(Debug, PartialEq)]
struct PaneKey {
    id: PaneId,
    index: Option<u32>,
    defaults: CellStyle,
    lines: Lines,
    border: CellStyle,
    status: Vec<styled::Cell>,
    selected: Vec<bool>,
    scrollbar: Option<BarDraw>,
    blend: Option<PaneBlend>,
}

/// Everything in a [`Draw`] but the panes' content, and the theme.
#[derive(Debug, PartialEq)]
struct DrawKey {
    panes: Vec<PaneKey>,
    active: Option<PaneId>,
    marked: Option<PaneId>,
    offset: u16,
    border: CellStyle,
    lines: Lines,
    outside: styled::Cell,
    inside: styled::Cell,
    clear: Clear,
    theme: Theme,
    size: (u16, u16),
}

impl DrawKey {
    fn of(d: &Draw, theme: Theme, cols: u16, rows: u16) -> Self {
        Self {
            panes: d
                .panes
                .iter()
                .map(|(&id, p)| PaneKey {
                    id,
                    index: p.index,
                    defaults: p.defaults.clone(),
                    lines: p.lines,
                    border: p.border.clone(),
                    status: p.status.clone(),
                    selected: p.selected.clone(),
                    scrollbar: p.scrollbar.clone(),
                    blend: p.blend,
                })
                .collect(),
            active: d.active,
            marked: d.marked,
            offset: d.offset,
            border: d.border.clone(),
            lines: d.lines,
            outside: d.outside.clone(),
            inside: d.inside.clone(),
            clear: d.clear,
            theme,
            size: (cols, rows),
        }
    }
}

/// The content rows row `y` of the scene shows, as (pane, content row,
/// its stamp, the stamp of the row above it, which decides `wrapped`);
/// `None` if a pane's stamps are unknown.
fn row_signature(scene: &Scene, d: &Draw, y: u16) -> Option<Vec<(PaneId, u32, u64, u64)>> {
    let mut sig: Vec<(PaneId, u32, u64, u64)> = Vec::new();
    for &(pane, py) in scene.row_panes(y) {
        let stamps = &d.panes.get(&pane)?.stamps;
        let at = usize::try_from(py).ok()?;
        let stamp = *stamps.get(at)?;
        let above = at
            .checked_sub(1)
            .map_or(Some(0), |a| stamps.get(a).copied())?;
        sig.push((pane, py, stamp, above));
    }
    Some(sig)
}

/// Row `y` of [`compose`]; `above_ended_written` is what the row above
/// returned. Returns whether this row ended written.
fn compose_row(
    scene: &Scene,
    d: &Draw,
    theme: &Theme,
    line: &mut [Cell],
    y: u16,
    above_ended_written: bool,
) -> bool {
    let line_cols = u16::try_from(line.len()).unwrap_or(u16::MAX);
    let mut ended_written = false;
    {
        for x in 0..scene.cols.min(line_cols) {
            let cell = match scene.get(x, y) {
                SceneCell::Pane { pane, px, py } => d
                    .panes
                    .get(pane)
                    .and_then(|p| {
                        p.content
                            .cell(u16::try_from(*px).ok()?, u16::try_from(*py).ok()?)
                    })
                    .cloned()
                    .unwrap_or_default(),
                SceneCell::Outside => theme.cell(&fill(&d.border, &d.outside)),
                SceneCell::Empty => theme.cell(&fill(&d.border, &d.inside)),
                SceneCell::Border(b) => {
                    let owner = b.style_pane(d.active).and_then(|p| d.panes.get(&p));
                    let (style, lines, index) = owner.map_or((&d.border, d.lines, None), |p| {
                        (&p.border, p.lines, p.index)
                    });
                    let text = b
                        .arrow_glyph(d.active)
                        .map_or_else(|| b.shape().glyph(lines, index), Into::into);
                    // screen-redraw.c:1250-1253: GRID_ATTR_REVERSE toggled
                    // beside the marked pane.
                    let marked = d
                        .marked
                        .is_some_and(|m| [b.up, b.down, b.left, b.right].contains(&Some(m)));
                    let mut style = theme.style(style);
                    if marked {
                        style.inverse = !style.inverse;
                    }
                    Cell {
                        text: text.into(),
                        width: 1,
                        style,
                        ..Cell::default()
                    }
                }
                SceneCell::Status { pane, offset, .. } => d
                    .panes
                    .get(pane)
                    .and_then(|p| p.status.get(usize::try_from(*offset).ok()?))
                    .map_or_else(Cell::default, |c| theme.cell(c)),
                SceneCell::Scrollbar { pane, col, row } => d
                    .panes
                    .get(pane)
                    .and_then(|p| Some((p.scrollbar.as_ref()?, &p.defaults)))
                    .map_or_else(Cell::default, |(b, def)| b.cell(theme, def, *col, *row)),
            };
            line[usize::from(x)] = cell;
        }
        // Pane and pane status spans go through `tty_draw_line`; borders and
        // fill are drawn cell by cell (`tty_cell`) and keep their text.
        let width = scene.cols.min(line_cols);
        let mut x = 0;
        while x < width {
            let Some(key) = span_key(scene.get(x, y), x) else {
                x += 1;
                continue;
            };
            let end = (x..width)
                .find(|&e| span_key(scene.get(e, y), e) != Some(key))
                .unwrap_or(width);
            let full = x == 0 && end == line_cols;
            // tty_draw_line's `wrapped`: a full-width row of a line the row
            // above wrapped onto, with the terminal cursor left past the
            // end of that row.
            let wrapped = full
                && above_ended_written
                && match scene.get(x, y) {
                    SceneCell::Pane { pane, py, .. } => py
                        .checked_sub(1)
                        .and_then(|above| {
                            d.panes
                                .get(pane)?
                                .content
                                .wrapped
                                .get(usize::try_from(above).ok()?)
                                .copied()
                        })
                        .unwrap_or(false),
                    _ => false,
                };
            let cells = &mut line[usize::from(x)..usize::from(end)];
            let keep = selected_span(d, scene, x, end, y);
            let written = draw_pane_span(scene.get(x, y), d, theme, cells, wrapped, &keep);
            // D26: the pane blend goes over the span as drawn (after the
            // window style).
            if let SceneCell::Pane { pane, .. } = scene.get(x, y)
                && let Some(blend) = d.panes.get(pane).and_then(|p| p.blend.as_ref())
            {
                blend.apply(cells);
            }
            ended_written = full && written;
            x = end;
        }
    }
    ended_written
}

/// One `tty_draw_line` span of `cell`'s pane (or status line) in `cells`
/// ([`draw_line_keeping`]). `redraw_draw_pane_span`: a pane's content draws
/// over its default colours (`style_ctx.defaults`, [`PaneDraw::defaults`]),
/// which `tty_attributes` puts in for a default fg or bg. Which cells are
/// empty is decided on the grid's own colours: a blank cell (no text)
/// stands for tmux's cells past the line's end, empty whatever the
/// defaults (tty-draw.c:243-246). Returns whether the span ended written.
fn draw_pane_span(
    cell: &SceneCell,
    d: &Draw,
    theme: &Theme,
    cells: &mut [Cell],
    wrapped: bool,
    keep: &[bool],
) -> bool {
    let bgs: Vec<Color> = cells.iter().map(|c| c.style.bg).collect();
    let defaults = match cell {
        SceneCell::Pane { pane, .. } => d
            .panes
            .get(pane)
            .map(|p| theme.style(&p.defaults))
            .filter(|s| (s.fg, s.bg) != (Color::Default, Color::Default)),
        _ => None,
    };
    // tty_fake_bce: no BCE and a default background that is not the
    // terminal's means spaces.
    let clear = match d.clear {
        Clear::FakeBce if defaults.is_some_and(|s| s.bg != Color::Default) => Clear::Spaces,
        c => c,
    };
    let written = draw_line_keeping(cells, &bgs, Color::Default, clear, wrapped, keep);
    if let Some(defaults) = defaults {
        for c in cells.iter_mut() {
            // Erased cells keep only their background.
            if c.style.fg == Color::Default && !c.text.is_empty() {
                c.style.fg = defaults.fg;
            }
            if c.style.bg == Color::Default {
                c.style.bg = defaults.bg;
            }
        }
    }
    written
}

/// Which cells of the span `x..end` of row `y` are [`PaneDraw::selected`].
fn selected_span(d: &Draw, scene: &Scene, x: u16, end: u16, y: u16) -> Vec<bool> {
    (x..end)
        .map(|x| match scene.get(x, y) {
            SceneCell::Pane { pane, px, py } => d.panes.get(pane).is_some_and(|p| {
                let cols = usize::from(p.content.cols);
                let at =
                    usize::try_from(*py).unwrap_or(0) * cols + usize::try_from(*px).unwrap_or(0);
                p.selected.get(at).copied().unwrap_or(false)
            }),
            _ => false,
        })
        .collect()
}

/// What makes cells one `tty_draw_line` span: the same pane's content or
/// status line, contiguous in the pane (screen-redraw.c `redraw_build_spans`).
fn span_key(cell: &SceneCell, x: u16) -> Option<(bool, PaneId, i64)> {
    match cell {
        SceneCell::Pane { pane, px, .. } => Some((false, *pane, i64::from(x) - i64::from(*px))),
        SceneCell::Status { pane, offset, .. } => {
            Some((true, *pane, i64::from(x) - i64::from(*offset)))
        }
        _ => None,
    }
}

/// tty-draw.c `tty_draw_line_get_empty`: blank (cleared or a single space),
/// no attributes (the charset counts, `GRID_ATTR_CHARSET`), no hyperlink,
/// and the background of the cell before it (`bgs`, as the grid stores
/// it); the foreground does not count. Snapshots carry no tab flag.
fn is_empty<T: PartialEq>(c: &Cell, bg: &T, last: &T) -> bool {
    let plain = Style {
        fg: c.style.fg,
        bg: c.style.bg,
        underline_color: c.style.underline_color,
        ..Style::default()
    };
    c.width == 1
        && !c.acs
        && !c.link
        && (c.text.is_empty() || c.text == " ")
        && bg == last
        && c.style == plain
}

/// The cells of one span as the terminal shows them after tty-draw.c
/// `tty_draw_line`: a run of empty cells is cleared with the last one's
/// attributes (`tty_draw_line_clear`). Ten or more are erased (EL/EL1/ECH)
/// unless the terminal would need a fake BCE, and keep only the background,
/// no text; shorter runs are spaces in the last cell's style. Erased cells
/// are the only empty-text cells left, so the compositor erases them too.
///
/// `wrapped` (the row continues a line wrapped onto it, see [`compose`])
/// writes a leading run as spaces whatever its length; it lasts until the
/// first flush, so only a run at the start of the row.
///
/// Returns whether the last cell was written, not erased: the terminal
/// cursor is then past the end of the span.
#[cfg(test)]
pub(crate) fn draw_line(cells: &mut [Cell], bce: bool, wrapped: bool) -> bool {
    let bgs: Vec<Color> = cells.iter().map(|c| c.style.bg).collect();
    let clear = if bce { Clear::Bce } else { Clear::FakeBce };
    draw_line_by(cells, &bgs, Color::Default, clear, wrapped)
}

/// [`draw_line`] where "the same background" is decided on `bgs`, one per
/// cell, starting from `default` (`defaults->bg`). tmux compares grid
/// colours, before a theme colour becomes what the terminal is sent, so
/// `bg=green` next to a theme green that resolves to the same colour still
/// differs (status lines, where both meet).
pub(crate) fn draw_line_by<T: PartialEq + Clone>(
    cells: &mut [Cell],
    bgs: &[T],
    default: T,
    clear: Clear,
    wrapped: bool,
) -> bool {
    draw_line_keeping(cells, bgs, default, clear, wrapped, &[])
}

/// [`draw_line_by`] where the cells `keep` marks are never empty: a
/// selected cell is always written (`tty_draw_line_get_empty`,
/// tty-draw.c:105-106).
fn draw_line_keeping<T: PartialEq + Clone>(
    cells: &mut [Cell],
    bgs: &[T],
    default: T,
    clear: Clear,
    mut wrapped: bool,
    keep: &[bool],
) -> bool {
    let empty = |cells: &[Cell], i: usize, last: &T| {
        !keep.get(i).copied().unwrap_or(false) && is_empty(&cells[i], &bgs[i], last)
    };
    let mut last_bg = default;
    let mut written = false;
    let mut i = 0;
    while i < cells.len() {
        if cells[i].width == 0 {
            i += 1;
            continue;
        }
        if !empty(cells, i, &last_bg) {
            if cells[i].text.is_empty() {
                cells[i].text = " ".into();
            }
            last_bg = bgs[i].clone();
            i += usize::from(cells[i].width);
            wrapped = false;
            written = true;
            continue;
        }
        let start = i;
        while i < cells.len() && empty(cells, i, &last_bg) {
            i += 1;
        }
        let last = cells[i - 1].style;
        let erase = !wrapped
            && i - start >= 10
            && match clear {
                Clear::Bce => true,
                Clear::FakeBce => last.bg == Color::Default,
                Clear::Spaces => false,
            };
        wrapped = false;
        written = !erase;
        let cell = if erase {
            Cell {
                text: CellText::new(),
                width: 1,
                style: Style {
                    bg: last.bg,
                    ..Style::default()
                },
                ..Cell::default()
            }
        } else {
            Cell {
                text: " ".into(),
                width: 1,
                style: last,
                ..Cell::default()
            }
        };
        cells[start..i].fill(cell);
    }
    written
}

/// `window_make_pane_status`: the border line under the status line, then
/// `expanded` (`pane-border-format`) drawn over it in the border style.
/// `under` is [`Scene::status_line`]. Under the focus ring (D23) the line
/// cells on the active pane's rectangle take `ring` (the active border
/// style), so the rectangle stays one colour behind another pane's label.
/// The line comes with its ranges.
#[must_use]
pub fn pane_status(
    under: &[Option<Shape>],
    border: &CellStyle,
    ring: &CellStyle,
    lines: Lines,
    index: Option<u32>,
    expanded: &str,
) -> styled::Line {
    if under.is_empty() {
        return styled::Line {
            cells: Vec::new(),
            ranges: Vec::new(),
        };
    }
    let row = under
        .iter()
        .map(|j| {
            let shape = j.unwrap_or_else(|| super::scene::Junction::Horizontal.into());
            styled::Cell {
                text: shape.glyph(lines, index),
                width: 1,
                style: if shape.heavy() { ring } else { border }.clone(),
            }
        })
        .collect();
    let base = CellStyle {
        attrs: border.attrs.without(Attrs::ACS),
        ..border.clone()
    };
    styled::draw_onto(&base, row, expanded, true, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::scene::Junction;

    /// A status line shorter than its format is cut; past its text the
    /// border shows through, in the border style.
    #[test]
    fn status_line_draws_over_the_border() {
        let green = CellStyle {
            fg: Colour::Ansi(2),
            ..CellStyle::default()
        };
        let under = [Some(Junction::Horizontal.into()); 6];
        let cells = pane_status(
            &under,
            &green,
            &green,
            Lines::Single,
            None,
            "#[reverse]0#[default] x",
        )
        .cells;
        let text: String = cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(text, "0 x───");
        assert!(cells[0].style.attrs.contains(Attrs::REVERSE));
        assert_eq!(cells[1].style.fg, Colour::Ansi(2));
        assert_eq!(cells[5].style, green);
    }

    fn text_cell(text: &str, fg: Color, bg: Color) -> Cell {
        Cell {
            text: text.into(),
            width: 1,
            style: Style {
                fg,
                bg,
                ..Style::default()
            },
            ..Cell::default()
        }
    }

    /// `screen_pane_spaces_bg`: `\e[31;44mx` then eleven spaces then `y`: the spaces are
    /// erased with the blue background and lose the red foreground; nine
    /// are written as spaces.
    #[test]
    fn long_space_runs_are_erased_whatever_their_foreground() {
        let (red, blue) = (Color::Palette(1), Color::Palette(4));
        for (n, erased) in [(11, true), (9, false)] {
            let mut row = vec![text_cell("x", red, blue)];
            row.extend((0..n).map(|_| text_cell(" ", red, blue)));
            row.push(text_cell("y", red, blue));
            draw_line(&mut row, true, false);
            let gap = &row[1..=n];
            if erased {
                assert!(gap.iter().all(|c| c.text.is_empty()
                    && c.style
                        == Style {
                            bg: blue,
                            ..Style::default()
                        }));
            } else {
                assert!(gap.iter().all(|c| c.text == " " && c.style.fg == red));
            }
            assert_eq!((row[0].text.as_str(), row[n + 1].text.as_str()), ("x", "y"));
        }
    }

    /// A space with another background, or with attributes, is not empty;
    /// without BCE a coloured run is written, not erased.
    #[test]
    fn background_change_and_attributes_break_a_run() {
        let blue = Color::Palette(4);
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, blue))
            .collect();
        draw_line(&mut row, true, false);
        assert_eq!(row[0].text, " ", "first blue space follows a default bg");
        assert!(row[1..].iter().all(|c| c.text.is_empty()));
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, blue))
            .collect();
        draw_line(&mut row, false, false);
        assert!(row.iter().all(|c| c.text == " "));
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, Color::Default))
            .collect();
        row[5].style.bold = true;
        draw_line(&mut row, true, false);
        assert!(row.iter().all(|c| c.text == " "));
    }

    /// Spaces under DEC graphics or inside a hyperlink are not empty, so a
    /// run of them is written with its foreground (`screen_pane_link_acs`).
    #[test]
    fn charset_and_link_spaces_are_not_empty() {
        let red = Color::Palette(1);
        for mark in [|c: &mut Cell| c.acs = true, |c: &mut Cell| c.link = true] {
            let mut row: Vec<Cell> = (0..12)
                .map(|_| text_cell(" ", red, Color::Default))
                .collect();
            row.iter_mut().for_each(mark);
            draw_line(&mut row, true, false);
            assert!(row.iter().all(|c| c.text == " " && c.style.fg == red));
        }
    }

    /// A wrapped row writes its leading run as spaces in the last cell's
    /// style; a later run is erased as usual (`screen_pane_wrap`).
    #[test]
    fn wrapped_row_writes_only_its_leading_run() {
        let red = Color::Palette(1);
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", red, Color::Default))
            .collect();
        row.push(text_cell("y", red, Color::Default));
        row.extend((0..12).map(|_| text_cell(" ", red, Color::Default)));
        assert!(!draw_line(&mut row, true, true));
        assert!(row[..12].iter().all(|c| c.text == " " && c.style.fg == red));
        assert!(row[13..].iter().all(|c| c.text.is_empty()));
    }

    /// The row cache composes what a full compose does, after every change
    /// to two side-by-side panes (echo, scroll, soft wrap, erase), and
    /// reports no change when nothing changed.
    #[test]
    #[allow(clippy::too_many_lines, reason = "one scenario, step by step")]
    fn cached_compose_matches_a_full_one() {
        use super::super::scene::{Indicators, SceneCache, ScenePane, WindowView};
        use vtmux_core::layout::{Geometry, PaneStatus};
        let mut terms = [
            vtmux_vt::PaneTerm::new(9, 4, 10),
            vtmux_vt::PaneTerm::new(10, 4, 10),
        ];
        let view = WindowView {
            sx: 20,
            sy: 4,
            panes: (0..2u32)
                .map(|i| ScenePane {
                    id: PaneId(i),
                    rect: if i == 0 {
                        Geometry::new(9, 4, 0, 0)
                    } else {
                        Geometry::new(10, 4, 10, 0)
                    },
                    floating: false,
                    lines: Lines::Single,
                    status: PaneStatus::Off,
                    scrollbar: None,
                })
                .collect(),
            indicators: Indicators::Colour,
            two_pane: None,
            ring: None,
        };
        let scenes = SceneCache::default();
        let mut cache = ComposeCache::default();
        let theme = Theme::default();
        let steps: [(usize, &[u8]); 8] = [
            (0, b"a"),
            (1, b"one\r\ntwo"),
            (0, b"\x1b[31mbcdefghijkl"),
            (1, b"\r\n\r\n\r\nscroll"),
            (0, b"\x1b[2J\x1b[H"),
            (1, b"\x1b[44m\x1b[K"),
            (0, b"0123456789abcdefgh"),
            (1, b"\x1bc"),
        ];
        let space = styled::Cell {
            text: " ".into(),
            width: 1,
            style: CellStyle::default(),
        };
        for (i, (pane, bytes)) in steps.iter().enumerate() {
            terms[*pane].feed(bytes);
            let draw = Draw {
                marked: None,
                panes: (0..2u32)
                    .map(|p| {
                        let view = terms[p as usize].screen();
                        (
                            PaneId(p),
                            PaneDraw {
                                content: view.shared(),
                                stamps: view.row_stamps().to_vec(),
                                index: Some(p),
                                defaults: CellStyle::default(),
                                lines: Lines::Single,
                                border: CellStyle::default(),
                                status: Vec::new(),
                                status_ranges: Vec::new(),
                                selected: Vec::new(),
                                scrollbar: None,
                                blend: None,
                            },
                        )
                    })
                    .collect(),
                active: Some(PaneId(1)),
                offset: 0,
                border: CellStyle::default(),
                lines: Lines::Single,
                outside: space.clone(),
                inside: space.clone(),
                clear: Clear::Bce,
            };
            let scene = Scene::cached(&scenes, view.clone(), 20, 4);
            let blank = |cols, rows| vtmux_vt::Snapshot {
                cols,
                rows,
                cells: vec![Cell::default(); usize::from(cols) * usize::from(rows)],
                wrapped: Vec::new(),
                cursor: None,
                cursor_visible: true,
            };
            let mut full = blank(20, 4);
            compose(&scene, &draw, &theme, &mut full);
            assert!(
                compose_cached(&mut cache, &scene, &draw, &theme, 20, 4),
                "step {i}"
            );
            let mut cached = blank(20, 4);
            cache.copy_to(&mut cached);
            assert_eq!(cached, full, "step {i}");
            assert!(
                !compose_cached(&mut cache, &scene, &draw, &theme, 20, 4),
                "step {i}: nothing new"
            );
        }
    }

    #[test]
    fn acs_fill_becomes_a_bullet() {
        let theme = Theme::default();
        let c = styled::Cell {
            text: "~".into(),
            width: 1,
            style: CellStyle {
                attrs: Attrs::ACS,
                ..CellStyle::default()
            },
        };
        assert_eq!(theme.cell(&c).text, "·");
    }
}
