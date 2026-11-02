//! Pane modes (window.c `window_pane_set_mode` / `window_pane_reset_mode`):
//! a pane's stack of modes, the newest first, drawn instead of the pane's
//! own screen and given its keys.
//!
//! Only copy mode exists so far; view mode, tree modes and clock mode are
//! later work. What a mode offers the rest of the server is [`PaneMode`]
//! (`struct window_mode`: `name`, `key_table`, `command`), and a mode that
//! asks the user a question keeps a [`prompt::ModePrompt`] on its pane
//! (`mode_tree_set_prompt`).

pub mod clock;
pub mod copy;
pub mod grid;
pub mod prompt;
pub mod reader;
pub mod tree;
pub mod window_tree;

use vtmux_core::ids::PaneId;
use vtmux_core::options::TreeId;

use crate::model::{Model, Redraw};
use crate::server::Server;

pub use clock::ClockMode;
pub use copy::CopyMode;
pub use window_tree::WindowTree;

/// What the server asks of a mode (`struct window_mode`). A mode without
/// a key table gets its keys from its own handler; one without a command
/// callback answers `send-keys -X` and `-N` with "not in a mode"
/// (cmd-send-keys.c:191-203).
pub trait PaneMode {
    /// `mode->name`.
    fn name(&self) -> &'static str;
    /// `mode->key_table`: `None` when the mode has none.
    fn key_table(&self, vi: bool) -> Option<&'static str>;
    /// `mode->command != NULL`.
    fn takes_commands(&self) -> bool;
    /// `wme->prefix = np` (`send-keys -N`).
    fn set_prefix(&mut self, n: u32);
    /// `mode->command` for `send-keys -X name args...`; `read_only_client`
    /// is `c->flags & CLIENT_READONLY` for the client it came from.
    ///
    /// # Errors
    /// A command tmux has that vtmux does not yet, or one a read-only
    /// client may not run.
    fn command(
        &mut self,
        env: &copy::Env,
        name: &str,
        args: &[String],
        read_only_client: bool,
    ) -> Result<copy::Outcome, copy::CommandError>;
}

impl PaneMode for CopyMode {
    fn name(&self) -> &'static str {
        "copy-mode"
    }

    /// `window_copy_key_table`.
    fn key_table(&self, vi: bool) -> Option<&'static str> {
        Some(if vi { "copy-mode-vi" } else { "copy-mode" })
    }

    fn takes_commands(&self) -> bool {
        true
    }

    fn set_prefix(&mut self, n: u32) {
        self.prefix = n;
    }

    fn command(
        &mut self,
        env: &copy::Env,
        name: &str,
        args: &[String],
        read_only_client: bool,
    ) -> Result<copy::Outcome, copy::CommandError> {
        Self::command(self, env, name, args, read_only_client)
    }
}

/// The key mode a mode's new screen starts with (`screen_init` ->
/// `screen_reinit`): mode 1 under `extended-keys always`.
pub(crate) fn screen_key_mode(model: &Model) -> vtmux_vt::KeyMode {
    if model.extended_keys() == vtmux_vt::ExtendedKeys::Always {
        vtmux_vt::KeyMode::Extended1
    } else {
        vtmux_vt::KeyMode::Standard
    }
}

/// One mode on a pane's stack (`struct window_mode_entry`).
#[derive(Clone, Debug)]
pub enum Mode {
    Copy(Box<CopyMode>),
    Clock(ClockMode),
    /// `choose-tree` (window-tree.c).
    Tree(Box<WindowTree>),
    /// A mode with no key table and no command callback, like tmux's tree
    /// and clock modes: only tests need one before those exist.
    #[cfg(test)]
    Probe,
}

impl PaneMode for Mode {
    fn name(&self) -> &'static str {
        match self {
            Self::Copy(m) => m.name(),
            Self::Clock(m) => m.name(),
            Self::Tree(_) => "tree-mode",
            #[cfg(test)]
            Self::Probe => "probe-mode",
        }
    }

    fn key_table(&self, vi: bool) -> Option<&'static str> {
        match self {
            Self::Copy(m) => m.key_table(vi),
            Self::Clock(m) => m.key_table(vi),
            Self::Tree(_) => None,
            #[cfg(test)]
            Self::Probe => None,
        }
    }

    fn takes_commands(&self) -> bool {
        match self {
            Self::Copy(m) => m.takes_commands(),
            Self::Clock(m) => m.takes_commands(),
            Self::Tree(_) => false,
            #[cfg(test)]
            Self::Probe => false,
        }
    }

    fn set_prefix(&mut self, n: u32) {
        match self {
            Self::Copy(m) => m.set_prefix(n),
            Self::Clock(m) => m.set_prefix(n),
            Self::Tree(_) => {}
            #[cfg(test)]
            Self::Probe => {}
        }
    }

    fn command(
        &mut self,
        env: &copy::Env,
        name: &str,
        args: &[String],
        read_only_client: bool,
    ) -> Result<copy::Outcome, copy::CommandError> {
        match self {
            Self::Copy(m) => PaneMode::command(&mut **m, env, name, args, read_only_client),
            Self::Clock(m) => m.command(env, name, args, read_only_client),
            Self::Tree(_) => Err(copy::CommandError::NotImplemented(name.to_owned())),
            #[cfg(test)]
            Self::Probe => Err(copy::CommandError::NotImplemented(name.to_owned())),
        }
    }
}

impl Mode {
    /// The key mode of the mode's own screen (`#{pane_key_mode}`).
    #[must_use]
    pub fn key_mode(&self) -> vtmux_vt::KeyMode {
        match self {
            Self::Copy(m) => m.key_mode,
            Self::Clock(m) => m.key_mode,
            Self::Tree(m) => m.key_mode,
            #[cfg(test)]
            Self::Probe => vtmux_vt::KeyMode::Standard,
        }
    }

    #[must_use]
    pub fn as_copy(&self) -> Option<&CopyMode> {
        match self {
            Self::Copy(m) => Some(m),
            Self::Clock(_) => None,
            Self::Tree(_) => None,
            #[cfg(test)]
            Self::Probe => None,
        }
    }

    pub fn as_copy_mut(&mut self) -> Option<&mut CopyMode> {
        match self {
            Self::Copy(m) => Some(m),
            Self::Clock(_) => None,
            Self::Tree(_) => None,
            #[cfg(test)]
            Self::Probe => None,
        }
    }

    /// `wme->kill`: leaving the mode kills the pane (`-k`).
    fn kill(&self) -> bool {
        match self {
            Self::Copy(m) => m.kill,
            Self::Tree(t) => t.kill,
            // clock-mode has no -k.
            Self::Clock(_) => false,
            #[cfg(test)]
            Self::Probe => false,
        }
    }
}

impl Model {
    /// `mode-keys` is `vi` for pane `p`'s window.
    #[must_use]
    pub fn mode_keys_vi(&self, p: PaneId) -> bool {
        self.panes
            .get(&p)
            .is_some_and(|pane| self.number_option(TreeId::Window(pane.window), "mode-keys") == 1)
    }

    /// The options copy mode reads for pane `p`.
    #[must_use]
    pub fn copy_env(&self, p: PaneId) -> Option<copy::Env> {
        let pane = self.panes.get(&p)?;
        let w = TreeId::Window(pane.window);
        // cs->s: the session of the command's target, here the pane's first.
        let session = self
            .sessions_with(pane.window)
            .next()
            .map_or(TreeId::GlobalSession, |s| TreeId::Session(s.id));
        let separators = self
            .options
            .get(session, "word-separators")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        Some(copy::Env {
            vi: self.number_option(w, "mode-keys") == 1,
            line_numbers: self.number_option(w, "copy-mode-line-numbers"),
            separators,
            wrap_search: self.number_option(w, "wrap-search") != 0,
        })
    }

    /// The pane's newest mode's key table (`wme->mode->key_table`), if it
    /// has one.
    #[must_use]
    pub fn mode_key_table(&self, p: PaneId) -> Option<&'static str> {
        let mode = self.panes.get(&p)?.modes.first()?;
        mode.key_table(self.mode_keys_vi(p))
    }

    /// A pane's mode changed: `PANE_CHANGED`, so the next name check
    /// looks at it (`#{pane_in_mode}` in `automatic-rename-format`), and
    /// redraw the window and its status (`window_pane_set_mode` and
    /// `window_pane_reset_mode`, window.c:1785-1790, 1828-1832).
    fn mode_changed(&mut self, p: PaneId) {
        let Some(pane) = self.panes.get_mut(&p) else {
            return;
        };
        pane.changed = true;
        let w = pane.window;
        self.redraw_panes.insert(p);
        self.redraw(Redraw::Window(w));
        self.redraw(Redraw::WindowStatus(w));
    }

    /// `window_pane_set_mode` for copy mode, `mode` built only if the pane
    /// is not in copy mode already. Returns whether the mode was entered
    /// (tmux's 0); a mode already on top is left alone, one further down
    /// comes to the top.
    pub fn set_copy_mode(
        &mut self,
        p: PaneId,
        kill: bool,
        mode: impl FnOnce(&Model) -> CopyMode,
    ) -> bool {
        let Some(pane) = self.panes.get(&p) else {
            return false;
        };
        if matches!(pane.modes.first(), Some(Mode::Copy(_))) {
            return false;
        }
        let existing = pane.modes.iter().position(|m| matches!(m, Mode::Copy(_)));
        let mut entry = if let Some(i) = existing {
            self.panes.get_mut(&p).expect("pane").modes.remove(i)
        } else {
            let mut m = mode(self);
            m.key_mode = screen_key_mode(self);
            Mode::Copy(Box::new(m))
        };
        if let Some(m) = entry.as_copy_mut() {
            m.kill = kill;
        }
        let pane = self.panes.get_mut(&p).expect("pane");
        let previous = pane.modes.first().map(Mode::name);
        let name = entry.name();
        pane.modes.insert(0, entry);
        self.mode_changed(p);
        self.fire_mode_changed(p, previous, Some(name), true);
        true
    }

    /// `window_pane_set_mode` for tree mode: like [`Self::set_copy_mode`],
    /// `mode` built (it may refuse, `None`) only if the pane has no tree
    /// mode on its stack.
    pub fn set_tree_mode(
        &mut self,
        p: PaneId,
        kill: bool,
        mode: impl FnOnce(&mut Model) -> Option<WindowTree>,
    ) -> bool {
        let Some(pane) = self.panes.get(&p) else {
            return false;
        };
        if matches!(pane.modes.first(), Some(Mode::Tree(_))) {
            return false;
        }
        let existing = pane.modes.iter().position(|m| matches!(m, Mode::Tree(_)));
        let mut entry = match existing {
            Some(i) => self.panes.get_mut(&p).expect("pane").modes.remove(i),
            None => match mode(self) {
                Some(mut t) => {
                    t.key_mode = screen_key_mode(self);
                    Mode::Tree(Box::new(t))
                }
                None => return false,
            },
        };
        if let Mode::Tree(t) = &mut entry {
            t.kill = kill;
        }
        let Some(pane) = self.panes.get_mut(&p) else {
            return false;
        };
        let previous = pane.modes.first().map(Mode::name);
        let name = entry.name();
        pane.modes.insert(0, entry);
        self.mode_changed(p);
        self.fire_mode_changed(p, previous, Some(name), true);
        true
    }

    /// `window_fire_pane_mode_changed` (window.c:158-178) for
    /// pane-mode-entered/exited, then pane-mode-changed.
    fn fire_mode_changed(
        &mut self,
        p: PaneId,
        previous: Option<&str>,
        current: Option<&str>,
        entered: bool,
    ) {
        let Some(w) = self.panes.get(&p).map(|x| x.window) else {
            return;
        };
        let first = if entered {
            "pane-mode-entered"
        } else {
            "pane-mode-exited"
        };
        for name in [first, "pane-mode-changed"] {
            let mut ev = crate::events::Event::new(name)
                .target(crate::target::Found::from_pane(self, p))
                .pane("pane", p)
                .window(self, "window", w)
                .int("mode_entered", i64::from(entered));
            if let Some(c) = current {
                ev = ev.text("current_mode", c);
            }
            if let Some(pr) = previous {
                ev = ev.text("previous_mode", pr);
            }
            self.push_event(ev);
        }
    }
}

impl Server {
    /// `window_pane_reset_mode`: the newest mode goes; one that was entered
    /// with `-k` kills the pane.
    pub fn reset_mode(&mut self, p: PaneId) {
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        if pane.modes.is_empty() {
            return;
        }
        let old = pane.modes.remove(0);
        if pane.modes.is_empty() {
            pane.unseen_changes = false;
        }
        let size = (usize::from(pane.term.cols()), usize::from(pane.term.rows()));
        match pane.modes.first_mut() {
            Some(Mode::Copy(next)) => next.resize(size.0, size.1),
            Some(Mode::Clock(next)) => next.resize(size.0, size.1),
            _ => {}
        }
        let tree_next = matches!(pane.modes.first(), Some(Mode::Tree(_)));
        let current = pane.modes.first().map(Mode::name);
        // A prompt of the mode that went goes with it.
        self.mode_prompt_gone(p);
        self.model.mode_changed(p);
        self.model
            .fire_mode_changed(p, Some(old.name()), current, false);
        // mode_tree_free: a window the mode zoomed is unzoomed.
        if let Mode::Tree(t) = &old
            && let Some(w) = t.unzoom
        {
            self.model.unzoom(w);
        }
        // window_tree_resize for the mode now shown.
        if tree_next {
            self.tree_update(p);
        }
        if old.kill() {
            self.destroy_pane(p);
        }
    }

    /// `window_pane_reset_mode_all`.
    pub fn reset_mode_all(&mut self, p: PaneId) {
        while self
            .model
            .panes
            .get(&p)
            .is_some_and(|pane| !pane.modes.is_empty())
        {
            self.reset_mode(p);
        }
    }
}

impl Server {
    /// `window_pane_key` for a pane in a mode (window.c:2080-2092): the
    /// shown mode's own `key` callback gets it, with the client it came
    /// from, after the mode's prompt. Copy mode takes keys only through
    /// its key table. `typed`: a key from a client's terminal, which has a
    /// mouse event with it (`&event->m`), unlike one `send-keys` gives
    /// (`NULL`, cmd-send-keys.c:97). Returns whether anything took it.
    pub(crate) fn mode_key(
        &mut self,
        p: PaneId,
        c: Option<crate::server::ClientId>,
        key: vtmux_core::keys::Key,
        typed: bool,
    ) -> bool {
        if self.mode_prompt_key(p, c, key) {
            return true;
        }
        // window_pane_key: a mode's `key` callback runs only with a client
        // (window.c:2088); clock mode's leaves the mode on any key (no
        // mouse yet).
        let top = self.model.panes.get(&p).and_then(|x| x.modes.first());
        let clock = matches!(top, Some(Mode::Clock(_)));
        let tree = matches!(top, Some(Mode::Tree(_)));
        if clock && c.is_some() {
            self.clock_key(p);
        }
        if tree && c.is_some() {
            self.tree_key(p, c, key, typed);
        }
        tree
    }

    /// Run `f` on pane `p`'s shown mode, out of the pane while it runs so
    /// it can read the pane's terminal. `None` when the pane is in no mode.
    pub(crate) fn with_mode<R>(
        &mut self,
        p: PaneId,
        f: impl FnOnce(&mut Mode, &copy::Env) -> R,
    ) -> Option<R> {
        let pane = self.model.panes.get_mut(&p)?;
        if pane.modes.is_empty() {
            return None;
        }
        let mut mode = pane.modes.remove(0);
        let result = self.model.copy_env(p).map(|env| f(&mut mode, &env));
        if let Some(pane) = self.model.panes.get_mut(&p) {
            if let Some(m) = mode.as_copy() {
                pane.search.clone_from(&m.pane_search);
            }
            pane.modes.insert(0, mode);
        }
        self.model.redraw_panes.insert(p);
        result
    }

    /// The soonest automatic refresh of a copy mode on top of a pane.
    pub(crate) fn copy_refresh_deadline(&self) -> Option<std::time::Instant> {
        self.model
            .panes
            .values()
            .filter_map(|p| match p.modes.first() {
                Some(Mode::Copy(m)) => m.refresh_due(),
                _ => None,
            })
            .min()
    }

    /// `window_copy_refresh_timer` (window-copy.c:3116-3143) for every copy
    /// mode that is due: with unseen output and no selection being made,
    /// copy the pane's screen again, following it if the cursor was at the
    /// bottom; then arm the timer again.
    pub(crate) fn copy_refresh_timers(&mut self, now: std::time::Instant) {
        let due: Vec<PaneId> = self
            .model
            .panes
            .iter()
            .filter(|(_, p)| match p.modes.first() {
                Some(Mode::Copy(m)) => m.refresh_due().is_some_and(|at| at <= now),
                _ => false,
            })
            .map(|(&id, _)| id)
            .collect();
        for p in due {
            let unseen = self.model.panes.get(&p).is_some_and(|x| x.unseen_changes);
            if unseen {
                self.copy_refresh(p, false);
            }
            if let Some(Mode::Copy(m)) = self
                .model
                .panes
                .get_mut(&p)
                .and_then(|x| x.modes.first_mut())
            {
                m.refresh_arm(now);
            }
        }
    }

    /// `window_copy_do_refresh`: the copy of pane `p`'s screen is taken
    /// again and the pane's unseen changes are seen. A timer refresh
    /// (`forced` false) leaves a selection in progress alone.
    pub(crate) fn copy_refresh(&mut self, p: PaneId, forced: bool) {
        let Some(env) = self.model.copy_env(p) else {
            return;
        };
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        let term = &pane.term;
        let Some(Mode::Copy(m)) = pane.modes.first_mut() else {
            return;
        };
        if !forced && !m.refresh_ready() {
            return;
        }
        let follow = m.follows();
        m.refresh(&env, term, follow);
        pane.unseen_changes = false;
        self.model.redraw_panes.insert(p);
    }
}
