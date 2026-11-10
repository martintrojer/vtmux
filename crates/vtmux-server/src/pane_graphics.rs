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
//! together, with their pending (chunked, unfinished) loads. A granted
//! screen's limit in Ghostty is what the others leave
//! ([`Model::balance_graphics`]), and a pending load never holds more
//! than its screen's limit (patch 0041); while a granted pane's output is
//! parsed its active screen may take the whole cap, and the excess is then
//! dropped across the server: other screens' pending loads, then images,
//! the screen that grew least recently first, each by lowering its limit
//! (Ghostty evicts unused images first, then the oldest: `evictImage`,
//! `graphics_storage.zig:1811`).

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
            let Some(group) = self.graphics.owner[screen] else {
                continue;
            };
            if !crate::pane_kitty::group_alive(group) {
                self.term.set_graphics_grant(screen == 1, false);
                self.graphics.owner[screen] = None;
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
        for (owner, _) in self.graphics.owner.iter_mut().zip(granted).filter(|x| x.1) {
            *owner = group;
        }
        self.graphics.held = true;
        true
    }

    /// A pane imported from a snapshot (its images are gone): a grant whose
    /// owner is gone ends, and one with no owner recorded is the
    /// foreground group's; a pane with no process has none.
    pub fn settle_imported_graphics_grants(&mut self) {
        let grants = self.term.graphics_grants();
        if grants == [false; 2] {
            self.graphics.owner = [None; 2];
            return;
        }
        if self.io.is_none() {
            self.reset_graphics();
            return;
        }
        let group = self.foreground_pgid();
        for (owner, _) in self.graphics.owner.iter_mut().zip(grants).filter(|x| x.1) {
            if owner.is_none() {
                *owner = group;
            }
        }
        self.end_dead_graphics_grants();
        self.graphics.held = true;
    }

    /// A respawn: the old program's grants end and its images go.
    pub fn reset_graphics(&mut self) {
        self.term.reset_graphics();
        self.graphics.owner = [None; 2];
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
    /// may load up to the whole cap, and so may a screen this output
    /// grants (what it takes beyond its share is evicted across the server
    /// after, [`Self::balance_graphics`]).
    pub fn prepare_graphics(&mut self, p: PaneId) {
        let on = self.graphics_on();
        if !on && !self.graphics_used {
            // Off and never used: tmux's APC, and nothing to count.
            if let Some(pane) = self.panes.get_mut(&p) {
                pane.term.set_graphics(false);
            }
            return;
        }
        let cap = self.graphics_cap();
        let Some(pane) = self.panes.get_mut(&p) else {
            return;
        };
        pane.term.set_graphics(on);
        pane.end_dead_graphics_grants();
        pane.term.set_graphics_share(cap.max(1));
        if pane.graphics.held {
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
        if pane.graphics.held {
            self.balance_graphics(Some(p), now);
        }
    }

    /// Hold the server to `vtmux-graphics-limit`, images and pending
    /// (chunked, unfinished) loads together. Over it, in order: the pending
    /// loads of screens other than the one just read (`loaded`), the one
    /// that grew least recently first, as they show nothing; then images,
    /// the screen that grew least recently first and the one just read
    /// last, each by lowering its limit (Ghostty's order within it); then
    /// the pending load of the screen just read. Each granted screen then
    /// gets what the others leave. A pane whose screens are neither
    /// granted nor hold anything is let go. Nothing to do while no screen
    /// was ever granted.
    pub fn balance_graphics(&mut self, loaded: Option<PaneId>, now: Instant) {
        if !self.graphics_used {
            return;
        }
        let cap = self.graphics_cap();
        let mut screens: Vec<Screen> = Vec::new();
        for (&id, pane) in &mut self.panes {
            if !pane.graphics.held {
                continue;
            }
            let grants = pane.term.graphics_grants();
            for alternate in [false, true] {
                let (stored, pending) = screen_bytes(pane, alternate);
                let i = usize::from(alternate);
                if stored + pending > pane.graphics.bytes[i] {
                    pane.graphics.loaded[i] = Some(now);
                }
                pane.graphics.bytes[i] = stored + pending;
                screens.push(Screen {
                    pane: id,
                    alternate,
                    stored,
                    pending,
                    granted: grants[i],
                    loaded: pane.graphics.loaded[i],
                    just_read: Some(id) == loaded,
                });
            }
        }
        let mut total: u64 = screens.iter().map(|s| s.stored + s.pending).sum();
        if total > cap {
            // Least recently grown first; the pane just read last.
            screens.sort_by_key(|s| (s.just_read, s.loaded));
            for pass in [Evict::StalePending, Evict::Images, Evict::Pending] {
                for s in &mut screens {
                    if total <= cap {
                        break;
                    }
                    let Some(pane) = self.panes.get_mut(&s.pane) else {
                        continue;
                    };
                    let drop_pending = match pass {
                        Evict::StalePending => !s.just_read,
                        Evict::Images => false,
                        Evict::Pending => true,
                    };
                    if drop_pending && s.pending > 0 {
                        pane.term.drop_graphics_pending(s.alternate);
                        total -= s.pending;
                        s.pending = 0;
                    } else if pass == Evict::Images && s.stored > 0 {
                        let keep = s.stored.saturating_sub(total - cap);
                        pane.term.set_graphics_limit(s.alternate, keep);
                        let (stored, _) = pane.term.graphics_storage(s.alternate);
                        total = total - s.stored + stored;
                        s.stored = stored;
                    } else {
                        continue;
                    }
                    pane.graphics.bytes[usize::from(s.alternate)] = s.stored + s.pending;
                }
            }
        }
        for s in &screens {
            let Some(pane) = self.panes.get_mut(&s.pane) else {
                continue;
            };
            let own = s.stored + s.pending;
            let limit = if s.granted {
                // What the others leave, never 0: Ghostty takes 0 as no
                // graphics and would stop answering the program.
                cap.saturating_sub(total - own).max(s.stored).max(1)
            } else {
                s.stored
            };
            if pane.term.graphics_storage(s.alternate).1 != limit {
                pane.term.set_graphics_limit(s.alternate, limit);
            }
        }
        for pane in self.panes.values_mut().filter(|p| p.graphics.held) {
            if pane.term.graphics_grants() == [false; 2] && pane.graphics.bytes == [0; 2] {
                pane.graphics.held = false;
            }
        }
        self.graphics_used = self.panes.values().any(|p| p.graphics.held);
    }
}

/// A screen's image bytes and its pending load's.
fn screen_bytes(pane: &Pane, alternate: bool) -> (u64, u64) {
    (
        pane.term.graphics_storage(alternate).0,
        pane.term.graphics_pending(alternate),
    )
}

/// A pane screen as [`Model::balance_graphics`] counts it.
struct Screen {
    pane: PaneId,
    alternate: bool,
    stored: u64,
    pending: u64,
    granted: bool,
    /// When it last grew.
    loaded: Option<Instant>,
    /// Its pane's output was just parsed.
    just_read: bool,
}

/// [`Model::balance_graphics`]'s passes over the limit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Evict {
    /// The pending loads of screens not just read.
    StalePending,
    /// Images.
    Images,
    /// The pending load of the screen just read.
    Pending,
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
        assert_eq!(a.model.panes[&p].graphics.owner, [None; 2], "off");
        assert!(!a.model.graphics_used);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
        a.pane_input(p, &image(1, 2, 2), false);
        let pane = &a.model.panes[&p];
        assert_eq!(pane.graphics.owner, [Some(group), None]);
        assert_eq!(pane.term.graphics_storage(false).0, 12);
        assert!(a.model.graphics_used);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "off"]);
        fake::kill(group);
        a.pane_input(p, b"x", false);
        let pane = &a.model.panes[&p];
        assert_eq!(pane.term.graphics_grants(), [false; 2]);
        assert_eq!(pane.graphics.owner, [None; 2]);
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

    /// A chunked load of `w` x `h` RGB as `id`, `chunks` escapes of 3072
    /// bytes each, the last with `m=1` (unfinished) unless `last`.
    fn chunked(id: u32, w: u32, h: u32, chunks: usize, last: bool) -> Vec<u8> {
        let data = "AAAA".repeat(1024);
        let mut out = format!("\x1b_Ga=t,f=24,s={w},v={h},i={id},q=2,m=1;{data}\x1b\\");
        for n in 1..chunks {
            let m = u8::from(n + 1 < chunks || !last);
            let _ = write!(out, "\x1b_Gm={m};{data}\x1b\\");
        }
        out.into_bytes()
    }

    /// Every pane's image and pending bytes together.
    fn held(a: &crate::server::Server) -> u64 {
        a.model
            .panes
            .values()
            .flat_map(|p| {
                [false, true]
                    .map(|alt| p.term.graphics_storage(alt).0 + p.term.graphics_pending(alt))
            })
            .sum()
    }

    /// D33: a chunked load never finished holds no more than
    /// `vtmux-graphics-limit`, read after read: one of 6 MB under a 1 MB
    /// limit stays under it, and the server with it.
    #[test]
    fn a_chunked_load_past_the_limit_is_bounded() {
        let dir = temp_dir("graphicspending");
        let mut a = server(&dir);
        ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics-limit", "1"]);
        let p = PaneId(0);
        // 2000 chunks of 3072 bytes: 6 MB, sent 100 chunks a read.
        let load = chunked(1, 1000, 2048, 2000, false);
        for read in load.chunks(load.len() / 20) {
            a.pane_input(p, read, false);
            assert!(held(&a) <= 1_000_000, "{}", held(&a));
        }
        // Its last chunk ends it, and an image that fits loads.
        a.pane_input(p, b"\x1b_Gm=0;AAAA\x1b\\", false);
        assert_eq!(held(&a), 0);
        a.pane_input(p, &image(2, 400, 300), false);
        assert_eq!(a.model.panes[&p].term.graphics_storage(false).0, 360_000);
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// D33: a load's first chunk costs its size, and one past the limit
    /// (a command may carry a little more) is not held: neither evicts
    /// %0's image.
    #[test]
    fn a_first_chunk_is_held_to_the_limit() {
        let dir = temp_dir("graphicsfirst");
        let mut a = server(&dir);
        ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
        ok(&mut a, &dir, &["split-window", "-t", "s", "cat", "-"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics-limit", "1"]);
        let (p0, p1) = (PaneId(0), PaneId(1));
        a.pane_input(p0, &image(1, 100, 300), false);
        let first = |id, w, h| {
            let load = String::from_utf8(image(id, w, h)).unwrap();
            load.replace("q=2;", "q=2,m=1;").into_bytes()
        };
        // 900,000 bytes: 90,000 left for %0's image.
        a.pane_input(p1, &first(2, 1000, 300), false);
        assert_eq!(a.model.panes[&p1].term.graphics_pending(false), 900_000);
        assert_eq!(held(&a), 990_000, "%0's image kept");
        // One byte too many: it fails, and holds nothing.
        a.pane_input(p1, b"\x1b_Gm=0,q=2;AA==\x1b\\", false);
        assert_eq!(held(&a), 90_000);
        // 1,002,000 bytes: past the limit, within one command.
        a.pane_input(p1, &first(3, 1000, 334), false);
        assert_eq!(a.model.panes[&p1].term.graphics_pending(false), 0);
        assert_eq!(held(&a), 90_000, "%0's image kept");
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// D33: pending loads count against the limit with the images. Over
    /// it, the pending load of a screen not just read goes first, the
    /// oldest first, before any image; the pane loading keeps its load.
    #[test]
    fn pending_loads_count_and_the_oldest_goes_first() {
        let dir = temp_dir("graphicsoldest");
        let mut a = server(&dir);
        ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
        ok(&mut a, &dir, &["split-window", "-t", "s", "cat", "-"]);
        ok(&mut a, &dir, &["split-window", "-t", "s", "cat", "-"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
        ok(&mut a, &dir, &["set", "-s", "vtmux-graphics-limit", "1"]);
        let (p0, p1, p2) = (PaneId(0), PaneId(1), PaneId(2));
        let pending = |a: &crate::server::Server, p| a.model.panes[&p].term.graphics_pending(false);
        let stored =
            |a: &crate::server::Server, p| a.model.panes[&p].term.graphics_storage(false).0;
        // %0 starts a load of 100 chunks (307,200 bytes), %1 one of 2,
        // and stop; %2 holds an image of 360,000.
        a.pane_input(p0, &chunked(1, 1000, 1000, 100, false), false);
        a.pane_input(p1, &chunked(2, 1000, 1000, 2, false), false);
        a.pane_input(p2, &image(3, 400, 300), false);
        let (was0, was1) = (pending(&a, p0), pending(&a, p1));
        assert!(was0 >= 307_200 && was1 >= 6144, "{was0} {was1}");
        assert_eq!(stored(&a, p2), 360_000);
        assert_eq!(held(&a), was0 + was1 + 360_000, "under the limit, all kept");
        // %2 starts another, a chunk a read: once over the limit, %0's
        // pending load (the oldest) goes, and that is enough: %1's stays,
        // and so do the image and %2's load.
        let load = chunked(4, 1000, 1000, 300, false);
        let start = load.iter().position(|&b| b == b';').unwrap();
        a.pane_input(p2, &load[..start], false);
        for chunk in load[start..].split_inclusive(|&b| b == b'\\') {
            a.pane_input(p2, chunk, false);
            assert!(held(&a) <= 1_000_000, "{}", held(&a));
            if pending(&a, p0) == 0 {
                break;
            }
        }
        assert_eq!(pending(&a, p0), 0);
        assert_eq!(pending(&a, p1), was1);
        assert_eq!(stored(&a, p2), 360_000);
        assert!(pending(&a, p2) > 0);
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
    }
}
