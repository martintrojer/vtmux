//! `choose-client` (window-client.c): the attached clients in a
//! [`ModeTree`], each line the mode's format expanded for its client, the
//! preview the client's current pane and its status line, or with `i` its
//! information. Enter runs the template for the current client (by
//! default detaching it); `d`/`x` detach the current client, `D`/`X` the
//! tagged ones (`x` and `X` with `MSG_DETACHKILL`). The mode ends when no
//! client is left.
//!
//! Not here yet: `z`/`Z` (`server_client_suspend`: vtmux has no
//! suspend-client), the `#{I/f:...}` and `#{I/c:...}` modifiers the
//! information lines use, and the mouse menu.

use vtmux_core::ids::PaneId;
use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;
use vtmux_core::style::{Attrs, CellStyle};
use vtmux_core::styled;

use super::tree::{Build, ModeTree, Sort, SortOrder};
use super::tree_mode::{Common, DEFAULT_KEY_FORMAT, TreeMode, rebuild};
use crate::format_ctx::ClientView;
use crate::render::tree::Canvas;
use crate::render::window::Source;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `WINDOW_CLIENT_DEFAULT_COMMAND`.
pub const DEFAULT_COMMAND: &str = "detach-client -t '%%'";

/// `WINDOW_CLIENT_DEFAULT_FORMAT`.
pub const DEFAULT_FORMAT: &str =
    "#[fg=themelightgrey]#{t/p:client_activity}: session #[default]#{session_name}";

/// `window_client_order_seq`.
const ORDERS: &[SortOrder] = &[
    SortOrder::Name,
    SortOrder::Size,
    SortOrder::Creation,
    SortOrder::Activity,
];

/// `WINDOW_CLIENT_FEATURE`.
fn feature(f: &str) -> String {
    format!(
        "#{{?#{{I/f:{f}}},#[fg=themegreen],#[fg=themelightgrey]}}#{{p/15:#{{l:{f}}}}}#[default]"
    )
}

/// `window_client_info_lines`.
fn info_lines() -> Vec<String> {
    let line = |name: &str, value: &str| {
        format!(
            "#[fg=themelightgrey]{name:<14}#[#{{E:tree-mode-border-style}},acs]x#[default] {value}"
        )
    };
    let features = |list: &[&str]| {
        list.iter()
            .map(|f| feature(f))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let more = |list: &[&str]| {
        format!(
            "              #[#{{E:tree-mode-border-style}},acs]x#[default] {}",
            features(list)
        )
    };
    vec![
        line(
            "Client Name",
            "#{client_name} #[fg=themelightgrey]#[fg=themelightgrey](PID #{client_pid})#[default]",
        ),
        line("Session", "#{session_name}"),
        line(
            "Attach Time",
            "#{t:client_created} #[fg=themelightgrey](#{t/r:client_created})#[default]",
        ),
        line(
            "Activity Time",
            "#{t:client_activity} #[fg=themelightgrey](#{t/r:client_activity})#[default]",
        ),
        line(
            "Terminal Type",
            "#{?client_termtype,#{client_termtype},Unknown}",
        ),
        line("TERM", "#{client_termname}"),
        line(
            "Size",
            "#{client_width}x#{client_height} #[fg=themelightgrey](cell #{client_cell_width}x#{client_cell_height})#[default]",
        ),
        line(
            "Bytes Written",
            "#{client_written} #[fg=themelightgrey](#{client_discarded} discarded)#[default]",
        ),
        line("Features", &features(&["256", "RGB", "bpaste", "ccolour"])),
        more(&["clipboard", "cstyle", "extkeys", "focus"]),
        more(&["hyperlinks", "ignorefkeys", "margins", "mouse"]),
        more(&["osc7", "overline", "progressbar", "rectfill"]),
        more(&["sixel", "strikethrough", "sync", "title"]),
        more(&["usstyle", "utf8"]),
        "#[#{E:tree-mode-border-style},acs]qqqqqqqqqqqqqqn#{R:q,#{window_width}}#[default]"
            .to_owned(),
        line("prefix", "#{prefix}"),
        line(
            "mouse",
            "#{?mouse,#{?#{I/c:kmous},,#[fg=themered]}on,#[fg=themelightgrey]off} \
#{?#{I/c:kmous},,#[align=right]unavailable: [kmous] missing}",
        ),
        line(
            "set-clipboard",
            "#{?#{!=:#{set-clipboard},off},#{?#{I/c:Ms},,#[fg=themered]}#{set-clipboard},#[fg=themelightgrey]off} \
#{?#{I/c:Ms},,#[align=right]unavailable: [Ms] #{?clipboard_invalid,invalid,missing}}",
        ),
        line(
            "get-clipboard",
            "#{?#{!=:#{get-clipboard},off},#{?#{I/c:Ms},,#[fg=themered]}#{get-clipboard},#[fg=themelightgrey]off} \
#{?#{I/c:Ms},,#[align=right]unavailable: [Ms] #{?clipboard_invalid,invalid,missing}}",
        ),
        line(
            "focus-events",
            "#{?focus-events,#{?#{I/f:focus},,#[fg=themered]}on,#[fg=themelightgrey]off} \
#{?#{I/f:focus},,#[align=right]unavailable: [Enfcs] or [Dcfcs] missing}",
        ),
        line(
            "extended-keys",
            "#{?#{!=:#{extended-keys},off},#{?#{I/f:extkeys},,#[fg=themered]}#{extended-keys},#[fg=themelightgrey]off} \
#{?#{I/f:extkeys},,#[align=right]unavailable: [Eneks] or [Dseks] missing}",
        ),
        line("set-titles", "#{?set-titles,on,#[fg=themelightgrey]off}"),
        line("escape-time", "#{escape-time} ms"),
    ]
}

/// A line's client (`struct window_client_itemdata`): the client and its
/// name (`item->ttyname` is the name for a tty client).
#[derive(Clone, Debug)]
pub struct Item {
    pub id: ClientId,
    pub name: String,
}

/// A client's status lines as last drawn (`c->status.screen`, as many as
/// `status_line_size`) and whether they are at the top
/// (`status_at_line`).
#[derive(Clone, Debug, Default)]
pub struct Status {
    rows: Vec<Vec<styled::Cell>>,
    top: bool,
}

/// The `choose-client` arguments the mode reads.
pub struct Options {
    pub previews: usize,
    pub sort: Option<SortOrder>,
    pub reversed: bool,
    pub filter: Option<String>,
    pub format: Option<String>,
    pub key_format: Option<String>,
    pub command: Option<String>,
    pub hide_this_pane: bool,
    pub info: bool,
}

/// `struct window_client_modedata`.
#[derive(Clone, Debug)]
pub struct ClientMode {
    pub common: Common,
    pub tree: ModeTree<ClientId, Item>,
    format: String,
    key_format: String,
    command: String,
    /// `-h`.
    hide_this_pane: bool,
    /// `data->preview_is_info`: `-i`, toggled by `i`.
    info: bool,
    /// The clients' status lines when the mode is next drawn
    /// ([`ClientMode::set_statuses`]).
    statuses: std::collections::BTreeMap<ClientId, Status>,
}

/// `sort_get_clients` (sort.c:452-476) with `sort_client_cmp`.
fn sorted(views: &[ClientView], server: &Server, sort: &Sort) -> Vec<ClientView> {
    let mut list: Vec<ClientView> = views
        .iter()
        .filter(|v| {
            server
                .clients
                .get(&ClientId(v.order as usize))
                .is_some_and(|c| !c.closing)
        })
        .cloned()
        .collect();
    let Some(order) = sort.order.filter(|_| list.len() > 1) else {
        return list;
    };
    if order == SortOrder::Order {
        if sort.reversed {
            list.reverse();
        }
        return list;
    }
    list.sort_by(|a, b| {
        let o = match order {
            SortOrder::Name => a.name.cmp(&b.name),
            SortOrder::Size => {
                (a.width, a.height.unwrap_or(0)).cmp(&(b.width, b.height.unwrap_or(0)))
            }
            SortOrder::Creation => a.order.cmp(&b.order),
            SortOrder::Activity => b.activity.cmp(&a.activity),
            _ => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.name.cmp(&b.name));
        if sort.reversed { o.reverse() } else { o }
    });
    list
}

impl ClientMode {
    /// `window_client_init` up to `mode_tree_build`.
    #[must_use]
    pub fn new(pane: PaneId, size: (usize, usize), o: Options) -> Self {
        let sort = Sort {
            order: o.sort,
            reversed: o.reversed,
            seq: ORDERS,
        };
        let mut tree = ModeTree::new(size, o.previews, sort, o.filter);
        // window_client_sort: no order is the first of the sequence.
        if tree.sort.order.is_none() {
            tree.sort.order = Some(ORDERS[0]);
        }
        tree.view_name = Some(if o.info { "info" } else { "preview" });
        Self {
            common: Common::new(pane),
            tree,
            format: o.format.unwrap_or_else(|| DEFAULT_FORMAT.to_owned()),
            key_format: o
                .key_format
                .unwrap_or_else(|| DEFAULT_KEY_FORMAT.to_owned()),
            command: o.command.unwrap_or_else(|| DEFAULT_COMMAND.to_owned()),
            hide_this_pane: o.hide_this_pane,
            info: o.info,
            statuses: std::collections::BTreeMap::new(),
        }
    }

    pub fn build(&mut self, server: &Server) {
        rebuild(self, server);
    }

    /// The clients' status lines as they are now, for the next draw
    /// (`window_client_draw` reads `c->status.screen` as it draws).
    pub(crate) fn set_statuses(&mut self, server: &Server) {
        self.statuses = server
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                let s = c.attached?;
                let lines = server.status_line_size(c) as usize;
                let rows = c.status.screen_rows();
                let rows = rows[..lines.min(rows.len())].to_vec();
                let top = server.model.status_top(TreeId::Session(s));
                Some((id, Status { rows, top }))
            })
            .collect();
    }
}

/// `format_single(NULL, fmt, c, NULL, NULL, NULL)` and friends: the
/// client's formats, its session's current window and pane.
fn client_ctx<'a>(source: &Source<'a>, view: &ClientView) -> crate::format_ctx::Ctx<'a> {
    let found = view
        .session
        .map_or_else(Found::default, |s| Found::from_session(source.model, s));
    let mut ctx = crate::format_ctx::Ctx::new(source.model, source.info, source.env, found);
    ctx.clients = std::rc::Rc::clone(&source.clients);
    ctx.client = Some(view.clone());
    ctx
}

impl TreeMode for ClientMode {
    type Tag = ClientId;
    type Data = Item;
    const HELP: &'static [(&'static str, &'static str)] = &[
        ("          i", "Toggle info view"),
        ("      Enter", "Choose selected %1"),
        ("          d", "Detach selected %1"),
        ("          D", "Detach tagged %1s"),
        ("          x", "Detach selected %1"),
        ("          X", "Detach tagged %1s"),
        ("          z", "Suspend selected %1"),
        ("          Z", "Suspend tagged %1s"),
        ("          f", "Enter a filter"),
    ];
    const HELP_WIDTH: usize = 0;
    const HELP_ITEM: &'static str = "client";

    fn tree(&self) -> &ModeTree<ClientId, Item> {
        &self.tree
    }
    fn tree_mut(&mut self) -> &mut ModeTree<ClientId, Item> {
        &mut self.tree
    }
    fn common(&self) -> &Common {
        &self.common
    }
    fn common_mut(&mut self) -> &mut Common {
        &mut self.common
    }
    fn of(mode: &super::Mode) -> Option<&Self> {
        match mode {
            super::Mode::Client(t) => Some(t),
            _ => None,
        }
    }
    fn into_mode(self: Box<Self>) -> super::Mode {
        super::Mode::Client(self)
    }
    fn from_mode(mode: super::Mode) -> Option<Box<Self>> {
        match mode {
            super::Mode::Client(t) => Some(t),
            _ => None,
        }
    }

    /// `window_client_build`.
    fn build_items(
        &self,
        server: &Server,
        b: &mut Build<ClientId, Item>,
        sort: &Sort,
        filter: Option<&str>,
    ) -> Option<ClientId> {
        let source = server.render_source();
        for view in sorted(&source.clients, server, sort) {
            let ctx = client_ctx(&source, &view);
            if let Some(f) = filter
                && !vtmux_core::format::is_true(&vtmux_core::format::expand_time(f, &ctx))
            {
                continue;
            }
            let text = vtmux_core::format::expand_time(&self.format, &ctx);
            let id = ClientId(view.order as usize);
            let item = Item {
                id,
                name: view.name.clone(),
            };
            b.add(None, id, item, view.name.clone(), Some(text), None);
        }
        None
    }

    /// `window_client_get_key`: `format_defaults(ft, c, NULL, 0, NULL)`.
    fn item_key(&self, server: &Server, data: &Item, line: usize) -> Option<Key> {
        let source = server.render_source();
        let view = source
            .clients
            .iter()
            .find(|v| v.order as usize == data.id.0)?;
        let mut ctx = client_ctx(&source, view);
        ctx.extra.push(("line".into(), line.to_string()));
        Key::from_name(&vtmux_core::format::expand_time(&self.key_format, &ctx))
    }

    /// No `searchcb`: the item's name (mode-tree.c:1215-1220).
    fn matches(&self, _server: &Server, _: &str, data: &Item, search: &str, icase: bool) -> bool {
        super::tree::contains(&data.name, search, icase)
    }

    /// `server_client_how_many() == 0`.
    fn empty(server: &Server) -> bool {
        !server
            .clients
            .values()
            .any(|c| c.attached.is_some() && !c.closing)
    }

    /// The keys window-client handles itself (window-client.c:530-565).
    fn mode_key(
        server: &mut Server,
        p: PaneId,
        c: Option<ClientId>,
        key: Key,
        item: Option<Item>,
    ) -> bool {
        let ch = key
            .key_and_modifiers()
            .as_char()
            .filter(|_| !key.has_ctrl() && !key.has_meta());
        let detach = |server: &mut Server, t: &mut Self, item: &Item, hup: bool| {
            // window_client_do_detach.
            if t.tree.current_item().map(|i| i.tag) == Some(item.id) {
                t.tree.down(false);
            }
            let live = server
                .clients
                .get(&item.id)
                .is_some_and(|c| c.attached.is_some() && !c.closing);
            if live {
                server.detach_client(item.id, hup);
            }
        };
        match ch {
            Some(k @ ('d' | 'x')) => {
                server.with_tree::<Self, _>(p, |server, t| {
                    if let Some(item) = &item {
                        detach(server, t, item, k == 'x');
                    }
                    rebuild(t, server);
                });
            }
            Some(k @ ('D' | 'X')) => {
                server.with_tree::<Self, _>(p, |server, t| {
                    let tagged: Vec<Item> = t
                        .tree
                        .tagged()
                        .into_iter()
                        .map(|(_, d)| d.clone())
                        .collect();
                    for item in &tagged {
                        detach(server, t, item, k == 'X');
                    }
                    rebuild(t, server);
                });
            }
            // server_client_suspend: vtmux has no suspend-client yet; the
            // tree is built again as tmux does after it.
            Some('z' | 'Z') => {
                server.with_tree::<Self, _>(p, |server, t| rebuild(t, server));
            }
            Some('i') => {
                server.with_tree::<Self, _>(p, |server, t| {
                    t.info = !t.info;
                    t.tree.view_name = Some(if t.info { "info" } else { "preview" });
                    rebuild(t, server);
                });
            }
            Some('\r') => {
                let command = server
                    .with_tree::<Self, _>(p, |_, t| t.command.clone())
                    .unwrap_or_default();
                if let Some(item) = item {
                    server.tree_run_command(c, None, &command, &item.name);
                }
                return true;
            }
            _ => {}
        }
        false
    }

    /// `window_client_draw`.
    fn draw_preview(
        &self,
        source: &Source<'_>,
        canvas: &mut Canvas,
        data: &Item,
        (cx, cy): (usize, usize),
        (sx, sy): (usize, usize),
    ) {
        let Some(view) = source
            .clients
            .iter()
            .find(|v| v.order as usize == data.id.0)
        else {
            return;
        };
        let Some((_, w, active)) = view.session.and_then(|s| source.model.current_of(s)) else {
            return;
        };
        if self.info {
            Self::draw_info(source, canvas, view, w, (cx, cy), (sx, sy));
            return;
        }
        let mut wp = Some(active);
        if self.hide_this_pane && active == self.common.pane {
            wp = source
                .model
                .windows
                .get(&w)
                .and_then(|x| x.panes.last.first().copied());
        }
        let status = self.statuses.get(&data.id).cloned().unwrap_or_default();
        let mut lines = status.rows.len();
        if lines >= sy {
            lines = 0;
        }
        let at = if status.top { lines } else { 0 };
        if let Some(pane) = wp.and_then(|p| source.model.panes.get(&p)) {
            canvas.preview(
                cx,
                cy + at,
                &vtmux_vt::snapshot(&pane.term),
                sx,
                sy.saturating_sub(2 + lines),
            );
        }
        let plain =
            crate::format_ctx::Ctx::new(source.model, source.info, source.env, Found::default());
        let border = source.style(TreeId::Window(w), "tree-mode-border-style", &plain);
        let hy = if at == 0 { cy + sy - 1 - lines } else { cy + 2 };
        let line = styled::Cell {
            text: "q".into(),
            width: 1,
            style: CellStyle {
                attrs: border.attrs.union(Attrs::ACS),
                ..border
            },
        };
        for x in 0..sx {
            canvas.put(cx + x, hy, line.clone());
        }
        // screen_write_fast_copy of the client's status lines.
        let sy0 = if at == 0 { cy + sy - lines } else { cy };
        for (i, row) in status.rows.iter().take(lines).enumerate() {
            for (x, cell) in row.iter().take(sx).enumerate() {
                canvas.put(cx + x, sy0 + i, cell.clone());
            }
        }
    }
}

impl ClientMode {
    /// `window_client_draw_info`.
    fn draw_info(
        source: &Source<'_>,
        canvas: &mut Canvas,
        view: &ClientView,
        w: vtmux_core::ids::WindowId,
        (cx, cy): (usize, usize),
        (sx, sy): (usize, usize),
    ) {
        let mut ctx = client_ctx(source, view);
        ctx.extra.push(("clipboard_invalid".into(), "0".into()));
        let lines = info_lines();
        let mut i = 0;
        for l in lines.iter().take(sy) {
            let expanded = vtmux_core::format::expand_time(l, &ctx);
            canvas.draw(cx, cy + i, sx, &CellStyle::default(), &expanded, false);
            i += 1;
        }
        if sx > 14 && i < sy {
            let plain = crate::format_ctx::Ctx::new(
                source.model,
                source.info,
                source.env,
                Found::default(),
            );
            let border = source.style(TreeId::Window(w), "tree-mode-border-style", &plain);
            canvas.vline(cx + 14, cy + i, sy - i, &border);
        }
    }
}
