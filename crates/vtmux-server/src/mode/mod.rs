//! Pane modes (window.c `window_pane_set_mode` / `window_pane_reset_mode`):
//! a pane's stack of modes, the newest first, drawn instead of the pane's
//! own screen and given its keys.
//!
//! Only copy mode exists so far; view mode, tree modes and clock mode are
//! later work.

pub mod copy;

use vtmux_core::ids::PaneId;
use vtmux_core::options::TreeId;

use crate::model::{Model, Redraw};
use crate::server::Server;

pub use copy::CopyMode;

/// One mode on a pane's stack (`struct window_mode_entry`).
#[derive(Clone, Debug)]
pub enum Mode {
    Copy(Box<CopyMode>),
}

impl Mode {
    /// `wme->mode->name`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Copy(_) => "copy-mode",
        }
    }

    /// `window_copy_key_table`.
    #[must_use]
    pub fn key_table(&self, vi: bool) -> &'static str {
        match self {
            Self::Copy(_) if vi => "copy-mode-vi",
            Self::Copy(_) => "copy-mode",
        }
    }

    #[must_use]
    pub fn as_copy(&self) -> Option<&CopyMode> {
        match self {
            Self::Copy(m) => Some(m),
        }
    }

    pub fn as_copy_mut(&mut self) -> Option<&mut CopyMode> {
        match self {
            Self::Copy(m) => Some(m),
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
    pub fn copy_env(&self, p: PaneId) -> Option<copy::Env<'_>> {
        let pane = self.panes.get(&p)?;
        let w = TreeId::Window(pane.window);
        Some(copy::Env {
            term: &pane.term,
            vi: self.number_option(w, "mode-keys") == 1,
            line_numbers: self.number_option(w, "copy-mode-line-numbers"),
        })
    }

    /// The pane's newest mode's key table (`wme->mode->key_table`).
    #[must_use]
    pub fn mode_key_table(&self, p: PaneId) -> Option<&'static str> {
        let mode = self.panes.get(&p)?.modes.first()?;
        Some(mode.key_table(self.mode_keys_vi(p)))
    }

    /// A pane's mode changed: redraw the window and its status
    /// (`window_pane_set_mode`, window.c:1787-1790).
    fn mode_changed(&mut self, p: PaneId) {
        let Some(w) = self.panes.get(&p).map(|pane| pane.window) else {
            return;
        };
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
        let mut entry = match existing {
            Some(i) => self.panes.get_mut(&p).expect("pane").modes.remove(i),
            None => Mode::Copy(Box::new(mode(self))),
        };
        if let Some(m) = entry.as_copy_mut() {
            m.kill = kill;
        }
        self.panes.get_mut(&p).expect("pane").modes.insert(0, entry);
        self.mode_changed(p);
        true
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
        if let Some(Mode::Copy(next)) = pane.modes.first_mut() {
            next.resize(&pane.term);
        }
        self.model.mode_changed(p);
        if old.as_copy().is_some_and(|m| m.kill) {
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
