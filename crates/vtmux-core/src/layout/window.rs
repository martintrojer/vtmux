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
    /// `w->modal`: the modal pane, which keeps the focus while it exists.
    pub modal: Option<PaneId>,
    /// `w->modal_last`: the pane active before the modal one, active again
    /// when it goes.
    pub modal_last: Option<PaneId>,
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
            modal: None,
            modal_last: None,
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

    /// window.c:1173-1179: a floating pane goes on top, or just under the
    /// modal pane.
    fn z_top_or_tail(&mut self, new: PaneId, flags: AddFlags) {
        if flags.floating {
            let at = self
                .modal
                .and_then(|m| self.z_order.iter().position(|&p| p == m))
                .map_or(0, |i| i + 1);
            self.z_order.insert(at, new);
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

    /// `window_redraw_active_switch` then `window_set_active_pane`, as
    /// select-pane (cmd-select-pane.c:195, :282), the mouse paths and
    /// modal spawn do: a newly selected floating pane goes to the head of
    /// the z-order (window.c:833-838). The previously active pane is not
    /// raised (the loop breaks on `wp == w->active` first). A pane that is
    /// already active is left alone (nothing to switch). `floating` is
    /// whether the cell of `pane` is floating.
    pub fn select_raising(&mut self, pane: PaneId, floating: bool) -> bool {
        if floating && self.active != Some(pane) {
            self.z_order.retain(|&p| p != pane);
            self.z_order.insert(0, pane);
        }
        self.select(pane)
    }

    /// `window_remove_pane` with `window_lost_pane` (window.c:1185-1233): a
    /// removed active pane gives way to the pane active before it was made
    /// modal (if it was the modal pane), else the last-used pane, else its
    /// predecessor in the window, else its successor.
    pub fn remove(&mut self, pane: PaneId) {
        let Some(at) = self.panes.iter().position(|&p| p == pane) else {
            return;
        };
        if self.modal_last == Some(pane) {
            self.modal_last = None;
        }
        self.last.retain(|&p| p != pane);
        let modal_last = if self.modal == Some(pane) {
            self.modal = None;
            self.modal_last.take()
        } else {
            None
        };
        if self.active == Some(pane) {
            let next = modal_last
                .filter(|p| self.panes.contains(p))
                .or_else(|| self.last.first().copied())
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
