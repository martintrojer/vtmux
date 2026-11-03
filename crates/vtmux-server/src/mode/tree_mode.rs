//! What every tree mode shares (mode-tree.c's paths through a mode's
//! callbacks): building over the server, keys, the search and filter
//! prompts, running the template, and the frame `mode_tree_draw` leaves.
//! `choose-tree` ([`super::window_tree`]), `choose-buffer`
//! ([`super::window_buffer`]) and `choose-client` ([`super::window_client`])
//! each implement [`TreeMode`]; the drawing is `render::tree`.

use std::marker::PhantomData;

use vtmux_core::ids::{PaneId, WindowId};
use vtmux_core::keys::Key;

use super::Mode;
use super::tree::{Action, Build, Env, ModeTree, Sort};
use crate::open_prompt::{Line, Owner, Reply};
use crate::prompt::Kind;
use crate::prompt_history::PromptType;
use crate::render::tree::{Canvas, Frame};
use crate::render::window::Source;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// `WINDOW_*_DEFAULT_KEY_FORMAT`, the same in every tree mode.
pub const DEFAULT_KEY_FORMAT: &str =
    "#{?#{e|<:#{line},10},#{line},#{e|<:#{line},36},M-#{a:#{e|+:97,#{e|-:#{line},10}}}}";

/// What every tree mode keeps besides its tree (`window_mode_entry` and
/// `mode_tree_data` fields).
#[derive(Clone, Debug)]
pub struct Common {
    /// The key mode of the mode's screen (`#{pane_key_mode}`).
    pub key_mode: vtmux_vt::KeyMode,
    /// The pane the mode is on (`data->wp`).
    pub pane: PaneId,
    /// `-k` (`wme->kill`).
    pub kill: bool,
    /// `-Z` zoomed the window (`mtd->zoomed == 0`): unzoom on exit.
    pub unzoom: Option<WindowId>,
    /// The screen as last drawn (`mtd->screen`); `None` when the mode is
    /// to be drawn again (`mode_tree_draw`) before the pane is next shown.
    pub frame: Option<Frame>,
}

impl Common {
    #[must_use]
    pub fn new(pane: PaneId) -> Self {
        Self {
            key_mode: vtmux_vt::KeyMode::Standard,
            pane,
            kill: false,
            unzoom: None,
            frame: None,
        }
    }
}

/// Which tree mode a [`Mode`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeKind {
    Window,
    Buffer,
    Client,
}

/// A tree mode: its `mode_tree_start` callbacks and its own keys.
pub(crate) trait TreeMode: Sized + 'static {
    type Tag: Copy + PartialEq;
    type Data: Clone;
    /// The mode's help lines (`helpcb`, `%1` the item), the help box's
    /// width (at least `MODE_TREE_HELP_DEFAULT_WIDTH`) and the item's name.
    const HELP: &'static [(&'static str, &'static str)];
    const HELP_WIDTH: usize;
    const HELP_ITEM: &'static str;

    fn tree(&self) -> &ModeTree<Self::Tag, Self::Data>;
    fn tree_mut(&mut self) -> &mut ModeTree<Self::Tag, Self::Data>;
    fn common(&self) -> &Common;
    fn common_mut(&mut self) -> &mut Common;
    fn of(mode: &Mode) -> Option<&Self>;
    fn into_mode(self: Box<Self>) -> Mode;
    /// The mode back out of a [`Mode`] that [`Self::of`] matched.
    fn from_mode(mode: Mode) -> Option<Box<Self>>;

    /// `buildcb`: the items, given the sort and the filter; the tag to
    /// select, if the mode chooses one.
    fn build_items(
        &self,
        server: &Server,
        b: &mut Build<Self::Tag, Self::Data>,
        sort: &Sort,
        filter: Option<&str>,
    ) -> Option<Self::Tag>;
    /// `keycb`: the item's shortcut key on `line`.
    fn item_key(&self, server: &Server, data: &Self::Data, line: usize) -> Option<Key>;
    /// `searchcb`.
    fn matches(&self, server: &Server, data: &Self::Data, search: &str, icase: bool) -> bool;
    /// After each build.
    fn built(&mut self) {}
    /// A key moved the selection or built the items again.
    fn moved(&mut self) {}
    /// Nothing is left to choose: the mode ends on the next key
    /// (`paste_is_empty`, `server_client_how_many() == 0`).
    fn empty(_server: &Server) -> bool {
        false
    }
    /// `swapcb` for `J`/`K`.
    fn swap(_server: &mut Server, _p: PaneId, _direction: isize) {}
    /// The mode's own keys after mode-tree's; `true` when it is finished.
    fn mode_key(
        server: &mut Server,
        p: PaneId,
        c: Option<ClientId>,
        key: Key,
        item: Option<Self::Data>,
    ) -> bool;
    /// `drawcb`: the item's preview in the `size` area at `at`.
    fn draw_preview(
        &self,
        source: &Source<'_>,
        canvas: &mut Canvas,
        data: &Self::Data,
        at: (usize, usize),
        size: (usize, usize),
    );
}

/// `mode_tree_build` for `mode` over `server`.
fn build_tree<M: TreeMode>(mode: &M, server: &Server, tree: &mut ModeTree<M::Tag, M::Data>) {
    tree.build(
        |b, sort, f| mode.build_items(server, b, sort, f),
        |d, line| mode.item_key(server, d, line),
    );
}

/// `mode_tree_build` for `mode`, its tree out of it meanwhile.
pub(crate) fn rebuild<M: TreeMode>(mode: &mut M, server: &Server) {
    let mut tree = std::mem::take(mode.tree_mut());
    build_tree(mode, server, &mut tree);
    *mode.tree_mut() = tree;
    mode.built();
}

/// [`Env`] for keys: builds over the server, with the tree out of the
/// mode.
pub(crate) struct KeyEnv<'a, M> {
    pub server: &'a Server,
    pub mode: &'a M,
}

impl<M: TreeMode> Env<M::Tag, M::Data> for KeyEnv<'_, M> {
    fn rebuild(&mut self, tree: &mut ModeTree<M::Tag, M::Data>) {
        build_tree(self.mode, self.server, tree);
    }

    fn matches(&self, data: &M::Data, search: &str, icase: bool) -> bool {
        self.mode.matches(self.server, data, search, icase)
    }
}

/// The search or filter prompt of a tree mode
/// (`mode_tree_search_callback`, `mode_tree_filter_callback`).
struct SearchPrompt<M> {
    filter: bool,
    mode: PhantomData<fn() -> M>,
}

impl<M> std::fmt::Debug for SearchPrompt<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchPrompt")
            .field("filter", &self.filter)
            .finish()
    }
}

impl<M: TreeMode> super::prompt::ModePromptOwner for SearchPrompt<M> {
    fn input(&mut self, server: &mut Server, p: PaneId, _c: Option<ClientId>, line: Line) -> Reply {
        let (value, done) = match line {
            Line::Done(v) => (v, true),
            Line::Changed(v) => (Some(v), false),
            Line::Move => return Reply::Continue,
        };
        let filter = self.filter;
        server.with_tree::<M, _>(p, |server, t| {
            let mut tree = std::mem::take(t.tree_mut());
            {
                let mut env = KeyEnv { server, mode: &*t };
                if filter {
                    tree.filter_for(&mut env, value.as_deref());
                } else {
                    tree.search_for(&mut env, value.as_deref());
                }
            }
            *t.tree_mut() = tree;
        });
        // An edit keeps the prompt; a finished line closes it.
        if done { Reply::Close } else { Reply::Continue }
    }
}

impl Mode {
    /// The tree mode this is, if it is one.
    #[must_use]
    pub(crate) fn tree_kind(&self) -> Option<TreeKind> {
        match self {
            Self::Tree(_) => Some(TreeKind::Window),
            Self::Buffer(_) => Some(TreeKind::Buffer),
            Self::Client(_) => Some(TreeKind::Client),
            _ => None,
        }
    }

    /// What a tree mode keeps besides its tree.
    #[must_use]
    pub(crate) fn tree_common(&self) -> Option<&Common> {
        match self {
            Self::Tree(t) => Some(t.common()),
            Self::Buffer(t) => Some(t.common()),
            Self::Client(t) => Some(t.common()),
            _ => None,
        }
    }

    pub(crate) fn tree_common_mut(&mut self) -> Option<&mut Common> {
        match self {
            Self::Tree(t) => Some(t.common_mut()),
            Self::Buffer(t) => Some(t.common_mut()),
            Self::Client(t) => Some(t.common_mut()),
            _ => None,
        }
    }

    /// A tree mode's screen size as last built.
    #[must_use]
    pub(crate) fn tree_size(&self) -> Option<(usize, usize)> {
        match self {
            Self::Tree(t) => Some(t.tree().size),
            Self::Buffer(t) => Some(t.tree().size),
            Self::Client(t) => Some(t.tree().size),
            _ => None,
        }
    }
}

impl Server {
    /// The tree mode on top of pane `p`, if one is.
    fn top_tree(&self, p: PaneId) -> Option<TreeKind> {
        self.model.panes.get(&p)?.modes.first()?.tree_kind()
    }

    /// Run `f` with pane `p`'s mode `M` out of the pane, then put it back
    /// (unless the pane or the mode went meanwhile) and redraw the pane:
    /// every path here is one where tmux calls `mode_tree_draw` (a key, a
    /// build, a prompt and its callbacks), so the frame is drawn again.
    pub(crate) fn with_tree<M: TreeMode, R>(
        &mut self,
        p: PaneId,
        f: impl FnOnce(&mut Self, &mut M) -> R,
    ) -> Option<R> {
        let pane = self.model.panes.get_mut(&p)?;
        let at = pane.modes.iter().position(|m| M::of(m).is_some())?;
        let mut mode = M::from_mode(pane.modes.remove(at)).expect("matched above");
        let result = f(self, &mut mode);
        mode.common_mut().frame = None;
        if let Some(pane) = self.model.panes.get_mut(&p) {
            let at = at.min(pane.modes.len());
            pane.modes.insert(at, mode.into_mode());
            self.model.redraw_panes.insert(p);
        }
        Some(result)
    }

    /// The `update` and `resize` callbacks (`window_*_update`,
    /// `mode_tree_resize`): the tree mode on top of the pane is built
    /// again for the pane's current size.
    pub(crate) fn tree_update(&mut self, p: PaneId) {
        match self.top_tree(p) {
            Some(TreeKind::Window) => self.tree_update_as::<super::WindowTree>(p),
            Some(TreeKind::Buffer) => self.tree_update_as::<super::BufferMode>(p),
            Some(TreeKind::Client) => self.tree_update_as::<super::ClientMode>(p),
            None => {}
        }
    }

    fn tree_update_as<M: TreeMode>(&mut self, p: PaneId) {
        let Some(size) = self
            .model
            .panes
            .get(&p)
            .map(|x| (usize::from(x.term.cols()), usize::from(x.term.rows())))
        else {
            return;
        };
        self.with_tree::<M, _>(p, |server, t| {
            t.tree_mut().size = size;
            rebuild(t, server);
        });
    }

    /// `server_client_check_modes` (server-client.c:2462-2478): tree
    /// modes in the current window of a client whose status is to be
    /// redrawn are built again; and any whose pane changed size
    /// (`mode_tree_resize`).
    pub(crate) fn tree_updates(&mut self) {
        let mut due: Vec<PaneId> = self
            .clients
            .values()
            .filter(|c| c.control.is_none() && c.status.status_redraw_pending())
            .filter_map(|c| c.attached)
            .filter_map(|s| self.model.current_of(s))
            .filter_map(|(_, w, _)| self.model.windows.get(&w))
            .flat_map(|w| w.panes.panes.iter().copied())
            .collect();
        due.extend(self.model.panes.iter().filter_map(|(&p, x)| {
            let size = (usize::from(x.term.cols()), usize::from(x.term.rows()));
            x.modes
                .first()?
                .tree_size()
                .filter(|s| *s != size)
                .map(|_| p)
        }));
        due.sort_unstable();
        due.dedup();
        for p in due {
            self.tree_update(p);
        }
    }

    /// `mode_tree_draw` for every tree mode whose frame is due: the
    /// screen the pane shows until the mode draws again. Pane output and
    /// model changes alone do not draw it (tmux draws only from the
    /// mode's key, update, resize and prompt paths).
    pub(crate) fn tree_draws(&mut self) {
        let due: Vec<PaneId> = self
            .model
            .panes
            .iter()
            .filter(|(_, x)| {
                x.modes
                    .iter()
                    .any(|m| m.tree_common().is_some_and(|c| c.frame.is_none()))
            })
            .map(|(&p, _)| p)
            .collect();
        for p in due {
            // window_client_draw reads the clients' status lines as it draws.
            if self.model.panes[&p]
                .modes
                .iter()
                .any(|m| matches!(m, Mode::Client(_)))
            {
                let mut modes =
                    std::mem::take(&mut self.model.panes.get_mut(&p).expect("pane").modes);
                for m in &mut modes {
                    if let Mode::Client(c) = m {
                        c.set_statuses(self);
                    }
                }
                self.model.panes.get_mut(&p).expect("pane").modes = modes;
            }
            let frames: Vec<(usize, Frame)> = {
                let source = self.render_source();
                let pane = &self.model.panes[&p];
                pane.modes
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.tree_common().is_some_and(|c| c.frame.is_none()))
                    .filter_map(|(i, m)| Some((i, source.mode_tree_frame(pane, m)?)))
                    .collect()
            };
            if let Some(pane) = self.model.panes.get_mut(&p) {
                for (i, frame) in frames {
                    if let Some(c) = pane.modes.get_mut(i).and_then(Mode::tree_common_mut) {
                        c.frame = Some(frame);
                    }
                }
            }
        }
    }

    /// The tree mode's `key` callback for a key from client `c`.
    pub(crate) fn tree_key(&mut self, p: PaneId, c: Option<ClientId>, key: Key, typed: bool) {
        match self.top_tree(p) {
            Some(TreeKind::Window) => self.tree_key_as::<super::WindowTree>(p, c, key, typed),
            Some(TreeKind::Buffer) => self.tree_key_as::<super::BufferMode>(p, c, key, typed),
            Some(TreeKind::Client) => self.tree_key_as::<super::ClientMode>(p, c, key, typed),
            None => {}
        }
    }

    fn tree_key_as<M: TreeMode>(&mut self, p: PaneId, c: Option<ClientId>, key: Key, typed: bool) {
        // window_buffer_key: no buffers left ends the mode at once.
        if M::empty(self) {
            self.reset_mode(p);
            return;
        }
        let Some((action, item)) = self.with_tree::<M, _>(p, |server, t| {
            let mut tree = std::mem::take(t.tree_mut());
            let before = (tree.current_item().map(|i| i.tag), tree.builds());
            let action = tree.key(&mut KeyEnv { server, mode: &*t }, key, typed);
            *t.tree_mut() = tree;
            if before != (t.tree().current_item().map(|i| i.tag), t.tree().builds()) {
                t.moved();
            }
            (action, t.tree().current_item().map(|i| i.data.clone()))
        }) else {
            return;
        };
        self.tree_action::<M>(p, c, action, item);
    }

    /// A mouse key at `at` in the pane: the tree's mouse branch
    /// (`mode_tree_key`), then the mode's (a double click is Enter). The
    /// mode's own mouse handling (`window_tree_mouse`) is not here yet. A
    /// mode prompt takes no mouse events yet.
    pub(crate) fn tree_mouse_key(
        &mut self,
        p: PaneId,
        c: ClientId,
        key: Key,
        at: Option<(u32, u32)>,
    ) {
        let prompt = self
            .model
            .panes
            .get(&p)
            .is_some_and(|x| x.mode_prompt.as_ref().is_some_and(|m| m.shown(x)));
        if prompt {
            return;
        }
        match self.top_tree(p) {
            Some(TreeKind::Window) => self.tree_mouse_as::<super::WindowTree>(p, c, key, at),
            Some(TreeKind::Buffer) => self.tree_mouse_as::<super::BufferMode>(p, c, key, at),
            Some(TreeKind::Client) => self.tree_mouse_as::<super::ClientMode>(p, c, key, at),
            None => {}
        }
    }

    fn tree_mouse_as<M: TreeMode>(
        &mut self,
        p: PaneId,
        c: ClientId,
        key: Key,
        at: Option<(u32, u32)>,
    ) {
        if M::empty(self) {
            self.reset_mode(p);
            return;
        }
        let Some((action, item)) = self.with_tree::<M, _>(p, |_, t| {
            let action = t.tree_mut().mouse_key(key, at);
            (action, t.tree().current_item().map(|i| i.data.clone()))
        }) else {
            return;
        };
        self.tree_action::<M>(p, Some(c), action, item);
    }

    fn tree_action<M: TreeMode>(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        action: Action,
        item: Option<M::Data>,
    ) {
        let finished = match action {
            Action::Finished => true,
            Action::Search => {
                self.tree_search_prompt::<M>(p, c, false);
                false
            }
            Action::Filter => {
                self.tree_search_prompt::<M>(p, c, true);
                false
            }
            Action::ClearFilter => {
                self.mode_prompt_clear(p);
                self.with_tree::<M, _>(p, |server, t| {
                    t.tree_mut().filter = None;
                    rebuild(t, server);
                });
                false
            }
            Action::Swap(d) => {
                M::swap(self, p, d);
                false
            }
            Action::Key(None) => false,
            Action::Key(Some(k)) => M::mode_key(self, p, c, k, item),
        };
        if finished || M::empty(self) {
            self.reset_mode(p);
        }
    }

    /// `mode_tree_set_prompt` for `(search) ` or `(filter) ` (with the
    /// current filter as its input).
    fn tree_search_prompt<M: TreeMode>(&mut self, p: PaneId, c: Option<ClientId>, filter: bool) {
        let (label, input) = if filter {
            let f = self
                .with_tree::<M, _>(p, |_, t| t.tree().filter.clone())
                .flatten()
                .unwrap_or_default();
            ("(filter) ", f)
        } else {
            ("(search) ", String::new())
        };
        let owner = SearchPrompt::<M> {
            filter,
            mode: PhantomData,
        };
        self.tree_prompt(
            p,
            c,
            label,
            &input,
            (PromptType::Search, Kind::Line),
            Box::new(owner),
        );
    }

    /// `mode_tree_set_prompt` on pane `p` for `owner`.
    pub(crate) fn tree_prompt(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        label: &str,
        input: &str,
        (ty, kind): (PromptType, Kind),
        owner: Box<dyn super::prompt::ModePromptOwner>,
    ) {
        let mut editor = match c {
            Some(c) => self.prompt_editor(c, label.to_owned(), input, kind, ty),
            None => crate::prompt::Prompt::new(label.to_owned(), input, kind, ty),
        };
        editor.no_format = true;
        let state = Found::from_pane(&self.model, p);
        self.mode_prompt_set(p, c, editor, state, Owner::Mode(owner));
    }

    /// `mode_tree_run_command`: the template with `%%` the target, parsed
    /// and appended to client `c`'s queue with `fs` as its current state;
    /// a parse error is the client's message, capitalised.
    pub(crate) fn tree_run_command(
        &mut self,
        c: Option<ClientId>,
        fs: Option<Found>,
        template: &str,
        name: &str,
    ) {
        let command = vtmux_core::cmd::template::replace(template, name, 1);
        if command.is_empty() {
            return;
        }
        let origin = crate::target::Origin {
            attached: c.and_then(|c| self.client_session(c)),
            inside_pane: None,
            current: fs,
            mouse: None,
        };
        let value = vtmux_core::cmd::ArgValue::Str(command);
        match crate::commands::make_commands(self, c, origin, None, &value) {
            Ok(calls) => {
                let mut item = crate::commands::Item::new(c, self.client_cwd(c, None));
                if let Some(client) = c.and_then(|c| self.clients.get(&c)) {
                    if let Some(hello) = client.hello.as_ref() {
                        item.env.clone_from(&hello.env);
                        item.utf8 = hello.flags.utf8;
                    }
                    item.origin.attached = client.attached;
                }
                item.origin.current = fs;
                self.cmdq_append(
                    c,
                    crate::cmdq::NewBatch::new(item, calls, crate::cmdq::End::Quiet),
                );
            }
            Err(msg) => {
                if let Some(c) = c {
                    let mut chars = msg.chars();
                    let msg = chars
                        .next()
                        .map(|f| f.to_uppercase().chain(chars).collect())
                        .unwrap_or_default();
                    self.status_message_set(c, msg);
                }
            }
        }
    }
}
