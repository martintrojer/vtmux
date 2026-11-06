//! What a checkpoint holds and when one is written (D27 decisions 16 and
//! 17), on a real server model.

use std::time::Instant;

use vtmux_core::ids::PaneId;
use vtmux_snapshot as snap;

use super::super::snapshot::tests::{listener, run, temp_dir};
use super::super::{Server, ServerConfig};
use super::{Mode, fingerprint};

/// Text no command line carries: the pane prints it from two halves.
const SECRET: &str = "SECRET-4242";

/// A server with session `a`: pane %0 showing [`SECRET`] in copy mode,
/// pane %1 piped (`pipe-pane`).
fn server(tag: &str) -> (Server, std::path::PathBuf) {
    let dir = temp_dir(tag);
    let (l, socket) = listener(&dir, "s");
    let mut server = Server::new(l, socket, ServerConfig::default()).unwrap();
    for words in [
        &[
            "new-session",
            "-d",
            "-s",
            "a",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
            "-",
        ][..],
        &["split-window", "-d", "-t", "a", "cat", "-"],
        &["pipe-pane", "-t", "%1", "cat >/dev/null"],
        &["set-buffer", "-b", "kept", "a buffer"],
    ] {
        let out = run(&mut server, &dir, words);
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    let pane = server.model.panes.get_mut(&PaneId(0)).unwrap();
    pane.term.feed(b"SECRET-");
    pane.term.feed(b"4242\r\n");
    let out = run(&mut server, &dir, &["copy-mode", "-t", "%0"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    (server, dir)
}

/// Whether any row of `s` reads [`SECRET`].
fn shows_secret(s: &snap::Snapshot) -> bool {
    let text = |r: &snap::Row| -> String {
        r.cells
            .iter()
            .filter_map(|c| char::from_u32(c.codepoint))
            .collect()
    };
    let mut found = false;
    for p in &s.panes {
        for m in &p.modes {
            if let snap::PaneMode::Copy(c) | snap::PaneMode::View(c) = m {
                found |= c.rows.iter().any(|r| text(r).contains(SECRET));
            }
        }
        if let Some(t) = &p.term {
            found |= t.primary.rows.iter().any(|r| text(r).contains(SECRET));
        }
    }
    for page in s.pages() {
        page.each_row(|r| found |= text(r).contains(SECRET))
            .unwrap();
    }
    found
}

#[test]
fn a_layout_checkpoint_holds_no_screen_and_nothing_of_the_process() {
    let (server, _dir) = server("ckpt-layout");
    // The upgrade snapshot carries the screen (copy mode's frozen grid)
    // and the process: what the checkpoint must not.
    let upgrade = server.to_snapshot(snap::Meta::default());
    assert!(shows_secret(&upgrade));
    assert!(upgrade.panes.iter().any(|p| p.pipe.is_some()));
    assert_ne!(upgrade.descriptors.len(), 0);

    let layout = server.checkpoint_snapshot(Mode::Layout);
    assert_eq!(layout.meta.purpose, snap::Purpose::CheckpointLayout);
    assert!(!shows_secret(&layout));
    assert_eq!(layout.pages().len(), 0);
    for p in &layout.panes {
        assert!(p.term.is_none(), "%{}", p.id);
        assert!(p.modes.is_empty(), "%{}: {:?}", p.id, p.modes);
        assert!(p.pipe.is_none(), "%{}", p.id);
    }
    assert!(layout.descriptors.is_empty() && layout.clients.is_empty());
    // Sessions, windows, layout, options and buffers stay.
    assert_eq!(layout.sessions.len(), 1);
    assert_eq!(layout.panes.len(), 2);
    assert_eq!(layout.windows, upgrade.windows);
    assert_eq!(layout.server.buffers, upgrade.server.buffers);
    assert_eq!(layout.server.server_options, upgrade.server.server_options);

    // `full` keeps the screens; still nothing of the process.
    let full = server.checkpoint_snapshot(Mode::Full);
    assert_eq!(full.meta.purpose, snap::Purpose::CheckpointFull);
    assert!(shows_secret(&full));
    assert!(full.descriptors.is_empty() && full.clients.is_empty());
    assert!(full.panes.iter().all(|p| p.pipe.is_none()));
}

#[test]
fn a_checkpoint_is_written_only_when_something_changed() {
    let (mut server, dir) = server("ckpt-changed");
    let state = dir.join("state");
    let first = server.checkpoint_to(&state, Mode::Full).unwrap();
    // Copy mode's frozen grid has a history page here only if the pane
    // scrolled; either way its pages are chunks, written once.
    assert_eq!(first.chunks_kept, 0);
    assert!(state.join(crate::persist::MANIFEST).is_file());
    // Unchanged: nothing written, though the clock moved.
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(server.checkpoint_to(&state, Mode::Full), None);
    assert!(!state.join(crate::persist::MANIFEST_PREV).exists());
    // A change: the next one is written, the previous kept.
    let out = run(
        &mut server,
        &dir,
        &["rename-window", "-t", "a:0", "renamed"],
    );
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    assert!(server.checkpoint_to(&state, Mode::Full).is_some());
    assert!(state.join(crate::persist::MANIFEST_PREV).is_file());
    let (back, from) = crate::persist::load(&state).unwrap();
    assert_eq!(from, crate::persist::MANIFEST);
    assert_eq!(back.windows[0].name, "renamed");
    // Another mode is another checkpoint.
    assert!(server.checkpoint_to(&state, Mode::Layout).is_some());
    assert_eq!(server.checkpoint_to(&state, Mode::Layout), None);
}

#[test]
fn the_fingerprint_ignores_the_clock_alone() {
    let (server, _dir) = server("ckpt-print");
    let mut a = server.checkpoint_snapshot(Mode::Layout);
    a.panes[0].output_ago_ms = Some(10);
    a.panes[0].resize_in_ms = Some(10);
    a.panes[0].rename_ago_ms = Some(10);
    a.sessions[0].lock_in_ms = Some(10);
    a.windows[0].silence_in_ms = Some(10);
    let mut b = a.clone();
    b.meta.written_us += 1_000_000;
    b.panes[0].output_ago_ms = Some(60_010);
    b.panes[0].resize_in_ms = None;
    b.panes[0].rename_ago_ms = Some(60_010);
    b.sessions[0].lock_in_ms = Some(1);
    b.windows[0].silence_in_ms = None;
    assert_eq!(fingerprint(&a), fingerprint(&b));
    b.panes[0].title = "changed".into();
    assert_ne!(fingerprint(&a), fingerprint(&b));
}

/// The perf gate of the checkpoint (`cargo test --release -p vtmux-server
/// --lib bench_checkpoint -- --ignored --nocapture`): 10 panes x 100k
/// lines of history. `layout` on the real model (terminal state is never
/// exported for it, so its cost must not grow with history); `full` with
/// the history as pages of 1000 rows (what the pane terminal state export
/// will hand the writer): the first checkpoint, an unchanged one, and one
/// with a new page per pane.
#[test]
#[ignore = "timing, run by hand in release"]
fn bench_checkpoint_ten_panes_of_100k_lines() {
    let dir = temp_dir("ckpt-bench");
    let (socket_listener, socket) = listener(&dir, "s");
    let mut server = Server::new(socket_listener, socket, ServerConfig::default()).unwrap();
    run(&mut server, &dir, &["set", "-g", "history-limit", "100000"]);
    run(
        &mut server,
        &dir,
        &[
            "new-session",
            "-d",
            "-s",
            "a",
            "-x",
            "200",
            "-y",
            "50",
            "cat",
            "-",
        ],
    );
    for _ in 1..10 {
        run(
            &mut server,
            &dir,
            &["new-window", "-d", "-t", "a", "cat", "-"],
        );
    }
    let start = Instant::now();
    let mut text = Vec::new();
    for line in 0..BENCH_LINES {
        text.extend_from_slice(format!("{}\r\n", bench_line(line)).as_bytes());
    }
    for p in server.model.panes.values_mut() {
        p.term.feed(&text);
    }
    println!("fed 10 x {BENCH_LINES} lines in {:?}", start.elapsed());
    bench_layout(&mut server, &dir);
    bench_full(&server, &dir);
}

const BENCH_LINES: usize = 100_000;
const BENCH_PER_PAGE: usize = 1000;

fn bench_line(n: usize) -> String {
    format!(
        "{n:07} the quick brown fox jumps over the lazy dog {}",
        n % 97
    )
}

fn bench_layout(server: &mut Server, dir: &std::path::Path) {
    let state = dir.join("state");
    let start = Instant::now();
    let written = server.checkpoint_to(&state, Mode::Layout).unwrap();
    println!(
        "layout first: {:?}, {} bytes",
        start.elapsed(),
        written.bytes
    );
    let start = Instant::now();
    assert_eq!(server.checkpoint_to(&state, Mode::Layout), None);
    println!("layout unchanged: {:?}", start.elapsed());
    run(server, dir, &["rename-window", "-t", "a:0", "x"]);
    let start = Instant::now();
    let written = server.checkpoint_to(&state, Mode::Layout).unwrap();
    println!(
        "layout changed: {:?}, {} bytes",
        start.elapsed(),
        written.bytes
    );
}

/// `server`'s full checkpoint with every pane's history as pages, each
/// pane `extra` lines longer.
fn bench_with_history(server: &Server, extra: usize) -> snap::Snapshot {
    let row = |n: usize| {
        let text = bench_line(n);
        let mut r = snap::Row {
            cells: vec![snap::Cell::default(); 200],
            cells_used: u16::try_from(text.len()).unwrap(),
            ..snap::Row::default()
        };
        for (c, ch) in r.cells.iter_mut().zip(text.chars()) {
            c.codepoint = u32::from(ch);
        }
        r
    };
    let mut s = server.checkpoint_snapshot(Mode::Full);
    for (i, p) in s.panes.iter_mut().enumerate() {
        let mut history = Vec::new();
        let mut b = snap::PageBuilder::new();
        for line in 0..BENCH_LINES + extra {
            b.push(&row(i * 1_000_000 + line));
            if b.rows() as usize == BENCH_PER_PAGE {
                history.push(std::mem::take(&mut b).finish(false));
            }
        }
        p.term = Some(snap::TermState {
            primary: snap::term::Screen {
                history,
                ..snap::term::Screen::default()
            },
            ..snap::TermState::default()
        });
    }
    s
}

fn bench_full(server: &Server, dir: &std::path::Path) {
    let full_dir = dir.join("full");
    let write = |s: &snap::Snapshot, what: &str| {
        let start = Instant::now();
        let w = crate::persist::write(&full_dir, s, &mut |_| Ok(())).unwrap();
        println!(
            "full {what}: {:?}, {} new, {} kept, {} removed, {} KB",
            start.elapsed(),
            w.chunks_new,
            w.chunks_kept,
            w.chunks_removed,
            w.bytes / 1000
        );
    };
    let s = bench_with_history(server, 0);
    write(&s, "first");
    let start = Instant::now();
    let print = fingerprint(&s);
    println!(
        "full fingerprint: {:?}, {} bytes",
        start.elapsed(),
        print.len()
    );
    write(&s, "again");
    write(
        &bench_with_history(server, BENCH_PER_PAGE),
        "one new page per pane",
    );
}
