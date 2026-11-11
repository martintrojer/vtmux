//! A string with no end (an OSC, an APC) is held in memory at most as far as
//! tmux holds it: `input_input` collects up to `input-buffer-size` and
//! discards the rest, so a program that opens one and never ends it, or ends
//! it after megabytes, costs the pane about that much and no more.
//!
//! Ghostty's OSC parser instead keeps up to 8 MiB of an OSC it knows how to
//! collect (OSC 52's clipboard text is one): 12 panes held ~110 MB. The
//! pane's scanner counts what tmux counts and, at the limit, gives Ghostty
//! an OSC it rejects, which it skips without keeping any of it.
//!
//! Read from `/proc/self/statm`, so Linux only; its own test binary, so no
//! other test's allocations move the number.

#![cfg(target_os = "linux")]

use vtmux_vt::PaneTerm;

fn rss_kib() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("statm");
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .expect("resident pages");
    pages * 4
}

/// Panes that opened `lead` and got `mib` MiB of text in 16 KiB reads (what a
/// pty read gives), none of them ended: what they cost together, in KiB.
fn cost(panes: usize, lead: &[u8], mib: usize) -> usize {
    let read = [b'a'; 16384];
    let before = rss_kib();
    let held: Vec<PaneTerm> = (0..panes)
        .map(|_| {
            let mut t = PaneTerm::new(80, 24, 100);
            t.feed(lead);
            for _ in 0..mib * 64 {
                t.feed(&read);
            }
            t
        })
        .collect();
    let cost = rss_kib().saturating_sub(before);
    drop(held);
    cost
}

/// Every string kind a pane may be inside, 4 panes at 9 MiB each (more than
/// Ghostty's 8 MiB limit). Under 5 MiB a pane: `input-buffer-size` is 1 MiB,
/// the pane keeps twice that of the input (a live upgrade replays it) and
/// once for the title being read.
#[test]
fn an_unfinished_string_costs_what_tmux_holds_of_it() {
    // The OSC Ghostty collects first: this is the one that was unbounded.
    for (what, lead) in [
        ("OSC 52", &b"\x1b]52;c;"[..]),
        ("OSC 9999 (unknown)", b"\x1b]9999;"),
        ("OSC 2", b"\x1b]2;"),
        ("APC", b"\x1b_"),
        ("APC G (kitty graphics)", b"\x1b_G"),
        // Its payload, which Ghostty buffers to 65 MiB unless told less.
        ("APC G payload (kitty graphics)", b"\x1b_Ga=T,f=24,s=1,v=1;"),
    ] {
        let kib = cost(4, lead, 9);
        assert!(kib < 4 * 5 * 1024, "{what}: {kib} KiB for 4 panes");
    }
}
