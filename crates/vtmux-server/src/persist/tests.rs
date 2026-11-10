//! The checkpoint writer on a real directory: private modes, rotation,
//! chunks written once and collected, and a crash at every stage keeping
//! the previous checkpoint (spec, *Tests > Persistence*).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use vtmux_snapshot as snap;

use super::*;

/// A fresh directory per test thread, under a parent that doesn't exist
/// yet (the writer makes it).
fn temp_dir(tag: &str) -> PathBuf {
    let thread = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
    let root = std::env::temp_dir().join(format!(
        "vtmux-persist-{tag}-{}-{thread}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    root.join("state").join("vtmux").join("default")
}

/// A history page of one row reading `text`.
fn page(text: &str) -> snap::HistoryPage {
    let row = snap::Row {
        cells: text
            .chars()
            .map(|c| snap::Cell {
                codepoint: u32::from(c),
                ..snap::Cell::default()
            })
            .collect(),
        ..snap::Row::default()
    };
    snap::HistoryPage::from_rows(&[row], false)
}

/// A snapshot whose one pane's history is one page per text.
fn snapshot(name: &str, pages: &[&str]) -> snap::Snapshot {
    snap::Snapshot {
        meta: snap::Meta {
            purpose: snap::Purpose::CheckpointFull,
            writer_version: name.into(),
            ..snap::Meta::default()
        },
        panes: vec![snap::Pane {
            term: Some(snap::TermState {
                primary: snap::term::Screen {
                    history: pages.iter().map(|t| page(t)).collect(),
                    ..snap::term::Screen::default()
                },
                ..snap::TermState::default()
            }),
            ..snap::Pane::default()
        }],
        ..snap::Snapshot::default()
    }
}

fn chunk_names(s: &snap::Snapshot) -> BTreeSet<String> {
    s.pages().iter().map(|p| snap::hex(&p.hash)).collect()
}

fn on_disk(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir.join(CHUNKS))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The server's `at`: no crash.
fn ok() -> impl FnMut(Stage) -> io::Result<()> {
    |_| Ok(())
}

#[test]
fn a_checkpoint_is_private_and_reads_back() {
    let dir = temp_dir("private");
    let s = snapshot("one", &["alpha", "beta"]);
    let w = write(&dir, &s, &mut ok()).unwrap();
    assert_eq!((w.chunks_new, w.chunks_kept, w.chunks_removed), (2, 0, 0));
    assert_eq!(load(&dir).unwrap(), (s.clone(), MANIFEST));
    // The directory, its new parents and chunks/ 0700; every file 0600.
    for d in [&dir, &dir.join(CHUNKS), dir.parent().unwrap()] {
        assert_eq!(mode(d), 0o700, "{}", d.display());
    }
    assert_eq!(mode(&dir.join(MANIFEST)), 0o600);
    for name in on_disk(&dir) {
        assert_eq!(mode(&dir.join(CHUNKS).join(&name)), 0o600, "{name}");
    }
    assert_eq!(on_disk(&dir), chunk_names(&s));
    // A directory made public meanwhile is made private again.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(dir.join(CHUNKS), fs::Permissions::from_mode(0o755)).unwrap();
    write(&dir, &s, &mut ok()).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(CHUNKS)), 0o700);
    assert_eq!(mode(&dir.join(MANIFEST_PREV)), 0o600);
}

#[test]
fn checkpoints_rotate_write_only_new_pages_and_collect_the_rest() {
    let dir = temp_dir("rotate");
    let s1 = snapshot("one", &["alpha", "beta"]);
    let s2 = snapshot("two", &["beta", "gamma"]);
    let s3 = snapshot("three", &["gamma", "delta"]);
    write(&dir, &s1, &mut ok()).unwrap();
    let w = write(&dir, &s2, &mut ok()).unwrap();
    assert_eq!((w.chunks_new, w.chunks_kept, w.chunks_removed), (1, 1, 0));
    assert_eq!(load(&dir).unwrap(), (s2.clone(), MANIFEST));
    // manifest.prev is the previous checkpoint, its pages all there.
    fs::remove_file(dir.join(MANIFEST)).unwrap();
    assert_eq!(load(&dir).unwrap(), (s1.clone(), MANIFEST_PREV));
    // ... and a checkpoint with no manifest keeps that one as prev.
    write(&dir, &s2, &mut ok()).unwrap();
    let w = write(&dir, &s3, &mut ok()).unwrap();
    // alpha: named by neither s3 nor s2 (now manifest.prev).
    assert_eq!((w.chunks_new, w.chunks_kept, w.chunks_removed), (1, 1, 1));
    let both: BTreeSet<String> = chunk_names(&s2).union(&chunk_names(&s3)).cloned().collect();
    assert_eq!(on_disk(&dir), both);
}

#[test]
fn a_crash_at_any_stage_keeps_the_previous_checkpoint() {
    for (stage, from) in [
        (Stage::ChunksWritten, MANIFEST),
        (Stage::ManifestWritten, MANIFEST),
        (Stage::PrevKept, MANIFEST_PREV),
    ] {
        let dir = temp_dir(&format!("crash-{stage:?}"));
        let s1 = snapshot("one", &["alpha", "beta"]);
        let s2 = snapshot("two", &["beta", "epsilon"]);
        write(&dir, &s1, &mut ok()).unwrap();
        let mut crash = |at: Stage| {
            if at == stage {
                Err(io::Error::other("crash"))
            } else {
                Ok(())
            }
        };
        let err = write(&dir, &s2, &mut crash).unwrap_err();
        assert_eq!(err.to_string(), "crash");
        assert_eq!(load(&dir).unwrap(), (s1.clone(), from), "{stage:?}");

        // What a crash mid-chunk leaves, and the crash's own leftovers
        // (s2's chunk, manifest.tmp), don't stop the next checkpoint and
        // are collected by it; s1 is its manifest.prev.
        fs::write(dir.join(CHUNKS).join("torn.tmp"), b"half").unwrap();
        let s3 = snapshot("three", &["gamma", "delta"]);
        write(&dir, &s3, &mut ok()).unwrap();
        assert_eq!(load(&dir).unwrap(), (s3.clone(), MANIFEST), "{stage:?}");
        let keep: BTreeSet<String> = chunk_names(&s1).union(&chunk_names(&s3)).cloned().collect();
        assert_eq!(on_disk(&dir), keep, "{stage:?}");
        assert!(!dir.join(MANIFEST_TMP).exists());
        fs::remove_file(dir.join(MANIFEST)).unwrap();
        assert_eq!(
            load(&dir).unwrap(),
            (s1.clone(), MANIFEST_PREV),
            "{stage:?}"
        );
    }
}

/// A checkpoint that holds less than its predecessors (`full` set to
/// `layout`) leaves nothing of them: `manifest.prev` is a copy of it and
/// their pages are collected; a crash before `manifest` is replaced still
/// reads the old one, every page there.
#[test]
fn a_replacing_checkpoint_leaves_nothing_of_the_previous_ones() {
    let dir = temp_dir("replacing");
    let full1 = snapshot("one", &["alpha", "beta"]);
    let full2 = snapshot("two", &["beta", "gamma"]);
    let layout = snapshot("layout", &[]);
    write(&dir, &full1, &mut ok()).unwrap();
    write(&dir, &full2, &mut ok()).unwrap();
    let mut crash = |at: Stage| {
        if at == Stage::PrevKept {
            Err(io::Error::other("crash"))
        } else {
            Ok(())
        }
    };
    let mut w = Writer::new(&dir, &layout).unwrap().replacing_prev();
    assert_eq!(w.step(None, &mut crash).unwrap_err().to_string(), "crash");
    assert_eq!(load(&dir).unwrap(), (full2.clone(), MANIFEST));
    assert_eq!(load_file(&dir, MANIFEST_PREV).unwrap().0, layout);

    let mut w = Writer::new(&dir, &layout).unwrap().replacing_prev();
    assert!(w.step(None, &mut ok()).unwrap());
    assert_eq!(load_file(&dir, MANIFEST).unwrap().0, layout);
    assert_eq!(load_file(&dir, MANIFEST_PREV).unwrap().0, layout);
    assert!(on_disk(&dir).is_empty());
    assert!(!dir.join(MANIFEST_TMP).exists());
    assert!(!dir.join(MANIFEST_PREV_TMP).exists());
    assert_eq!(mode(&dir.join(MANIFEST_PREV)), 0o600);
}

#[test]
fn a_torn_or_missing_manifest_falls_back_to_the_previous() {
    let dir = temp_dir("torn");
    let s1 = snapshot("one", &["alpha"]);
    let s2 = snapshot("two", &["beta"]);
    assert_eq!(load(&dir).unwrap_err().kind(), io::ErrorKind::NotFound);
    write(&dir, &s1, &mut ok()).unwrap();
    write(&dir, &s2, &mut ok()).unwrap();
    let bytes = fs::read(dir.join(MANIFEST)).unwrap();
    fs::write(dir.join(MANIFEST), &bytes[..bytes.len() / 2]).unwrap();
    assert_eq!(load(&dir).unwrap(), (s1, MANIFEST_PREV));
    // A chunk of the latest gone: the previous too.
    fs::write(dir.join(MANIFEST), &bytes).unwrap();
    fs::remove_file(dir.join(CHUNKS).join(snap::hex(&s2.pages()[0].hash))).unwrap();
    assert_eq!(load(&dir).unwrap().1, MANIFEST_PREV);
    fs::write(dir.join(MANIFEST_PREV), b"not a snapshot at all").unwrap();
    let err = load(&dir).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(
        err.to_string()
            .contains("manifest.prev: not a vtmux snapshot"),
        "{err}"
    );
}

/// A torn `manifest` over a good `manifest.prev`: the next checkpoint
/// replaces the torn one and keeps `manifest.prev`, the one checkpoint a
/// crash can fall back to, at every stage; the one after rotates as usual.
/// Also by a writer that wrote the manifest before it was torn
/// ([`Writer::after`]): only the same bytes skip the read.
#[test]
fn a_torn_manifest_is_replaced_not_kept_over_the_good_previous() {
    let stages = [
        Stage::ChunksWritten,
        Stage::ManifestWritten,
        Stage::PrevKept,
    ];
    for (stage, known) in stages.into_iter().flat_map(|s| [(s, false), (s, true)]) {
        let dir = temp_dir(&format!("torn-rotate-{stage:?}-{known}"));
        let s1 = snapshot("one", &["alpha"]);
        let s2 = snapshot("two", &["beta"]);
        write(&dir, &s1, &mut ok()).unwrap();
        write(&dir, &s2, &mut ok()).unwrap();
        let bytes = fs::read(dir.join(MANIFEST)).unwrap();
        fs::write(dir.join(MANIFEST), &bytes[..bytes.len() / 2]).unwrap();
        assert_eq!(load(&dir).unwrap(), (s1.clone(), MANIFEST_PREV));
        let s3 = snapshot("three", &["gamma"]);
        let mut crash = |at: Stage| {
            if at == stage {
                Err(io::Error::other("crash"))
            } else {
                Ok(())
            }
        };
        let mut w = Writer::new(&dir, &s3)
            .unwrap()
            .after(known.then(|| bytes.clone()));
        assert_eq!(w.step(None, &mut crash).unwrap_err().to_string(), "crash");
        assert_eq!(
            load(&dir).unwrap(),
            (s1.clone(), MANIFEST_PREV),
            "{stage:?}"
        );
        write(&dir, &s3, &mut ok()).unwrap();
        assert_eq!(load(&dir).unwrap(), (s3.clone(), MANIFEST), "{stage:?}");
        assert_eq!(load_file(&dir, MANIFEST_PREV).unwrap().0, s1, "{stage:?}");
        let s4 = snapshot("four", &["delta"]);
        write(&dir, &s4, &mut ok()).unwrap();
        assert_eq!(load_file(&dir, MANIFEST_PREV).unwrap().0, s3, "{stage:?}");
    }
}

#[test]
fn nothing_is_collected_while_the_previous_manifest_cant_be_listed() {
    let dir = temp_dir("unlisted");
    let s1 = snapshot("one", &["alpha"]);
    write(&dir, &s1, &mut ok()).unwrap();
    // `manifest` (s1) becomes manifest.prev at the next write: from a
    // newer vtmux, say, whose pages this build can't see.
    let mut bytes = fs::read(dir.join(MANIFEST)).unwrap();
    bytes[8..12].copy_from_slice(&(snap::FORMAT_VERSION + 1).to_le_bytes());
    fs::write(dir.join(MANIFEST), &bytes).unwrap();
    let s2 = snapshot("two", &["beta"]);
    let w = write(&dir, &s2, &mut ok()).unwrap();
    assert_eq!(w.chunks_removed, 0);
    let all: BTreeSet<String> = chunk_names(&s1).union(&chunk_names(&s2)).cloned().collect();
    assert_eq!(on_disk(&dir), all);
}

#[test]
fn the_state_dir_follows_xdg_state_home_else_home() {
    let socket = Path::new("/tmp/tmux-1000/work");
    let home = Path::new("/home/u");
    let leaf = format!("work-{}", path_hash(socket));
    assert_eq!(
        state_dir(socket, Some("/x/state".as_ref()), Some(home)),
        Some(PathBuf::from("/x/state/vtmux").join(&leaf))
    );
    // A relative XDG_STATE_HOME is ignored (XDG base directory rules).
    for xdg in [None, Some("rel/state"), Some("")] {
        assert_eq!(
            state_dir(socket, xdg.map(AsRef::as_ref), Some(home)),
            Some(PathBuf::from("/home/u/.local/state/vtmux").join(&leaf)),
            "{xdg:?}"
        );
    }
    assert_eq!(state_dir(socket, None, None), None);
    assert_eq!(state_dir(Path::new("/"), None, Some(home)), None);
}

/// Sockets of one name in two directories (`-S a/s`, `-S b/s`; one `-L`
/// under two `TMUX_TMPDIR`s) get two directories, each still named for
/// the socket; the hash is fixed forever (a restore looks it up).
#[test]
fn the_state_dir_is_one_per_socket_path() {
    let home = Some(Path::new("/home/u"));
    let a = state_dir(Path::new("/t/a/rvsock"), None, home).unwrap();
    let b = state_dir(Path::new("/t/b/rvsock"), None, home).unwrap();
    assert_ne!(a, b);
    for d in [&a, &b] {
        let leaf = d.file_name().unwrap().to_str().unwrap();
        let hash = leaf.strip_prefix("rvsock-").unwrap();
        assert_eq!(hash.len(), 12, "{leaf}");
        assert!(hash.bytes().all(|c| c.is_ascii_hexdigit()), "{leaf}");
    }
    // FNV-1a 64 of the bytes, top 48 bits (checked by hand in Python).
    assert_eq!(
        path_hash(Path::new("/tmp/tmux-1000/default")),
        "79301cf9592f"
    );
}

#[test]
fn the_lock_is_held_by_one_at_a_time() {
    let dir = temp_dir("lock");
    let first = Lock::take(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(LOCK)), 0o600);
    assert_eq!(
        Lock::take(&dir).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(first);
    let again = Lock::take(&dir).unwrap();
    // The lock file is no chunk and no manifest: a checkpoint leaves it.
    write(&dir, &snapshot("one", &["alpha"]), &mut ok()).unwrap();
    assert!(dir.join(LOCK).is_file());
    drop(again);
}

/// [`Writer`] a chunk per step at a zero budget; the manifest last.
#[test]
fn a_writer_writes_a_chunk_per_step_then_the_manifest() {
    let dir = temp_dir("steps");
    let s1 = snapshot("one", &["alpha"]);
    write(&dir, &s1, &mut ok()).unwrap();
    let s2 = snapshot("two", &["alpha", "beta", "gamma", "delta"]);
    let mut w = Writer::new(&dir, &s2).unwrap();
    assert_eq!((w.chunks_left(), w.written().chunks_kept), (3, 1));
    for left in [2, 1, 0] {
        assert!(!w.step(Some(Duration::ZERO), &mut ok()).unwrap() || left == 0);
        assert_eq!(w.chunks_left(), left);
        if left > 0 {
            assert_eq!(load(&dir).unwrap(), (s1.clone(), MANIFEST));
        }
    }
    assert_eq!(load(&dir).unwrap(), (s2.clone(), MANIFEST));
    let done = w.written();
    assert_eq!(
        (done.chunks_new, done.chunks_kept, done.chunks_removed),
        (3, 1, 0)
    );
}
