//! What formats read from outside the server's own state: the time and each
//! pane's foreground process.
//!
//! tmux reads both afresh at each expansion (`time(NULL)` in
//! `format_expand1`, format.c:6899; `format_cb_current_command` and
//! `format_cb_current_path`, format.c:958-998, through `osdep_get_name` and
//! `osdep_get_cwd`). vtmux reads each once per render turn while the turn
//! holds them ([`LiveReads::hold`]): every client drawn in that turn sees
//! the same values, so clients that share one drawing
//! ([`crate::render::share`]) are each sent what they would be sent drawn
//! alone, and a pane whose foreground process changes in the middle of the
//! turn shows one name to all of them. Outside a turn (commands, hooks)
//! every read is live, as tmux's.

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;

use vtmux_core::ids::PaneId;
use vtmux_core::time::{SystemWallClock, Tm, WallClock};

use crate::model::Pane;

/// The values a render turn read ([`LiveReads::hold`]); `None` outside one.
#[derive(Debug, Default)]
pub struct LiveReads(RefCell<Option<Held>>);

#[derive(Debug, Default)]
struct Held {
    now: Option<i64>,
    panes: BTreeMap<PaneId, Foreground>,
}

/// A pane's foreground process as first read in the turn; each part is
/// read on first use.
#[derive(Debug, Default)]
struct Foreground {
    name: OnceCell<Option<String>>,
    cwd: OnceCell<Option<PathBuf>>,
}

impl LiveReads {
    /// From now until [`Self::release`], each value is read once.
    pub fn hold(&self) {
        *self.0.borrow_mut() = Some(Held::default());
    }

    /// Back to reading every value live.
    pub fn release(&self) {
        *self.0.borrow_mut() = None;
    }

    /// Seconds since the epoch ([`crate::mode::clock::now`]).
    #[must_use]
    pub fn now(&self) -> i64 {
        let mut held = self.0.borrow_mut();
        match held.as_mut() {
            Some(h) => *h.now.get_or_insert_with(crate::mode::clock::now),
            None => crate::mode::clock::now(),
        }
    }

    /// The name of `pane`'s foreground process (`osdep_get_name`), `None`
    /// when it has no terminal or the name can't be read.
    #[must_use]
    pub fn foreground_name(&self, pane: &Pane) -> Option<String> {
        let read = || {
            let io = pane.io.as_ref()?;
            #[cfg(test)]
            if let Some(name) = fake::name(pane.id) {
                return Some(name);
            }
            crate::sys::pty::foreground_name(&io.owned().try_clone_to_owned().ok()?)
        };
        match self.0.borrow_mut().as_mut() {
            Some(h) => {
                let held = h.panes.entry(pane.id).or_default();
                held.name.get_or_init(read).clone()
            }
            None => read(),
        }
    }

    /// The working directory of `pane`'s foreground process
    /// (`osdep_get_cwd`).
    #[must_use]
    pub fn foreground_cwd(&self, pane: &Pane) -> Option<PathBuf> {
        let read = || {
            let io = pane.io.as_ref()?;
            crate::sys::pty::foreground_cwd(&io.owned().try_clone_to_owned().ok()?)
        };
        match self.0.borrow_mut().as_mut() {
            Some(h) => {
                let held = h.panes.entry(pane.id).or_default();
                held.cwd.get_or_init(read).clone()
            }
            None => read(),
        }
    }
}

/// The time formats see: [`LiveReads::now`] in the local zone.
impl WallClock for LiveReads {
    fn now(&self) -> i64 {
        LiveReads::now(self)
    }

    fn local_time(&self, epoch: i64) -> Option<Tm> {
        SystemWallClock.local_time(epoch)
    }
}

/// A test's stand-in for a pane's foreground process name, so a test can
/// make it change between two reads.
#[cfg(test)]
pub mod fake {
    use std::cell::RefCell;

    use vtmux_core::ids::PaneId;

    type Names = Box<dyn FnMut(PaneId) -> Option<String>>;

    thread_local! {
        static NAMES: RefCell<Option<Names>> = RefCell::new(None);
    }

    /// Every read of a pane's foreground name on this thread asks `names`
    /// first, until [`clear`].
    pub fn set(names: impl FnMut(PaneId) -> Option<String> + 'static) {
        NAMES.with(|n| *n.borrow_mut() = Some(Box::new(names)));
    }

    pub fn clear() {
        NAMES.with(|n| *n.borrow_mut() = None);
    }

    pub(super) fn name(p: PaneId) -> Option<String> {
        NAMES.with(|n| n.borrow_mut().as_mut().and_then(|f| f(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::LiveReads;

    /// Held, the time is read once; released, it is read again.
    #[test]
    fn a_held_turn_reads_the_time_once() {
        let live = LiveReads::default();
        live.hold();
        let first = live.now();
        assert_eq!(live.now(), first);
        live.0.borrow_mut().as_mut().unwrap().now = Some(1);
        assert_eq!(live.now(), 1, "the held value");
        live.release();
        assert!(live.now() >= first, "live again");
    }
}
