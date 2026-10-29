//! The window state layout strings read: pane order, active pane, the
//! last-used stack, z-order and `pane-base-index` (window.c).

use crate::ids::PaneId;

/// `window_add_pane` flags (`SPAWN_BEFORE`, `SPAWN_FULLSIZE`,
/// `SPAWN_FLOATING`).
#[derive(Clone, Copy, Debug, Default)]
pub struct AddFlags {
    pub before: bool,
    pub full: bool,
    pub floating: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowPanes {
    /// `w->panes`: window order, which `"i"` indexes.
    pub panes: Vec<PaneId>,
    pub active: Option<PaneId>,
    /// `w->last_panes`: most recently active first (`"l"` is the position).
    pub last: Vec<PaneId>,
    /// `w->z_index`: topmost first; floating panes are counted for `"z"`.
    pub z_order: Vec<PaneId>,
    /// `pane-base-index`.
    pub base_index: u32,
}

impl WindowPanes {
    /// A window with its first pane, which is active.
    #[must_use]
    pub fn new(first: PaneId) -> Self {
        Self {
            panes: vec![first],
            active: Some(first),
            last: Vec::new(),
            z_order: vec![first],
            base_index: 0,
        }
    }

    /// `window_add_pane` (window.c:1148-1183): `other` defaults to the active
    /// pane. The new pane is not made active; see [`Self::select`].
    pub fn add(&mut self, new: PaneId, other: Option<PaneId>, flags: AddFlags) {
        let other = other.or(self.active);
        let at = other.and_then(|o| self.panes.iter().position(|&p| p == o));
        let (Some(at), false) = (at, self.panes.is_empty()) else {
            self.panes.insert(0, new);
            self.z_top_or_tail(new, flags);
            return;
        };
        if flags.before {
            let i = if flags.full { 0 } else { at };
            self.panes.insert(i, new);
        } else if flags.full || flags.floating {
            self.panes.push(new);
        } else {
            self.panes.insert(at + 1, new);
        }
        self.z_top_or_tail(new, flags);
    }

    fn z_top_or_tail(&mut self, new: PaneId, flags: AddFlags) {
        if flags.floating {
            self.z_order.insert(0, new);
        } else {
            self.z_order.push(new);
        }
    }

    /// `window_set_active_pane` (window.c:755): the old active pane goes on
    /// top of the last-used stack. False when `pane` already is active.
    pub fn select(&mut self, pane: PaneId) -> bool {
        if self.active == Some(pane) {
            return false;
        }
        self.last.retain(|&p| p != pane);
        if let Some(old) = self.active {
            self.last.retain(|&p| p != old);
            self.last.insert(0, old);
        }
        self.active = Some(pane);
        true
    }

    /// `window_remove_pane` with `window_lost_pane` (window.c:1185-1233): a
    /// removed active pane gives way to the last-used pane, else its
    /// predecessor in the window, else its successor.
    pub fn remove(&mut self, pane: PaneId) {
        let Some(at) = self.panes.iter().position(|&p| p == pane) else {
            return;
        };
        self.last.retain(|&p| p != pane);
        if self.active == Some(pane) {
            let next = self
                .last
                .first()
                .copied()
                .or_else(|| at.checked_sub(1).map(|i| self.panes[i]))
                .or_else(|| self.panes.get(at + 1).copied());
            self.active = next;
            if let Some(n) = next {
                self.last.retain(|&p| p != n);
            }
        }
        self.panes.remove(at);
        self.z_order.retain(|&p| p != pane);
    }

    /// `window_pane_index`: position in the window plus `pane-base-index`.
    #[must_use]
    pub fn index_of(&self, pane: PaneId) -> Option<u32> {
        let at = self.panes.iter().position(|&p| p == pane)?;
        Some(self.base_index + u32::try_from(at).ok()?)
    }

    /// `window_pane_last_index`.
    #[must_use]
    pub fn last_index_of(&self, pane: PaneId) -> Option<u32> {
        u32::try_from(self.last.iter().position(|&p| p == pane)?).ok()
    }
}
