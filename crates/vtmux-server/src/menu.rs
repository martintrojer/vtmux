//! Window menus (menu.c): a list of items drawn over a window
//! ([`crate::render::menu`]), owned by the window (`w->menu`) and taking
//! the keys of every client looking at it while it is up.
//!
//! A menu has no terminal and no job. The command that opens it
//! (`display-menu`) returns at once; choosing an item queues that item's
//! command for the client that chose it, from the target and the mouse
//! event the menu was opened with.

use vtmux_core::cmd::ArgValue;
use vtmux_core::format::trim::trim_right;
use vtmux_core::ids::WindowId;
use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;
use vtmux_core::style::CellStyle;
use vtmux_core::styled::format_width;

use crate::cmdq::{End, NewBatch};
use crate::input::{MOUSE_MASK_BUTTONS, MouseInput, mouse_wheel};
use crate::model::Redraw;
use crate::mouse::MouseEvent;
use crate::render::popup::BoxLines;
use crate::server::{ClientId, Server};
use crate::target::{Found, Origin};

/// One line of a menu (`struct menu_item`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// An empty name: a line across the menu.
    Separator,
    Item {
        /// Expanded and fitted, with the key on the right when it fits. A
        /// leading `-` marks the item disabled; it is not drawn.
        name: String,
        /// The shortcut (`KEYC_UNKNOWN` is `None`).
        key: Option<Key>,
        /// Expanded when the menu opened.
        command: String,
    },
}

impl Entry {
    /// An item that can be chosen: not a separator, not disabled.
    fn choosable(&self) -> bool {
        matches!(self, Self::Item { name, .. } if !name.starts_with('-'))
    }
}

/// A menu's items as `menu_add_item` builds them (`struct menu`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Items {
    pub title: String,
    pub entries: Vec<Entry>,
    /// The widest item or the title (`menu->width`); the menu is 4 wider.
    pub width: usize,
}

impl Items {
    /// `menu_create`.
    #[must_use]
    pub fn new(title: String) -> Self {
        Self {
            width: format_width(&title),
            title,
            entries: Vec::new(),
        }
    }

    /// `menu_add_item` with no item: never first, never twice in a row.
    pub fn separator(&mut self) {
        if !matches!(self.entries.last(), None | Some(Entry::Separator)) {
            self.entries.push(Entry::Separator);
        }
    }

    /// `menu_add_item` (menu.c:65-151) for an expanded name: empty, it adds
    /// nothing. The name is fitted to the client's width less 4, trimmed
    /// from the left with a `>` when too long; the key goes on the right
    /// if it is short or there is room for it.
    pub fn item(&mut self, text: &str, key: Option<Key>, command: String, client_cols: u32) {
        if text.is_empty() {
            return;
        }
        // `c->tty.sx - 4`, and the lengths are bytes (`strlen`).
        let mut max = usize::try_from(client_cols.saturating_sub(4)).unwrap_or(0);
        let len = text.len();
        let mut shown = None;
        if let Some(k) = key.filter(|k| *k != Key::NONE && !text.starts_with('-')) {
            let name = k.name(false);
            let keylen = name.len() + 3;
            if keylen <= max / 4 {
                max -= keylen;
                shown = Some(name);
            } else if keylen < max && len < max - keylen {
                shown = Some(name);
            }
        }
        let mut suffix = "";
        if len > max {
            max = max.saturating_sub(1);
            suffix = ">";
        }
        let trimmed = trim_right(text, max);
        let name = match shown {
            Some(k) => format!("{trimmed}{suffix}#[default] #[align=right]({k})"),
            None => format!("{trimmed}{suffix}"),
        };
        let mut width = format_width(&name);
        if name.starts_with('-') {
            width = width.saturating_sub(1);
        }
        self.width = self.width.max(width);
        self.entries.push(Entry::Item { name, key, command });
    }
}

/// What a key or mouse event does to a menu (`menu_key`'s result).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Stay,
    Close,
    /// Run this item's command and close.
    Run(usize),
}

/// An open menu (`struct menu_data`).
#[derive(Clone, Debug)]
pub struct Menu {
    pub items: Items,
    /// Top-left corner in the window.
    pub px: u32,
    pub py: u32,
    pub choice: Option<usize>,
    pub lines: BoxLines,
    /// `-s`, `-H` and `-S`, applied over the options at every draw.
    pub style: Option<String>,
    pub selected_style: Option<String>,
    pub border_style: Option<String>,
    /// `-O` (`MENU_STAYOPEN`).
    pub stay_open: bool,
    /// Opened with no mouse event and no `-M` (`MENU_NOMOUSE`).
    pub no_mouse: bool,
    /// The target the commands start from (`md->fs`).
    pub found: Found,
    /// The opening event (`md->key`, `md->m`), given to the commands.
    pub key: Option<Key>,
    pub mouse: Option<MouseEvent>,
    /// A tree mode's menu (`mode_tree_display_menu`): choosing an item
    /// hands its key to the mode instead of running a command.
    pub tree: Option<TreeMenu>,
}

/// Whose tree-mode menu it is (`struct mode_tree_menu`).
#[derive(Clone, Copy, Debug)]
pub struct TreeMenu {
    pub pane: vtmux_core::ids::PaneId,
    /// The mode's [`crate::mode::tree_mode::Common::serial`].
    pub serial: u64,
    pub line: usize,
}

impl Menu {
    /// `menu_width`.
    #[must_use]
    pub fn box_width(&self) -> u32 {
        u32::try_from(self.items.width + 4).unwrap_or(u32::MAX)
    }

    /// `menu_height`.
    #[must_use]
    pub fn box_height(&self) -> u32 {
        u32::try_from(self.items.entries.len() + 2).unwrap_or(u32::MAX)
    }

    /// `menu_get_cursor`, in the window.
    #[must_use]
    pub fn cursor(&self) -> (u32, u32) {
        let y = self
            .choice
            .map_or(self.py, |c| self.py + 1 + u32::try_from(c).unwrap_or(0));
        (self.px + 2, y)
    }

    /// `menu_resize` and `menu_display`'s fit: moved back inside a window
    /// of `sx` x `sy`, to 0 when it is no smaller than the window.
    pub fn fit(&mut self, sx: u32, sy: u32) {
        let (w, h) = (self.box_width(), self.box_height());
        if self.px + w > sx {
            self.px = sx.saturating_sub(w);
        }
        if self.py + h > sy {
            self.py = sy.saturating_sub(h);
        }
    }

    fn choosable(&self, i: i64) -> bool {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.items.entries.get(i))
            .is_some_and(Entry::choosable)
    }

    fn count(&self) -> i64 {
        i64::try_from(self.items.entries.len()).unwrap_or(i64::MAX)
    }

    fn set(&mut self, c: i64) {
        self.choice = usize::try_from(c).ok();
    }

    /// `menu_display`'s starting choice (menu.c:631-662), with no mouse:
    /// the first item that can be chosen from `start` on, wrapping; past
    /// the end, the last one before it. -1 chooses nothing.
    pub fn start_at(&mut self, start: i64) {
        let n = self.count();
        if start >= n {
            let start = n - 1;
            let mut choice = start + 1;
            loop {
                if self.choosable(choice - 1) {
                    self.set(choice - 1);
                    break;
                }
                choice -= 1;
                if choice == 0 {
                    choice = n;
                }
                if choice == start + 1 {
                    break;
                }
            }
        } else if start >= 0 {
            let mut choice = start;
            loop {
                if self.choosable(choice) {
                    self.set(choice);
                    break;
                }
                choice += 1;
                if choice == n {
                    choice = 0;
                }
                if choice == start {
                    break;
                }
            }
        }
    }

    /// `menu_key` for a key (menu.c:363-498): an item's shortcut first,
    /// then moving (skipping separators and disabled items), Enter to
    /// choose and the cancel keys. Every other key is taken and ignored.
    pub fn key(&mut self, key: Key) -> Action {
        let key = key.without_flags();
        let shortcut = self.items.entries.iter().position(|e| {
            matches!(e, Entry::Item { key: Some(k), .. } if e.choosable() && k.without_flags() == key)
        });
        if let Some(i) = shortcut {
            self.choice = Some(i);
            return self.chosen();
        }
        let n = self.count();
        let mut c = self.choice.map_or(-1, |c| i64::try_from(c).unwrap_or(-1));
        let old = c.max(0);
        let ctrl = |b: u8| Key::ascii(b).ctrl();
        match key {
            k if k == Key::BTAB || k == Key::UP || k == Key::ascii(b'k') => loop {
                c = if c <= 0 { n - 1 } else { c - 1 };
                if self.choosable(c) || c == old {
                    break;
                }
            },
            k if k == Key::DOWN || k == Key::ascii(b'j') => loop {
                c = if c == -1 || c == n - 1 { 0 } else { c + 1 };
                if self.choosable(c) || c == old {
                    break;
                }
            },
            k if k == Key::PPAGE || k == ctrl(b'b') => {
                if c < 6 {
                    c = 0;
                } else {
                    let mut i = 5;
                    while i > 0 {
                        c -= 1;
                        if c != 0 && self.choosable(c) {
                            i -= 1;
                        } else if c == 0 {
                            break;
                        }
                    }
                }
            }
            k if k == Key::NPAGE => {
                if c > n - 6 {
                    c = n - 1;
                } else {
                    let mut i = 5;
                    while i > 0 {
                        c += 1;
                        if c != n - 1 && self.choosable(c) {
                            i -= 1;
                        } else if c == n - 1 {
                            break;
                        }
                    }
                }
                while !self.choosable(c) && c != 0 {
                    c -= 1;
                }
            }
            k if k == Key::HOME || k == Key::ascii(b'g') => {
                c = 0;
                while !self.choosable(c) && c != n - 1 {
                    c += 1;
                }
            }
            k if k == Key::END || k == Key::ascii(b'G') => {
                c = n - 1;
                while !self.choosable(c) && c != 0 {
                    c -= 1;
                }
            }
            k if k == Key::ascii(b'\r') => return self.chosen(),
            k if k == Key::ascii(0x1b)
                || k == ctrl(b'[')
                || k == ctrl(b'c')
                || k == ctrl(b'g')
                || k == Key::ascii(b'q') =>
            {
                return Action::Close;
            }
            _ => return Action::Stay,
        }
        self.set(c);
        Action::Stay
    }

    /// `menu_key` for a mouse event (menu.c:313-361) at window position
    /// `(x, y)` (`u32::MAX` on the status line), button byte `b`.
    pub fn mouse(&mut self, x: u32, y: u32, b: u32) -> Action {
        let buttons = b & MOUSE_MASK_BUTTONS;
        let release = buttons == 3;
        let drag = b & 32 != 0;
        // A move with no button held reports as a release: it only
        // highlights.
        let moving = drag && release;
        if self.no_mouse {
            return if buttons == 0 {
                Action::Stay
            } else {
                Action::Close
            };
        }
        let n = u32::try_from(self.items.entries.len()).unwrap_or(u32::MAX);
        let width = u32::try_from(self.items.width).unwrap_or(u32::MAX);
        // `m->x > md->px + 4 + width`: one column past the box still counts.
        let outside = x < self.px
            || x > self.px.saturating_add(4).saturating_add(width)
            || y < self.py.saturating_add(1)
            || y > self.py.saturating_add(n);
        if outside {
            if self.stay_open {
                if !release && !mouse_wheel(b) && !drag {
                    return Action::Close;
                }
            } else if !moving && release {
                return Action::Close;
            }
            self.choice = None;
            return Action::Stay;
        }
        if self.stay_open {
            if !mouse_wheel(b) && !drag {
                return self.chosen();
            }
        } else if !moving && release {
            return self.chosen();
        }
        self.choice = usize::try_from(y - (self.py + 1)).ok();
        Action::Stay
    }

    /// `menu_key`'s `chosen:`: nothing chosen closes; a separator or a
    /// disabled item closes too, unless `-O`.
    fn chosen(&self) -> Action {
        match self.choice {
            Some(c) if self.items.entries[c].choosable() => Action::Run(c),
            Some(_) if self.stay_open => Action::Stay,
            _ => Action::Close,
        }
    }
}

/// `style_parse` of `-s`/`-S`/`-H` over `base`, keeping only the colours
/// (menu.c:179-216).
fn colours_over(base: &mut CellStyle, extra: Option<&str>) {
    let Some(extra) = extra else { return };
    let default = CellStyle::default();
    let mut sy = vtmux_core::style::Style::new(&default);
    if sy.parse(base, extra).is_ok() {
        base.fg = sy.cell.fg;
        base.bg = sy.cell.bg;
    }
}

/// A menu's three styles at one draw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuStyles {
    pub normal: CellStyle,
    pub selected: CellStyle,
    pub border: CellStyle,
}

impl Server {
    /// The open menu of window `w`.
    pub(crate) fn window_menu(&self, w: WindowId) -> Option<&Menu> {
        self.model.windows.get(&w)?.menu.as_ref()
    }

    /// `menu_display` (menu.c:574-670) once the items and position are
    /// worked out: fitted to window `w`, remembered for `-x L`/`-y L`,
    /// replacing any menu the window had.
    pub(crate) fn menu_display(&mut self, w: WindowId, mut menu: Menu) {
        let Some(window) = self.model.windows.get_mut(&w) else {
            return;
        };
        menu.fit(window.sx(), window.sy());
        window.menu_last = (menu.px, menu.py);
        self.menu_close(w);
        let Some(window) = self.model.windows.get_mut(&w) else {
            return;
        };
        window.menu = Some(menu);
        // redraw_invalidate_scene, window_update_focus,
        // server_redraw_window.
        self.update_focus(Some(w));
        self.model.redraw(Redraw::Window(w));
    }

    /// `menu_close`: the window's menu goes, its active pane may get the
    /// focus back and the window is redrawn.
    pub(crate) fn menu_close(&mut self, w: WindowId) {
        let had = self
            .model
            .windows
            .get_mut(&w)
            .and_then(|x| x.menu.take())
            .is_some();
        if had {
            self.update_focus(Some(w));
            self.model.redraw(Redraw::Window(w));
        }
    }

    /// `server_client_handle_menu_key` (server-client.c:1717-1751): the menu
    /// of client `c`'s window takes the key, and a mouse event at its
    /// window position. `false` when that window has no menu.
    pub(crate) fn menu_key(&mut self, c: ClientId, key: Key, mouse: Option<&MouseInput>) -> bool {
        let Some(w) = self.client_window(c) else {
            return false;
        };
        if self.window_menu(w).is_none() {
            return false;
        }
        let at = mouse
            .filter(|_| key.is_mouse())
            .map(|m| (self.menu_mouse_position(c, m), m.b));
        let Some(menu) = self.model.windows.get_mut(&w).and_then(|x| x.menu.as_mut()) else {
            return false;
        };
        let before = menu.choice;
        let action = match at {
            Some((pos, button)) => menu.mouse(pos.0, pos.1, button),
            None => menu.key(key),
        };
        match action {
            // server_redraw_window_menu.
            Action::Stay if menu.choice != before => self.model.redraw(Redraw::Window(w)),
            Action::Stay => {}
            Action::Close => self.menu_close(w),
            Action::Run(i) => {
                let Entry::Item {
                    command,
                    key: item_key,
                    ..
                } = menu.items.entries[i].clone()
                else {
                    return true;
                };
                let (found, key, mouse, tree) =
                    (menu.found, menu.key, menu.mouse.clone(), menu.tree);
                self.menu_close(w);
                match tree {
                    // menu_key: `md->cb` with the item's key.
                    Some(t) => {
                        if let Some(k) = item_key {
                            self.tree_menu_choose(c, t, k);
                        }
                    }
                    None => self.menu_run(c, &command, found, key, mouse),
                }
            }
        }
        true
    }

    /// The window position of mouse report `m` from client `c`
    /// (server-client.c:1730-1745): below a top status line, `u32::MAX`
    /// on a status line.
    fn menu_mouse_position(&self, c: ClientId, m: &MouseInput) -> (u32, u32) {
        const NOWHERE: (u32, u32) = (u32::MAX, u32::MAX);
        let (status_at, lines) = self.status_place(c);
        let row = match status_at {
            Some(0) if m.y < lines => return NOWHERE,
            Some(0) => m.y - lines,
            Some(at) if m.y >= at => return NOWHERE,
            _ => m.y,
        };
        (m.x, row)
    }

    /// `menu_key`'s command (menu.c:520-536): parsed and queued for client
    /// `c`, from the menu's target and with its opening event; a parse
    /// error is the client's message.
    fn menu_run(
        &mut self,
        c: ClientId,
        command: &str,
        found: Found,
        key: Option<Key>,
        mouse: Option<MouseEvent>,
    ) {
        let origin = Origin {
            attached: self.client_session(c),
            inside_pane: None,
            current: Some(found).filter(|f| f.still_valid(&self.model)),
            mouse: mouse.as_ref().map(|m| m.target),
        };
        let value = ArgValue::Str(command.to_owned());
        match crate::commands::make_commands(self, Some(c), origin, None, &value) {
            Ok(calls) => {
                let mut item = crate::commands::Item::new(Some(c), self.client_cwd(Some(c), None));
                if let Some(client) = self.clients.get(&c) {
                    if let Some(hello) = client.hello.as_ref() {
                        item.env = Some(std::rc::Rc::clone(&client.environ));
                        item.utf8 = hello.flags.utf8;
                    }
                    if client.control.is_some() {
                        item.control = Some(0);
                    }
                }
                item.origin = origin;
                item.key = key;
                item.mouse = mouse;
                self.cmdq_append(Some(c), NewBatch::new(item, calls, End::Quiet));
            }
            Err(msg) => self.client_error_message(Some(c), &msg),
        }
    }

    /// `menu_reapply_styles` (menu.c:170-217): `menu-style`,
    /// `menu-selected-style` and `menu-border-style` of the menu's window,
    /// expanded for its target, then `-s`, `-H` and `-S` over their
    /// colours.
    pub(crate) fn menu_styles(
        &self,
        w: WindowId,
        menu: &Menu,
        source: &crate::render::window::Source<'_>,
    ) -> MenuStyles {
        let ctx = self.format_ctx(menu.found, None);
        let tree = TreeId::Window(w);
        let style = |name: &str, extra: Option<&str>| {
            let mut s = source.style(tree, name, &ctx);
            colours_over(&mut s, extra);
            s
        };
        MenuStyles {
            normal: style("menu-style", menu.style.as_deref()),
            selected: style("menu-selected-style", menu.selected_style.as_deref()),
            border: style("menu-border-style", menu.border_style.as_deref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(names: &[&str]) -> Menu {
        let mut items = Items::new(String::new());
        for n in names {
            if n.is_empty() {
                items.separator();
            } else {
                let key = Key::from_name(&n[n.len() - 1..]);
                items.item(n, key, format!("run {n}"), 80);
            }
        }
        Menu {
            items,
            px: 0,
            py: 0,
            choice: None,
            lines: BoxLines::Single,
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

    #[test]
    fn separators_are_never_first_or_doubled() {
        let m = menu(&["", "a", "", "", "b", ""]);
        assert_eq!(
            m.items
                .entries
                .iter()
                .map(|e| matches!(e, Entry::Separator))
                .collect::<Vec<_>>(),
            [false, true, false, true]
        );
    }

    #[test]
    fn key_shown_on_the_right_when_it_fits() {
        let m = menu(&["Alpha a"]);
        let Entry::Item { name, .. } = &m.items.entries[0] else {
            panic!()
        };
        assert_eq!(name, "Alpha a#[default] #[align=right](a)");
        assert_eq!(m.items.width, 11);
        // Disabled: no key, and the `-` takes no column.
        let m = menu(&["-Off x"]);
        let Entry::Item { name, .. } = &m.items.entries[0] else {
            panic!()
        };
        assert_eq!(name, "-Off x");
        assert_eq!(m.items.width, 5);
    }

    /// menu.c:107-119: too long for the client, trimmed from the left with
    /// a `>`; the key is dropped when the text does not leave room.
    #[test]
    fn long_items_are_trimmed() {
        let mut items = Items::new(String::new());
        items.item(
            "This item is much wider than the window",
            Key::from_name("t"),
            String::new(),
            12,
        );
        let Entry::Item { name, .. } = &items.entries[0] else {
            panic!()
        };
        assert_eq!(name, " window>");
    }

    #[test]
    fn moving_skips_separators_and_disabled_items() {
        let mut m = menu(&["a a", "", "-b b", "c c"]);
        m.start_at(0);
        assert_eq!(m.choice, Some(0));
        assert_eq!(m.key(Key::DOWN), Action::Stay);
        assert_eq!(m.choice, Some(3));
        assert_eq!(m.key(Key::DOWN), Action::Stay);
        assert_eq!(m.choice, Some(0));
        assert_eq!(m.key(Key::UP), Action::Stay);
        assert_eq!(m.choice, Some(3));
        assert_eq!(m.key(Key::ascii(b'g')), Action::Stay);
        assert_eq!(m.choice, Some(0));
        assert_eq!(m.key(Key::END), Action::Stay);
        assert_eq!(m.choice, Some(3));
        assert_eq!(m.key(Key::ascii(b'\r')), Action::Run(3));
        assert_eq!(m.key(Key::ascii(0x1b)), Action::Close);
        assert_eq!(m.key(Key::ascii(b'x')), Action::Stay);
    }

    /// menu.c:363-372: an item's key wins over the movement keys, and a
    /// disabled item's key does nothing.
    #[test]
    fn shortcuts_come_first() {
        let mut m = menu(&["a j", "b q", "-c c"]);
        assert_eq!(m.key(Key::ascii(b'q')), Action::Run(1));
        assert_eq!(m.key(Key::ascii(b'j')), Action::Run(0));
        assert_eq!(m.key(Key::ascii(b'c')), Action::Stay);
    }

    #[test]
    fn starting_choice_wraps() {
        let mut m = menu(&["-a a", "b b", "", "c c"]);
        m.start_at(0);
        assert_eq!(m.choice, Some(1));
        m.start_at(2);
        assert_eq!(m.choice, Some(3));
        m.start_at(9);
        assert_eq!(m.choice, Some(3));
        m.choice = None;
        m.start_at(-1);
        assert_eq!(m.choice, None);
    }

    #[test]
    fn fit_moves_back_inside() {
        let mut m = menu(&["abc a"]);
        m.px = 30;
        m.py = 30;
        m.fit(20, 10);
        assert_eq!((m.px, m.py), (20 - m.box_width(), 10 - 3));
        m.fit(2, 2);
        assert_eq!((m.px, m.py), (0, 0));
    }

    #[test]
    fn keyboard_menu_closes_on_a_mouse_release() {
        let mut m = menu(&["a a"]);
        assert_eq!(m.mouse(1, 1, 0), Action::Stay);
        assert_eq!(m.mouse(1, 1, 3), Action::Close);
    }

    #[test]
    fn mouse_press_highlights_and_release_chooses() {
        let mut m = menu(&["a a", "b b"]);
        m.no_mouse = false;
        assert_eq!(m.mouse(3, 2, 0), Action::Stay);
        assert_eq!(m.choice, Some(1));
        assert_eq!(m.mouse(3, 2, 3), Action::Run(1));
        // Outside: a release closes, unless it is a move.
        assert_eq!(m.mouse(40, 2, 32 | 3), Action::Stay);
        assert_eq!(m.choice, None);
        assert_eq!(m.mouse(40, 2, 3), Action::Close);
    }
}
