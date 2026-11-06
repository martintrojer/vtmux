//! A store taken apart and put together again, for the snapshot a live
//! upgrade or a checkpoint carries (D27). The parts are plain data; the
//! server turns them into the snapshot's records and back.

use super::{Entry, OptionValue, OptionsStore, TreeId, monitor, table, vtmux};
use crate::ids::{PaneId, WindowId};

/// Everything an [`OptionsStore`] holds: each tree's own entries (by name,
/// in name order) and the hook bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreParts {
    /// Every tree the store has, the three global ones first, then the
    /// sessions', windows' and panes' (empty ones included: a session's
    /// tree exists while the session does).
    pub trees: Vec<(TreeId, Vec<(String, OptionValue)>)>,
    /// The window each pane's tree inherits from.
    pub pane_window: Vec<(PaneId, WindowId)>,
    pub hook_events: Vec<String>,
    /// `fire_count` and `fire_time` of each hook that ran, by tree.
    pub hook_fires: Vec<(TreeId, String, u32, i64)>,
    pub monitors: Vec<(TreeId, String, monitor::Monitor)>,
    pub monitor_serial: u64,
    /// `TMUX_TERM`.
    pub term: String,
}

/// The table row of option `name`: tmux's table or vtmux's own options;
/// `None` for a user option (`@name`) or a name this build does not know.
#[must_use]
pub fn def_of(name: &str) -> Option<&'static super::OptionDef> {
    table::find(name).or_else(|| vtmux::EXTENSIONS.iter().find(|d| d.name == name))
}

impl OptionsStore {
    /// The store as [`StoreParts`].
    #[must_use]
    pub fn export(&self) -> StoreParts {
        let entries = |o: &super::Options| -> Vec<(String, OptionValue)> {
            o.iter()
                .map(|(n, e)| (n.to_owned(), e.value.clone()))
                .collect()
        };
        let mut trees = vec![
            (TreeId::Server, entries(&self.server)),
            (TreeId::GlobalSession, entries(&self.global_session)),
            (TreeId::GlobalWindow, entries(&self.global_window)),
        ];
        let mut sessions: Vec<_> = self.sessions.iter().collect();
        sessions.sort_by_key(|(id, _)| **id);
        trees.extend(
            sessions
                .into_iter()
                .map(|(id, o)| (TreeId::Session(*id), entries(o))),
        );
        let mut windows: Vec<_> = self.windows.iter().collect();
        windows.sort_by_key(|(id, _)| **id);
        trees.extend(
            windows
                .into_iter()
                .map(|(id, o)| (TreeId::Window(*id), entries(o))),
        );
        let mut panes: Vec<_> = self.panes.iter().collect();
        panes.sort_by_key(|(id, _)| **id);
        trees.extend(
            panes
                .into_iter()
                .map(|(id, o)| (TreeId::Pane(*id), entries(o))),
        );
        let mut pane_window: Vec<_> = self.pane_window.iter().map(|(p, w)| (*p, *w)).collect();
        pane_window.sort_unstable();
        let mut hook_fires: Vec<_> = self
            .hook_fires
            .iter()
            .map(|((t, n), (c, at))| (*t, n.clone(), *c, *at))
            .collect();
        hook_fires.sort_by(|a, b| (tree_key(a.0), &a.1).cmp(&(tree_key(b.0), &b.1)));
        StoreParts {
            trees,
            pane_window,
            hook_events: self.hook_events.iter().cloned().collect(),
            hook_fires,
            monitors: self
                .monitors()
                .into_iter()
                .map(|(t, n, m)| (t, n.to_owned(), m.clone()))
                .collect(),
            monitor_serial: self.monitor_serial,
            term: self.term.clone(),
        }
    }

    /// A store from [`StoreParts`]. An option this build does not know
    /// (removed since the snapshot was written) is dropped.
    #[must_use]
    pub fn import(parts: StoreParts) -> Self {
        let defaults = super::Defaults {
            term: parts.term.clone(),
            ..super::Defaults::default()
        };
        let mut store = Self::new(&defaults);
        store.server = super::Options::default();
        store.global_session = super::Options::default();
        store.global_window = super::Options::default();
        for (tree, entries) in parts.trees {
            let o = store.tree_mut(tree);
            for (name, value) in entries {
                let def = def_of(&name);
                if def.is_none() && !name.starts_with('@') {
                    continue;
                }
                o.insert(&name, Entry { def, value });
            }
        }
        store.pane_window = parts.pane_window.into_iter().collect();
        store.hook_events = parts.hook_events.into_iter().collect();
        store.hook_fires = parts
            .hook_fires
            .into_iter()
            .map(|(t, n, c, at)| ((t, n), (c, at)))
            .collect();
        store.monitors = parts
            .monitors
            .into_iter()
            .map(|(t, n, m)| ((t, n), m))
            .collect();
        store.monitor_serial = parts.monitor_serial;
        store
    }
}

/// A sort key for trees: the order [`StoreParts::trees`] lists them in.
fn tree_key(t: TreeId) -> (u8, u32) {
    match t {
        TreeId::Server => (0, 0),
        TreeId::GlobalSession => (1, 0),
        TreeId::GlobalWindow => (2, 0),
        TreeId::Session(s) => (3, s.0),
        TreeId::Window(w) => (4, w.0),
        TreeId::Pane(p) => (5, p.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SessionId;
    use crate::options::Value;

    #[test]
    fn a_store_survives_export_and_import() {
        let mut store = OptionsStore::new(&super::super::Defaults::default());
        store.add_session(SessionId(3));
        store.add_window(WindowId(1));
        store.add_pane(PaneId(2), WindowId(1));
        store.set_table_value(TreeId::Session(SessionId(3)), "status", Value::Choice(0));
        store.hook_fired(TreeId::GlobalSession, "after-new-window", 7);
        let back = OptionsStore::import(store.export());
        assert_eq!(back, store);
        assert_eq!(back.export(), store.export());
    }
}
