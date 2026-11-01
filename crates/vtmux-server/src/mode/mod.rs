//! Pane modes (window.c `window_pane_set_mode` / `window_pane_reset_mode`):
//! a pane's stack of modes, the newest first, drawn instead of the pane's
//! own screen and given its keys.
//!
//! Only copy mode exists so far; view mode, tree modes and clock mode are
//! later work.

pub mod copy;
pub mod grid;
pub mod reader;

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

    /// The pane's newest mode's key table (`wme->mode->key_table`).
    #[must_use]
    pub fn mode_key_table(&self, p: PaneId) -> Option<&'static str> {
        let mode = self.panes.get(&p)?.modes.first()?;
        Some(mode.key_table(self.mode_keys_vi(p)))
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
        let mut entry = match existing {
            Some(i) => self.panes.get_mut(&p).expect("pane").modes.remove(i),
            None => Mode::Copy(Box::new(mode(self))),
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
        if let Some(Mode::Copy(next)) = pane.modes.first_mut() {
            next.resize(usize::from(pane.term.cols()), usize::from(pane.term.rows()));
        }
        let current = pane.modes.first().map(Mode::name);
        self.model.mode_changed(p);
        self.model
            .fire_mode_changed(p, Some(old.name()), current, false);
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

impl Server {
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
