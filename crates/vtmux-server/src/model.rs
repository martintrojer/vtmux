//! Sessions, windows and panes in ID-keyed arenas.
//!
//! Edge-visible orderings are reproduced on purpose (spec, *Rust at the
//! core, tmux at the edge*): sessions are listed in name order (tmux keeps
//! them in an RB tree keyed by name, session.c `session_cmp`), windows by
//! their index in the session, and IDs come from monotonic allocators that
//! never reuse a number while the server lives (session.c:30, window.c:61).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use vtmux_core::ids::{IdAllocator, PaneId, SessionId, WindowId};
use vtmux_core::options::{Defaults, OptionsStore};
use vtmux_vt::PaneTerm;

use crate::pane_io::PaneIo;

#[derive(Debug)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    /// Seconds since the epoch (`#{session_created}`).
    pub created: i64,
    /// Ordering for "most recently active" (cmd-find.c
    /// `cmd_find_session_better`): a server-wide counter bumped on activity.
    pub activity: u64,
    pub cwd: PathBuf,
    /// Window index -> window (tmux's winlinks).
    pub windows: BTreeMap<u32, WindowId>,
    /// The current window's index.
    pub current: u32,
    /// The session's own environment (update-environment, `-e`).
    pub env: Vec<(String, String)>,
}

/// The pane layout of a window: a tiled tree plus a z-ordered floating
/// layer (D15). M0 creates one tiled pane per window; the shape is here so
/// splits and floating panes extend it rather than replace it.
#[derive(Debug, Default)]
pub struct Layout {
    /// Tiled panes in layout order (M0: exactly one).
    pub tiled: Vec<PaneId>,
    /// Floating panes, bottom to top (empty in M0).
    pub floating: Vec<PaneId>,
}

impl Layout {
    /// Every pane, tiled then floating: tmux's pane list order, which
    /// `#{pane_index}` counts.
    pub fn panes(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.tiled.iter().chain(&self.floating).copied()
    }

    pub fn remove(&mut self, pane: PaneId) {
        self.tiled.retain(|p| *p != pane);
        self.floating.retain(|p| *p != pane);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiled.is_empty() && self.floating.is_empty()
    }
}

#[derive(Debug)]
pub struct Window {
    pub id: WindowId,
    pub name: String,
    /// `automatic-rename`: off once a name is given.
    pub auto_rename: bool,
    pub layout: Layout,
    pub active: PaneId,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug)]
pub struct Pane {
    pub id: PaneId,
    pub window: WindowId,
    pub term: PaneTerm,
    /// The PTY and its process; `None` once the PTY is closed.
    pub io: Option<PaneIo>,
    pub pid: i32,
    /// The command it was started with (`#{pane_start_command}`).
    pub argv: Vec<String>,
    pub shell: PathBuf,
    pub cwd: PathBuf,
    /// The process has been reaped.
    pub exited: bool,
    /// Output arrived since the last automatic-rename check (tmux's
    /// `PANE_CHANGED`).
    pub changed: bool,
    pub last_rename: Option<Instant>,
}

/// The server's sessions, windows and panes.
#[derive(Debug)]
pub struct Model {
    pub options: OptionsStore,
    pub sessions: BTreeMap<SessionId, Session>,
    /// Name -> ID: the listing order (tmux sorts sessions by name).
    pub by_name: BTreeMap<String, SessionId>,
    pub windows: BTreeMap<WindowId, Window>,
    pub panes: BTreeMap<PaneId, Pane>,
    pub session_ids: IdAllocator<SessionId>,
    pub window_ids: IdAllocator<WindowId>,
    pub pane_ids: IdAllocator<PaneId>,
    activity: u64,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            options: OptionsStore::new(&Defaults::default()),
            sessions: BTreeMap::new(),
            by_name: BTreeMap::new(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            session_ids: IdAllocator::new(),
            window_ids: IdAllocator::new(),
            pane_ids: IdAllocator::new(),
            activity: 0,
        }
    }
}

impl Model {
    /// The next activity stamp.
    pub fn touch(&mut self) -> u64 {
        self.activity += 1;
        self.activity
    }

    /// Sessions in name order.
    pub fn sessions_by_name(&self) -> impl Iterator<Item = &Session> {
        self.by_name.values().map(|id| &self.sessions[id])
    }

    #[must_use]
    pub fn session_named(&self, name: &str) -> Option<&Session> {
        self.by_name.get(name).map(|id| &self.sessions[id])
    }

    /// The session's current window and its active pane.
    #[must_use]
    pub fn current_of(&self, s: SessionId) -> Option<(u32, WindowId, PaneId)> {
        let session = self.sessions.get(&s)?;
        let w = *session.windows.get(&session.current)?;
        Some((session.current, w, self.windows.get(&w)?.active))
    }

    /// Sessions that link window `w`.
    pub fn sessions_with(&self, w: WindowId) -> impl Iterator<Item = &Session> {
        self.sessions
            .values()
            .filter(move |s| s.windows.values().any(|x| *x == w))
    }

    /// `#{pane_index}`: position in the window's pane list.
    #[must_use]
    pub fn pane_index(&self, p: PaneId) -> Option<usize> {
        let pane = self.panes.get(&p)?;
        self.windows
            .get(&pane.window)?
            .layout
            .panes()
            .position(|x| x == p)
    }

    pub fn insert_session(&mut self, session: Session) {
        self.options.add_session(session.id);
        self.by_name.insert(session.name.clone(), session.id);
        self.sessions.insert(session.id, session);
    }

    /// Remove a session and every window no other session links; returns
    /// the panes that went with them (their PTYs close when dropped).
    pub fn remove_session(&mut self, id: SessionId) -> Vec<Pane> {
        let Some(session) = self.sessions.remove(&id) else {
            return Vec::new();
        };
        self.options.remove_session(id);
        self.by_name.remove(&session.name);
        let mut gone = Vec::new();
        for w in session.windows.values() {
            if self.sessions_with(*w).next().is_none()
                && let Some(window) = self.windows.remove(w)
            {
                self.options.remove_window(*w);
                for p in window.layout.panes() {
                    self.options.remove_pane(p);
                    gone.extend(self.panes.remove(&p));
                }
            }
        }
        gone
    }

    /// Remove a pane; an emptied window is unlinked everywhere and an
    /// emptied session destroyed (`server_destroy_pane` -> `server_kill_window`
    /// -> `session_detach`, server-fn.c).
    pub fn remove_pane(&mut self, id: PaneId) -> Option<Pane> {
        let pane = self.panes.remove(&id)?;
        self.options.remove_pane(id);
        let wid = pane.window;
        let empty = self.windows.get_mut(&wid).is_none_or(|w| {
            w.layout.remove(id);
            if w.active == id
                && let Some(next) = w.layout.panes().next()
            {
                w.active = next;
            }
            w.layout.is_empty()
        });
        if empty {
            self.windows.remove(&wid);
            self.options.remove_window(wid);
            let mut dead = Vec::new();
            for s in self.sessions.values_mut() {
                s.windows.retain(|_, w| *w != wid);
                if s.windows.is_empty() {
                    dead.push(s.id);
                } else if !s.windows.contains_key(&s.current) {
                    // session_detach picks the last window (session.c
                    // session_detach -> session_next_session... M0 has one).
                    s.current = *s.windows.keys().next().expect("non-empty");
                }
            }
            for s in dead {
                self.remove_session(s);
            }
        }
        Some(pane)
    }
}
