//! `customize-mode` (window-customize.c): the options, hooks, environments
//! and key tables in a [`ModeTree`], each section a root; the preview is
//! the item's description and values. Enter and `s` set the current item
//! (a prompt, or a flag or choice toggled), `S` sets it globally, `w` for
//! the window; `u` unsets it and `U` the tagged ones; `d` resets it to its
//! default and `D` the tagged ones; `a` renames an array item; `C` shows
//! only what changed; Enter on a section adds an item to it.
//!
//! `H`, which tmux handles but does not document, hides the global
//! options.
//!
//! `e` edits the current value in `editor` (`window_customize_start_edit`):
//! tmux runs the editor in a modal floating pane, vtmux in a popup over
//! the client ([`crate::popup::Editor`], compat/DIVERGENCES.md D-028);
//! while it runs the mode shows `WAITING FOR EDITOR` and takes only `q`,
//! Escape and C-c, which end it. A right-click opens the mode's menu
//! ([`super::tree_mode`]).

use vtmux_core::colour::Colour;
use vtmux_core::ids::{PaneId, SessionId};
use vtmux_core::keys::Key;
use vtmux_core::options::{ArrayKey, Entry, Kind, OptionDef, OptionValue, Scope, TreeId};
use vtmux_core::style::{CellStyle, Style};

use super::tree::{Build, ModeTree, Sort};
use super::tree_mode::{Common, TreeMode, rebuild};
use crate::open_prompt::{Line, Reply};
use crate::prompt::Kind as PromptKind;
use crate::prompt_history::PromptType;
use crate::render::tree::Canvas;
use crate::render::window::Source;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `WINDOW_CUSTOMIZE_DEFAULT_FORMAT`.
pub const DEFAULT_FORMAT: &str = "#{?is_option,\
#{?option_is_global,,#[reverse](#{option_scope})#[default] }\
#[fg=themelightgrey]#[ignore]#{option_value}\
#{?option_unit, #{option_unit},},\
#{?is_environment,#[fg=themelightgrey]#[ignore]#{environment_value},#{key}}}";

/// `enum window_customize_scope` without `NONE` and `KEY`: where an option
/// or environment item lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    Server,
    GlobalSession,
    Session,
    GlobalWindow,
    Window,
    Pane,
    GlobalEnvironment,
    SessionEnvironment,
}

impl Place {
    /// The server, global session or global window options.
    fn is_global(self) -> bool {
        matches!(
            self,
            Self::Server | Self::GlobalSession | Self::GlobalWindow
        )
    }
}

/// Options or hooks (`enum window_customize_option_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Options,
    Hooks,
}

/// A line's item (`struct window_customize_itemdata`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A section's title: nothing to set, Enter adds to it.
    Title(Title),
    Option {
        section: Section,
        place: Place,
        /// The tree the item was built from (`item->oo`).
        tree: TreeId,
        name: String,
        key: Option<ArrayKey>,
    },
    Environment {
        place: Place,
        /// The session for a session environment (`item->environ`).
        session: Option<SessionId>,
        name: String,
    },
    /// A key binding, or its Command, Note or Repeat line.
    Key {
        table: String,
        key: Key,
        line: KeyLine,
    },
}

/// A section title (`mode_tree_add` with no item data).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Title {
    Options(Section, Scope),
    Environment(Place),
    Keys(String),
}

/// Which line of a key binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyLine {
    Binding,
    Command,
    Note,
    Repeat,
}

/// What a line is, which its tag is made from: stable across builds, as
/// `window_customize_get_tag`'s pointers and table offsets are.
#[derive(Hash)]
enum TagOf<'a> {
    Title(&'a Title),
    Option(TreeId, &'a str, Option<&'a ArrayKey>),
    Environment(Option<SessionId>, &'a str),
    Key(&'a str, Key, KeyLine),
}

/// A line's tag: a hash of its [`TagOf`] (`DefaultHasher::new` has fixed
/// keys, so it is the same every build).
pub type Tag = u64;

fn tag(of: &TagOf<'_>) -> Tag {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    of.hash(&mut h);
    h.finish()
}

/// `struct window_customize_modedata`.
#[derive(Clone, Debug)]
pub struct CustomizeMode {
    pub common: Common,
    pub tree: ModeTree<Tag, Item>,
    /// The target the mode started from (`data->fs`).
    start: Found,
    format: String,
    hide_global: bool,
    hide_default: bool,
    /// `-y` (`PROMPT_ACCEPT`).
    accept: bool,
    /// The pid of the editor open for this mode (`data->editor`,
    /// `spawn_get_editor_pid`), which the waiting box shows.
    editing: Option<i32>,
}

/// What an editor started by `e` is editing (`window_customize_editdata`):
/// the item as it was, which must still be valid when the editor exits.
#[derive(Clone, Debug)]
pub struct Edit {
    item: Item,
}

/// The `customize-mode` arguments the mode reads.
pub struct Options {
    pub previews: usize,
    pub filter: Option<String>,
    pub format: Option<String>,
    pub accept: bool,
}

/// What a prompt of the mode is for.
#[derive(Clone, Debug)]
enum Ask {
    /// `window_customize_set_option_callback`.
    SetOption {
        section: Section,
        tree: TreeId,
        name: String,
        key: Option<ArrayKey>,
    },
    /// `window_customize_set_environment_callback`.
    SetEnvironment {
        session: Option<SessionId>,
        name: String,
        hidden: bool,
    },
    /// `window_customize_add_option_callback`.
    AddOption { section: Section, tree: TreeId },
    /// `window_customize_add_environment_callback`.
    AddEnvironment { session: Option<SessionId> },
    /// `window_customize_set_array_key_callback`.
    ArrayKey {
        tree: TreeId,
        name: String,
        key: ArrayKey,
    },
    /// `window_customize_set_command_callback`.
    Command { table: String, key: Key },
    /// `window_customize_set_note_callback`.
    Note { table: String, key: Key },
    /// `window_customize_add_key_callback`.
    AddKey { table: String },
    /// `window_customize_change_current_callback`.
    Current(Change),
    /// `window_customize_change_tagged_callback`.
    Tagged(Change),
}

/// `enum window_customize_change`.
#[derive(Clone, Copy, Debug)]
enum Change {
    Unset,
    Reset,
}

#[derive(Debug)]
struct CustomizePrompt(Ask);

impl super::prompt::ModePromptOwner for CustomizePrompt {
    fn input(&mut self, server: &mut Server, p: PaneId, c: Option<ClientId>, line: Line) -> Reply {
        let Line::Done(value) = line else {
            return Reply::Continue;
        };
        server.customize_prompt_input(p, c, &self.0, value.as_deref());
        Reply::Close
    }
}

/// The trees of a target (`window_customize_get_tree`).
#[derive(Clone, Copy, Debug)]
struct Trees {
    session: Option<SessionId>,
    window: Option<vtmux_core::ids::WindowId>,
    pane: Option<PaneId>,
}

impl Trees {
    fn of(found: Found) -> Self {
        Self {
            session: found.session,
            window: found.window,
            pane: found.pane,
        }
    }

    fn tree(self, place: Place) -> Option<TreeId> {
        Some(match place {
            Place::Server => TreeId::Server,
            Place::GlobalSession => TreeId::GlobalSession,
            Place::Session => TreeId::Session(self.session?),
            Place::GlobalWindow => TreeId::GlobalWindow,
            Place::Window => TreeId::Window(self.window?),
            Place::Pane => TreeId::Pane(self.pane?),
            Place::GlobalEnvironment | Place::SessionEnvironment => return None,
        })
    }
}

/// The place of tree `t`.
fn place_of(t: TreeId) -> Place {
    match t {
        TreeId::Server => Place::Server,
        TreeId::GlobalSession => Place::GlobalSession,
        TreeId::Session(_) => Place::Session,
        TreeId::GlobalWindow => Place::GlobalWindow,
        TreeId::Window(_) => Place::Window,
        TreeId::Pane(_) => Place::Pane,
    }
}

/// `oe->flags & OPTIONS_TABLE_IS_STYLE` and `IS_COLOUR`.
fn string_check(def: &OptionDef) -> Option<vtmux_core::options::StringCheck> {
    match def.kind {
        Kind::String(c) => Some(c),
        _ => None,
    }
}

impl CustomizeMode {
    /// The pid of the editor open for the mode.
    #[must_use]
    pub fn editing(&self) -> Option<i32> {
        self.editing
    }

    /// `window_customize_init` up to `mode_tree_build`.
    #[must_use]
    pub fn new(pane: PaneId, size: (usize, usize), start: Found, o: Options) -> Self {
        let mut tree = ModeTree::new(size, o.previews, Sort::default(), o.filter);
        tree.preview_rows = Some(12);
        Self {
            common: Common::new(pane),
            tree,
            start,
            format: o.format.unwrap_or_else(|| DEFAULT_FORMAT.to_owned()),
            hide_global: false,
            hide_default: false,
            editing: None,
            accept: o.accept,
        }
    }

    pub fn build(&mut self, server: &Server) {
        rebuild(self, server);
    }

    /// The mode's target now (`data->fs` if still valid, else the pane's).
    fn found(&self, server: &Server) -> Found {
        if self.start.still_valid(&server.model) {
            self.start
        } else {
            Found::from_pane(&server.model, self.common.pane)
        }
    }

    /// `window_customize_check_item`: the item's tree or environment is
    /// still the one its place names for the mode's target.
    fn check(&self, server: &Server, item: &Item) -> bool {
        let trees = Trees::of(self.found(server));
        match item {
            Item::Option { place, tree, .. } => trees.tree(*place) == Some(*tree),
            Item::Environment { place, session, .. } => match place {
                Place::GlobalEnvironment => true,
                _ => session.is_some() && *session == trees.session,
            },
            Item::Key { .. } | Item::Title(_) => true,
        }
    }
}

/// `window_customize_scope_text`.
fn scope_text(model: &crate::model::Model, place: Place, found: Found) -> String {
    match place {
        Place::Pane => found
            .pane
            .and_then(|p| model.pane_index(p))
            .map(|i| format!("pane {i}"))
            .unwrap_or_default(),
        Place::Session | Place::SessionEnvironment => found
            .session
            .and_then(|s| model.sessions.get(&s))
            .map(|s| format!("session {}", s.name))
            .unwrap_or_default(),
        Place::Window => found
            .index
            .map(|i| format!("window {i}"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The options of a section (`window_customize_build_options`): tree
/// `trees[0]` gives the names, a deeper one the value it holds.
struct OptionSection<'a> {
    title: Title,
    section: Section,
    trees: &'a [TreeId],
}

impl TreeMode for CustomizeMode {
    type Tag = Tag;
    type Data = Item;
    const HELP: &'static [(&'static str, &'static str)] = &[
        ("   Enter, s", "Set %1 value"),
        ("          S", "Set global %1 value"),
        ("          w", "Set window %1 value"),
        ("          d", "Set to default value"),
        ("          D", "Set tagged %1s to default value"),
        ("          u", "Unset an %1"),
        ("          U", "Unset tagged %1s"),
        ("          a", "Change array key"),
        ("          e", "Open %1 value in editor"),
        ("          f", "Enter a filter"),
        ("          C", "Toggle only changed items"),
        ("          v", "Toggle information"),
    ];
    const HELP_WIDTH: usize = 52;
    const HELP_ITEM: &'static str = "item";
    /// `window_customize_menu_items` (window-customize.c); `Tag All` sends
    /// `\024`, as tmux does.
    const MENU: &'static [(&'static str, Key)] = &[
        ("Select", Key::ascii(b'\r')),
        ("Edit", Key::ascii(b'e')),
        ("Expand", Key::RIGHT),
        ("", Key::NONE),
        ("Tag", Key::ascii(b't')),
        ("Tag All", Key::ascii(0x14)),
        ("Tag None", Key::ascii(b'T')),
        ("", Key::NONE),
        ("Changed Only", Key::ascii(b'C')),
        ("", Key::NONE),
        ("Cancel", Key::ascii(b'q')),
    ];

    fn tree(&self) -> &ModeTree<Tag, Item> {
        &self.tree
    }
    fn tree_mut(&mut self) -> &mut ModeTree<Tag, Item> {
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
            super::Mode::Customize(t) => Some(t),
            _ => None,
        }
    }
    fn into_mode(self: Box<Self>) -> super::Mode {
        super::Mode::Customize(self)
    }
    fn from_mode(mode: super::Mode) -> Option<Box<Self>> {
        match mode {
            super::Mode::Customize(t) => Some(t),
            _ => None,
        }
    }

    /// `window_customize_build`.
    fn build_items(
        &self,
        server: &Server,
        b: &mut Build<Tag, Item>,
        _sort: &Sort,
        filter: Option<&str>,
    ) -> Option<Tag> {
        let found = self.found(server);
        let t = Trees::of(found);
        let session: Vec<TreeId> = [TreeId::GlobalSession]
            .into_iter()
            .chain(t.session.map(TreeId::Session))
            .collect();
        let window: Vec<TreeId> = [TreeId::GlobalWindow]
            .into_iter()
            .chain(t.window.map(TreeId::Window))
            .chain(t.pane.map(TreeId::Pane))
            .collect();
        let sections = [
            OptionSection {
                title: Title::Options(Section::Options, Scope::Server),
                section: Section::Options,
                trees: &[TreeId::Server],
            },
            OptionSection {
                title: Title::Options(Section::Options, Scope::Session),
                section: Section::Options,
                trees: &session,
            },
            OptionSection {
                title: Title::Options(Section::Options, Scope::Window),
                section: Section::Options,
                trees: &window,
            },
            OptionSection {
                title: Title::Options(Section::Hooks, Scope::Session),
                section: Section::Hooks,
                trees: &session,
            },
            OptionSection {
                title: Title::Options(Section::Hooks, Scope::Window),
                section: Section::Hooks,
                trees: &window,
            },
        ];
        for s in &sections {
            self.build_options(server, b, s, found, filter);
        }
        self.build_environment(server, b, Place::GlobalEnvironment, found, filter);
        self.build_environment(server, b, Place::SessionEnvironment, found, filter);
        for (name, table) in server.key_bindings.tables() {
            if !table.is_empty() {
                self.build_keys(server, b, name, table, found, filter);
            }
        }
        None
    }

    /// No `keycb`: lines 0-9 are `0`-`9`, 10-35 `M-a`-`M-z`.
    fn item_key(&self, _server: &Server, _data: &Item, line: usize) -> Option<Key> {
        let line = u8::try_from(line).ok()?;
        match line {
            0..=9 => Some(Key::ascii(b'0' + line)),
            10..=35 => Some(Key::ascii(b'a' + line - 10).meta()),
            _ => None,
        }
    }

    /// No `searchcb`: the name.
    fn matches(&self, _server: &Server, name: &str, _: &Item, search: &str, icase: bool) -> bool {
        super::tree::contains(name, search, icase)
    }

    fn mode_key(
        server: &mut Server,
        p: PaneId,
        c: Option<ClientId>,
        key: Key,
        item: Option<Item>,
    ) -> bool {
        server.customize_key(p, c, key, item.as_ref());
        false
    }

    /// `window_customize_draw`.
    fn draw_preview(
        &self,
        source: &Source<'_>,
        canvas: &mut Canvas,
        data: &Item,
        at: (usize, usize),
        size: (usize, usize),
    ) {
        let mut w = Writer {
            canvas,
            x: at.0,
            y: at.1,
            cx: at.0,
            cy: at.1,
            sx: size.0,
            sy: size.1,
        };
        match data {
            Item::Key { table, key, .. } => draw_key(source, &mut w, table, *key),
            Item::Environment { .. } => self.draw_environment(source, &mut w, data),
            Item::Option { .. } => self.draw_option(source, &mut w, data),
            Item::Title(_) => {}
        }
    }
}

/// The title of a section, as tmux names it.
fn title_name(t: &Title) -> String {
    match t {
        Title::Options(Section::Options, Scope::Server) => "Server Options".into(),
        Title::Options(Section::Options, Scope::Session) => "Session Options".into(),
        Title::Options(Section::Options, _) => "Window & Pane Options".into(),
        Title::Options(Section::Hooks, Scope::Session | Scope::Server) => "Session Hooks".into(),
        Title::Options(Section::Hooks, _) => "Window & Pane Hooks".into(),
        Title::Environment(Place::GlobalEnvironment) => "Global Environment".into(),
        Title::Environment(_) => "Session Environment".into(),
        Title::Keys(name) => format!("Key Table - {name}"),
    }
}

/// The format variables of one option or environment line.
type Vars = Vec<(std::borrow::Cow<'static, str>, String)>;

impl CustomizeMode {
    fn expand(server: &Server, found: Found, vars: &Vars, text: &str) -> String {
        let mut ctx =
            crate::format_ctx::Ctx::new(&server.model, &server.info, &server.global_env, found);
        ctx.clients = server.client_views();
        ctx.extra.extend(vars.iter().cloned());
        vtmux_core::format::expand_str(text, &ctx)
    }

    /// `window_customize_build_options` and `window_customize_build_option`.
    fn build_options(
        &self,
        server: &Server,
        b: &mut Build<Tag, Item>,
        s: &OptionSection<'_>,
        found: Found,
        filter: Option<&str>,
    ) {
        let store = &server.model.options;
        let top = b.add(
            None,
            tag(&TagOf::Title(&s.title)),
            Item::Title(s.title.clone()),
            title_name(&s.title),
            None,
            Some(false),
        );
        b.set_no_tag(top);
        // User options from every tree, in the order the trees list them,
        // then the first tree's table options; each from the deepest tree
        // that holds it.
        let mut names: Vec<&str> = Vec::new();
        for &t in s.trees {
            for (name, _) in store.tree(t).iter() {
                if name.starts_with('@') && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        for (name, _) in store.tree(s.trees[0]).iter() {
            if !name.starts_with('@') {
                names.push(name);
            }
        }
        let mut count = 0;
        for name in names {
            let Some(&tree) = s
                .trees
                .iter()
                .rev()
                .find(|&&t| store.tree(t).get_only(name).is_some())
            else {
                continue;
            };
            let entry = store.tree(tree).get_only(name).expect("found above");
            count += self.build_option(server, b, top, s.section, tree, name, entry, found, filter);
        }
        if self.hide_default && count == 0 {
            b.remove(top);
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "window_customize_build_option"
    )]
    fn build_option(
        &self,
        server: &Server,
        b: &mut Build<Tag, Item>,
        top: usize,
        section: Section,
        tree: TreeId,
        name: &str,
        entry: &Entry,
        found: Found,
        filter: Option<&str>,
    ) -> usize {
        let store = &server.model.options;
        let def = entry.def;
        let is_hook = def.is_some_and(|d| d.hook);
        let monitor = store.monitor(tree, name).map(|m| m.describe(name));
        let is_user_hook = name.starts_with('@') && store.is_hook_event(name);
        let any_hook = is_hook || monitor.is_some() || is_user_hook;
        if (section == Section::Hooks) != any_hook {
            return 0;
        }
        let array = entry.is_array();
        let place = place_of(tree);
        let global = place.is_global();
        if self.hide_global && global {
            return 0;
        }
        if self.hide_default && !option_changed(store, name, entry, None) {
            return 0;
        }
        let flag = |b: bool| u8::from(b).to_string();
        let mut vars: Vars = vec![
            ("is_option".into(), "1".into()),
            ("is_key".into(), "0".into()),
            ("is_environment".into(), "0".into()),
            ("option_name".into(), name.to_owned()),
            ("option_is_global".into(), flag(global)),
            ("option_is_array".into(), flag(array)),
            ("option_is_hook".into(), flag(is_hook)),
            ("option_is_monitor".into(), flag(monitor.is_some())),
            (
                "option_scope".into(),
                scope_text(&server.model, place, found),
            ),
            (
                "option_unit".into(),
                def.and_then(|d| d.unit).unwrap_or_default().to_owned(),
            ),
            ("option_monitor".into(), monitor.unwrap_or_default()),
        ];
        if !array {
            vars.push(("option_value".into(), entry.value.to_text(def, None, false)));
        }
        if let Some(f) = filter
            && !vtmux_core::format::is_true(&Self::expand(server, found, &vars, f))
        {
            return 0;
        }
        let item = Item::Option {
            section,
            place,
            tree,
            name: name.to_owned(),
            key: None,
        };
        let text = (!array).then(|| Self::expand(server, found, &vars, &self.format));
        let id = b.add(
            Some(top),
            tag(&TagOf::Option(tree, name, None)),
            item,
            name.to_owned(),
            text,
            Some(false),
        );
        if !array {
            return 1;
        }
        // window_customize_build_array.
        let OptionValue::Array(items) = &entry.value else {
            return 1;
        };
        let mut count = 1;
        for k in items.keys() {
            if self.hide_default && !option_changed(store, name, entry, Some(k)) {
                continue;
            }
            let full = format!("{name}[{k}]");
            // format_add replaces the name and adds the item's value.
            let mut vars = vars.clone();
            vars.retain(|(n, _)| n != "option_name");
            vars.push(("option_name".into(), full.clone()));
            vars.push((
                "option_value".into(),
                entry.value.to_text(def, Some(k), false),
            ));
            let text = Self::expand(server, found, &vars, &self.format);
            b.add(
                Some(id),
                tag(&TagOf::Option(tree, name, Some(k))),
                Item::Option {
                    section,
                    place,
                    tree,
                    name: name.to_owned(),
                    key: Some(k.clone()),
                },
                full,
                Some(text),
                None,
            );
            count += 1;
        }
        count
    }

    /// `window_customize_build_environment`.
    fn build_environment(
        &self,
        server: &Server,
        b: &mut Build<Tag, Item>,
        place: Place,
        found: Found,
        filter: Option<&str>,
    ) {
        if self.hide_default {
            return;
        }
        let title = Title::Environment(place);
        let top = b.add(
            None,
            tag(&TagOf::Title(&title)),
            Item::Title(title.clone()),
            title_name(&title),
            None,
            Some(false),
        );
        b.set_no_tag(top);
        let session = match place {
            Place::GlobalEnvironment => None,
            _ => found.session,
        };
        let env = match session {
            None if place == Place::GlobalEnvironment => &server.global_env,
            None => return,
            Some(s) => match server.model.sessions.get(&s) {
                Some(x) => &x.env,
                None => return,
            },
        };
        let flag = |b: bool| u8::from(b).to_string();
        for (name, e) in env.iter() {
            let vars: Vars = vec![
                ("is_option".into(), "0".into()),
                ("is_key".into(), "0".into()),
                ("is_environment".into(), "1".into()),
                (
                    "environment_is_global".into(),
                    flag(place == Place::GlobalEnvironment),
                ),
                (
                    "environment_scope".into(),
                    scope_text(&server.model, place, found),
                ),
                ("environment_name".into(), name.to_owned()),
                ("environment_hidden".into(), flag(e.hidden)),
                ("environment_removed".into(), flag(e.value.is_none())),
                (
                    "environment_value".into(),
                    e.value.clone().unwrap_or_default(),
                ),
            ];
            if let Some(f) = filter
                && !vtmux_core::format::is_true(&Self::expand(server, found, &vars, f))
            {
                continue;
            }
            let (shown, text) = match &e.value {
                None => (format!("-{name}"), None),
                Some(_) => (
                    name.to_owned(),
                    Some(Self::expand(server, found, &vars, &self.format)),
                ),
            };
            b.add(
                Some(top),
                tag(&TagOf::Environment(session, name)),
                Item::Environment {
                    place,
                    session,
                    name: name.to_owned(),
                },
                shown,
                text,
                Some(false),
            );
        }
    }

    /// `window_customize_build_keys`.
    fn build_keys(
        &self,
        server: &Server,
        b: &mut Build<Tag, Item>,
        name: &str,
        table: &vtmux_core::keys::KeyTable,
        found: Found,
        filter: Option<&str>,
    ) {
        let title = Title::Keys(name.to_owned());
        let top = b.add(
            None,
            tag(&TagOf::Title(&title)),
            Item::Title(title.clone()),
            title_name(&title),
            None,
            Some(false),
        );
        b.set_no_tag(top);
        let mut count = 0;
        for bd in table.bindings() {
            if self.hide_default && !key_changed(table, bd) {
                continue;
            }
            let key_name = bd.key.name(false);
            let mut vars: Vars = vec![
                ("is_option".into(), "0".into()),
                ("is_key".into(), "1".into()),
                ("is_environment".into(), "0".into()),
                ("key".into(), key_name.clone()),
            ];
            if let Some(n) = &bd.note {
                vars.push(("key_note".into(), n.clone()));
            }
            if let Some(f) = filter
                && !vtmux_core::format::is_true(&Self::expand(server, found, &vars, f))
            {
                continue;
            }
            let item = |line| Item::Key {
                table: name.to_owned(),
                key: bd.key,
                line,
            };
            let tag = |line| tag(&TagOf::Key(name, bd.key, line));
            let expanded = Self::expand(server, found, &vars, &self.format);
            let child = b.add(
                Some(top),
                tag(KeyLine::Binding),
                item(KeyLine::Binding),
                expanded,
                None,
                Some(false),
            );
            let lines = [
                (
                    KeyLine::Command,
                    "Command",
                    format!(
                        "#[fg=themelightgrey]#[ignore]{}",
                        bd.commands.print(vtmux_core::cmd::PrintFlags::default())
                    ),
                ),
                (
                    KeyLine::Note,
                    "Note",
                    bd.note
                        .as_ref()
                        .map(|n| format!("#[fg=themelightgrey]#[ignore]{n}"))
                        .unwrap_or_default(),
                ),
                (
                    KeyLine::Repeat,
                    "Repeat",
                    format!(
                        "#[fg=themelightgrey]#[ignore]{}",
                        if bd.repeat { "on" } else { "off" }
                    ),
                ),
            ];
            for (line, label, text) in lines {
                let id = b.add(
                    Some(child),
                    tag(line),
                    item(line),
                    label.to_owned(),
                    Some(text),
                    None,
                );
                b.set_draw_as_parent(id);
                b.set_no_tag(id);
            }
            count += 1;
        }
        if self.hide_default && count == 0 {
            b.remove(top);
        }
    }
}

/// `window_customize_option_is_changed`: user options and user hooks are
/// always changed.
fn option_changed(
    store: &vtmux_core::options::OptionsStore,
    name: &str,
    entry: &Entry,
    key: Option<&ArrayKey>,
) -> bool {
    if entry.def.is_none() || (name.starts_with('@') && store.is_hook_event(name)) {
        return true;
    }
    vtmux_core::options::is_changed(entry, key)
}

/// `window_customize_key_is_changed`.
fn key_changed(table: &vtmux_core::keys::KeyTable, bd: &vtmux_core::keys::Binding) -> bool {
    let Some(d) = table.get_default(bd.key) else {
        return true;
    };
    d.repeat != bd.repeat
        || d.note != bd.note
        || d.commands.print(vtmux_core::cmd::PrintFlags::default())
            != bd.commands.print(vtmux_core::cmd::PrintFlags::default())
}

/// A `screen_write_ctx` over the preview's area: the cursor and the area.
struct Writer<'a> {
    canvas: &'a mut Canvas,
    /// The cursor.
    x: usize,
    y: usize,
    /// The area's corner and size.
    cx: usize,
    cy: usize,
    sx: usize,
    sy: usize,
}

impl Writer<'_> {
    /// Rows left from the cursor (`sy - (s->cy - cy)`).
    fn left(&self) -> usize {
        self.sy.saturating_sub(self.y - self.cy)
    }

    /// `screen_write_cursormove(ctx, cx, s->cy + 1, 0)` and the check after
    /// it: `false` when no room is left.
    fn skip_line(&mut self) -> bool {
        self.y += 1;
        self.x = self.cx;
        self.y < self.cy + self.sy - 1
    }

    /// `screen_write_text` (screen-write.c:494-580) in `lines` rows from
    /// the cursor: words wrapped at spaces, `more` leaving the cursor after
    /// the text. `false` when it did not all fit.
    fn text(&mut self, lines: usize, more: bool, style: &CellStyle, text: &str) -> bool {
        if lines == 0 {
            return false;
        }
        let chars: Vec<(char, usize)> = text
            .chars()
            .map(|c| (c, usize::from(vtmux_core::width::char_width(c))))
            .collect();
        let (cx, width, cy) = (self.cx, self.sx, self.y);
        let mut left = (cx + width).saturating_sub(self.x);
        let mut idx = 0;
        loop {
            let mut at = 0;
            let mut end = idx;
            while end < chars.len() {
                let (c, w) = chars[end];
                if c == '\n' || at + w > left {
                    break;
                }
                at += w;
                end += 1;
            }
            let next = if end == chars.len() {
                end
            } else if chars[end].0 == '\n' || chars[end].0 == ' ' {
                end + 1
            } else {
                match (idx + 1..=end).rev().find(|&i| chars[i].0 == ' ') {
                    Some(i) => {
                        end = i;
                        i + 1
                    }
                    None => end,
                }
            };
            for &(c, w) in &chars[idx..end] {
                self.put(c, w, style);
            }
            idx = next;
            if self.y == cy + lines - 1 || idx == chars.len() {
                break;
            }
            self.y += 1;
            self.x = cx;
            left = width;
        }
        if (self.y == cy + lines - 1 && (!more || self.x == cx + width)) || idx != chars.len() {
            return false;
        }
        if !more || self.x == cx + width {
            self.y += 1;
            self.x = cx;
        }
        true
    }

    /// `screen_write_cell` of one character: control characters are
    /// dropped (`screen_write_cell_is_single` fails and nothing prints).
    fn put(&mut self, c: char, w: usize, style: &CellStyle) {
        if (c as u32) < 0x20 || c == '\u{7f}' {
            return;
        }
        if w == 0 {
            return;
        }
        let mut x = self.x;
        self.canvas.put_wide(&mut x, self.y, style, c, w);
        self.x = x;
    }

    /// `window_customize_write_value`: the label, then the value in light
    /// grey.
    fn value(&mut self, lines: usize, label: &str, value: &str) -> bool {
        if lines == 0 {
            return false;
        }
        let cy = self.y;
        if !self.text(lines, true, &CellStyle::default(), label) {
            return false;
        }
        if self.y - cy >= lines {
            return false;
        }
        let lines = lines - (self.y - cy);
        let grey = CellStyle {
            fg: Colour::Theme(2),
            ..CellStyle::default()
        };
        self.text(lines, false, &grey, value)
    }
}

/// `window_customize_draw_key`.
fn draw_key(source: &Source<'_>, w: &mut Writer<'_>, table: &str, key: Key) {
    let Some(kt) = source.keys.table(table) else {
        return;
    };
    let Some(bd) = kt.get(key) else {
        return;
    };
    let plain = CellStyle::default();
    let note = bd
        .note
        .as_deref()
        .unwrap_or("There is no note for this key.");
    let period = if !note.is_empty() && !note.ends_with('.') {
        "."
    } else {
        ""
    };
    if !w.text(w.sy, false, &plain, &format!("{note}{period}")) || !w.skip_line() {
        return;
    }
    let left = w.left();
    if !w.text(
        left,
        false,
        &plain,
        &format!("This key is in the {table} table."),
    ) {
        return;
    }
    let left = w.left();
    if !w.value(left, "Repeat: ", if bd.repeat { "on" } else { "off" }) || !w.skip_line() {
        return;
    }
    let cmd = bd.commands.print(vtmux_core::cmd::PrintFlags::default());
    let left = w.left();
    if !w.value(left, "Command: ", &cmd) {
        return;
    }
    if let Some(d) = kt.get_default(key) {
        let default = d.commands.print(vtmux_core::cmd::PrintFlags::default());
        if default != cmd {
            let left = w.left();
            w.value(left, "The default is: ", &default);
        }
    }
}

impl CustomizeMode {
    /// `window_customize_draw_option`.
    #[allow(clippy::too_many_lines, reason = "one function in tmux")]
    fn draw_option(&self, source: &Source<'_>, w: &mut Writer<'_>, item: &Item) {
        let Item::Option {
            place,
            tree,
            name,
            key,
            ..
        } = item
        else {
            return;
        };
        let model = source.model;
        let store = &model.options;
        let found = if self.start.still_valid(model) {
            self.start
        } else {
            Found::from_pane(model, self.common.pane)
        };
        if Trees::of(found).tree(*place) != Some(*tree) {
            return;
        }
        let Some(holder) = store.holder(*tree, name) else {
            return;
        };
        let Some(entry) = store.tree(holder).get_only(name) else {
            return;
        };
        let def = entry.def;
        let is_hook = def.is_some_and(|d| d.hook);
        let monitor = store.monitor(holder, name).map(|m| m.describe(name));
        let is_user_hook = name.starts_with('@') && store.is_hook_event(name);
        let any_hook = is_hook || monitor.is_some() || is_user_hook;
        let (space, unit) = match def.and_then(|d| d.unit) {
            Some(u) => (" ", u),
            None => ("", ""),
        };
        let plain = CellStyle::default();
        let text = match def.and_then(|d| d.text) {
            Some(t) => t,
            None if monitor.is_some() => "This hook runs when a monitor changes.",
            None if is_user_hook => "This hook doesn't have a description.",
            None => "This option doesn't have a description.",
        };
        if !w.text(w.sy, false, &plain, text) || !w.skip_line() {
            return;
        }
        let kind = match def.map(|d| d.scope) {
            None => "user",
            Some(Scope::WindowPane) => "window and pane",
            Some(Scope::Window) => "window",
            Some(Scope::Session) => "session",
            Some(Scope::Server) => "server",
        };
        let line = if monitor.is_some() {
            "This is a monitor hook.".to_owned()
        } else if is_user_hook {
            "This is a user hook.".to_owned()
        } else if is_hook {
            format!("This is a {kind} hook.")
        } else {
            format!("This is a {kind} option.")
        };
        let left = w.left();
        if !w.text(left, false, &plain, &line) {
            return;
        }
        if let Some(m) = &monitor {
            let left = w.left();
            if !w.value(left, "Monitor: ", m) {
                return;
            }
        }
        if def.is_some_and(|d| d.array) {
            if is_hook {
                if key.is_none() {
                    let left = w.left();
                    if w.text(left, false, &plain, "This is an array hook.") {
                        Self::hook_fire(source, w, holder, name);
                    }
                    return;
                }
            } else {
                let line = match key {
                    Some(k) => format!("This is an array option, key {k}."),
                    None => "This is an array option.".to_owned(),
                };
                let left = w.left();
                if !w.text(left, false, &plain, &line) {
                    return;
                }
            }
            if key.is_none() {
                return;
            }
        }
        if !w.skip_line() {
            return;
        }
        let value = entry.value.to_text(def, key.as_ref(), false);
        let default = def
            .filter(|_| key.is_none())
            .map(vtmux_core::options::default_text)
            .filter(|d| *d != value);
        let left = w.left();
        if any_hook {
            if !w.value(left, "Hook command: ", &format!("{value}{space}{unit}")) {
                return;
            }
            if !Self::hook_fire(source, w, holder, name) {
                return;
            }
        } else if !w.value(left, "Option value: ", &format!("{value}{space}{unit}")) {
            return;
        }
        let mut ctx = crate::format_ctx::Ctx::new(model, source.info, source.env, found);
        ctx.clients = source.clients.views().clone();
        if def.is_none_or(|d| matches!(d.kind, Kind::String(_))) {
            let expanded = vtmux_core::format::expand_str(&value, &ctx);
            if expanded != value {
                let left = w.left();
                if !w.value(left, "This expands to: ", &expanded) {
                    return;
                }
            }
        }
        if let Some(Kind::Choice(names)) = def.map(|d| d.kind) {
            let left = w.left();
            if !w.value(left, "Available values are: ", &names.join(", ")) {
                return;
            }
        }
        let example = |w: &mut Writer<'_>, label: &str, style: CellStyle| -> bool {
            let left = w.left();
            if !w.text(left, true, &plain, label) {
                return false;
            }
            let left = w.left();
            w.text(left, false, &style, "EXAMPLE")
        };
        if let Some(d) = def {
            use vtmux_core::options::StringCheck;
            if d.kind == Kind::Colour {
                let fg = match &entry.value {
                    OptionValue::Single(vtmux_core::options::Value::Colour(c)) => *c,
                    _ => Colour::Default,
                };
                let style = CellStyle {
                    fg,
                    ..CellStyle::default()
                };
                if !example(w, "This is a colour option: ", style) {
                    return;
                }
            }
            let check = string_check(d);
            if check == Some(StringCheck::Colour) {
                let style = colour_style(&value, &ctx);
                if !example(w, "This is a colour option: ", style) {
                    return;
                }
            }
            if check == Some(StringCheck::Style) {
                let style = style_of(&value, &ctx);
                if !example(w, "This is a style option: ", style) {
                    return;
                }
            }
        }
        if let Some(d) = default {
            let left = w.left();
            if !w.value(left, "The default is: ", &format!("{d}{space}{unit}")) {
                return;
            }
        }
        w.y += 1;
        w.x = w.cx;
        if w.y > w.cy + w.sy - 1 {
            return;
        }
        if def.is_some_and(|d| d.array) {
            return;
        }
        // The window's and the global values, for a value set lower down.
        let (window, global) = match place {
            Place::Pane => {
                let wt = store.parent(*tree);
                (wt, wt.and_then(|t| store.parent(t)))
            }
            Place::Window | Place::Session => (None, store.parent(*tree)),
            _ => (None, None),
        };
        if let Some(wt) = window
            && holder != wt
            && let Some(e) = store.tree(wt).get_only(name)
        {
            let v = e.value.to_text(e.def, None, false);
            let label = format!(
                "Window value (from window {}): ",
                found.index.unwrap_or_default()
            );
            let left = w.left();
            if !w.value(left, &label, &format!("{v}{space}{unit}")) {
                return;
            }
        }
        if let Some(gt) = global
            && holder != gt
            && let Some(e) = store.tree(gt).get_only(name)
        {
            let v = e.value.to_text(e.def, None, false);
            let left = w.left();
            w.value(left, "Global value: ", &format!("{v}{space}{unit}"));
        }
    }

    /// `window_customize_write_hook_fire`: a monitor hook's own count.
    fn hook_fire(source: &Source<'_>, w: &mut Writer<'_>, tree: TreeId, name: &str) -> bool {
        let store = &source.model.options;
        let (count, time) = store
            .monitor(tree, name)
            .map_or_else(|| store.hook_fire_stats(tree, name), |m| m.fires);
        let left = w.left();
        let plain = CellStyle::default();
        if time != 0 {
            let when = vtmux_core::format::pretty_time(time, &vtmux_core::format::SystemWallClock);
            return w.text(
                left,
                false,
                &plain,
                &format!("This hook has been fired {count} times, last {when}."),
            );
        }
        w.text(
            left,
            false,
            &plain,
            &format!("This hook has been fired {count} times."),
        )
    }

    /// `window_customize_draw_environment`.
    fn draw_environment(&self, source: &Source<'_>, w: &mut Writer<'_>, item: &Item) {
        let Item::Environment {
            place,
            session,
            name,
        } = item
        else {
            return;
        };
        let model = source.model;
        let found = if self.start.still_valid(model) {
            self.start
        } else {
            Found::from_pane(model, self.common.pane)
        };
        let env = if *place == Place::GlobalEnvironment {
            source.env
        } else if *session != found.session {
            return;
        } else if let Some(s) = session.and_then(|s| model.sessions.get(&s)) {
            &s.env
        } else {
            return;
        };
        let Some(e) = env.find(name) else {
            return;
        };
        let plain = CellStyle::default();
        let kind = if *place == Place::GlobalEnvironment {
            "global"
        } else {
            "session"
        };
        if !w.text(
            w.sy,
            false,
            &plain,
            &format!("This is a {kind} environment variable."),
        ) {
            return;
        }
        if e.hidden {
            let left = w.left();
            if !w.text(left, false, &plain, "This variable is hidden.") {
                return;
            }
        }
        if !w.skip_line() {
            return;
        }
        let left = w.left();
        let ok = match &e.value {
            None => w.text(left, false, &plain, "Variable is removed."),
            Some(v) => w.value(left, "Variable value: ", v),
        };
        if !ok || *place != Place::SessionEnvironment {
            return;
        }
        let Some(g) = source.env.find(name) else {
            return;
        };
        let left = w.left();
        match &g.value {
            None => {
                w.text(left, false, &plain, "Global variable is removed.");
            }
            Some(v) => {
                w.value(left, "Global value: ", v);
            }
        }
    }
}

/// `style_apply` of a style option's value (expanded if it has `#{`).
fn style_of(value: &str, ctx: &crate::format_ctx::Ctx<'_>) -> CellStyle {
    let text = if value.contains("#{") {
        vtmux_core::format::expand_str(value, ctx)
    } else {
        value.to_owned()
    };
    let base = CellStyle::default();
    let mut sy = Style::new(&base);
    if sy.parse(&base, &text).is_err() {
        return base;
    }
    sy.cell
}

/// `style_apply` of a colour string option (`style_parse_colour`): the
/// colour as the foreground; empty or invalid is the default.
fn colour_style(value: &str, ctx: &crate::format_ctx::Ctx<'_>) -> CellStyle {
    let text = if value.contains("#{") {
        vtmux_core::format::expand_str(value, ctx)
    } else {
        value.to_owned()
    };
    let fg = Colour::parse(&text).unwrap_or(Colour::Default);
    CellStyle {
        fg,
        ..CellStyle::default()
    }
}

impl Server {
    /// The keys customize-mode handles itself (window-customize.c:2730-2920).
    #[allow(clippy::too_many_lines, reason = "one switch, as window_customize_key")]
    fn customize_key(&mut self, p: PaneId, c: Option<ClientId>, key: Key, item: Option<&Item>) {
        let ch = key
            .key_and_modifiers()
            .as_char()
            .filter(|_| !key.has_ctrl() && !key.has_meta());
        let Some(ch) = ch else { return };
        let Some((found, accept, tagged)) = self
            .with_tree::<CustomizeMode, _>(p, |s, t| (t.found(s), t.accept, t.tree.count_tagged()))
        else {
            return;
        };
        let valid = |server: &mut Self, item: &Item| {
            server
                .with_tree::<CustomizeMode, _>(p, |s, t| t.check(s, item))
                .unwrap_or(false)
        };
        let valid = item.is_some_and(|i| valid(self, i));
        match (ch, item) {
            ('\r' | 's', Some(Item::Title(title))) => self.customize_add(p, c, title, found),
            ('\r' | 's', Some(i @ Item::Key { .. })) => self.customize_set_key(p, c, i),
            ('\r' | 's', Some(i @ Item::Environment { .. })) if valid => {
                self.customize_set_environment(p, c, i, false, found);
            }
            ('\r' | 's', Some(i @ Item::Option { .. })) if valid => {
                self.customize_set_option(p, c, i, false, true, found);
            }
            ('w', Some(i @ Item::Option { .. })) if valid => {
                self.customize_set_option(p, c, i, false, false, found);
            }
            ('S' | 'W', Some(i @ Item::Environment { .. })) if valid => {
                self.customize_set_environment(p, c, i, true, found);
            }
            ('S' | 'W', Some(i @ Item::Option { .. })) if valid => {
                self.customize_set_option(p, c, i, true, false, found);
            }
            ('d', _) => {
                let name = match item {
                    Some(Item::Option {
                        name, key: None, ..
                    }) => name.clone(),
                    Some(Item::Key { key, .. }) => key.name(false),
                    _ => return,
                };
                self.customize_confirm(
                    p,
                    c,
                    &format!("Reset {name} to default? "),
                    Ask::Current(Change::Reset),
                    accept,
                );
            }
            ('D', _) if tagged != 0 => {
                self.customize_confirm(
                    p,
                    c,
                    &format!("Reset {tagged} tagged to default? "),
                    Ask::Tagged(Change::Reset),
                    accept,
                );
            }
            ('u', _) => {
                let label = match item {
                    None | Some(Item::Title(_)) => return,
                    Some(Item::Option {
                        name, key: Some(k), ..
                    }) => format!("Unset {name}[{k}]? "),
                    Some(Item::Option { name, .. } | Item::Environment { name, .. }) => {
                        format!("Unset {name}? ")
                    }
                    Some(Item::Key { key, .. }) => format!("Unset {}? ", key.name(false)),
                };
                self.customize_confirm(p, c, &label, Ask::Current(Change::Unset), accept);
            }
            ('U', _) if tagged != 0 => {
                self.customize_confirm(
                    p,
                    c,
                    &format!("Unset {tagged} tagged? "),
                    Ask::Tagged(Change::Unset),
                    accept,
                );
            }
            (
                'a',
                Some(Item::Option {
                    tree,
                    name,
                    key: Some(k),
                    ..
                }),
            ) if valid => {
                let ask = Ask::ArrayKey {
                    tree: *tree,
                    name: name.clone(),
                    key: k.clone(),
                };
                self.customize_prompt(p, c, &format!("({name}[{k}]) "), &k.to_string(), ask);
            }
            ('e', Some(i)) => {
                if let Some(c) = c {
                    self.customize_start_edit(p, c, i);
                }
            }
            ('C' | 'H', _) => {
                self.with_tree::<CustomizeMode, _>(p, |s, t| {
                    if ch == 'C' {
                        t.hide_default = !t.hide_default;
                    } else {
                        t.hide_global = !t.hide_global;
                    }
                    rebuild(t, s);
                });
            }
            _ => {}
        }
        // window_customize_key: these build the items again whatever they
        // did (a stale item is then gone).
        if matches!(ch, '\r' | 's' | 'w' | 'S' | 'W') {
            self.customize_rebuild(p);
        }
    }

    /// `window_customize_start_edit` (window-customize.c:2052-2115): the
    /// current option (not a flag or choice), key Command or Note, or
    /// environment variable, in the editor; an empty value is a newline.
    fn customize_start_edit(&mut self, p: PaneId, c: ClientId, item: &Item) {
        let valid = self
            .with_tree::<CustomizeMode, _>(p, |s, t| (t.editing.is_some(), t.check(s, item)))
            .unwrap_or((true, false));
        if valid.0 {
            return;
        }
        let value = match item {
            Item::Option {
                tree, name, key, ..
            } => {
                if !valid.1 {
                    return;
                }
                let Some(entry) = self.model.options.get(*tree, name) else {
                    return;
                };
                if entry
                    .def
                    .is_some_and(|d| matches!(d.kind, Kind::Flag | Kind::Choice(_)))
                {
                    return;
                }
                entry.value.to_text(entry.def, key.as_ref(), false)
            }
            Item::Key { table, key, line } => {
                let Some(bd) = self.key_bindings.get(table, *key) else {
                    return;
                };
                match line {
                    KeyLine::Command => bd.commands.print(vtmux_core::cmd::PrintFlags::default()),
                    KeyLine::Note => bd.note.clone().unwrap_or_default(),
                    _ => return,
                }
            }
            Item::Environment { session, name, .. } => {
                if !valid.1 {
                    return;
                }
                match self.env_of(*session).and_then(|e| e.find(name)) {
                    Some(e) if e.value.is_some() => e.value.clone().unwrap_or_default(),
                    _ => return,
                }
            }
            Item::Title(_) => return,
        };
        let value = if value.is_empty() {
            "\n".to_owned()
        } else {
            value
        };
        let edit = Edit { item: item.clone() };
        if self.spawn_editor(c, p, value.as_bytes(), crate::popup::Edit::Customize(edit)) {
            let pid = self.editor_pid(p);
            self.with_tree::<CustomizeMode, _>(p, |_, t| t.editing = pid);
        }
    }

    /// `window_customize_edit_close_cb` (window-customize.c:1979-2043):
    /// the editor is done; `value` is what it left in the file, `None`
    /// when it failed. Nothing changes if the mode is gone, the file is
    /// empty, or the item is no longer valid; one trailing newline goes.
    pub(crate) fn customize_edit_done(&mut self, p: PaneId, edit: &Edit, value: Option<Vec<u8>>) {
        // data->dead, or not this mode's editor (spawn_cancel_editor when
        // the mode went): nothing happens.
        if self.with_tree::<CustomizeMode, _>(p, |_, t| t.editing.take().is_some()) != Some(true) {
            return;
        }
        let Some(mut value) = value.filter(|v| !v.is_empty()) else {
            return;
        };
        if value.last() == Some(&b'\n') {
            value.pop();
        }
        let value = vtmux_core::rawstr::from_vec(value);
        let Some(found) = self.with_tree::<CustomizeMode, _>(p, |s, t| t.found(s)) else {
            return;
        };
        let item = &edit.item;
        let valid = self
            .with_tree::<CustomizeMode, _>(p, |s, t| t.check(s, item))
            .unwrap_or(false);
        match item {
            Item::Option {
                section,
                tree,
                name,
                key,
                ..
            } => {
                // window_customize_option_editable, then
                // window_customize_set_option_value.
                let editable = valid
                    && self.model.options.get(*tree, name).is_some_and(|e| {
                        !e.def
                            .is_some_and(|d| matches!(d.kind, Kind::Flag | Kind::Choice(_)))
                    });
                if !editable {
                    return;
                }
                let result = crate::commands::set_option::with_option_host(self, found, |o, h| {
                    o.set_from_prompt(*tree, name, key.as_ref(), &value, h)
                });
                if result.is_err() {
                    return;
                }
                if *section == Section::Hooks && name.starts_with('@') {
                    self.model.options.add_hook_event(name);
                }
                self.options_push_changes(name);
            }
            Item::Key { table, key, line } => match line {
                KeyLine::Command => {
                    let parsed = {
                        let mut env = crate::server::GlobalEnv::new(
                            &mut self.global_env,
                            &self.model,
                            &self.info,
                        );
                        vtmux_core::cmd::parse_string(&value, &mut env)
                    };
                    let Ok(list) = parsed else { return };
                    if let Some(bd) = self.key_bindings.get_mut(table, *key) {
                        bd.commands = list;
                    }
                }
                KeyLine::Note => {
                    if let Some(bd) = self.key_bindings.get_mut(table, *key) {
                        bd.note = (!value.is_empty()).then_some(value);
                    }
                }
                _ => return,
            },
            Item::Environment { session, name, .. } => {
                if !valid {
                    return;
                }
                let Some(env) = self.env_of_mut(*session) else {
                    return;
                };
                let hidden = env.find(name).is_some_and(|e| e.hidden);
                env.set(name, &value, hidden);
            }
            Item::Title(_) => return,
        }
        self.customize_rebuild(p);
    }

    /// `mode_tree_set_prompt` with `PROMPT_NOFORMAT` for one of the mode's
    /// line prompts.
    fn customize_prompt(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        label: &str,
        input: &str,
        ask: Ask,
    ) {
        self.tree_prompt(
            p,
            c,
            label,
            input,
            (PromptType::Command, PromptKind::Line),
            Box::new(CustomizePrompt(ask)),
        );
    }

    /// A single-key question (`PROMPT_SINGLE`); `-y` answers it at once.
    fn customize_confirm(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        label: &str,
        ask: Ask,
        accept: bool,
    ) {
        self.tree_prompt(
            p,
            c,
            label,
            "",
            (PromptType::Command, PromptKind::Single),
            Box::new(CustomizePrompt(ask)),
        );
        if accept && let Some(c) = c {
            self.mode_prompt_key(p, Some(c), Key::from_char('y'));
        }
    }

    /// `window_customize_add_current`: Enter on a section's title.
    fn customize_add(&mut self, p: PaneId, c: Option<ClientId>, title: &Title, found: Found) {
        let trees = Trees::of(found);
        match title {
            Title::Options(section, scope) => {
                let tree = match scope {
                    Scope::Server => Some(TreeId::Server),
                    Scope::Session => trees.tree(Place::Session),
                    _ => trees.tree(Place::Pane),
                };
                let Some(tree) = tree else { return };
                let what = match section {
                    Section::Hooks => "hook",
                    Section::Options => "option",
                };
                self.customize_prompt(
                    p,
                    c,
                    &format!("New user {what}: "),
                    "@",
                    Ask::AddOption {
                        section: *section,
                        tree,
                    },
                );
            }
            Title::Environment(place) => {
                let session = match place {
                    Place::GlobalEnvironment => None,
                    _ => match found.session {
                        Some(s) => Some(s),
                        None => return,
                    },
                };
                self.customize_prompt(
                    p,
                    c,
                    "New environment: ",
                    "",
                    Ask::AddEnvironment { session },
                );
            }
            Title::Keys(table) => {
                self.customize_prompt(
                    p,
                    c,
                    &format!("New key in {table}: "),
                    "",
                    Ask::AddKey {
                        table: table.clone(),
                    },
                );
            }
        }
    }

    /// `window_customize_set_option`: a flag or choice changes at once,
    /// anything else asks for its value. `global` is `S`; `pane` is false
    /// for `w`.
    fn customize_set_option(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        item: &Item,
        global: bool,
        pane: bool,
        found: Found,
    ) {
        let Item::Option {
            section,
            place,
            tree,
            name,
            key,
        } = item
        else {
            return;
        };
        let store = &self.model.options;
        let Some(entry) = store.get(*tree, name) else {
            return;
        };
        let def = entry.def;
        let pane = pane && def.is_none_or(|d| d.scope == Scope::WindowPane);
        let array = def.is_some_and(|d| d.array);
        let to = if array {
            *place
        } else if global {
            match place {
                Place::Session => Place::GlobalSession,
                Place::Window | Place::Pane => Place::GlobalWindow,
                other => *other,
            }
        } else {
            match place {
                Place::GlobalSession => Place::Session,
                Place::Window | Place::Pane | Place::GlobalWindow if pane => Place::Pane,
                Place::Window | Place::Pane | Place::GlobalWindow => Place::Window,
                other => *other,
            }
        };
        let target = if to == *place {
            Some(*tree)
        } else {
            Trees::of(found).tree(to)
        };
        let Some(target) = target else { return };
        if let Some(d) = def
            && matches!(d.kind, Kind::Flag | Kind::Choice(_))
        {
            self.model.options.toggle(target, d);
            self.options_push_changes(d.name);
            self.customize_rebuild(p);
            return;
        }
        let text = scope_text(&self.model, to, found);
        let space = if !text.is_empty() {
            ", for "
        } else if to == Place::Server {
            ""
        } else {
            ", global"
        };
        let label = match (array, key) {
            (true, None) => format!("({name}[+]{space}{text}) "),
            (true, Some(k)) => format!("({name}[{k}]{space}{text}) "),
            (false, _) => format!("({name}{space}{text}) "),
        };
        let value = entry.value.to_text(def, key.as_ref(), false);
        self.customize_prompt(
            p,
            c,
            &label,
            &value,
            Ask::SetOption {
                section: *section,
                tree: target,
                name: name.clone(),
                key: key.clone(),
            },
        );
    }

    /// `window_customize_set_environment`.
    fn customize_set_environment(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        item: &Item,
        global: bool,
        found: Found,
    ) {
        let Item::Environment {
            place,
            session,
            name,
        } = item
        else {
            return;
        };
        let Some(e) = self.env_of(*session).and_then(|env| env.find(name)) else {
            return;
        };
        let (value, hidden) = (e.value.clone().unwrap_or_default(), e.hidden);
        let (to, session) = if global {
            (Place::GlobalEnvironment, None)
        } else {
            (*place, *session)
        };
        let text = scope_text(&self.model, to, found);
        let space = if !text.is_empty() {
            ", for "
        } else if to == Place::GlobalEnvironment {
            ", global"
        } else {
            ""
        };
        self.customize_prompt(
            p,
            c,
            &format!("({name}{space}{text}) "),
            &value,
            Ask::SetEnvironment {
                session,
                name: name.clone(),
                hidden,
            },
        );
    }

    /// The global environment, or session `s`'s.
    fn env_of(&self, s: Option<SessionId>) -> Option<&crate::environ::Environ> {
        match s {
            None => Some(&self.global_env),
            Some(s) => self.model.sessions.get(&s).map(|x| &x.env),
        }
    }

    fn env_of_mut(&mut self, s: Option<SessionId>) -> Option<&mut crate::environ::Environ> {
        match s {
            None => Some(&mut self.global_env),
            Some(s) => self.model.sessions.get_mut(&s).map(|x| &mut x.env),
        }
    }

    /// `window_customize_set_key`: Repeat toggles, Command and Note ask.
    fn customize_set_key(&mut self, p: PaneId, c: Option<ClientId>, item: &Item) {
        let Item::Key { table, key, line } = item else {
            return;
        };
        let Some(bd) = self.key_bindings.get(table, *key) else {
            return;
        };
        let label = format!("({}) ", key.name(false));
        match line {
            KeyLine::Repeat => {
                if let Some(bd) = self.key_bindings.get_mut(table, *key) {
                    bd.repeat = !bd.repeat;
                }
                self.customize_rebuild(p);
            }
            KeyLine::Command => {
                let value = bd.commands.print(vtmux_core::cmd::PrintFlags::default());
                let ask = Ask::Command {
                    table: table.clone(),
                    key: *key,
                };
                self.customize_prompt(p, c, &label, &value, ask);
            }
            KeyLine::Note => {
                let value = bd.note.clone().unwrap_or_default();
                let ask = Ask::Note {
                    table: table.clone(),
                    key: *key,
                };
                self.customize_prompt(p, c, &label, &value, ask);
            }
            KeyLine::Binding => self.customize_rebuild(p),
        }
    }

    /// `mode_tree_build` and `mode_tree_draw` for the mode on `p`.
    fn customize_rebuild(&mut self, p: PaneId) {
        self.with_tree::<CustomizeMode, _>(p, |s, t| rebuild(t, s));
    }

    /// The prompts' callbacks.
    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        reason = "one callback per prompt, as tmux"
    )]
    fn customize_prompt_input(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        ask: &Ask,
        s: Option<&str>,
    ) {
        let Some(s) = s else { return };
        let Some(found) = self.with_tree::<CustomizeMode, _>(p, |sv, t| t.found(sv)) else {
            return;
        };
        let trees = Trees::of(found);
        // window_customize_check_item for the prompt's own item.
        let valid_tree = |t: TreeId| trees.tree(place_of(t)) == Some(t);
        let valid_session = |sess: Option<SessionId>| sess.is_none() || sess == trees.session;
        let fail = |server: &mut Self, msg: String| {
            if let Some(c) = c {
                let mut chars = msg.chars();
                let msg = chars
                    .next()
                    .map(|f| f.to_uppercase().chain(chars).collect())
                    .unwrap_or_default();
                server.status_message_set(c, msg);
            }
        };
        match ask {
            Ask::SetOption {
                section,
                tree,
                name,
                key,
            } => {
                if s.is_empty()
                    || !valid_tree(*tree)
                    || self.model.options.get(*tree, name).is_none()
                {
                    return;
                }
                let result = crate::commands::set_option::with_option_host(self, found, |o, h| {
                    o.set_from_prompt(*tree, name, key.as_ref(), s, h)
                });
                if let Err(e) = result {
                    fail(self, e.to_string());
                    return;
                }
                if *section == Section::Hooks && name.starts_with('@') {
                    self.model.options.add_hook_event(name);
                }
                self.options_push_changes(name);
            }
            Ask::SetEnvironment {
                session,
                name,
                hidden,
            } => {
                if !valid_session(*session) {
                    return;
                }
                let Some(env) = self.env_of_mut(*session) else {
                    return;
                };
                let hidden = env.find(name).map_or(*hidden, |e| e.hidden);
                env.set(name, s, hidden);
            }
            Ask::AddOption { section, tree } => {
                if s.is_empty() || !valid_tree(*tree) {
                    return;
                }
                let n = s.find([' ', '\t']).unwrap_or(s.len());
                let value = s[n..].trim_start_matches([' ', '\t']);
                if n == 0 || value.is_empty() {
                    fail(self, "User option must be @name value".into());
                    return;
                }
                let name = &s[..n];
                if !name.starts_with('@') || name.contains('[') {
                    let what = match section {
                        Section::Hooks => "hook",
                        Section::Options => "option",
                    };
                    fail(self, format!("User {what} name must start with @"));
                    return;
                }
                self.model
                    .options
                    .add_user(*tree, name, value, *section == Section::Hooks);
                self.options_push_changes(name);
            }
            Ask::AddEnvironment { session } => {
                if s.is_empty() || !valid_session(*session) {
                    return;
                }
                if let Some(rest) = s.strip_prefix('-') {
                    if rest.is_empty() || rest.contains('=') {
                        fail(self, format!("Bad environment variable: {s}"));
                        return;
                    }
                    if let Some(env) = self.env_of_mut(*session) {
                        env.clear(rest);
                    }
                } else {
                    let Some(eq) = s.find('=').filter(|&i| i != 0) else {
                        fail(self, "Environment variable must be NAME=value".into());
                        return;
                    };
                    if let Some(env) = self.env_of_mut(*session) {
                        env.set(&s[..eq], &s[eq + 1..], false);
                    }
                }
            }
            Ask::ArrayKey { tree, name, key } => {
                if s.is_empty() || !valid_tree(*tree) {
                    return;
                }
                let result = crate::commands::set_option::with_option_host(self, found, |o, h| {
                    o.rename_array_key(*tree, name, key, s, h)
                });
                match result {
                    Err(e) => {
                        fail(self, e.to_string());
                        return;
                    }
                    Ok(false) => return,
                    Ok(true) => self.options_push_changes(name),
                }
            }
            Ask::Command { table, key } => {
                if s.is_empty() || self.key_bindings.get(table, *key).is_none() {
                    return;
                }
                let parsed = {
                    let mut env = crate::server::GlobalEnv::new(
                        &mut self.global_env,
                        &self.model,
                        &self.info,
                    );
                    vtmux_core::cmd::parse_string(s, &mut env)
                };
                match parsed {
                    Err(e) => {
                        fail(self, e.to_string());
                        return;
                    }
                    Ok(list) => {
                        if let Some(bd) = self.key_bindings.get_mut(table, *key) {
                            bd.commands = list;
                        }
                    }
                }
            }
            Ask::Note { table, key } => {
                if s.is_empty() {
                    return;
                }
                let Some(bd) = self.key_bindings.get_mut(table, *key) else {
                    return;
                };
                bd.note = Some(s.to_owned());
            }
            Ask::AddKey { table } => {
                if s.is_empty() {
                    return;
                }
                let n = s.find([' ', '\t']).unwrap_or(s.len());
                let command = s[n..].trim_start_matches([' ', '\t']);
                if n == 0 || command.is_empty() {
                    fail(self, "Key binding must be key command".into());
                    return;
                }
                let Some(key) = Key::from_name(&s[..n]).filter(|k| *k != Key::NONE) else {
                    fail(self, format!("Unknown key: {}", &s[..n]));
                    return;
                };
                let parsed = {
                    let mut env = crate::server::GlobalEnv::new(
                        &mut self.global_env,
                        &self.model,
                        &self.info,
                    );
                    vtmux_core::cmd::parse_string(command, &mut env)
                };
                match parsed {
                    Err(e) => {
                        fail(self, e.to_string());
                        return;
                    }
                    Ok(list) => self.key_bindings.add(table, key, None, false, Some(list)),
                }
            }
            Ask::Current(change) => {
                if !is_yes(s) {
                    return;
                }
                let item = self
                    .with_tree::<CustomizeMode, _>(p, |_, t| {
                        t.tree.current_item().map(|i| i.data.clone())
                    })
                    .flatten();
                if let Some(item) = item {
                    self.customize_change(p, &item, *change, true);
                }
            }
            Ask::Tagged(change) => {
                if !is_yes(s) {
                    return;
                }
                let items: Vec<Item> = self
                    .with_tree::<CustomizeMode, _>(p, |_, t| {
                        t.tree
                            .tagged()
                            .into_iter()
                            .map(|(_, d)| d.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                for item in items {
                    self.customize_change(p, &item, *change, false);
                }
            }
        }
        self.customize_rebuild(p);
    }

    /// `window_customize_change_each` and the current item's change: unset
    /// or reset `item`. `current`: the item is the current line (tmux
    /// compares it with `mode_tree_get_current`), so an item that goes
    /// moves the selection up first.
    fn customize_change(&mut self, p: PaneId, item: &Item, change: Change, current: bool) {
        let valid = self
            .with_tree::<CustomizeMode, _>(p, |s, t| t.check(s, item))
            .unwrap_or(false);
        let is_current = current
            || self
                .with_tree::<CustomizeMode, _>(p, |_, t| {
                    t.tree.current_item().is_some_and(|i| i.data == *item)
                })
                .unwrap_or(false);
        let up = |server: &mut Self| {
            if is_current {
                server.with_tree::<CustomizeMode, _>(p, |_, t| t.tree.up(false));
            }
        };
        match (change, item) {
            (Change::Unset, Item::Key { table, key, .. }) => {
                if self.key_bindings.get(table, *key).is_none() {
                    return;
                }
                up(self);
                self.key_bindings.remove(table, *key);
            }
            (Change::Reset, Item::Key { table, key, .. }) => {
                let Some(kt) = self.key_bindings.table(table) else {
                    return;
                };
                let Some(bd) = kt.get(*key) else { return };
                let default = kt.get_default(*key);
                if default.is_some_and(|d| d == bd) {
                    return;
                }
                if default.is_none() {
                    up(self);
                }
                self.key_bindings.reset(table, *key);
            }
            (Change::Unset, Item::Environment { session, name, .. }) => {
                if !valid || self.env_of(*session).and_then(|e| e.find(name)).is_none() {
                    return;
                }
                up(self);
                if let Some(env) = self.env_of_mut(*session) {
                    env.unset(name);
                }
            }
            (
                Change::Unset,
                Item::Option {
                    tree, name, key, ..
                },
            ) => {
                if !valid {
                    return;
                }
                let Some(holder) = self.model.options.holder(*tree, name) else {
                    return;
                };
                if key.is_some() {
                    up(self);
                }
                self.model.options.unset(holder, name, key.as_ref());
                self.options_push_changes(name);
            }
            (
                Change::Reset,
                Item::Option {
                    tree,
                    name,
                    key: None,
                    ..
                },
            ) => {
                if !valid {
                    return;
                }
                self.model.options.reset(*tree, name);
                self.options_push_changes(name);
            }
            _ => {}
        }
    }
}

/// `tolower(s[0]) == 'y' && s[1] == '\0'`.
fn is_yes(s: &str) -> bool {
    s.eq_ignore_ascii_case("y")
}
