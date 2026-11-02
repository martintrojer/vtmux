//! A tree mode's screen (`mode_tree_draw`, mode-tree.c:821-1044): a line
//! per item in view, the prefix (`MODE_TREE_PREFIX_FORMAT`), the name with
//! its tag and the item's text; the selected line in
//! `tree-mode-selection-style`, tagged ones cyan. With the preview on,
//! the box under the list with its title and inside it the current item's
//! preview (`window_tree_draw`, window-tree.c:885-915) or information
//! (`window_tree_draw_info`); the help screen over everything.

use vtmux_core::colour::Colour;
use vtmux_core::options::TreeId;
use vtmux_core::style::{Attrs, CellStyle};
use vtmux_core::styled;
use vtmux_vt::Snapshot;

use super::Caps;
use super::window::Source;
use crate::mode::tree::{ModeTree, Preview};
use crate::mode::window_tree::{Item, Strip, WindowTree};
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

/// `WINDOW_TREE_FLAG` (window-tree.c:146-147).
fn info_flag(label: &str, cond: &str) -> String {
    format!("#{{?{cond},#[fg=themegreen],#[fg=themelightgrey]}}{label}#[default]")
}

/// An information line (window-tree.c:149-216): the name, the border's
/// line, the value.
fn info_line(name: &str, value: &str) -> String {
    format!("#[fg=themelightgrey]{name:<14}#[#{{E:tree-mode-border-style}},acs]x#[default] {value}")
}

fn info_flags(flags: &[(&str, &str)]) -> String {
    flags
        .iter()
        .map(|(l, c)| info_flag(l, c))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `window_tree_pane_info_lines`.
fn pane_info_lines() -> Vec<String> {
    vec![
        info_line(
            "Pane",
            "#{pane_index} #[fg=themelightgrey](#{pane_id})#[default]",
        ),
        info_line("Title", "#{pane_title}"),
        info_line(
            "Command",
            "#{pane_current_command} #[fg=themelightgrey](PID #{pane_pid})#[default]",
        ),
        info_line("Path", "#{pane_current_path}"),
        info_line("TTY", "#{pane_tty}"),
        info_line(
            "Position",
            "#{pane_x},#{pane_y} #{pane_width}x#{pane_height}",
        ),
        info_line("Mode", "#{?pane_in_mode,#{pane_mode},none}"),
        info_line(
            "Flags",
            &info_flags(&[
                ("active", "pane_active"),
                ("zoomed", "window_zoomed_flag"),
                ("marked", "pane_marked"),
                ("sync", "pane_synchronized"),
                ("dead", "pane_dead"),
                ("piped", "pane_pipe"),
            ]),
        ),
    ]
}

/// `window_tree_window_info_lines`.
fn window_info_lines() -> Vec<String> {
    vec![
        info_line(
            "Window",
            "#{window_index}: #{window_name} #[fg=themelightgrey](#{window_id})#[default]",
        ),
        info_line("Size", "#{window_width}x#{window_height}"),
        info_line("Panes", "#{window_panes}"),
        info_line(
            "Activity Time",
            "#{t:window_activity} #[fg=themelightgrey](#{t/r:window_activity})#[default]",
        ),
        info_line("Sessions", "#{s/,/ /:window_linked_sessions_list}"),
        info_line(
            "Flags",
            &info_flags(&[
                ("active", "window_active"),
                ("last", "window_last_flag"),
                ("bell", "window_bell_flag"),
                ("activity", "window_activity_flag"),
                ("silence", "window_silence_flag"),
                ("zoomed", "window_zoomed_flag"),
                ("marked", "window_marked_flag"),
            ]),
        ),
    ]
}

/// `window_tree_session_info_lines`.
fn session_info_lines() -> Vec<String> {
    vec![
        info_line(
            "Session",
            "#{session_name} #[fg=themelightgrey](#{session_id})#[default]",
        ),
        info_line(
            "Created Time",
            "#{t:session_created} #[fg=themelightgrey](#{t/r:session_created})#[default]",
        ),
        info_line(
            "Activity Time",
            "#{t:session_activity} #[fg=themelightgrey](#{t/r:session_activity})#[default]",
        ),
        info_line(
            "Attached Time",
            "#{?#{t:session_last_attached},#{t:session_last_attached} \
#[fg=themelightgrey](#{t/r:session_last_attached})#[default],never}",
        ),
        info_line("Clients", "#{s/,/ /:session_attached_list}"),
        info_line("Windows", "#{session_windows}"),
        info_line("Path", "#{session_path}"),
        info_line(
            "Group",
            "#{?session_grouped,#{session_group} (#{session_group_size}),none}",
        ),
        info_line(
            "Flags",
            &info_flags(&[
                ("attached", "session_attached"),
                ("grouped", "session_grouped"),
                ("marked", "session_marked"),
                ("bell", "session_bell_flag"),
                ("activity", "session_activity_flag"),
                ("silence", "session_silence_flag"),
            ]),
        ),
    ]
}

/// A cell `format_draw` did not write keeps what was under it.
const KEEP: &str = "\u{0}";

/// The screen being drawn: styled cells, turned into terminal cells at
/// the end.
struct Canvas {
    cols: usize,
    rows: usize,
    cells: Vec<styled::Cell>,
    /// Cells copied from a pane's screen, already terminal cells; a cell
    /// drawn later replaces one.
    raw: Vec<Option<vtmux_vt::Cell>>,
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
            self.raw[y * self.cols + x] = None;
        }
    }

    fn put_raw(&mut self, x: usize, y: usize, c: vtmux_vt::Cell) {
        if x < self.cols && y < self.rows {
            self.raw[y * self.cols + x] = Some(c);
        }
    }

    /// `screen_write_vline` with no ends: `x` in `style`'s line drawing
    /// from row `y`, `n` rows.
    fn vline(&mut self, x: usize, y: usize, n: usize, style: &CellStyle) {
        let cell = styled::Cell {
            text: "x".into(),
            width: 1,
            style: CellStyle {
                attrs: style.attrs.union(Attrs::ACS),
                ..style.clone()
            },
        };
        for i in 0..n.max(1) {
            self.put(x, y + i, cell.clone());
        }
    }

    /// `screen_write_preview` (screen-write.c:939-990): `nx` x `ny` of
    /// `src` at `x`, `y`: the part around its cursor if shown (the cursor
    /// cell reversed), else the top left.
    fn preview(&mut self, x: usize, y: usize, src: &vtmux_vt::Snapshot, nx: usize, ny: usize) {
        if nx == 0 || ny == 0 {
            return;
        }
        let (sx, sy) = (usize::from(src.cols), usize::from(src.rows));
        let cursor = src
            .cursor
            .filter(|_| src.cursor_visible)
            .map(|(cx, cy)| (usize::from(cx), usize::from(cy)));
        let around = |c: usize, n: usize, size: usize| {
            let mut p = c.saturating_sub(n / 3);
            if c < n / 3 {
                p = 0;
            }
            if p + n > size {
                p = size.saturating_sub(n);
            }
            p
        };
        let (px, py) = cursor.map_or((0, 0), |(cx, cy)| (around(cx, nx, sx), around(cy, ny, sy)));
        let cell = |x: usize, y: usize| src.cell(u16::try_from(x).ok()?, u16::try_from(y).ok()?);
        for yy in py..(py + ny).min(sy) {
            for xx in px..(px + nx).min(sx) {
                let Some(c) = cell(xx, yy) else {
                    break;
                };
                if xx + usize::from(c.width) > px + nx {
                    break;
                }
                self.put_raw(x + xx - px, y + yy - py, c.clone());
            }
        }
        if let Some((cx, cy)) = cursor
            && let Some(c) = cell(cx, cy)
        {
            let mut c = c.clone();
            c.style.inverse = true;
            if c.text.is_empty() {
                c.text = " ".into();
            }
            self.put_raw(x + cx - px, y + cy - py, c);
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
        mode: &WindowTree,
        caps: Caps,
    ) -> Snapshot {
        let tree = &mode.tree;
        let (cols, rows) = (usize::from(pane.term.cols()), usize::from(pane.term.rows()));
        let mut c = Canvas {
            cols,
            rows,
            cells: vec![blank(Colour::Default); cols * rows],
            raw: vec![None; cols * rows],
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
            if let Some((sx, sy)) = tree.preview_size()
                && let Some(item) = tree.current_item()
            {
                self.tree_preview(&mut c, mode, item.data, (2, h + 1), (sx, sy));
            }
        }
        if tree.help {
            self.tree_help(&mut c, &found, &border);
        }
        let theme = self.theme(caps);
        let cells = c
            .cells
            .iter()
            .zip(c.raw)
            .map(|(x, raw)| raw.unwrap_or_else(|| theme.cell(x)))
            .collect();
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

    /// `window_tree_draw`: the preview of `item` in the `size` area at
    /// `at`.
    fn tree_preview(
        &self,
        c: &mut Canvas,
        mode: &WindowTree,
        item: Item,
        at: (usize, usize),
        size: (usize, usize),
    ) {
        let model = self.model;
        // window_tree_pull_item: the item's session, winlink and pane.
        let found = match item {
            Item::Session(s) => Found::from_session(model, s),
            Item::Window(s, i) => Found::from_winlink(model, s, i),
            Item::Pane(s, i, p) => {
                let link = Found::from_winlink(model, s, i);
                if model
                    .panes
                    .get(&p)
                    .is_none_or(|x| Some(x.window) != link.window)
                {
                    return;
                }
                Found {
                    pane: Some(p),
                    ..link
                }
            }
        };
        let Some(p) = found.pane.filter(|_| found.session.is_some()) else {
            return;
        };
        let mode_tree = model
            .panes
            .get(&mode.pane())
            .map_or(TreeId::GlobalWindow, |x| TreeId::Window(x.window));
        if mode.info() {
            self.tree_info(c, item, found, mode_tree, at, size);
            return;
        }
        if let Item::Pane(..) = item {
            if mode.hidden_pane() != Some(p)
                && let Some(pane) = model.panes.get(&p)
            {
                c.preview(at.0, at.1, &vtmux_vt::snapshot(&pane.term), size.0, size.1);
            }
        } else {
            let mut offset = mode.offset(model);
            if let Some(strip) = mode.strip(model, item, size.0, &mut offset) {
                self.tree_strip(c, &strip, mode_tree, at, size);
            }
        }
    }

    /// `window_tree_draw_session` and `window_tree_draw_window`: the
    /// strip's items side by side, each its (active) pane's screen with
    /// `tree-mode-preview-format` in a box in its middle, lines between
    /// them, and `<` or `>` where more are off that side.
    fn tree_strip(
        &self,
        c: &mut Canvas,
        strip: &Strip,
        mode_tree: TreeId,
        (cx, cy): (usize, usize),
        (sx, sy): (usize, usize),
    ) {
        let model = self.model;
        // window_tree_border_cell with no format: the mode's window.
        let plain = crate::format_ctx::Ctx::new(model, self.info, self.env, Found::default());
        let border = self.style(mode_tree, "tree-mode-border-style", &plain);
        if strip.left {
            c.vline(cx + 2, cy, sy, &border);
            c.puts(&mut { cx }, cy + sy / 2, &border, "<");
        }
        if strip.right {
            c.vline(cx + sx - 3, cy, sy, &border);
            c.puts(&mut { cx + sx - 1 }, cy + sy / 2, &border, ">");
        }
        for (i, found) in strip.items[strip.start..strip.end].iter().enumerate() {
            let Some(p) = found.pane else { continue };
            let last = strip.start + i == strip.end - 1;
            let ty = if strip.panes {
                "pane_format"
            } else {
                "window_format"
            };
            let mut ctx = crate::format_ctx::Ctx::new(model, self.info, self.env, *found);
            ctx.clients = self.clients.clone();
            for name in ["session_format", "window_format", "pane_format"] {
                ctx.extra
                    .push((name.into(), if name == ty { "1" } else { "0" }.into()));
            }
            // The options of the window (session preview) or the pane.
            let tree = if strip.panes {
                TreeId::Pane(p)
            } else {
                found.window.map_or(TreeId::GlobalWindow, TreeId::Window)
            };
            let wtree = found.window.map_or(TreeId::GlobalWindow, TreeId::Window);
            let gc = self.style(tree, "tree-mode-border-style", &ctx);
            let mut label_gc = self.style(wtree, "tree-mode-preview-style", &ctx);
            label_gc.bg = gc.bg;
            let offset = usize::from(strip.left) * 3 + i * strip.each;
            let width = if last {
                strip.each + strip.remaining
            } else {
                strip.each - 1
            };
            if let Some(pane) = model.panes.get(&p) {
                c.preview(cx + offset, cy, &vtmux_vt::snapshot(&pane.term), width, sy);
            }
            let format = self.string(tree, "tree-mode-preview-format");
            if !format.is_empty() {
                let label = vtmux_core::format::expand_time(&format, &ctx);
                if !label.is_empty() {
                    tree_label(c, (cx + offset, cy), (width, sy), &gc, &label_gc, &label);
                }
            }
            if !last {
                c.vline(cx + offset + width, cy, sy, &gc);
            }
        }
    }

    /// `window_tree_draw_info`: the pane's, window's and session's
    /// information lines as far as the item goes, sections apart by a
    /// line, the column line down the rest.
    fn tree_info(
        &self,
        c: &mut Canvas,
        item: Item,
        found: Found,
        mode_tree: TreeId,
        (cx, cy): (usize, usize),
        (sx, sy): (usize, usize),
    ) {
        let mut sections = Vec::new();
        if matches!(item, Item::Pane(..)) {
            sections.push(pane_info_lines());
        }
        if matches!(item, Item::Pane(..) | Item::Window(..)) {
            sections.push(window_info_lines());
        }
        sections.push(session_info_lines());
        let mut ctx = crate::format_ctx::Ctx::new(self.model, self.info, self.env, found);
        ctx.clients = self.clients.clone();
        // window_tree_border_cell with no format: the mode's window.
        let plain = crate::format_ctx::Ctx::new(self.model, self.info, self.env, Found::default());
        let border = self.style(mode_tree, "tree-mode-border-style", &plain);
        let mut i = 0;
        'sections: for (j, lines) in sections.iter().enumerate() {
            if j != 0 {
                if i == sy {
                    break;
                }
                let line = styled::Cell {
                    text: "q".into(),
                    width: 1,
                    style: CellStyle {
                        attrs: border.attrs.union(Attrs::ACS),
                        ..border.clone()
                    },
                };
                for x in 0..sx {
                    c.put(cx + x, cy + i, line.clone());
                }
                if sx > 14 {
                    c.put(
                        cx + 14,
                        cy + i,
                        styled::Cell {
                            text: "n".into(),
                            ..line
                        },
                    );
                }
                i += 1;
            }
            for l in lines {
                if i == sy {
                    break 'sections;
                }
                let expanded = vtmux_core::format::expand_time(l, &ctx);
                c.draw(cx, cy + i, sx, &CellStyle::default(), &expanded, false);
                i += 1;
            }
        }
        if sx > 14 && i < sy {
            c.vline(cx + 14, cy + i, sy - i, &border);
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

/// `window_tree_draw_label`: `label` in a box in the middle of the
/// `size` area at `at`, trimmed to fit.
fn tree_label(
    c: &mut Canvas,
    (px, py): (usize, usize),
    (sx, sy): (usize, usize),
    border: &CellStyle,
    label_gc: &CellStyle,
    label: &str,
) {
    if sx < 5 || sy < 3 {
        return;
    }
    let mut label = label.to_owned();
    let mut width = styled::format_width(&label);
    if width > sx - 4 {
        label = vtmux_core::format::trim::trim_left(&label, sx - 4);
        width = styled::format_width(&label);
    }
    if width == 0 {
        return;
    }
    // (sx - width + 1) / 2 and (sy + 1) / 2.
    let (ox, oy) = ((sx - width).div_ceil(2), sy.div_ceil(2));
    c.draw_box(px + ox - 2, py + oy - 1, width + 4, 3, border);
    for x in 0..width + 2 {
        c.put(px + ox - 1 + x, py + oy, blank(border.bg));
    }
    c.draw(px + ox, py + oy, width, label_gc, &label, false);
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
