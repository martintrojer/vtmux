//! Split, resize and close arithmetic, ported from layout.c (3.8). Every
//! function mirrors the C one it names, including its unsigned wrap-around
//! (`wrapping_*`): the result must match tmux cell for cell.
//!
//! tmux asks "is this cell at the top/bottom of the window?" by walking up to
//! the root (`layout_add_horizontal_border`); here the walk goes down and
//! carries a [`Pos`], which gives the same answer.

use std::fmt;

use super::cell::{
    Cell, Dir, Geometry, Leaf, Node, PANE_MAXIMUM, PANE_MINIMUM, first_tiled, last_tiled,
    neighbour, neighbour_dir,
};
use super::{Layout, PaneStatus};
use crate::cmd::NumError;
use crate::ids::PaneId;

/// The window facts the arithmetic reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Env {
    pub status: PaneStatus,
    /// Scrollbar width + pad when `pane-scrollbars` is `on` (always).
    pub scrollbar: Option<u32>,
}

/// Whether a cell touches the window's top and bottom edge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pos {
    pub top: bool,
    pub bottom: bool,
}

impl Pos {
    pub const ROOT: Self = Self {
        top: true,
        bottom: true,
    };

    /// `layout_add_horizontal_border`: the cell needs a line for the pane
    /// status.
    fn border(self, env: Env) -> bool {
        match env.status {
            PaneStatus::Top => self.top,
            PaneStatus::Bottom => self.bottom,
            PaneStatus::Off => false,
        }
    }
}

pub(crate) fn child_pos<L>(node: &Node<L>, idx: usize, pos: Pos) -> Pos {
    if node.dir == Dir::TopBottom {
        Pos {
            top: pos.top && first_tiled(&node.children) == Some(idx),
            bottom: pos.bottom && last_tiled(&node.children) == Some(idx),
        }
    } else {
        pos
    }
}

pub(crate) fn pos_at<L>(root: &Cell<L>, path: &[usize]) -> Pos {
    let mut pos = Pos::ROOT;
    let mut cur = root;
    for &i in path {
        if let Cell::Node(n) = cur {
            pos = child_pos(n, i, pos);
            cur = &n.children[i];
        }
    }
    pos
}

fn to_i32(v: u32) -> i32 {
    // C converts u_int to int implicitly.
    v as i32
}

/// `layout_fix_offsets1`.
fn fix_offsets1<L>(node: &mut Node<L>) {
    let (mut xoff, mut yoff) = (node.g.xoff, node.g.yoff);
    let (pxoff, pyoff) = (node.g.xoff, node.g.yoff);
    for c in &mut node.children {
        if !c.occupies() {
            continue;
        }
        let dir = node.dir;
        {
            let g = c.g_mut();
            if dir == Dir::LeftRight {
                g.xoff = xoff;
                g.yoff = pyoff;
            } else {
                g.xoff = pxoff;
                g.yoff = yoff;
            }
        }
        if let Cell::Node(n) = c {
            fix_offsets1(n);
        }
        let g = *c.g();
        if dir == Dir::LeftRight {
            xoff += to_i32(g.sx) + 1;
        } else {
            yoff += to_i32(g.sy) + 1;
        }
    }
}

/// `layout_fix_offsets`.
pub(crate) fn fix_offsets<L>(root: &mut Cell<L>) {
    if root.is_floating() {
        return;
    }
    let g = root.g_mut();
    g.xoff = 0;
    g.yoff = 0;
    if let Cell::Node(n) = root {
        fix_offsets1(n);
    }
}

/// `layout_resize_check`: how much can be taken from `c` along `dir`.
pub(crate) fn resize_check<L>(c: &Cell<L>, dir: Dir, env: Env, pos: Pos) -> u32 {
    if !c.occupies() {
        return 0;
    }
    match c {
        Cell::Pane(l) => {
            let (available, minimum) = if dir == Dir::LeftRight {
                (l.g.sx, PANE_MINIMUM + env.scrollbar.unwrap_or(0))
            } else {
                (
                    l.g.sy,
                    if pos.border(env) {
                        PANE_MINIMUM + 1
                    } else {
                        PANE_MINIMUM
                    },
                )
            };
            available.saturating_sub(minimum)
        }
        Cell::Floating(_) => 0,
        Cell::Node(n) => {
            let each = |i: usize, ch: &Cell<L>| resize_check(ch, dir, env, child_pos(n, i, pos));
            if n.dir == dir {
                n.children
                    .iter()
                    .enumerate()
                    .map(|(i, ch)| each(i, ch))
                    .fold(0u32, u32::wrapping_add)
            } else {
                n.children
                    .iter()
                    .enumerate()
                    .filter(|(_, ch)| ch.occupies())
                    .map(|(i, ch)| each(i, ch))
                    .min()
                    .unwrap_or(u32::MAX)
            }
        }
    }
}

/// `layout_resize_adjust`: change `c`'s size evenly, children included. The
/// change must already be bounded to the space available.
pub(crate) fn resize_adjust<L>(c: &mut Cell<L>, dir: Dir, mut change: i32, env: Env, pos: Pos) {
    c.g_mut().grow(dir, change);
    let Cell::Node(n) = c else { return };
    if n.dir != dir {
        for i in 0..n.children.len() {
            if n.children[i].occupies() {
                let p = child_pos(n, i, pos);
                resize_adjust(&mut n.children[i], dir, change, env, p);
            }
        }
        return;
    }
    if !n.children.iter().any(Cell::occupies) {
        return;
    }
    while change != 0 {
        let mut changed = false;
        for i in 0..n.children.len() {
            if change == 0 {
                break;
            }
            if !n.children[i].occupies() {
                continue;
            }
            let p = child_pos(n, i, pos);
            if change > 0 {
                resize_adjust(&mut n.children[i], dir, 1, env, p);
                change -= 1;
                changed = true;
                continue;
            }
            if resize_check(&n.children[i], dir, env, p) > 0 {
                resize_adjust(&mut n.children[i], dir, -1, env, p);
                change += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

pub(crate) fn adjust_at<L>(root: &mut Cell<L>, path: &[usize], dir: Dir, change: i32, env: Env) {
    let pos = pos_at(root, path);
    resize_adjust(root.get_mut(path), dir, change, env, pos);
}

/// `layout_remove_tile`: give the cell's space to its nearest neighbour.
/// `path` must be a tiled-or-containing cell; floating cells do nothing.
fn remove_tile<L>(root: &mut Cell<L>, path: &[usize], env: Env) {
    if root.get(path).is_floating() {
        return;
    }
    if let Some((&idx, parent_path)) = path.split_last() {
        let Cell::Node(parent) = root.get(parent_path) else {
            return;
        };
        let dir = parent.dir;
        match neighbour(&parent.children, idx) {
            None => remove_tile(root, parent_path, env),
            Some(n) => {
                let change = to_i32(parent.children[idx].g().size(dir)) + 1;
                let mut np = parent_path.to_vec();
                np.push(n);
                adjust_at(root, &np, dir, change, env);
            }
        }
        *root.get_mut(path).g_mut() = Geometry::default();
    }
}

/// `layout_destroy_cell`: remove the cell at `path` and give its space away.
/// Returns false when it was the root, the last cell of the window.
pub(crate) fn destroy_cell<L>(root: &mut Cell<L>, path: &[usize], env: Env) -> bool {
    let Some((&idx, parent_path)) = path.split_last() else {
        return false;
    };
    let Cell::Node(parent) = root.get(parent_path) else {
        return false;
    };
    let dir = parent.dir;
    if parent.children[idx].is_tiled() {
        match neighbour(&parent.children, idx) {
            Some(other) => {
                let change = to_i32(parent.children[idx].g().size(dir)) + 1;
                let mut op = parent_path.to_vec();
                op.push(other);
                adjust_at(root, &op, dir, change, env);
            }
            None => remove_tile(root, parent_path, env),
        }
    }
    let Cell::Node(parent) = root.get_mut(parent_path) else {
        return false;
    };
    parent.children.remove(idx);
    if parent.children.len() == 1 {
        let mut only = parent.children.remove(0);
        if parent_path.is_empty() {
            if only.is_tiled() {
                let g = only.g_mut();
                g.xoff = 0;
                g.yoff = 0;
            }
            *root = only;
        } else {
            let (&pi, gp_path) = parent_path.split_last().expect("non-empty");
            let Cell::Node(gp) = root.get_mut(gp_path) else {
                return true;
            };
            gp.children[pi] = only;
        }
    }
    true
}

/// `layout_new_pane_size`.
fn new_pane_size<L>(
    previous: u32,
    c: &Cell<L>,
    dir: Dir,
    size: u32,
    count_left: u32,
    size_left: u32,
    env: Env,
    pos: Pos,
) -> u32 {
    if count_left == 1 {
        return size_left;
    }
    let available = resize_check(c, dir, env, pos);
    let mut min = (PANE_MINIMUM + 1).wrapping_mul(count_left - 1);
    let cur = c.g().size(dir);
    if cur.wrapping_sub(available) > min {
        min = cur.wrapping_sub(available);
    }
    let mut new_size = cur.wrapping_mul(size).checked_div(previous).unwrap_or(0);
    let max = size_left.wrapping_sub(min);
    if new_size > max {
        new_size = max;
    }
    if new_size < PANE_MINIMUM {
        new_size = PANE_MINIMUM;
    }
    new_size
}

/// `layout_set_size_check`: can `c` be resized to `size` along `dir`?
pub(crate) fn set_size_check<L>(c: &Cell<L>, dir: Dir, size: u32, env: Env, pos: Pos) -> bool {
    let Cell::Node(n) = c else {
        return size >= PANE_MINIMUM;
    };
    let mut available = size;
    let count = u32::try_from(n.children.len()).unwrap_or(u32::MAX);
    if n.dir == dir {
        if available < (count * 2).wrapping_sub(1) {
            return false;
        }
        let previous = n.g.size(dir);
        for (idx, ch) in n.children.iter().enumerate() {
            let idx32 = u32::try_from(idx).unwrap_or(u32::MAX);
            let p = child_pos(n, idx, pos);
            let new_size = new_pane_size(previous, ch, dir, size, count - idx32, available, env, p);
            if idx32 == count - 1 {
                if new_size > available {
                    return false;
                }
                available -= new_size;
            } else {
                if new_size + 1 > available {
                    return false;
                }
                available -= new_size + 1;
            }
            if !set_size_check(ch, dir, new_size, env, p) {
                return false;
            }
        }
    } else {
        for (idx, ch) in n.children.iter().enumerate() {
            if !matches!(ch, Cell::Node(_)) {
                continue;
            }
            if !set_size_check(ch, dir, size, env, child_pos(n, idx, pos)) {
                return false;
            }
        }
    }
    true
}

/// `layout_resize_child_cells`: fit all child cells into the current cell.
pub(crate) fn resize_child_cells<L>(c: &mut Cell<L>, env: Env, pos: Pos) {
    let Cell::Node(n) = c else { return };
    let mut count = 0u32;
    let mut prev = 0u32;
    for ch in &n.children {
        if !ch.occupies() {
            continue;
        }
        count += 1;
        prev = prev.wrapping_add(ch.g().size(n.dir));
    }
    prev = prev.wrapping_add(count.wrapping_sub(1));
    let mut available = n.g.size(n.dir);
    let mut idx = 0u32;
    for i in 0..n.children.len() {
        if !n.children[i].occupies() {
            continue;
        }
        let p = child_pos(n, i, pos);
        let (dir, g) = (n.dir, n.g);
        let left = count - idx;
        if dir == Dir::TopBottom {
            let ch = n.children[i].g_mut();
            ch.sx = g.sx;
            ch.xoff = g.xoff;
        } else {
            let s = new_pane_size(prev, &n.children[i], dir, g.sx, left, available, env, p);
            n.children[i].g_mut().sx = s;
            available = available.wrapping_sub(s.wrapping_add(1));
        }
        if dir == Dir::LeftRight {
            let ch = n.children[i].g_mut();
            ch.sy = g.sy;
            ch.yoff = g.yoff;
        } else {
            let s = new_pane_size(prev, &n.children[i], dir, g.sy, left, available, env, p);
            n.children[i].g_mut().sy = s;
            available = available.wrapping_sub(s.wrapping_add(1));
        }
        resize_child_cells(&mut n.children[i], env, p);
        idx += 1;
    }
}

/// `layout_split_check_space`.
fn split_check_space(g: &Geometry, dir: Dir, env: Env, pos: Pos) -> bool {
    match dir {
        Dir::LeftRight => {
            let minimum = match env.scrollbar {
                Some(w) => PANE_MINIMUM * 2 + w,
                None => PANE_MINIMUM * 2 + 1,
            };
            g.sx >= minimum
        }
        Dir::TopBottom => {
            let minimum = if pos.border(env) {
                PANE_MINIMUM * 2 + 2
            } else {
                PANE_MINIMUM * 2 + 1
            };
            g.sy >= minimum
        }
    }
}

/// `layout_split_sizes`: (size1, size2, saved size).
#[must_use]
pub fn split_sizes(g: &Geometry, size: i32, before: bool, dir: Dir) -> (u32, u32, u32) {
    let ss = g.size(dir);
    let mut s2 = if size < 0 {
        (ss.wrapping_add(1) / 2).wrapping_sub(1)
    } else if before {
        ss.wrapping_sub(size as u32).wrapping_sub(1)
    } else {
        size as u32
    };
    if s2 < PANE_MINIMUM {
        s2 = PANE_MINIMUM;
    } else if s2 > ss.wrapping_sub(2) {
        s2 = ss.wrapping_sub(2);
    }
    (ss.wrapping_sub(1).wrapping_sub(s2), s2, ss)
}

/// `layout_spread_cell` for the node at `path`. True when anything changed.
pub(crate) fn spread_cell<L>(root: &mut Cell<L>, path: &[usize], env: Env) -> bool {
    let ppos = pos_at(root, path);
    let Cell::Node(parent) = root.get_mut(path) else {
        return false;
    };
    let number =
        u32::try_from(parent.children.iter().filter(|c| c.is_tiled()).count()).unwrap_or(u32::MAX);
    if number <= 1 {
        return false;
    }
    let size = match parent.dir {
        Dir::LeftRight => parent.g.sx,
        Dir::TopBottom => {
            if ppos.border(env) {
                parent.g.sy.wrapping_sub(1)
            } else {
                parent.g.sy
            }
        }
    };
    if size < number - 1 {
        return false;
    }
    let each = (size - (number - 1)) / number;
    if each == 0 {
        return false;
    }
    let mut remainder = size.wrapping_sub(number * (each + 1)).wrapping_add(1);
    let mut changed = false;
    for i in 0..parent.children.len() {
        if !parent.children[i].is_tiled() {
            continue;
        }
        let p = child_pos(parent, i, ppos);
        let ch = &mut parent.children[i];
        let change;
        if parent.dir == Dir::LeftRight {
            let mut c = to_i32(each).wrapping_sub(to_i32(ch.g().sx));
            if remainder > 0 {
                c += 1;
                remainder -= 1;
            }
            change = c;
            resize_adjust(ch, Dir::LeftRight, change, env, p);
        } else {
            let mut this = if p.border(env) { each + 1 } else { each };
            if remainder > 0 {
                this += 1;
                remainder -= 1;
            }
            change = to_i32(this).wrapping_sub(to_i32(ch.g().sy));
            resize_adjust(ch, Dir::TopBottom, change, env, p);
        }
        if change != 0 {
            changed = true;
        }
    }
    changed
}

/// Why a split or a new pane failed (the strings tmux prints).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitError {
    /// `can't split a floating pane` (`layout_get_tiled_cell`).
    Floating,
    /// `no space for a new pane`.
    NoSpace,
    /// The target pane is not in the layout.
    NoPane,
}

impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Floating => "can't split a floating pane",
            Self::NoSpace => "no space for a new pane",
            Self::NoPane => "pane not in the layout",
        })
    }
}

impl std::error::Error for SplitError {}

/// A `split-window` for the layout: `-h`, `-b`, `-f` and the resolved size.
#[derive(Clone, Copy, Debug, Default)]
pub struct SplitRequest {
    pub horizontal: bool,
    pub before: bool,
    pub full: bool,
    /// The `-l`/`-p` size in cells; `None` splits in half.
    pub size: Option<i64>,
}

/// An `-x`/`-y` style value that may be a percentage, with tmux's causes
/// (`args_string_percentage`, arguments.c:997-1040).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PercentError {
    Empty,
    Invalid,
    TooSmall,
    TooLarge,
}

impl fmt::Display for PercentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty",
            Self::Invalid => "invalid",
            Self::TooSmall => "too small",
            Self::TooLarge => "too large",
        })
    }
}

impl From<NumError> for PercentError {
    fn from(e: NumError) -> Self {
        match e {
            NumError::TooSmall => Self::TooSmall,
            NumError::TooLarge => Self::TooLarge,
            NumError::Missing | NumError::Invalid => Self::Invalid,
        }
    }
}

/// `args_string_percentage`: `value` as a number in `min..=max`, or a
/// percentage of `cur`.
pub fn percentage(value: &str, min: i64, max: i64, cur: i64) -> Result<i64, PercentError> {
    if value.is_empty() {
        return Err(PercentError::Empty);
    }
    if let Some(p) = value.strip_suffix('%') {
        let ll = crate::cmd::strtonum(p, 0, 1000)?;
        let ll = cur * ll / 100;
        if ll < min {
            return Err(PercentError::TooSmall);
        }
        if ll > max {
            return Err(PercentError::TooLarge);
        }
        Ok(ll)
    } else {
        Ok(crate::cmd::strtonum(value, min, max)?)
    }
}

/// `-x`/`-y`/`-X`/`-Y` of a floating pane (`layout_floating_args_parse`).
#[derive(Clone, Debug, Default)]
pub struct FloatArgs {
    pub width: Option<String>,
    pub height: Option<String>,
    pub x: Option<String>,
    pub y: Option<String>,
}

/// What `resize-pane` was asked: `-x`, `-y`, then the `-U -D -L -R` counts
/// (already converted: `adjustment` errors belong to the caller).
#[derive(Clone, Debug, Default)]
pub struct ResizeRequest {
    pub x: Option<String>,
    pub y: Option<String>,
    /// `U`, `D`, `L`, `R` in tmux's order of application.
    pub adjust: Vec<(char, i32)>,
}

impl Layout {
    pub(crate) fn env(&self) -> Env {
        Env {
            status: self.opts.status,
            scrollbar: self.opts.scrollbar_always,
        }
    }

    /// `layout_fix_offsets`.
    pub fn fix_offsets(&mut self) {
        fix_offsets(&mut self.root);
    }

    /// The pane's own rectangle (`wp->xoff`, `wp->yoff`, `wp->sx`, `wp->sy`
    /// as `layout_fix_panes` sets them, scrollbars aside).
    #[must_use]
    pub fn pane_rect(&self, pane: PaneId) -> Option<Geometry> {
        Some(self.cell_rect(&self.root.find_pane(pane)?))
    }

    /// [`Self::pane_rect`] for the pane whose cell is at `path`: what a
    /// pane given that cell (`wp->layout_cell`) is resized to.
    #[must_use]
    pub fn cell_rect(&self, path: &[usize]) -> Geometry {
        let cell = self.root.get(path);
        let mut g = *cell.g();
        if !cell.is_floating() && pos_at(&self.root, path).border(self.env()) {
            if self.opts.status == PaneStatus::Top {
                g.yoff += 1;
            }
            if g.sy > 1 {
                g.sy -= 1;
            }
        }
        g
    }

    /// `layout_add_horizontal_border(root, lc, status)` for a tiled
    /// pane's cell: it needs a row for the pane status line.
    #[must_use]
    pub fn pane_needs_border(&self, pane: PaneId) -> bool {
        self.root
            .find_pane(pane)
            .is_some_and(|path| self.cell_needs_border(&path))
    }

    /// [`Self::pane_needs_border`] for the cell at `path`.
    #[must_use]
    pub fn cell_needs_border(&self, path: &[usize]) -> bool {
        !self.root.get(path).is_floating() && pos_at(&self.root, path).border(self.env())
    }

    /// `layout_split_pane` for a new pane `new` next to `target`
    /// (`-f`: next to the whole layout). On success the new pane has its
    /// cell; the caller adds it to the window's pane list.
    pub fn split(
        &mut self,
        target: PaneId,
        new: PaneId,
        req: &SplitRequest,
    ) -> Result<(), SplitError> {
        let env = self.env();
        let ty = if req.horizontal {
            Dir::LeftRight
        } else {
            Dir::TopBottom
        };
        let target_path = self.root.find_pane(target).ok_or(SplitError::NoPane)?;
        if self.root.get(&target_path).is_floating() {
            return Err(SplitError::Floating);
        }
        let lc_path = if req.full { Vec::new() } else { target_path };
        let lc = self.root.get(&lc_path);
        if lc.is_floating() {
            return Err(SplitError::Floating);
        }
        let g = *lc.g();
        let lc_is_node_of = matches!(lc, Cell::Node(n) if n.dir == ty);
        let pos = pos_at(&self.root, &lc_path);
        if !split_check_space(&g, ty, env, pos) {
            return Err(SplitError::NoSpace);
        }
        let hint = req.size.map_or(-1, |s| s as i32);
        let (size1, size2, saved) = split_sizes(&g, hint, req.before, ty);
        let new_size = if req.before { size2 } else { size1 };
        if req.full && !set_size_check(&self.root, ty, new_size, env, Pos::ROOT) {
            return Err(SplitError::NoSpace);
        }
        let leaf = |g: Geometry| Cell::Pane(Leaf { pane: new, g });
        let parent_dir = lc_path
            .split_last()
            .and_then(|(_, pp)| match self.root.get(pp) {
                Cell::Node(n) => Some(n.dir),
                _ => None,
            });
        // Where the old cell ends up, for the size step.
        let mut resize_first = false;
        let (lc1_path, lc2_path);
        if parent_dir == Some(ty) {
            let (&idx, pp) = lc_path.split_last().expect("has parent");
            let Cell::Node(p) = self.root.get_mut(pp) else {
                unreachable!()
            };
            let at = if req.before { idx } else { idx + 1 };
            p.children.insert(at, leaf(Geometry::default()));
            let mut a = pp.to_vec();
            let mut b = pp.to_vec();
            a.push(idx);
            b.push(idx + 1);
            (lc1_path, lc2_path) = (a, b);
        } else if req.full && lc_path.is_empty() && lc_is_node_of {
            // The new full-size pane joins the root node: squeeze the old
            // children first.
            let root = &mut self.root;
            root.g_mut().grow_to(ty, new_size);
            resize_child_cells(root, env, Pos::ROOT);
            root.g_mut().grow_to(ty, saved);
            resize_first = true;
            let size = saved.wrapping_sub(1).wrapping_sub(new_size);
            let ng = match ty {
                Dir::LeftRight => Geometry::new(size, g.sy, 0, 0),
                Dir::TopBottom => Geometry::new(g.sx, size, 0, 0),
            };
            let Cell::Node(n) = root else { unreachable!() };
            if req.before {
                n.children.insert(0, leaf(ng));
            } else {
                n.children.push(leaf(ng));
            }
            (lc1_path, lc2_path) = (Vec::new(), Vec::new());
        } else {
            // Replace the cell by a new node holding it and the new cell.
            let old = self.root.get(&lc_path).clone();
            let mut children = vec![old];
            if req.before {
                children.insert(0, leaf(Geometry::default()));
            } else {
                children.push(leaf(Geometry::default()));
            }
            let node = Cell::Node(Node {
                dir: ty,
                g,
                children,
            });
            *self.root.get_mut(&lc_path) = node;
            let mut a = lc_path.clone();
            let mut b = lc_path.clone();
            a.push(0);
            b.push(1);
            (lc1_path, lc2_path) = (a, b);
        }
        // `lc1` is the top/left cell, `lc2` the bottom/right one.
        let (p1, p2) = (lc1_path.clone(), lc2_path.clone());
        if !resize_first {
            match ty {
                Dir::LeftRight => {
                    *self.root.get_mut(&p1).g_mut() = Geometry::new(size1, g.sy, g.xoff, g.yoff);
                    let x2 = g.xoff + to_i32(size1) + 1;
                    *self.root.get_mut(&p2).g_mut() = Geometry::new(size2, g.sy, x2, g.yoff);
                }
                Dir::TopBottom => {
                    *self.root.get_mut(&p1).g_mut() = Geometry::new(g.sx, size1, g.xoff, g.yoff);
                    let y2 = g.yoff + to_i32(size1) + 1;
                    *self.root.get_mut(&p2).g_mut() = Geometry::new(g.sx, size2, g.xoff, y2);
                }
            }
        }
        if req.full {
            if !resize_first {
                // The old root is the child that is not the new pane.
                let old_path = if req.before { p2 } else { p1 };
                let pos = pos_at(&self.root, &old_path);
                resize_child_cells(self.root.get_mut(&old_path), env, pos);
            }
            fix_offsets(&mut self.root);
        }
        Ok(())
    }

    /// `layout_close_pane` (with `layout_destroy_cell`): false when `pane`
    /// is the only cell, which kills the window instead.
    pub fn close_pane(&mut self, pane: PaneId) -> bool {
        let Some(path) = self.root.find_pane(pane) else {
            return true;
        };
        let env = self.env();
        if !destroy_cell(&mut self.root, &path, env) {
            return false;
        }
        fix_offsets(&mut self.root);
        true
    }

    /// `layout_resize_pane_to`: set the size of the pane's cell (or its
    /// nearest ancestor that splits along `dir`) to `new_size`. Whether it
    /// got as far as `layout_resize_layout`, which tmux ends with a
    /// window-layout-changed event (layout.c:1002).
    pub fn resize_pane_to(&mut self, pane: PaneId, dir: Dir, new_size: u32) -> bool {
        let Some(path) = self.root.find_pane(pane) else {
            return false;
        };
        let Some(k) = self.splitting_ancestor(&path, dir) else {
            return false;
        };
        let lc_path = &path[..=k];
        let size = to_i32(self.root.get(lc_path).g().size(dir));
        let Cell::Node(parent) = self.root.get(&path[..k]) else {
            return false;
        };
        let change = if last_tiled(&parent.children) == Some(path[k]) {
            size.wrapping_sub(to_i32(new_size))
        } else {
            to_i32(new_size).wrapping_sub(size)
        };
        self.resize_pane(pane, dir, change, true)
    }

    /// The depth `k` such that the node at `path[..k]` splits along `dir`
    /// and is the deepest such ancestor.
    fn splitting_ancestor(&self, path: &[usize], dir: Dir) -> Option<usize> {
        (0..path.len())
            .rev()
            .find(|&k| matches!(self.root.get(&path[..k]), Cell::Node(n) if n.dir == dir))
    }

    /// `layout_resize_pane`: grow (positive) or shrink the pane. Whether it
    /// reached `layout_resize_layout`, as [`Self::resize_pane_to`].
    pub fn resize_pane(&mut self, pane: PaneId, dir: Dir, change: i32, opposite: bool) -> bool {
        let Some(path) = self.root.find_pane(pane) else {
            return false;
        };
        let Some(k) = self.splitting_ancestor(&path, dir) else {
            return false;
        };
        let Cell::Node(parent) = self.root.get(&path[..k]) else {
            return false;
        };
        let mut idx = path[k];
        if last_tiled(&parent.children) == Some(idx) {
            match neighbour_dir(&parent.children, idx, false) {
                Some(i) => idx = i,
                None => return false,
            }
        }
        self.resize_layout(&path[..k], idx, dir, change, opposite);
        true
    }

    /// `layout_resize_layout` for child `idx` of the node at `parent_path`.
    fn resize_layout(
        &mut self,
        parent_path: &[usize],
        idx: usize,
        dir: Dir,
        change: i32,
        opposite: bool,
    ) {
        let env = self.env();
        let ppos = pos_at(&self.root, parent_path);
        let Cell::Node(parent) = self.root.get_mut(parent_path) else {
            return;
        };
        let mut needed = change;
        while needed != 0 {
            let size = if change > 0 {
                let s = grow_step(parent, idx, dir, needed, opposite, env, ppos);
                needed -= to_i32(s);
                s
            } else {
                let s = shrink_step(parent, idx, dir, needed, env, ppos);
                needed += to_i32(s);
                s
            };
            if size == 0 {
                break;
            }
        }
        fix_offsets(&mut self.root);
    }

    /// `layout_search_by_border`: the path of the cell whose right or bottom
    /// border is at window position `(x, y)` (the cell before the gap `(x,
    /// y)` is in), searching into the cell that contains it.
    #[must_use]
    pub fn search_by_border(&self, x: i64, y: i64) -> Option<super::Path> {
        let mut path = Vec::new();
        let mut cell = &self.root;
        'down: loop {
            let Cell::Node(n) = cell else {
                return None;
            };
            let mut last: Option<(usize, &Geometry)> = None;
            for (i, child) in n.children.iter().enumerate() {
                let g = child.g();
                let (xoff, yoff) = (i64::from(g.xoff), i64::from(g.yoff));
                if x >= xoff
                    && x < xoff + i64::from(g.sx)
                    && y >= yoff
                    && y < yoff + i64::from(g.sy)
                {
                    path.push(i);
                    cell = child;
                    continue 'down;
                }
                if let Some((li, lg)) = last {
                    let gap = match n.dir {
                        Dir::LeftRight => x < xoff && x >= i64::from(lg.xoff) + i64::from(lg.sx),
                        Dir::TopBottom => y < yoff && y >= i64::from(lg.yoff) + i64::from(lg.sy),
                    };
                    if gap {
                        path.push(li);
                        return Some(path);
                    }
                }
                last = Some((i, g));
            }
            return None;
        }
    }

    /// `cmd_resize_pane_mouse_resize_tiled` (cmd-resize-pane.c:357-420) on
    /// the layout: a drag from window position `from` to `to` moves the
    /// borders found at `from` and around it (`layout_search_by_border`)
    /// with `layout_resize_layout`. Returns how many cells it resized, each
    /// a window-layout-changed event in tmux (layout.c:1002).
    pub fn drag_border(&mut self, from: (i64, i64), to: (i64, i64)) -> u32 {
        const OFFSETS: [(i64, i64); 5] = [(0, 0), (0, 1), (1, 0), (0, -1), (-1, 0)];
        let ((lx, ly), (x, y)) = (from, to);
        let mut cells: Vec<super::Path> = Vec::new();
        for (dx, dy) in OFFSETS {
            if let Some(p) = self.search_by_border(lx + dx, ly + dy)
                && !cells.contains(&p)
            {
                cells.push(p);
            }
        }
        let mut resizes = 0;
        for path in cells {
            let Some((&idx, parent)) = path.split_last() else {
                continue;
            };
            let Cell::Node(n) = self.root.get(parent) else {
                continue;
            };
            let change = match n.dir {
                Dir::TopBottom if y != ly => y - ly,
                Dir::LeftRight if x != lx => x - lx,
                _ => continue,
            };
            let change = i32::try_from(change).unwrap_or(0);
            let dir = n.dir;
            self.resize_layout(parent, idx, dir, change, false);
            resizes += 1;
        }
        resizes
    }

    /// `layout_spread_out` (`select-layout -E`): spread the cells around
    /// `pane`, walking up until something changes.
    pub fn spread_out(&mut self, pane: PaneId) {
        let Some(mut path) = self.root.find_pane(pane) else {
            return;
        };
        if path.is_empty() {
            return;
        }
        let env = self.env();
        path.pop();
        loop {
            if spread_cell(&mut self.root, &path, env) {
                fix_offsets(&mut self.root);
                break;
            }
            if path.pop().is_none() {
                break;
            }
        }
    }

    /// `layout_resize` + `resize_window`: the window becomes `sx` by `sy`
    /// (clamped to tmux's limits) and the layout follows.
    pub fn resize_window(&mut self, sx: u32, sy: u32) {
        let sx = sx.clamp(PANE_MINIMUM, PANE_MAXIMUM);
        let sy = sy.clamp(PANE_MINIMUM, PANE_MAXIMUM);
        self.layout_resize(sx, sy);
        self.sx = sx.max(self.root.g().sx);
        self.sy = sy.max(self.root.g().sy);
    }

    fn layout_resize(&mut self, sx: u32, sy: u32) {
        let env = self.env();
        if self.root.is_floating() {
            self.clamp_floating(sx, sy);
            return;
        }
        for dir in [Dir::LeftRight, Dir::TopBottom] {
            let cur = self.root.g().size(dir);
            let want = if dir == Dir::LeftRight { sx } else { sy };
            let mut change = to_i32(want).wrapping_sub(to_i32(cur));
            let limit = to_i32(resize_check(&self.root, dir, env, Pos::ROOT));
            if change < 0 && change < -limit {
                change = -limit;
            }
            if limit == 0 {
                change = if want <= cur {
                    0
                } else {
                    to_i32(want.wrapping_sub(cur))
                };
            }
            if change != 0 {
                resize_adjust(&mut self.root, dir, change, env, Pos::ROOT);
            }
        }
        fix_offsets(&mut self.root);
        self.clamp_floating(sx, sy);
    }

    /// `layout_clamp_floating_panes`.
    fn clamp_floating(&mut self, sx: u32, sy: u32) {
        clamp_walk(&mut self.root, sx, sy, &self.borderless);
    }

    /// `layout_floating_args_parse`: the geometry of a new floating pane.
    /// `Err` is the full message (`position too large`, `invalid width`...).
    /// `border` is whether the pane will have one (`lines` is not `none`);
    /// `start` is the cell's saved floating geometry (`lc->fg`), which a
    /// pane floated before keeps, unset parts as tmux's sentinels.
    pub fn floating_geometry(
        &mut self,
        args: &FloatArgs,
        border: bool,
        start: Option<Geometry>,
    ) -> Result<Geometry, String> {
        let (wsx, wsy) = (i64::from(self.sx), i64::from(self.sy));
        let pad = i32::from(border);
        let mut sx = i64::from(start.map_or(self.sx / 2, |g| g.sx));
        let mut sy = i64::from(start.map_or(self.sy / 4, |g| g.sy));
        let max = i64::from(PANE_MAXIMUM);
        let pos_err = |e: PercentError| format!("position {e}");
        if let Some(v) = &args.width {
            sx = percentage(v, 0, max, wsx).map_err(pos_err)?;
            if border {
                sx -= 2;
            }
        }
        if let Some(v) = &args.height {
            sy = percentage(v, 0, max, wsy).map_err(pos_err)?;
            if border {
                sy -= 2;
            }
        }
        let mut ox = start.map_or(i32::MAX, |g| g.xoff);
        let mut oy = start.map_or(i32::MAX, |g| g.yoff);
        if let Some(v) = &args.x {
            ox = percentage(v, -sx, wsx, wsx).map_err(pos_err)? as i32;
        }
        if let Some(v) = &args.y {
            oy = percentage(v, -sy, wsy, wsy).map_err(pos_err)? as i32;
        }
        if !self.root.leaves().iter().any(|(_, _, f)| *f) {
            self.last_new = (0, 0);
        }
        let sx32 = sx as i32;
        let sy32 = sy as i32;
        if ox == i32::MAX {
            ox = if self.last_new.0 == 0 {
                4
            } else {
                let o = self.last_new.0 + 4;
                if o + sx32 + pad > to_i32(self.sx) {
                    4
                } else {
                    o
                }
            };
            self.last_new.0 = ox;
        } else if args.x.is_some() && border {
            ox += 1;
        }
        if oy == i32::MAX {
            oy = if self.last_new.1 == 0 {
                2
            } else {
                let o = self.last_new.1 + 2;
                if o + sy32 + pad > to_i32(self.sy) {
                    2
                } else {
                    o
                }
            };
            self.last_new.1 = oy;
        } else if args.y.is_some() && border {
            oy += 1;
        }
        if !(i64::from(PANE_MINIMUM)..=max).contains(&sx) {
            return Err("invalid width".to_owned());
        }
        if !(i64::from(PANE_MINIMUM)..=max).contains(&sy) {
            return Err("invalid height".to_owned());
        }
        Ok(Geometry::new(sx as u32, sy as u32, ox, oy))
    }

    /// `layout_split_floating_cell` (layout.c:1593-1740): split the floating
    /// pane `target`. The target keeps its (shrunk) cell; the returned
    /// geometry is the new pane's, for [`Self::add_floating`]. `Err` is
    /// `no space for a new pane`.
    pub fn split_floating(
        &mut self,
        target: PaneId,
        req: &SplitRequest,
        border: bool,
    ) -> Result<Geometry, SplitError> {
        let SplitRequest {
            horizontal,
            before,
            full,
            ..
        } = *req;
        let path = self.root.find_pane(target).ok_or(SplitError::NoPane)?;
        let lc = self.root.get_mut(&path);
        let wsx = to_i32(self.sx);
        let wsy = to_i32(self.sy);
        let (tborder, bborder, lborder, rborder) = (1, wsy - 1, 3, wsx - 3);
        let border = i32::from(border);
        let mut old = *lc.g();
        let (osx, osy) = (to_i32(old.sx), to_i32(old.sy));
        // First, move the target cell in-bounds.
        if lborder > old.xoff - border {
            old.xoff = lborder + border;
        }
        if rborder < old.xoff + osx + border {
            old.xoff = rborder - osx - border;
        }
        if tborder > old.yoff - border {
            old.yoff = tborder + border;
        }
        if bborder < old.yoff + osy + border {
            old.yoff = bborder - osy - border;
        }
        // Move the new cell to its ideal position.
        let mut new = old;
        if horizontal {
            let d = to_i32(old.sx) + 2 * border;
            new.xoff += if before { -d } else { d };
        } else {
            let d = to_i32(old.sy) + 2 * border;
            new.yoff += if before { -d } else { d };
        }
        // Out of bounds: split the available space between both cells.
        if lborder > new.xoff - border {
            let space = old.xoff + to_i32(old.sx) - lborder - 3 * border + 1;
            let size = (space / 2) as u32;
            new.sx = size;
            old.sx = size;
            new.xoff = lborder + border;
            old.xoff = new.xoff + to_i32(new.sx) + 2 * border;
            if space % 2 == 0 {
                old.sx = old.sx.wrapping_sub(1);
            }
        } else if rborder < new.xoff + to_i32(new.sx) + border {
            let space = rborder - old.xoff - 3 * border + 1;
            let size = (space / 2) as u32;
            new.sx = size;
            old.sx = size;
            new.xoff = old.xoff + to_i32(old.sx) + 2 * border;
            if space % 2 == 0 {
                new.sx = new.sx.wrapping_sub(1);
            }
        } else if tborder > new.yoff - border {
            let space = to_i32(old.sy) + old.yoff - tborder - 3 * border + 1;
            let size = (space / 2) as u32;
            new.sy = size;
            old.sy = size;
            new.yoff = tborder + border;
            old.yoff = new.yoff + to_i32(new.sy) + 2 * border;
            if space % 2 == 0 {
                old.sy = old.sy.wrapping_sub(1);
            }
        } else if bborder < new.yoff + to_i32(new.sy) + border {
            let space = bborder - old.yoff - 3 * border + 1;
            let size = (space / 2) as u32;
            new.sy = size;
            old.sy = size;
            new.yoff = old.yoff + to_i32(old.sy) + 2 * border;
            if space % 2 == 0 {
                new.sy = new.sy.wrapping_sub(1);
            }
        }
        // Expand the new cell over the whole space where it was spawned.
        if full {
            if horizontal {
                new.yoff = tborder + border;
                new.sy = (bborder - tborder - 2 * border) as u32;
                if before {
                    new.xoff = lborder + border;
                    new.sx = (old.xoff - new.xoff - 2 * border) as u32;
                } else {
                    new.sx = (rborder - new.xoff - border) as u32;
                }
            } else {
                new.xoff = lborder + border;
                new.sx = (rborder - lborder - 2 * border) as u32;
                if before {
                    new.yoff = tborder + border;
                    new.sy = (old.yoff - new.yoff - 2 * border) as u32;
                } else {
                    new.sy = (bborder - new.yoff - border) as u32;
                }
            }
        }
        // The C compares u_int values, so a negative size wrapped to huge.
        if new.sx < PANE_MINIMUM
            || new.sy < PANE_MINIMUM
            || old.sx < PANE_MINIMUM
            || old.sy < PANE_MINIMUM
        {
            return Err(SplitError::NoSpace);
        }
        *lc.g_mut() = old;
        Ok(new)
    }

    /// `layout_floating_pane`: a floating cell for `new`, placed after
    /// `after`'s cell (the root when `None`).
    pub fn add_floating(&mut self, after: Option<PaneId>, new: PaneId, g: Geometry) {
        let path = after
            .and_then(|p| self.root.find_pane(p))
            .unwrap_or_default();
        let leaf = Cell::Floating(Leaf { pane: new, g });
        if path.is_empty() {
            let old = std::mem::replace(
                &mut self.root,
                Cell::Pane(Leaf {
                    pane: new,
                    g: Geometry::default(),
                }),
            );
            let og = *old.g();
            self.root = Cell::Node(Node {
                dir: Dir::TopBottom,
                g: og,
                children: vec![old, leaf],
            });
            return;
        }
        let (&idx, pp) = path.split_last().expect("non-empty");
        if let Cell::Node(p) = self.root.get_mut(pp) {
            p.children.insert(idx + 1, leaf);
        }
    }

    /// `resize-pane -x/-y/-U/-D/-L/-R` for `pane` (cmd-resize-pane.c:110-188).
    /// `Err` is the message `cmdq_error` prints. `resized` counts the
    /// `layout_resize_layout` runs, each a window-layout-changed event in
    /// tmux (layout.c:1002), including those before an error.
    /// `border`: the pane has one (`window_pane_get_pane_lines` is not
    /// `none`), which a floating pane's absolute size includes.
    pub fn resize_pane_cmd(
        &mut self,
        pane: PaneId,
        req: &ResizeRequest,
        border: bool,
        resized: &mut u32,
    ) -> Result<(), String> {
        let path = self.root.find_pane(pane).ok_or("pane not in the layout")?;
        let floating = self.root.get(&path).is_floating();
        let (wsx, wsy) = (i64::from(self.sx), i64::from(self.sy));
        let max = i64::from(PANE_MAXIMUM);
        if let Some(v) = &req.x {
            let x = percentage(v, 0, max, wsx).map_err(|e| format!("width {e}"))?;
            if floating {
                self.resize_floating_to(pane, Dir::LeftRight, x as u32, border)
                    .map_err(|e| format!("size {e}"))?;
            } else {
                *resized += u32::from(self.resize_pane_to(pane, Dir::LeftRight, x as u32));
            }
        }
        if let Some(v) = &req.y {
            let mut y = percentage(v, 0, max, wsy).map_err(|e| format!("height {e}"))?;
            if let Some(r) = self.pane_rect(pane) {
                let sy = i64::from(self.sy);
                match self.opts.status {
                    PaneStatus::Top if y != i64::from(i32::MAX) && r.yoff == 1 => y += 1,
                    PaneStatus::Bottom
                        if y != i64::from(i32::MAX)
                            && i64::from(r.yoff) + i64::from(r.sy) == sy - 1 =>
                    {
                        y += 1;
                    }
                    _ => {}
                }
            }
            if floating {
                self.resize_floating_to(pane, Dir::TopBottom, y as u32, border)
                    .map_err(|e| format!("size {e}"))?;
            } else {
                *resized += u32::from(self.resize_pane_to(pane, Dir::TopBottom, y as u32));
            }
        }
        // tmux never resets `opposite` between flags.
        let mut opposite = false;
        for &(flag, adjust) in &req.adjust {
            let dir = if matches!(flag, 'L' | 'R') {
                Dir::LeftRight
            } else {
                Dir::TopBottom
            };
            if floating {
                if matches!(flag, 'L' | 'U') {
                    opposite = true;
                }
                self.resize_floating(pane, dir, adjust, opposite)
                    .map_err(|e| format!("adjustment {e}"))?;
            } else {
                let adjust = if matches!(flag, 'L' | 'U') {
                    adjust.wrapping_neg()
                } else {
                    adjust
                };
                *resized += u32::from(self.resize_pane(pane, dir, adjust, true));
            }
        }
        if !path.is_empty() {
            fix_offsets(&mut self.root);
        }
        Ok(())
    }

    /// `layout_resize_floating_pane_to`.
    pub fn resize_floating_to(
        &mut self,
        pane: PaneId,
        dir: Dir,
        size: u32,
        border: bool,
    ) -> Result<(), String> {
        let path = self.root.find_pane(pane).ok_or("pane not in the layout")?;
        let c = self.root.get_mut(&path);
        if !c.is_floating() {
            return Err("pane is not floating".to_owned());
        }
        let mut size = size;
        if border && size >= PANE_MINIMUM + 2 {
            size -= 2;
        }
        if !(PANE_MINIMUM..=PANE_MAXIMUM).contains(&size) {
            return Err("size is too big or too small".to_owned());
        }
        c.g_mut().grow_to(dir, size);
        Ok(())
    }

    /// `layout_resize_floating_pane`.
    pub fn resize_floating(
        &mut self,
        pane: PaneId,
        dir: Dir,
        change: i32,
        opposite: bool,
    ) -> Result<(), String> {
        let path = self.root.find_pane(pane).ok_or("pane not in the layout")?;
        let c = self.root.get_mut(&path);
        if !c.is_floating() {
            return Err("pane is not floating".to_owned());
        }
        if change == 0 {
            return Ok(());
        }
        let g = c.g_mut();
        let size = to_i32(g.size(dir)).wrapping_add(change) as u32;
        if !(PANE_MINIMUM..=PANE_MAXIMUM).contains(&size) {
            return Err("change is too big or too small".to_owned());
        }
        g.grow_to(dir, size);
        if opposite {
            match dir {
                Dir::LeftRight => g.xoff -= change,
                Dir::TopBottom => g.yoff -= change,
            }
        }
        Ok(())
    }
}

impl Layout {
    /// The geometry of `pane`'s cell if it is floating.
    #[must_use]
    pub fn pane_cell_geometry(&self, pane: PaneId) -> Option<Geometry> {
        let path = self.root.find_pane(pane)?;
        match self.root.get(&path) {
            Cell::Floating(l) => Some(l.g),
            _ => None,
        }
    }

    /// The geometry of `pane`'s cell if it is floating (`lc->g` of a
    /// `LAYOUT_CELL_FLOATING` cell), for the commands that move it.
    pub fn floating_cell_mut(&mut self, pane: PaneId) -> Option<&mut Geometry> {
        let path = self.root.find_pane(pane)?;
        match self.root.get_mut(&path) {
            Cell::Floating(l) => Some(&mut l.g),
            _ => None,
        }
    }

    /// `break-pane -W` (cmd-break-pane.c `cmd_break_pane_float`): the tiled
    /// cell of `pane` gives its space away (`layout_remove_tile`) and
    /// floats at `g`. False when the pane is not tiled.
    pub fn float_pane(&mut self, pane: PaneId, g: Geometry) -> bool {
        let Some(path) = self.root.find_pane(pane) else {
            return false;
        };
        if !self.root.get(&path).is_tiled() {
            return false;
        }
        let env = self.env();
        remove_tile(&mut self.root, &path, env);
        let cell = self.root.get_mut(&path);
        *cell = Cell::Floating(Leaf { pane, g });
        self.saved_float.remove(&pane);
        fix_offsets(&mut self.root);
        true
    }

    /// Tiling a floating pane (cmd-join-pane.c `cmd_join_pane_tile`): its
    /// geometry is kept for a later float (`lc->fg`), then
    /// `layout_insert_tile` takes half its nearest neighbour's space.
    /// `Err` is tmux's message; the cell stays floating then.
    pub fn tile_pane(&mut self, pane: PaneId) -> Result<(), SplitError> {
        let path = self.root.find_pane(pane).ok_or(SplitError::NoPane)?;
        let Cell::Floating(leaf) = self.root.get(&path) else {
            return Err(SplitError::NoPane);
        };
        let saved = leaf.g;
        self.saved_float.insert(pane, saved);
        let env = self.env();
        if !insert_tile(&mut self.root, &path, self.sx, self.sy, env) {
            return Err(SplitError::NoSpace);
        }
        let g = *self.root.get(&path).g();
        *self.root.get_mut(&path) = Cell::Pane(Leaf { pane, g });
        fix_offsets(&mut self.root);
        Ok(())
    }

    /// The floating geometry `pane`'s cell had before it was tiled
    /// (`lc->fg`), if any.
    #[must_use]
    pub fn saved_float(&self, pane: PaneId) -> Option<Geometry> {
        self.saved_float.get(&pane).copied()
    }
}

/// `layout_resize_set_size`.
fn resize_set_size<L>(root: &mut Cell<L>, path: &[usize], dir: Dir, size: u32, env: Env) {
    let change = to_i32(size).wrapping_sub(to_i32(root.get(path).g().size(dir)));
    adjust_at(root, path, dir, change, env);
}

/// `layout_insert_tile` (layout.c:2003-2060) for the cell at `path`, not
/// tiled yet: it takes half the space of its nearest neighbour, or its
/// parent is tiled first when it has none. False when the neighbour has
/// no room.
fn insert_tile<L>(root: &mut Cell<L>, path: &[usize], sx: u32, sy: u32, env: Env) -> bool {
    if root.get(path).is_tiled() {
        return false;
    }
    let Some((&idx, pp)) = path.split_last() else {
        // Only pane in the layout.
        *root.g_mut() = Geometry::new(sx, sy, 0, 0);
        return true;
    };
    let Cell::Node(parent) = root.get(pp) else {
        return false;
    };
    let ty = parent.dir;
    match neighbour(&parent.children, idx) {
        None => {
            insert_tile(root, pp, sx, sy, env);
            let size1 = root.get(pp).g().size(ty);
            resize_set_size(root, path, ty, size1, env);
        }
        Some(n) => {
            let mut np = pp.to_vec();
            np.push(n);
            let g = *root.get(&np).g();
            if !split_check_space(&g, ty, env, pos_at(root, &np)) {
                return false;
            }
            let (size1, size2, _) = split_sizes(&g, -1, false, ty);
            resize_set_size(root, path, ty, size1, env);
            resize_set_size(root, &np, ty, size2, env);
        }
    }
    let pg = *root.get(pp).g();
    let (other, size) = match ty {
        Dir::LeftRight => (Dir::TopBottom, pg.sy),
        Dir::TopBottom => (Dir::LeftRight, pg.sx),
    };
    resize_set_size(root, path, other, size, env);
    true
}

/// `layout_clamp_floating_panes` for one subtree.
fn clamp_walk(
    c: &mut Cell<PaneId>,
    sx: u32,
    sy: u32,
    borderless: &std::collections::BTreeSet<PaneId>,
) {
    match c {
        Cell::Floating(l) => {
            let pad = u32::from(!borderless.contains(&l.pane));
            let g = &mut l.g;
            let avail = sx.saturating_sub(2 * pad);
            if g.sx > avail {
                g.sx = avail.max(PANE_MINIMUM);
            }
            let avail = sy.saturating_sub(2 * pad);
            if g.sy > avail {
                g.sy = avail.max(PANE_MINIMUM);
            }
            let padi = to_i32(pad);
            if g.xoff + to_i32(g.sx) + padi > to_i32(sx) {
                g.xoff = if g.sx + 2 * pad >= sx {
                    padi
                } else {
                    to_i32(sx) - to_i32(g.sx) - padi
                };
            }
            if g.yoff + to_i32(g.sy) + padi > to_i32(sy) {
                g.yoff = if g.sy + 2 * pad >= sy {
                    padi
                } else {
                    to_i32(sy) - to_i32(g.sy) - padi
                };
            }
        }
        Cell::Node(n) => n
            .children
            .iter_mut()
            .for_each(|c| clamp_walk(c, sx, sy, borderless)),
        Cell::Pane(_) => {}
    }
}

/// `layout_resize_pane_grow` for child `idx` of `parent`.
fn grow_step<L>(
    parent: &mut Node<L>,
    idx: usize,
    dir: Dir,
    needed: i32,
    opposite: bool,
    env: Env,
    ppos: Pos,
) -> u32 {
    let check = |parent: &Node<L>, i: usize| {
        resize_check(&parent.children[i], dir, env, child_pos(parent, i, ppos))
    };
    let mut size = 0;
    let mut remove = neighbour_dir(&parent.children, idx, true);
    while let Some(r) = remove {
        size = check(parent, r);
        if size > 0 {
            break;
        }
        remove = neighbour_dir(&parent.children, r, true);
    }
    if opposite && remove.is_none() {
        remove = neighbour_dir(&parent.children, idx, false);
        while let Some(r) = remove {
            size = check(parent, r);
            if size > 0 {
                break;
            }
            remove = neighbour_dir(&parent.children, r, false);
        }
    }
    let Some(remove) = remove else { return 0 };
    if i64::from(size) > i64::from(needed) {
        size = needed as u32;
    }
    let (pa, pr) = (
        child_pos(parent, idx, ppos),
        child_pos(parent, remove, ppos),
    );
    resize_adjust(&mut parent.children[idx], dir, to_i32(size), env, pa);
    resize_adjust(&mut parent.children[remove], dir, -to_i32(size), env, pr);
    size
}

/// `layout_resize_pane_shrink` for child `idx` of `parent`.
fn shrink_step<L>(
    parent: &mut Node<L>,
    idx: usize,
    dir: Dir,
    needed: i32,
    env: Env,
    ppos: Pos,
) -> u32 {
    let mut remove = Some(idx);
    let mut size = 0;
    while let Some(r) = remove {
        size = resize_check(&parent.children[r], dir, env, child_pos(parent, r, ppos));
        if size != 0 {
            break;
        }
        remove = neighbour_dir(&parent.children, r, false);
    }
    let Some(remove) = remove else { return 0 };
    let Some(add) = neighbour_dir(&parent.children, idx, true) else {
        return 0;
    };
    let want = needed.wrapping_neg() as u32;
    if size > want {
        size = want;
    }
    let (pa, pr) = (
        child_pos(parent, add, ppos),
        child_pos(parent, remove, ppos),
    );
    resize_adjust(&mut parent.children[add], dir, to_i32(size), env, pa);
    resize_adjust(&mut parent.children[remove], dir, -to_i32(size), env, pr);
    size
}

#[cfg(test)]
mod float_tests {
    use super::*;

    /// Two panes side by side in an 80x24 window: %1 left, %2 right.
    fn two() -> Layout {
        let mut l = Layout::new(80, 24, PaneId(1));
        let req = SplitRequest {
            horizontal: true,
            ..SplitRequest::default()
        };
        l.split(PaneId(1), PaneId(2), &req).unwrap();
        l
    }

    /// break-pane -W: the floated pane's space goes to its neighbour, and
    /// joining it to itself takes half back (the reference's layouts for
    /// the same commands, `oracle/scenarios/floating_break_move.toml`).
    #[test]
    fn float_then_tile_gives_and_takes_space() {
        let mut l = two();
        assert!(l.float_pane(PaneId(2), Geometry::new(40, 6, 4, 2)));
        assert_eq!(l.pane_rect(PaneId(1)), Some(Geometry::new(80, 24, 0, 0)));
        assert_eq!(l.pane_rect(PaneId(2)), Some(Geometry::new(40, 6, 4, 2)));
        assert!(
            !l.float_pane(PaneId(2), Geometry::default()),
            "already floating"
        );
        l.tile_pane(PaneId(2)).unwrap();
        assert_eq!(l.pane_rect(PaneId(1)), Some(Geometry::new(39, 24, 0, 0)));
        assert_eq!(l.pane_rect(PaneId(2)), Some(Geometry::new(40, 24, 40, 0)));
        assert_eq!(l.saved_float(PaneId(2)), Some(Geometry::new(40, 6, 4, 2)));
    }

    /// A pane floated again starts from where it floated before, and
    /// `-x` / `-y` (with the border taken off) override that.
    #[test]
    fn floating_again_starts_from_the_saved_geometry() {
        let mut l = two();
        l.float_pane(PaneId(2), Geometry::new(30, 8, 6, 6));
        l.tile_pane(PaneId(2)).unwrap();
        let start = l.saved_float(PaneId(2));
        let g = l
            .floating_geometry(&FloatArgs::default(), true, start)
            .unwrap();
        assert_eq!(g, Geometry::new(30, 8, 6, 6));
        let args = FloatArgs {
            width: Some("20".into()),
            ..FloatArgs::default()
        };
        let g = l.floating_geometry(&args, true, start).unwrap();
        assert_eq!(g, Geometry::new(18, 8, 6, 6));
    }

    /// A borderless floating pane is kept inside the window without the
    /// one-cell pad (`layout_clamp_floating_panes`).
    #[test]
    fn clamp_pads_only_bordered_panes() {
        let mut l = Layout::new(80, 24, PaneId(1));
        l.add_floating(None, PaneId(2), Geometry::new(10, 4, 75, 22));
        l.add_floating(Some(PaneId(2)), PaneId(3), Geometry::new(10, 4, 75, 22));
        l.borderless.insert(PaneId(3));
        l.resize_window(60, 20);
        assert_eq!(l.pane_rect(PaneId(2)), Some(Geometry::new(10, 4, 49, 15)));
        assert_eq!(l.pane_rect(PaneId(3)), Some(Geometry::new(10, 4, 50, 16)));
    }
}
