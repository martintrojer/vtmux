//! Wait channels (cmd-wait-for.c): named channels a client's command queue
//! blocks on until `wait-for -S` signals them, plus the `-L`/`-U` lock.
//!
//! A blocked command is identified by its client: a client runs one batch
//! at a time, so at most one of its commands can be blocked. When and how a
//! channel is dropped is visible (a pending signal survives or not), so the
//! two removal rules follow `cmd_wait_for_remove` and
//! `cmd_wait_for_remove_empty` exactly.

use std::collections::BTreeMap;

use crate::server::ClientId;

#[derive(Debug, Default)]
struct Channel {
    locked: bool,
    /// Signalled with nobody waiting: the next wait returns at once.
    woken: bool,
    waiters: Vec<ClientId>,
    lockers: Vec<ClientId>,
}

impl Channel {
    /// `cmd_wait_for_remove`: unlocked, nobody waiting and signalled.
    fn spent(&self) -> bool {
        !self.locked && self.waiters.is_empty() && self.woken
    }

    /// `cmd_wait_for_remove_empty`: nothing at all left on it.
    fn empty(&self) -> bool {
        !self.locked && !self.woken && self.waiters.is_empty() && self.lockers.is_empty()
    }
}

/// Whether the command goes on or blocks its client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Proceed,
    Block,
}

/// `wait-for -U` on a channel that is not locked.
#[derive(Debug, PartialEq, Eq)]
pub struct NotLocked;

#[derive(Debug, Default)]
pub struct WaitChannels {
    channels: BTreeMap<String, Channel>,
}

impl WaitChannels {
    fn remove_if(&mut self, name: &str, rule: fn(&Channel) -> bool) {
        if self.channels.get(name).is_some_and(rule) {
            self.channels.remove(name);
        }
    }

    /// `cmd_wait_for_signal`: wake every waiter, or leave the signal
    /// pending when there are none. A second signal with nobody waiting
    /// drops the channel, pending signal included.
    pub fn signal(&mut self, name: &str) -> Vec<ClientId> {
        let ch = self.channels.entry(name.to_owned()).or_default();
        if ch.waiters.is_empty() && !ch.woken {
            ch.woken = true;
            return Vec::new();
        }
        let woken = std::mem::take(&mut ch.waiters);
        self.remove_if(name, Channel::spent);
        woken
    }

    /// `cmd_wait_for_wait`: a pending signal lets `client` through.
    pub fn wait(&mut self, name: &str, client: ClientId) -> Outcome {
        let ch = self.channels.entry(name.to_owned()).or_default();
        if ch.woken {
            self.remove_if(name, Channel::spent);
            return Outcome::Proceed;
        }
        ch.waiters.push(client);
        Outcome::Block
    }

    /// `cmd_wait_for_lock`.
    pub fn lock(&mut self, name: &str, client: ClientId) -> Outcome {
        let ch = self.channels.entry(name.to_owned()).or_default();
        if ch.locked {
            ch.lockers.push(client);
            Outcome::Block
        } else {
            ch.locked = true;
            Outcome::Proceed
        }
    }

    /// `cmd_wait_for_unlock`: hand the lock to the first queued locker, if
    /// any (it is returned to be woken), else release it.
    pub fn unlock(&mut self, name: &str) -> Result<Option<ClientId>, NotLocked> {
        let ch = self
            .channels
            .get_mut(name)
            .filter(|c| c.locked)
            .ok_or(NotLocked)?;
        if ch.lockers.is_empty() {
            ch.locked = false;
            self.remove_if(name, Channel::spent);
            Ok(None)
        } else {
            Ok(Some(ch.lockers.remove(0)))
        }
    }

    /// `cmd_wait_for_list`: waiters, then queued lockers.
    #[must_use]
    pub fn blocked(&self, name: &str) -> Vec<ClientId> {
        self.channels
            .get(name)
            .map(|c| c.waiters.iter().chain(&c.lockers).copied().collect())
            .unwrap_or_default()
    }

    /// `cmd_wait_for_wake`: release `client` from the channel without a
    /// signal (a locker goes on without the lock).
    pub fn wake(&mut self, name: &str, client: ClientId) -> bool {
        let Some(ch) = self.channels.get_mut(name) else {
            return false;
        };
        if let Some(i) = ch.waiters.iter().position(|&c| c == client) {
            ch.waiters.remove(i);
        } else if let Some(i) = ch.lockers.iter().position(|&c| c == client) {
            ch.lockers.remove(i);
        } else {
            return false;
        }
        self.remove_if(name, Channel::empty);
        true
    }

    /// `cmd_wait_for_client_lost`: forget the client everywhere.
    pub fn client_lost(&mut self, client: ClientId) {
        for ch in self.channels.values_mut() {
            ch.waiters.retain(|&c| c != client);
            ch.lockers.retain(|&c| c != client);
        }
        self.channels.retain(|_, ch| !ch.empty());
    }

    /// `cmd_wait_for_flush` (the server is exiting): wake everyone, leave
    /// every channel signalled and unlocked.
    pub fn flush(&mut self) -> Vec<ClientId> {
        let mut woken = Vec::new();
        for ch in self.channels.values_mut() {
            woken.append(&mut ch.waiters);
            ch.woken = true;
            woken.append(&mut ch.lockers);
            ch.locked = false;
        }
        self.channels.retain(|_, ch| !ch.spent());
        woken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ClientId = ClientId(1);
    const B: ClientId = ClientId(2);

    /// Reference: `wait-for -S y; wait-for y` returns at once, but after
    /// two signals with nobody waiting `wait-for z` blocks.
    #[test]
    fn pending_signal_is_one_shot_and_a_second_signal_drops_it() {
        let mut w = WaitChannels::default();
        assert_eq!(w.signal("y"), Vec::<ClientId>::new());
        assert_eq!(w.wait("y", A), Outcome::Proceed);
        assert_eq!(w.wait("y", A), Outcome::Block);
        // Waking waiters does not mark the channel signalled, so it stays
        // (cmd_wait_for_remove needs `woken`) and the next wait blocks.
        assert_eq!(w.signal("y"), vec![A]);
        assert_eq!(w.wait("y", A), Outcome::Block);

        w.signal("z");
        w.signal("z");
        assert!(!w.channels.contains_key("z"));
        assert_eq!(w.wait("z", A), Outcome::Block);
    }

    #[test]
    fn signal_wakes_all_waiters_in_order() {
        let mut w = WaitChannels::default();
        w.wait("c", A);
        w.wait("c", B);
        assert_eq!(w.blocked("c"), vec![A, B]);
        assert_eq!(w.signal("c"), vec![A, B]);
        assert_eq!(w.blocked("c"), Vec::<ClientId>::new());
    }

    /// Reference: `-U` on an unlocked channel fails; `-S ; -L ; -U` then
    /// `wait` blocks (the unlock drops the signalled channel), while
    /// `-S ; -L` then `wait` passes and the channel stays locked.
    #[test]
    fn lock_unlock() {
        let mut w = WaitChannels::default();
        assert_eq!(w.unlock("x"), Err(NotLocked));
        assert_eq!(w.lock("x", A), Outcome::Proceed);
        assert_eq!(w.lock("x", B), Outcome::Block);
        assert_eq!(w.unlock("x"), Ok(Some(B)));
        assert_eq!(w.unlock("x"), Ok(None));
        assert_eq!(w.unlock("x"), Err(NotLocked));

        w.signal("s");
        w.lock("s", A);
        w.unlock("s").unwrap();
        assert_eq!(w.wait("s", A), Outcome::Block);

        let mut w = WaitChannels::default();
        w.signal("t");
        w.lock("t", A);
        assert_eq!(w.wait("t", B), Outcome::Proceed);
        assert_eq!(w.unlock("t"), Ok(None));
    }

    #[test]
    fn wake_and_client_lost_drop_empty_channels_but_keep_pending_signals() {
        let mut w = WaitChannels::default();
        w.wait("c", A);
        assert!(!w.wake("c", B));
        assert!(w.wake("c", A));
        assert!(w.channels.is_empty());

        w.signal("saved");
        w.wait("last", A);
        w.client_lost(A);
        assert!(!w.channels.contains_key("last"));
        assert_eq!(w.wait("saved", B), Outcome::Proceed);
    }

    #[test]
    fn flush_wakes_waiters_and_lockers() {
        let mut w = WaitChannels::default();
        w.wait("c", A);
        w.lock("l", A);
        w.lock("l", B);
        assert_eq!(w.flush(), vec![A, B]);
        assert!(w.channels.is_empty());
    }
}
