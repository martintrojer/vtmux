//! Layout strings out: v2 JSON (`#{window_layout}`) and v1 (`csum,WxH,X,Y{..}`),
//! layout-custom.c `layout_dump`.

use std::fmt::Write;

use super::cell::{Cell, Dir};
use super::window::WindowPanes;
use crate::ids::PaneId;

/// `layout_checksum` (layout-custom.c:241): a rotate-and-add over the bytes,
/// each added as a signed `char` (x86 tmux).
#[must_use]
pub fn checksum(s: &str) -> u16 {
    s.bytes().fold(0u16, |csum, b| {
        let csum = (csum >> 1).wrapping_add((csum & 1) << 15);
        csum.wrapping_add(i16::from(b as i8) as u16)
    })
}

/// `layout_cell_zindex`: how many floating panes sit above `pane` in the
/// window's z-order.
fn zindex(root: &Cell<PaneId>, win: &WindowPanes, pane: PaneId) -> u32 {
    let mut i = 0;
    for &wq in &win.z_order {
        if wq == pane {
            break;
        }
        if root
            .find_pane(wq)
            .is_some_and(|p| root.get(&p).is_floating())
        {
            i += 1;
        }
    }
    i
}

fn append_v2(c: &Cell<PaneId>, root: &Cell<PaneId>, win: &WindowPanes, out: &mut String) -> bool {
    let g = c.g();
    let ty = match c {
        Cell::Node(n) if n.dir == Dir::TopBottom => 'v',
        Cell::Node(_) => 'h',
        _ => 'p',
    };
    let _ = write!(
        out,
        "{{\"t\":\"{ty}\",\"w\":{},\"h\":{},\"x\":{},\"y\":{}",
        g.sx, g.sy, g.xoff, g.yoff
    );
    match c {
        Cell::Node(n) => {
            if n.children.is_empty() {
                return false;
            }
            out.push_str(",\"c\":[");
            for (i, ch) in n.children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !append_v2(ch, root, win, out) {
                    return false;
                }
            }
            out.push(']');
        }
        Cell::Pane(l) | Cell::Floating(l) => {
            if win.active == Some(l.pane) {
                out.push_str(",\"a\":true");
            } else if let Some(i) = win.last_index_of(l.pane) {
                let _ = write!(out, ",\"l\":{i}");
            }
            let Some(i) = win.index_of(l.pane) else {
                return false;
            };
            let _ = write!(out, ",\"i\":{i}");
            if c.is_floating() {
                let _ = write!(out, ",\"z\":{}", zindex(root, win, l.pane));
            }
            let _ = write!(out, ",\"I\":\"%{}\"", l.pane.0);
        }
    }
    out.push('}');
    true
}

/// The v2 string for `root`, as `#{window_layout}` prints it; `None` where
/// tmux's `layout_dump` returns NULL (a pane missing from the window).
#[must_use]
pub fn dump_v2(root: &Cell<PaneId>, win: &WindowPanes) -> Option<String> {
    let mut body = String::new();
    append_v2(root, root, win, &mut body).then(|| format!("{{\"V\":2,\"L\":{body}}}"))
}

/// `layout_custom_copy_layout`: the tiled part only; a node left with one
/// child is replaced by it.
fn tiled_copy(c: &Cell<PaneId>) -> Option<Cell<PaneId>> {
    match c {
        Cell::Floating(_) => None,
        Cell::Pane(_) => Some(c.clone()),
        Cell::Node(n) => {
            let mut kids: Vec<_> = n.children.iter().filter_map(tiled_copy).collect();
            match kids.len() {
                0 => None,
                1 => kids.pop(),
                _ => Some(Cell::Node(super::cell::Node {
                    dir: n.dir,
                    g: n.g,
                    children: kids,
                })),
            }
        }
    }
}

fn append_v1(c: &Cell<PaneId>, out: &mut String) {
    let g = c.g();
    match c {
        Cell::Pane(l) | Cell::Floating(l) => {
            let _ = write!(out, "{}x{},{},{},{}", g.sx, g.sy, g.xoff, g.yoff, l.pane.0);
        }
        Cell::Node(n) => {
            let _ = write!(out, "{}x{},{},{}", g.sx, g.sy, g.xoff, g.yoff);
            let (open, close) = if n.dir == Dir::LeftRight {
                ('{', '}')
            } else {
                ('[', ']')
            };
            out.push(open);
            for (i, ch) in n.children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                append_v1(ch, out);
            }
            out.push(close);
        }
    }
}

/// The v1 string a control client without `new-layouts` sees
/// (`LAYOUT_CUSTOM_OLD_FORMAT`): the tiled part, `0000,` when there is none.
#[must_use]
pub fn dump_v1(root: &Cell<PaneId>) -> String {
    let Some(mut compat) = tiled_copy(root) else {
        return "0000,".to_owned();
    };
    if compat.is_tiled() {
        let g = compat.g_mut();
        g.xoff = 0;
        g.yoff = 0;
    }
    let mut body = String::new();
    append_v1(&compat, &mut body);
    format!("{:04x},{body}", checksum(&body))
}
