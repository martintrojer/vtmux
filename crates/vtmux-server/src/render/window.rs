//! The model's side of drawing a window: which panes are visible, their
//! border options and styles, status lines and the client's theme colours,
//! gathered into a [`Scene`] and a [`Draw`].

use std::collections::BTreeMap;

use vtmux_core::colour::Colour;
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::layout::{Cell as LayoutCell, Dir, PaneStatus};
use vtmux_core::options::TreeId;
use vtmux_core::style::{CellStyle, Style};
use vtmux_core::styled;
use vtmux_core::time::WallClock as _;
use vtmux_vt::{Color, Snapshot};

use super::Caps;
use super::compose::{Draw, PaneDraw, Theme, pane_status};
use super::scene::{Indicators, Lines, Scene, ScenePane, WindowView};
use crate::environ::Environ;
use crate::format_ctx::{Ctx, ServerInfo};
use crate::mode::copy::Paint;
use crate::model::Model;
use crate::target::Found;

/// What the window drawing reads from the server.
pub struct Source<'a> {
    pub model: &'a Model,
    pub info: &'a ServerInfo,
    pub env: &'a Environ,
}

impl Source<'_> {
    pub(super) fn ctx(&self, found: Found, caps: Caps) -> Ctx<'_> {
        let mut ctx = Ctx::new(self.model, self.info, self.env, found);
        ctx.extra
            .push(("client_colours".into(), caps.colours.to_string()));
        ctx
    }

    pub(super) fn string(&self, tree: TreeId, name: &str) -> String {
        self.model
            .options
            .get(tree, name)
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default()
    }

    /// `options_string_to_style` + `style_apply`: the option expanded and
    /// parsed over the default cell; an invalid style is the default.
    pub(crate) fn style(&self, tree: TreeId, name: &str, ctx: &Ctx<'_>) -> CellStyle {
        let text = self.string(tree, name);
        let text = if text.contains("#{") {
            vtmux_core::format::expand_str(&text, ctx)
        } else {
            text
        };
        let base = CellStyle::default();
        let mut sy = Style::new(&base);
        if sy.parse(&base, &text).is_err() {
            return base;
        }
        sy.cell
    }

    /// `server_client_update_theme_colours`. The terminal's background is
    /// never queried, so `detect` uses the dark theme, as tmux does while
    /// the theme is unknown.
    #[must_use]
    pub fn theme(&self, caps: Caps) -> Theme {
        // colour.c colour_theme_table: terminal colours per slot.
        const TERMINAL: [u8; 10] = [0, 7, 7, 0, 2, 3, 1, 4, 6, 5];
        const SLOTS: [&str; 10] = [
            "black",
            "white",
            "light-grey",
            "dark-grey",
            "green",
            "yellow",
            "red",
            "blue",
            "cyan",
            "magenta",
        ];
        let option = self.model.number_option(TreeId::Server, "theme");
        if option == 1 {
            return Theme(TERMINAL.map(Color::Palette));
        }
        let prefix = if option == 2 { "light" } else { "dark" };
        let ctx = self.ctx(Found::default(), caps);
        Theme(SLOTS.map(|slot| {
            let value = self.string(TreeId::Server, &format!("{prefix}-theme-{slot}"));
            // server_client_update_theme_colours (server-client.c:1293):
            // FORMAT_NOJOBS.
            match Colour::parse(&vtmux_core::format::expand_str_nojobs(&value, &ctx)) {
                None | Some(Colour::Theme(_)) => Color::Default,
                Some(c) => Theme::default().colour(c),
            }
        }))
    }

    /// The scene and drawing data for session `s`'s current window on a
    /// `cols` x `rows` area.
    #[must_use]
    #[allow(clippy::too_many_lines)] // one pass over the window's panes
    pub fn window(&self, s: SessionId, cols: u16, rows: u16, caps: Caps) -> Option<(Scene, Draw)> {
        let found = Found::from_session(self.model, s);
        let w = found.window?;
        let window = self.model.windows.get(&w)?;
        let active = window.active();
        let wtree = TreeId::Window(w);
        // D23: under the focus ring the window sits 1 cell in from the
        // drawn area's corner, inside a frame the scene draws.
        let ring = self.model.number_option(wtree, "vtmux-focus-ring") != 0;
        let offset = u16::from(ring);
        let mut panes = Vec::new();
        let mut draws = BTreeMap::new();
        for &p in window.panes.z_order.iter().rev() {
            let Some(pane) = self.model.panes.get(&p) else {
                continue;
            };
            if !window.is_visible(p) {
                continue;
            }
            let floating = window.is_floating(p);
            let lines = self.lines(w, p, floating);
            let hides = window.zoom.as_ref().is_some_and(|z| z.pane == p)
                && pane
                    .modes
                    .first()
                    .is_some_and(crate::mode::Mode::hides_pane_status);
            let mut rect = pane.rect;
            rect.xoff += i32::from(offset);
            rect.yoff += i32::from(offset);
            panes.push(ScenePane {
                id: p,
                rect,
                floating,
                lines,
                status: if hides {
                    PaneStatus::Off
                } else {
                    self.pane_status(w, p, floating, lines)
                },
            });
            let ctx = self.ctx(
                Found {
                    pane: Some(p),
                    ..found
                },
                caps,
            );
            let option = if p == active {
                "pane-active-border-style"
            } else {
                "pane-border-style"
            };
            let (content, selected) = self.pane_screen(pane, TreeId::Session(s), &ctx, caps);
            draws.insert(
                p,
                PaneDraw {
                    content,
                    selected,
                    index: self.model.pane_index(p),
                    lines,
                    border: self.style(TreeId::Pane(p), option, &ctx),
                    status: Vec::new(),
                },
            );
        }
        let frame = 2 * u32::from(offset);
        let view = WindowView {
            sx: window.sx() + frame,
            sy: window.sy() + frame,
            panes,
            // D23: pane-border-indicators is ignored under the ring.
            indicators: if ring {
                Indicators::Off
            } else {
                Indicators::from_choice(self.model.number_option(wtree, "pane-border-indicators"))
            },
            two_pane: two_pane(self.model, w),
            ring: ring.then_some(active),
        };
        let scene = Scene::build(&view, cols, rows);
        let ring_style = draws
            .get(&active)
            .map(|d| d.border.clone())
            .unwrap_or_default();
        for p in &view.panes {
            let under = scene.status_line(p.id);
            if under.is_empty() {
                continue;
            }
            let ctx = self.ctx(
                Found {
                    pane: Some(p.id),
                    ..found
                },
                caps,
            );
            let format = self.string(TreeId::Pane(p.id), "pane-border-format");
            let expanded = vtmux_core::format::expand_time(&format, &ctx);
            let d = draws.get_mut(&p.id).expect("drawn pane");
            d.status = pane_status(&under, &d.border, &ring_style, d.lines, d.index, &expanded);
        }
        let ctx = self.ctx(found, caps);
        let draw = Draw {
            panes: draws,
            active: Some(active),
            offset,
            border: self.style(wtree, "pane-border-style", &ctx),
            lines: Lines::from_choice(self.model.number_option(wtree, "pane-border-lines")),
            outside: self.fill(w, false, caps),
            inside: self.fill(w, true, caps),
            clear: caps.clear(),
        };
        Some((scene, draw))
    }

    /// What `pane` shows: its shown mode's screen, else its own, and which
    /// cells are selected.
    fn pane_screen(
        &self,
        pane: &crate::model::Pane,
        tree: TreeId,
        ctx: &Ctx<'_>,
        caps: Caps,
    ) -> (Snapshot, Vec<bool>) {
        if let Some(crate::mode::Mode::Clock(m)) = pane.modes.first() {
            let screen = self.clock_screen(pane, m, ctx, caps);
            return (screen, Vec::new());
        }
        if let Some(crate::mode::Mode::Panes(m)) = pane.modes.first() {
            return (self.panes_screen(pane, m, caps), Vec::new());
        }
        let (mut screen, selected) = match pane.modes.first() {
            Some(crate::mode::Mode::Copy(m)) => {
                let vi = self
                    .model
                    .number_option(TreeId::Window(pane.window), "mode-keys")
                    == 1;
                (self.copy_screen(pane, m, ctx, caps), m.selection_mask(vi))
            }
            Some(crate::mode::Mode::Tree(t)) => (self.tree_screen(pane, &t.tree, caps), Vec::new()),
            _ => (vtmux_vt::snapshot(&pane.term), Vec::new()),
        };
        if let Some(prompt) = pane.mode_prompt.as_ref().filter(|m| m.shown(pane)) {
            self.mode_prompt(&mut screen, prompt, tree, caps);
        }
        (screen, selected)
    }

    /// Clock mode's screen (`window_clock_draw_screen`): the time now in
    /// `clock-mode-style`, coloured by `clock-mode-colour`.
    fn clock_screen(
        &self,
        pane: &crate::model::Pane,
        m: &crate::mode::ClockMode,
        ctx: &Ctx<'_>,
        caps: Caps,
    ) -> Snapshot {
        let wtree = TreeId::Window(pane.window);
        // style_apply on a colour option: `style_parse_colour`, so the text
        // is one colour; one that does not parse is the default.
        let value = self.string(wtree, "clock-mode-colour");
        let value = if value.contains("#{") {
            vtmux_core::format::expand_str(&value, ctx)
        } else {
            value
        };
        let colour = Colour::parse(&value).map_or(Color::Default, |c| self.theme(caps).colour(c));
        let style = self.model.number_option(wtree, "clock-mode-style");
        let tm = vtmux_core::time::SystemWallClock
            .local_time(crate::mode::clock::now())
            .unwrap_or_else(|| vtmux_core::time::Tm::utc(0));
        m.screen(&crate::mode::clock::time_text(&tm, style), colour)
    }

    /// `mode_tree_draw_prompt` (mode-tree.c:1046-1070): the mode's prompt
    /// over the last row of its screen, or the first with
    /// `status-position top`, and the cursor on it, shown.
    fn mode_prompt(
        &self,
        screen: &mut Snapshot,
        prompt: &crate::mode::prompt::ModePrompt,
        tree: TreeId,
        caps: Caps,
    ) {
        let (cols, rows) = (usize::from(screen.cols), usize::from(screen.rows));
        if cols == 0 || rows == 0 {
            return;
        }
        let py = if prompt.top { 0 } else { rows - 1 };
        let (row, cx) = super::status::mode_prompt_row(self, tree, &prompt.open, cols);
        let theme = self.theme(caps);
        for (x, drawn) in row.iter().enumerate() {
            if drawn.text != super::status::KEEP {
                screen.cells[py * cols + x] = theme.cell(drawn);
            }
        }
        screen.cursor = Some((
            u16::try_from(cx.min(cols - 1)).unwrap_or(0),
            u16::try_from(py).unwrap_or(0),
        ));
        screen.cursor_visible = true;
    }

    /// Copy mode's screen for `pane` (`window_copy_write_line`): the rows in
    /// view, then `copy-mode-position-format` drawn over the top row in
    /// `copy-mode-position-style` unless hidden, then the `$` of a cursor
    /// past the last column. The mark's line and search matches take their
    /// styles, selected cells `copy-mode-selection-style`.
    /// Line numbers are not drawn yet.
    fn copy_screen(
        &self,
        pane: &crate::model::Pane,
        m: &crate::mode::CopyMode,
        ctx: &Ctx<'_>,
        caps: Caps,
    ) -> Snapshot {
        // format_draw keeps what a section does not cover: those cells are
        // marked and keep the pane's.
        const KEEP: &str = "\u{0}";
        let wtree = TreeId::Window(pane.window);
        let theme = self.theme(caps);
        let sel = self.style(wtree, "copy-mode-selection-style", ctx);
        let styles = [
            "copy-mode-mark-style",
            "copy-mode-match-style",
            "copy-mode-current-match-style",
        ]
        .map(|name| theme.style(&self.style(wtree, name, ctx)));
        let paint = |c: &mut vtmux_vt::Cell, p: Paint| {
            let (style, inverse) = match p {
                Paint::Selection => return select_cell(&theme, &sel, c),
                Paint::Mark { inverse } => (styles[0], inverse),
                Paint::Match { inverse } => (styles[1], inverse),
                Paint::CurrentMatch { inverse } => (styles[2], inverse),
            };
            // window_copy_update_style: the style's attributes and colours
            // replace the cell's, the colours swapped on the mark's cell.
            c.style = style;
            if inverse {
                std::mem::swap(&mut c.style.fg, &mut c.style.bg);
            }
        };
        let vi = self.model.number_option(wtree, "mode-keys") == 1;
        let mut screen = m.screen(vi, &paint);
        // `s->rupper < s->rlower`: a screen of one row has no indicator.
        if m.shows_position() && screen.rows > 1 {
            let format = self.string(wtree, "copy-mode-position-format");
            let expanded = vtmux_core::format::expand_time(&format, ctx);
            if !expanded.is_empty() {
                let base = self.style(wtree, "copy-mode-position-style", ctx);
                let cols = usize::from(screen.cols);
                let under = vec![
                    styled::Cell {
                        text: KEEP.into(),
                        width: 1,
                        style: base.clone(),
                    };
                    cols
                ];
                let drawn = styled::draw_onto(&base, under, &expanded, false, false);
                for (x, c) in drawn.cells.iter().enumerate() {
                    if c.text != KEEP {
                        screen.cells[x] = theme.cell(c);
                    }
                }
            }
        }
        m.mark_overflow(&mut screen, &paint);
        screen
    }

    /// `window_pane_get_pane_lines`: a floating pane's own option, else the
    /// window's.
    fn lines(&self, w: WindowId, p: PaneId, floating: bool) -> Lines {
        let tree = if floating {
            TreeId::Pane(p)
        } else {
            TreeId::Window(w)
        };
        Lines::from_choice(self.model.number_option(tree, "pane-border-lines"))
    }

    /// `window_pane_get_pane_status` (window.c:2930). Read from the option,
    /// not the layout's, which is off under the focus ring (the ring's edge
    /// carries the line).
    fn pane_status(&self, w: WindowId, p: PaneId, floating: bool, lines: Lines) -> PaneStatus {
        if !floating {
            return self.model.border_status(w);
        }
        if lines == Lines::None {
            return PaneStatus::Off;
        }
        match self
            .model
            .number_option(TreeId::Pane(p), "pane-border-status")
        {
            1 | 3 => PaneStatus::Top,
            2 | 4 => PaneStatus::Bottom,
            _ => PaneStatus::Off,
        }
    }

    /// `window_set_fill_cell`: `fill-character` expanded with `is_inside`
    /// and drawn into one cell; anything but a single-width character is
    /// the ACS bullet.
    fn fill(&self, w: WindowId, inside: bool, caps: Caps) -> styled::Cell {
        let mut ctx = self.ctx(
            Found {
                window: Some(w),
                ..Found::default()
            },
            caps,
        );
        ctx.extra
            .push(("is_inside".into(), if inside { "1" } else { "0" }.into()));
        ctx.extra
            .push(("is_outside".into(), if inside { "0" } else { "1" }.into()));
        let value = self.string(TreeId::Window(w), "fill-character");
        // window_get_border_cell's fill-character (window-border.c:41):
        // FORMAT_NOJOBS.
        let expanded = vtmux_core::format::expand_str_nojobs(&value, &ctx);
        let cell = styled::draw(&CellStyle::default(), 1, &expanded, false, false)
            .cells
            .remove(0);
        if cell.width == 1 {
            return cell;
        }
        styled::Cell {
            text: "~".into(),
            width: 1,
            style: CellStyle {
                attrs: vtmux_core::style::Attrs::ACS,
                ..CellStyle::default()
            },
        }
    }
}

/// `redraw_check_two_pane_colours`: exactly two tiled panes with a parent
/// node, and that node's direction. A zoomed window has none (the zoomed
/// pane's cell is the root).
fn two_pane(model: &Model, w: WindowId) -> Option<Dir> {
    let window = model.windows.get(&w)?;
    if window.zoomed() {
        return None;
    }
    let root = &window.layout.root;
    let tiled: Vec<_> = root
        .leaves()
        .into_iter()
        .filter(|(_, _, floating)| !floating)
        .map(|(p, _, _)| *p)
        .collect();
    if tiled.len() != 2 {
        return None;
    }
    let mut dir = None;
    for p in tiled {
        let path = root.find_pane(p)?;
        let (_, parent) = path.split_last()?;
        if let LayoutCell::Node(n) = root.get(parent) {
            dir = Some(n.dir);
        }
    }
    dir
}

/// Copy the active pane's cursor onto the screen.
pub fn cursor(screen: &mut Snapshot, draw: &Draw, model: &Model, rows: u16) {
    screen.cursor = None;
    screen.cursor_visible = false;
    let Some(p) = draw.active else { return };
    let (Some(d), Some(pane)) = (draw.panes.get(&p), model.panes.get(&p)) else {
        return;
    };
    let Some((cx, cy)) = d.content.cursor else {
        return;
    };
    let x = pane.rect.xoff + i32::from(cx) + i32::from(draw.offset);
    let y = pane.rect.yoff + i32::from(cy) + i32::from(draw.offset);
    let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
        return;
    };
    // Panes mode leaves the cursor one past its last write; at the
    // window's edge the terminal keeps it in the last column.
    let x = if x == screen.cols {
        x.saturating_sub(1)
    } else {
        x
    };
    if x < screen.cols && y < rows {
        screen.cursor = Some((x, y));
        screen.cursor_visible = d.content.cursor_visible;
    }
}

/// `screen_select_cell` (screen.c:641-661): the selection style over a
/// cell, its default colours the cell's own; `noattr` drops the cell's
/// attributes, else they are added to the style's.
fn select_cell(theme: &super::compose::Theme, sel: &CellStyle, c: &mut vtmux_vt::Cell) {
    use vtmux_core::style::Attrs;
    let default = |x: Colour| matches!(x, Colour::Default | Colour::Terminal);
    let mut style = theme.style(sel);
    if default(sel.fg) {
        style.fg = c.style.fg;
    }
    if default(sel.bg) {
        style.bg = c.style.bg;
    }
    if !sel.attrs.contains(Attrs::NOATTR) {
        let src = c.style;
        style.bold |= src.bold;
        style.faint |= src.faint;
        style.italic |= src.italic;
        style.blink |= src.blink;
        style.inverse |= src.inverse;
        style.invisible |= src.invisible;
        style.strikethrough |= src.strikethrough;
        style.overline |= src.overline;
        if style.underline == vtmux_vt::Underline::None {
            style.underline = src.underline;
        }
    }
    c.style = style;
}
