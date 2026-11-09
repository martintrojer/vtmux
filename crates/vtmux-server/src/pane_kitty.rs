//! D29's grant follows the program that pushed it: a program that is
//! killed never pops its kitty keyboard flags, and the shell it leaves
//! behind would be sent `CSI 99;5u` for C-c until a `reset`.
//!
//! So a pane remembers, per screen, the process group in the PTY's
//! foreground when the screen's flags first went non-zero, and ends the
//! grant (empties that screen's stack) once that group has no process
//! left. The check runs where a stale grant would show: before a pane's
//! output is parsed (its `CSI ? u` answer and the gate on a push read the
//! stack) and before a key is encoded for the pane. A running program,
//! one stopped or in the background included, keeps its grant (D28 rule
//! 5); a pane with no grant does nothing here.
//!
//! A program in the shell's own process group (no job control, or
//! `exec`) ends with the shell, which ends the pane, so there is nothing
//! to track. A grant of unknown owner (a snapshot from a build that did
//! not record it) is settled once, when the snapshot is imported
//! ([`Pane::settle_imported_kitty_grants`]).

use crate::model::Pane;

impl Pane {
    /// The PTY's foreground process group.
    fn foreground_pgid(&self) -> Option<i32> {
        #[cfg(test)]
        if let Some(group) = fake::foreground(self.id) {
            return Some(group);
        }
        crate::sys::pty::foreground_pgid(self.io.as_ref()?.owned())
    }

    /// End the grant of each screen whose pushing process group is gone.
    pub fn end_dead_kitty_grants(&mut self) {
        for screen in 0..2 {
            let Some(group) = self.kitty_owner[screen] else {
                continue;
            };
            if group_alive(group) {
                continue;
            }
            // Asked again later when the parser is inside a sequence.
            if self.term.end_kitty_grant(screen == 1) {
                self.kitty_owner[screen] = None;
            }
        }
    }

    /// After output was parsed: the process group in the foreground owns
    /// the grant of each screen whose program was just granted, and a
    /// screen back to flags 0 has no owner.
    pub fn note_kitty_grants(&mut self) {
        let granted = self.term.take_kitty_grants();
        if granted == [false; 2] && self.kitty_owner == [None; 2] {
            return;
        }
        if granted != [false; 2] {
            let group = self.foreground_pgid();
            for (owner, _) in self.kitty_owner.iter_mut().zip(granted).filter(|x| x.1) {
                *owner = group;
            }
        }
        if !self.term.kitty_granted() {
            self.kitty_owner[usize::from(self.term.alternate_on())] = None;
        }
    }

    /// A pane imported from a snapshot: end the grants whose owner is gone,
    /// and settle the ones with none recorded (written by a build before
    /// the owner was kept, or a downgrade and back in between, where the
    /// program may have ended unseen). The program on the active screen is
    /// the foreground process group, unless that is the shell's own: then
    /// whatever pushed is gone. The primary screen not in use is the
    /// shell's (its process group lives as long as the pane), and the
    /// alternate screen not in use belongs to a program
    /// that has left (the shell's, untracked, is left as it is). A pane
    /// with no process has no grant.
    pub fn settle_imported_kitty_grants(&mut self) {
        if self.io.is_none() {
            self.kitty_owner = [None; 2];
            return;
        }
        let active = usize::from(self.term.alternate_on());
        for screen in 0..2 {
            if self.kitty_owner[screen].is_some() {
                continue;
            }
            if screen != active {
                // The primary screen's is the shell's, left alone.
                if screen == 1 {
                    self.term.end_kitty_grant(true);
                }
            } else if self.term.kitty_granted() {
                match self.foreground_pgid() {
                    Some(group) if group != self.pid => self.kitty_owner[screen] = Some(group),
                    Some(_) => {
                        self.term.end_kitty_grant(screen == 1);
                    }
                    None => {}
                }
            }
        }
        self.end_dead_kitty_grants();
    }
}

fn group_alive(group: i32) -> bool {
    #[cfg(test)]
    if let Some(alive) = fake::alive(group) {
        return alive;
    }
    crate::sys::pty::group_alive(group)
}

/// A test's stand-ins for the PTY's foreground process group and for which
/// process groups are alive, since a test's panes are not under job
/// control.
#[cfg(test)]
pub mod fake {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use vtmux_core::ids::PaneId;

    thread_local! {
        static FOREGROUND: RefCell<BTreeMap<PaneId, i32>> = RefCell::default();
        static DEAD: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
    }

    /// `pane`'s foreground process group reads as `group`.
    pub fn set_foreground(pane: PaneId, group: i32) {
        FOREGROUND.with(|f| f.borrow_mut().insert(pane, group));
    }

    /// `group` has no process left.
    pub fn kill(group: i32) {
        DEAD.with(|d| d.borrow_mut().push(group));
    }

    /// Back to the real PTY and `kill(2)`.
    pub fn clear() {
        FOREGROUND.with(|f| f.borrow_mut().clear());
        DEAD.with(|d| d.borrow_mut().clear());
    }

    pub(super) fn foreground(pane: PaneId) -> Option<i32> {
        FOREGROUND.with(|f| f.borrow().get(&pane).copied())
    }

    pub(super) fn alive(group: i32) -> Option<bool> {
        DEAD.with(|d| d.borrow().contains(&group).then_some(false))
            .or_else(|| (group >= FAKE_BASE).then_some(true))
    }

    /// Fake groups start here: alive until killed.
    pub const FAKE_BASE: i32 = 1_000_000;
}
