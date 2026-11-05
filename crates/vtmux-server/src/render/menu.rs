//! A window menu as `menu_update` draws it into its screen
//! (screen-write.c `screen_write_box` and `screen_write_menu`), copied over
//! the window's scene (screen-redraw.c `redraw_mark_menu`,
//! `redraw_draw_menu_span`). Drawn over the panes and under the status
//! line, which it never covers.

use vtmux_core::colour::Colour;
use vtmux_core::style::{Attrs, CellStyle};
use vtmux_core::styled;
use vtmux_vt::{Cell, Snapshot};

use super::compose::{Clear, Theme, draw_line_by};
use super::popup::BoxLines;
use crate::menu::{Entry, Menu, MenuStyles};

/// The glyphs `screen_write_box_border_set` puts in a cell of each type:
/// up-down, left-right, the four corners (right-down, left-down, right-up,
/// left-up) and the separators' ends (up-right-down, up-left-down).
/// `true` keeps `GRID_ATTR_CHARSET`: the single lines are the ACS
/// characters, and with no lines the border cell's own space stays in that
/// charset, so a run of them is never cleared.
fn glyphs(lines: BoxLines) -> ([&'static str; 8], bool) {
    match lines {
        BoxLines::Single => (["x", "q", "l", "k", "m", "j", "t", "u"], true),
        BoxLines::Double => (["║", "═", "╔", "╗", "╚", "╝", "╠", "╣"], false),
        BoxLines::Heavy => (["┃", "━", "┏", "┓", "┗", "┛", "┣", "┫"], false),
        BoxLines::Rounded => (["│", "─", "╭", "╮", "╰", "╯", "├", "┤"], false),
        BoxLines::Simple => (["|", "-", "+", "+", "+", "+", "+", "+"], false),
        BoxLines::Padded => ([" "; 8], false),
        BoxLines::None => ([" "; 8], true),
    }
}

/// The menu's screen (`md->s`): `width + 4` by `count + 2` cells.
#[must_use]
pub fn rows(menu: &Menu, styles: &MenuStyles) -> Vec<Vec<styled::Cell>> {
    let w = menu.items.width + 4;
    let (g, charset) = glyphs(menu.lines);
    let border = |i: usize| styled::Cell {
        text: g[i].into(),
        width: 1,
        style: CellStyle {
            attrs: if charset {
                styles.border.attrs.union(Attrs::ACS)
            } else {
                styles.border.attrs
            },
            ..styles.border.clone()
        },
    };
    let line = |l: usize, mid: usize, r: usize| {
        let mut row = vec![border(mid); w];
        row[0] = border(l);
        row[w - 1] = border(r);
        row
    };
    let mut out = Vec::with_capacity(menu.items.entries.len() + 2);
    let mut top = line(2, 1, 3);
    if !menu.items.title.is_empty() && w > 4 {
        // format_draw at x 2, `nx - 4` wide, in the border cell without
        // the charset; the line shows where the title does not reach.
        let base = CellStyle {
            attrs: styles.border.attrs,
            ..styles.border.clone()
        };
        let under = top[2..w - 2].to_vec();
        let title = styled::draw_onto(&base, under, &menu.items.title, false, false);
        top.splice(2..w - 2, title.cells);
    }
    out.push(top);
    for (i, e) in menu.items.entries.iter().enumerate() {
        let Entry::Item { name, .. } = e else {
            out.push(line(6, 1, 7));
            continue;
        };
        let disabled = name.starts_with('-');
        let gc = if menu.choice == Some(i) && !disabled {
            &styles.selected
        } else {
            &styles.normal
        };
        let mut row = line(0, 0, 0);
        for c in &mut row[1..w - 1] {
            *c = styled::Cell {
                text: " ".into(),
                width: 1,
                style: gc.clone(),
            };
        }
        let (base, text) = if disabled {
            let dim = CellStyle {
                attrs: gc.attrs.union(Attrs::DIM),
                ..gc.clone()
            };
            (dim, &name[1..])
        } else {
            (gc.clone(), name.as_str())
        };
        let under = row[2..w - 2].to_vec();
        let drawn = styled::draw_onto(&base, under, text, false, false);
        row.splice(2..w - 2, drawn.cells);
        out.push(row);
    }
    out.push(line(4, 1, 5));
    out
}

/// Draw `menu` over `window`, the scene of a window that starts `offset`
/// cells in (the focus ring, D23) and is `wsx` x `wsy`: each row as
/// `tty_draw_line` writes it (no defaults, so runs of plain spaces are
/// cleared), clipped as `redraw_window_to_scene` clips.
pub fn draw(
    menu: &Menu,
    styles: &MenuStyles,
    theme: &Theme,
    clear: Clear,
    window: &mut Snapshot,
    offset: u16,
    (wsx, wsy): (u32, u32),
) {
    let cols = usize::from(window.cols);
    let off = u32::from(offset);
    for (py, row) in (0u32..).zip(rows(menu, styles)) {
        let wy = menu.py + py;
        if wy > wsy {
            break;
        }
        let Some(y) = usize::try_from(wy + off)
            .ok()
            .filter(|y| *y < usize::from(window.rows))
        else {
            break;
        };
        // The visible part of the row: window columns up to the window's
        // width (inclusive, as tmux compares), inside the scene.
        let x0 = usize::try_from(menu.px + off).unwrap_or(usize::MAX);
        let last = usize::try_from(wsx.saturating_sub(menu.px)).unwrap_or(usize::MAX);
        let n = row.len().min(last + 1).min(cols.saturating_sub(x0));
        if n == 0 {
            continue;
        }
        let mut cells: Vec<Cell> = row[..n].iter().map(|c| theme.cell(c)).collect();
        let bgs: Vec<Colour> = row[..n].iter().map(|c| c.style.bg).collect();
        draw_line_by(&mut cells, &bgs, Colour::Default, clear, false);
        let at = y * cols + x0;
        window.cells[at..at + n].clone_from_slice(&cells);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::Items;
    use crate::target::Found;
    use vtmux_core::keys::Key;

    fn text(rows: &[Vec<styled::Cell>], theme: &Theme) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|c| String::from(theme.cell(c).text)).collect())
            .collect()
    }

    fn menu(lines: BoxLines) -> Menu {
        let mut items = Items::new("Menu".into());
        items.item("Alpha item", Key::from_name("a"), String::new(), 40);
        items.item("Beta item", Key::from_name("b"), String::new(), 40);
        items.separator();
        items.item("-Gamma item", Key::from_name("g"), String::new(), 40);
        Menu {
            items,
            px: 0,
            py: 0,
            choice: Some(1),
            lines,
            style: None,
            selected_style: None,
            border_style: None,
            stay_open: false,
            no_mouse: true,
            found: Found::default(),
            key: None,
            mouse: None,
            tree: None,
        }
    }

    /// regress screen-redraw-results/menu-basic.result, without colours.
    #[test]
    fn single_lines_with_separator() {
        let theme = Theme::default();
        let rows = rows(&menu(BoxLines::Single), &MenuStyles::default());
        assert_eq!(
            text(&rows, &theme),
            [
                "┌─Menu───────────┐",
                "│ Alpha item (a) │",
                "│ Beta item  (b) │",
                "├────────────────┤",
                "│ Gamma item     │",
                "└────────────────┘",
            ]
        );
    }

    #[test]
    fn separator_ends_follow_the_lines() {
        let theme = Theme::default();
        for (lines, sep) in [
            (BoxLines::Double, "╠═"),
            (BoxLines::Heavy, "┣━"),
            (BoxLines::Rounded, "├─"),
            (BoxLines::Simple, "+-"),
            (BoxLines::Padded, "  "),
            (BoxLines::None, "  "),
        ] {
            let rows = rows(&menu(lines), &MenuStyles::default());
            assert!(text(&rows, &theme)[3].starts_with(sep), "{lines:?}");
        }
    }

    #[test]
    fn disabled_items_are_dim_and_never_selected() {
        let mut m = menu(BoxLines::Single);
        m.choice = Some(3);
        let styles = MenuStyles {
            selected: CellStyle {
                bg: Colour::Ansi(3),
                ..CellStyle::default()
            },
            ..MenuStyles::default()
        };
        let rows = rows(&m, &styles);
        let g = &rows[4][2];
        assert_eq!(g.text, "G");
        assert!(g.style.attrs.contains(Attrs::DIM));
        assert_eq!(g.style.bg, Colour::Default);
    }
}
