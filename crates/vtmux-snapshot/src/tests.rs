//! Whole-stream tests: round trips, versions, unknown tags, manifests and
//! chunks, consistency checks, the golden files.

use super::*;
use crate::model::tests::{
    sample_client, sample_layout, sample_pane, sample_server, sample_session, sample_window,
};
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
    let mut bytes = header();
    bytes.extend_from_slice(&body);
    let got = decode(&bytes).unwrap();
    let mut want = s.clone();
    want.sessions.push(s.sessions[0].clone());
    assert_eq!(got, want);
}

#[test]
fn damaged_streams_fail_without_panicking() {
    let bytes = encode(&sample());
    for cut in (12..bytes.len()).step_by(17) {
        // A cut may land on a field boundary and decode; it must not panic.
        let _ = decode(&bytes[..cut]).map(|s| s.check());
    }
    for at in (12..bytes.len()).step_by(13) {
        for bit in [0x01, 0x80] {
            let mut b = bytes.clone();
            b[at] ^= bit;
            let _ = decode(&b).map(|s| s.check());
        }
    }
    // A length running past the end.
    let mut bytes = header();
    put_varint(&mut bytes, 2);
    put_varint(&mut bytes, 1_000_000);
    assert_eq!(decode(&bytes), Err(Error::Truncated));
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
}

fn golden(version: u32) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("v{version}.snap"))
}

/// Golden v1: written from [`sample`] when v1 was current. When the format
/// moves on, freeze today's `sample` as `sample_v1` (as it decodes under
/// the new version) and keep this test on it; never edit `v1.snap`.
#[test]
fn golden_v1_decodes() {
    let bytes = std::fs::read(golden(1)).expect("tests/golden/v1.snap");
    assert_eq!(version(&bytes).unwrap(), 1);
    let s = decode(&bytes).unwrap();
    s.check().unwrap();
    assert_eq!(s, sample());
}

/// The current version's encoding is pinned by its golden file, so a change
/// to the bytes can't go unnoticed. `VTMUX_SNAPSHOT_BLESS=1` writes the
/// file for a new version; it never overwrites one.
#[test]
fn the_current_version_encodes_to_its_golden_file() {
    let path = golden(FORMAT_VERSION);
    let bytes = encode(&sample());
    if !path.exists() && std::env::var_os("VTMUX_SNAPSHOT_BLESS").is_some() {
        std::fs::write(&path, &bytes).unwrap();
    }
    let want = std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "{} missing: run with VTMUX_SNAPSHOT_BLESS=1",
            path.display()
        )
    });
    assert!(
        want == bytes,
        "the encoding of format {FORMAT_VERSION} changed: bump FORMAT_VERSION and add a \
         golden file (or, before v{FORMAT_VERSION} is released, delete it and bless again)"
    );
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
