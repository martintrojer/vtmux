//! Desktop notifications to attached Ghostty clients (spec D30,
//! `vtmux-notifications`). tmux 3.8 drops them: `input_osc_9`
//! (input.c:3030-3037) handles only `9;4`, and `input_exit_osc`
//! (input.c:2699-2789) has no 777.

use std::hash::Hasher as _;
use std::time::{Duration, Instant};

use vtmux_core::ids::WindowId;
use vtmux_core::options::TreeId;
use vtmux_vt::Notification;

use crate::server::{ClientId, Server};

/// The option, read at each notification.
const OPTION: &str = "vtmux-notifications";

/// A client's limit, as Ghostty limits itself (`showDesktopNotification`,
/// Surface.zig:6612-6647): at most one a second, and one identical to the
/// last shown at most once in 5 s. Only a notification sent counts.
#[derive(Debug, Default)]
pub(crate) struct Limit {
    /// When the last one was sent, and its title and body hashed (Ghostty
    /// hashes the title then the body, so it does too).
    last: Option<(Instant, u64)>,
}

impl Limit {
    /// Whether a notification of `title` and `body` goes at `now`; if it
    /// does, it counts from now.
    pub(crate) fn admit(&mut self, now: Instant, title: &[u8], body: &[u8]) -> bool {
        let mut hash = std::hash::DefaultHasher::new();
        hash.write(title);
        hash.write(body);
        let digest = hash.finish();
        if let Some((at, last)) = self.last {
            let since = now.saturating_duration_since(at);
            if since < Duration::from_secs(1) || (last == digest && since < Duration::from_secs(5))
            {
                return false;
            }
        }
        self.last = Some((now, digest));
        true
    }
}

impl Server {
    /// A notification from a pane of `window`: with the option on, to
    /// every attached client whose session holds the window, current or
    /// not, whose terminal is Ghostty, within its [`Limit`]. Never to a
    /// control client or a suspended one. It goes with the client's
    /// passthrough, after this turn's frame, and like passthrough is
    /// dropped while the client's terminal is blocked.
    pub(crate) fn pane_notification(&mut self, window: WindowId, n: &Notification) {
        if self.model.number_option(TreeId::Server, OPTION) == 0 {
            return;
        }
        let now = Instant::now();
        let mut osc = None;
        let ids: Vec<ClientId> = self
            .clients
            .iter()
            // A suspended client's terminal is not ours (its passthrough
            // is dropped): it is not sent one, so its limit is not used.
            .filter(|(_, c)| c.control.is_none() && c.caps.is_some() && !c.suspended)
            .filter(|(_, c)| c.term.as_ref().is_some_and(|t| is_ghostty(t.term_type())))
            .filter_map(|(&id, c)| c.attached.map(|s| (id, s)))
            .filter(|&(_, s)| {
                self.model
                    .sessions
                    .get(&s)
                    .is_some_and(|x| x.windows.values().any(|w| *w == window))
            })
            .map(|(id, _)| id)
            .collect();
        for id in ids {
            let client = self.clients.get_mut(&id).expect("client");
            if client.notified.admit(now, &n.title, &n.body) {
                let osc = osc.get_or_insert_with(|| n.osc());
                client.passthrough.extend_from_slice(osc);
            }
        }
    }
}

/// Whether a terminal that answered XTVERSION with `term_type` is
/// Ghostty, the one terminal known to show both forms (its
/// `#{client_termtype}`, `ghostty 1.2.3`; tmux knows it by the same
/// prefix, tty-keys.c `tty_keys_extended_device_attributes`).
fn is_ghostty(term_type: &str) -> bool {
    term_type.starts_with("ghostty ")
}

#[cfg(test)]
mod tests {
    use super::Limit;
    use std::time::{Duration, Instant};

    /// Ghostty's own limits (Surface.zig:6612-6647), on a clock of our own.
    #[test]
    fn the_limit_is_ghosttys() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut l = Limit::default();
        assert!(l.admit(at(0), b"", b"a"));
        assert!(!l.admit(at(999), b"", b"b"), "within the second");
        assert!(l.admit(at(1000), b"", b"b"));
        assert!(!l.admit(at(2500), b"", b"b"), "identical within 5 s");
        assert!(l.admit(at(2500), b"", b"c"), "another after a second");
        assert!(!l.admit(at(7499), b"", b"c"));
        assert!(l.admit(at(7500), b"", b"c"), "identical after 5 s");
        // Refused ones don't count: `b` was last sent at 1000.
        assert!(l.admit(at(8500), b"", b"b"));
        // The title is part of what is identical.
        assert!(l.admit(at(9500), b"T", b"b"));
    }
}
