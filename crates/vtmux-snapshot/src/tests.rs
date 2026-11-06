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

#[test]
fn pages_are_found_everywhere_they_live() {
    let s = sample();
    // Pane 1's primary (2) and alternate-entry (1) history, its copy mode
    // (1); the popup's terminal (2 + 1).
    assert_eq!(s.pages().len(), 7);
    let mut n = 0;
    sample().pages_mut(&mut |_| n += 1);
    assert_eq!(n, 7);
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
