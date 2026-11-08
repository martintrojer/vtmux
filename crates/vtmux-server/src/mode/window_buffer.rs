//! `choose-buffer` (window-buffer.c): the paste buffers in a
//! [`ModeTree`], each line the mode's format expanded for its buffer, the
//! preview its first lines. Enter and `p` run the template for the
//! current buffer, `P` for each tagged one; `d` deletes the current
//! buffer, `D` the tagged ones. The mode ends when no buffer is left.
//!
//! `e` edits the current buffer in `editor` (`window_buffer_start_edit`),
//! in a modal floating pane as customize-mode's `e` does
//! ([`crate::editor`]); while it runs the mode shows `WAITING FOR EDITOR`
//! and takes only `q`, Escape and C-c, which end it. A right-click opens the mode's menu
//! ([`super::tree_mode`]).

use vtmux_core::ids::PaneId;
use vtmux_core::keys::Key;
use vtmux_core::style::CellStyle;

use super::tree::{Build, ModeTree, Sort, SortOrder};
use super::tree_mode::{Common, DEFAULT_KEY_FORMAT, TreeMode, rebuild};
use crate::paste::PasteBuffer;
use crate::render::tree::Canvas;
use crate::render::window::Source;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `WINDOW_BUFFER_DEFAULT_COMMAND`.
pub const DEFAULT_COMMAND: &str = "paste-buffer -p -b '%%'";

/// `WINDOW_BUFFER_DEFAULT_FORMAT`.
pub const DEFAULT_FORMAT: &str = "#{t/p:buffer_created}: #{buffer_sample}";

/// `window_buffer_order_seq`.
const ORDERS: &[SortOrder] = &[SortOrder::Creation, SortOrder::Name, SortOrder::Size];

/// A line's buffer (`struct window_buffer_itemdata`): its name.
pub type Item = String;

/// The `choose-buffer` arguments the mode reads.
pub struct Options {
    pub previews: usize,
    pub sort: Option<SortOrder>,
    pub reversed: bool,
    pub filter: Option<String>,
    pub format: Option<String>,
    pub key_format: Option<String>,
    pub command: Option<String>,
}

/// `struct window_buffer_modedata`.
#[derive(Clone, Debug)]
pub struct BufferMode {
    pub common: Common,
    /// The tag is the buffer's creation order (`item->order`).
    pub tree: ModeTree<u64, Item>,
    /// The target the mode started from (`data->fs`).
    start: Found,
    format: String,
    key_format: String,
    command: String,
    /// The pid of the editor open for this mode (`data->editor`,
    /// `spawn_get_editor_pid`), which the waiting box shows.
    editing: Option<i32>,
}

/// What an editor started by `e` is editing (`window_buffer_editdata`):
/// the buffer's name and which buffer it was (`ed->pb`), so a buffer of
/// that name made since is left alone.
#[derive(Debug)]
pub struct Edit {
    name: String,
    order: u64,
}

/// `sort_get_buffers` (sort.c:429-449) with `sort_buffer_cmp`: the time
/// list (oldest first, `paste_walk`), sorted.
fn sorted<'a>(server: &'a Server, sort: &Sort) -> Vec<&'a PasteBuffer> {
    let mut list = server.model.paste.by_time();
    list.reverse();
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
            SortOrder::Creation => b.order.cmp(&a.order),
            SortOrder::Size => a.data.len().cmp(&b.data.len()),
            _ => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.name.cmp(&b.name));
        if sort.reversed { o.reverse() } else { o }
    });
    list
}

impl BufferMode {
    /// `window_buffer_init` up to `mode_tree_build`.
    #[must_use]
    pub fn new(pane: PaneId, size: (usize, usize), start: Found, o: Options) -> Self {
        let sort = Sort {
            order: o.sort,
            reversed: o.reversed,
            seq: ORDERS,
        };
        let mut tree = ModeTree::new(size, o.previews, sort, o.filter);
        // window_buffer_sort: no order is the first of the sequence.
        if tree.sort.order.is_none() {
            tree.sort.order = Some(ORDERS[0]);
        }
        Self {
            common: Common::new(pane),
            tree,
            start,
            format: o.format.unwrap_or_else(|| DEFAULT_FORMAT.to_owned()),
            key_format: o
                .key_format
                .unwrap_or_else(|| DEFAULT_KEY_FORMAT.to_owned()),
            command: o.command.unwrap_or_else(|| DEFAULT_COMMAND.to_owned()),
            editing: None,
        }
    }

    /// The pid of the editor open for the mode.
    #[must_use]
    pub fn editing(&self) -> Option<i32> {
        self.editing
    }

    pub fn build(&mut self, server: &Server) {
        rebuild(self, server);
    }

    /// `format_defaults(ft, NULL, s, wl, wp)` for the starting target, if
    /// it is still valid, with the buffer.
    fn ctx<'a>(&self, server: &'a Server, name: &str) -> crate::format_ctx::Ctx<'a> {
        let found = if self.start.still_valid(&server.model) {
            self.start
        } else {
            Found::default()
        };
        let mut ctx =
            crate::format_ctx::Ctx::new(&server.model, &server.info, &server.global_env, found);
        ctx.clients = server.client_views();
        ctx.buffer = Some(name.to_owned());
        ctx
    }
}

impl TreeMode for BufferMode {
    type Tag = u64;
    type Data = Item;
    const HELP: &'static [(&'static str, &'static str)] = &[
        ("      Enter", "Paste selected %1"),
        ("          p", "Paste selected %1"),
        ("          P", "Paste tagged %1s"),
        ("          d", "Delete selected %1"),
        ("          D", "Delete tagged %1s"),
        ("          e", "Open %1 in editor"),
        ("          f", "Enter a filter"),
    ];
    const HELP_WIDTH: usize = 0;
    const HELP_ITEM: &'static str = "buffer";
    /// `window_buffer_menu_items` (window-buffer.c); `Tag All` sends
    /// `\024`, as tmux does.
    const MENU: &'static [(&'static str, Key)] = &[
        ("Paste", Key::ascii(b'p')),
        ("Paste Tagged", Key::ascii(b'P')),
        ("", Key::NONE),
        ("Tag", Key::ascii(b't')),
        ("Tag All", Key::ascii(0x14)),
        ("Tag None", Key::ascii(b'T')),
        ("", Key::NONE),
        ("Delete", Key::ascii(b'd')),
        ("Delete Tagged", Key::ascii(b'D')),
        ("", Key::NONE),
        ("Cancel", Key::ascii(b'q')),
    ];

    fn tree(&self) -> &ModeTree<u64, Item> {
        &self.tree
    }
    fn tree_mut(&mut self) -> &mut ModeTree<u64, Item> {
        &mut self.tree
    }
    fn editing(&self) -> bool {
        self.editing.is_some()
    }
    fn common(&self) -> &Common {
        &self.common
    }
    fn common_mut(&mut self) -> &mut Common {
        &mut self.common
    }
    fn of(mode: &super::Mode) -> Option<&Self> {
        match mode {
            super::Mode::Buffer(t) => Some(t),
            _ => None,
        }
    }
    fn into_mode(self: Box<Self>) -> super::Mode {
        super::Mode::Buffer(self)
    }
    fn from_mode(mode: super::Mode) -> Option<Box<Self>> {
        match mode {
            super::Mode::Buffer(t) => Some(t),
            _ => None,
        }
    }

    /// `window_buffer_build`.
    fn build_items(
        &self,
        server: &Server,
        b: &mut Build<u64, Item>,
        sort: &Sort,
        filter: Option<&str>,
    ) -> Option<u64> {
        for pb in sorted(server, sort) {
            let ctx = self.ctx(server, &pb.name);
            if let Some(f) = filter
                && !vtmux_core::format::is_true(&vtmux_core::format::expand_time(f, &ctx))
            {
                continue;
            }
            let text = vtmux_core::format::expand_time(&self.format, &ctx);
            b.add(
                None,
                pb.order,
                pb.name.clone(),
                pb.name.clone(),
                Some(text),
                None,
            );
        }
        None
    }

    /// `window_buffer_get_key`.
    fn item_key(&self, server: &Server, data: &Item, line: usize) -> Option<Key> {
        server.model.paste.get(data)?;
        let mut ctx = self.ctx(server, data);
        ctx.extra.push(("line".into(), line.to_string()));
        Key::from_name(&vtmux_core::format::expand_time(&self.key_format, &ctx))
    }

    /// `window_buffer_search`: the name, then the bytes (ASCII case).
    fn matches(&self, server: &Server, _: &str, data: &Item, search: &str, icase: bool) -> bool {
        let Some(pb) = server.model.paste.get(data) else {
            return false;
        };
        if super::tree::contains(data, search, icase) {
            return true;
        }
        let find = search.as_bytes();
        !find.is_empty()
            && pb.data.windows(find.len()).any(|w| {
                if icase {
                    w.eq_ignore_ascii_case(find)
                } else {
                    w == find
                }
            })
    }

    /// `paste_is_empty`.
    fn empty(server: &Server) -> bool {
        server.model.paste.by_time().is_empty()
    }

    /// The keys window-buffer handles itself (window-buffer.c:618-645).
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
        match ch {
            Some('e') => {
                if let (Some(c), Some(name)) = (c, item) {
                    server.buffer_start_edit(p, c, &name);
                }
            }
            Some('d') => {
                server.with_tree::<Self, _>(p, |server, t| {
                    if let Some(name) = item {
                        // window_buffer_do_delete: keep a valid selection.
                        if !t.tree.down(false) {
                            t.tree.up(false);
                        }
                        server.model.paste.remove(&name);
                    }
                    rebuild(t, server);
                });
            }
            Some('D') => {
                server.with_tree::<Self, _>(p, |server, t| {
                    let tagged: Vec<(u64, Item)> = t
                        .tree
                        .tagged()
                        .into_iter()
                        .map(|(k, d)| (k, d.clone()))
                        .collect();
                    for (tag, name) in tagged {
                        let current = t.tree.current_item().map(|i| i.tag);
                        if Some(tag) == current && !t.tree.down(false) {
                            t.tree.up(false);
                        }
                        server.model.paste.remove(&name);
                    }
                    rebuild(t, server);
                });
            }
            Some('P') => {
                let (names, command) = server
                    .with_tree::<Self, _>(p, |_, t| {
                        let names: Vec<Item> = t
                            .tree
                            .tagged()
                            .into_iter()
                            .map(|(_, d)| d.clone())
                            .collect();
                        (names, t.command.clone())
                    })
                    .unwrap_or_default();
                for name in names {
                    if server.model.paste.get(&name).is_some() {
                        server.tree_run_command(c, None, &command, &name);
                    }
                }
                return true;
            }
            Some('p' | '\r') => {
                let command = server
                    .with_tree::<Self, _>(p, |_, t| t.command.clone())
                    .unwrap_or_default();
                if let Some(name) = item.filter(|n| server.model.paste.get(n).is_some()) {
                    server.tree_run_command(c, None, &command, &name);
                }
                return true;
            }
            _ => {}
        }
        false
    }

    /// `window_buffer_draw`: the buffer's first lines through `vis`.
    fn draw_preview(
        &self,
        source: &Source<'_>,
        canvas: &mut Canvas,
        data: &Item,
        (cx, cy): (usize, usize),
        (sx, sy): (usize, usize),
    ) {
        let Some(pb) = source.model.paste.get(data) else {
            return;
        };
        for (i, line) in pb.data.split(|&b| b == b'\n').take(sy).enumerate() {
            let text = crate::paste::vis_line(line);
            let mut x = cx;
            nputs(canvas, &mut x, cy + i, sx, &text);
        }
    }
}

impl Server {
    /// `window_buffer_start_edit`: buffer `name`'s data in the editor,
    /// unless one is open for the mode already.
    fn buffer_start_edit(&mut self, p: PaneId, c: ClientId, name: &str) {
        if self.with_tree::<BufferMode, _>(p, |_, t| t.editing.is_some()) != Some(false) {
            return;
        }
        let Some(pb) = self.model.paste.get(name) else {
            return;
        };
        let (data, edit) = (
            pb.data.clone(),
            Edit {
                name: pb.name.clone(),
                order: pb.order,
            },
        );
        if self.spawn_editor(c, p, &data, crate::editor::Edit::Buffer(edit)) {
            let pid = self.editor_pid(p);
            self.with_tree::<BufferMode, _>(p, |_, t| t.editing = pid);
        }
    }

    /// `window_buffer_edit_close_cb` (window-buffer.c:536-587): the
    /// editor is done; `value` is what it left in the file, `None` when it
    /// failed. Nothing changes if the mode is gone (`spawn_cancel_editor`
    /// in `window_buffer_free`), the file is empty, or the buffer is gone
    /// or another one; a newline the editor added to a buffer that did not
    /// end in one goes.
    pub(crate) fn buffer_edit_done(&mut self, p: PaneId, edit: &Edit, value: Option<Vec<u8>>) {
        if self.with_tree::<BufferMode, _>(p, |_, t| t.editing.take().is_some()) != Some(true) {
            return;
        }
        let Some(mut value) = value.filter(|v| !v.is_empty()) else {
            return;
        };
        let Some(pb) = self
            .model
            .paste
            .get(&edit.name)
            .filter(|pb| pb.order == edit.order)
        else {
            return;
        };
        if pb.data.last().is_some_and(|&b| b != b'\n') && value.last() == Some(&b'\n') {
            value.pop();
        }
        if !value.is_empty() {
            self.model.paste.replace(&edit.name, value);
        }
        self.with_tree::<BufferMode, _>(p, |s, t| rebuild(t, s));
    }
}

/// `screen_write_nputs` with the default cell: at most `max` columns, a
/// wide character that does not fit padded with spaces; tabs and printable
/// ASCII only (the text has been through `vis`).
fn nputs(c: &mut Canvas, x: &mut usize, y: usize, max: usize, text: &str) {
    let style = CellStyle::default();
    let mut size = 0;
    for ch in text.chars() {
        let w = usize::from(vtmux_core::width::char_width(ch));
        if !ch.is_ascii() {
            if size + w > max {
                let mut pad = String::new();
                for _ in size..max {
                    pad.push(' ');
                }
                c.puts(x, y, &style, &pad);
                break;
            }
            size += w;
            c.put_wide(x, y, &style, ch, w);
            continue;
        }
        if size + 1 > max {
            break;
        }
        if ch == '\t' || (' '..'\x7f').contains(&ch) {
            size += 1;
            c.puts(x, y, &style, &ch.to_string());
        }
    }
}
