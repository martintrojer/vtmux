//! What each cell of the window area shows: pane content, a border, a pane
//! status line, or nothing (screen-redraw.c `redraw_build_cells`). Built
//! fresh per frame from the visible panes; [`super::compose`] turns it into
//! cells.
//!
//! A border cell can belong to up to four panes, one per side; which pane
//! colours it is decided when drawing ([`Border::style_pane`]), except for
//! the two-pane half-colouring, which is fixed here
//! (`redraw_mark_two_pane_colours`).
//!
//! A pane's scrollbar ([`crate::scrollbar`]) is marked after its borders.
//! An `on` bar sits in columns the pane gave up, inside its borders; an
//! auto-hide bar goes over the pane.

use std::cell::RefCell;
use std::rc::Rc;

use vtmux_core::ids::PaneId;
use vtmux_core::layout::{Dir, Geometry, PaneStatus};

/// `pane-border-lines`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lines {
    Single,
    Double,
    Heavy,
    Simple,
    Number,
    Spaces,
    None,
}

impl Lines {
    /// The option's choice index (options-table.c `pane-border-lines`).
    #[must_use]
    pub fn from_choice(n: i64) -> Self {
        match n {
            1 => Self::Double,
            2 => Self::Heavy,
            3 => Self::Simple,
            4 => Self::Number,
            5 => Self::Spaces,
            6 => Self::None,
            _ => Self::Single,
        }
    }
}

/// `pane-border-indicators`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indicators {
    Off,
    Colour,
    Arrows,
    Both,
}

impl Indicators {
    #[must_use]
    pub fn from_choice(n: i64) -> Self {
        match n {
            0 => Self::Off,
            2 => Self::Arrows,
            3 => Self::Both,
            _ => Self::Colour,
        }
    }

    fn colour(self) -> bool {
        matches!(self, Self::Colour | Self::Both)
    }

    fn arrows(self) -> bool {
        matches!(self, Self::Arrows | Self::Both)
    }
}

/// Which neighbouring cells a border cell joins.
#[expect(clippy::struct_excessive_bools, reason = "one per side")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sides {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

impl Sides {
    fn merge(self, o: Self) -> Self {
        Self {
            left: self.left || o.left,
            right: self.right || o.right,
            up: self.up || o.up,
            down: self.down || o.down,
        }
    }
}

/// The shape of a border cell (tmux's `CELL_*` types), named by what it
/// looks like in single lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Junction {
    Vertical,
    Horizontal,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TeeDown,
    TeeUp,
    TeeRight,
    TeeLeft,
    Cross,
    /// No line (`CELL_NONE`).
    Dot,
}

impl Junction {
    /// `redraw_get_cell_type`: a line that only reaches one way is drawn
    /// as if it went both ways.
    fn from_sides(s: Sides) -> Self {
        match (s.left, s.right, s.up, s.down) {
            (true, true, true, true) => Self::Cross,
            (true, true, true, false) => Self::TeeUp,
            (true, true, false, true) => Self::TeeDown,
            (true, true | false, false, false) | (false, true, false, false) => Self::Horizontal,
            (true, false, true, true) => Self::TeeLeft,
            (true, false, true, false) => Self::BottomRight,
            (true, false, false, true) => Self::TopRight,
            (false, true, true, true) => Self::TeeRight,
            (false, true, true, false) => Self::BottomLeft,
            (false, true, false, true) => Self::TopLeft,
            (false, false, true, _) | (false, false, false, true) => Self::Vertical,
            (false, false, false, false) => Self::Dot,
        }
    }

    /// Position in tmux's border tables (`CELL_UD` .. `CELL_NONE`).
    fn index(self) -> usize {
        self as usize
    }
}

/// The glyph for a border cell (window-border.c `window_get_border_cell`;
/// UTF-8 forms from tty-acs.c). `index` is the pane's `pane_index` for
/// `number` lines, `None` when there is no pane.
#[must_use]
pub fn glyph(lines: Lines, junction: Junction, index: Option<u32>) -> String {
    // CELL_BORDERS " xqlkmjwvtun~" through tty_acs_table, then the double,
    // heavy and SIMPLE_BORDERS tables, all from CELL_UD.
    const SINGLE: [&str; 12] = ["│", "─", "┌", "┐", "└", "┘", "┬", "┴", "├", "┤", "┼", "·"];
    const DOUBLE: [&str; 12] = ["║", "═", "╔", "╗", "╚", "╝", "╦", "╩", "╠", "╣", "╬", "·"];
    const HEAVY: [&str; 12] = ["┃", "━", "┏", "┓", "┗", "┛", "┳", "┻", "┣", "┫", "╋", "·"];
    const SIMPLE: [&str; 12] = ["|", "-", "+", "+", "+", "+", "+", "+", "+", "+", "+", "."];
    let i = junction.index();
    match lines {
        Lines::Number if junction == Junction::Dot => "·".into(),
        Lines::Number => index.map_or_else(|| "*".into(), |n| (n % 10).to_string()),
        Lines::Double => DOUBLE[i].into(),
        Lines::Heavy => HEAVY[i].into(),
        Lines::Simple => SIMPLE[i].into(),
        Lines::None | Lines::Spaces => " ".into(),
        Lines::Single => SINGLE[i].into(),
    }
}

/// One visible pane as the scene needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScenePane {
    pub id: PaneId,
    /// `wp->xoff`, `wp->yoff`, `wp->sx`, `wp->sy`.
    pub rect: Geometry,
    pub floating: bool,
    pub lines: Lines,
    /// `window_pane_get_pane_status`.
    pub status: PaneStatus,
    /// Its scrollbar, when one is drawn.
    pub scrollbar: Option<crate::scrollbar::Placement>,
}

impl ScenePane {
    /// The columns of the pane's left and right borders: a reserved
    /// scrollbar's columns are inside them (`redraw_mark_pane_borders`,
    /// screen-redraw.c:688-695).
    fn border_columns(&self) -> (i32, i32) {
        let g = &self.rect;
        let (mut left, mut right) = (g.xoff - 1, g.xoff + int(g.sx));
        if let Some(sb) = self.scrollbar.filter(|sb| !sb.overlay) {
            if sb.left {
                left -= int(sb.width);
            } else {
                right += int(sb.width);
            }
        }
        (left, right)
    }
}

/// The window being drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowView {
    pub sx: u32,
    pub sy: u32,
    /// The window cell at the scene's top left: the client's view
    /// ([`crate::view::View`]; `redraw_get_window_offset`).
    pub ox: u32,
    pub oy: u32,
    /// Bottom of the z-order first: later panes are marked over earlier.
    pub panes: Vec<ScenePane>,
    pub indicators: Indicators,
    /// `redraw_check_two_pane_colours`: exactly two tiled panes, split this
    /// way.
    pub two_pane: Option<Dir>,
}

/// A border cell. `up`, `down`, `left`, `right` are the panes on each side
/// of it (`top_wp` ...).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Border {
    pub up: Option<PaneId>,
    pub down: Option<PaneId>,
    pub left: Option<PaneId>,
    pub right: Option<PaneId>,
    /// The pane whose style colours this cell, when fixed at build time.
    pub style: Option<PaneId>,
    sides: Sides,
    /// An indicator arrow is drawn here if it points at the active pane.
    pub arrow: bool,
}

impl Border {
    #[must_use]
    pub fn junction(&self) -> Junction {
        Junction::from_sides(self.sides)
    }

    fn has(&self, p: PaneId) -> bool {
        [self.up, self.down, self.left, self.right].contains(&Some(p))
    }

    /// `redraw_get_pane_for_border_style`: the fixed owner, else the active
    /// pane if it is adjacent, else the first of up, down, left, right.
    #[must_use]
    pub fn style_pane(&self, active: Option<PaneId>) -> Option<PaneId> {
        self.style
            .or(active.filter(|&a| self.has(a)))
            .or(self.up)
            .or(self.down)
            .or(self.left)
            .or(self.right)
    }

    /// `redraw_draw_border_arrow`: the arrow pointing at the active pane.
    #[must_use]
    pub fn arrow_glyph(&self, active: Option<PaneId>) -> Option<&'static str> {
        let active = active?;
        if !self.arrow {
            return None;
        }
        if self.left == Some(active) {
            Some("←")
        } else if self.right == Some(active) {
            Some("→")
        } else if self.up == Some(active) {
            Some("↑")
        } else if self.down == Some(active) {
            Some("↓")
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneCell {
    /// Beyond the window.
    Outside,
    /// Inside the window, nothing on it.
    Empty,
    Pane {
        pane: PaneId,
        px: u32,
        py: u32,
    },
    Border(Border),
    /// Column `col` of `pane`'s scrollbar (padding included), on row
    /// `row` of the bar.
    Scrollbar {
        pane: PaneId,
        col: u32,
        row: u32,
    },
    /// Column `offset` of `pane`'s status line, over a border of shape
    /// `under`.
    Status {
        pane: PaneId,
        offset: u32,
        under: Junction,
    },
}

/// The scene a client drew last and the view it came from
/// ([`Scene::cached`]): the scene only changes with the layout.
#[derive(Debug, Default)]
pub struct SceneCache(RefCell<Option<(WindowView, Rc<Scene>)>>);

#[derive(Clone, Debug)]
pub struct Scene {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<SceneCell>,
    wsx: i32,
    wsy: i32,
    /// The window cell at the scene's top left.
    ox: i32,
    oy: i32,
    /// Some cell is a pane status line's: without one, [`Self::status_line`]
    /// need not scan the scene (it runs per pane, every frame).
    has_status: bool,
    /// Per row, the pane content rows on it in order, each once per run
    /// ([`Self::row_panes`]).
    row_panes: Vec<Vec<(PaneId, u32)>>,
    /// Some cell is [`SceneCell::Outside`], some [`SceneCell::Empty`]
    /// ([`Self::fills`]).
    fills: (bool, bool),
}

fn int(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

impl Scene {
    /// [`Self::build`], or the scene `cache` holds when it was built
    /// from the same view at the same size.
    #[must_use]
    pub fn cached(cache: &SceneCache, win: WindowView, cols: u16, rows: u16) -> Rc<Self> {
        let mut slot = cache.0.borrow_mut();
        if let Some((view, scene)) = slot.as_ref()
            && *view == win
            && (scene.cols, scene.rows) == (cols, rows)
        {
            return Rc::clone(scene);
        }
        let scene = Rc::new(Self::build(&win, cols, rows));
        *slot = Some((win, Rc::clone(&scene)));
        scene
    }

    /// `redraw_build_cells` for a `cols` x `rows` area showing the window
    /// from cell (`win.ox`, `win.oy`).
    #[must_use]
    pub fn build(win: &WindowView, cols: u16, rows: u16) -> Self {
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        for y in 0..rows {
            for x in 0..cols {
                // redraw_reset_cell
                let (wx, wy) = (win.ox + u32::from(x), win.oy + u32::from(y));
                cells.push(if wx < win.sx && wy < win.sy {
                    SceneCell::Empty
                } else {
                    SceneCell::Outside
                });
            }
        }
        let mut scene = Self {
            cols,
            rows,
            cells,
            wsx: int(win.sx),
            wsy: int(win.sy),
            ox: int(win.ox),
            oy: int(win.oy),
            has_status: false,
            row_panes: Vec::new(),
            fills: (false, false),
        };
        for p in &win.panes {
            scene.mark_pane(p, win.indicators);
        }
        if win.indicators.colour()
            && let Some(dir) = win.two_pane
        {
            scene.mark_two_pane_colours(dir);
        }
        scene.has_status = scene
            .cells
            .iter()
            .any(|c| matches!(c, SceneCell::Status { .. }));
        scene.fills = (
            scene.cells.iter().any(|c| matches!(c, SceneCell::Outside)),
            scene.cells.iter().any(|c| matches!(c, SceneCell::Empty)),
        );
        scene.row_panes = (0..rows)
            .map(|y| {
                let mut on: Vec<(PaneId, u32)> = Vec::new();
                for x in 0..cols {
                    if let SceneCell::Pane { pane, py, .. } = scene.get(x, y)
                        && on.last() != Some(&(*pane, *py))
                    {
                        on.push((*pane, *py));
                    }
                }
                on
            })
            .collect();
        scene
    }

    /// Whether some cell is outside the window, and whether some is inside
    /// it but on no pane: without one, its fill cell is never drawn.
    #[must_use]
    pub fn fills(&self) -> (bool, bool) {
        self.fills
    }

    /// The pane content rows row `y` shows, left to right: (pane, content
    /// row), once for each run of the row's pane cells, the cells between
    /// runs skipped. The compositor's row cache keys on it every frame.
    #[must_use]
    pub fn row_panes(&self, y: u16) -> &[(PaneId, u32)] {
        self.row_panes
            .get(usize::from(y))
            .map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn get(&self, x: u16, y: u16) -> &SceneCell {
        &self.cells[usize::from(y) * usize::from(self.cols) + usize::from(x)]
    }

    /// `redraw_window_to_scene`. A window coordinate equal to the window
    /// size still passes the first test, as in tmux.
    fn index(&self, wx: i32, wy: i32) -> Option<usize> {
        if wx < 0 || wy < 0 || wx > self.wsx || wy > self.wsy {
            return None;
        }
        let x = u16::try_from(wx - self.ox)
            .ok()
            .filter(|x| *x < self.cols)?;
        let y = u16::try_from(wy - self.oy)
            .ok()
            .filter(|y| *y < self.rows)?;
        Some(usize::from(y) * usize::from(self.cols) + usize::from(x))
    }

    /// `redraw_pane_to_scene`: a floating pane is clipped at the window edge.
    fn pane_index(&self, p: &ScenePane, px: u32, py: u32) -> Option<usize> {
        let g = &p.rect;
        let (wx, wy) = (g.xoff + int(px), g.yoff + int(py));
        if p.floating {
            let (left, right) = (g.xoff - 1, g.xoff + int(g.sx));
            let (top, bottom) = (g.yoff - 1, g.yoff + int(g.sy));
            if (left < 0 && wx < 0)
                || (right > self.wsx && wx >= self.wsx)
                || (top < 0 && wy < 0)
                || (bottom > self.wsy && wy >= self.wsy)
            {
                return None;
            }
        }
        self.index(wx, wy)
    }

    /// `redraw_mark_pane`.
    fn mark_pane(&mut self, p: &ScenePane, indicators: Indicators) {
        for py in 0..p.rect.sy {
            for px in 0..p.rect.sx {
                if let Some(i) = self.pane_index(p, px, py) {
                    self.cells[i] = SceneCell::Pane { pane: p.id, px, py };
                }
            }
        }
        self.mark_borders(p, indicators);
        self.mark_scrollbar(p);
    }

    /// `redraw_mark_pane_scrollbar`: beside the pane, or over its edge
    /// columns when it is an overlay.
    fn mark_scrollbar(&mut self, p: &ScenePane) {
        let Some(sb) = p.scrollbar else {
            return;
        };
        let g = &p.rect;
        let w = int(sb.width);
        let start = match (sb.overlay, sb.left) {
            (true, true) => g.xoff,
            (true, false) => g.xoff + int(g.sx) - w,
            (false, true) => g.xoff - w,
            (false, false) => g.xoff + int(g.sx),
        };
        for row in 0..g.sy {
            for (col, wx) in (0..).zip(start..start + w) {
                if let Some(i) = self.index(wx, g.yoff + int(row)) {
                    self.cells[i] = SceneCell::Scrollbar {
                        pane: p.id,
                        col,
                        row,
                    };
                }
            }
        }
    }

    /// `redraw_mark_pane_borders`.
    fn mark_borders(&mut self, p: &ScenePane, indicators: Indicators) {
        if p.floating && p.lines == Lines::None {
            return;
        }
        let g = &p.rect;
        let (mut left, mut right) = p.border_columns();
        let (mut top, mut bottom) = (g.yoff - 1, g.yoff + int(g.sy));
        let mark_left = left >= 0;
        let mut mark_top = top >= 0;
        let (mark_right, mut mark_bottom);
        if p.floating {
            mark_right = right < self.wsx;
            mark_bottom = bottom < self.wsy;
            left = left.max(0);
            right = right.min(self.wsx - 1);
            top = top.max(0);
            bottom = bottom.min(self.wsy - 1);
        } else {
            mark_right = right <= self.wsx;
            mark_bottom = bottom <= self.wsy;
            if p.status == PaneStatus::Top && bottom < self.wsy {
                mark_bottom = false;
            } else if p.status == PaneStatus::Bottom {
                mark_top = false;
            }
        }
        let across = |wx: i32| Sides {
            left: wx > left,
            right: wx < right,
            ..Sides::default()
        };
        let down = |wy: i32| Sides {
            up: wy > top,
            down: wy < bottom,
            ..Sides::default()
        };
        if mark_top {
            for wx in left..=right {
                self.mark_cell(wx, top, p, (false, true), across(wx));
            }
        }
        if mark_bottom {
            for wx in left..=right {
                self.mark_cell(wx, bottom, p, (true, false), across(wx));
            }
        }
        if mark_left {
            for wy in top..=bottom {
                self.mark_cell(left, wy, p, (false, false), down(wy));
            }
        }
        if mark_right {
            for wy in top..=bottom {
                self.mark_cell(right, wy, p, (false, false), down(wy));
            }
        }
        self.mark_status(p, right, top, bottom);
        if indicators.arrows() {
            self.mark_arrows(p, (left, right, top, bottom));
        }
    }

    /// `redraw_mark_border_cell`. `owner` is (pane is above the cell, pane
    /// is below it).
    fn mark_cell(&mut self, wx: i32, wy: i32, p: &ScenePane, owner: (bool, bool), sides: Sides) {
        let Some(i) = self.index(wx, wy) else {
            return;
        };
        let reset = if p.floating {
            !matches!(&self.cells[i], SceneCell::Border(b) if b.has(p.id))
        } else {
            match self.cells[i] {
                SceneCell::Empty | SceneCell::Outside => true,
                SceneCell::Border(_) => false,
                _ => return,
            }
        };
        if reset {
            self.cells[i] = SceneCell::Border(Border::default());
        }
        let SceneCell::Border(b) = &mut self.cells[i] else {
            unreachable!("just made a border");
        };
        if owner.0 {
            b.up = Some(p.id);
        }
        if owner.1 {
            b.down = Some(p.id);
        }
        if sides.up || sides.down {
            if wx < p.rect.xoff {
                b.right = Some(p.id);
            } else if wx >= p.rect.xoff + int(p.rect.sx) {
                b.left = Some(p.id);
            }
        }
        b.sides = b.sides.merge(sides);
    }

    /// `redraw_mark_border_status`: the status line takes over the border
    /// cells from two columns in to one before the right edge.
    fn mark_status(&mut self, p: &ScenePane, right: i32, top: i32, bottom: i32) {
        let wy = match p.status {
            PaneStatus::Off => return,
            PaneStatus::Top => top,
            PaneStatus::Bottom => bottom,
        };
        for (offset, wx) in (0..).zip(p.rect.xoff + 2..right) {
            let Some(i) = self.index(wx, wy) else {
                continue;
            };
            if let SceneCell::Border(b) = &self.cells[i] {
                self.cells[i] = SceneCell::Status {
                    pane: p.id,
                    offset,
                    under: b.junction(),
                };
            }
        }
    }

    /// `redraw_mark_border_arrows`: the second cell of each edge.
    fn mark_arrows(&mut self, p: &ScenePane, (left, right, top, bottom): (i32, i32, i32, i32)) {
        let mut mark = |wx: i32, wy: i32| {
            if let Some(i) = self.index(wx, wy)
                && let SceneCell::Border(b) = &mut self.cells[i]
            {
                b.arrow = true;
            }
        };
        let wx = p.rect.xoff + 1;
        if (left..=right).contains(&wx) {
            mark(wx, top);
            mark(wx, bottom);
        }
        let wy = p.rect.yoff + 1;
        if (top..=bottom).contains(&wy) {
            mark(left, wy);
            mark(right, wy);
        }
    }

    /// `redraw_mark_two_pane_colours`: each half of the divider takes the
    /// style of the pane on its side.
    fn mark_two_pane_colours(&mut self, dir: Dir) {
        let (half_x, half_y) = (self.wsx / 2, self.wsy / 2);
        let cols = usize::from(self.cols);
        for (i, cell) in self.cells.iter_mut().enumerate() {
            let SceneCell::Border(b) = cell else {
                continue;
            };
            let (wx, wy) = (self.ox + int_usize(i % cols), self.oy + int_usize(i / cols));
            match dir {
                Dir::LeftRight if b.left.is_some() && b.right.is_some() => {
                    b.style = if wy <= half_y { b.left } else { b.right };
                }
                Dir::TopBottom if b.up.is_some() && b.down.is_some() => {
                    b.style = if wx <= half_x { b.up } else { b.down };
                }
                _ => {}
            }
        }
    }

    /// `redraw_pane_status_width` and `redraw_get_status_border_cell_type`:
    /// the width of `pane`'s status line and the border shape under each
    /// column (`None` where no status cell is visible: drawn as a line).
    #[must_use]
    pub fn status_line(&self, pane: PaneId) -> Vec<Option<Junction>> {
        let mut out: Vec<Option<Junction>> = Vec::new();
        if !self.has_status {
            return out;
        }
        for cell in &self.cells {
            if let SceneCell::Status {
                pane: p,
                offset,
                under,
            } = cell
                && *p == pane
            {
                let at = usize::try_from(*offset).unwrap_or(usize::MAX);
                if out.len() <= at {
                    out.resize(at + 1, None);
                }
                out[at] = Some(*under);
            }
        }
        out
    }
}

fn int_usize(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u32, sx: u32, sy: u32, xoff: i32, yoff: i32) -> ScenePane {
        ScenePane {
            id: PaneId(id),
            rect: Geometry::new(sx, sy, xoff, yoff),
            floating: false,
            lines: Lines::Single,
            status: PaneStatus::Off,
            scrollbar: None,
        }
    }

    fn text(scene: &Scene, active: Option<PaneId>) -> Vec<String> {
        (0..scene.rows)
            .map(|y| {
                (0..scene.cols)
                    .map(|x| match scene.get(x, y) {
                        SceneCell::Pane { pane, .. } => pane.0.to_string(),
                        SceneCell::Border(b) => b
                            .arrow_glyph(active)
                            .map_or_else(|| glyph(Lines::Single, b.junction(), None), Into::into),
                        SceneCell::Status { .. } => "s".into(),
                        SceneCell::Scrollbar { .. } => "S".into(),
                        SceneCell::Empty => ".".into(),
                        SceneCell::Outside => "#".into(),
                    })
                    .collect()
            })
            .collect()
    }

    /// The shape of the reference 3.8 screen for `splitw -h; splitw -v`
    /// with `pane-border-indicators both` (full size in the oracle's
    /// `screen_split_borders`), here at 9x5: shared junction `├`, arrows on the active pane's edges.
    #[test]
    fn three_panes_share_borders() {
        let win = WindowView {
            sx: 9,
            sy: 5,
            ox: 0,
            oy: 0,
            panes: vec![
                pane(0, 4, 5, 0, 0),
                pane(1, 4, 2, 5, 0),
                pane(2, 4, 2, 5, 3),
            ],
            indicators: Indicators::Both,
            two_pane: None,
        };
        let scene = Scene::build(&win, 9, 5);
        assert_eq!(
            text(&scene, Some(PaneId(2))),
            [
                "0000│1111",
                "0000│1111",
                "0000├─↓──",
                "0000│2222",
                "0000→2222",
            ]
        );
        let SceneCell::Border(b) = scene.get(4, 0) else {
            panic!()
        };
        assert_eq!(b.style_pane(Some(PaneId(2))), Some(PaneId(0)));
        let SceneCell::Border(b) = scene.get(4, 2) else {
            panic!()
        };
        assert_eq!(b.style_pane(Some(PaneId(2))), Some(PaneId(2)));
    }

    /// `redraw_window_to_scene` with a view offset: the scene is the
    /// window from (`ox`, `oy`), with the window's right and bottom edge
    /// borders and the cells past them outside; pane cells keep their pane
    /// coordinates, and the divider's colour halves are by window row.
    #[test]
    fn a_scene_shows_the_window_from_its_offset() {
        let win = WindowView {
            sx: 9,
            sy: 5,
            ox: 3,
            oy: 2,
            panes: vec![pane(0, 4, 5, 0, 0), pane(1, 4, 5, 5, 0)],
            indicators: Indicators::Colour,
            two_pane: Some(Dir::LeftRight),
        };
        let scene = Scene::build(&win, 8, 4);
        assert_eq!(
            text(&scene, None),
            ["0│1111│#", "0│1111│#", "0│1111│#", "─┴────┘#"]
        );
        assert_eq!(
            scene.get(0, 0),
            &SceneCell::Pane {
                pane: PaneId(0),
                px: 3,
                py: 2,
            }
        );
        let owner = |y| match scene.get(1, y) {
            SceneCell::Border(b) => b.style_pane(None),
            _ => None,
        };
        // Window rows 2, 3, 4 against half 5 / 2 = 2.
        assert_eq!(
            [owner(0), owner(1), owner(2)],
            [Some(PaneId(0)), Some(PaneId(1)), Some(PaneId(1))]
        );
    }

    /// `redraw_mark_two_pane_colours`: the divider's top half takes the
    /// left pane's style, the bottom half the right pane's (`wy <= sy/2`).
    #[test]
    fn two_panes_split_the_divider_colour() {
        let win = WindowView {
            sx: 5,
            sy: 4,
            ox: 0,
            oy: 0,
            panes: vec![pane(0, 2, 4, 0, 0), pane(1, 2, 4, 3, 0)],
            indicators: Indicators::Colour,
            two_pane: Some(Dir::LeftRight),
        };
        let scene = Scene::build(&win, 5, 4);
        let owner = |y| match scene.get(2, y) {
            SceneCell::Border(b) => b.style_pane(Some(PaneId(1))),
            _ => None,
        };
        assert_eq!(
            [owner(0), owner(1), owner(2), owner(3)],
            [
                Some(PaneId(0)),
                Some(PaneId(0)),
                Some(PaneId(0)),
                Some(PaneId(1))
            ]
        );
    }

    /// A top status line starts two columns in and stops before the right
    /// border; its columns remember the junction under them.
    #[test]
    fn status_line_over_the_top_border() {
        let mut a = pane(0, 4, 3, 0, 1);
        let mut b = pane(1, 4, 3, 5, 1);
        a.status = PaneStatus::Top;
        b.status = PaneStatus::Top;
        let win = WindowView {
            sx: 9,
            sy: 4,
            ox: 0,
            oy: 0,
            panes: vec![a, b],
            indicators: Indicators::Off,
            two_pane: Some(Dir::LeftRight),
        };
        let scene = Scene::build(&win, 9, 4);
        assert_eq!(text(&scene, None)[0], "──ss┬──ss");
        assert_eq!(text(&scene, None)[1], "0000│1111");
        assert_eq!(
            scene.status_line(PaneId(0)),
            [Some(Junction::Horizontal); 2]
        );
    }

    #[test]
    fn glyph_tables() {
        assert_eq!(glyph(Lines::Heavy, Junction::Cross, None), "╋");
        assert_eq!(glyph(Lines::Double, Junction::TeeRight, None), "╠");
        assert_eq!(glyph(Lines::Simple, Junction::Vertical, None), "|");
        assert_eq!(glyph(Lines::Number, Junction::Vertical, Some(12)), "2");
        assert_eq!(glyph(Lines::Number, Junction::Vertical, None), "*");
        assert_eq!(glyph(Lines::Spaces, Junction::Cross, None), " ");
    }
}
