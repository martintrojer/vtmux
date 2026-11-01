//! The tree that tree modes show (mode-tree.c): items in a tree, the
//! lines of the expanded part, the selected line and the view's offset,
//! tags, sorting, a filter with its "no matches" fallback, searching, the
//! help screen and the preview's height. A mode builds its items
//! ([`Build`]) and reacts to what [`ModeTree::key`] hands back; the
//! drawing is `render::tree`.
//!
//! Items keep their tagged and expanded state across builds by tag, as
//! `mode_tree_add` finds them in the saved list.

use vtmux_core::keys::Key;

/// `enum sort_order` (tmux.h:2566-2576) with `sort_order_from_string`
/// and `sort_order_to_string` (sort.c:371-417).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Activity,
    Creation,
    Index,
    Modifier,
    Name,
    /// `order`: the list as it is.
    Order,
    Size,
    Z,
}

impl SortOrder {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let eq = |n: &str| s.eq_ignore_ascii_case(n);
        Some(if eq("activity") {
            Self::Activity
        } else if eq("creation") {
            Self::Creation
        } else if eq("index") || eq("key") {
            Self::Index
        } else if eq("modifier") {
            Self::Modifier
        } else if eq("name") || eq("title") {
            Self::Name
        } else if eq("order") {
            Self::Order
        } else if eq("size") {
            Self::Size
        } else if eq("z") {
            Self::Z
        } else {
            return None;
        })
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Creation => "creation",
            Self::Index => "index",
            Self::Modifier => "modifier",
            Self::Name => "name",
            Self::Order => "order",
            Self::Size => "size",
            Self::Z => "z",
        }
    }
}

/// `struct sort_criteria`: the order (`None` is `SORT_END`, no sorting),
/// reversed, and the orders the `O` key steps through.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sort {
    pub order: Option<SortOrder>,
    pub reversed: bool,
    pub seq: &'static [SortOrder],
}

impl Sort {
    /// `sort_next_order` (sort.c:349-369): the order after the current one
    /// in the sequence, the first after the last or after one not in it.
    fn next(&mut self) {
        if self.seq.is_empty() {
            return;
        }
        let i = self
            .seq
            .iter()
            .position(|o| Some(*o) == self.order)
            .map_or(0, |i| (i + 1) % self.seq.len());
        self.order = Some(self.seq[i]);
    }
}

/// `enum mode_tree_preview`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preview {
    Off,
    #[default]
    Normal,
    Big,
}

/// `struct mode_tree_item`.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools, reason = "mode_tree_item's flags")]
pub struct Item<K, D> {
    pub tag: K,
    /// The mode's own data for the item (`itemdata`).
    pub data: D,
    pub name: String,
    pub text: Option<String>,
    parent: Option<usize>,
    children: Vec<usize>,
    pub expanded: bool,
    pub tagged: bool,
    pub no_tag: bool,
    /// `mode_tree_align`: -1 left, 0 none, 1 right.
    pub align: i8,
    pub draw_as_parent: bool,
    /// The line it is on, while it is on one.
    line: usize,
    /// Its shortcut key (`mti->key`), `None` for `KEYC_NONE`.
    pub key: Option<Key>,
}

/// `struct mode_tree_line`.
#[derive(Clone, Copy, Debug)]
pub struct Line {
    pub item: usize,
    pub depth: usize,
    pub last: bool,
    /// None of the item's siblings has children.
    pub flat: bool,
}

/// The items a mode builds (`mode_tree_add`, `mode_tree_remove`), with
/// the state the last build's items had.
pub struct Build<K, D> {
    items: Vec<Item<K, D>>,
    roots: Vec<usize>,
    saved: Vec<(K, bool, bool)>,
}

impl<K: Copy + PartialEq, D> Build<K, D> {
    /// `mode_tree_add`: a saved item keeps its expanded state, and its tag
    /// when its parent is expanded; a new one is expanded as asked
    /// (`None`, tmux's -1: expanded).
    pub fn add(
        &mut self,
        parent: Option<usize>,
        tag: K,
        data: D,
        name: String,
        text: Option<String>,
        expanded: Option<bool>,
    ) -> usize {
        let parent_expanded = parent.is_none_or(|p| self.items[p].expanded);
        let (tagged, expanded) = match self.saved.iter().find(|s| s.0 == tag) {
            Some(&(_, tagged, expanded)) => (parent_expanded && tagged, expanded),
            None => (false, expanded.unwrap_or(true)),
        };
        let id = self.items.len();
        self.items.push(Item {
            tag,
            data,
            name,
            text,
            parent,
            children: Vec::new(),
            expanded,
            tagged,
            no_tag: false,
            align: 0,
            draw_as_parent: false,
            line: 0,
            key: None,
        });
        match parent {
            Some(p) => self.items[p].children.push(id),
            None => self.roots.push(id),
        }
        id
    }

    /// `mode_tree_remove`: the item and its children leave the tree.
    pub fn remove(&mut self, id: usize) {
        let list = match self.items[id].parent {
            Some(p) => &mut self.items[p].children,
            None => &mut self.roots,
        };
        list.retain(|&x| x != id);
    }

    pub fn set_align(&mut self, id: usize, align: i8) {
        self.items[id].align = align;
    }
}

/// What a key leaves for the mode to do (`mode_tree_key`'s result and the
/// key it hands back).
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// The mode is finished (`q`, Escape, ...).
    Finished,
    /// The mode's own key, possibly changed (`\r` for a shortcut key);
    /// `None` when used up.
    Key(Option<Key>),
    /// Open the search prompt (`C-s`, `?`, `/`).
    Search,
    /// Open the filter prompt with the current filter (`f`).
    Filter,
    /// Close any prompt and clear the filter (`c`).
    ClearFilter,
    /// Swap the current item with the next (1) or previous (-1) at its
    /// depth (`J`, `K`).
    Swap(isize),
}

/// What the tree needs from the mode while handling keys: building the
/// items again and matching a search.
pub trait Env<K, D> {
    /// `mode_tree_build`: the items again (see [`ModeTree::build`]).
    fn rebuild(&mut self, tree: &mut ModeTree<K, D>);
    /// `searchcb`.
    fn matches(&self, data: &D, search: &str, icase: bool) -> bool;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SearchDir {
    #[default]
    Forward,
    Backward,
}

/// `struct mode_tree_data`.
#[derive(Clone, Debug)]
pub struct ModeTree<K, D> {
    items: Vec<Item<K, D>>,
    roots: Vec<usize>,
    lines: Vec<Line>,
    maxdepth: usize,
    /// The mode's screen size.
    pub size: (usize, usize),
    pub width: usize,
    pub height: usize,
    pub current: usize,
    pub offset: usize,
    pub sort: Sort,
    pub preview: Preview,
    pub view_name: Option<&'static str>,
    pub search: Option<String>,
    search_icase: bool,
    search_dir: SearchDir,
    pub filter: Option<String>,
    pub no_matches: bool,
    pub help: bool,
}

impl<K, D> Default for ModeTree<K, D> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            roots: Vec::new(),
            lines: Vec::new(),
            maxdepth: 0,
            size: (0, 0),
            width: 0,
            height: 0,
            current: 0,
            offset: 0,
            sort: Sort::default(),
            preview: Preview::default(),
            view_name: None,
            search: None,
            search_icase: false,
            search_dir: SearchDir::default(),
            filter: None,
            no_matches: false,
            help: false,
        }
    }
}

/// `mode_tree_is_lowercase`: a search with no capitals ignores case.
fn is_lowercase(s: &str) -> bool {
    !s.chars().any(char::is_uppercase)
}

/// `strstr` or `strcasestr` (ASCII case, as the C library's).
#[must_use]
pub fn contains(haystack: &str, needle: &str, icase: bool) -> bool {
    if icase {
        haystack
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
    } else {
        haystack.contains(needle)
    }
}

impl<K: Copy + PartialEq, D> ModeTree<K, D> {
    /// `mode_tree_start`: a screen of `size`, `-N` given `n` times, `-O`
    /// and `-r`, `-f`.
    #[must_use]
    pub fn new(size: (usize, usize), previews: usize, sort: Sort, filter: Option<String>) -> Self {
        Self {
            size,
            preview: match previews {
                0 => Preview::Normal,
                1 => Preview::Off,
                _ => Preview::Big,
            },
            sort,
            filter,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    #[must_use]
    pub fn item(&self, id: usize) -> &Item<K, D> {
        &self.items[id]
    }

    /// The parent of item `id`.
    #[must_use]
    pub fn parent(&self, id: usize) -> Option<usize> {
        self.items[id].parent
    }

    #[must_use]
    pub fn has_children(&self, id: usize) -> bool {
        !self.items[id].children.is_empty()
    }

    /// Item `id` is on a line and its parent's line is the last of its
    /// list (`mtd->line_list[mti->parent->line].last`).
    #[must_use]
    pub fn parent_last(&self, id: usize) -> bool {
        self.items[id]
            .parent
            .is_some_and(|p| self.lines.get(self.items[p].line).is_some_and(|l| l.last))
    }

    #[must_use]
    pub fn maxdepth(&self) -> usize {
        self.maxdepth
    }

    /// The current line's item (`mode_tree_get_current`).
    #[must_use]
    pub fn current_item(&self) -> Option<&Item<K, D>> {
        self.lines.get(self.current).map(|l| &self.items[l.item])
    }

    /// `mode_tree_build`: the items again from `build` (given the sort
    /// and the filter; it returns the tag to select, if it chooses one),
    /// built again without the filter when nothing matches it; then the
    /// lines, their keys from `key` (the item's data and line), the view's
    /// height, and the selected line, kept on its tag.
    pub fn build(
        &mut self,
        mut build: impl FnMut(&mut Build<K, D>, &Sort, Option<&str>) -> Option<K>,
        mut key: impl FnMut(&D, usize) -> Option<Key>,
    ) {
        let mut tag = self.current_item().map(|i| i.tag);
        let saved: Vec<_> = self
            .items
            .iter()
            .map(|i| (i.tag, i.tagged, i.expanded))
            .collect();
        let mut b = Build {
            items: Vec::new(),
            roots: Vec::new(),
            saved,
        };
        let mut chosen = build(&mut b, &self.sort, self.filter.as_deref());
        self.no_matches = b.roots.is_empty();
        if self.no_matches {
            b.items.clear();
            chosen = build(&mut b, &self.sort, None);
        }
        if chosen.is_some() {
            tag = chosen;
        }
        self.items = b.items;
        self.roots = b.roots;
        self.lines.clear();
        self.maxdepth = 0;
        let roots = self.roots.clone();
        self.build_lines(&roots, 0, &mut key);
        if tag.is_none() {
            tag = self.current_item().map(|i| i.tag);
        }
        self.width = self.size.0;
        if self.preview == Preview::Off {
            self.height = self.size.1;
        } else {
            self.set_height();
        }
        if let Some(tag) = tag {
            self.set_current(tag);
        } else {
            self.set_current_index(None);
        }
        self.check_selected();
    }

    /// `mode_tree_build_lines`.
    fn build_lines(
        &mut self,
        list: &[usize],
        depth: usize,
        key: &mut impl FnMut(&D, usize) -> Option<Key>,
    ) {
        self.maxdepth = self.maxdepth.max(depth);
        let mut flat = true;
        let first = self.lines.len();
        for (n, &id) in list.iter().enumerate() {
            let line = self.lines.len();
            self.lines.push(Line {
                item: id,
                depth,
                last: n + 1 == list.len(),
                flat: false,
            });
            self.items[id].line = line;
            if !self.items[id].children.is_empty() {
                flat = false;
            }
            if self.items[id].expanded {
                let children = self.items[id].children.clone();
                self.build_lines(&children, depth + 1, key);
            }
            self.items[id].key = key(&self.items[id].data, line).filter(|k| *k != Key::NONE);
        }
        for l in &mut self.lines[first..] {
            if l.depth == depth {
                l.flat = flat;
            }
        }
    }

    /// `mode_tree_set_height` with no height callback.
    fn set_height(&mut self) {
        let sy = self.size.1;
        let n = self.lines.len();
        self.height = match self.preview {
            Preview::Normal => {
                let mut h = (sy / 3) * 2;
                if h > n {
                    h = sy / 2;
                }
                if h < 10 { sy } else { h }
            }
            Preview::Big => (sy / 4).min(n).max(2),
            Preview::Off => sy,
        };
        if sy.saturating_sub(self.height) < 2 {
            self.height = sy;
        }
    }

    /// `mode_tree_check_selected`: no empty lines below the list, and the
    /// selected line in view.
    fn check_selected(&mut self) {
        let (n, h) = (self.lines.len(), self.height);
        if h == 0 {
            return;
        }
        if n <= h {
            self.offset = 0;
        } else if self.offset > n - h {
            self.offset = n - h;
        }
        if self.current < self.offset {
            self.offset = self.current;
        } else if self.current > self.offset + h - 1 {
            self.offset = self.current + 1 - h;
        }
    }

    /// The line of the item tagged `tag` (`mode_tree_get_tag`).
    #[must_use]
    pub fn find(&self, tag: K) -> Option<usize> {
        self.lines
            .iter()
            .position(|l| self.items[l.item].tag == tag)
    }

    /// `mode_tree_set_current`: select the line with `tag`, or keep the
    /// selection within the lines. Whether `tag` was found.
    pub fn set_current(&mut self, tag: K) -> bool {
        let found = self.find(tag);
        self.set_current_index(found);
        found.is_some()
    }

    fn set_current_index(&mut self, found: Option<usize>) {
        if let Some(i) = found {
            self.current = i;
            self.check_selected();
        } else if self.current >= self.lines.len() && !self.lines.is_empty() {
            self.current = self.lines.len() - 1;
            self.check_selected();
        }
    }

    /// `mode_tree_expand`: expand the item with `tag` if it is on a line.
    pub fn expand(&mut self, env: &mut dyn Env<K, D>, tag: K) {
        if let Some(i) = self.find(tag) {
            let id = self.lines[i].item;
            if !self.items[id].expanded {
                self.items[id].expanded = true;
                env.rebuild(self);
            }
        }
    }

    /// `mode_tree_count_tagged`.
    #[must_use]
    pub fn count_tagged(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| self.items[l.item].tagged)
            .count()
    }

    /// `mode_tree_each_tagged` with `current`: the tagged lines' data, or
    /// the current line's when none is tagged.
    #[must_use]
    pub fn tagged_or_current(&self) -> Vec<&D> {
        let tagged: Vec<_> = self
            .lines
            .iter()
            .map(|l| &self.items[l.item])
            .filter(|i| i.tagged)
            .map(|i| &i.data)
            .collect();
        if tagged.is_empty() {
            self.current_item().map(|i| &i.data).into_iter().collect()
        } else {
            tagged
        }
    }

    /// `mode_tree_up`.
    fn up(&mut self, wrap: bool) {
        if self.lines.is_empty() {
            return;
        }
        if self.current == 0 {
            if wrap {
                self.current = self.lines.len() - 1;
                if self.lines.len() >= self.height {
                    self.offset = self.lines.len() - self.height;
                }
            }
        } else {
            self.current -= 1;
            if self.current < self.offset {
                self.offset -= 1;
            }
        }
    }

    /// `mode_tree_down`.
    fn down(&mut self, wrap: bool) {
        if self.lines.is_empty() {
            return;
        }
        if self.current == self.lines.len() - 1 {
            if wrap {
                self.current = 0;
                self.offset = 0;
            }
        } else {
            self.current += 1;
            if self.current + 1 > self.offset + self.height {
                self.offset += 1;
            }
        }
    }

    /// `mode_tree_swap`: the next line at the current one's depth with the
    /// same parent, `direction` lines at a time.
    #[must_use]
    pub fn swap_partner(&self, direction: isize) -> Option<usize> {
        let depth = self.lines.get(self.current)?.depth;
        let mut at = self.current;
        loop {
            at = at.checked_add_signed(direction)?;
            let d = self.lines.get(at)?.depth;
            if d <= depth {
                return (d == depth).then_some(at);
            }
        }
    }

    /// The siblings of item `id` (its parent's children, or the roots).
    fn siblings(&self, id: usize) -> &[usize] {
        match self.items[id].parent {
            Some(p) => &self.items[p].children,
            None => &self.roots,
        }
    }

    fn next_sibling(&self, id: usize) -> Option<usize> {
        let s = self.siblings(id);
        let i = s.iter().position(|&x| x == id)?;
        s.get(i + 1).copied()
    }

    fn prev_sibling(&self, id: usize) -> Option<usize> {
        let s = self.siblings(id);
        let i = s.iter().position(|&x| x == id)?;
        i.checked_sub(1).map(|i| s[i])
    }

    /// The last item of `id`'s subtree, going down last children.
    fn last_descendant(&self, mut id: usize) -> usize {
        while let Some(&last) = self.items[id].children.last() {
            id = last;
        }
        id
    }

    /// `mode_tree_search_forward` and `mode_tree_search_backward`: the
    /// next item in tree order (collapsed ones included) that matches,
    /// wrapping, not the current one.
    fn search_from(&self, env: &dyn Env<K, D>) -> Option<usize> {
        let search = self.search.as_deref()?;
        let last = self.lines.get(self.current)?.item;
        let mut id = last;
        loop {
            id = if self.search_dir == SearchDir::Forward {
                if let Some(&first) = self.items[id].children.first() {
                    first
                } else if let Some(next) = self.next_sibling(id) {
                    next
                } else {
                    let mut up = self.items[id].parent;
                    loop {
                        match up {
                            None => break self.roots[0],
                            Some(p) => match self.next_sibling(p) {
                                Some(next) => break next,
                                None => up = self.items[p].parent,
                            },
                        }
                    }
                }
            } else if let Some(prev) = self.prev_sibling(id) {
                self.last_descendant(prev)
            } else if let Some(p) = self.items[id].parent {
                p
            } else {
                self.last_descendant(*self.roots.last()?)
            };
            if id == last {
                return None;
            }
            if env.matches(&self.items[id].data, search, self.search_icase) {
                return Some(id);
            }
        }
    }

    /// `mode_tree_search_set`: select the next match, its parents expanded.
    pub fn search_set(&mut self, env: &mut dyn Env<K, D>) {
        let Some(id) = self.search_from(env) else {
            return;
        };
        let tag = self.items[id].tag;
        let mut up = self.items[id].parent;
        while let Some(p) = up {
            self.items[p].expanded = true;
            up = self.items[p].parent;
        }
        env.rebuild(self);
        self.set_current(tag);
    }

    /// `mode_tree_search_callback` for a finished search line (`None`
    /// cancelled): the search is set, empty clears it.
    pub fn search_for(&mut self, env: &mut dyn Env<K, D>, s: Option<&str>) {
        match s.filter(|s| !s.is_empty()) {
            None => self.search = None,
            Some(s) => {
                self.search = Some(s.to_owned());
                self.search_icase = is_lowercase(s);
                self.search_set(env);
            }
        }
    }

    /// `mode_tree_filter_callback`: the filter is set (empty or cancelled
    /// clears it) and the items are built again.
    pub fn filter_for(&mut self, env: &mut dyn Env<K, D>, s: Option<&str>) {
        self.filter = s.filter(|s| !s.is_empty()).map(str::to_owned);
        env.rebuild(self);
    }

    /// `mode_tree_key` after the prompt: the help screen, shortcut keys,
    /// then the keys every tree mode has. `typed`: the key came with a
    /// mouse event (`m != NULL`), as a terminal's keys do; `t` then moves
    /// down. Mouse keys are not here yet.
    #[allow(clippy::too_many_lines, reason = "one switch, as mode_tree_key")]
    pub fn key(&mut self, env: &mut dyn Env<K, D>, key: Key, typed: bool) -> Action {
        if self.lines.is_empty() {
            return Action::Finished;
        }
        if self.help {
            if key != Key::FOCUS_IN && key != Key::FOCUS_OUT {
                self.help = false;
            }
            return Action::Key(None);
        }
        let k = key.key_and_modifiers();
        if let Some(i) = self
            .lines
            .iter()
            .position(|l| self.items[l.item].key == Some(k))
        {
            self.current = i;
            return Action::Key(Some(Key::ascii(b'\r')));
        }
        let c = |b: u8| Key::ascii(b);
        let line = self.lines[self.current];
        let current = line.item;
        match k {
            _ if [c(b'q'), c(0x1b), c(b'[').ctrl(), c(b'g').ctrl()].contains(&k) => {
                return Action::Finished;
            }
            _ if k == Key::F1 || k == c(b'h').ctrl() => self.help = true,
            _ if [Key::UP, c(b'k'), c(b'p').ctrl()].contains(&k) => self.up(true),
            _ if [Key::DOWN, c(b'j'), c(b'n').ctrl()].contains(&k) => self.down(true),
            _ if k == Key::UP.shift() || k == c(b'K') => return Action::Swap(-1),
            _ if k == Key::DOWN.shift() || k == c(b'J') => return Action::Swap(1),
            _ if k == Key::PPAGE || k == c(b'b').ctrl() => {
                for _ in 0..self.height {
                    if self.current == 0 {
                        break;
                    }
                    self.up(true);
                }
            }
            _ if k == Key::NPAGE || k == c(b'f').ctrl() => {
                for _ in 0..self.height {
                    if self.current == self.lines.len() - 1 {
                        break;
                    }
                    self.down(true);
                }
            }
            _ if k == c(b'g') || k == Key::HOME => {
                self.current = 0;
                self.offset = 0;
            }
            _ if k == c(b'G') || k == Key::END => {
                self.current = self.lines.len() - 1;
                self.offset = (self.current + 1).saturating_sub(self.height);
            }
            _ if k == c(b't') => {
                // Parents and children are never both tagged.
                if self.items[current].no_tag {
                } else if self.items[current].tagged {
                    self.items[current].tagged = false;
                } else {
                    let mut up = self.items[current].parent;
                    while let Some(p) = up {
                        self.items[p].tagged = false;
                        up = self.items[p].parent;
                    }
                    self.clear_tagged(current);
                    self.items[current].tagged = true;
                }
                if typed {
                    self.down(false);
                }
            }
            _ if k == c(b'T') => {
                for l in &self.lines {
                    self.items[l.item].tagged = false;
                }
            }
            _ if k == c(b't').ctrl() => {
                for i in 0..self.lines.len() {
                    let id = self.lines[i].item;
                    self.items[id].tagged = match self.items[id].parent {
                        None => !self.items[id].no_tag,
                        Some(p) => self.items[p].no_tag,
                    };
                }
            }
            _ if k == c(b'O') => {
                self.sort.next();
                env.rebuild(self);
            }
            _ if k == c(b'r') => {
                self.sort.reversed = !self.sort.reversed;
                env.rebuild(self);
            }
            _ if [Key::LEFT, c(b'h'), c(b'-')].contains(&k) => {
                let target = if line.flat || !self.items[current].expanded {
                    self.items[current].parent
                } else {
                    Some(current)
                };
                match target {
                    None => self.up(false),
                    Some(t) => {
                        self.items[t].expanded = false;
                        self.current = self.items[t].line;
                        env.rebuild(self);
                    }
                }
            }
            _ if [Key::RIGHT, c(b'l'), c(b'+')].contains(&k) => {
                if line.flat || self.items[current].expanded {
                    self.down(false);
                } else {
                    self.items[current].expanded = true;
                    env.rebuild(self);
                }
            }
            _ if k == c(b'-').meta() || k == c(b'+').meta() => {
                let expanded = k == c(b'+').meta();
                for &r in &self.roots {
                    self.items[r].expanded = expanded;
                }
                env.rebuild(self);
            }
            _ if [c(b'?'), c(b'/'), c(b's').ctrl()].contains(&k) => {
                self.search_dir = SearchDir::Forward;
                return Action::Search;
            }
            _ if k == c(b'n') || k == c(b'N') => {
                self.search_dir = if k == c(b'n') {
                    SearchDir::Forward
                } else {
                    SearchDir::Backward
                };
                self.search_set(env);
            }
            _ if k == c(b'f') => return Action::Filter,
            _ if k == c(b'c') => return Action::ClearFilter,
            _ if k == c(b'v') => {
                self.preview = match self.preview {
                    Preview::Off => Preview::Big,
                    Preview::Normal => Preview::Off,
                    Preview::Big => Preview::Normal,
                };
                env.rebuild(self);
                if self.preview != Preview::Off {
                    self.check_selected();
                }
            }
            _ => return Action::Key(Some(key)),
        }
        // A key mode-tree handled is the mode's too, as tmux hands it on.
        Action::Key(Some(key))
    }

    /// `mode_tree_clear_tagged` on `id`'s children.
    fn clear_tagged(&mut self, id: usize) {
        for c in self.items[id].children.clone() {
            self.items[c].tagged = false;
            self.clear_tagged(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree of `a` (children `a0`, `a1`) and `b` (child `b0`), by name.
    #[derive(Default)]
    struct Fixed {
        builds: usize,
    }

    fn fill(b: &mut Build<&'static str, &'static str>, filter: Option<&str>) {
        for (root, kids) in [("a", &["a0", "a1"][..]), ("b", &["b0"][..])] {
            let r = b.add(None, root, root, root.into(), None, None);
            let mut any = false;
            for k in kids {
                if filter.is_none_or(|f| k.contains(f)) {
                    b.add(Some(r), k, k, (*k).into(), None, None);
                    any = true;
                }
            }
            if !any {
                b.remove(r);
            }
        }
    }

    impl Env<&'static str, &'static str> for Fixed {
        fn rebuild(&mut self, tree: &mut ModeTree<&'static str, &'static str>) {
            self.builds += 1;
            tree.build(
                |b, _, f| {
                    fill(b, f);
                    None
                },
                |_, line| {
                    u8::try_from(line)
                        .ok()
                        .filter(|l| *l < 10)
                        .map(|l| Key::from_char(char::from(b'0' + l)))
                },
            );
        }
        fn matches(&self, data: &&'static str, search: &str, icase: bool) -> bool {
            contains(data, search, icase)
        }
    }

    fn tree() -> (ModeTree<&'static str, &'static str>, Fixed) {
        let mut t = ModeTree::new((20, 10), 1, Sort::default(), None);
        let mut env = Fixed::default();
        env.rebuild(&mut t);
        (t, env)
    }

    fn names(t: &ModeTree<&'static str, &'static str>) -> Vec<&'static str> {
        t.lines().iter().map(|l| t.item(l.item).tag).collect()
    }

    /// mode-tree.c:308-362: expanded items' children follow them; keys go
    /// by line; collapsing with `h` on a child goes to its parent.
    #[test]
    fn lines_expand_and_collapse() {
        let (mut t, mut env) = tree();
        assert_eq!(names(&t), ["a", "a0", "a1", "b", "b0"]);
        assert!(!t.lines()[0].flat && t.lines()[1].flat);
        assert_eq!(t.item(t.lines()[2].item).key, Some(Key::from_char('2')));
        t.key(&mut env, Key::from_char('j'), false);
        t.key(&mut env, Key::from_char('h'), false);
        assert_eq!(names(&t), ["a", "b", "b0"]);
        assert_eq!(t.current, 0);
        t.key(&mut env, Key::from_char('l'), false);
        assert_eq!(names(&t), ["a", "a0", "a1", "b", "b0"]);
        t.key(&mut env, Key::from_char('-').meta(), false);
        assert_eq!(names(&t), ["a", "b"]);
    }

    /// A shortcut key selects its line and becomes Enter.
    #[test]
    fn shortcut_keys_choose() {
        let (mut t, mut env) = tree();
        assert_eq!(
            t.key(&mut env, Key::from_char('3'), false),
            Action::Key(Some(Key::ascii(b'\r')))
        );
        assert_eq!(t.current, 3);
    }

    /// Tagging: parents and children are not both tagged; C-t tags the
    /// roots; tags survive a build.
    #[test]
    fn tags() {
        let (mut t, mut env) = tree();
        t.key(&mut env, Key::from_char('j'), false);
        t.key(&mut env, Key::from_char('t'), false);
        assert_eq!(t.count_tagged(), 1);
        t.key(&mut env, Key::from_char('g'), false);
        t.key(&mut env, Key::from_char('t'), false);
        assert!(t.item(t.lines()[0].item).tagged);
        assert!(!t.item(t.lines()[1].item).tagged);
        env.rebuild(&mut t);
        assert_eq!(t.count_tagged(), 1);
        t.key(&mut env, Key::from_char('t').ctrl(), false);
        assert_eq!(t.count_tagged(), 2);
        t.key(&mut env, Key::from_char('T'), false);
        assert_eq!(t.count_tagged(), 0);
    }

    /// The search walks collapsed items too and expands the match's
    /// parents; the filter keeps what is collapsed collapsed, and one that
    /// matches nothing shows everything.
    #[test]
    fn search_and_filter() {
        let (mut t, mut env) = tree();
        t.key(&mut env, Key::from_char('-').meta(), false);
        t.search_for(&mut env, Some("b0"));
        assert_eq!(names(&t), ["a", "b", "b0"]);
        assert_eq!(t.current, 2);
        t.search_for(&mut env, Some("A"));
        assert_eq!(t.current, 2, "case matters with a capital");
        t.filter_for(&mut env, Some("1"));
        assert_eq!(names(&t), ["a"]);
        assert!(!t.no_matches);
        t.filter_for(&mut env, Some("zz"));
        assert!(t.no_matches);
        assert_eq!(names(&t), ["a", "b", "b0"]);
    }

    /// `O` steps through the mode's orders, wrapping.
    #[test]
    fn sort_cycles() {
        let mut s = Sort {
            order: Some(SortOrder::Size),
            reversed: false,
            seq: &[SortOrder::Index, SortOrder::Name],
        };
        s.next();
        assert_eq!(s.order, Some(SortOrder::Index));
        s.next();
        assert_eq!(s.order, Some(SortOrder::Name));
        s.next();
        assert_eq!(s.order, Some(SortOrder::Index));
    }
}
