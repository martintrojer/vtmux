//! What a checkpoint holds and when one is written (D27 decisions 16 and
//! 17), on a real server model.

use std::time::Instant;

use vtmux_core::ids::PaneId;
use vtmux_snapshot as snap;

use super::super::snapshot::tests::{listener, run, temp_dir};
use super::super::{Server, ServerConfig};
use super::{Mode, fingerprint};
use crate::persist::{MANIFEST, MANIFEST_PREV};

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

/// Plant typed text everywhere a snapshot keeps it, each its own word:
/// the environment (global and session), the message log (every command
/// line), prompt history (command and search), the pane's last search,
/// a title a program set and keys its program has not read yet.
fn plant_typed(server: &mut Server, dir: &std::path::Path) {
    for words in [
        &["set-environment", "-g", "MY_API_TOKEN", "ENVSECRET"][..],
        &[
            "set-environment",
            "-t",
            "a",
            "SESSION_TOKEN",
            "SESSIONENVSECRET",
        ],
        &["display-message", "-p", "LOGSECRET"],
    ] {
        let out = run(server, dir, words);
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    let limit = 100;
    server.prompt_history.add(
        "display PROMPTSECRET",
        crate::prompt_history::PromptType::Command,
        limit,
    );
    server.prompt_history.add(
        "SEARCHHISTSECRET",
        crate::prompt_history::PromptType::Search,
        limit,
    );
    let pane = server.model.panes.get_mut(&PaneId(1)).unwrap();
    pane.search = Some(("SEARCHSECRET".into(), false));
    pane.term.feed(b"\x1b]2;TITLESECRET\x07");
    if let Some(io) = pane.io.as_mut() {
        io.queue(b"QUEUEDSECRET");
    }
}

/// Every string field of `s`, via its encoding: whether `word` is in it.
fn holds(s: &snap::Snapshot, word: &str) -> bool {
    let bytes = snap::encode_manifest(s);
    bytes.windows(word.len()).any(|w| w == word.as_bytes())
}

#[test]
fn a_layout_checkpoint_holds_no_screen_and_nothing_of_the_process() {
    let (mut server, dir) = server("ckpt-layout");
    plant_typed(&mut server, &dir);
    // The upgrade snapshot carries the screen (copy mode's frozen grid),
    // the process and everything typed: what the checkpoint must not.
    let upgrade = server.to_snapshot(snap::Meta::default());
    assert!(shows_secret(&upgrade));
    assert!(upgrade.panes.iter().any(|p| p.pipe.is_some()));
    assert_ne!(upgrade.descriptors.len(), 0);
    let typed = [
        "ENVSECRET",
        "SESSIONENVSECRET",
        "LOGSECRET",
        "PROMPTSECRET",
        "SEARCHHISTSECRET",
        "SEARCHSECRET",
        "TITLESECRET",
        "QUEUEDSECRET",
        "a buffer",
    ];
    for word in typed {
        assert!(holds(&upgrade, word), "{word}");
    }
    assert!(upgrade.server.hook_fires.iter().any(|h| h.count > 0));
    assert_ne!(upgrade.server.cmd_number, 0);

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
    for word in typed {
        assert!(!holds(&layout, word), "layout: {word}");
    }
    let x = &layout.server;
    assert!(x.messages.is_empty() && x.buffers.is_empty() && x.hook_fires.is_empty());
    assert_eq!((x.message_next, x.cmd_number, x.next_client), (0, 0, 0));
    // Sessions, windows, layout and options stay.
    assert_eq!(layout.sessions.len(), 1);
    assert_eq!(layout.sessions[0].name, upgrade.sessions[0].name);
    assert_eq!(layout.panes.len(), 2);
    assert_eq!(layout.windows, upgrade.windows);
    assert_eq!(layout.server.server_options, upgrade.server.server_options);
    assert_eq!(layout.server.key_tables, upgrade.server.key_tables);
    assert_eq!(
        layout
            .panes
            .iter()
            .map(|p| (&p.argv, &p.cwd))
            .collect::<Vec<_>>(),
        upgrade
            .panes
            .iter()
            .map(|p| (&p.argv, &p.cwd))
            .collect::<Vec<_>>()
    );

    // `full` keeps the screens, titles and buffers; still nothing of the
    // process or typed.
    let full = server.checkpoint_snapshot(Mode::Full);
    assert_eq!(full.meta.purpose, snap::Purpose::CheckpointFull);
    assert!(shows_secret(&full));
    assert!(full.descriptors.is_empty() && full.clients.is_empty());
    assert!(full.panes.iter().all(|p| p.pipe.is_none()));
    assert_eq!(full.server.buffers, upgrade.server.buffers);
    for word in typed {
        let kept = matches!(word, "TITLESECRET" | "a buffer");
        assert_eq!(holds(&full, word), kept, "full: {word}");
    }
}

/// `capture-pane -e -S - -E -` of `t`.
fn captured(t: &vtmux_vt::PaneTerm) -> Vec<u8> {
    vtmux_vt::capture(
        t,
        vtmux_vt::CaptureOpts {
            escapes: true,
            start: vtmux_vt::LineRef::Start,
            end: vtmux_vt::LineRef::End,
            ..vtmux_vt::CaptureOpts::default()
        },
    )
}

/// A `full` checkpoint carries every pane's terminal, its history as
/// chunks: what comes back from disk is the same screen and history
/// (`capture-pane -e -S -`), the same cursor. Compressed history goes in
/// the vtmux row format, never in Ghostty's (decision 23: only an
/// upgrade stream carries that). A mode 2048 grant (D31) does not go: a
/// restored pane is dead, its program gone, so nothing is required.
#[test]
fn a_full_checkpoint_carries_every_pane_s_terminal() {
    let (mut server, dir) = server("ckpt-term");
    let pane = server.model.panes.get_mut(&PaneId(1)).unwrap();
    pane.term.set_inband_resize(true);
    pane.term.feed(b"\x1b[?2048h");
    assert!(pane.term.inband_resize_granted());
    for n in 0..300 {
        pane.term.feed(
            format!(
                "\x1b[3{}mline {n} \x1b]8;;http://h/{n}\x07link\x1b]8;;\x07 e\u{301}\x1b[m\r\n",
                n % 8
            )
            .as_bytes(),
        );
    }
    pane.term.feed(b"half");
    while pane.term.compress_step() == vtmux_vt::Compression::Pending {}
    let state = dir.join("state");
    let written = server.checkpoint_to(&state, Mode::Full).unwrap();
    assert!(written.chunks_new > 0, "{written:?}");
    let (back, from) = crate::persist::load(&state).unwrap();
    assert_eq!(from, MANIFEST);
    assert!(back.meta.requires.is_empty(), "{:?}", back.meta.requires);
    for id in [0, 1] {
        let p = back.panes.iter().find(|p| p.id == id).unwrap();
        let t = p
            .term
            .as_ref()
            .unwrap_or_else(|| panic!("%{id}: no terminal"));
        assert!(!crate::snapshot_term::has_ghostty_pages(t), "%{id}");
        assert!(!t.inband_resize, "%{id}");
        let restored =
            crate::snapshot_term::term_from_snapshot(t, &back.meta.ghostty_patches).unwrap();
        let live = &server.model.panes[&PaneId(id)].term;
        assert_eq!(captured(&restored), captured(live), "%{id}");
        assert_eq!(restored.cursor(), live.cursor(), "%{id}");
        assert_eq!(restored.history_size(), live.history_size(), "%{id}");
    }
    // A layout checkpoint of the same server holds none of it.
    let layout = dir.join("layout");
    assert!(server.checkpoint_to(&layout, Mode::Layout).is_some());
    let (back, _) = crate::persist::load(&layout).unwrap();
    assert!(back.panes.iter().all(|p| p.term.is_none()));
    assert_eq!(back.pages().len(), 0);
}

#[test]
fn a_checkpoint_is_written_only_when_something_changed() {
    let (mut server, dir) = server("ckpt-changed");
    let state = dir.join("state");
    let first = server.checkpoint_to(&state, Mode::Full).unwrap();
    // Copy mode's frozen grid has a history page here only if the pane
    // scrolled; either way its pages are chunks, written once.
    assert_eq!(first.chunks_kept, 0);
    assert!(state.join(MANIFEST).is_file());
    // Unchanged: nothing written, though the clock moved.
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(server.checkpoint_to(&state, Mode::Full), None);
    assert!(!state.join(MANIFEST_PREV).exists());
    // A change: the next one is written, the previous kept.
    let out = run(
        &mut server,
        &dir,
        &["rename-window", "-t", "a:0", "renamed"],
    );
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    assert!(server.checkpoint_to(&state, Mode::Full).is_some());
    assert!(state.join(MANIFEST_PREV).is_file());
    let (back, from) = crate::persist::load(&state).unwrap();
    assert_eq!(from, MANIFEST);
    assert_eq!(back.windows[0].name, "renamed");
    // Another mode is another checkpoint.
    assert!(server.checkpoint_to(&state, Mode::Layout).is_some());
    assert_eq!(server.checkpoint_to(&state, Mode::Layout), None);
}

#[test]
fn read_only_commands_are_no_change() {
    let (mut server, dir) = server("ckpt-polled");
    let state = dir.join("state");
    assert!(server.checkpoint_to(&state, Mode::Layout).is_some());
    // Each command is logged, numbered, fires its `after-*` hook and
    // comes from a new client: none of it is the server's structure.
    for words in [
        &["list-sessions"][..],
        &["list-panes", "-a"],
        &["display-message", "-p", "#{session_name}"],
        &["show-messages"],
        &["has-session", "-t", "a"],
    ] {
        let out = run(&mut server, &dir, words);
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    assert_eq!(server.checkpoint_to(&state, Mode::Layout), None);
    // Neither is typed text, in `layout`.
    plant_typed(&mut server, &dir);
    assert_eq!(server.checkpoint_to(&state, Mode::Layout), None);
    // A buffer is a change in `full` only.
    let out = run(&mut server, &dir, &["set-buffer", "more"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    assert_eq!(server.checkpoint_to(&state, Mode::Layout), None);
    assert!(server.checkpoint_to(&state, Mode::Full).is_some());
}

/// A first `full` checkpoint writes its chunks a step per loop turn: the
/// manifest only after the last, the previous one readable meanwhile.
#[test]
fn a_periodic_checkpoint_writes_its_chunks_over_several_turns() {
    let (mut server, dir) = server("ckpt-steps");
    // Copy mode over panes with history: a page each to write.
    for _ in 0..3 {
        let out = run(
            &mut server,
            &dir,
            &["new-window", "-d", "-t", "a", "cat", "-"],
        );
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    for id in 1..5 {
        let pane = server.model.panes.get_mut(&PaneId(id)).unwrap();
        for n in 0..100 {
            pane.term.feed(format!("pane {id} line {n}\r\n").as_bytes());
        }
        let target = format!("%{id}");
        let out = run(&mut server, &dir, &["copy-mode", "-t", &target]);
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    let out = run(&mut server, &dir, &["set", "-s", "vtmux-persist", "full"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    let pages = server.checkpoint_snapshot(Mode::Full).pages().len();
    assert!(pages >= 4, "{pages}");
    let state = dir.join("state");
    server.persist.dir = Some(state.clone());
    server.persist.step = std::time::Duration::ZERO;
    let start = Instant::now();
    server.persist_timers(start);
    assert!(server.persist_deadline().is_some_and(|d| d > start));
    let due = start + super::interval();
    server.persist_timers(due);
    // One chunk per turn at a zero budget; the loop doesn't sleep.
    let mut turns = 1;
    while !state.join(MANIFEST).exists() {
        assert!(
            server
                .persist_deadline()
                .is_some_and(|d| d <= Instant::now())
        );
        let chunks = std::fs::read_dir(state.join(crate::persist::CHUNKS))
            .unwrap()
            .count();
        assert_eq!(chunks, turns, "turn {turns}");
        server.persist_timers(due);
        turns += 1;
        assert!(turns <= pages + 1, "{turns} turns for {pages} pages");
    }
    assert_eq!(turns, pages);
    let (back, from) = crate::persist::load(&state).unwrap();
    assert_eq!(from, MANIFEST);
    assert_eq!(back.pages().len(), pages);
    // Done: back to the interval, and nothing more to write.
    assert_eq!(server.persist_deadline(), Some(due + super::interval()));
    assert_eq!(server.checkpoint(), None);
}

/// Every file under `dir`, recursively.
fn all_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            out.extend(all_files(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

/// `vtmux-persist` set from `full` to `layout` at run time writes the
/// `layout` checkpoint at once and leaves no screen or history on disk:
/// `manifest.prev` was the full checkpoint (and `chunks/` its pages)
/// until the next changed checkpoint, which may be far off.
#[test]
fn full_set_to_layout_writes_a_layout_checkpoint_and_prunes() {
    let (mut server, dir) = server("ckpt-prune");
    for _ in 0..2 {
        let out = run(
            &mut server,
            &dir,
            &["new-window", "-d", "-t", "a", "cat", "-"],
        );
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    for id in 1..3 {
        let pane = server.model.panes.get_mut(&PaneId(id)).unwrap();
        for n in 0..100 {
            pane.term
                .feed(format!("history {id} of line {n}\r\n").as_bytes());
        }
        let target = format!("%{id}");
        let out = run(&mut server, &dir, &["copy-mode", "-t", &target]);
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    let out = run(&mut server, &dir, &["set", "-s", "vtmux-persist", "full"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    let state = dir.join("state");
    server.persist.dir = Some(state.clone());
    let now = Instant::now();
    // The loop sees `full`, and writes the full checkpoints.
    server.persist_timers(now);
    assert!(server.checkpoint().is_some());
    let out = run(&mut server, &dir, &["rename-window", "-t", "a:0", "next"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    assert!(server.checkpoint().is_some());
    let chunks = state.join(crate::persist::CHUNKS);
    let full = |name| crate::persist::load_file(&state, name).unwrap().0;
    assert_ne!(full(MANIFEST_PREV).pages().len(), 0);
    assert!(std::fs::read_dir(&chunks).unwrap().count() > 0);

    let out = run(&mut server, &dir, &["set", "-s", "vtmux-persist", "layout"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    // The next turn, not the next interval.
    server.persist_timers(now);
    for name in [MANIFEST, MANIFEST_PREV] {
        let s = full(name);
        assert_eq!(s.meta.purpose, snap::Purpose::CheckpointLayout, "{name}");
        assert!(s.pages().is_empty(), "{name}");
        assert!(s.panes.iter().all(|p| p.term.is_none()), "{name}");
    }
    assert_eq!(std::fs::read_dir(&chunks).unwrap().count(), 0);
    let files = all_files(&state);
    let names: Vec<_> = files
        .iter()
        .map(|f| f.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|n| ["lock", MANIFEST, MANIFEST_PREV].contains(&n.as_str())),
        "{names:?}"
    );
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        assert!(!bytes.windows(7).any(|w| w == b"of line"), "{f:?}");
    }
    // Written once: nothing changed since.
    assert_eq!(server.checkpoint(), None);
    server.persist_timers(now + super::interval());
    assert_eq!(server.checkpoint(), None);
}

/// `full` set to `layout` and the server shut down in the same turn
/// (`set -s vtmux-persist layout ; kill-server`): the shutdown checkpoint
/// prunes, as the next turn would have.
#[test]
fn full_set_to_layout_then_shut_down_at_once_prunes() {
    let (mut server, dir) = server("ckpt-prune-exit");
    let out = run(&mut server, &dir, &["set", "-s", "vtmux-persist", "full"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    let state = dir.join("state");
    server.persist.dir = Some(state.clone());
    server.persist_timers(Instant::now());
    assert!(server.checkpoint().is_some());
    let out = run(&mut server, &dir, &["rename-window", "-t", "a:0", "next"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    assert!(server.checkpoint().is_some());
    let load = |name| crate::persist::load_file(&state, name).unwrap().0;
    assert!(shows_secret(&load(MANIFEST_PREV)));

    let out = run(&mut server, &dir, &["set", "-s", "vtmux-persist", "layout"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    server.checkpoint_exit();
    for name in [MANIFEST, MANIFEST_PREV] {
        let s = load(name);
        assert_eq!(s.meta.purpose, snap::Purpose::CheckpointLayout, "{name}");
        assert!(s.panes.iter().all(|p| p.term.is_none()), "{name}");
    }
    for f in all_files(&state) {
        let bytes = std::fs::read(&f).unwrap();
        assert!(
            !bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
            "{f:?}"
        );
    }
}

/// Two servers whose checkpoints would share a directory: the first to
/// write holds its lock, the other writes nothing and says why once.
#[test]
fn a_locked_directory_is_not_written() {
    let (mut one, dir) = server("ckpt-lock-a");
    let (mut two, dir2) = server("ckpt-lock-b");
    let state = dir.join("state");
    assert!(one.checkpoint_to(&state, Mode::Layout).is_some());
    let out = run(&mut two, &dir2, &["rename-window", "-t", "a:0", "two"]);
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    assert_eq!(two.checkpoint_to(&state, Mode::Layout), None);
    assert_eq!(two.checkpoint_to(&state, Mode::Layout), None);
    let log = run(&mut two, &dir2, &["show-messages"]);
    let said: Vec<&str> = log
        .lines()
        .filter(|t| t.contains("in use by another server"))
        .collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(!state.join(MANIFEST_PREV).exists());
    assert_ne!(
        crate::persist::load(&state).unwrap().0.windows[0].name,
        "two"
    );
    // The first gone, its lock goes with it. A pane another test thread is
    // spawning right now may have forked while `one` held the lock and
    // still holds the descriptor (and so the flock, which lives with the
    // open file) until it execs: wait for that, as a real second server
    // would on its next checkpoint.
    drop(one);
    let give_up = Instant::now() + std::time::Duration::from_secs(60);
    while two.checkpoint_to(&state, Mode::Layout).is_none() {
        assert!(Instant::now() < give_up, "the lock was never released");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        crate::persist::load(&state).unwrap().0.windows[0].name,
        "two"
    );
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
    b.server.activity += 1;
    b.sessions[0].activity += 1;
    b.sessions[0].activity_secs += 60;
    b.windows[0].activity += 1;
    b.windows[0].activity_secs += 60;
    assert_eq!(fingerprint(&a), fingerprint(&b));
    b.panes[0].title = "changed".into();
    assert_ne!(fingerprint(&a), fingerprint(&b));
}

/// The perf gate of the checkpoint (`cargo test --release -p vtmux-server
/// --lib bench_checkpoint -- --ignored --nocapture`): 10 panes x 100k
/// lines of history. `layout` on the real model (terminal state is never
/// exported for it, so its cost must not grow with history); `full` on
/// the real model (every terminal exported, the unchanged one too, as the
/// fingerprint needs it), then with the history as synthetic pages of
/// 1000 rows: the first checkpoint, an unchanged one, and one with a new
/// page per pane.
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
    bench_real_full(&mut server, &dir);
    bench_full(&server, &dir);
}

fn bench_real_full(server: &mut Server, dir: &std::path::Path) {
    let start = Instant::now();
    let pages = server.checkpoint_snapshot(Mode::Full).pages().len();
    println!("full export: {:?}, {pages} pages", start.elapsed());
    let state = dir.join("real");
    let start = Instant::now();
    let written = server.checkpoint_to(&state, Mode::Full).unwrap();
    println!(
        "full first: {:?}, {} chunks, {} KB",
        start.elapsed(),
        written.chunks_new,
        written.bytes / 1000
    );
    let start = Instant::now();
    assert_eq!(server.checkpoint_to(&state, Mode::Full), None);
    println!("full unchanged: {:?}", start.elapsed());
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
    // The periodic path: a step per loop turn at the server's budget.
    let stepped = dir.join("stepped");
    let start = Instant::now();
    let mut w = crate::persist::Writer::new(&stepped, &s).unwrap();
    let begun = start.elapsed();
    let (mut steps, mut longest) = (0, std::time::Duration::ZERO);
    loop {
        let t = Instant::now();
        let done = w.step(Some(super::STEP), &mut |_| Ok(())).unwrap();
        longest = longest.max(t.elapsed());
        steps += 1;
        if done {
            break;
        }
    }
    println!(
        "full first, stepped: {:?} in {steps} steps, longest {longest:?}, begun in {begun:?}",
        start.elapsed()
    );
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
