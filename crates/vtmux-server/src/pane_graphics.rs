//! D33's pane side, as the server keeps it: who owns each screen's grant
//! and how the image memory of every pane is shared.
//!
//! A grant follows the program that started it, as D29's does
//! ([`crate::pane_kitty`]): the pane remembers the process group in the
//! PTY's foreground when the screen was granted, and the grant ends once
//! that group has no process left (checked before the pane's output is
//! parsed), at RIS or at a respawn. The images stay: `kitten icat` exits
//! and its picture stays, until Ghostty frees the last of them.
//!
//! `vtmux-graphics-limit` caps the images of every pane and screen
//! together. A granted screen's limit in Ghostty is what the others leave
//! ([`Model::balance_graphics`]); while a granted pane's output is parsed
//! its active screen may take the whole cap, and the excess is then
//! evicted across the server, the screen that loaded least recently first,
//! each by lowering its limit (Ghostty evicts unused images first, then the
//! oldest: `evictImage`, `graphics_storage.zig:1811`).

use std::time::Instant;

use vtmux_core::options::TreeId;

use vtmux_core::ids::PaneId;

use crate::model::{Model, Pane};

/// One MB of `vtmux-graphics-limit`: 10^6 bytes, as Ghostty counts.
const MB: u64 = 1_000_000;

impl Pane {
    /// End the graphics grant of each screen whose process group is gone.
    pub fn end_dead_graphics_grants(&mut self) {
        for screen in 0..2 {
            let Some(group) = self.graphics_owner[screen] else {
                continue;
            };
            if !crate::pane_kitty::group_alive(group) {
                self.term.set_graphics_grant(screen == 1, false);
                self.graphics_owner[screen] = None;
            }
        }
    }

    /// After output was parsed: the process group in the foreground owns
    /// each screen just granted. Returns whether one was.
    pub fn note_graphics_grants(&mut self) -> bool {
        let granted = self.term.take_graphics_grants();
        if granted == [false; 2] {
            return false;
        }
        let group = self.foreground_pgid();
        for (owner, _) in self.graphics_owner.iter_mut().zip(granted).filter(|x| x.1) {
            *owner = group;
        }
        self.graphics = true;
        true
    }

    /// A respawn: the old program's grants end and its images go.
    pub fn reset_graphics(&mut self) {
        self.term.reset_graphics();
        self.graphics_owner = [None; 2];
    }
}

impl Model {
    /// The server's `vtmux-graphics` (D33).
    #[must_use]
    pub fn graphics_on(&self) -> bool {
        self.number_option(TreeId::Server, "vtmux-graphics") != 0
    }

    /// `vtmux-graphics-limit` in bytes.
    #[must_use]
    pub fn graphics_cap(&self) -> u64 {
        u64::try_from(self.number_option(TreeId::Server, "vtmux-graphics-limit"))
            .unwrap_or(0)
            .saturating_mul(MB)
    }

    /// Before pane `p`'s output is parsed: its active screen, if granted,
    /// may load up to the whole cap (what it takes beyond its share is
    /// evicted across the server after, [`Self::balance_graphics`]), and a
    /// screen granted by this output starts with what the others leave.
    pub fn prepare_graphics(&mut self, p: PaneId) {
        let on = self.graphics_on();
        let cap = self.graphics_cap();
        let used = if self.graphics_used {
            self.graphics_bytes()
        } else {
            0
        };
        let Some(pane) = self.panes.get_mut(&p) else {
            return;
        };
        pane.term.set_graphics(on);
        pane.end_dead_graphics_grants();
        pane.term
            .set_graphics_share(cap.saturating_sub(used).max(1));
        if pane.graphics {
            let alternate = pane.term.alternate_on();
            if pane.term.graphics_grants()[usize::from(alternate)] {
                pane.term.set_graphics_limit(alternate, cap.max(1));
            }
        }
    }

    /// After pane `p`'s output was parsed: the owner of a screen it
    /// granted, and every pane's images held to `vtmux-graphics-limit`.
    pub fn graphics_fed(&mut self, p: PaneId, now: Instant) {
        let Some(pane) = self.panes.get_mut(&p) else {
            return;
        };
        self.graphics_used |= pane.note_graphics_grants();
        if pane.graphics {
            self.balance_graphics(Some(p), now);
        }
    }

    /// Every pane's image bytes together.
    fn graphics_bytes(&self) -> u64 {
        self.panes
            .values()
            .filter(|p| p.graphics)
            .flat_map(|p| {
                [
                    p.term.graphics_storage(false).0,
                    p.term.graphics_storage(true).0,
                ]
            })
            .sum()
    }

    /// Hold the server to `vtmux-graphics-limit`: evict what is over it,
    /// the screen that loaded least recently first (`loaded` is the pane
    /// whose output was just parsed, which goes last), then give each
    /// granted screen what the others leave. A pane whose screens are
    /// neither granted nor hold an image is let go. Nothing to do while no
    /// screen was ever granted.
    pub fn balance_graphics(&mut self, loaded: Option<PaneId>, now: Instant) {
        if !self.graphics_used {
            return;
        }
        let cap = self.graphics_cap();
        // (pane, alternate, bytes, granted), with the time each last grew.
        let mut screens: Vec<(PaneId, bool, u64, bool, Option<Instant>)> = Vec::new();
        for (&id, pane) in &mut self.panes {
            if !pane.graphics {
                continue;
            }
            let grants = pane.term.graphics_grants();
            for alternate in [false, true] {
                let (bytes, _) = pane.term.graphics_storage(alternate);
                let i = usize::from(alternate);
                if bytes > pane.graphics_bytes[i] {
                    pane.graphics_loaded[i] = Some(now);
                }
                pane.graphics_bytes[i] = bytes;
                screens.push((id, alternate, bytes, grants[i], pane.graphics_loaded[i]));
            }
        }
        let mut total: u64 = screens.iter().map(|s| s.2).sum();
        if total > cap {
            // Least recently loaded first; the pane just read last.
            screens.sort_by_key(|s| (Some(s.0) == loaded, s.4));
            for s in &mut screens {
                if total <= cap {
                    break;
                }
                if s.2 == 0 {
                    continue;
                }
                let Some(pane) = self.panes.get_mut(&s.0) else {
                    continue;
                };
                let keep = s.2.saturating_sub(total - cap);
                pane.term.set_graphics_limit(s.1, keep);
                let (bytes, _) = pane.term.graphics_storage(s.1);
                total = total - s.2 + bytes;
                s.2 = bytes;
                pane.graphics_bytes[usize::from(s.1)] = bytes;
            }
        }
        for &(id, alternate, bytes, granted, _) in &screens {
            let Some(pane) = self.panes.get_mut(&id) else {
                continue;
            };
            let limit = if granted {
                // What the others leave, never 0: Ghostty takes 0 as no
                // graphics and would stop answering the program.
                cap.saturating_sub(total - bytes).max(bytes).max(1)
            } else {
                bytes
            };
            if pane.term.graphics_storage(alternate).1 != limit {
                pane.term.set_graphics_limit(alternate, limit);
            }
        }
        for pane in self.panes.values_mut().filter(|p| p.graphics) {
            if pane.term.graphics_grants() == [false; 2] && pane.graphics_bytes == [0; 2] {
                pane.graphics = false;
            }
        }
        self.graphics_used = self.panes.values().any(|p| p.graphics);
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use vtmux_core::ids::PaneId;

    use crate::pane_kitty::fake;
    use crate::server::snapshot::tests::{ok, server, temp_dir};

    /// An RGB image `w` x `h`, transmitted as `id` with no reply.
    fn image(id: u32, w: u32, h: u32) -> Vec<u8> {
        let raw = vec![7u8; usize::try_from(w * h * 3).unwrap()];
        let mut b64 = String::new();
        // Base64 of a run of the byte 7, three at a time: "BwcH".
        for _ in 0..raw.len() / 3 {
            b64.push_str("BwcH");
        }
        let mut out = String::new();
        let _ = write!(out, "\x1b_Ga=t,f=24,s={w},v={h},i={id},q=2;{b64}\x1b\\");
        out.into_bytes()
    }

    /// D33: a screen is granted at its first graphics command while the
    /// option is on, owned by the process group in the foreground then,
    /// and the grant ends once that group is gone; the image stays.
    #[test]
    fn a_graphics_grant_ends_with_its_process_group() {
        let dir = temp_dir("graphicsowner");
        let mut a = server(&dir);
        ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
        let p = PaneId(0);
        let group = fake::FAKE_BASE + 11;
        fake::set_foreground(p, group);
        a.pane_input(p, &image(1, 2, 2), false);
        assert_eq!(a.model.panes[&p].graphics_owner, [None; 2], "off");
        assert!(!a.model.graphics_used);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
        a.pane_input(p, &image(1, 2, 2), false);
        let pane = &a.model.panes[&p];
        assert_eq!(pane.graphics_owner, [Some(group), None]);
        assert_eq!(pane.term.graphics_storage(false).0, 12);
        assert!(a.model.graphics_used);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "off"]);
        fake::kill(group);
        a.pane_input(p, b"x", false);
        let pane = &a.model.panes[&p];
        assert_eq!(pane.term.graphics_grants(), [false; 2]);
        assert_eq!(pane.graphics_owner, [None; 2]);
        assert_eq!(pane.term.graphics_storage(false).0, 12, "the image stays");
        fake::clear();
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// D33: `vtmux-graphics-limit` holds every pane's images together. A
    /// load over it evicts across panes, the screen that loaded least
    /// recently first; lowering the limit evicts at once.
    #[test]
    fn the_limit_is_shared_and_evicts_across_panes() {
        let dir = temp_dir("graphicslimit");
        let mut a = server(&dir);
        ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
        ok(&mut a, &dir, &["split-window", "-t", "s", "cat", "-"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics-limit", "1"]);
        let (p0, p1) = (PaneId(0), PaneId(1));
        // 400x300 RGB: 360,000 bytes each.
        a.pane_input(p0, &image(1, 400, 300), false);
        a.pane_input(p1, &image(2, 400, 300), false);
        let bytes = |a: &crate::server::Server, p| a.model.panes[&p].term.graphics_storage(false).0;
        assert_eq!((bytes(&a, p0), bytes(&a, p1)), (360_000, 360_000));
        // Each granted screen may hold what the others leave.
        assert_eq!(a.model.panes[&p0].term.graphics_storage(false).1, 640_000);
        // A third over the limit: %0's, the oldest load, goes.
        a.pane_input(p1, &image(3, 400, 300), false);
        assert_eq!((bytes(&a, p0), bytes(&a, p1)), (0, 720_000));
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics-limit", "0"]);
        a.model.balance_graphics(None, std::time::Instant::now());
        assert_eq!(bytes(&a, p1), 0, "lowered: evicted at once");
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
    }
}
