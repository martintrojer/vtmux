//! The screen of `panes-mode` (window-panes.c:709-875 `window_panes_draw_*`):
//! each tiled pane of the source window in its place, scaled down when the
//! screen is smaller than the window, with its number over it, and the
//! borders between the panes from `display-panes-border-style`.
//!
//! Floating panes are not drawn: vtmux cannot make them yet.

use vtmux_core::colour::Colour;
use vtmux_core::ids::{PaneId, WindowId};
use vtmux_core::layout::{Cell as LayoutCell, Dir, Geometry, Layout, PaneStatus};
use vtmux_core::options::TreeId;
use vtmux_core::style::CellStyle;
use vtmux_core::styled;
use vtmux_vt::{Cell, Snapshot, Style};

use super::Caps;
use super::status::KEEP;
use super::window::Source;
use crate::mode::PanesMode;
use crate::mode::panes::source_link;
use crate::model::Pane;
use crate::target::Found;

const L: u8 = 1;
const R: u8 = 2;
const U: u8 = 4;
const D: u8 = 8;

/// tty-acs.c's UTF-8 forms of `CELL_BORDERS` (tmux.h:857), by the
/// directions a border cell joins.
fn border_glyph(mask: u8) -> &'static str {
    match mask {
        m if m == L | R | U | D => "┼",
        m if m == L | R | U => "┴",
        m if m == L | R | D => "┬",
        m if m == L | R || m == L || m == R => "─",
        m if m == L | U | D => "┤",
        m if m == L | U => "┘",
        m if m == L | D => "┐",
        m if m == R | U | D => "├",
        m if m == R | U => "└",
        m if m == R | D => "┌",
        _ => "│",
    }
}

/// The map of border joins over the screen.
struct Borders {
    map: Vec<u8>,
    dsx: u32,
    dsy: u32,
    osx: u32,
    osy: u32,
}

impl Borders {
    fn map_x(&self, x: i64) -> i64 {
        if self.osx <= self.dsx {
            x
        } else {
            x * i64::from(self.dsx) / i64::from(self.osx)
        }
    }

    fn map_y(&self, y: i64) -> i64 {
        if self.osy <= self.dsy {
            y
        } else {
            y * i64::from(self.dsy) / i64::from(self.osy)
        }
    }

    fn at(&self, x: i64, y: i64) -> Option<usize> {
        let ok = (0..i64::from(self.dsx)).contains(&x) && (0..i64::from(self.dsy)).contains(&y);
        ok.then(|| usize::try_from(y * i64::from(self.dsx) + x).unwrap_or(0))
    }

    fn mark(&mut self, x: i64, y: i64, mask: u8) {
        if let Some(i) = self.at(x, y) {
            self.map[i] |= mask;
        }
    }

    /// `window_panes_mark_vline`.
    fn vline(&mut self, x: i64, y: i64, y2: i64) {
        if x < 0 || x >= i64::from(self.dsx) || y2 <= y {
            return;
        }
        let y = y.max(0);
        let y2 = y2.min(i64::from(self.dsy));
        for yy in y..y2 {
            let mut mask = 0;
            if yy > y {
                mask |= U;
            }
            if yy + 1 < y2 {
                mask |= D;
            }
            if mask == 0 {
                mask = U | D;
            }
            self.mark(x, yy, mask);
        }
    }

    /// `window_panes_mark_hline`.
    fn hline(&mut self, x: i64, x2: i64, y: i64) {
        if y < 0 || y >= i64::from(self.dsy) || x2 <= x {
            return;
        }
        let x = x.max(0);
        let x2 = x2.min(i64::from(self.dsx));
        for xx in x..x2 {
            let mut mask = 0;
            if xx > x {
                mask |= L;
            }
            if xx + 1 < x2 {
                mask |= R;
            }
            if mask == 0 {
                mask = L | R;
            }
            self.mark(xx, y, mask);
        }
    }

    /// `window_panes_mark_borders_cell`: a line between each tiled child
    /// of a node and the next.
    fn cells(&mut self, lc: &LayoutCell) {
        let LayoutCell::Node(n) = lc else { return };
        for (i, child) in n.children.iter().enumerate() {
            self.cells(child);
            if child.is_floating() || !n.children[i + 1..].iter().any(|c| !c.is_floating()) {
                continue;
            }
            let (cg, g) = (child.g(), &n.g);
            if n.dir == Dir::LeftRight {
                let x = self.map_x(i64::from(cg.xoff) + i64::from(cg.sx));
                let y = self.map_y(i64::from(g.yoff));
                let y2 = self.map_y(i64::from(g.yoff) + i64::from(g.sy));
                self.vline(x, y, y2);
            } else {
                let x = self.map_x(i64::from(g.xoff));
                let x2 = self.map_x(i64::from(g.xoff) + i64::from(g.sx));
                let y = self.map_y(i64::from(cg.yoff) + i64::from(cg.sy));
                self.hline(x, x2, y);
            }
        }
    }

    /// `window_panes_mark_border_joins_cell`: where a border meets a
    /// perpendicular one, each gets the arm toward the other.
    fn joins(&mut self, lc: &LayoutCell) {
        let LayoutCell::Node(n) = lc else { return };
        let horizontal = |m: u8| m & (L | R) != 0;
        let vertical = |m: u8| m & (U | D) != 0;
        for (i, child) in n.children.iter().enumerate() {
            self.joins(child);
            if child.is_floating() || !n.children[i + 1..].iter().any(|c| !c.is_floating()) {
                continue;
            }
            let (cg, g) = (child.g(), &n.g);
            if n.dir == Dir::LeftRight {
                let x = self.map_x(i64::from(cg.xoff) + i64::from(cg.sx));
                let y = self.map_y(i64::from(g.yoff));
                let y2 = self.map_y(i64::from(g.yoff) + i64::from(g.sy));
                if x < 0 || x >= i64::from(self.dsx) {
                    continue;
                }
                if y > 0 && self.at(x, y - 1).is_some_and(|i| horizontal(self.map[i])) {
                    self.mark(x, y - 1, D);
                    self.mark(x, y, U);
                }
                if y2 < i64::from(self.dsy)
                    && self.at(x, y2).is_some_and(|i| horizontal(self.map[i]))
                {
                    self.mark(x, y2, U);
                    self.mark(x, y2 - 1, D);
                }
            } else {
                let x = self.map_x(i64::from(g.xoff));
                let x2 = self.map_x(i64::from(g.xoff) + i64::from(g.sx));
                let y = self.map_y(i64::from(cg.yoff) + i64::from(cg.sy));
                if y < 0 || y >= i64::from(self.dsy) {
                    continue;
                }
                if x > 0 && self.at(x - 1, y).is_some_and(|i| vertical(self.map[i])) {
                    self.mark(x - 1, y, R);
                    self.mark(x, y, L);
                }
                if x2 < i64::from(self.dsx) && self.at(x2, y).is_some_and(|i| vertical(self.map[i]))
                {
                    self.mark(x2, y, L);
                    self.mark(x2 - 1, y, R);
                }
            }
        }
    }

    /// `window_panes_mark_pane_status_borders`: the line a pane's status
    /// takes at the window's top or bottom edge.
    fn status_lines(&mut self, layout: &Layout, panes: &[PaneId]) {
        let status = layout.opts.status;
        if status == PaneStatus::Off {
            return;
        }
        for &p in panes {
            let Some(path) = layout.root.find_pane(p) else {
                continue;
            };
            if !layout.pane_needs_border(p) {
                continue;
            }
            let g = layout.root.get(&path).g();
            let x = self.map_x(i64::from(g.xoff));
            let x2 = self.map_x(i64::from(g.xoff) + i64::from(g.sx));
            let y = if status == PaneStatus::Top {
                self.map_y(i64::from(g.yoff))
            } else {
                self.map_y(i64::from(g.yoff) + i64::from(g.sy)) - 1
            };
            self.hline(x, x2, y);
        }
    }
}

/// A pane's place on the screen.
struct Area {
    x: u32,
    y: u32,
    sx: u32,
    sy: u32,
}

/// `window_panes_get_geometry` (window-panes.c:171-228). `border` is
/// whether the pane's cell takes a row for its status line, which the
/// area gives up.
fn geometry(
    g: &Geometry,
    (osx, osy): (u32, u32),
    (dsx, dsy): (u32, u32),
    border: Option<PaneStatus>,
) -> Option<Area> {
    if osx == 0 || osy == 0 || dsx == 0 || dsy == 0 {
        return None;
    }
    let (gx, gy) = (
        u64::from(g.xoff.unsigned_abs()),
        u64::from(g.yoff.unsigned_abs()),
    );
    let (gsx, gsy) = (u64::from(g.sx), u64::from(g.sy));
    let (osx, osy, dsx, dsy) = (
        u64::from(osx),
        u64::from(osy),
        u64::from(dsx),
        u64::from(dsy),
    );
    let (x, y, mut x2, mut y2) = if osx <= dsx && osy <= dsy {
        (gx, gy, gx + gsx, gy + gsy)
    } else {
        (
            gx * dsx / osx,
            gy * dsy / osy,
            (gx + gsx) * dsx / osx,
            (gy + gsy) * dsy / osy,
        )
    };
    if x >= dsx || y >= dsy {
        return None;
    }
    if x2 <= x {
        x2 = x + 1;
    }
    if y2 <= y {
        y2 = y + 1;
    }
    x2 = x2.min(dsx);
    y2 = y2.min(dsy);
    let (sx, mut sy, mut y) = (x2 - x, y2 - y, y);
    if let Some(status) = border
        && sy > 1
    {
        if status == PaneStatus::Top {
            y += 1;
        }
        sy -= 1;
    }
    let n = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
    Some(Area {
        x: n(x),
        y: n(y),
        sx: n(sx),
        sy: n(sy),
    })
}

/// The screen being drawn.
struct Canvas {
    snap: Snapshot,
    /// The write position (`s->cx`, `s->cy`), which is where the terminal
    /// cursor is left: hidden, but `tty_cursor` still goes there.
    cx: u32,
    cy: u32,
}

impl Canvas {
    /// `screen_write_cursormove` then `screen_write_cell`: the cursor is
    /// left after the cell.
    fn put(&mut self, x: u32, y: u32, cell: Cell) {
        self.cx = x + u32::from(cell.width.max(1));
        self.cy = y;
        self.set(x, y, cell);
    }

    fn set(&mut self, x: u32, y: u32, cell: Cell) {
        if x < u32::from(self.snap.cols) && y < u32::from(self.snap.rows) {
            let i = y as usize * usize::from(self.snap.cols) + x as usize;
            self.snap.cells[i] = cell;
        }
    }

    /// `screen_write_puts`: text at `x`, `y` in `style`.
    fn puts(&mut self, mut x: u32, y: u32, text: &str, style: Style) {
        for c in text.chars() {
            let width = vtmux_core::width::char_width(c);
            self.put(
                x,
                y,
                Cell {
                    text: c.to_string(),
                    width: width.max(1),
                    style,
                    ..Cell::default()
                },
            );
            x += u32::from(width.max(1));
        }
    }

    /// `screen_write_fast_copy` of `src` from its top left into the
    /// `nx` by `ny` area at `x`, `y`.
    fn copy(&mut self, src: &Snapshot, x: u32, y: u32, nx: u32, ny: u32) {
        self.cx = x;
        self.cy = y;
        self.copy_from(src, (0, 0), (x, y), (nx, ny));
    }

    fn copy_from(
        &mut self,
        src: &Snapshot,
        (px, py): (u32, u32),
        (x, y): (u32, u32),
        (nx, ny): (u32, u32),
    ) {
        for yy in 0..ny {
            for xx in 0..nx {
                let Some(c) = u16::try_from(px + xx)
                    .ok()
                    .zip(u16::try_from(py + yy).ok())
                    .and_then(|(sx, sy)| src.cell(sx, sy))
                else {
                    continue;
                };
                // A wide character that would not fit stops the row.
                if u32::from(c.width) + xx > nx {
                    break;
                }
                self.set(x + xx, y + yy, c.clone());
            }
        }
    }

    /// `screen_write_preview` (screen-write.c:937-990): the area around the
    /// cursor of `src` if it shows one, else its top left, and the
    /// cursor's cell reversed.
    fn preview(&mut self, src: &Snapshot, x: u32, y: u32, nx: u32, ny: u32) {
        let cursor = src.cursor.filter(|_| src.cursor_visible);
        let (mut px, mut py) = (0, 0);
        if let Some((cx, cy)) = cursor {
            let (cols, rows) = (u32::from(src.cols), u32::from(src.rows));
            let window = |c: u32, n: u32, size: u32| {
                let mut p = c.saturating_sub(n / 3);
                if c < n / 3 {
                    p = 0;
                }
                if p + n > size {
                    p = size.saturating_sub(n);
                }
                p
            };
            px = window(u32::from(cx), nx, cols);
            py = window(u32::from(cy), ny, rows);
        }
        self.cx = x;
        self.cy = y;
        self.copy_from(src, (px, py), (x, y), (nx, ny));
        if let Some((cx, cy)) = cursor {
            let mut cell = src.cell(cx, cy).cloned().unwrap_or_default();
            if cell.text.is_empty() {
                cell.text = " ".into();
                cell.width = 1;
            }
            cell.style.inverse = true;
            self.put(x + u32::from(cx) - px, y + u32::from(cy) - py, cell);
        }
    }
}

/// `clock-mode` and `display-panes`'s 5x5 digits (window-clock.c:54-70).
#[rustfmt::skip]
const DIGITS: [[u8; 5]; 10] = [
    [0b11111, 0b10001, 0b10001, 0b10001, 0b11111], // 0
    [0b00001, 0b00001, 0b00001, 0b00001, 0b00001], // 1
    [0b11111, 0b00001, 0b11111, 0b10000, 0b11111], // 2
    [0b11111, 0b00001, 0b11111, 0b00001, 0b11111], // 3
    [0b10001, 0b10001, 0b11111, 0b00001, 0b00001], // 4
    [0b11111, 0b10000, 0b11111, 0b00001, 0b11111], // 5
    [0b11111, 0b10000, 0b11111, 0b10001, 0b11111], // 6
    [0b11111, 0b00001, 0b00001, 0b00001, 0b00001], // 7
    [0b11111, 0b10001, 0b11111, 0b10001, 0b11111], // 8
    [0b11111, 0b10001, 0b11111, 0b00001, 0b11111], // 9
];

impl Source<'_> {
    /// The screen of pane `pane` in panes mode `m`
    /// (`window_panes_draw_screen`).
    pub(super) fn panes_screen(&self, pane: &Pane, m: &PanesMode, caps: Caps) -> Snapshot {
        let (cols, rows) = (pane.term.cols(), pane.term.rows());
        let blank = Cell {
            width: 1,
            ..Cell::default()
        };
        let mut canvas = Canvas {
            snap: Snapshot {
                cols,
                rows,
                cells: vec![blank; usize::from(cols) * usize::from(rows)],
                wrapped: Vec::new(),
                cursor: None,
                cursor_visible: false,
            },
            cx: 0,
            cy: 0,
        };
        let Some(window) = self.model.windows.get(&m.source_window) else {
            return canvas.snap;
        };
        let layout = window.full_layout();
        let os = (layout.sx, layout.sy);
        let ds = (u32::from(cols), u32::from(rows));
        let theme = self.theme(caps);
        let wtree = TreeId::Window(pane.window);
        let tiled: Vec<PaneId> = window
            .panes
            .panes
            .iter()
            .copied()
            .filter(|p| !window.is_floating(*p))
            .collect();
        for &q in &tiled {
            self.panes_draw_pane(&mut canvas, pane, m, q, (layout, os, ds), caps);
        }
        // The borders over the panes.
        let mut borders = Borders {
            map: vec![0; usize::from(cols) * usize::from(rows)],
            dsx: ds.0,
            dsy: ds.1,
            osx: os.0,
            osy: os.1,
        };
        borders.cells(&layout.root);
        borders.status_lines(layout, &tiled);
        borders.joins(&layout.root);
        let found = self.panes_found(m, window.id, pane.id);
        let ctx = self.ctx(found, caps);
        let style = theme.style(&self.style(wtree, "display-panes-border-style", &ctx));
        for (i, &mask) in borders.map.iter().enumerate() {
            if mask != 0 {
                let (x, y) = (i % usize::from(cols), i / usize::from(cols));
                canvas.put(
                    u32::try_from(x).unwrap_or(0),
                    u32::try_from(y).unwrap_or(0),
                    Cell {
                        text: border_glyph(mask).into(),
                        width: 1,
                        style,
                        acs: true,
                        link: false,
                    },
                );
            }
        }
        // MODE_CURSOR is off: the cursor is hidden where drawing left it.
        canvas.snap.cursor = Some((
            u16::try_from(canvas.cx.min(u32::from(cols))).unwrap_or(0),
            u16::try_from(canvas.cy.min(u32::from(rows.saturating_sub(1)))).unwrap_or(0),
        ));
        canvas.snap
    }

    /// The session and window index `window_panes_get_source` finds, for
    /// formats about pane `p`.
    fn panes_found(&self, m: &PanesMode, w: WindowId, p: PaneId) -> Found {
        let (session, index) = source_link(self.model, m).unzip();
        Found {
            session,
            index,
            window: Some(w),
            pane: Some(p),
            new_index: None,
        }
    }

    /// `window_panes_draw_pane`.
    fn panes_draw_pane(
        &self,
        canvas: &mut Canvas,
        mode_pane: &Pane,
        m: &PanesMode,
        q: PaneId,
        (layout, os, ds): (&Layout, (u32, u32), (u32, u32)),
        caps: Caps,
    ) {
        let Some(path) = layout.root.find_pane(q) else {
            return;
        };
        let border = layout.pane_needs_border(q).then_some(layout.opts.status);
        let Some(a) = geometry(layout.root.get(&path).g(), os, ds, border) else {
            return;
        };
        let Some(index) = self.model.pane_index(q) else {
            return;
        };
        let live;
        let src = match (q == mode_pane.id, m.preview.as_ref(), m.frozen.get(&q)) {
            (true, Some(pre), _) if a.sx <= u32::from(pre.cols) && a.sy <= u32::from(pre.rows) => {
                pre
            }
            (_, _, Some(frozen)) => frozen,
            _ => {
                live = vtmux_vt::snapshot(&self.model.panes[&q].term);
                &live
            }
        };
        if os.0 <= ds.0 && os.1 <= ds.1 {
            canvas.copy(src, a.x, a.y, a.sx, a.sy);
        } else {
            canvas.preview(src, a.x, a.y, a.sx, a.sy);
        }
        self.panes_draw_number(canvas, mode_pane, m, q, index, &a, caps);
    }

    /// `window_panes_draw_number`.
    #[allow(clippy::too_many_arguments)] // the C's own
    fn panes_draw_number(
        &self,
        canvas: &mut Canvas,
        mode_pane: &Pane,
        m: &PanesMode,
        q: PaneId,
        index: u32,
        a: &Area,
        caps: Caps,
    ) {
        let digits = index.to_string();
        let len = u32::try_from(digits.len()).unwrap_or(u32::MAX);
        let letter = (10..35)
            .contains(&index)
            .then(|| char::from(b'a' + u8::try_from(index - 10).unwrap_or(0)));
        if a.sx < len {
            return;
        }
        let Some(window) = self.model.windows.get(&m.source_window) else {
            return;
        };
        let wtree = TreeId::Window(mode_pane.window);
        let found = self.panes_found(m, m.source_window, q);
        let ctx = self.ctx(found, caps);
        let name = if window.active() == q {
            "display-panes-active-colour"
        } else {
            "display-panes-colour"
        };
        // OPTIONS_TABLE_IS_COLOUR: the whole value is one colour; one that
        // does not parse is the default.
        let text = self.string(wtree, name);
        let text = if text.contains("#{") {
            vtmux_core::format::expand_str(&text, &ctx)
        } else {
            text
        };
        let theme = self.theme(caps);
        let colour = Colour::parse(&text).unwrap_or(Colour::Default);
        let fg = theme.colour(colour);
        let text_style = Style {
            fg,
            ..Style::default()
        };
        let block = Style {
            bg: fg,
            ..Style::default()
        };
        let format = !self.string(wtree, "display-panes-format").is_empty();
        let big = len * 6 - 1;
        if a.sx < big || a.sy < if format { 7 } else { 5 } {
            let mut width = len;
            if letter.is_some() && a.sx >= len + 2 {
                width += 2;
            }
            let cx = a.x + (a.sx - width) / 2;
            let cy = a.y + a.sy / 2;
            canvas.puts(cx, cy, &digits, text_style);
            if let (true, Some(l)) = (width > len, letter) {
                canvas.puts(cx + len, cy, &format!(" {l}"), text_style);
            }
            if format && a.sy > 1 {
                self.panes_draw_format(canvas, wtree, &found, a, colour, caps);
            }
            return;
        }
        let mut px = (a.sx - big) / 2;
        let py = (a.sy - 5) / 2;
        for ch in digits.chars() {
            let rows = DIGITS[ch.to_digit(10).unwrap_or(0) as usize];
            for (j, bits) in rows.iter().enumerate() {
                for i in 0..5 {
                    if bits >> (4 - i) & 1 == 1 {
                        canvas.put(
                            a.x + px + i,
                            a.y + py + u32::try_from(j).unwrap_or(0),
                            Cell {
                                text: " ".into(),
                                width: 1,
                                style: block,
                                ..Cell::default()
                            },
                        );
                    }
                }
            }
            px += 6;
        }
        if a.sy <= 6 {
            return;
        }
        self.panes_draw_format(canvas, wtree, &found, a, colour, caps);
        if let Some(l) = letter {
            canvas.puts(a.x + px - 2, a.y + py + 5, &l.to_string(), text_style);
        }
    }

    /// `window_panes_draw_format`: `display-panes-format` (the mode pane's
    /// window's) for the pane across the top row of its area, in the
    /// number's colour.
    fn panes_draw_format(
        &self,
        canvas: &mut Canvas,
        wtree: TreeId,
        found: &Found,
        a: &Area,
        colour: Colour,
        caps: Caps,
    ) {
        if a.sx == 0 || found.session.is_none() {
            return;
        }
        // screen_write_cursormove(x, y), then format_draw puts it back.
        canvas.cx = a.x;
        canvas.cy = a.y;
        let format = self.string(wtree, "display-panes-format");
        if format.is_empty() {
            return;
        }
        let ctx = self.ctx(*found, caps);
        let expanded = vtmux_core::format::expand_str(&format, &ctx);
        if expanded.is_empty() {
            return;
        }
        let base = CellStyle {
            fg: colour,
            ..CellStyle::default()
        };
        let under = vec![
            styled::Cell {
                text: KEEP.into(),
                width: 1,
                style: base.clone(),
            };
            a.sx as usize
        ];
        let drawn = styled::draw_onto(&base, under, &expanded, false, false);
        let theme = self.theme(caps);
        for (i, c) in drawn.cells.iter().enumerate() {
            if c.text != KEEP {
                canvas.set(a.x + u32::try_from(i).unwrap_or(0), a.y, theme.cell(c));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values from tmux 3.8 window-panes.c:171-228 read as the
    /// spec, and the oracle scenarios `panes_mode_*` for the screens.
    #[test]
    fn geometry_scales_only_into_a_smaller_screen() {
        let g = Geometry::new(39, 10, 41, 12);
        let area = |os, ds, b| geometry(&g, os, ds, b).map(|a| (a.x, a.y, a.sx, a.sy));
        assert_eq!(area((80, 24), (80, 24), None), Some((41, 12, 39, 10)));
        assert_eq!(area((80, 24), (40, 12), None), Some((20, 6, 20, 5)));
        // The status row at the top is the first row of the area's cell.
        assert_eq!(
            area((80, 24), (80, 24), Some(PaneStatus::Top)),
            Some((41, 13, 39, 9))
        );
        assert_eq!(
            area((80, 24), (80, 24), Some(PaneStatus::Bottom)),
            Some((41, 12, 39, 9))
        );
        assert_eq!(area((80, 24), (0, 24), None), None);
        // A cell that scales to nothing keeps one cell.
        assert_eq!(
            geometry(&Geometry::new(1, 1, 79, 23), (80, 24), (8, 2), None)
                .map(|a| (a.x, a.y, a.sx, a.sy)),
            Some((7, 1, 1, 1))
        );
    }

    #[test]
    fn border_cells_join_the_arms_they_have() {
        assert_eq!(border_glyph(L | R | U | D), "┼");
        assert_eq!(border_glyph(U | D), "│");
        assert_eq!(border_glyph(L), "─");
        assert_eq!(border_glyph(R | U | D), "├");
        assert_eq!(border_glyph(L | U | D), "┤");
        assert_eq!(border_glyph(L | R | U), "┴");
        assert_eq!(border_glyph(R | D), "┌");
    }

    /// window-clock.c's table: one is a bar on the right, and every digit
    /// is five rows of five columns.
    #[test]
    fn digits_are_the_clock_table() {
        assert_eq!(DIGITS[1], [1; 5]);
        assert_eq!(DIGITS[0][0], 0b11111);
        assert_eq!(DIGITS[8][2], 0b11111);
        assert!(DIGITS.iter().flatten().all(|row| *row < 32));
    }
}
