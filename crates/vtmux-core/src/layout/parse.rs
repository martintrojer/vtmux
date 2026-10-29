//! Layout strings in: layout-custom.c `layout_parse` and its helpers. v1 and
//! v2 share the post-processing; the pane list the window already has decides
//! which cell gets which pane.

use super::arith::{self, Env};
use super::cell::{Cell, Dir, Geometry, Leaf, Node, PANE_MAXIMUM, PANE_MINIMUM, WINDOW_MAXIMUM};
use super::emit::checksum;
use super::json::{self, Object};
use super::window::WindowPanes;
use crate::ids::{PaneId, WindowId};

/// One pane of the window being laid out: its id (in window order) and, when
/// it floats now, its cell's geometry (v1 keeps floating panes as they are).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPane {
    pub id: PaneId,
    pub floating: Option<Geometry>,
}

/// The per-pane facts of a v2 string (`layout_parse_cell_ctx`).
#[derive(Clone, Copy, Debug, Default)]
struct Rec {
    active: Option<bool>,
    last: Option<u32>,
    index: u32,
    z: Option<u32>,
}

/// What a parsed v2 string says about window state, applied to a
/// [`WindowPanes`] by [`ParsedState::apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedState {
    /// The pane marked `"a":true`.
    pub active: Option<PaneId>,
    /// The new last-used stack, most recent first.
    pub last: Vec<PaneId>,
    /// The floating panes, topmost first (by `"z"` ascending).
    pub floating_z: Vec<PaneId>,
}

impl ParsedState {
    /// `layout_parse_apply_ctx` (layout-custom.c:1154-1203).
    pub fn apply(&self, win: &mut WindowPanes, root: &Cell<PaneId>) {
        let floating = |p: PaneId| {
            root.find_pane(p)
                .is_some_and(|x| root.get(&x).is_floating())
        };
        win.z_order.retain(|&p| !floating(p));
        let mut z = self.floating_z.clone();
        z.append(&mut win.z_order);
        win.z_order = z;
        if let Some(a) = self.active {
            win.select(a);
        }
        win.last.clone_from(&self.last);
    }
}

/// A successfully parsed layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub root: Cell<PaneId>,
    /// `window_resize` to this when the layout has tiled cells.
    pub resize: Option<(u32, u32)>,
    /// v2 only.
    pub state: Option<ParsedState>,
}

const V1_MAX_DEPTH: u32 = 1000;

/// `%hx,%n` with `n == 5` (layout-custom.c:1052). `Some((csum, rest))`.
fn header(input: &str) -> Option<(u16, &str)> {
    let b = input.as_bytes();
    let mut i = 0;
    let neg = match b.first() {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        i += 2;
    }
    let start = i;
    while b.get(i).is_some_and(u8::is_ascii_hexdigit) {
        i += 1;
    }
    if i == start || b.get(i) != Some(&b',') || i + 1 != 5 {
        return None;
    }
    let v = u32::from_str_radix(&input[start..i], 16).ok()?;
    let v = if neg { v.wrapping_neg() } else { v };
    Some((v as u16, &input[i + 1..]))
}

/// `layout_construct_cell`: `WxH,X,Y` and an optional `,id` that is skipped.
fn v1_cell(s: &[u8], pos: &mut usize) -> Option<Cell<Rec>> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let digits = |i: usize| {
        (i..s.len().max(i))
            .take_while(|&k| at(k).is_ascii_digit())
            .count()
    };
    let num = |i: usize, max_len: usize| -> Option<u32> {
        let n = digits(i);
        if n == 0 || n > max_len {
            return None;
        }
        std::str::from_utf8(&s[i..i + n]).ok()?.parse().ok()
    };
    let p = *pos;
    if !at(p).is_ascii_digit() {
        return None;
    }
    // sscanf "%5ux%5u,%5d,%5d": each of the first three is 1-5 digits
    // before its separator; the last is read to 5 digits only.
    let sx = num(p, 5)?;
    let mut q = p + digits(p);
    if at(q) != b'x' {
        return None;
    }
    q += 1;
    let sy = num(q, 5)?;
    q += digits(q);
    if at(q) != b',' {
        return None;
    }
    q += 1;
    let xoff = num(q, 5)?;
    q += digits(q);
    if at(q) != b',' {
        return None;
    }
    q += 1;
    let nd = digits(q);
    if nd == 0 {
        return None;
    }
    let yoff: u32 = std::str::from_utf8(&s[q..q + nd.min(5)])
        .ok()?
        .parse()
        .ok()?;
    q += nd;
    if !(PANE_MINIMUM..=PANE_MAXIMUM).contains(&sx)
        || !(PANE_MINIMUM..=PANE_MAXIMUM).contains(&sy)
        || xoff > WINDOW_MAXIMUM
        || yoff > WINDOW_MAXIMUM
    {
        return None;
    }
    if at(q) == b',' {
        let saved = q;
        q += 1;
        q += digits(q);
        if at(q) == b'x' {
            q = saved;
        }
    }
    *pos = q;
    Some(Cell::Pane(Leaf {
        pane: Rec::default(),
        g: Geometry::new(sx, sy, xoff as i32, yoff as i32),
    }))
}

/// `layout_construct_v1`.
fn v1_tree(s: &[u8], pos: &mut usize, depth: u32) -> Option<Cell<Rec>> {
    if depth > V1_MAX_DEPTH {
        return None;
    }
    let cell = v1_cell(s, pos)?;
    let dir = match s.get(*pos).copied().unwrap_or(0) {
        b',' | b'}' | b']' | 0 => return Some(cell),
        b'{' => Dir::LeftRight,
        b'[' => Dir::TopBottom,
        _ => return None,
    };
    let mut children = Vec::new();
    loop {
        *pos += 1;
        children.push(v1_tree(s, pos, depth + 1)?);
        if s.get(*pos) != Some(&b',') {
            break;
        }
    }
    let close = if dir == Dir::LeftRight { b'}' } else { b']' };
    if s.get(*pos) != Some(&close) {
        return None;
    }
    *pos += 1;
    Some(Cell::Node(Node {
        dir,
        g: *cell.g(),
        children,
    }))
}

fn range(o: &Object, key: &str, lo: i64, hi: i64, what: &str) -> Result<i64, String> {
    let n = o.number(key)?;
    if n < lo || n > hi {
        return Err(format!("invalid {what} {n}"));
    }
    Ok(n)
}

/// `layout_parse_json_layout`.
fn json_cell(o: &Object, num_active: &mut u32) -> Result<Cell<Rec>, String> {
    let ty = o.string("t")?;
    let dir = match ty {
        "p" => None,
        "v" => Some(Dir::TopBottom),
        "h" => Some(Dir::LeftRight),
        _ => return Err(format!("unknown cell type \"{ty}\"")),
    };
    let (min, max) = (i64::from(PANE_MINIMUM), i64::from(PANE_MAXIMUM));
    let wm = i64::from(WINDOW_MAXIMUM);
    let sx = range(o, "w", min, max, "width")? as u32;
    let sy = range(o, "h", min, max, "height")? as u32;
    let xoff = range(o, "x", -wm, wm, "x-offset")? as i32;
    let yoff = range(o, "y", -wm, wm, "y-offset")? as i32;
    let g = Geometry::new(sx, sy, xoff, yoff);
    let Some(dir) = dir else {
        if o.find("c").is_some() {
            return Err("panes cannot have children".to_owned());
        }
        let imax = i64::from(i32::MAX);
        let mut rec = Rec {
            index: range(o, "i", 0, imax, "index")? as u32,
            ..Rec::default()
        };
        if o.find("a").is_some() {
            let a = o.boolean("a")?;
            rec.active = Some(a);
            if a {
                *num_active += 1;
            }
        } else if o.find("l").is_some() {
            rec.last = Some(range(o, "l", 0, imax, "last")? as u32);
        }
        let floating = o.find("z").is_some();
        if floating {
            rec.z = Some(range(o, "z", 0, imax - 1, "floating zindex")? as u32);
        }
        let leaf = Leaf { pane: rec, g };
        return Ok(if floating {
            Cell::Floating(leaf)
        } else {
            Cell::Pane(leaf)
        });
    };
    let kids = o.array("c")?;
    if kids.len() < 2 {
        return Err("nodes must have more than one child".to_owned());
    }
    let children = kids
        .iter()
        .map(|k| json_cell(k, num_active))
        .collect::<Result<_, _>>()?;
    Ok(Cell::Node(Node { dir, g, children }))
}

/// Records in depth-first order.
fn recs(c: &Cell<Rec>) -> Vec<Rec> {
    c.leaves().into_iter().map(|(r, _, _)| *r).collect()
}

/// `layout_parse_ctx_check_indexes`.
fn check_indexes(r: &[Rec]) -> Result<(), String> {
    let dup = |mut v: Vec<u32>| {
        v.sort_unstable();
        v.windows(2).any(|w| w[0] == w[1])
    };
    if dup(r.iter().map(|x| x.index).collect()) {
        return Err("duplicate pane index".to_owned());
    }
    if dup(r.iter().filter_map(|x| x.z).collect()) {
        return Err("duplicate pane z-index".to_owned());
    }
    if dup(r.iter().filter_map(|x| x.last).collect()) {
        return Err("duplicate last pane index".to_owned());
    }
    Ok(())
}

/// `layout_construct`: the cell tree and the version.
fn construct(input: &str) -> Result<(Cell<Rec>, i64), String> {
    let input = input.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}');
    if !input.starts_with('{') {
        let (csum, rest) = header(input).ok_or("malformed layout header")?;
        if csum != checksum(rest) {
            return Err("invalid layout checksum".to_owned());
        }
        let s = rest.as_bytes();
        let mut pos = 0;
        let root = v1_tree(s, &mut pos, 0).ok_or("invalid layout")?;
        if pos != s.len() {
            return Err("trailing data".to_owned());
        }
        return Ok((root, 1));
    }
    let top = json::parse(input)?;
    let version = top.number("V")?;
    let mut num_active = 0;
    let root = json_cell(top.object("L")?, &mut num_active)?;
    if version != 2 {
        return Err("version mismatch".to_owned());
    }
    if num_active > 1 {
        return Err("more than one active pane".to_owned());
    }
    check_indexes(&recs(&root))?;
    Ok((root, version))
}

/// `layout_find_bottomright`: the path to the last leaf.
fn bottom_right<L>(root: &Cell<L>) -> Vec<usize> {
    let mut path = Vec::new();
    let mut c = root;
    while let Cell::Node(n) = c {
        let i = n.children.len() - 1;
        path.push(i);
        c = &n.children[i];
    }
    path
}

/// `layout_check`: do the sizes fit?
fn check<L>(c: &Cell<L>) -> bool {
    let Cell::Node(n) = c else { return true };
    let mut total = 0u32;
    for ch in n.children.iter().filter(|c| c.occupies()) {
        let (own, other) = match n.dir {
            Dir::LeftRight => (ch.g().sx, (ch.g().sy, n.g.sy)),
            Dir::TopBottom => (ch.g().sy, (ch.g().sx, n.g.sx)),
        };
        if other.0 != other.1 || !check(ch) {
            return false;
        }
        total = total.wrapping_add(own).wrapping_add(1);
    }
    total == 0 || total.wrapping_sub(1) == n.g.size(n.dir)
}

/// `layout_parse` (layout-custom.c:582-722): parse `input` for the window
/// `window` whose panes are `panes`, in window order. `Err` is tmux's cause
/// string; `select-layout` prints `cause: input`.
pub fn parse(window: WindowId, input: &str, panes: &[WindowPane]) -> Result<Parsed, String> {
    let (mut root, version) = construct(input)?;
    let with_floating = version > 1;
    let npanes = panes
        .iter()
        .filter(|p| with_floating || p.floating.is_none())
        .count();
    if npanes == 0 {
        return Err(format!("window {window} has no panes"));
    }
    let env = Env {
        status: super::PaneStatus::Off,
        scrollbar: None,
    };
    loop {
        let ncells = root.count_cells(with_floating);
        if npanes > ncells {
            return Err(format!("have {npanes} panes but need {ncells}"));
        }
        if npanes == ncells {
            break;
        }
        let path = bottom_right(&root);
        if !arith::destroy_cell(&mut root, &path, env) {
            break;
        }
    }

    // Older tmux wrote a root larger than its children: correct it.
    if let Cell::Node(n) = &root {
        let (mut sx, mut sy) = (0u32, 0u32);
        for ch in n.children.iter().filter(|c| c.occupies()) {
            match n.dir {
                Dir::LeftRight => {
                    sy = ch.g().sy.wrapping_add(1);
                    sx = sx.wrapping_add(ch.g().sx).wrapping_add(1);
                }
                Dir::TopBottom => {
                    sx = ch.g().sx.wrapping_add(1);
                    sy = sy.wrapping_add(ch.g().sy).wrapping_add(1);
                }
            }
        }
        if sx != 0 && sy != 0 && (n.g.sx != sx || n.g.sy != sy) {
            let Cell::Node(n) = &mut root else {
                unreachable!()
            };
            n.g.sx = sx.wrapping_sub(1);
            n.g.sy = sy.wrapping_sub(1);
        }
    }
    if !check(&root) {
        return Err("size mismatch after applying layout".to_owned());
    }
    let resize = root.occupies().then(|| (root.g().sx, root.g().sy));

    let (mut root, state) = if version > 1 {
        assign_from_recs(root, panes)
    } else {
        (assign_fallback(root, panes), None)
    };
    arith::fix_offsets(&mut root);
    Ok(Parsed {
        root,
        resize,
        state,
    })
}

/// `layout_assign_from_ctx` and the v2 half of `layout_parse_apply_ctx`:
/// the i-th smallest `"i"` gets the i-th pane of the window.
fn assign_from_recs(root: Cell<Rec>, panes: &[WindowPane]) -> (Cell<PaneId>, Option<ParsedState>) {
    let mut order: Vec<(u32, usize)> = recs(&root)
        .iter()
        .enumerate()
        .map(|(k, r)| (r.index, k))
        .collect();
    order.sort_unstable();
    let mut pane_of = vec![PaneId(0); order.len()];
    for (slot, (_, k)) in order.iter().enumerate() {
        pane_of[*k] = panes[slot].id;
    }
    let all = recs(&root);
    let mut k = 0;
    let root = root.map(&mut |_| {
        let p = pane_of[k];
        k += 1;
        p
    });

    let mut floating: Vec<(u32, PaneId)> = all
        .iter()
        .enumerate()
        .filter_map(|(k, r)| r.z.map(|z| (z, pane_of[k])))
        .collect();
    floating.sort_unstable();
    let active = all
        .iter()
        .position(|r| r.active == Some(true))
        .map(|k| pane_of[k]);
    let mut last: Vec<(u32, PaneId)> = all
        .iter()
        .enumerate()
        .filter(|(_, r)| r.active != Some(true))
        .filter_map(|(k, r)| r.last.map(|l| (l, pane_of[k])))
        .collect();
    last.sort_unstable();
    let state = ParsedState {
        active,
        last: last.into_iter().map(|(_, p)| p).collect(),
        floating_z: floating.into_iter().map(|(_, p)| p).collect(),
    };
    (root, Some(state))
}

/// `layout_assign_fallback`: tiled cells take the window's tiled panes in
/// order; floating panes keep their cells and become the root's last
/// children.
fn assign_fallback(root: Cell<Rec>, panes: &[WindowPane]) -> Cell<PaneId> {
    let mut tiled = panes.iter().filter(|p| p.floating.is_none());
    let mut root = root.map(&mut |_| tiled.next().map_or(PaneId(0), |p| p.id));
    let floats: Vec<_> = panes
        .iter()
        .filter_map(|p| p.floating.map(|g| Cell::Floating(Leaf { pane: p.id, g })))
        .collect();
    if floats.is_empty() {
        return root;
    }
    if matches!(root, Cell::Pane(_)) {
        let g = *root.g();
        root = Cell::Node(Node {
            dir: Dir::TopBottom,
            g,
            children: vec![root],
        });
    }
    if let Cell::Node(n) = &mut root {
        n.children.extend(floats);
    }
    root
}

impl super::Layout {
    /// The window's panes as [`parse`] wants them.
    #[must_use]
    pub fn pane_list(&self, win: &WindowPanes) -> Vec<WindowPane> {
        win.panes
            .iter()
            .map(|&id| WindowPane {
                id,
                floating: self.root.find_pane(id).and_then(|p| {
                    let c = self.root.get(&p);
                    c.is_floating().then(|| *c.g())
                }),
            })
            .collect()
    }

    /// `select-layout <string>`: parse `input` and make it the layout.
    pub fn select_custom(
        &mut self,
        window: WindowId,
        win: &mut WindowPanes,
        input: &str,
    ) -> Result<(), String> {
        let parsed = parse(window, input, &self.pane_list(win))?;
        if let Some((sx, sy)) = parsed.resize {
            self.sx = sx;
            self.sy = sy;
        }
        self.root = parsed.root;
        if let Some(state) = parsed.state {
            state.apply(win, &self.root);
        }
        Ok(())
    }
}
