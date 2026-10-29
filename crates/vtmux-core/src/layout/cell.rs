//! The cell tree: tiled cells and the floating layer (D15), with tmux's
//! one-cell borders (layout.c). `L` is what a leaf holds: a [`PaneId`] in a
//! window, a parse record while a layout string is being read.

use crate::ids::PaneId;

/// Smallest pane (tmux.h `PANE_MINIMUM`).
pub const PANE_MINIMUM: u32 = 1;
/// Largest pane (tmux.h `PANE_MAXIMUM`).
pub const PANE_MAXIMUM: u32 = 10_000;
/// Largest window and offset (tmux.h `WINDOW_MAXIMUM`).
pub const WINDOW_MAXIMUM: u32 = 10_000;

/// Which way a node lays out its children: tmux's `LAYOUT_LEFTRIGHT`
/// (`h`, `{}` in v1) or `LAYOUT_TOPBOTTOM` (`v`, `[]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    LeftRight,
    TopBottom,
}

/// A cell's rectangle. Sizes are what tmux's `u_int` holds; offsets are
/// signed because a floating cell may sit partly outside the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    pub sx: u32,
    pub sy: u32,
    pub xoff: i32,
    pub yoff: i32,
}

impl Geometry {
    #[must_use]
    pub fn new(sx: u32, sy: u32, xoff: i32, yoff: i32) -> Self {
        Self { sx, sy, xoff, yoff }
    }

    /// The size along `dir` (`sx` for left-right, `sy` for top-bottom).
    #[must_use]
    pub fn size(&self, dir: Dir) -> u32 {
        match dir {
            Dir::LeftRight => self.sx,
            Dir::TopBottom => self.sy,
        }
    }

    /// Set the size along `dir`.
    pub fn grow_to(&mut self, dir: Dir, size: u32) {
        match dir {
            Dir::LeftRight => self.sx = size,
            Dir::TopBottom => self.sy = size,
        }
    }

    /// `lc->g.sx += change`, wrapping as C's unsigned arithmetic does.
    pub fn grow(&mut self, dir: Dir, change: i32) {
        match dir {
            Dir::LeftRight => self.sx = self.sx.wrapping_add_signed(change),
            Dir::TopBottom => self.sy = self.sy.wrapping_add_signed(change),
        }
    }
}

/// A cell holding one pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf<L = PaneId> {
    pub pane: L,
    pub g: Geometry,
}

/// A cell holding child cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node<L = PaneId> {
    pub dir: Dir,
    pub g: Geometry,
    pub children: Vec<Cell<L>>,
}

/// One cell. A floating cell is a leaf tmux flags `LAYOUT_CELL_FLOATING`; it
/// takes no space from the tiled layout and keeps its own geometry. tmux
/// appends floating cells to the root's children, but also inserts one after
/// the target pane's cell in any node, so `Floating` may appear anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell<L = PaneId> {
    Pane(Leaf<L>),
    Floating(Leaf<L>),
    Node(Node<L>),
}

/// A route from the root: the child index taken at each node.
pub type Path = Vec<usize>;

impl<L> Cell<L> {
    #[must_use]
    pub fn g(&self) -> &Geometry {
        match self {
            Self::Pane(l) | Self::Floating(l) => &l.g,
            Self::Node(n) => &n.g,
        }
    }

    pub fn g_mut(&mut self) -> &mut Geometry {
        match self {
            Self::Pane(l) | Self::Floating(l) => &mut l.g,
            Self::Node(n) => &mut n.g,
        }
    }

    /// `layout_cell_is_tiled`: a leaf that is not floating.
    #[must_use]
    pub fn is_tiled(&self) -> bool {
        matches!(self, Self::Pane(_))
    }

    #[must_use]
    pub fn is_floating(&self) -> bool {
        matches!(self, Self::Floating(_))
    }

    #[must_use]
    pub fn children(&self) -> &[Cell<L>] {
        match self {
            Self::Node(n) => &n.children,
            _ => &[],
        }
    }

    /// `layout_cell_has_tiled_child`.
    #[must_use]
    pub fn has_tiled_child(&self) -> bool {
        self.children().iter().any(Cell::occupies)
    }

    /// Tiled, or a node with a tiled leaf below it: tmux's "neighbour"
    /// test, the cells that take space from the tiled layout.
    #[must_use]
    pub fn occupies(&self) -> bool {
        self.is_tiled() || self.has_tiled_child()
    }

    /// The cell at `path`. Panics on a path that is not in the tree.
    #[must_use]
    pub fn get(&self, path: &[usize]) -> &Cell<L> {
        path.iter().fold(self, |c, &i| &c.children()[i])
    }

    pub fn get_mut(&mut self, path: &[usize]) -> &mut Cell<L> {
        path.iter().fold(self, |c, &i| match c {
            Self::Node(n) => &mut n.children[i],
            _ => panic!("path through a leaf"),
        })
    }

    /// Rebuild the tree with every leaf payload passed through `f`
    /// (depth-first, in child order).
    pub fn map<M>(self, f: &mut impl FnMut(L) -> M) -> Cell<M> {
        match self {
            Self::Pane(l) => Cell::Pane(Leaf {
                pane: f(l.pane),
                g: l.g,
            }),
            Self::Floating(l) => Cell::Floating(Leaf {
                pane: f(l.pane),
                g: l.g,
            }),
            Self::Node(n) => Cell::Node(Node {
                dir: n.dir,
                g: n.g,
                children: n.children.into_iter().map(|c| c.map(f)).collect(),
            }),
        }
    }

    /// Every leaf with its geometry and whether it floats, depth-first.
    #[must_use]
    pub fn leaves(&self) -> Vec<(&L, &Geometry, bool)> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves<'a>(&'a self, out: &mut Vec<(&'a L, &'a Geometry, bool)>) {
        match self {
            Self::Pane(l) => out.push((&l.pane, &l.g, false)),
            Self::Floating(l) => out.push((&l.pane, &l.g, true)),
            Self::Node(n) => n.children.iter().for_each(|c| c.collect_leaves(out)),
        }
    }

    /// Leaves in the tiled layout, depth-first.
    #[must_use]
    pub fn count_cells(&self, with_floating: bool) -> usize {
        match self {
            Self::Pane(_) => 1,
            Self::Floating(_) => usize::from(with_floating),
            Self::Node(n) => n
                .children
                .iter()
                .map(|c| c.count_cells(with_floating))
                .sum(),
        }
    }
}

impl Cell<PaneId> {
    /// The path to the leaf holding `pane`.
    #[must_use]
    pub fn find_pane(&self, pane: PaneId) -> Option<Path> {
        match self {
            Self::Pane(l) | Self::Floating(l) => (l.pane == pane).then(Vec::new),
            Self::Node(n) => n.children.iter().enumerate().find_map(|(i, c)| {
                c.find_pane(pane).map(|mut p| {
                    p.insert(0, i);
                    p
                })
            }),
        }
    }
}

/// `layout_cell_get_neighbour_dir`: the nearest tiled-or-tiled-containing
/// sibling of `children[idx]`, forwards or backwards.
pub(crate) fn neighbour_dir<L>(children: &[Cell<L>], idx: usize, forward: bool) -> Option<usize> {
    let mut i = idx;
    loop {
        if forward {
            i += 1;
            if i >= children.len() {
                return None;
            }
        } else {
            i = i.checked_sub(1)?;
        }
        if children[i].occupies() {
            return Some(i);
        }
    }
}

/// `layout_cell_get_neighbour`: prefers the next sibling, the previous one
/// when `idx` is the last child.
pub(crate) fn neighbour<L>(children: &[Cell<L>], idx: usize) -> Option<usize> {
    let forward = idx + 1 != children.len();
    neighbour_dir(children, idx, forward).or_else(|| neighbour_dir(children, idx, !forward))
}

pub(crate) fn first_tiled<L>(children: &[Cell<L>]) -> Option<usize> {
    children.iter().position(Cell::occupies)
}

pub(crate) fn last_tiled<L>(children: &[Cell<L>]) -> Option<usize> {
    children.iter().rposition(Cell::occupies)
}
