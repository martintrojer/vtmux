//! A tree mode's screen (`mode_tree_draw`, mode-tree.c:821-1044): a line
//! per item in view, the prefix (`MODE_TREE_PREFIX_FORMAT`), the name with
//! its tag and the item's text; the selected line in
//! `tree-mode-selection-style`, tagged ones cyan. With the preview on,
//! the box under the list with its title (the preview inside it is not
//! drawn yet); the help screen over everything.

use vtmux_core::colour::Colour;
use vtmux_core::options::TreeId;
use vtmux_core::style::{Attrs, CellStyle};
use vtmux_core::styled;
use vtmux_vt::Snapshot;

use super::Caps;
use super::window::Source;
use crate::mode::tree::{ModeTree, Preview};
use crate::target::Found;

/// `MODE_TREE_PREFIX_STYLE` and `MODE_TREE_PREFIX_FORMAT`
/// (mode-tree.c:39-55).
const PREFIX_STYLE: &str = "#[fg=themelightgrey]#[bg=default]#[noacs]";

fn prefix_format() -> String {
    let s = PREFIX_STYLE;
    format!(
        "{s}#{{p/#{{mode_tree_key_width}}:#{{?#{{!=:#{{mode_tree_key}},}},(#{{mode_tree_key}}),}}}}\
#{{R:#{{?mode_tree_parent_last,    ,#[acs]x{s}   }},#{{mode_tree_repeat}}}}\
#{{?mode_tree_branch,#[acs]#{{?mode_tree_last,mq,tq}}+{s} ,}}\
#{{?mode_tree_has_children,#{{?mode_tree_expanded,#[fg=themered]-{s} ,#[fg=themegreen]+{s} }},#{{?mode_tree_flat,,  }}}}"
    )
}

/// `mode_tree_help_start`, `mode_tree_help_end` (mode-tree.c:177-224)
/// and `window_tree_help_lines` (window-tree.c:1067-1096), `%1` the item.
const HELP_START: &[(&str, &str)] = &[
    ("      Up, k", "Move cursor up"),
    ("    Down, j", "Move cursor down"),
    ("          g", "Go to top"),
    ("          G", "Go to bottom"),
    (" PPage, C-b", "Page up"),
    (" NPage, C-f", "Page down"),
    ("    Left, h", "Collapse %1"),
    ("   Right, l", "Expand %1"),
    ("        M--", "Collapse all %1s"),
    ("        M-+", "Expand all %1s"),
    ("          t", "Toggle %1 tag"),
    ("          T", "Untag all %1s"),
    ("        C-t", "Tag all %1s"),
    ("        C-s", "Search forward"),
    ("          n", "Repeat search forward"),
    ("          N", "Repeat search backward"),
    ("          f", "Filter %1s"),
    ("          O", "Change sort order"),
    ("          r", "Reverse sort order"),
    ("          v", "Toggle preview"),
];
const WINDOW_TREE_HELP: &[(&str, &str)] = &[
    ("      Enter", "Choose selected item"),
    ("       S-Up", "Swap current and previous window"),
    ("     S-Down", "Swap current and next window"),
    ("          x", "Kill selected item"),
    ("          X", "Kill tagged items"),
    ("          <", "Scroll previews left"),
    ("          >", "Scroll previews right"),
    ("          m", "Set the marked pane"),
    ("          M", "Clear the marked pane"),
    ("          i", "Toggle session, window and pane information"),
    ("          :", "Run a command for each tagged item"),
    ("          f", "Enter a format"),
    ("          H", "Jump to the starting pane"),
];
const HELP_END: &[(&str, &str)] = &[("  q, Escape", "Exit mode")];
/// `window_tree_help`'s width (over `MODE_TREE_HELP_DEFAULT_WIDTH`, 39).
const HELP_WIDTH: usize = 51;

/// A cell `format_draw` did not write keeps what was under it.
const KEEP: &str = "\u{0}";

/// The screen being drawn: styled cells, turned into terminal cells at
/// the end.
struct Canvas {
    cols: usize,
    rows: usize,
    cells: Vec<styled::Cell>,
}

fn blank(bg: Colour) -> styled::Cell {
    styled::Cell {
        text: " ".into(),
        width: 1,
        style: CellStyle {
            bg,
            ..CellStyle::default()
        },
    }
}

impl Canvas {
    fn put(&mut self, x: usize, y: usize, c: styled::Cell) {
        if x < self.cols && y < self.rows {
            self.cells[y * self.cols + x] = c;
        }
    }

    /// `screen_write_clearendofline` (or the whole line) with `bg`.
    fn clear_from(&mut self, x: usize, y: usize, bg: Colour) {
        for x in x..self.cols {
            self.put(x, y, blank(bg));
        }
    }

    /// `format_draw` of `text` at `x`, `y` in at most `width` cells over
    /// `base`; `default_colours` is its last argument.
    fn draw(
        &mut self,
        x: usize,
        y: usize,
        width: usize,
        base: &CellStyle,
        text: &str,
        default_colours: bool,
    ) {
        let width = width.min(self.cols.saturating_sub(x));
        if width == 0 {
            return;
        }
        let under = vec![
            styled::Cell {
                text: KEEP.into(),
                width: 1,
                style: base.clone(),
            };
            width
        ];
        let line = styled::draw_onto(base, under, text, false, default_colours);
        for (i, c) in line.cells.into_iter().enumerate() {
            if c.text != KEEP {
                self.put(x + i, y, c);
            }
        }
    }

    /// `screen_write_box` with `BOX_LINES_DEFAULT`.
    fn draw_box(&mut self, x: usize, y: usize, w: usize, h: usize, style: &CellStyle) {
        if w < 2 || h < 2 {
            return;
        }
        let acs = |ch: char| styled::Cell {
            text: ch.to_string(),
            width: 1,
            style: CellStyle {
                attrs: style.attrs.union(Attrs::ACS),
                ..style.clone()
            },
        };
        self.put(x, y, acs('l'));
        self.put(x + w - 1, y, acs('k'));
        self.put(x, y + h - 1, acs('m'));
        self.put(x + w - 1, y + h - 1, acs('j'));
        for i in 1..w - 1 {
            self.put(x + i, y, acs('q'));
            self.put(x + i, y + h - 1, acs('q'));
        }
        for i in 1..h - 1 {
            self.put(x, y + i, acs('x'));
            self.put(x + w - 1, y + i, acs('x'));
        }
    }

    /// `screen_write_puts`: plain text in `style`.
    fn puts(&mut self, x: &mut usize, y: usize, style: &CellStyle, text: &str) {
        for ch in text.chars() {
            self.put(
                *x,
                y,
                styled::Cell {
                    text: ch.to_string(),
                    width: 1,
                    style: style.clone(),
                },
            );
            *x += 1;
        }
    }
}

impl Source<'_> {
    /// `mode_tree_draw` for a window tree on pane `pane`.
    #[must_use]
    pub fn tree_screen(
        &self,
        pane: &crate::model::Pane,
        tree: &ModeTree<crate::mode::window_tree::Tag, crate::mode::window_tree::Item>,
        caps: Caps,
    ) -> Snapshot {
        let (cols, rows) = (usize::from(pane.term.cols()), usize::from(pane.term.rows()));
        let mut c = Canvas {
            cols,
            rows,
            cells: vec![blank(Colour::Default); cols * rows],
        };
        let wtree = TreeId::Window(pane.window);
        let found = Found::from_pane(self.model, pane.id);
        let ctx = crate::format_ctx::Ctx::new(self.model, self.info, self.env, found);
        let sel = self.style(wtree, "tree-mode-selection-style", &ctx);
        let border = self.style(wtree, "tree-mode-border-style", &ctx);
        let (w, h) = (tree.width.min(cols), tree.height.min(rows));
        let lines = tree.lines();
        if !lines.is_empty() && w != 0 && h != 0 {
            self.tree_lines(&mut c, tree, &found, &sel, w, h);
            if tree.preview != Preview::Off {
                tree_preview_box(&mut c, tree, &border, w, h);
            }
        }
        if tree.help {
            self.tree_help(&mut c, &found, &border);
        }
        let theme = self.theme(caps);
        let cells = c.cells.iter().map(|x| theme.cell(x)).collect();
        Snapshot {
            cols: pane.term.cols(),
            rows: pane.term.rows(),
            cells,
            wrapped: Vec::new(),
            cursor: u16::try_from(tree.current.saturating_sub(tree.offset))
                .ok()
                .map(|y| (0, y)),
            cursor_visible: false,
        }
    }

    fn tree_lines(
        &self,
        c: &mut Canvas,
        tree: &ModeTree<crate::mode::window_tree::Tag, crate::mode::window_tree::Item>,
        found: &Found,
        sel: &CellStyle,
        w: usize,
        h: usize,
    ) {
        let lines = tree.lines();
        let keylen = lines
            .iter()
            .filter_map(|l| tree.item(l.item).key)
            .map(|k| k.name(false).len() + 3)
            .max()
            .unwrap_or(0);
        let mut alignlen = vec![0usize; tree.maxdepth() + 1];
        for l in lines {
            let item = tree.item(l.item);
            if item.align != 0 {
                alignlen[l.depth] = alignlen[l.depth].max(item.name.len());
            }
        }
        let prefix_format = prefix_format();
        let cyan = Colour::Theme(8);
        for (i, line) in lines.iter().enumerate().skip(tree.offset).take(h) {
            let y = i - tree.offset;
            let item = tree.item(line.item);
            let mut ctx = crate::format_ctx::Ctx::new(self.model, self.info, self.env, *found);
            let flag = |b: bool| u8::from(b).to_string();
            let mut add = |k: &'static str, v: String| ctx.extra.push((k.into(), v));
            add(
                "mode_tree_key",
                item.key.map(|k| k.name(false)).unwrap_or_default(),
            );
            add("mode_tree_key_width", keylen.to_string());
            add("mode_tree_selected", flag(i == tree.current));
            add("mode_tree_repeat", line.depth.saturating_sub(1).to_string());
            add("mode_tree_branch", flag(line.depth != 0));
            add(
                "mode_tree_parent_last",
                flag(line.depth != 0 && tree.parent_last(line.item)),
            );
            add("mode_tree_has_children", flag(tree.has_children(line.item)));
            add("mode_tree_last", flag(line.last));
            add("mode_tree_expanded", flag(item.expanded));
            add("mode_tree_flat", flag(line.flat));
            let prefix = vtmux_core::format::expand_time(&prefix_format, &ctx);
            let prefix_width = styled::format_width(&prefix).min(w);
            let tag = if item.tagged { "*" } else { "" };
            let separator = if item.text.is_some() {
                "#[fg=themelightgrey]: #[default]"
            } else {
                ""
            };
            // `%*s` with `align * alignlen`: right-aligned for 1,
            // left-aligned (padded after) for -1.
            let pad = alignlen[line.depth];
            let name = match item.align {
                1 => format!("{:>pad$}", item.name),
                -1 => format!("{:<pad$}", item.name),
                _ => item.name.clone(),
            };
            let text = format!("{name}{tag}{separator}");
            let text_width = styled::format_width(&text);
            let left = w.saturating_sub(prefix_width);
            let width = prefix_width + text_width.min(left);
            let selected = i == tree.current;
            let mut gc = if selected {
                sel.clone()
            } else {
                CellStyle::default()
            };
            if item.tagged {
                gc.fg = cyan;
            }
            if selected {
                c.clear_from(0, y, sel.bg);
            } else {
                c.clear_from(0, y, Colour::Default);
            }
            // The selected line's prefix is in its style; others' in the
            // default cell, not cyan when tagged.
            let prefix_base = if selected {
                gc.clone()
            } else {
                CellStyle::default()
            };
            c.draw(0, y, prefix_width, &prefix_base, &prefix, selected);
            if left != 0 {
                c.draw(prefix_width, y, left, &gc, &text, selected);
                if let Some(t) = &item.text
                    && width < w
                {
                    c.draw(width, y, w - width, &gc, t, selected);
                }
            }
        }
    }

    /// `mode_tree_draw_help`.
    fn tree_help(&self, c: &mut Canvas, found: &Found, border: &CellStyle) {
        let lines: Vec<&(&str, &str)> = HELP_START
            .iter()
            .chain(WINDOW_TREE_HELP)
            .chain(HELP_END)
            .collect();
        let w = HELP_WIDTH;
        let (box_w, box_h) = (w + 2, lines.len() + 2);
        if c.cols < box_w || c.rows < box_h {
            return;
        }
        let (x, y) = ((c.cols - box_w) / 2, (c.rows - box_h) / 2);
        c.draw_box(x, y, box_w, box_h, border);
        let ctx = crate::format_ctx::Ctx::new(self.model, self.info, self.env, *found);
        for (n, (keys, what)) in lines.iter().enumerate() {
            let line = format!(
                "#[fg=themelightgrey]{keys} #[#{{E:tree-mode-border-style}},acs]x#[default] {what}"
            );
            let line = vtmux_core::cmd::template::replace(&line, "item", 1);
            let expanded = vtmux_core::format::expand_time(&line, &ctx);
            for i in 0..w {
                c.put(x + 1 + i, y + 1 + n, blank(Colour::Default));
            }
            c.draw(x + 1, y + 1 + n, w, &CellStyle::default(), &expanded, false);
        }
    }
}

/// The preview's box under the list and its title (mode-tree.c:960-1003).
fn tree_preview_box<K, D>(
    canvas: &mut Canvas,
    tree: &ModeTree<K, D>,
    border: &CellStyle,
    w: usize,
    h: usize,
) where
    K: Copy + PartialEq,
{
    let sy = canvas.rows;
    if sy <= 4 || h < 2 || sy - h <= 4 || w <= 4 {
        return;
    }
    let Some(line) = tree.lines().get(tree.current) else {
        return;
    };
    let mut id = line.item;
    if tree.item(id).draw_as_parent
        && let Some(p) = tree.parent(id)
    {
        id = p;
    }
    let name = &tree.item(id).name;
    canvas.draw_box(0, h, w, sy - h, border);
    let text = if tree.sort.seq.is_empty() {
        format!(" {name}")
    } else {
        let order = tree
            .sort
            .order
            .map_or("", crate::mode::tree::SortOrder::name);
        let reversed = if tree.sort.reversed { ", reversed" } else { "" };
        let view = tree
            .view_name
            .map(|v| format!(" (view: {v})"))
            .unwrap_or_default();
        format!(" {name} (sort: {order}{reversed}){view}")
    };
    let len = text.chars().count();
    if w - 2 >= len {
        let mut at = 1;
        canvas.puts(&mut at, h, border, &text);
        let n = if tree.no_matches {
            "no matches".len()
        } else {
            "active".len()
        };
        if tree.filter.is_some() && w - 2 >= len + 10 + n + 2 {
            let state = if tree.no_matches {
                "no matches"
            } else {
                "active"
            };
            canvas.puts(&mut at, h, border, &format!(" (filter: {state}) "));
        } else {
            canvas.puts(&mut at, h, border, " ");
        }
    }
}
