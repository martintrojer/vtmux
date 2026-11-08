//! `PaneTerm::export_state` / `import_state` round trips (live upgrade
//! spec, decisions 5 and 24): a pane imported from an export is the same
//! terminal. Every capture (`capture-pane -e -S -` and the other flags),
//! the cursor, the modes, every row's flags and the whole re-exported
//! state are equal, and feeding both the same input afterwards keeps them
//! equal and gives the same replies, which proves the state no capture
//! shows (saved cursor, charsets, tab stops, scroll region, modes,
//! parser state) came across too.

#[path = "state/harness.rs"]
mod harness;

use harness::{Fuzz, PROBE, exported, exported_flat, fuzz_seeds, observe, unescape};
use vtmux_vt::{CompressedPages, Compression, PageState, PaneTerm, StateError};

fn assert_same(a: &PaneTerm, b: &PaneTerm, ctx: &str) {
    assert_alike(a, b, ctx, true);
}

/// `exact`: the re-exported states are equal, pages and capacities too;
/// else only their rows are.
fn assert_alike(a: &PaneTerm, b: &PaneTerm, ctx: &str, exact: bool) {
    let (oa, ob) = (observe(a, exact), observe(b, exact));
    if oa != ob {
        let line = oa
            .lines()
            .zip(ob.lines())
            .position(|(x, y)| x != y)
            .unwrap_or(0);
        panic!(
            "{ctx}: observations differ at line {line}\n--- source\n{}\n--- import\n{}",
            oa.lines().nth(line).unwrap_or(""),
            ob.lines().nth(line).unwrap_or("")
        );
    }
    let (da, db) = if exact {
        let (sa, sb) = (exported(a), exported(b));
        if sa == sb {
            return;
        }
        (format!("{sa:#?}"), format!("{sb:#?}"))
    } else {
        (exported_flat(a), exported_flat(b))
    };
    if da != db {
        let line = da
            .lines()
            .zip(db.lines())
            .position(|(x, y)| x != y)
            .unwrap_or(0);
        let context = |d: &str| {
            d.lines()
                .skip(line.saturating_sub(12))
                .take(16)
                .collect::<Vec<_>>()
                .join("\n")
        };
        panic!(
            "{ctx}: re-exported states differ at line {line}\n--- source\n{}\n--- import\n{}",
            context(&da),
            context(&db)
        );
    }
}

/// Export `a`, import it, check both alike, then feed `rest` to both and
/// check again, replies included.
fn round_trip(a: &mut PaneTerm, compressed: CompressedPages, rest: &[u8], ctx: &str) -> PaneTerm {
    let _ = a.take_answers();
    let state = a.export_state_with(compressed);
    let mut b = PaneTerm::import_state(&state).unwrap_or_else(|e| panic!("{ctx}: import: {e}"));
    assert_same(a, &b, ctx);
    a.feed(rest);
    b.feed(rest);
    assert_eq!(
        format!("{:?}", a.take_answers()),
        format!("{:?}", b.take_answers()),
        "{ctx}: replies after import"
    );
    assert_alike(a, &b, &format!("{ctx} (after more input)"), false);
    b
}

fn pane(cols: u16, rows: u16, limit: usize, bytes: &[u8]) -> PaneTerm {
    let mut t = PaneTerm::new(cols, rows, limit);
    t.feed(bytes);
    t
}

/// A cursor saved before the screen shrank past it stays where it was
/// (tmux never clamps `input_save_state`'s position), so after an import
/// and a grow back DECRC puts it there, as without the import (patch
/// 0030).
#[test]
fn saved_cursor_off_a_shrunk_screen() {
    for save in [&b"\x1b7"[..], b"\x1b[s"] {
        let mut a = pane(30, 12, 100, b"\x1b[5;6H");
        a.feed(save);
        a.resize(30, 2);
        let state = a.export_state();
        let mut b = PaneTerm::import_state(&state).unwrap();
        assert_same(&a, &b, "saved off screen");
        for t in [&mut a, &mut b] {
            t.resize(30, 12);
            t.feed(b"\x1b8");
        }
        assert_eq!(b.cursor(), a.cursor());
        assert_eq!(b.cursor(), (5, 4));
    }
}

#[test]
fn plain_text_and_history() {
    let mut text = Vec::new();
    for i in 0..300 {
        text.extend_from_slice(format!("line {i} {}\r\n", "x".repeat(i % 50)).as_bytes());
    }
    let mut a = pane(40, 10, 100, &text);
    round_trip(&mut a, CompressedPages::Rows, PROBE, "plain");
}

#[test]
fn styles_wide_graphemes_links_and_flags() {
    let bytes = "a\x1b[1;3;4:3;9;53;38;5;3;48;2;1;2;3;58;5;200mb\x1b[0m\u{4e16}\u{754c}e\u{301}\
        \u{1f468}\u{200d}\u{1f469}\x1b]8;id=k;http://x\x1b\\L\x1b]8;;\x1b\\\x1b]8;;http://y\x07M\x1b]8;;\x07\
        \x1b[38;5;9mP\x1b[48;5;12m\x1b[K\r\n0123456789012345678901234567\u{4e16}tail\r\n\tA\tB\x1b[2;5H\x1b[X\x1b[3@\x1b[2P\r\n\
        \x1b]133;A\x07$ \x1b]133;B\x07cmd\x1b]133;C\x07\r\nout\r\n\x1b]133;D;2\x07\x1b(0lqk\x1b(B";
    let mut a = pane(30, 6, 50, bytes.as_bytes());
    round_trip(&mut a, CompressedPages::Rows, PROBE, "styles");
}

#[test]
fn cursor_modes_charsets_tabs_and_region() {
    let bytes = b"\x1b[?1h\x1b=\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[4h\x1b[?7l\
        \x1b[?7h\x1b[?25l\x1b[3 q\x1b[?12h\x1b[3g\x1b[1;5H\x1bH\x1b[1;13H\x1bH\x1b[3;7r\x1b[?6h\
        \x1b)0\x0e\x1b[2;4Hab\x1b7\x1b[5;20Hzz\x1b[?1s\x1b[?1l\x1b[22t\x1b]2;one\x07\x1b[22t\
        \x1b]2;two\x07\x1b]4;1;rgb:10/20/30\x07\x1b]10;rgb:ff/00/00\x07\x1b[>4;1m\x1b[?2026h\
        \x1b]9;4;2;37\x07\x1b]9;4;3\x07\
        0123456789012345678901234567890123456789";
    let mut a = pane(40, 8, 20, bytes);
    // LNM is the server's to set (an empty pane's MODE_CRLF), not a
    // program's.
    a.set_crlf(true);
    round_trip(&mut a, CompressedPages::Rows, PROBE, "modes");
}

#[test]
fn pending_wrap_and_saved_cursor() {
    let mut a = pane(10, 3, 10, b"abcdefghij\x1b7\r\nxyz\x1b[1;10Hq");
    round_trip(&mut a, CompressedPages::Rows, b"!\x1b8!", "pending wrap");
}

#[test]
fn alternate_screen_keeps_primary_history() {
    let mut bytes = Vec::new();
    for i in 0..60 {
        bytes.extend_from_slice(format!("p{i}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b]133;A\x07$ \x1b[?1049h\x1b[2J\x1b[Halt screen\x1b[5;5Hthere");
    let mut a = pane(20, 6, 30, &bytes);
    assert!(a.alternate_on());
    round_trip(
        &mut a,
        CompressedPages::Rows,
        b"more\x1b[?1049lback\r\n",
        "alternate",
    );
}

#[test]
fn half_parsed_sequences_carry_on() {
    for tail in [
        &b"\x1b["[..],
        b"\x1b[38;5",
        b"\x1b]2;half title",
        b"\x1bP1$q",
        b"\x1bPtmux;\x1b\x1b]2;x",
        b"\x1bkname so far",
        b"\xe4\xb8",
        b"\x1b]8;;http://half",
        b"\x1b]133;D;",
    ] {
        let mut a = pane(20, 4, 10, b"start ");
        a.feed(tail);
        round_trip(
            &mut a,
            CompressedPages::Rows,
            b"m\x07\x1b\\ end",
            &format!("tail {tail:?}"),
        );
    }
}

/// REP repeats only a character printed right before it (tmux's
/// `INPUT_LAST`, input.c `INPUT_CSI_REP`): whether one was is carried,
/// also with the REP half-parsed at the export.
#[test]
fn rep_after_a_print_carries_on() {
    for (head, rest) in [
        (&b"ab"[..], &b"\x1b[3b"[..]),
        (b"ab\x1b[3", b"b"),
        (b"\xe4\xb8\x96", b"\x1b[2b"),
        (b"ab\r", b"\x1b[3b"),
        (b"ab\x1b[1m", b"\x1b[3b"),
    ] {
        let mut a = pane(20, 4, 10, head);
        round_trip(&mut a, CompressedPages::Rows, rest, &format!("{head:?}"));
    }
}

/// A screen narrowed without a reflow (the alternate screen) keeps its
/// rows' cells used past the new width, as tmux's grid lines keep
/// `cellused` when only `grid->sx` changes (screen.c
/// `screen_resize_cursor` with `reflow` 0).
#[test]
fn narrowed_alternate_screen_round_trips() {
    let mut bytes = b"\x1b[?1049h".to_vec();
    bytes.extend_from_slice("x".repeat(58).as_bytes());
    bytes.extend_from_slice(b"\r\nshort");
    let mut a = pane(58, 6, 0, &bytes);
    a.resize(48, 6);
    round_trip(&mut a, CompressedPages::Rows, b"\x1b[?1049l", "narrowed");
}

#[test]
fn reflow_resize_and_limits() {
    let mut text = Vec::new();
    for i in 0..500 {
        text.extend_from_slice(format!("{i:04} {}\r\n", "wrapme ".repeat(i % 7)).as_bytes());
    }
    let mut a = pane(50, 10, 200, &text);
    a.resize(23, 7);
    a.set_history_limit(150);
    a.set_history_budget(1 << 20);
    round_trip(&mut a, CompressedPages::Rows, b"after resize\r\n", "resize");
}

/// Compressed history: as rows (any build) and as blobs (decision 23,
/// same build), the import is the same terminal and its pages stay
/// compressed.
#[test]
fn compressed_history_as_rows_and_blobs() {
    let mut text = Vec::new();
    for i in 0..5000 {
        text.extend_from_slice(
            format!(
                "\x1b[3{}m{i:05}\x1b[m history {}\r\n",
                i % 8,
                "z".repeat(i % 60)
            )
            .as_bytes(),
        );
    }
    let mut a = pane(80, 24, 4000, &text);
    while a.compress_step() == Compression::Pending {}
    // Ghostty reclaims compressed pages' memory on Linux and macOS.
    let reclaims = cfg!(any(target_os = "linux", target_os = "macos"));
    assert!(a.compressed_pages() > 0 || !reclaims, "nothing compressed");
    if a.compressed_pages() == 0 {
        return;
    }
    let state = a.export_state_with(CompressedPages::Blob);
    let blobs = state
        .primary
        .pages
        .iter()
        .filter(|p| matches!(p, PageState::Blob(_)))
        .count();
    assert_eq!(u64::try_from(blobs).unwrap(), a.compressed_pages());
    let b = PaneTerm::import_state(&state).expect("import blobs");
    assert_eq!(
        b.compressed_pages(),
        a.compressed_pages(),
        "blobs stay compressed"
    );
    assert_same(&a, &b, "blobs");
    let mut a2 = pane(80, 24, 4000, &text);
    while a2.compress_step() == Compression::Pending {}
    round_trip(&mut a2, CompressedPages::Blob, PROBE, "blobs, more input");
    round_trip(&mut a2, CompressedPages::Rows, PROBE, "compressed as rows");
}

/// `compress_all` compresses what a whole incremental pass would, in one
/// call, and `compress_all_of` so for many terminals over threads (a live
/// upgrade's history then travels as blobs, decision 23); nothing a
/// capture shows changes.
#[test]
fn compress_all_does_a_pass_at_once() {
    let mut text = Vec::new();
    for i in 0..5000 {
        text.extend_from_slice(
            format!(
                "\x1b[3{}m{i:05}\x1b[m all {}\r\n",
                i % 8,
                "y".repeat(i % 50)
            )
            .as_bytes(),
        );
    }
    let reclaims = cfg!(any(target_os = "linux", target_os = "macos"));
    let mut stepped = pane(80, 24, 4000, &text);
    while stepped.compress_step() == Compression::Pending {}
    let mut a = pane(80, 24, 4000, &text);
    let before = observe(&a, true);
    assert_eq!(a.compressed_pages(), 0);
    let r = a.compress_all();
    if !reclaims {
        return;
    }
    assert_eq!(r, Compression::Complete);
    // (Counted before any capture: reading history restores its pages.)
    let pass = stepped.compressed_pages();
    assert!(pass > 0, "nothing compressed");
    assert_eq!(a.compressed_pages(), pass);
    // Many terminals at once, over threads: each as a pass leaves it.
    let mut many: Vec<PaneTerm> = (0..5).map(|_| pane(80, 24, 4000, &text)).collect();
    let shown: Vec<String> = many.iter().map(|t| observe(t, true)).collect();
    vtmux_vt::compress_all_of(many.iter_mut());
    assert!(many.iter().all(|t| t.compressed_pages() == pass));
    assert_eq!(observe(&a, true), before, "compressing changed what shows");
    for (t, shown) in many.iter().zip(shown) {
        assert_eq!(
            observe(t, true),
            shown,
            "compress_all_of changed what shows"
        );
    }
}

#[test]
fn foreign_blobs_are_refused() {
    let mut text = Vec::new();
    for i in 0..3000 {
        text.extend_from_slice(format!("{i} {}\r\n", "q".repeat(70)).as_bytes());
    }
    let mut a = pane(80, 24, 3000, &text);
    while a.compress_step() == Compression::Pending {}
    let mut state = a.export_state_with(CompressedPages::Blob);
    let mut changed = false;
    for p in &mut state.primary.pages {
        if let PageState::Blob(b) = p {
            b.stack = "another build".into();
            changed = true;
        }
    }
    if changed {
        assert_eq!(
            PaneTerm::import_state(&state).err(),
            Some(StateError::ForeignBlob)
        );
    }
}

#[test]
fn fuzzed_escape_sequences_round_trip() {
    for seed in fuzz_seeds() {
        let mut f = Fuzz(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let cols = 8 + u16::try_from(f.below(60)).unwrap();
        let rows = 2 + u16::try_from(f.below(20)).unwrap();
        let limit = [0, 5, 50, 300][f.below(4)];
        let n = 30 + f.below(300);
        let head = f.input(n);
        let n = 20 + f.below(100);
        let mut tail = f.input(n);
        tail.extend_from_slice(PROBE);
        // Cut the head anywhere, mid-sequence too.
        let cut = f.below(head.len() + 1);
        let mut a = PaneTerm::new(cols, rows, limit);
        a.feed(&head[..cut]);
        // A resize writes its own sequences to Ghostty, which would break
        // a half-parsed one (a resize mid-sequence desyncs Ghostty's parser
        // from tmux's in `PaneTerm` itself, import or not).
        if f.below(4) == 0 && a.pending_input().is_empty() {
            a.resize(
                8 + u16::try_from(f.below(60)).unwrap(),
                2 + u16::try_from(f.below(20)).unwrap(),
            );
        }
        let mut rest = head[cut..].to_vec();
        rest.extend_from_slice(&tail);
        round_trip(
            &mut a,
            CompressedPages::Rows,
            &rest,
            &format!("seed {seed} {cols}x{rows} limit {limit} cut {cut}"),
        );
    }
}

/// What the oracle scenarios' pane programs wrote to their terminals
/// (`tests/state/oracle_programs.txt`, recorded by `regen_corpus.py`),
/// each cut at several points: the import at each cut is the same
/// terminal and stays so through the rest of the program's output. At
/// the end, the pane's history compressed, as blobs too.
#[test]
fn oracle_pane_programs_round_trip() {
    let text = include_str!("state/oracle_programs.txt");
    let mut lines = text.lines().filter(|l| !l.starts_with('#'));
    let mut programs = 0;
    while let (Some(name), Some(size), Some(bytes)) = (lines.next(), lines.next(), lines.next()) {
        let name = name.strip_prefix("name=").expect("name line");
        let (cols, rows) = size
            .strip_prefix("size=")
            .and_then(|s| s.split_once('x'))
            .expect("size line");
        let (cols, rows): (u16, u16) = (cols.parse().unwrap(), rows.parse().unwrap());
        let bytes = unescape(bytes.strip_prefix("bytes=").expect("bytes line"));
        programs += 1;
        for k in 1..=4 {
            let cut = bytes.len() * k / 4;
            let mut a = PaneTerm::new(cols, rows, 2000);
            a.feed(&bytes[..cut]);
            let mut rest = bytes[cut..].to_vec();
            rest.extend_from_slice(PROBE);
            round_trip(
                &mut a,
                CompressedPages::Rows,
                &rest,
                &format!("{name} cut {cut}/{}", bytes.len()),
            );
        }
        let mut a = PaneTerm::new(cols, rows, 2000);
        a.feed(&bytes);
        while a.compress_step() == Compression::Pending {}
        round_trip(
            &mut a,
            CompressedPages::Blob,
            PROBE,
            &format!("{name} compressed"),
        );
    }
    assert!(programs > 200, "corpus has {programs} programs");
}
