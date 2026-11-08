//! A pane's terminal through the snapshot format and back (live upgrade
//! spec, decisions 5, 11 and 23): `term_to_snapshot`, a whole snapshot
//! stream encoded and decoded (the upgrade stream inline, a checkpoint as
//! manifest plus chunks), `term_from_snapshot`. The pane that comes back
//! is the same terminal: every capture (`capture-pane -e -S -` and the
//! other flags), cursor, mode, row flag, history time and the re-exported
//! state outside the page structure are equal, and the same further input
//! keeps them equal, replies included. Corpus: the spike's escape-sequence
//! fuzzer and the oracle scenarios' pane programs.

#[path = "../../vtmux-vt/tests/state/harness.rs"]
mod harness;

use std::collections::HashMap;

use harness::{Fuzz, PROBE, exported, exported_flat, fuzz_seeds, observe, oracle_programs};
use vtmux_server::snapshot_term::{
    Form, TermError, ghostty_pages_requirement, has_ghostty_pages, reader_features,
    term_from_snapshot, term_from_snapshot_with, term_to_snapshot,
};
use vtmux_snapshot as snap;
use vtmux_vt::{Compression, ImportCheck, PATCH_STACK, PaneTerm, StateError};

/// `t` alone in a consistent snapshot, encoded as `form` travels (an
/// upgrade stream for `SamePin`, a checkpoint manifest and its chunks for
/// `Rows`), decoded, checked, and its terminal taken out again.
fn through_stream(t: snap::TermState, form: Form) -> snap::TermState {
    let mut s = snap::Snapshot {
        windows: vec![snap::Window {
            id: 1,
            layout: snap::Layout {
                root: snap::LayoutCell::Pane(snap::Leaf {
                    pane: 1,
                    ..snap::Leaf::default()
                }),
                ..snap::Layout::default()
            },
            panes: vec![1],
            active: Some(1),
            ..snap::Window::default()
        }],
        panes: vec![snap::Pane {
            id: 1,
            window: 1,
            term: Some(t),
            ..snap::Pane::default()
        }],
        ..snap::Snapshot::default()
    };
    PATCH_STACK.clone_into(&mut s.meta.ghostty_patches);
    let term = s.panes[0].term.as_ref().unwrap();
    if has_ghostty_pages(term) {
        s.meta.requires.push(ghostty_pages_requirement());
    }
    let decoded = match form {
        Form::SamePin => {
            let bytes = snap::encode(&s);
            snap::decode_for(&bytes, &reader_features(), |_| None).expect("decode")
        }
        Form::Rows => {
            assert!(!has_ghostty_pages(term), "a checkpoint holds rows only");
            let chunks: HashMap<snap::Hash, Vec<u8>> = s
                .pages()
                .into_iter()
                .map(|p| (p.hash, p.data.clone()))
                .collect();
            let manifest = snap::encode_manifest(&s);
            snap::decode_with(&manifest, |h| chunks.get(h).cloned()).expect("decode")
        }
    };
    decoded.check().expect("check");
    assert_eq!(decoded, s, "the stream gives back what was encoded");
    decoded.panes.into_iter().next().unwrap().term.unwrap()
}

/// Compare `a` and `b`: everything observable, and the re-exported state
/// with the page structure flattened (the import rebuilds page tables);
/// `times`: history row times too, equal right after an import.
fn assert_alike(a: &PaneTerm, b: &PaneTerm, ctx: &str, times: bool) {
    let pairs = [
        (observe(a, times), observe(b, times), "observations"),
        (exported_flat(a), exported_flat(b), "re-exported states"),
        (
            format!("{:?}", exported(a).pane.times),
            format!("{:?}", exported(b).pane.times),
            "history times",
        ),
    ];
    for (x, y, what) in pairs.iter().take(if times { 3 } else { 2 }) {
        if x != y {
            let line = x
                .lines()
                .zip(y.lines())
                .position(|(p, q)| p != q)
                .unwrap_or(0);
            let context = |d: &str| {
                d.lines()
                    .skip(line.saturating_sub(8))
                    .take(12)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            panic!(
                "{ctx}: {what} differ at line {line}\n--- source\n{}\n--- import\n{}",
                context(x),
                context(y)
            );
        }
    }
}

/// Export `a` through the snapshot, import it, compare, then feed `rest`
/// to both and compare again, replies included.
fn round_trip(a: &mut PaneTerm, form: Form, rest: &[u8], ctx: &str) -> PaneTerm {
    let _ = a.take_answers();
    let t = through_stream(term_to_snapshot(a, form), form);
    let mut b = term_from_snapshot(&t, PATCH_STACK).unwrap_or_else(|e| panic!("{ctx}: {e}"));
    assert_alike(a, &b, ctx, true);
    if form == Form::SamePin {
        assert_eq!(
            b.compressed_pages(),
            a.compressed_pages(),
            "{ctx}: pages in Ghostty's form stay compressed"
        );
    }
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

fn compressed(mut t: PaneTerm) -> PaneTerm {
    while t.compress_step() == Compression::Pending {}
    t
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
        let cut = f.below(head.len() + 1);
        let mut a = PaneTerm::new(cols, rows, limit);
        a.feed(&head[..cut]);
        // See the spike's fuzz test: a resize mid-sequence desyncs the
        // parsers in `PaneTerm` itself.
        if f.below(4) == 0 && a.pending_input().is_empty() {
            a.resize(
                8 + u16::try_from(f.below(60)).unwrap(),
                2 + u16::try_from(f.below(20)).unwrap(),
            );
        }
        let mut rest = head[cut..].to_vec();
        rest.extend_from_slice(&tail);
        let form = if seed % 2 == 0 {
            Form::Rows
        } else {
            Form::SamePin
        };
        round_trip(
            &mut a,
            form,
            &rest,
            &format!("seed {seed} {cols}x{rows} limit {limit} cut {cut} {form:?}"),
        );
    }
}

/// The oracle pane programs, each cut at a quarter, half, three quarters
/// and the end, through a checkpoint; at the end with its history
/// compressed too, through an upgrade stream (pages in Ghostty's form) and
/// through a checkpoint (compressed pages decoded into rows).
#[test]
fn oracle_pane_programs_round_trip() {
    let programs = oracle_programs();
    assert!(programs.len() > 200, "corpus has {}", programs.len());
    for (name, cols, rows, bytes) in &programs {
        for k in 1..=4 {
            let cut = bytes.len() * k / 4;
            let mut a = pane(*cols, *rows, 2000, &bytes[..cut]);
            let mut rest = bytes[cut..].to_vec();
            rest.extend_from_slice(PROBE);
            round_trip(&mut a, Form::Rows, &rest, &format!("{name} cut {cut}"));
        }
        for form in [Form::SamePin, Form::Rows] {
            let mut a = compressed(pane(*cols, *rows, 2000, bytes));
            round_trip(&mut a, form, PROBE, &format!("{name} compressed {form:?}"));
        }
    }
}

/// History spread over many Ghostty pages, some compressed: each form
/// keeps the page layout, and a checkpoint is rows only.
#[test]
fn long_history_in_both_forms() {
    let mut text = Vec::new();
    for i in 0..6000 {
        text.extend_from_slice(
            format!(
                "\x1b[3{}m{i:05}\x1b[48;5;{}m history\x1b[m {}\u{4e16}\r\n",
                i % 8,
                i % 256,
                "z".repeat(i % 70)
            )
            .as_bytes(),
        );
    }
    let mut a = compressed(pane(90, 30, 5000, &text));
    a.feed(b"\x1b]133;A\x07$ \x1b]8;;http://x\x07link\x1b]8;;\x07\r\nlive rows\r\n");
    let rows = term_to_snapshot(&a, Form::Rows);
    assert!(!has_ghostty_pages(&rows));
    assert!(
        rows.primary.history.len() > 10,
        "one history page a Ghostty page"
    );
    assert_eq!(rows.primary.rows.len(), 30);
    let same = term_to_snapshot(&a, Form::SamePin);
    // Only compressed pages go in Ghostty's form: the visible rows and the
    // pages still resident are rows either way.
    assert_eq!(has_ghostty_pages(&same), a.compressed_pages() > 0);
    assert_eq!(same.primary.rows, rows.primary.rows);
    // The import follows the layout: Ghostty's pages come back as they
    // were split, with their capacities.
    for form in [Form::SamePin, Form::Rows] {
        let b = term_from_snapshot(
            &through_stream(term_to_snapshot(&a, form), form),
            PATCH_STACK,
        )
        .expect("import");
        assert_eq!(page_shapes(&b), page_shapes(&a), "{form:?}");
    }
    round_trip(&mut a, Form::SamePin, PROBE, "long, same pin");
    round_trip(&mut a, Form::Rows, PROBE, "long, rows");
    // Under an alternate screen of another size: the compressed history
    // goes into the copy at the terminal's size as it is.
    let mut a = compressed(pane(90, 30, 5000, &text));
    assert!(a.compressed_pages() > 0);
    a.feed(b"\x1b[?1049hvim");
    a.resize(70, 20);
    let rows = term_to_snapshot(&a, Form::Rows);
    let sized = rows.sized_primary.as_ref().expect("left at 90x30");
    assert_eq!(rows.primary.rows.len(), 20);
    assert_ne!(rows.primary.history.len(), 0);
    assert_eq!(sized.screen.as_ref().map(|s| s.rows.len()), Some(30));
    round_trip(
        &mut a,
        Form::SamePin,
        b"\x1b[?1049l",
        "long alternate, same pin",
    );
    let mut a = compressed(pane(90, 30, 5000, &text));
    a.feed(b"\x1b[?1049hvim");
    a.resize(70, 20);
    round_trip(&mut a, Form::Rows, b"\x1b[?1049l", "long alternate, rows");
}

/// Each page of the primary screen: rows, width and capacity, or a blob.
fn page_shapes(t: &PaneTerm) -> Vec<String> {
    t.export_state_with(vtmux_vt::CompressedPages::Rows)
        .primary
        .pages
        .iter()
        .map(|p| match p {
            vtmux_vt::PageState::Rows(r) => format!("{} {} {:?}", r.rows.len(), r.cols, r.capacity),
            vtmux_vt::PageState::Blob(_) => "blob".to_owned(),
        })
        .collect()
}

/// The alternate screen up over primary history, the frozen copy and
/// marks with it, a sync hold, links with bytes that aren't UTF-8.
/// What main added to a pane's state after the spike: REP's last print
/// (also with the REP half-parsed) and a narrowed alternate screen's
/// cells-used counts past its width.
#[test]
fn rep_and_a_narrowed_alternate_screen() {
    for form in [Form::Rows, Form::SamePin] {
        for (head, rest) in [(&b"ab"[..], &b"\x1b[3b"[..]), (b"ab\x1b[3", b"b")] {
            let mut a = pane(20, 4, 10, head);
            round_trip(&mut a, form, rest, &format!("{form:?} {head:?}"));
        }
        let mut bytes = b"\x1b[?1049h".to_vec();
        bytes.extend_from_slice("x".repeat(58).as_bytes());
        let mut a = pane(58, 6, 0, &bytes);
        a.resize(48, 6);
        round_trip(&mut a, form, b"\x1b[?1049l", &format!("{form:?} narrowed"));
    }
}

#[test]
fn alternate_screen_sync_hold_and_raw_links() {
    let mut bytes = Vec::new();
    for i in 0..80 {
        bytes.extend_from_slice(format!("p{i} \x1b]8;id=").as_bytes());
        bytes.extend_from_slice(b"\xfe;http://\xff");
        bytes.extend_from_slice(format!("{i}\x07L\x1b]8;;\x07\r\n").as_bytes());
    }
    bytes.extend_from_slice(
        b"\x1b]133;A\x07$ \x1b[?1049h\x1b[2J\x1b[Halt\x1b[5;5Hthere\x1b[?2026hheld",
    );
    let mut a = pane(24, 7, 40, &bytes);
    assert!(a.alternate_on() && a.sync_held().is_some());
    for form in [Form::Rows, Form::SamePin] {
        let mut a = pane(24, 7, 40, &bytes);
        round_trip(
            &mut a,
            form,
            b"x\x1b[?2026l\x1b[?1049lback\r\n",
            "alternate",
        );
    }
    round_trip(&mut a, Form::Rows, PROBE, "alternate, probe");
}

/// The full check's import ([`ImportCheck::Full`]) reads the same
/// terminal as the restore's (and the dry run's, both `Lazy`), and both
/// refuse a corrupt page.
#[test]
fn the_full_check_imports_what_the_lazy_one_does() {
    let mut text = Vec::new();
    for i in 0..3000 {
        text.extend_from_slice(
            format!("\x1b[3{}m{i} {}\r\n", i % 8, "d".repeat(i % 70)).as_bytes(),
        );
    }
    let a = compressed(pane(80, 24, 3000, &text));
    for form in [Form::SamePin, Form::Rows] {
        let t = through_stream(term_to_snapshot(&a, form), form);
        let full = term_from_snapshot_with(&t, PATCH_STACK, ImportCheck::Full).expect("full");
        assert_alike(&a, &full, &format!("full {form:?}"), true);
        let mut bad = t.clone();
        let Some(page) = bad.primary.history.iter_mut().find(|p| p.is_ghostty()) else {
            continue;
        };
        let at = page.ghostty.len() - 3;
        page.ghostty[at] ^= 0x40;
        for check in [ImportCheck::Lazy, ImportCheck::Full] {
            assert!(
                term_from_snapshot_with(&bad, PATCH_STACK, check).is_err(),
                "{check:?}: a corrupt page imported"
            );
        }
    }
}

/// A stream from another Ghostty patch stack can't carry its pages in
/// Ghostty's form into this one: the import refuses rather than lose them.
#[test]
fn foreign_ghostty_pages_are_refused() {
    let mut text = Vec::new();
    for i in 0..3000 {
        text.extend_from_slice(format!("{i} {}\r\n", "q".repeat(70)).as_bytes());
    }
    let a = compressed(pane(80, 24, 3000, &text));
    let t = term_to_snapshot(&a, Form::SamePin);
    if has_ghostty_pages(&t) {
        assert_eq!(
            term_from_snapshot(&t, "another build").err(),
            Some(TermError::State(StateError::ForeignBlob))
        );
    }
}

/// A snapshot whose page layout doesn't add up is read without it, and
/// rows wider than the terminal are refused, not imported.
#[test]
fn a_bad_layout_is_ignored_and_bad_rows_refused() {
    let mut text = Vec::new();
    for i in 0..2000 {
        text.extend_from_slice(format!("row {i}\r\n").as_bytes());
    }
    let a = pane(40, 10, 1500, &text);
    let mut t = term_to_snapshot(&a, Form::Rows);
    t.primary.pages[0].rows += 1;
    let b = term_from_snapshot(&t, PATCH_STACK).expect("layout ignored");
    assert_eq!(observe(&a, false), observe(&b, false));
    let mut wide = term_to_snapshot(&a, Form::Rows);
    wide.primary.pages.clear();
    wide.primary.rows[0].cells = vec![snap::Cell::default(); 41];
    assert!(matches!(
        term_from_snapshot(&wide, PATCH_STACK),
        Err(TermError::Snapshot(_))
    ));
    let mut short = term_to_snapshot(&a, Form::Rows);
    short.primary.rows.pop();
    assert!(matches!(
        term_from_snapshot(&short, PATCH_STACK),
        Err(TermError::Snapshot(_))
    ));
    let mut tall = term_to_snapshot(&a, Form::Rows);
    tall.primary.pages[0].rows = 70_000;
    assert!(
        term_from_snapshot(&tall, PATCH_STACK).is_ok(),
        "layout ignored"
    );
}

/// Sixty lines of 75 characters and a prompt, at 80x20.
fn sixty_lines() -> PaneTerm {
    let mut text = Vec::new();
    for i in 10..70 {
        text.extend_from_slice(format!("{i}{}\r\n", "x".repeat(73)).as_bytes());
    }
    text.extend_from_slice(b"$ ");
    pane(80, 20, 2000, &text)
}

/// A snapshot from a build before tmux's `hscrolled` was kept (no
/// `HistoryState::hscrolled`): after a width change then a taller screen
/// it left the times' start at the rows scrolled before the pull, past the
/// newest row, which this build's check refuses. The import starts them
/// where they end instead, so an upgrade from that build goes through.
#[test]
fn an_older_writers_times_start_where_they_end() {
    let mut a = sixty_lines();
    a.resize(40, 20);
    let before = term_to_snapshot(&a, Form::Rows).history.scrolled;
    a.resize(40, 30);
    let mut t = through_stream(term_to_snapshot(&a, Form::Rows), Form::Rows);
    assert!(
        t.history.scrolled < before,
        "a taller screen pulled rows back"
    );
    assert!(t.history.times.is_empty(), "a reflow drops the times");
    t.history.times_base = before;
    let refused = term_from_snapshot(&t, PATCH_STACK).map(|_| ()).unwrap_err();
    assert!(refused.to_string().contains("history ids"), "{refused}");
    t.history.hscrolled = None;
    let mut b = term_from_snapshot(&t, PATCH_STACK).expect("an older writer's snapshot");
    a.feed(b"\r\nmore\r\n");
    b.feed(b"\r\nmore\r\n");
    for y in [0, a.history_size() - 1] {
        assert_eq!(b.history_id(y), a.history_id(y), "history row {y}");
    }
}

/// The primary screen the alternate one left at its size, and the cursor
/// `?1049h` saved, cross a snapshot: the end of the alternate screen
/// resizes the primary as tmux 3.8 does (oracle `alt_exit_hscrolled`,
/// shrink; `alt_screen_exit`, `s47`). A row-form snapshot, which a reader
/// from before the field may read, also has the screen at the terminal's
/// size where that reader reads it; one in Ghostty's form goes only to a
/// binary of this patch stack, which has the field.
#[test]
fn the_primary_size_crosses_a_snapshot() {
    for form in [Form::Rows, Form::SamePin] {
        for (enter, leave, back, end) in [
            // ?1049l: the saved cursor, on the last row; the shorter screen
            // pushed 30 rows, which a taller one pulls back.
            (
                &b"\x1b[?1049h\x1b[H\x1b[2Jvim"[..],
                &b"\x1b[?1049l"[..],
                (41, (2, 19)),
                (1, (2, 59)),
            ),
            // ?47l: the alternate cursor, on the top row; the rows under it
            // go, none into history.
            (
                b"\x1b[?47h\x1b[H\x1b[2Jvim",
                b"\x1b[?47l",
                (11, (3, 0)),
                (0, (3, 11)),
            ),
        ] {
            let mut a = sixty_lines();
            a.resize(80, 50);
            a.feed(enter);
            a.resize(80, 20);
            let t = through_stream(term_to_snapshot(&a, form), form);
            let sized = t.sized_primary.as_ref().expect("the primary's size");
            assert_eq!((sized.cols, sized.rows), (80, 50));
            let (at_size, older) = match form {
                Form::Rows => (sized.screen.as_ref().expect("the screen"), 20),
                Form::SamePin => {
                    assert_eq!(sized.screen, None);
                    (&t.primary, 50)
                }
            };
            assert_eq!(at_size.rows.len(), 50, "{form:?}");
            assert_eq!(t.primary.rows.len(), older, "{form:?}");
            let mut b = term_from_snapshot(&t, PATCH_STACK).expect("import");
            assert_eq!(b.alternate_saved(), a.alternate_saved());
            b.feed(leave);
            assert_eq!((b.history_size(), b.cursor()), back, "{form:?}");
            b.resize(80, 60);
            assert_eq!((b.history_size(), b.cursor()), end, "{form:?}");
        }
    }
}

/// Both cursor slots and a primary screen left at another size, DECSC's
/// slot copied as the alternate screen came up, come back exactly in
/// either form, whatever the writer put where an older reader reads.
#[test]
fn alternate_screen_slots_round_trip() {
    for form in [Form::Rows, Form::SamePin] {
        for enter in [&b"\x1b[?1049h"[..], b"\x1b[?47h"] {
            for resize in [None, Some((60, 30)), Some((90, 12))] {
                let mut a = sixty_lines();
                a.feed(b"\x1b[3;3H\x1b7\x1b[5;7H\x1b]133;A\x07$ ");
                a.feed(enter);
                a.feed(b"\x1b[4;4Halt\x1b[6;6H\x1b7");
                if let Some((c, r)) = resize {
                    a.resize(c, r);
                }
                let ctx = format!("{form:?} {enter:?} {resize:?}");
                round_trip(&mut a, form, b"\x1b[?1049l\x1b8\x1b[?47l\x1b8x", &ctx);
            }
        }
    }
}

/// What a reader from before `sized_primary` and `alternate_saved` (the
/// build before, after a downgrade or reading a checkpoint) reads of a
/// pane with `?1049` or `?47` up: the primary screen at the terminal's
/// size, reflowed as its emulator kept it, and `?1049h`'s save in the
/// primary screen's DECSC slot, where its `?1049l` restores from. Nothing
/// is required, so it reads the stream at all (live upgrade spec,
/// *Required and ignorable fields*). The older reader is played by
/// dropping the fields, which this build then reads as an older writer's.
#[test]
fn an_older_reader_takes_the_screens_it_knows() {
    for (enter, leave) in [
        (
            &b"\x1b[5;7H\x1b[?1049h\x1b[H\x1b[2Jvim"[..],
            &b"\x1b[?1049l"[..],
        ),
        (b"\x1b[5;7H\x1b[?47h\x1b[H\x1b[2Jvim", b"\x1b[?47l"),
    ] {
        for resize in [None, Some((60, 30)), Some((100, 12))] {
            let ctx = format!("{enter:?} {resize:?}");
            let mut a = sixty_lines();
            a.feed(enter);
            if let Some((c, r)) = resize {
                a.resize(c, r);
            }
            let mut t = through_stream(term_to_snapshot(&a, Form::Rows), Form::Rows);
            t.sized_primary = None;
            t.alternate_saved = None;
            t.alternate_saved_layout = None;
            let mut b = term_from_snapshot(&t, PATCH_STACK)
                .unwrap_or_else(|e| panic!("{ctx}: an older reader's import: {e}"));
            assert_eq!(b.alternate_saved(), a.alternate_saved(), "{ctx}");
            assert_eq!(
                vtmux_vt::capture(&b, vtmux_vt::CaptureOpts::default()),
                vtmux_vt::capture(&a, vtmux_vt::CaptureOpts::default()),
                "{ctx}: the alternate screen"
            );
            a.feed(leave);
            b.feed(leave);
            if resize.is_none() {
                assert_alike(&a, &b, &ctx, false);
            } else if enter.starts_with(b"\x1b[5;7H\x1b[?1049") {
                // Its primary screen is at the pane's size already.
                assert_eq!(b.cursor(), (6, 4), "{ctx}: ?1049h's cursor");
            }
        }
    }
}

/// A writer from before `alternate_saved` kept `?1049h`'s save in the
/// primary screen's DECSC slot while the alternate screen was up (upstream
/// Ghostty's `saveCursor`), and nowhere else: an upgrade from it keeps the
/// save, so `?1049l` puts the cursor back after the text printed before
/// `?1049h`. Under `?47` the slot is DECSC's. Played by dropping the
/// fields from a row-form stream, which holds the save there too.
#[test]
fn an_older_writers_1049_save_is_kept() {
    for (enter, saved) in [
        (&b"abc\x1b[?1049h\x1b[H\x1b[2Jvim"[..], Some((5, 19))),
        (b"abc\x1b7\x1b[?47h\x1b[H\x1b[2Jvim", None),
    ] {
        let mut a = sixty_lines();
        a.feed(enter);
        let mut t = through_stream(term_to_snapshot(&a, Form::Rows), Form::Rows);
        t.alternate_saved = None;
        t.alternate_saved_layout = None;
        let mut b = term_from_snapshot(&t, PATCH_STACK).expect("an older writer's snapshot");
        assert_eq!(a.alternate_saved(), saved);
        assert_eq!(b.alternate_saved(), saved);
        let leave = if saved.is_some() {
            &b"\x1b[?1049l"[..]
        } else {
            b"\x1b[?47l\x1b8"
        };
        a.feed(leave);
        b.feed(leave);
        assert_eq!(b.cursor(), a.cursor());
        assert_eq!(b.cursor(), (5, 19));
    }
}
