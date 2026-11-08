//! The model's side of drawing a window: which panes are visible, their
//! border options and styles, status lines and the client's theme colours,
//! gathered into a [`Scene`] and a [`Draw`].

use std::collections::BTreeMap;
use std::rc::Rc;

use vtmux_core::colour::Colour;
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::layout::{Cell as LayoutCell, Dir, PaneStatus};
use vtmux_core::options::TreeId;
use vtmux_core::style::{CellStyle, Style};
use vtmux_core::styled;
use vtmux_core::time::WallClock as _;
use vtmux_vt::{Color, Snapshot};

use super::Caps;
use super::blend::{TermColours, resolve};
use super::compose::{BarDraw, Draw, PaneDraw, Theme, pane_status};
use super::palette::{Dim, Palette};
use super::scene::{Indicators, Lines, Scene, ScenePane, WindowView};
use crate::environ::Environ;
use crate::format_ctx::{ClientView, Ctx, ServerInfo};
use crate::format_jobs::{JobHandle, TAG_PANE};
use crate::mode::copy::Paint;
use crate::model::Model;
use crate::target::Found;

/// What a pane shows ([`Source::pane_screen`]).
struct PaneScreen {
    content: Rc<Snapshot>,
    /// [`PaneDraw::stamps`]: only for the pane's own screen.
    stamps: Vec<u64>,
    /// [`PaneDraw::selected`].
    selected: Vec<bool>,
    /// [`PaneDraw::no_palette`].
    no_palette: Vec<bool>,
}

/// A palette as `tty_attributes` reads it (`wp->palette`, `pd->palette`):
/// the entries the program on `term` set with OSC 4 (`palette->palette`)
/// over `pane-colours` in `tree` (`default_palette`, colour.c
/// `colour_palette_get`); `None` when neither has an entry.
pub(crate) fn palette(
    model: &Model,
    tree: TreeId,
    term: &vtmux_vt::PaneTerm,
    theme: &Theme,
) -> Option<Palette> {
    use vtmux_core::options::{ArrayKey, OptionValue, Value};
    let set = term.palette_entries();
    let option = match model.options.get(tree, "pane-colours").map(|e| &e.value) {
        Some(OptionValue::Array(items)) if !items.is_empty() => Some(items),
        _ => None,
    };
    if set.is_none() && option.is_none() {
        return None;
    }
    let mut p = [None; 256];
    for (k, v) in option.into_iter().flatten() {
        if let (ArrayKey::Index(i), Value::Colour(c)) = (k, v)
            && let Some(slot) = usize::try_from(*i).ok().and_then(|i| p.get_mut(i))
        {
            *slot = Some(theme.colour(*c));
        }
    }
    for (slot, c) in p.iter_mut().zip(set.into_iter().flatten()) {
        if let Some((r, g, b)) = *c {
            *slot = Some(Color::Rgb(r, g, b));
        }
    }
    Some(Rc::new(p))
}

/// What the window drawing reads from the server.
pub struct Source<'a> {
    pub model: &'a Model,
    pub info: &'a ServerInfo,
    pub env: &'a Environ,
    /// The attached clients (`#{session_attached_list}`).
    pub clients: Clients<'a>,
    /// The last [`Source::theme`], for the option values it came from.
    pub themes: &'a ThemeCache,
    /// The key tables (customize-mode's preview).
    pub keys: &'a vtmux_core::keys::KeyBindings,
}

/// The attached clients as formats see them, worked out on first use:
/// only the tree modes read them, and a frame is drawn far more often.
pub struct Clients<'a> {
    make: Box<dyn Fn() -> Rc<[ClientView]> + 'a>,
    views: std::cell::OnceCell<Rc<[ClientView]>>,
}

impl<'a> Clients<'a> {
    pub fn new(make: impl Fn() -> Rc<[ClientView]> + 'a) -> Self {
        Self {
            make: Box::new(make),
            views: std::cell::OnceCell::new(),
        }
    }

    /// The clients, in connection order.
    pub fn views(&self) -> &Rc<[ClientView]> {
        self.views.get_or_init(&self.make)
    }
}

/// Each pane's border style as a client last expanded it (`wp->border_gc`,
/// `wp->active_border_gc`). tmux expands `pane-border-style` or
/// `pane-active-border-style` once and keeps it until a client draws the
/// borders or the status again (`window_pane_get_border_style`,
/// window-border.c:151-177; reset in `redraw_draw`,
/// screen-redraw.c:1726-1731), so pane output alone, a moved cursor say,
/// does not change a border. tmux keeps one per pane, but only a draw that
/// resets it first draws a border with it: a client's terminal shows the
/// style its own last border or status redraw expanded, with its own
/// session and client (`format_create_defaults(NULL, c, s, ...)`). So each
/// client keeps its own ([`crate::render::WindowCache`]).
/// [`BorderStyles::forget`] is the reset; the [`BorderKey`] also expands
/// it again on the changes tmux redraws the borders for (an option, the
/// active or marked pane, a mode, the layout), whether or not vtmux asks
/// for a redraw there.
#[derive(Clone, Debug, Default)]
pub struct BorderStyles {
    kept: std::cell::RefCell<BTreeMap<PaneId, (BorderKey, CellStyle)>>,
    /// The panes whose style the last [`Source::window`] expanded rather
    /// than took from `kept`.
    fresh: std::cell::RefCell<Vec<PaneId>>,
}

/// The border styles one client's drawing used, for another client to
/// take the same drawing ([`BorderStyles::follow`]): per pane, the
/// [`BorderKey`], the style, and whether it was expanded for this
/// drawing.
#[derive(Debug)]
pub struct BorderShare(Vec<(PaneId, BorderKey, CellStyle, bool)>);

/// What a kept border style was expanded under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BorderKey {
    /// The active pane's style or the other one (tmux keeps both).
    active: bool,
    /// [`vtmux_core::options::OptionsStore::generation`]: every option
    /// change redraws every client (`options_push_changes`).
    options: u64,
    /// `#{pane_in_mode}`: `window_pane_set_mode` and
    /// `window_pane_reset_mode` redraw the borders (window.c:1788, 1831).
    in_mode: bool,
    /// `#{pane_marked}` (select-pane -m redraws the borders).
    marked: Option<PaneId>,
    /// `#{pane_modal_flag}`.
    modal: Option<PaneId>,
    /// `layout_fix_panes` and a redraw follow every layout change.
    rect: vtmux_core::layout::Geometry,
}

impl BorderStyles {
    /// `redraw_draw` with `REDRAW_PANE_BORDER` or `REDRAW_PANE_STATUS`
    /// (screen-redraw.c:1726-1731): the styles of window `w`'s panes are
    /// expanded again. Panes that are gone go too.
    pub fn forget(&self, model: &Model, w: WindowId) {
        self.kept
            .borrow_mut()
            .retain(|p, _| model.panes.get(p).is_some_and(|pane| pane.window != w));
    }

    /// The styles the last [`Source::window`] with these styles drew
    /// `draw`'s panes with.
    #[must_use]
    pub fn share(&self, draw: &Draw) -> BorderShare {
        let kept = self.kept.borrow();
        let fresh = self.fresh.borrow();
        BorderShare(
            draw.panes
                .keys()
                .filter_map(|p| {
                    let (key, style) = kept.get(p)?;
                    Some((*p, *key, style.clone(), fresh.contains(p)))
                })
                .collect(),
        )
    }

    /// Whether [`Source::window`] with these styles would draw every pane
    /// of `share` in the style it has there, in the same turn (the model
    /// unchanged): a style kept under the same key must be the same, and
    /// one these styles would expand again must have been expanded for
    /// `share` too (an expansion depends only on the model and the
    /// session, not the client). If so, these styles become what that
    /// drawing would have left.
    #[must_use]
    pub fn follow(&self, share: &BorderShare) -> bool {
        let mut kept = self.kept.borrow_mut();
        let same = share
            .0
            .iter()
            .all(|(p, key, style, fresh)| match kept.get(p) {
                Some((k, s)) if k == key => s == style,
                _ => *fresh,
            });
        if same {
            for (p, key, style, _) in &share.0 {
                if kept.get(p).is_none_or(|(k, _)| k != key) {
                    kept.insert(*p, (*key, style.clone()));
                }
            }
        }
        same
    }
}

/// [`Source::theme`]'s last result and its inputs: the `theme` option, the
/// ten slot options' values and the client's colours. tmux works the
/// colours out once when one of these options changes or a client
/// attaches (`server_client_update_theme_colours`), not per frame; the
/// slot formats normally read only `#{client_colours}`.
#[derive(Debug, Default)]
pub struct ThemeCache(std::cell::RefCell<Option<ThemeInputs>>);

#[derive(Debug)]
struct ThemeInputs {
    /// [`vtmux_core::options::OptionsStore::generation`] when last checked:
    /// while it stays, so do the option values.
    generation: u64,
    option: i64,
    values: Vec<String>,
    colours: u32,
    /// The client's reported theme ([`Caps::light_theme`]) when `option`
    /// is `auto`: it picks the dark or the light slot options.
    light: bool,
    theme: Theme,
}

impl Source<'_> {
    pub(super) fn ctx(&self, found: Found, caps: Caps) -> Ctx<'_> {
        let mut ctx = Ctx::new(self.model, self.info, self.env, found);
        ctx.extra
            .push(("client_colours".into(), caps.colours.to_string()));
        ctx
    }

    /// `window_make_pane_status`'s format tree (window-border.c:199): the
    /// client's jobs, tagged with the pane, for a status line.
    fn border_ctx(&self, found: Found, p: PaneId, caps: Caps, jobs: Option<&JobHandle>) -> Ctx<'_> {
        let mut ctx = self.ctx(
            Found {
                pane: Some(p),
                ..found
            },
            caps,
        );
        ctx.jobs = jobs.map(|j| j.clone().with_tag(TAG_PANE | p.0));
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
        self.parsed_style(tree, name, ctx).cell
    }

    /// Style option `name` parsed whole (`options_string_to_style`); one
    /// that does not parse is the default style.
    fn parsed_style(&self, tree: TreeId, name: &str, ctx: &Ctx<'_>) -> Style {
        let text = self.string(tree, name);
        let text = if text.contains("#{") {
            vtmux_core::format::expand_str(&text, ctx)
        } else {
            text
        };
        let base = CellStyle::default();
        let mut sy = Style::new(&base);
        if sy.parse(&base, &text).is_err() {
            return Style::new(&base);
        }
        sy
    }

    /// `server_client_update_theme_colours` (server-client.c:1274-1310):
    /// `detect` takes the client's theme ([`Caps::light_theme`], dark while
    /// unknown), `light` and `dark` force one.
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
        let generation = self.model.options.generation();
        if let Some(t) = self.themes.0.borrow().as_ref()
            && (t.generation, t.colours, t.light) == (generation, caps.colours, caps.light_theme)
        {
            return t.theme;
        }
        let option = self.model.number_option(TreeId::Server, "theme");
        if option == 1 {
            return Theme(TERMINAL.map(Color::Palette));
        }
        let light = option == 2 || (option != 3 && caps.light_theme);
        let prefix = if light { "light" } else { "dark" };
        let values: Vec<String> = SLOTS
            .iter()
            .map(|slot| self.string(TreeId::Server, &format!("{prefix}-theme-{slot}")))
            .collect();
        if let Some(t) = self.themes.0.borrow_mut().as_mut()
            && (t.option, &t.values, t.colours, t.light)
                == (option, &values, caps.colours, caps.light_theme)
        {
            t.generation = generation;
            return t.theme;
        }
        let ctx = self.ctx(Found::default(), caps);
        let mut slots = values.iter();
        let theme = Theme([(); 10].map(|()| {
            let value = slots.next().map_or("", String::as_str);
            // server_client_update_theme_colours (server-client.c:1293):
            // FORMAT_NOJOBS.
            match Colour::parse(&vtmux_core::format::expand_str_nojobs(value, &ctx)) {
                None | Some(Colour::Theme(_)) => Color::Default,
                Some(c) => Theme::default().colour(c),
            }
        }));
        *self.themes.0.borrow_mut() = Some(ThemeInputs {
            generation,
            option,
            values,
            colours: caps.colours,
            light: caps.light_theme,
            theme,
        });
        theme
    }

    /// The cursor of `pane`'s screen as drawn (`wp->screen`: its mode's,
    /// else the program's) in session `s`, when that screen shows it
    /// (`MODE_CURSOR`). A plain pane's is where the program left it, as
    /// `#{cursor_x}` reports it, whether or not its output is held.
    pub(crate) fn screen_cursor(
        &self,
        pane: &crate::model::Pane,
        s: SessionId,
        caps: Caps,
    ) -> Option<(u16, u16)> {
        if pane.modes.is_empty() && !pane.mode_prompt.as_ref().is_some_and(|m| m.shown(pane)) {
            let shown = pane.term.screen().snapshot().cursor_visible;
            return shown.then(|| pane.term.cursor());
        }
        let found = Found {
            pane: Some(pane.id),
            ..Found::from_session(self.model, s)
        };
        let ctx = self.ctx(found, caps);
        let screen = self
            .pane_screen(pane, TreeId::Session(s), &ctx, caps)
            .content;
        screen.cursor.filter(|_| screen.cursor_visible)
    }

    /// The scene and drawing data for session `s`'s current window on a
    /// `cols` x `rows` area, seen from window cell `origin` (the client's
    /// [`crate::view::View`]).
    #[must_use]
    #[allow(clippy::too_many_lines)] // one pass over the window's panes
    #[expect(clippy::too_many_arguments, reason = "the window's drawing inputs")]
    pub fn window(
        &self,
        s: SessionId,
        cols: u16,
        rows: u16,
        origin: (u32, u32),
        caps: Caps,
        jobs: Option<&JobHandle>,
        cache: Option<&super::WindowCache>,
    ) -> Option<(Rc<Scene>, Draw)> {
        let found = Found::from_session(self.model, s);
        let w = found.window?;
        let window = self.model.windows.get(&w)?;
        let active = window.active();
        let wtree = TreeId::Window(w);
        let marked_pane = self.model.marked().and_then(|m| m.pane);
        let generation = self.model.options.generation();
        if let Some(c) = cache {
            c.borders.fresh.borrow_mut().clear();
        }
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
            let rect = pane.rect;
            let placement = self.model.scrollbar_placement(p);
            panes.push(ScenePane {
                id: p,
                rect,
                floating,
                lines,
                scrollbar: placement,
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
            let key = BorderKey {
                active: p == active,
                options: generation,
                in_mode: !pane.modes.is_empty(),
                marked: marked_pane,
                modal: window.panes.modal,
                rect: pane.rect,
            };
            let border = self.border_style(cache.map(|c| &c.borders), p, key, &ctx);
            let palette = palette(self.model, TreeId::Pane(p), &pane.term, &self.theme(caps));
            let shown = self.pane_screen(pane, TreeId::Session(s), &ctx, caps);
            let (defaults, dim) = self.pane_defaults(pane, p == active, &ctx);
            draws.insert(
                p,
                PaneDraw {
                    content: shown.content,
                    stamps: shown.stamps,
                    selected: shown.selected,
                    index: self.model.pane_index(p),
                    defaults,
                    lines,
                    border,
                    status: Vec::new(),
                    status_ranges: Vec::new(),
                    scrollbar: placement.map(|placement| BarDraw {
                        style: self.model.scrollbar_style(p),
                        placement,
                        slider: self.model.scrollbar_slider(p),
                    }),
                    palette,
                    no_palette: shown.no_palette,
                    dim,
                    blend: None,
                },
            );
        }
        let status_rows: BTreeMap<PaneId, PaneStatus> =
            panes.iter().map(|p| (p.id, p.status)).collect();
        let view = WindowView {
            sx: window.sx(),
            sy: window.sy(),
            ox: origin.0,
            oy: origin.1,
            panes,
            indicators: Indicators::from_choice(
                self.model.number_option(wtree, "pane-border-indicators"),
            ),
            two_pane: two_pane(self.model, w),
        };
        let order: Vec<PaneId> = view.panes.iter().map(|p| p.id).collect();
        let scene = match cache {
            Some(cache) => Scene::cached(&cache.composition.borrow().scene, view, cols, rows),
            None => Rc::new(Scene::build(&view, cols, rows)),
        };
        for p in order {
            let mut under = scene.status_line(p);
            // redraw_mark_menu goes over the scene first, so the status
            // line is as wide as what the menu leaves of it
            // (redraw_pane_status_width).
            if let (Some(menu), Some(pane)) = (window.menu.as_ref(), self.model.panes.get(&p)) {
                let r = pane.rect;
                let wy = match status_rows.get(&p) {
                    Some(PaneStatus::Top) => i64::from(r.yoff) - 1,
                    Some(PaneStatus::Bottom) => i64::from(r.yoff) + i64::from(r.sy),
                    _ => -1,
                };
                let (mx, my) = (i64::from(menu.px), i64::from(menu.py));
                let (mw, mh) = (i64::from(menu.box_width()), i64::from(menu.box_height()));
                let covered = |o: usize| {
                    let wx = i64::from(r.xoff) + 2 + i64::try_from(o).unwrap_or(i64::MAX);
                    (mx..mx + mw).contains(&wx) && (my..my + mh).contains(&wy)
                };
                while under
                    .len()
                    .checked_sub(1)
                    .is_some_and(|o| under[o].is_none() || covered(o))
                {
                    under.pop();
                }
            }
            if under.is_empty() {
                continue;
            }
            let ctx = self.border_ctx(found, p, caps, jobs);
            let format = self.string(TreeId::Pane(p), "pane-border-format");
            let expanded = vtmux_core::format::expand_time(&format, &ctx);
            let d = draws.get_mut(&p).expect("drawn pane");
            let line = pane_status(&under, &d.border, d.lines, d.index, &expanded);
            (d.status, d.status_ranges) = (line.cells, line.ranges);
        }
        let ctx = self.ctx(found, caps);
        // Never drawn: a scene without such cells.
        let unused = || styled::Cell {
            text: String::new(),
            width: 1,
            style: CellStyle::default(),
        };
        let marked = found
            .index
            .and_then(|i| {
                self.model
                    .marked()
                    .filter(|m| m.session == Some(s) && m.index == Some(i))
            })
            .and_then(|m| m.pane);
        let draw = Draw {
            panes: draws,
            active: Some(active),
            marked,
            border: self.style(wtree, "pane-border-style", &ctx),
            lines: Lines::from_choice(self.model.number_option(wtree, "pane-border-lines")),
            // Expanded only when drawn: `fill-character` is a format.
            outside: if scene.fills().0 {
                self.fill(w, false, caps)
            } else {
                unused()
            },
            inside: if scene.fills().1 {
                self.fill(w, true, caps)
            } else {
                unused()
            },
            clear: caps.clear(),
            isolates: caps.isolates,
        };
        Some((scene, draw))
    }

    /// `window_pane_get_border_style`: pane `p`'s border style kept in
    /// `kept` (the drawing client's), else `pane-active-border-style`
    /// (`key.active`) or `pane-border-style` expanded and kept.
    fn border_style(
        &self,
        kept: Option<&BorderStyles>,
        p: PaneId,
        key: BorderKey,
        ctx: &Ctx<'_>,
    ) -> CellStyle {
        if let Some((k, style)) = kept.and_then(|b| b.kept.borrow().get(&p).cloned())
            && k == key
        {
            return style;
        }
        let option = if key.active {
            "pane-active-border-style"
        } else {
            "pane-border-style"
        };
        let style = self.style(TreeId::Pane(p), option, ctx);
        if let Some(b) = kept {
            b.kept.borrow_mut().insert(p, (key, style.clone()));
            b.fresh.borrow_mut().push(p);
        }
        style
    }

    /// tty.c `tty_default_colours` (`tty_style_changed`): the pane's
    /// default fg and bg and its dim. Each style is added to the colours
    /// the program set with OSC 10 and 11 (`tty_window_default_style`,
    /// `style_add`); the active pane takes `window-active-style`'s colours
    /// where they are set (an OSC colour counts) and its `dim=`, the others
    /// `window-style`'s.
    fn pane_defaults(
        &self,
        pane: &crate::model::Pane,
        active: bool,
        ctx: &Ctx<'_>,
    ) -> (CellStyle, Option<Dim>) {
        let tree = TreeId::Pane(pane.id);
        let (fg, bg) = pane.term.default_colours();
        let rgb =
            |c: Option<vtmux_vt::Rgb>| c.map_or(Colour::Default, |(r, g, b)| Colour::Rgb(r, g, b));
        let set = |c: Colour, base: Colour| if c == Colour::Default { base } else { c };
        let add = |sy: &Style| CellStyle {
            fg: set(sy.cell.fg, rgb(fg)),
            bg: set(sy.cell.bg, rgb(bg)),
            ..CellStyle::default()
        };
        let normal = self.parsed_style(tree, "window-style", ctx);
        if !active {
            return (add(&normal), Dim::new(normal.dim));
        }
        let on = self.parsed_style(tree, "window-active-style", ctx);
        let (a, n) = (add(&on), add(&normal));
        let colours = CellStyle {
            fg: set(a.fg, n.fg),
            bg: set(a.bg, n.bg),
            ..CellStyle::default()
        };
        (colours, Dim::new(on.dim))
    }

    /// D26: each pane's blend for a client with terminal colours
    /// `colours`: the active pane of the window takes
    /// `vtmux-pane-active-blend`, the others `vtmux-pane-inactive-blend`,
    /// none of them while the window is zoomed (one pane shows, nothing to
    /// tell apart).
    pub fn blend(
        &self,
        s: SessionId,
        draw: &mut Draw,
        colours: &TermColours,
        caps: Caps,
        theme: &Theme,
    ) {
        let Some(w) = Found::from_session(self.model, s).window else {
            return;
        };
        let tree = TreeId::Window(w);
        let active = self.string(tree, "vtmux-pane-active-blend");
        let inactive = self.string(tree, "vtmux-pane-inactive-blend");
        if active.is_empty() && inactive.is_empty() {
            return;
        }
        let zoomed = self.model.windows.get(&w).is_some_and(|w| w.zoom.is_some());
        let on = resolve(&active, false, colours, caps, theme);
        let off = (!zoomed)
            .then(|| resolve(&inactive, true, colours, caps, theme))
            .flatten();
        for (&p, d) in &mut draw.panes {
            d.blend = if Some(p) == draw.active { on } else { off };
        }
    }

    /// What `pane` shows: its shown mode's screen, else its own
    /// ([`PaneScreen`]).
    fn pane_screen(
        &self,
        pane: &crate::model::Pane,
        tree: TreeId,
        ctx: &Ctx<'_>,
        caps: Caps,
    ) -> PaneScreen {
        let only = |screen: Snapshot| PaneScreen {
            content: Rc::new(screen),
            stamps: Vec::new(),
            selected: Vec::new(),
            no_palette: Vec::new(),
        };
        if let Some(crate::mode::Mode::Clock(m)) = pane.modes.first() {
            let screen = self.clock_screen(pane, m, ctx, caps);
            // The time is written with GRID_FLAG_NOPALETTE
            // (window-clock.c:273-274, 286-287); the rest is cleared.
            let no_palette = screen.cells.iter().map(|c| !c.text.is_empty()).collect();
            return PaneScreen {
                no_palette,
                ..only(screen)
            };
        }
        if let Some(crate::mode::Mode::Panes(m)) = pane.modes.first() {
            return only(self.panes_screen(pane, m, caps));
        }
        if pane.modes.is_empty() && !pane.mode_prompt.as_ref().is_some_and(|m| m.shown(pane)) {
            // MODE_SYNC (screen-write.c:211-240): while the program has
            // synchronized output on, nothing new of it is drawn. The held
            // grid keeps the stamps it had, so its rows stay cached.
            let (content, stamps) = if let Some(held) = pane.term.sync_held() {
                (Rc::clone(&held.screen), held.stamps.to_vec())
            } else {
                let view = pane.term.screen();
                (view.shared(), view.row_stamps().to_vec())
            };
            return PaneScreen {
                content,
                stamps,
                selected: Vec::new(),
                no_palette: Vec::new(),
            };
        }
        let top = pane.modes.first();
        let (mut screen, selected, no_palette) =
            match (top, top.and_then(crate::mode::Mode::as_copy)) {
                // Copy and view mode draw alike (window_copy_write_line).
                (_, Some(m)) => {
                    let wtree = TreeId::Window(pane.window);
                    let vi = self.model.number_option(wtree, "mode-keys") == 1;
                    let numbers = self.model.number_option(wtree, "copy-mode-line-numbers");
                    let (screen, no_palette) = self.copy_screen(pane, m, ctx, caps);
                    (screen, m.selection_mask(vi, numbers), no_palette)
                }
                (Some(m), _) if m.tree_kind().is_some() => {
                    (self.tree_screen(pane, m, caps), Vec::new(), Vec::new())
                }
                (Some(crate::mode::Mode::Switch(m)), _) => {
                    (self.switch_screen(pane, m, caps), Vec::new(), Vec::new())
                }
                // A mode prompt over a plain pane (a held pane drew above).
                _ => (
                    pane.term.sync_held().map_or_else(
                        || vtmux_vt::snapshot(&pane.term),
                        |h| h.screen.as_ref().clone(),
                    ),
                    Vec::new(),
                    Vec::new(),
                ),
            };
        if let Some(prompt) = pane.mode_prompt.as_ref().filter(|m| m.shown(pane)) {
            self.mode_prompt(&mut screen, prompt, tree, caps);
        }
        PaneScreen {
            content: Rc::new(screen),
            stamps: Vec::new(),
            selected,
            no_palette,
        }
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
    /// styles, selected cells `copy-mode-selection-style`, the line
    /// numbers in `copy-mode-line-number-style` and
    /// `copy-mode-current-line-number-style`, and the cursor's line in
    /// `copy-mode-current-line-style`.
    fn copy_screen(
        &self,
        pane: &crate::model::Pane,
        m: &crate::mode::CopyMode,
        ctx: &Ctx<'_>,
        caps: Caps,
    ) -> (Snapshot, Vec<bool>) {
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
        let numbers = [
            "copy-mode-line-number-style",
            "copy-mode-current-line-number-style",
        ]
        .map(|name| theme.style(&self.style(wtree, name, ctx)));
        let line = self.style(wtree, "copy-mode-current-line-style", ctx);
        let line_style = theme.style(&line);
        let paint = |c: &mut vtmux_vt::Cell, p: Paint| {
            let (style, inverse) = match p {
                Paint::Selection => return select_cell(&theme, &sel, c),
                // style_apply on the default cell (window-copy.c:5444-5448).
                Paint::LineNumber { current } => {
                    c.style = numbers[usize::from(current)];
                    return;
                }
                Paint::CurrentLine => return current_line_cell(&line, line_style, c),
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
        let line_numbers = self.model.number_option(wtree, "copy-mode-line-numbers");
        let mut screen = m.screen(vi, line_numbers, &paint);
        // The indicator goes after the line numbers, in the text's columns
        // (window-copy.c:5476-5480).
        let text = m.text_columns(line_numbers);
        // GRID_FLAG_NOPALETTE (window-copy.c:5434-5450): the line numbers
        // and the position indicator; the text keeps the grid cells' flags,
        // its styles included (window_copy_update_style copies colours and
        // attributes only).
        let cols = usize::from(screen.cols).max(1);
        let mut no_palette = vec![false; screen.cells.len()];
        for row in no_palette.chunks_mut(cols) {
            let n = text.start.min(row.len());
            row[..n].fill(true);
        }
        // `s->rupper < s->rlower`: a screen of one row has no indicator.
        if m.shows_position() && screen.rows > 1 {
            let format = self.string(wtree, "copy-mode-position-format");
            let expanded = vtmux_core::format::expand_time(&format, ctx);
            if !expanded.is_empty() {
                let base = self.style(wtree, "copy-mode-position-style", ctx);
                let under = vec![
                    styled::Cell {
                        text: KEEP.into(),
                        width: 1,
                        style: base.clone(),
                    };
                    text.len()
                ];
                let drawn = styled::draw_onto(&base, under, &expanded, false, false);
                let first = drawn.cells.iter().position(|c| c.text != KEEP);
                let last = first
                    .and_then(|f| m.position_clip(text.start + f))
                    .map_or(usize::MAX, |l| l - text.start);
                for (x, c) in drawn.cells.iter().enumerate().take(last.saturating_add(1)) {
                    if c.text != KEEP {
                        let at = text.start + x;
                        let cell = theme.cell(c);
                        // On the second half of a wide character: tmux
                        // copies the cell over it with no overwrite check
                        // (format_draw, screen_write_fast_copy). Without
                        // line numbers a command's redraw writes the row
                        // as it goes, so the terminal clears the character,
                        // in the cell's background; with them every redraw
                        // draws the pane from the mode's screen
                        // (window_copy_redraw_lines, tty_draw_line), which
                        // draws the character and skips the cell.
                        if at > 0 && screen.cells[at].width == 0 && screen.cells[at - 1].width == 2
                        {
                            if text.start > 0 {
                                continue;
                            }
                            screen.cells[at - 1] = vtmux_vt::Cell {
                                width: 1,
                                style: vtmux_vt::Style {
                                    bg: cell.style.bg,
                                    ..vtmux_vt::Style::default()
                                },
                                ..vtmux_vt::Cell::default()
                            };
                        }
                        screen.cells[at] = cell;
                        if let Some(m) = no_palette.get_mut(at) {
                            *m = true;
                        }
                    }
                }
            }
        }
        m.mark_overflow(&mut screen, line_numbers, &paint);
        (screen, no_palette)
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

    /// `window_pane_get_pane_status` (window.c:2930).
    fn pane_status(&self, w: WindowId, p: PaneId, floating: bool, lines: Lines) -> PaneStatus {
        if !floating {
            return self.model.pane_status(w);
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

/// Where the active pane's cursor goes on a terminal `cols` wide showing
/// the window through `view` from screen row `top` (below status lines at
/// the top), in screen coordinates, and whether it shows
/// (`server_client_reset_state`, server-client.c:2240-2276). A cursor in the
/// view, its right and bottom edges included, is at its place there, no
/// further right than the terminal's last column (`tty_cursor`); one
/// outside it is hidden at the terminal's 0,0, the status lines not added.
#[must_use]
pub fn cursor_of(
    draw: &Draw,
    model: &Model,
    cols: u16,
    view: crate::view::View,
    top: u16,
) -> (Option<(u16, u16)>, bool) {
    let Some(p) = draw.active else {
        return (None, false);
    };
    let (Some(d), Some(pane)) = (draw.panes.get(&p), model.panes.get(&p)) else {
        return (None, false);
    };
    let Some((mut cx, cy)) = d.content.cursor else {
        return (None, false);
    };
    // The program's own screen: tmux's `s->cx` is the column count while
    // a wrap is pending (Ghostty keeps the last column and a flag), so the
    // cursor of a full line sits on the border right of the pane.
    if Rc::ptr_eq(&d.content, &pane.term.screen().shared()) {
        let (tx, ty) = pane.term.cursor();
        if ty == cy && tx > cx {
            cx = tx;
        }
    }
    let x = i64::from(pane.rect.xoff) + i64::from(cx) - i64::from(view.ox);
    let y = i64::from(pane.rect.yoff) + i64::from(cy) - i64::from(view.oy);
    let shown = |at: i64, size: u32| (0..=i64::from(size)).contains(&at);
    if !shown(x, view.sx) || !shown(y, view.sy) {
        return (Some((0, 0)), false);
    }
    let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
        return (Some((0, 0)), false);
    };
    (
        Some((x.min(cols.saturating_sub(1)), y.saturating_add(top))),
        d.content.cursor_visible,
    )
}

/// `screen_select_cell` (screen.c:641-661): the selection style over a
/// cell, its default colours the cell's own; `noattr` drops the cell's
/// attributes, else they are added to the style's.
/// window-copy.c `window_copy_update_style` (5141-5147): on the cursor's
/// line, `copy-mode-current-line-style`'s colours replace the cell's where
/// the style sets them and its attributes are added.
fn current_line_cell(style: &CellStyle, themed: vtmux_vt::Style, c: &mut vtmux_vt::Cell) {
    if style.fg != Colour::Default {
        c.style.fg = themed.fg;
    }
    if style.bg != Colour::Default {
        c.style.bg = themed.bg;
    }
    let s = &mut c.style;
    s.bold |= themed.bold;
    s.faint |= themed.faint;
    s.italic |= themed.italic;
    s.blink |= themed.blink;
    s.inverse |= themed.inverse;
    s.invisible |= themed.invisible;
    s.strikethrough |= themed.strikethrough;
    s.overline |= themed.overline;
    if themed.underline != vtmux_vt::Underline::None {
        s.underline = themed.underline;
    }
}

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
