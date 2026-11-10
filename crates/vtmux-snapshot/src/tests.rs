//! Whole-stream tests: round trips, versions, unknown tags, manifests and
//! chunks, consistency checks, the golden files.

use super::*;
use crate::model::tests::{
    sample_client, sample_layout, sample_pane, sample_server, sample_session, sample_window,
};
use crate::row::tests::sample_row;
use crate::wire::{Value as _, put_field, put_varint};

/// A consistent snapshot with every record type in it.
pub(crate) fn sample() -> Snapshot {
    let mut panes = vec![sample_pane()];
    for id in 2..=4 {
        panes.push(Pane {
            id,
            term: None,
            modes: Vec::new(),
            ..sample_pane()
        });
    }
    panes.push(Pane {
        id: 5,
        window: 1,
        ..Pane::default()
    });
    let lone = Window {
        id: 1,
        name: "one".into(),
        layout: Layout {
            root: LayoutCell::Pane(Leaf {
                pane: 5,
                g: Geometry::default(),
            }),
            ..sample_layout()
        },
        panes: vec![5],
        active: Some(5),
        ..Window::default()
    };
    Snapshot {
        meta: Meta {
            purpose: Purpose::Upgrade,
            writer_version: "next-3.8".into(),
            binary: "/usr/local/bin/vtmux".into(),
            ghostty_patches: "0123abcd".into(),
            written_us: 1_790_000_300_000_000,
            // Nothing beyond v1 (a_stream_requiring_an_unknown_feature_is_refused).
            requires: Vec::new(),
            // Added within v1: out of the golden sample
            // (the_writer_executable_descriptor_round_trips).
            binary_fd: None,
            // Added within v1 too (the_upgrade_requester_round_trips).
            requester: None,
        },
        server: sample_server(),
        sessions: vec![sample_session()],
        windows: vec![lone, sample_window()],
        panes,
        clients: vec![sample_client()],
        descriptors: vec![
            Descriptor {
                fd: 3,
                role: Role::Listener(()),
            },
            Descriptor {
                fd: 7,
                role: Role::PanePty(1),
            },
        ],
    }
}

#[test]
fn a_snapshot_round_trips() {
    let s = sample();
    s.check().unwrap();
    let bytes = encode(&s);
    assert_eq!(&bytes[..8], MAGIC);
    assert_eq!(version(&bytes).unwrap(), FORMAT_VERSION);
    assert_eq!(decode(&bytes).unwrap(), s);
    assert_eq!(
        decode(&encode(&Snapshot::default())).unwrap(),
        Snapshot::default()
    );
}

/// `Meta::binary_fd`, added within v1, round-trips; a reader of v1 that
/// predates it skips it (unknown tags are skipped) and keeps the rest.
#[test]
fn the_writer_executable_descriptor_round_trips() {
    let mut s = sample();
    s.meta.binary_fd = Some(17);
    let back = decode(&encode(&s)).unwrap();
    assert_eq!(back.meta.binary_fd, Some(17));
    assert_eq!(back.meta.binary, s.meta.binary);
}

/// `Meta::requester`, added within v1, round-trips in both forms; a
/// stream without it (the golden one) decodes with none.
#[test]
fn the_upgrade_requester_round_trips() {
    for requester in [Requester::Handshake(12), Requester::Client(4)] {
        let mut s = sample();
        s.meta.requester = Some(requester.clone());
        let back = decode(&encode(&s)).unwrap();
        assert_eq!(back.meta.requester, Some(requester));
        assert_eq!(back.meta.binary, s.meta.binary);
    }
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert_eq!(golden.meta.requester, None);
}

/// A pane's auto-hide scrollbar state, added within v1 under
/// [`SCROLLBAR_FEATURE`], round-trips; the golden stream has none.
#[test]
fn an_auto_hide_scrollbar_round_trips() {
    let mut s = sample();
    s.meta.requires = vec![SCROLLBAR_FEATURE.into()];
    s.panes[0].scrollbar_visible = true;
    s.panes[0].scrollbar_hover = true;
    s.panes[0].scrollbar_hide_in_ms = Some(600);
    let back = decode(&encode(&s)).unwrap();
    assert_eq!(back, s);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(
        golden.panes.iter().all(|p| !p.scrollbar_visible
            && !p.scrollbar_hover
            && p.scrollbar_hide_in_ms.is_none())
    );
}

/// A mode 2048 grant (D31), added within v1 under
/// [`INBAND_RESIZE_FEATURE`], round-trips and this build reads it; the
/// golden stream has none.
#[test]
fn a_mode_2048_grant_round_trips_under_its_feature() {
    let mut s = sample();
    s.meta.requires = vec![INBAND_RESIZE_FEATURE.into()];
    s.panes[0].term.as_mut().unwrap().inband_resize = true;
    let back = decode(&encode(&s)).unwrap();
    assert_eq!(back, s);
    assert!(Features::default().understands(INBAND_RESIZE_FEATURE));
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(
        golden
            .panes
            .iter()
            .filter_map(|p| p.term.as_ref())
            .all(|t| !t.inband_resize)
    );
}

/// A pane's output generation and OSC 133 command state, added within v1
/// as ignorable fields (an older reader starts them afresh), round-trip;
/// the golden stream has none.
#[test]
fn a_panes_shell_commands_round_trip() {
    let mut s = sample();
    s.panes[0].output_generation = 42;
    s.panes[0].last_output_time = Some(1_790_000_099);
    s.panes[0].last_prompt_time = Some(1_790_000_100);
    s.panes[0].command_start_time = Some(1_790_000_101);
    s.panes[0].command_end_time = Some(1_790_000_104);
    s.panes[0].command_status = Some(255);
    s.panes[1].command_start_time = Some(1_790_000_200);
    s.panes[1].command_running = true;
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(golden.panes.iter().all(|p| p.output_generation == 0
        && p.last_output_time.is_none()
        && p.last_prompt_time.is_none()
        && p.command_start_time.is_none()
        && p.command_end_time.is_none()
        && !p.command_running
        && p.command_status.is_none()));
}

/// The process groups holding a pane's kitty keyboard grants (D29), added
/// within v1 as ignorable fields (an older reader skips them, and a
/// stream without them reads as no recorded owner), round-trip; the golden
/// stream has none.
#[test]
fn a_panes_kitty_owners_round_trip() {
    let mut s = sample();
    s.panes[0].kitty_owner_primary = Some(4242);
    s.panes[0].kitty_owner_alternate = Some(4343);
    s.panes[1].kitty_owner_alternate = Some(7);
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(
        golden
            .panes
            .iter()
            .all(|p| p.kitty_owner_primary.is_none() && p.kitty_owner_alternate.is_none())
    );
    assert_eq!(s.meta.requires, Vec::<String>::new());
}

/// Where a client terminal's queries stand, added within v1, round-trips,
/// required or not ([`TERM_QUERIES_FEATURE`], which the builds before it
/// was ignorable listed); the golden stream has none.
#[test]
fn a_clients_terminal_queries_round_trip() {
    let mut s = sample();
    s.clients[0].queries = Some(TermQueries {
        start_in_ms: Some(4000),
        da: true,
        da2: false,
        xtversion: true,
        sync: false,
        colours_in_ms: None,
        fg: true,
        bg: false,
        winsize: true,
    });
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    s.meta.requires = vec![TERM_QUERIES_FEATURE.into()];
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(golden.clients.iter().all(|c| c.queries.is_none()));
}

/// The title a client's terminal shows, added within v1, round-trips,
/// required or not ([`TERMINAL_TITLE_FEATURE`]); the golden stream has
/// none.
#[test]
fn a_clients_terminal_title_round_trips() {
    let mut s = sample();
    s.clients[0].terminal_title = Some("INNER".into());
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    s.meta.requires = vec![TERMINAL_TITLE_FEATURE.into()];
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(golden.clients.iter().all(|c| c.terminal_title.is_none()));
}

/// A client's panned view, added within v1, round-trips; the golden
/// stream has none.
#[test]
fn a_clients_pan_round_trips() {
    let mut s = sample();
    s.clients[0].pan = Some(ClientPan {
        window: 1,
        ox: 20,
        oy: 6,
    });
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(golden.clients.iter().all(|c| c.pan.is_none()));
}

/// A client's view, added within v1, round-trips; the golden stream has
/// none.
#[test]
fn a_clients_view_round_trips() {
    let mut s = sample();
    s.clients[0].view = Some(ClientView {
        ox: 0,
        oy: 8,
        sx: 40,
        sy: 11,
        bigger: true,
    });
    assert_eq!(decode(&encode(&s)).unwrap(), s);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(golden.clients.iter().all(|c| c.view.is_none()));
}

#[test]
fn pages_are_found_everywhere_they_live() {
    let s = sample();
    // Pane 1's primary (2) and alternate-entry (1) history, its copy mode
    // (1); the popup's terminal (2 + 1).
    assert_eq!(s.pages().len(), 7);
    let mut n = 0;
    sample().pages_mut(&mut |_| n += 1);
    assert_eq!(n, 7);
    // The primary screen at the size the alternate one left it, beside
    // its copy at the terminal's: in a manifest and back from its chunks.
    let mut s = sample();
    let t = s.panes[0].term.as_mut().unwrap();
    let mut screen = t.primary.clone();
    screen.history = vec![HistoryPage::from_rows(&[sample_row()], false)];
    t.sized_primary = Some(term::SizedPrimary {
        cols: 3,
        rows: 2,
        screen: Some(screen),
        ..term::SizedPrimary::default()
    });
    assert_eq!(s.pages().len(), 8);
    let mut n = 0;
    s.pages_mut(&mut |_| n += 1);
    assert_eq!(n, 8);
    let chunks: std::collections::BTreeMap<Hash, Vec<u8>> =
        s.pages().iter().map(|p| (p.hash, p.data.clone())).collect();
    assert_eq!(
        decode_with(&encode_manifest(&s), |h| chunks.get(h).cloned()).unwrap(),
        s
    );
}

#[test]
fn a_manifest_names_pages_and_reads_them_from_chunks() {
    let s = sample();
    let chunks: std::collections::BTreeMap<Hash, Vec<u8>> =
        s.pages().iter().map(|p| (p.hash, p.data.clone())).collect();
    let manifest = encode_manifest(&s);
    let inline = encode(&s);
    let page_bytes: usize = chunks.values().map(Vec::len).sum();
    assert!(manifest.len() + page_bytes <= inline.len() + 64);

    assert_eq!(
        decode_with(&manifest, |h| chunks.get(h).cloned()).unwrap(),
        s
    );
    // Without its chunks, or with a corrupt one, the manifest is refused.
    assert!(matches!(decode(&manifest), Err(Error::MissingChunk(_))));
    let corrupt = decode_with(&manifest, |h| {
        let mut c = chunks.get(h).cloned()?;
        c[0] ^= 1;
        Some(c)
    });
    assert!(matches!(corrupt, Err(Error::MissingChunk(_))));
}

/// A manifest read with some chunks missing: [`decode_partial`] keeps
/// it, [`Snapshot::unreadable`] names each part holding a page that does
/// not check (a pane's terminal state, a copy mode, a popup's terminal
/// state), and [`Snapshot::without`] leaves out only those, so what is
/// left checks and is the rest as it was.
#[test]
fn the_parts_holding_pages_that_do_not_check_can_be_left_out() {
    let mut s = sample();
    // A page of its own (pages are named by their content).
    let PaneMode::Copy(copy) = &mut s.panes[0].modes[0] else {
        panic!("the sample's first mode is copy mode");
    };
    copy.history = vec![HistoryPage::from_rows(
        &std::iter::repeat_n(sample_row(), 5).collect::<Vec<_>>(),
        false,
    )];
    let copy_page = copy.history[0].hash;
    let chunks: std::collections::BTreeMap<Hash, Vec<u8>> =
        s.pages().iter().map(|p| (p.hash, p.data.clone())).collect();
    let manifest = encode_manifest(&s);
    let mut partial = decode_partial(&manifest, &Features::default(), |h| {
        chunks.get(h).filter(|_| *h != copy_page).cloned()
    })
    .unwrap();
    let bad = partial.unreadable();
    assert_eq!(
        bad,
        [(
            Part::PaneMode(1, 0, ModeKind::Copy),
            Error::MissingChunk(copy_page)
        )]
    );
    assert_eq!(bad[0].0.to_string(), "%1's copy mode");
    // The same grid in view mode is named as view mode.
    let mut viewed = partial.clone();
    let PaneMode::Copy(c) = viewed.panes[0].modes[0].clone() else {
        unreachable!()
    };
    viewed.panes[0].modes[0] = PaneMode::View(c);
    let bad_view = viewed.unreadable();
    assert_eq!(bad_view[0].0, Part::PaneMode(1, 0, ModeKind::View));
    assert_eq!(bad_view[0].0.to_string(), "%1's view mode");
    partial.without(&[bad[0].0]);
    assert_eq!(partial.check(), Ok(()));
    let mut want = s.clone();
    want.panes[0].modes.remove(0);
    assert_eq!(partial, want);

    // Every chunk missing: every part with history is named, in order.
    let mut none = decode_partial(&manifest, &Features::default(), |_| None).unwrap();
    let parts: Vec<Part> = none.unreadable().into_iter().map(|(p, _)| p).collect();
    let popup = s.clients.iter().find(|c| c.popup.is_some()).unwrap().id;
    assert_eq!(
        parts,
        [
            Part::PaneTerm(1),
            Part::PaneMode(1, 0, ModeKind::Copy),
            Part::PopupTerm(popup)
        ]
    );
    none.without(&parts);
    assert_eq!(none.check(), Ok(()));
    assert_eq!(none.pages(), Vec::<&HistoryPage>::new());
    assert_eq!(none.panes[0].modes.len(), s.panes[0].modes.len() - 1);
    // A strict decode refuses both.
    assert!(decode_with(&manifest, |_| None).is_err());
}

/// A page equal to one read already is not read again (the copy of the
/// history an alternate screen shows is mostly the primary's pages); one
/// under the same name with other bytes is, and its part is named.
#[test]
fn a_page_equal_to_one_read_is_read_once() {
    let mut s = sample();
    let page = HistoryPage::from_rows(
        &std::iter::repeat_n(sample_row(), 5).collect::<Vec<_>>(),
        false,
    );
    let t = s.panes[0].term.as_mut().unwrap();
    t.primary.history = vec![page.clone()];
    t.alt_entry.get_or_insert_default().history = vec![page.clone()];
    let PaneMode::Copy(copy) = &mut s.panes[0].modes[0] else {
        panic!("the sample's first mode is copy mode");
    };
    copy.history = vec![page.clone()];
    assert_eq!(s.unreadable(), []);
    let mut other = page;
    *other.data.last_mut().unwrap() ^= 1;
    let PaneMode::Copy(copy) = &mut s.panes[0].modes[0] else {
        unreachable!()
    };
    copy.history = vec![other];
    let parts: Vec<Part> = s.unreadable().into_iter().map(|(p, _)| p).collect();
    assert_eq!(parts, [Part::PaneMode(1, 0, ModeKind::Copy)]);
}

#[test]
fn page_refs_names_a_manifests_pages_without_their_chunks() {
    let s = sample();
    let want: Vec<Hash> = s.pages().iter().map(|p| p.hash).collect();
    assert_eq!(page_refs(&encode_manifest(&s)).unwrap(), want);
    assert_eq!(page_refs(&encode(&s)).unwrap(), want);
    // What can't be read whole is an error: its caller keeps every chunk.
    let mut newer = encode_manifest(&s);
    newer[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    assert!(matches!(page_refs(&newer), Err(Error::Newer { .. })));
    let mut future = sample();
    future.meta.requires = vec!["from-the-future".into()];
    assert!(matches!(
        page_refs(&encode_manifest(&future)),
        Err(Error::Unsupported(_))
    ));
    let manifest = encode_manifest(&s);
    assert_eq!(
        page_refs(&manifest[..manifest.len() - 1]),
        Err(Error::Truncated)
    );
}

#[test]
fn newer_versions_are_refused_older_ones_too_old_and_junk_is_not_a_snapshot() {
    let mut bytes = encode(&sample());
    bytes[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    assert_eq!(
        decode(&bytes),
        Err(Error::Newer {
            found: FORMAT_VERSION + 1,
            supported: FORMAT_VERSION
        })
    );
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode(&bytes),
        Err(Error::TooOld {
            found: 0,
            oldest: MIN_VERSION
        })
    );
    assert_eq!(decode(b"#!/bin/sh\necho hi\n"), Err(Error::BadMagic));
    assert!(
        Error::Newer {
            found: FORMAT_VERSION + 1,
            supported: FORMAT_VERSION
        }
        .from_newer_binary()
    );
    assert!(Error::Unsupported("x".to_owned()).from_newer_binary());
    for e in [
        Error::BadMagic,
        Error::TooOld {
            found: 0,
            oldest: MIN_VERSION,
        },
        Error::Truncated,
        Error::Corrupt,
    ] {
        assert!(!e.from_newer_binary(), "{e}");
    }
    assert_eq!(decode(b"VTMUX"), Err(Error::Truncated));
}

#[test]
fn unknown_tags_are_skipped_at_every_level() {
    let s = sample();
    let mut body = Vec::new();
    s.put(&mut body);
    // A new top-level record, and a session carrying a field from the
    // future, each with a nested payload.
    put_field(&mut body, 1000, |o| put_field(o, 1, |o| o.push(9)));
    let mut session = Vec::new();
    s.sessions[0].put(&mut session);
    put_field(&mut session, 77, |o| o.extend_from_slice(b"future"));
    put_field(&mut body, 3, |o| o.extend_from_slice(&session));
    let got = decode(&frame(&body)).unwrap();
    let mut want = s.clone();
    want.sessions.push(s.sessions[0].clone());
    assert_eq!(got, want);
}

#[test]
fn every_cut_and_every_bit_flip_is_an_error() {
    let bytes = encode(&sample());
    // A cut at a record boundary used to decode as a smaller snapshot.
    for cut in 0..bytes.len() {
        assert!(decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    for at in 0..bytes.len() {
        for bit in 0..8 {
            let mut b = bytes.clone();
            b[at] ^= 1 << bit;
            assert!(decode(&b).is_err(), "bit {bit} of byte {at}");
        }
    }
    let mut long = bytes.clone();
    long.push(0);
    assert_eq!(
        decode(&long),
        Err(Error::Malformed("bytes after the snapshot"))
    );
    assert_eq!(decode(&bytes[..bytes.len() - 1]), Err(Error::Truncated));
    let mut flipped = bytes.clone();
    *flipped.last_mut().unwrap() ^= 1;
    assert_eq!(decode(&flipped), Err(Error::Corrupt));
}

#[test]
fn a_cut_or_damaged_manifest_is_an_error() {
    let s = sample();
    let chunks: std::collections::BTreeMap<Hash, Vec<u8>> =
        s.pages().iter().map(|p| (p.hash, p.data.clone())).collect();
    let manifest = encode_manifest(&s);
    let fetch = |h: &Hash| chunks.get(h).cloned();
    for cut in 0..manifest.len() {
        assert!(
            decode_with(&manifest[..cut], fetch).is_err(),
            "cut at {cut}"
        );
    }
    for at in (0..manifest.len()).step_by(3) {
        let mut b = manifest.clone();
        b[at] ^= 0x10;
        assert!(decode_with(&b, fetch).is_err(), "byte {at}");
    }
}

#[test]
fn damaged_bodies_with_a_matching_hash_fail_without_panicking() {
    // A writer bug, not damage in transit: the hash is right, the body
    // wrong. Decoding and checking must fail cleanly or succeed.
    let mut body = Vec::new();
    sample().put(&mut body);
    for cut in (0..body.len()).step_by(7) {
        let _ = decode(&frame(&body[..cut])).map(|s| s.check());
    }
    for at in (0..body.len()).step_by(5) {
        for bit in [0x01, 0x80] {
            let mut b = body.clone();
            b[at] ^= bit;
            let _ = decode(&frame(&b)).map(|s| s.check());
        }
    }
    // A length running past the end.
    let mut body = Vec::new();
    put_varint(&mut body, 2);
    put_varint(&mut body, 1_000_000);
    assert_eq!(decode(&frame(&body)), Err(Error::Truncated));
}

/// Rows keeping tmux's line flags, added within v1 beside the rows
/// ([`KeptRows`]), round-trip everywhere rows go (a screen, the screen
/// saved under an alternate one, a synchronized-output hold, copy mode, a
/// history page), and need no [`Meta::requires`] entry: a reader before
/// them skips the tags and reads the rest unchanged. The golden stream has
/// none.
#[test]
fn kept_line_flags_round_trip_and_require_nothing() {
    let plain = sample();
    let mut s = sample();
    let kept = KeptLineFlags {
        extended: true,
        hyperlink: false,
    };
    let linked = KeptLineFlags {
        extended: true,
        hyperlink: true,
    };
    let row = Row {
        kept,
        ..sample_row()
    };
    let term = s.panes[0].term.as_mut().unwrap();
    term.primary.rows[1].kept = kept;
    term.primary.rows[2].kept = linked;
    term.primary.history = vec![HistoryPage::from_rows(&[sample_row(), row.clone()], true)];
    term.alt_entry.as_mut().unwrap().saved[0].kept = linked;
    term.sync.as_mut().unwrap().rows[0].kept = kept;
    let PaneMode::Copy(c) = &mut s.panes[0].modes[0] else {
        panic!("copy mode first")
    };
    c.rows[0].kept = kept;
    let back = decode(&encode(&s)).unwrap();
    assert_eq!(back.meta.requires, Vec::<String>::new());
    assert_eq!(back, s);
    let t = back.panes[0].term.as_ref().unwrap();
    assert_eq!(t.primary.history[0].decode_rows().unwrap()[1], row);

    // Without the kept flags the stream is the sample's.
    let mut cleared = back;
    let term = cleared.panes[0].term.as_mut().unwrap();
    for r in &mut term.primary.rows {
        r.kept = KeptLineFlags::default();
    }
    term.primary
        .history
        .clone_from(&plain.panes[0].term.as_ref().unwrap().primary.history);
    term.alt_entry.as_mut().unwrap().saved[0].kept = KeptLineFlags::default();
    term.sync.as_mut().unwrap().rows[0].kept = KeptLineFlags::default();
    if let PaneMode::Copy(c) = &mut cleared.panes[0].modes[0] {
        c.rows[0].kept = KeptLineFlags::default();
    }
    assert_eq!(encode(&cleared), encode(&plain));

    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(golden.pages().iter().all(|p| p.kept.is_empty()));
}

#[test]
fn a_stream_requiring_an_unknown_feature_is_refused() {
    let mut s = sample();
    s.meta.requires = vec!["from-the-future".into()];
    assert_eq!(
        decode(&encode(&s)),
        Err(Error::Unsupported("from-the-future".into()))
    );

    // Decision 23's pages need the same Ghostty patch stack.
    let mut s = sample();
    s.meta.requires = vec![ghostty_pages_feature("0123abcd")];
    let bytes = encode(&s);
    assert_eq!(
        decode(&bytes),
        Err(Error::Unsupported("ghostty-pages-0123abcd".into()))
    );
    let other = Features {
        ghostty_patches: Some("feedf00d".into()),
    };
    assert!(matches!(
        decode_for(&bytes, &other, |_| None),
        Err(Error::Unsupported(_))
    ));
    let same = Features {
        ghostty_patches: Some("0123abcd".into()),
    };
    assert_eq!(decode_for(&bytes, &same, |_| None).unwrap(), s);
}

/// Pages in Ghostty's own form (decision 23) travel inline in an upgrade
/// stream: decoding fetches no chunk for them and `check` leaves their
/// bytes to Ghostty, while their rows-form neighbours are still verified.
#[test]
fn ghostty_pages_decode_without_chunks_and_pass_check() {
    let mut s = sample();
    s.meta.requires = vec![ghostty_pages_feature("0123abcd")];
    let term = s.panes[0].term.as_mut().unwrap();
    term.primary
        .history
        .insert(0, HistoryPage::from_ghostty(vec![7; 300]));
    let same = Features {
        ghostty_patches: Some("0123abcd".into()),
    };
    let back = decode_for(&encode(&s), &same, |_| panic!("no chunk wanted")).unwrap();
    assert_eq!(back, s);
    back.check().unwrap();
    let page = &back.panes[0].term.as_ref().unwrap().primary.history[0];
    assert!(page.is_ghostty() && page.compressed && page.data.is_empty());
    // Without the feature the stream is refused, never read without them.
    assert!(matches!(decode(&encode(&s)), Err(Error::Unsupported(_))));
}

/// A window whose layout nests `depth` nodes deep.
fn nested_layout(depth: usize) -> Snapshot {
    let mut cell = LayoutCell::Pane(Leaf::default());
    for _ in 0..depth {
        cell = LayoutCell::Node(Node {
            children: vec![cell],
            ..Node::default()
        });
    }
    Snapshot {
        windows: vec![Window {
            layout: Layout {
                root: cell,
                ..Layout::default()
            },
            ..Window::default()
        }],
        ..Snapshot::default()
    }
}

#[test]
fn layouts_nest_over_a_hundred_levels_but_not_without_end() {
    let deep = nested_layout(120);
    assert_eq!(decode(&encode(&deep)).unwrap(), deep);
    assert_eq!(
        decode(&encode(&nested_layout(130))),
        Err(Error::Malformed("records nest too deeply"))
    );
    // Built as bytes: a value this deep overflows the encoder's stack too.
    let mut cell = Vec::new();
    put_field(&mut cell, 1, |_| {});
    for _ in 0..20_000 {
        let mut node = Vec::new();
        put_field(&mut node, 3, |o| o.extend_from_slice(&cell));
        cell.clear();
        put_field(&mut cell, 3, |o| o.extend_from_slice(&node));
    }
    let mut layout = Vec::new();
    put_field(&mut layout, 3, |o| o.extend_from_slice(&cell));
    let mut window = Vec::new();
    put_field(&mut window, 5, |o| o.extend_from_slice(&layout));
    let mut body = Vec::new();
    put_field(&mut body, 4, |o| o.extend_from_slice(&window));
    assert_eq!(
        decode(&frame(&body)),
        Err(Error::Malformed("records nest too deeply"))
    );
}

#[test]
fn records_repeated_empty_cost_their_size_against_a_budget() {
    // 2 MB of empty client records: a default `Client` each.
    let mut body = Vec::new();
    for _ in 0..1_000_000 {
        put_field(&mut body, 6, |_| {});
    }
    assert_eq!(
        decode(&frame(&body)),
        Err(Error::Malformed(
            "snapshot decodes far larger than its bytes"
        ))
    );
    // Wide blank screen rows, a few bytes each.
    let blank = Row {
        cells: vec![Cell::default(); row::MAX_COLS],
        ..Row::default()
    };
    let mut screen = Vec::new();
    let row = blank.encode();
    for _ in 0..100_000 {
        put_field(&mut screen, 1, |o| o.extend_from_slice(&row));
    }
    let mut body = Vec::new();
    put_field(&mut body, 5, |o| {
        put_field(o, 24, |o| put_field(o, 3, |o| o.extend_from_slice(&screen)));
    });
    assert_eq!(
        decode(&frame(&body)),
        Err(Error::Malformed(
            "snapshot decodes far larger than its bytes"
        ))
    );
    // A real snapshot is far inside it.
    decode(&encode(&sample())).unwrap();
}

#[test]
fn check_catches_dangling_references() {
    let mut s = sample();
    s.sessions[0].windows[0].window = 42;
    assert!(matches!(s.check(), Err(Error::Inconsistent(_))));

    let mut s = sample();
    s.windows[1].panes.retain(|&p| p != 3);
    assert!(matches!(s.check(), Err(Error::Inconsistent(_))));

    let mut s = sample();
    s.panes[4].window = 2;
    assert!(matches!(s.check(), Err(Error::Inconsistent(_))));

    let mut s = sample();
    s.clients[0].session = Some(99);
    assert!(matches!(s.check(), Err(Error::Inconsistent(_))));

    let mut s = sample();
    s.panes.push(s.panes[0].clone());
    assert!(matches!(s.check(), Err(Error::Inconsistent(_))));

    let mut s = sample();
    s.pages_mut(&mut |p| p.raw_len += 1);
    assert!(s.check().is_err());

    // A page whose data doesn't match its name.
    let mut s = sample();
    s.pages_mut(&mut |p| {
        let mut raw = p.raw().unwrap();
        raw[0] ^= 1;
        p.data = lz4_flex::block::compress(&raw);
    });
    assert!(matches!(s.check(), Err(Error::MissingChunk(_))));
}

fn golden(version: u32) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("v{version}.snap"))
}

/// `s` with its history pages' bytes dropped: what it means, whatever
/// LZ4 encoder compressed its pages. Pages keep their names, which are
/// their content's hashes.
fn meaning(s: &Snapshot) -> Snapshot {
    let mut s = s.clone();
    s.pages_mut(&mut |p| p.data = Vec::new());
    s
}

/// Golden v1: written from [`sample`] when v1 was current. When the format
/// moves on, freeze today's `sample` as `sample_v1` (as it decodes under
/// the new version) and keep this test on it; never edit `v1.snap`. Pages
/// compare by name, so another LZ4 encoder can't fail it.
#[test]
fn golden_v1_decodes() {
    let bytes = std::fs::read(golden(1)).expect("tests/golden/v1.snap");
    assert_eq!(version(&bytes).unwrap(), 1);
    let s = decode(&bytes).unwrap();
    s.check().unwrap();
    assert_eq!(meaning(&s), meaning(&sample()));
}

/// The current version's encoding is pinned by its golden file, so a change
/// to the bytes can't go unnoticed: the golden snapshot encodes to the
/// golden bytes, and [`sample`] encodes as it does except for its pages'
/// compressed bytes (those are the LZ4 encoder's). `VTMUX_SNAPSHOT_BLESS=1`
/// writes the file for a new version; it never overwrites one.
#[test]
fn the_current_version_encodes_to_its_golden_file() {
    let path = golden(FORMAT_VERSION);
    if !path.exists() && std::env::var_os("VTMUX_SNAPSHOT_BLESS").is_some() {
        std::fs::write(&path, encode(&sample())).unwrap();
    }
    let want = std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "{} missing: run with VTMUX_SNAPSHOT_BLESS=1",
            path.display()
        )
    });
    let golden = decode(&want).unwrap();
    assert!(
        encode(&golden) == want && encode_manifest(&sample()) == encode_manifest(&golden),
        "the encoding of format {FORMAT_VERSION} changed: bump FORMAT_VERSION and add a \
         golden file (or, before v{FORMAT_VERSION} is released, delete it and bless again)"
    );
}

/// A window's cell size in pixels and the one pending came within v1:
/// they round trip, and a stream without them (the golden file's) has 0,
/// which a reader takes as tmux's default.
#[test]
fn window_cell_pixels_round_trip_and_default_to_zero() {
    let mut s = sample();
    s.windows[0].cell_pixels = (10, 20);
    s.windows[0].pending_pixels = (8, 16);
    let back = decode(&encode(&s)).unwrap();
    assert_eq!(
        (back.windows[0].cell_pixels, back.windows[0].pending_pixels),
        ((10, 20), (8, 16))
    );
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert!(
        golden
            .windows
            .iter()
            .all(|w| w.cell_pixels == (0, 0) && w.pending_pixels == (0, 0))
    );
}

/// `Server::start_binary` came within v1, so the golden sample leaves it
/// empty: it gets its own round trip, and a stream without it decodes
/// with it empty.
#[test]
fn the_start_binary_round_trips_and_defaults_to_empty() {
    let mut s = sample();
    assert_eq!(s.server.start_binary, "");
    s.server.start_binary = "/home/u/bin/tmux".into();
    assert_eq!(
        decode(&encode(&s)).unwrap().server.start_binary,
        "/home/u/bin/tmux"
    );
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert_eq!(golden.server.start_binary, "");
}

/// `Server::persist_hold` came within v1, like `start_binary`: it round
/// trips, and a stream without it (the golden file's) holds nothing back.
#[test]
fn the_persist_hold_round_trips_and_defaults_to_free() {
    let mut s = sample();
    assert_eq!(s.server.persist_hold, 0);
    s.server.persist_hold = PERSIST_HOLD_WHILE_THERE;
    assert_eq!(
        decode(&encode(&s)).unwrap().server.persist_hold,
        PERSIST_HOLD_WHILE_THERE
    );
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert_eq!(golden.server.persist_hold, 0);
}

/// `Server::link_number` came within v1: it round trips, and a stream
/// without it (the golden file's) has none.
#[test]
fn the_link_number_round_trips_and_defaults_to_none() {
    let mut s = sample();
    s.server.link_number = 77;
    assert_eq!(decode(&encode(&s)).unwrap().server.link_number, 77);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert_eq!(golden.server.link_number, 0);
}

/// A selection's `clipx` (the line numbers' gutter) came within v1: it
/// round trips, and a stream without it decodes with no gutter.
#[test]
fn the_selection_clip_round_trips_and_defaults_to_none() {
    fn clipx(s: &Snapshot) -> Vec<u64> {
        s.panes
            .iter()
            .flat_map(|p| &p.modes)
            .filter_map(|m| match m {
                PaneMode::Copy(c) => c.sel.as_ref().map(|sel| sel.clipx),
                _ => None,
            })
            .collect()
    }
    let mut s = sample();
    assert_eq!(clipx(&s), [0]);
    for p in &mut s.panes {
        for m in &mut p.modes {
            if let PaneMode::Copy(c) = m
                && let Some(sel) = c.sel.as_mut()
            {
                sel.clipx = 4;
            }
        }
    }
    assert_eq!(clipx(&decode(&encode(&s)).unwrap()), [4]);
    let golden = decode(&std::fs::read(golden(1)).unwrap()).unwrap();
    assert_eq!(clipx(&golden), [0]);
}

#[test]
fn page_names_hash_the_rows_not_the_compressed_bytes() {
    let rows = vec![sample_row(); 10];
    let page = HistoryPage::from_rows(&rows, true);
    let mut raw = Vec::new();
    for r in &rows {
        r.encode_into(&mut raw);
    }
    assert_eq!(page.hash, *blake3::hash(&raw).as_bytes());
    // The same rows compressed by another encoder (here: stored as LZ4
    // literals only) keep the name and read back the same.
    let literal = lz4_literals(&raw);
    assert_ne!(literal, page.data);
    let restored = HistoryPage {
        data: literal,
        ..page.clone()
    };
    assert!(restored.verified());
    assert_eq!(restored.decode_rows().unwrap(), rows);
}

/// `raw` as one LZ4 block of literals and no matches: valid LZ4 that no
/// compressing encoder writes.
fn lz4_literals(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let n = raw.len();
    if n < 15 {
        out.push(u8::try_from(n << 4).unwrap());
    } else {
        out.push(0xf0);
        let mut rest = n - 15;
        while rest >= 255 {
            out.push(255);
            rest -= 255;
        }
        out.push(u8::try_from(rest).unwrap());
    }
    out.extend_from_slice(raw);
    out
}

/// `cargo test --release -p vtmux-snapshot -- --ignored --nocapture`:
/// encode and decode 10 panes x 100k lines of 80 columns.
#[test]
#[ignore = "timing, run by hand in release"]
fn bench_ten_panes_of_100k_lines() {
    use crate::row::{Cell, Color, PageBuilder, Row, Style};
    use std::time::Instant;
    const LINES: usize = 100_000;
    const PER_PAGE: usize = 1000;
    // 1000 distinct rows, made before the clock starts.
    let words = "the quick brown fox jumps over the lazy dog 0123456789 ";
    let sample_rows: Vec<Row> = (0..PER_PAGE)
        .map(|line| {
            let len = 20 + (line * 7) % 60;
            let mut row = Row {
                cells: vec![Cell::default(); 80],
                cells_used: u16::try_from(len).unwrap(),
                ..Row::default()
            };
            for (i, c) in row.cells.iter_mut().enumerate().take(len) {
                c.codepoint = u32::from(words.as_bytes()[(i + line) % words.len()]);
                if i < 8 && line % 3 == 0 {
                    c.style = Style {
                        fg: Color::Palette(2),
                        ..Style::default()
                    };
                }
            }
            row
        })
        .collect();
    let t = Instant::now();
    let mut panes = Vec::new();
    for pane in 0..10u32 {
        let mut history = Vec::new();
        let mut b = PageBuilder::new();
        for line in 0..LINES {
            b.push(&sample_rows[(line + pane as usize) % PER_PAGE]);
            if b.rows() as usize == PER_PAGE {
                history.push(std::mem::take(&mut b).finish(false));
            }
        }
        panes.push(Pane {
            id: pane,
            term: Some(TermState {
                primary: term::Screen {
                    history,
                    ..term::Screen::default()
                },
                ..TermState::default()
            }),
            ..Pane::default()
        });
    }
    let built = t.elapsed();
    let t = Instant::now();
    let mut raw = Vec::new();
    for line in 0..LINES * 10 {
        sample_rows[line % PER_PAGE].encode_into(&mut raw);
        if line % PER_PAGE == PER_PAGE - 1 {
            raw.clear();
        }
    }
    let rows_only = t.elapsed();
    let s = Snapshot {
        panes,
        ..Snapshot::default()
    };
    let t = Instant::now();
    let bytes = encode(&s);
    let encoded = t.elapsed();
    let t = Instant::now();
    let back = decode(&bytes).unwrap();
    let decoded = t.elapsed();
    let t = Instant::now();
    let mut cells = 0usize;
    for p in back.pages() {
        p.each_row(|r| cells += r.cells.len()).unwrap();
    }
    let rows = t.elapsed();
    assert_eq!(cells, 10 * LINES * 80);
    println!(
        "rows->pages {built:?} (row codec alone {rows_only:?}), encode {encoded:?}, decode+verify {decoded:?}, \
         pages->rows {rows:?}, {} MB",
        bytes.len() / 1_000_000
    );
}
