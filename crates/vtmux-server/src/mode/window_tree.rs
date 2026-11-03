//! `choose-tree` (window-tree.c): sessions, their windows and the
//! windows' panes in a [`ModeTree`], each line the mode's format expanded
//! for its item; Enter runs the template for the chosen item, `x` and `X`
//! kill after a prompt on the mode screen, `:` runs a command for each
//! tagged item, `J`/`K` swap windows, `H` goes back to the starting pane.
//! The preview under the list shows the current item: a session's windows
//! or a window's panes side by side ([`Strip`], scrolled with `<` and
//! `>`), a pane's screen, or with `i` the item's information; the drawing
//! is `render::tree`.
//!
//! Not here yet: session groups (`-G`) and the marked pane (`m`/`M` do
//! nothing), and the mouse.

use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::keys::Key;

use super::tree::{Build, ModeTree, Sort, SortOrder};
use super::tree_mode::{Common, KeyEnv, TreeMode, rebuild};
use crate::model::Model;
use crate::open_prompt::{Line, Reply};
use crate::prompt::Kind;
use crate::prompt_history::PromptType;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `WINDOW_TREE_DEFAULT_COMMAND` (window-tree.c:38).
pub const DEFAULT_COMMAND: &str = "switch-client -Zt '%%'";

/// `WINDOW_TREE_DEFAULT_FORMAT` (window-tree.c:40-58).
pub const DEFAULT_FORMAT: &str = "#{?pane_format,\
#{?pane_marked,#[fg=thememagenta],}#{?pane_floating_flag,#[underscore],}\
#{pane_current_command}#[fg=themelightgrey]#{pane_flags}\
#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}\
,window_format,\
#{?window_marked_flag,#[fg=thememagenta],}\
#{window_name}#[fg=themelightgrey]#{window_flags}\
#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}\
,\
#[fg=themelightgrey]#{session_windows} windows\
#{?session_grouped, (group #{session_group}: #{session_group_list}),}\
#{?session_attached, (attached),}\
}";

/// `WINDOW_TREE_DEFAULT_KEY_FORMAT` (window-tree.c:60-65).
pub use super::tree_mode::DEFAULT_KEY_FORMAT;

/// `window_tree_order_seq` (window-tree.c:136-142).
const ORDERS: &[SortOrder] = &[
    SortOrder::Index,
    SortOrder::Name,
    SortOrder::Activity,
    SortOrder::Z,
];

/// What `-s`, `-w` or neither start on (`enum window_tree_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    Session,
    Window,
    Pane,
}

/// A line's item (`struct window_tree_itemdata`): a session, a window by
/// its index in the session, or a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Session(SessionId),
    Window(SessionId, u32),
    Pane(SessionId, u32, PaneId),
}

/// An item's identity across builds (tmux's tag: the session, winlink or
/// pane pointer; a winlink is a session's index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Session(SessionId),
    Window(SessionId, u32),
    Pane(PaneId),
}

/// `struct window_tree_modedata`.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "window_tree_modedata's flags"
)]
pub struct WindowTree {
    pub common: Common,
    pub tree: ModeTree<Tag, Item>,
    format: String,
    key_format: String,
    command: String,
    /// `-h`.
    hide_this_pane: bool,
    /// `-y` (`PROMPT_ACCEPT`).
    accept: bool,
    /// The target the mode started from (`data->fs`).
    start: Found,
    /// What to select on the first build; `None` after it.
    start_type: Option<Start>,
    /// `data->offset`: how far `<` and `>` scrolled the preview's strip,
    /// clamped when it is laid out ([`WindowTree::strip`]).
    offset: i64,
    /// `data->preview_is_info`: `i` shows the item's information.
    info: bool,
}

/// A session's windows or a window's panes laid out side by side in the
/// preview (`window_tree_draw_session`, `window_tree_draw_window`,
/// window-tree.c:517-806): the ones from `start` to `end` are shown, each
/// `each` columns with the last `remaining` wider, after a `<` column
/// (`left`) and before a `>` one (`right`) when more are off that side.
#[derive(Debug, PartialEq, Eq)]
pub struct Strip {
    /// Every window (its active pane) or pane, in order.
    pub items: Vec<Found>,
    /// The items are panes (`#{pane_format}`), not windows.
    pub panes: bool,
    pub start: usize,
    pub end: usize,
    pub left: bool,
    pub right: bool,
    pub each: usize,
    pub remaining: usize,
}

impl Strip {
    /// The layout of `total` items `sx` columns wide around the
    /// `current`th, `offset` clamped to what can scroll; `None` when
    /// nothing fits.
    #[must_use]
    pub fn layout(total: usize, current: usize, sx: usize, offset: &mut i64) -> Option<Self> {
        if total == 0 {
            return None;
        }
        let visible = if sx / total < 24 {
            (sx / 24).max(1)
        } else {
            total
        };
        let (mut start, mut end) = if current < visible {
            (0, visible)
        } else if current >= total - visible {
            (total - visible, total)
        } else {
            let start = current - visible / 2;
            (start, start + visible)
        };
        let lo = -i64::try_from(start).unwrap_or(0);
        let hi = i64::try_from(total - end).unwrap_or(0);
        *offset = (*offset).max(lo).min(hi);
        start = start.saturating_add_signed(isize::try_from(*offset).unwrap_or(0));
        end = end.saturating_add_signed(isize::try_from(*offset).unwrap_or(0));
        let (mut left, mut right) = (start != 0, end != total);
        if (left && right && sx <= 6) || ((left || right) && sx <= 3) {
            left = false;
            right = false;
        }
        let room = sx - 3 * (usize::from(left) + usize::from(right));
        let each = room / visible;
        if each == 0 {
            return None;
        }
        Some(Self {
            items: Vec::new(),
            panes: false,
            start,
            end,
            left,
            right,
            each,
            remaining: room - visible * each,
        })
    }
}

/// The `choose-tree` arguments the mode reads.
pub struct Options {
    pub start: Start,
    pub previews: usize,
    pub sort: Option<SortOrder>,
    pub reversed: bool,
    pub filter: Option<String>,
    pub format: Option<String>,
    pub key_format: Option<String>,
    pub command: Option<String>,
    pub hide_this_pane: bool,
    pub accept: bool,
}

/// The mode while it builds or draws: the tree's callbacks over the model.
struct View<'a> {
    model: &'a Model,
    server: &'a Server,
    mode: &'a WindowTree,
}

impl View<'_> {
    fn ctx(&self, found: Found) -> crate::format_ctx::Ctx<'_> {
        let mut ctx = crate::format_ctx::Ctx::new(
            self.model,
            &self.server.info,
            &self.server.global_env,
            found,
        );
        ctx.clients = self.server.client_views();
        ctx
    }

    /// `format_defaults(ft, NULL, s, wl, wp)` with `FORMAT_TYPE_*` as
    /// `#{pane_format}` and friends.
    fn expand(&self, item: Item, template: &str, extra: Option<(&'static str, String)>) -> String {
        let (found, ty) = match item {
            Item::Session(s) => (Found::from_session(self.model, s), "session_format"),
            Item::Window(s, i) => (Found::from_winlink(self.model, s, i), "window_format"),
            Item::Pane(s, i, p) => (
                Found {
                    pane: Some(p),
                    ..Found::from_winlink(self.model, s, i)
                },
                "pane_format",
            ),
        };
        let mut ctx = self.ctx(found);
        for name in ["session_format", "window_format", "pane_format"] {
            ctx.extra
                .push((name.into(), if name == ty { "1" } else { "0" }.into()));
        }
        if let Some((k, v)) = extra {
            ctx.extra.push((k.into(), v));
        }
        vtmux_core::format::expand_time(template, &ctx)
    }

    fn filter_pane(&self, s: SessionId, i: u32, p: PaneId, filter: Option<&str>) -> bool {
        filter
            .is_none_or(|f| vtmux_core::format::is_true(&self.expand(Item::Pane(s, i, p), f, None)))
    }

    /// `window_tree_build`: the sessions, sorted; their windows, sorted;
    /// the windows' panes that pass the filter, sorted. A window with no
    /// such pane and a session with no such window go.
    fn build(&self, b: &mut Build<Tag, Item>, sort: &Sort, filter: Option<&str>) -> Option<Tag> {
        let m = self.model;
        let mut sessions: Vec<_> = m.sessions_by_name().collect();
        if let Some(order) = sort.order.filter(|_| sessions.len() > 1) {
            if order == SortOrder::Order {
                if sort.reversed {
                    sessions.reverse();
                }
            } else {
                // sort_session_cmp.
                sessions.sort_by(|a, b| {
                    let o = match order {
                        SortOrder::Index => a.id.cmp(&b.id),
                        SortOrder::Creation => a.created.cmp(&b.created),
                        SortOrder::Activity => b.activity.cmp(&a.activity),
                        _ => std::cmp::Ordering::Equal,
                    }
                    .then_with(|| a.name.cmp(&b.name));
                    if sort.reversed { o.reverse() } else { o }
                });
            }
        }
        for s in sessions {
            let text = self.expand(Item::Session(s.id), &self.mode.format, None);
            let expanded = Some(self.mode.start_type != Some(Start::Session));
            let r = b.add(
                None,
                Tag::Session(s.id),
                Item::Session(s.id),
                s.name.clone(),
                Some(text),
                expanded,
            );
            let mut links: Vec<(u32, WindowId)> = s.windows.iter().map(|(i, w)| (*i, *w)).collect();
            sort_links(m, &mut links, sort);
            let mut any = false;
            for (i, w) in links {
                any |= self.build_window(b, s.id, i, w, r, sort, filter);
            }
            if !any {
                b.remove(r);
            }
        }
        let st = self.mode.start;
        match self.mode.start_type? {
            Start::Session => st.session.map(Tag::Session),
            Start::Window => Some(Tag::Window(st.session?, st.index?)),
            Start::Pane => {
                let w = st.window?;
                if m.windows.get(&w)?.panes.panes.len() == 1 {
                    Some(Tag::Window(st.session?, st.index?))
                } else {
                    st.pane.map(Tag::Pane)
                }
            }
        }
    }

    /// `window_tree_build_window`.
    #[allow(clippy::too_many_arguments)]
    fn build_window(
        &self,
        build: &mut Build<Tag, Item>,
        s: SessionId,
        i: u32,
        w: WindowId,
        parent: usize,
        sort: &Sort,
        filter: Option<&str>,
    ) -> bool {
        let m = self.model;
        let Some(window) = m.windows.get(&w) else {
            return false;
        };
        let text = self.expand(Item::Window(s, i), &self.mode.format, None);
        let expanded = Some(!matches!(
            self.mode.start_type,
            Some(Start::Session | Start::Window)
        ));
        let id = build.add(
            Some(parent),
            Tag::Window(s, i),
            Item::Window(s, i),
            i.to_string(),
            Some(text),
            expanded,
        );
        build.set_align(id, 1);
        let mut panes = window.panes.panes.clone();
        sort_panes(m, &mut panes, sort, self.mode.common.pane);
        let mut found = 0;
        for p in panes {
            if !self.filter_pane(s, i, p, filter) {
                continue;
            }
            found += 1;
            if self.mode.hide_this_pane && p == self.mode.common.pane {
                continue;
            }
            let item = Item::Pane(s, i, p);
            let text = self.expand(item, &self.mode.format, None);
            let name = m.pane_index(p).unwrap_or(0).to_string();
            let pid = build.add(Some(id), Tag::Pane(p), item, name, Some(text), None);
            build.set_align(pid, 1);
        }
        if found == 0 {
            build.remove(id);
            return false;
        }
        true
    }

    /// `window_tree_get_key`: the key format for the line.
    fn key(&self, item: &Item, line: usize) -> Option<Key> {
        let ty = match item {
            Item::Session(s) => Item::Session(*s),
            Item::Window(s, i) => Item::Window(*s, *i),
            Item::Pane(s, i, p) => Item::Pane(*s, *i, *p),
        };
        let text = self.expand(ty, &self.mode.key_format, Some(("line", line.to_string())));
        Key::from_name(&text)
    }

    /// `window_tree_search`: a session's or window's name, a pane's
    /// foreground command.
    fn matches(&self, item: &Item, search: &str, icase: bool) -> bool {
        let m = self.model;
        let hay = match *item {
            Item::Session(s) => m.sessions.get(&s).map(|x| x.name.clone()),
            Item::Window(s, i) => m
                .sessions
                .get(&s)
                .and_then(|x| x.windows.get(&i))
                .and_then(|w| m.windows.get(w))
                .map(|w| w.name.clone()),
            Item::Pane(_, _, p) => m.panes.get(&p).and_then(|x| x.io.as_ref()).and_then(|io| {
                crate::sys::pty::foreground_name(&io.owned().try_clone_to_owned().ok()?)
            }),
        };
        hay.filter(|h| !h.is_empty())
            .is_some_and(|h| super::tree::contains(&h, search, icase))
    }
}

/// `sort_winlink_cmp` without `-r`.
fn winlink_cmp(
    m: &Model,
    order: SortOrder,
    a: (u32, WindowId),
    b: (u32, WindowId),
) -> std::cmp::Ordering {
    let (wa, wb) = (&m.windows[&a.1], &m.windows[&b.1]);
    match order {
        SortOrder::Index => a.0.cmp(&b.0),
        SortOrder::Creation => wa.id.cmp(&wb.id),
        SortOrder::Activity => wb.activity.cmp(&wa.activity),
        SortOrder::Name => wa.name.cmp(&wb.name),
        SortOrder::Size => (u64::from(wa.sx()) * u64::from(wa.sy()))
            .cmp(&(u64::from(wb.sx()) * u64::from(wb.sy()))),
        SortOrder::Modifier | SortOrder::Order | SortOrder::Z => std::cmp::Ordering::Equal,
    }
    .then_with(|| wa.name.cmp(&wb.name))
}

/// `sort_qsort` with `sort_winlink_cmp` over one session's windows.
fn sort_links(m: &Model, links: &mut [(u32, WindowId)], sort: &Sort) {
    let Some(order) = sort.order.filter(|_| links.len() > 1) else {
        return;
    };
    if order == SortOrder::Order {
        if sort.reversed {
            links.reverse();
        }
        return;
    }
    links.sort_by(|a, b| {
        let o = winlink_cmp(m, order, *a, *b);
        if sort.reversed { o.reverse() } else { o }
    });
}

/// `sort_pane_cmp` over one window's panes; `own` is the tree mode's
/// pane, out of the pane while the mode builds.
fn sort_panes(m: &Model, panes: &mut [PaneId], sort: &Sort, own: PaneId) {
    let Some(order) = sort.order.filter(|_| panes.len() > 1) else {
        return;
    };
    if order == SortOrder::Order {
        if sort.reversed {
            panes.reverse();
        }
        return;
    }
    // `wp->screen->title`: a pane in a mode shows the mode's screen,
    // whose title is empty (screen_init).
    let title = |p: &PaneId| {
        let pane = &m.panes[p];
        if pane.modes.is_empty() && *p != own {
            pane.term.title()
        } else {
            String::new()
        }
    };
    panes.sort_by(|a, b| {
        let (pa, pb) = (&m.panes[a], &m.panes[b]);
        let o = match order {
            SortOrder::Activity => pa.active_point.cmp(&pb.active_point),
            SortOrder::Creation => a.cmp(b),
            SortOrder::Size => (u64::from(pa.rect.sx) * u64::from(pa.rect.sy))
                .cmp(&(u64::from(pb.rect.sx) * u64::from(pb.rect.sy))),
            SortOrder::Index => m.pane_index(*a).cmp(&m.pane_index(*b)),
            SortOrder::Name => title(a).cmp(&title(b)),
            SortOrder::Z => m.pane_zindex(*a).cmp(&m.pane_zindex(*b)),
            SortOrder::Modifier | SortOrder::Order => std::cmp::Ordering::Equal,
        }
        .then_with(|| title(a).cmp(&title(b)));
        if sort.reversed { o.reverse() } else { o }
    });
}

impl WindowTree {
    /// `window_tree_init`.
    #[must_use]
    pub fn new(pane: PaneId, size: (usize, usize), start: Found, o: Options) -> Self {
        let sort = Sort {
            order: o.sort,
            reversed: o.reversed,
            seq: ORDERS,
        };
        let mut tree = ModeTree::new(size, o.previews, sort, o.filter);
        // window_tree_sort: no order is the first of the sequence.
        if tree.sort.order.is_none() {
            tree.sort.order = Some(ORDERS[0]);
        }
        tree.view_name = Some("preview");
        Self {
            common: Common::new(pane),
            tree,
            format: o.format.unwrap_or_else(|| DEFAULT_FORMAT.to_owned()),
            key_format: o
                .key_format
                .unwrap_or_else(|| DEFAULT_KEY_FORMAT.to_owned()),
            command: o.command.unwrap_or_else(|| DEFAULT_COMMAND.to_owned()),
            hide_this_pane: o.hide_this_pane,
            accept: o.accept,
            start,
            start_type: Some(o.start),
            offset: 0,
            info: false,
        }
    }

    /// The pane the mode is on.
    #[must_use]
    pub fn pane(&self) -> PaneId {
        self.common.pane
    }

    /// `data->preview_is_info`.
    #[must_use]
    pub fn info(&self) -> bool {
        self.info
    }

    /// The pane `-h` keeps out of the previews.
    #[must_use]
    pub fn hidden_pane(&self) -> Option<PaneId> {
        self.hide_this_pane.then_some(self.common.pane)
    }

    /// The strip the preview of `item` shows in `sx` columns, with the
    /// offset clamped as drawing it does; `None` for a pane or an item
    /// that is gone.
    #[must_use]
    pub fn strip(&self, model: &Model, item: Item, sx: usize, offset: &mut i64) -> Option<Strip> {
        let (items, current, panes) = match item {
            Item::Session(s) => {
                let session = model.sessions.get(&s)?;
                let items: Vec<Found> = session
                    .windows
                    .keys()
                    .map(|&i| Found::from_winlink(model, s, i))
                    .collect();
                let current = session
                    .windows
                    .keys()
                    .position(|&i| i == session.current)
                    .unwrap_or(items.len());
                (items, current, false)
            }
            Item::Window(s, i) => {
                let link = Found::from_winlink(model, s, i);
                let w = model.windows.get(&link.window?)?;
                let shown: Vec<PaneId> = w
                    .panes
                    .panes
                    .iter()
                    .copied()
                    .filter(|p| self.hidden_pane() != Some(*p))
                    .collect();
                let current = shown
                    .iter()
                    .position(|p| Some(*p) == w.panes.active)
                    .unwrap_or(shown.len());
                let items = shown
                    .into_iter()
                    .map(|p| Found {
                        pane: Some(p),
                        ..link
                    })
                    .collect();
                (items, current, true)
            }
            Item::Pane(..) => return None,
        };
        let mut strip = Strip::layout(items.len(), current, sx, offset)?;
        strip.items = items;
        strip.panes = panes;
        Some(strip)
    }

    /// The offset as drawn now: clamped by the strip the preview shows,
    /// if it shows one (tmux clamps `data->offset` while drawing).
    #[must_use]
    pub fn offset(&self, model: &Model) -> i64 {
        let mut offset = self.offset;
        if !self.info
            && let Some((sx, _)) = self.tree.preview_size()
            && let Some(item) = self.tree.current_item()
        {
            let _ = self.strip(model, item.data, sx, &mut offset);
        }
        offset
    }

    /// `mode_tree_build` over `server`'s model, the start type used up
    /// after the first.
    pub fn build(&mut self, server: &Server) {
        rebuild(self, server);
    }
}

impl TreeMode for WindowTree {
    type Tag = Tag;
    type Data = Item;
    const HELP: &'static [(&'static str, &'static str)] = &[
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
    /// `window_tree_help`'s width.
    const HELP_WIDTH: usize = 51;
    const HELP_ITEM: &'static str = "item";

    fn tree(&self) -> &ModeTree<Tag, Item> {
        &self.tree
    }
    fn tree_mut(&mut self) -> &mut ModeTree<Tag, Item> {
        &mut self.tree
    }
    fn common(&self) -> &Common {
        &self.common
    }
    fn common_mut(&mut self) -> &mut Common {
        &mut self.common
    }
    fn of(mode: &crate::mode::Mode) -> Option<&Self> {
        match mode {
            crate::mode::Mode::Tree(t) => Some(t),
            _ => None,
        }
    }
    fn into_mode(self: Box<Self>) -> crate::mode::Mode {
        crate::mode::Mode::Tree(self)
    }
    fn from_mode(mode: crate::mode::Mode) -> Option<Box<Self>> {
        match mode {
            crate::mode::Mode::Tree(t) => Some(t),
            _ => None,
        }
    }

    fn build_items(
        &self,
        server: &Server,
        b: &mut Build<Tag, Item>,
        sort: &Sort,
        filter: Option<&str>,
    ) -> Option<Tag> {
        View {
            model: &server.model,
            server,
            mode: self,
        }
        .build(b, sort, filter)
    }

    fn item_key(&self, server: &Server, data: &Item, line: usize) -> Option<Key> {
        View {
            model: &server.model,
            server,
            mode: self,
        }
        .key(data, line)
    }

    fn matches(&self, server: &Server, data: &Item, search: &str, icase: bool) -> bool {
        View {
            model: &server.model,
            server,
            mode: self,
        }
        .matches(data, search, icase)
    }

    /// The start type is used up by the first build.
    fn built(&mut self) {
        self.start_type = None;
    }

    /// `window_tree_key`: another current item scrolls the preview back (a
    /// build makes new items, so it counts).
    fn moved(&mut self) {
        self.offset = 0;
    }

    fn swap(server: &mut Server, p: PaneId, direction: isize) {
        server.tree_swap(p, direction);
    }

    fn mode_key(
        server: &mut Server,
        p: PaneId,
        c: Option<ClientId>,
        key: Key,
        item: Option<Item>,
    ) -> bool {
        server.tree_mode_key(p, c, key, item)
    }

    fn draw_preview(
        &self,
        source: &crate::render::window::Source<'_>,
        canvas: &mut crate::render::tree::Canvas,
        data: &Item,
        at: (usize, usize),
        size: (usize, usize),
    ) {
        source.tree_preview(canvas, self, *data, at, size);
    }
}

/// Which window-tree prompt is open (`mode_tree_set_prompt`'s callbacks).
#[derive(Debug, Clone, Copy)]
enum Ask {
    KillCurrent,
    KillTagged,
    Command,
}

#[derive(Debug)]
struct TreePrompt(Ask);

impl crate::mode::prompt::ModePromptOwner for TreePrompt {
    fn input(&mut self, server: &mut Server, p: PaneId, c: Option<ClientId>, line: Line) -> Reply {
        let (value, done) = match line {
            Line::Done(v) => (v, true),
            Line::Changed(v) => (Some(v), false),
            Line::Move => return Reply::Continue,
        };
        server.tree_prompt_input(p, c, self.0, value.as_deref());
        if done { Reply::Close } else { Reply::Continue }
    }
}

impl Server {
    /// The keys window-tree handles itself (window-tree.c:1471-1567);
    /// `true` when the mode is finished.
    pub(super) fn tree_mode_key(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        k: Key,
        item: Option<Item>,
    ) -> bool {
        let Some(item) = item else { return false };
        let ch = k
            .key_and_modifiers()
            .as_char()
            .filter(|_| !k.has_ctrl() && !k.has_meta());
        match ch {
            Some(c @ ('<' | '>' | 'i')) => self.tree_preview_key(p, c),
            Some('H') => self.tree_home(p),
            Some('x') => {
                let model = &self.model;
                let label = match item {
                    Item::Session(s) => model
                        .sessions
                        .get(&s)
                        .map(|x| format!("Kill session {}? ", x.name)),
                    Item::Window(s, index) => model
                        .sessions
                        .get(&s)
                        .filter(|x| x.windows.contains_key(&index))
                        .map(|_| format!("Kill window {index}? ")),
                    Item::Pane(_, _, q) => model.pane_index(q).map(|n| format!("Kill pane {n}? ")),
                };
                if let Some(label) = label {
                    self.window_tree_prompt(p, c, &label, Kind::Single, Ask::KillCurrent);
                }
            }
            Some('X') => {
                let n = self
                    .with_tree::<WindowTree, _>(p, |_, t| t.tree.count_tagged())
                    .unwrap_or(0);
                if n != 0 {
                    let label = format!("Kill {n} tagged? ");
                    self.window_tree_prompt(p, c, &label, Kind::Single, Ask::KillTagged);
                }
            }
            Some(':') => {
                let n = self
                    .with_tree::<WindowTree, _>(p, |_, t| t.tree.count_tagged())
                    .unwrap_or(0);
                let label = if n == 0 {
                    "(current) ".to_owned()
                } else {
                    format!("({n} tagged) ")
                };
                self.window_tree_prompt(p, c, &label, Kind::Line, Ask::Command);
            }
            Some('\r') => {
                if let Some(name) = self.tree_target(item) {
                    let command = self
                        .with_tree::<WindowTree, _>(p, |_, t| t.command.clone())
                        .unwrap_or_default();
                    self.tree_run_command(c, None, &command, &name);
                }
                return true;
            }
            _ => {}
        }
        false
    }

    /// `H`: the starting session and window expanded, the starting pane
    /// selected, else its window.
    fn tree_home(&mut self, p: PaneId) {
        self.with_tree::<WindowTree, _>(p, |server, t| {
            let st = t.start;
            let mut tree = std::mem::take(&mut t.tree);
            {
                let mut env = KeyEnv { server, mode: &*t };
                if let Some(s) = st.session {
                    tree.expand(&mut env, Tag::Session(s));
                }
                let wl = st
                    .session
                    .zip(st.index)
                    .map(|(s, index)| Tag::Window(s, index));
                if let Some(wl) = wl {
                    tree.expand(&mut env, wl);
                }
                if !tree.set_current(Tag::Pane(t.common.pane))
                    && let Some(wl) = wl
                {
                    tree.set_current(wl);
                }
            }
            t.tree = tree;
        });
    }

    /// The preview's keys: `<` and `>` scroll its strip from where it is
    /// drawn, `i` toggles the information view.
    fn tree_preview_key(&mut self, p: PaneId, c: char) {
        self.with_tree::<WindowTree, _>(p, |server, t| {
            if c == 'i' {
                t.info = !t.info;
                t.tree.view_name = Some(if t.info { "info" } else { "preview" });
            } else {
                t.offset = t.offset(&server.model) + if c == '<' { -1 } else { 1 };
            }
        });
    }

    /// `mode_tree_set_prompt` for window tree's own prompts; `-y` accepts
    /// a single-key prompt at once with `y` (`PROMPT_ACCEPT`,
    /// `mode_tree_prompt_accept`).
    fn window_tree_prompt(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        label: &str,
        kind: Kind,
        ask: Ask,
    ) {
        self.tree_prompt(
            p,
            c,
            label,
            "",
            (PromptType::Command, kind),
            Box::new(TreePrompt(ask)),
        );
        let accept = self
            .with_tree::<WindowTree, _>(p, |_, t| t.accept)
            .unwrap_or(false);
        if accept
            && matches!(kind, Kind::Single)
            && let Some(c) = c
        {
            self.mode_prompt_key(p, Some(c), Key::from_char('y'));
        }
    }

    /// The prompts' callbacks (`window_tree_kill_*_callback`,
    /// `window_tree_command_callback`).
    fn tree_prompt_input(&mut self, p: PaneId, c: Option<ClientId>, ask: Ask, value: Option<&str>) {
        match ask {
            Ask::KillCurrent | Ask::KillTagged => {
                let Some(v) = value.filter(|v| !v.is_empty()) else {
                    return;
                };
                let mut chars = v.chars();
                if !chars.next().is_some_and(|ch| ch.eq_ignore_ascii_case(&'y'))
                    || chars.next().is_some()
                {
                    return;
                }
                let items: Vec<Item> = self
                    .with_tree::<WindowTree, _>(p, |_, t| {
                        if matches!(ask, Ask::KillCurrent) {
                            t.tree.current_item().map(|i| i.data).into_iter().collect()
                        } else {
                            t.tree.tagged_or_current().into_iter().copied().collect()
                        }
                    })
                    .unwrap_or_default();
                for item in items {
                    self.tree_kill(item);
                }
                // server_renumber_all.
                let sessions: Vec<SessionId> = self.model.sessions.keys().copied().collect();
                for s in sessions {
                    self.model.renumber_if_set(s);
                }
                self.recalculate_sizes();
                self.tree_update(p);
            }
            Ask::Command => {
                let Some(v) = value.filter(|v| !v.is_empty()) else {
                    return;
                };
                let items: Vec<Item> = self
                    .with_tree::<WindowTree, _>(p, |_, t| {
                        t.tree.tagged_or_current().into_iter().copied().collect()
                    })
                    .unwrap_or_default();
                for item in items {
                    if let Some(name) = self.tree_target(item) {
                        let fs = self.tree_found(item);
                        self.tree_run_command(c, fs, v, &name);
                    }
                }
                self.cmdq_process();
                self.tree_update(p);
            }
        }
    }

    /// `window_tree_kill_each`.
    fn tree_kill(&mut self, item: Item) {
        match item {
            Item::Session(s) => {
                if self.model.sessions.contains_key(&s) {
                    self.destroy_session(s);
                }
            }
            Item::Window(s, i) => {
                if let Some(&w) = self.model.sessions.get(&s).and_then(|x| x.windows.get(&i)) {
                    self.kill_window(w, false);
                }
            }
            Item::Pane(_, _, p) => {
                if self.model.panes.contains_key(&p) {
                    self.destroy_pane(p);
                }
            }
        }
    }

    /// The item's target in the model now (`window_tree_pull_item`).
    fn tree_found(&self, item: Item) -> Option<Found> {
        let model = &self.model;
        let found = match item {
            Item::Session(s) => Found::from_session(model, s),
            Item::Window(s, index) => Found::from_winlink(model, s, index),
            Item::Pane(s, index, pane) => {
                let link = Found::from_winlink(model, s, index);
                if model
                    .panes
                    .get(&pane)
                    .is_none_or(|x| Some(x.window) != link.window)
                {
                    return None;
                }
                Found {
                    pane: Some(pane),
                    ..link
                }
            }
        };
        found.session.is_some().then_some(found)
    }

    /// `window_tree_get_target`: `=s:`, `=s:i.` or `=s:i.%p`.
    fn tree_target(&self, item: Item) -> Option<String> {
        let f = self.tree_found(item)?;
        let name = &self.model.sessions.get(&f.session?)?.name;
        Some(match item {
            Item::Session(_) => format!("={name}:"),
            Item::Window(_, i) => {
                f.window?;
                format!("={name}:{i}.")
            }
            Item::Pane(_, i, p) => format!("={name}:{i}.%{}", p.0),
        })
    }

    /// `mode_tree_swap` with `window_tree_swap`: two windows of one
    /// session trade places, unless the sort would undo it.
    pub(super) fn tree_swap(&mut self, p: PaneId, direction: isize) {
        let Some(Some((cur, other, sort))) = self.with_tree::<WindowTree, _>(p, |_, t| {
            let at = t.tree.swap_partner(direction)?;
            let cur = t.tree.current_item()?.data;
            let other = t.tree.item(t.tree.lines()[at].item).data;
            Some((cur, other, t.tree.sort))
        }) else {
            return;
        };
        let (Item::Window(s1, i1), Item::Window(s2, i2)) = (cur, other) else {
            return;
        };
        if s1 != s2 {
            return;
        }
        let m = &mut self.model;
        let Some(session) = m.sessions.get(&s1) else {
            return;
        };
        let (Some(&w1), Some(&w2)) = (session.windows.get(&i1), session.windows.get(&i2)) else {
            return;
        };
        // sort_would_window_tree_swap: an order other than index that
        // tells the two apart would put them back.
        if let Some(order) = sort.order.filter(|o| *o != SortOrder::Index)
            && winlink_cmp(m, order, (i1, w1), (i2, w2)) != std::cmp::Ordering::Equal
        {
            return;
        }
        let current = session.current;
        let (st1, st2) = (m.next_link_stamp(), m.next_link_stamp());
        let session = m.sessions.get_mut(&s1).expect("checked");
        session.windows.insert(i1, w2);
        session.windows.insert(i2, w1);
        session.link_seq.insert(i2, st1);
        session.link_seq.insert(i1, st2);
        session.sync_link_tree();
        // session_set_current to the other winlink: the current window
        // stays current.
        if current == i1 {
            m.set_current(s1, i2);
        } else if current == i2 {
            m.set_current(s1, i1);
        }
        m.redraw(crate::model::Redraw::Session(s1));
        self.recalculate_sizes();
        self.with_tree::<WindowTree, _>(p, |server, t| {
            if let Some(at) = t.tree.swap_partner(direction) {
                t.tree.current = at;
            }
            t.build(server);
        });
    }
}

/// `-O`: `sort_order_from_string`, an error for an unknown order.
///
/// # Errors
/// `invalid sort order`.
pub fn sort_option(s: Option<&str>) -> Result<Option<SortOrder>, vtmux_core::error::EdgeError> {
    match s {
        None => Ok(None),
        Some(s) => SortOrder::parse(s)
            .map(Some)
            .ok_or(vtmux_core::error::EdgeError::InvalidSortOrder),
    }
}

#[cfg(test)]
mod tests {
    use super::Strip;

    /// (start, end, left, right, each, remaining, offset after clamping).
    fn layout(
        total: usize,
        current: usize,
        sx: usize,
        offset: i64,
    ) -> (usize, usize, bool, bool, usize, usize, i64) {
        let mut offset = offset;
        let s = Strip::layout(total, current, sx, &mut offset).expect("fits");
        (s.start, s.end, s.left, s.right, s.each, s.remaining, offset)
    }

    /// `window_tree_draw_session`/`_window` (window-tree.c:534-585), with
    /// the numbers the reference drew in `tree_preview_layouts` and
    /// `tree_preview_narrow`: as many 24-column items as fit around the
    /// current one, `<`/`>` columns for those off a side, the offset
    /// clamped to what can scroll.
    #[test]
    fn strip_layout() {
        // 4 windows in 76 columns: 3 visible, the first current.
        assert_eq!(layout(4, 0, 76, 0), (0, 3, false, true, 24, 1, 0));
        // > scrolls one; a second > is clamped.
        assert_eq!(layout(4, 0, 76, 1), (1, 4, true, false, 24, 1, 1));
        assert_eq!(layout(4, 0, 76, 2), (1, 4, true, false, 24, 1, 1));
        assert_eq!(layout(4, 0, 76, -3), (0, 3, false, true, 24, 1, 0));
        // Everything fits: no arrows, the last one wider.
        assert_eq!(layout(3, 2, 76, 0), (0, 3, false, false, 25, 1, 0));
        // 5 windows in 46 columns, the middle current: one visible.
        assert_eq!(layout(5, 2, 46, 0), (2, 3, true, true, 40, 0, 0));
        assert_eq!(layout(5, 4, 46, 0), (4, 5, true, false, 43, 0, 0));
        // Too narrow for arrows.
        assert_eq!(layout(3, 1, 6, 0), (1, 2, false, false, 6, 0, 0));
        assert!(Strip::layout(0, 0, 76, &mut 0).is_none());
    }
}
