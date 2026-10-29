//! The named layouts, ported from layout-set.c (3.8): `even-horizontal`,
//! `even-vertical`, `main-horizontal`, `main-horizontal-mirrored`,
//! `main-vertical`, `main-vertical-mirrored` and `tiled`.
//!
//! Each rebuilds the tree from the window's panes in window order. Floating
//! cells keep their geometry; tmux puts them wherever the pane order falls
//! (even and the "other" node of main-*) or appends them to the root
//! (`layout_set_link_floating`, tiled and the two-pane main-* case), and so
//! does this.

use super::Layout;
use super::arith::{self, Pos};
use super::cell::{Cell, Dir, Geometry, Leaf, Node, PANE_MINIMUM};
use crate::ids::PaneId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedLayout {
    EvenHorizontal,
    EvenVertical,
    MainHorizontal,
    MainHorizontalMirrored,
    MainVertical,
    MainVerticalMirrored,
    Tiled,
}

/// The window options the named layouts read (defaults are tmux's).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetOpts {
    pub main_pane_height: String,
    pub main_pane_width: String,
    pub other_pane_height: String,
    pub other_pane_width: String,
    pub tiled_layout_max_columns: u32,
}

impl Default for SetOpts {
    fn default() -> Self {
        Self {
            main_pane_height: "24".to_owned(),
            main_pane_width: "80".to_owned(),
            other_pane_height: "0".to_owned(),
            other_pane_width: "0".to_owned(),
            tiled_layout_max_columns: 0,
        }
    }
}

impl NamedLayout {
    /// `layout_sets[]`, in tmux's order (what next-layout cycles through).
    pub const ALL: [Self; 7] = [
        Self::EvenHorizontal,
        Self::EvenVertical,
        Self::MainHorizontal,
        Self::MainHorizontalMirrored,
        Self::MainVertical,
        Self::MainVerticalMirrored,
        Self::Tiled,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::EvenHorizontal => "even-horizontal",
            Self::EvenVertical => "even-vertical",
            Self::MainHorizontal => "main-horizontal",
            Self::MainHorizontalMirrored => "main-horizontal-mirrored",
            Self::MainVertical => "main-vertical",
            Self::MainVerticalMirrored => "main-vertical-mirrored",
            Self::Tiled => "tiled",
        }
    }

    /// `layout_set_lookup`: an exact name, else the one name `name` is a
    /// prefix of (ambiguous or unknown: `None`).
    #[must_use]
    pub fn lookup(name: &str) -> Option<Self> {
        if let Some(l) = Self::ALL.iter().find(|l| l.name() == name) {
            return Some(*l);
        }
        let mut hits = Self::ALL.iter().filter(|l| l.name().starts_with(name));
        match (hits.next(), hits.next()) {
            (Some(l), None) => Some(*l),
            _ => None,
        }
    }

    /// `layout_set_next`: the layout after `last` (`w->lastlayout`).
    #[must_use]
    pub fn next(last: Option<Self>) -> Self {
        match last.and_then(|l| Self::ALL.iter().position(|&x| x == l)) {
            Some(i) => Self::ALL[(i + 1) % Self::ALL.len()],
            None => Self::ALL[0],
        }
    }

    /// `layout_set_previous`.
    #[must_use]
    pub fn previous(last: Option<Self>) -> Self {
        let n = Self::ALL.len();
        match last.and_then(|l| Self::ALL.iter().position(|&x| x == l)) {
            Some(i) => Self::ALL[(i + n - 1) % n],
            None => Self::ALL[n - 1],
        }
    }
}

fn leaf(pane: PaneId, g: Geometry) -> Cell<PaneId> {
    Cell::Pane(Leaf { pane, g })
}

/// The main/other size computation shared by the four main-* layouts.
/// `avail` is the window size less one border; `main` and `other` are the
/// option strings. Returns (main, other).
fn main_sizes(avail: u32, main: &str, other: &str, default_main: u32) -> (u32, u32) {
    let pct = |s: &str| arith::percentage(s, 0, i64::from(avail), i64::from(avail));
    let mut mainv = pct(main).map_or(default_main, |v| v as u32);
    if mainv + PANE_MINIMUM >= avail {
        mainv = if avail <= PANE_MINIMUM + PANE_MINIMUM {
            PANE_MINIMUM
        } else {
            avail - PANE_MINIMUM
        };
        return (mainv, PANE_MINIMUM);
    }
    let otherv = match pct(other) {
        Ok(0) | Err(_) => avail - mainv,
        Ok(v) => {
            let v = v as u32;
            if v > avail || avail - v < mainv {
                avail - mainv
            } else {
                mainv = avail - v;
                v
            }
        }
    };
    (mainv, otherv)
}

impl Layout {
    /// The window's cells for `panes`, in window order: (pane, cell is
    /// tiled, the cell).
    fn cells_in_order(&self, panes: &[PaneId]) -> Vec<(PaneId, Cell<PaneId>)> {
        panes
            .iter()
            .filter_map(|&p| {
                let path = self.root.find_pane(p)?;
                Some((p, self.root.get(&path).clone()))
            })
            .collect()
    }

    /// `layout_set_select`: arrange the window's panes (`panes`, window
    /// order) as `which`. Returns false when there was nothing to do (fewer
    /// than two tiled panes), as tmux does.
    pub fn select_named(&mut self, which: NamedLayout, panes: &[PaneId], opts: &SetOpts) -> bool {
        let cells = self.cells_in_order(panes);
        let n = cells.iter().filter(|(_, c)| c.is_tiled()).count();
        if n <= 1 {
            return false;
        }
        let root = match which {
            NamedLayout::EvenHorizontal => self.even(cells, Dir::LeftRight),
            NamedLayout::EvenVertical => self.even(cells, Dir::TopBottom),
            NamedLayout::MainHorizontal => self.main(cells, n - 1, opts, Dir::TopBottom, false),
            NamedLayout::MainHorizontalMirrored => {
                self.main(cells, n - 1, opts, Dir::TopBottom, true)
            }
            NamedLayout::MainVertical => self.main(cells, n - 1, opts, Dir::LeftRight, false),
            NamedLayout::MainVerticalMirrored => {
                self.main(cells, n - 1, opts, Dir::LeftRight, true)
            }
            NamedLayout::Tiled => self.tiled(&cells, n, opts),
        };
        self.install(root);
        true
    }

    /// Make `root` the layout and resize the window to it
    /// (`window_resize(w, lcroot->g.sx, lcroot->g.sy)`).
    fn install(&mut self, root: Cell<PaneId>) {
        self.sx = root.g().sx;
        self.sy = root.g().sy;
        self.root = root;
    }

    fn even(&mut self, cells: Vec<(PaneId, Cell<PaneId>)>, dir: Dir) -> Cell<PaneId> {
        let n = cells.iter().filter(|(_, c)| c.is_tiled()).count() as u32;
        let (sx, sy) = if dir == Dir::LeftRight {
            ((n * (PANE_MINIMUM + 1) - 1).max(self.sx), self.sy)
        } else {
            (self.sx, (n * (PANE_MINIMUM + 1) - 1).max(self.sy))
        };
        let (wsx, wsy) = (self.sx, self.sy);
        let children = cells
            .into_iter()
            .map(|(_, mut c)| {
                if c.is_tiled() {
                    let g = c.g_mut();
                    g.sx = wsx;
                    g.sy = wsy;
                }
                c
            })
            .collect();
        let mut root = Cell::Node(Node {
            dir,
            g: Geometry::new(sx, sy, 0, 0),
            children,
        });
        let env = self.env();
        arith::spread_cell(&mut root, &[], env);
        arith::fix_offsets(&mut root);
        root
    }

    /// The four main-* layouts. `dir` is the root's direction: `TopBottom`
    /// for main-horizontal, `LeftRight` for main-vertical. `n` is the number
    /// of tiled panes besides the main one.
    fn main(
        &mut self,
        cells: Vec<(PaneId, Cell<PaneId>)>,
        n: usize,
        opts: &SetOpts,
        dir: Dir,
        mirrored: bool,
    ) -> Cell<PaneId> {
        let env = self.env();
        let (wsx, wsy) = (self.sx, self.sy);
        // `dir` splits the main/other axis; `cross` is the other axis.
        let (main_s, other_s, cross_w, avail, default_main) = if dir == Dir::TopBottom {
            (
                &opts.main_pane_height,
                &opts.other_pane_height,
                wsx,
                wsy - 1,
                24,
            )
        } else {
            (
                &opts.main_pane_width,
                &opts.other_pane_width,
                wsy,
                wsx - 1,
                80,
            )
        };
        let (mainv, otherv) = main_sizes(avail, main_s, other_s, default_main);
        let nn = n as u32;
        let cross = (nn * (PANE_MINIMUM + 1) - 1).max(cross_w);
        let (rsx, rsy, main_g, other_g_of) = if dir == Dir::TopBottom {
            (
                cross,
                mainv + otherv + 1,
                Geometry::new(cross, mainv, 0, 0),
                (cross, otherv),
            )
        } else {
            (
                mainv + otherv + 1,
                cross,
                Geometry::new(mainv, cross, 0, 0),
                (otherv, cross),
            )
        };
        let mut it = cells.into_iter();
        // The main pane is the first tiled one; panes before it (floating)
        // are "other" panes too.
        let all: Vec<(PaneId, Cell<PaneId>)> = it.by_ref().collect();
        let main_at = all
            .iter()
            .position(|(_, c)| c.is_tiled())
            .expect("a tiled pane");
        let main_cell = leaf(all[main_at].0, main_g);
        let rest: Vec<(PaneId, Cell<PaneId>)> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != main_at)
            .map(|(_, x)| x.clone())
            .collect();
        let mut children = vec![main_cell];
        let other_node_dir = if dir == Dir::TopBottom {
            Dir::LeftRight
        } else {
            Dir::TopBottom
        };
        let mut spread_path = None;
        if n == 1 {
            // The next tiled pane after the main one.
            let (op, _) = all
                .iter()
                .skip(main_at + 1)
                .find(|(_, c)| c.is_tiled())
                .expect("second tiled pane");
            let og = Geometry::new(other_g_of.0, other_g_of.1, 0, 0);
            let other = leaf(*op, og);
            if mirrored {
                children.insert(0, other);
            } else {
                children.push(other);
            }
            // link_floating: every floating cell goes on the end.
            children.extend(
                all.iter()
                    .filter(|(_, c)| !c.is_tiled())
                    .map(|(_, c)| c.clone()),
            );
        } else {
            let others = rest
                .into_iter()
                .map(|(p, mut c)| {
                    if c.is_tiled() {
                        let g = if dir == Dir::TopBottom {
                            Geometry::new(PANE_MINIMUM, other_g_of.1, 0, 0)
                        } else {
                            Geometry::new(other_g_of.0, PANE_MINIMUM, 0, 0)
                        };
                        c = leaf(p, g);
                    }
                    c
                })
                .collect();
            let node = Cell::Node(Node {
                dir: other_node_dir,
                g: Geometry::new(other_g_of.0, other_g_of.1, 0, 0),
                children: others,
            });
            if mirrored {
                children.insert(0, node);
                spread_path = Some(vec![0]);
            } else {
                children.push(node);
                spread_path = Some(vec![1]);
            }
        }
        let mut root = Cell::Node(Node {
            dir,
            g: Geometry::new(rsx, rsy, 0, 0),
            children,
        });
        if let Some(p) = spread_path {
            // layout_spread_cell on the "other" node, which uses the root's
            // geometry for the pane-status border test.
            arith::spread_cell(&mut root, &p, env);
        }
        arith::fix_offsets(&mut root);
        root
    }

    fn tiled(
        &mut self,
        cells: &[(PaneId, Cell<PaneId>)],
        n: usize,
        opts: &SetOpts,
    ) -> Cell<PaneId> {
        let env = self.env();
        let (wsx, wsy) = (self.sx, self.sy);
        let max_columns = opts.tiled_layout_max_columns;
        let n32 = n as u32;
        let (mut rows, mut columns) = (1u32, 1u32);
        while rows * columns < n32 {
            rows += 1;
            if rows * columns < n32 && (max_columns == 0 || columns < max_columns) {
                columns += 1;
            }
        }
        let width = ((wsx - (columns - 1)) / columns).max(PANE_MINIMUM);
        let height = ((wsy - (rows - 1)) / rows).max(PANE_MINIMUM);
        let sx = ((width + 1) * columns - 1).max(wsx);
        let sy = ((height + 1) * rows - 1).max(wsy);

        let tiled: Vec<PaneId> = cells
            .iter()
            .filter(|(_, c)| c.is_tiled())
            .map(|(p, _)| *p)
            .collect();
        let floats: Vec<Cell<PaneId>> = cells
            .iter()
            .filter(|(_, c)| !c.is_tiled())
            .map(|(_, c)| c.clone())
            .collect();

        let mut root = Cell::Node(Node {
            dir: Dir::TopBottom,
            g: Geometry::new(sx, sy, 0, 0),
            children: Vec::new(),
        });
        let mut idx = 0usize;
        for j in 0..rows {
            if idx >= tiled.len() {
                break;
            }
            let Cell::Node(rn) = &mut root else {
                unreachable!()
            };
            if n32.wrapping_sub(j * columns) == 1 || columns == 1 {
                rn.children
                    .push(leaf(tiled[idx], Geometry::new(wsx, height, 0, 0)));
                idx += 1;
                continue;
            }
            let mut row = Vec::new();
            let mut i = 0;
            while i < columns {
                row.push(leaf(tiled[idx], Geometry::new(width, height, 0, 0)));
                idx += 1;
                if idx >= tiled.len() {
                    break;
                }
                i += 1;
            }
            if i == columns {
                i -= 1;
            }
            let used = (i + 1) * (width + 1) - 1;
            if wsx > used {
                row.last_mut()
                    .expect("row has a pane")
                    .g_mut()
                    .grow(Dir::LeftRight, (wsx - used) as i32);
            }
            rn.children.push(Cell::Node(Node {
                dir: Dir::LeftRight,
                g: Geometry::new(wsx, height, 0, 0),
                children: row,
            }));
        }
        let used = rows * height + rows - 1;
        if wsy > used {
            let Cell::Node(rn) = &mut root else {
                unreachable!()
            };
            let last = rn.children.len() - 1;
            arith::resize_adjust(
                &mut rn.children[last],
                Dir::TopBottom,
                (wsy - used) as i32,
                env,
                Pos {
                    top: last == 0,
                    bottom: true,
                },
            );
        }
        let Cell::Node(rn) = &mut root else {
            unreachable!()
        };
        rn.children.extend(floats);
        arith::fix_offsets(&mut root);
        root
    }
}
