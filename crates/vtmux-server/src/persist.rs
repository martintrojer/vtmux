//! `vtmux-persist` checkpoints on disk (D27 phase 3,
//! docs/specs/2026-11-06-live-upgrade.md, decisions 8, 17 and 20; *Design >
//! Persistence > Writing*). One directory per socket name, private to the
//! user (scrollback may hold secrets):
//!
//! ```text
//! $XDG_STATE_HOME/vtmux/<socket-name>/   0700  (default ~/.local/state)
//!   manifest       the latest checkpoint  0600
//!   manifest.prev  the one before it      0600
//!   chunks/        0700
//!     <hash>       a history page, LZ4    0600
//! ```
//!
//! A manifest is a snapshot stream whose history pages are named, not
//! inline ([`vtmux_snapshot::encode_manifest`]); each page lives in
//! `chunks/<hex of its hash>`, written once and shared by every manifest
//! that names it, so after the first checkpoint only new pages are written
//! (decision 16).
//!
//! [`write`] is crash-safe at every point: it writes each missing chunk as
//! `chunks/<hash>.tmp`, syncs it and renames it, syncs `chunks/`, then
//! writes `manifest.tmp`, syncs it, renames `manifest` to `manifest.prev`
//! and `manifest.tmp` to `manifest`, and syncs the directory. Only then
//! does it delete the chunks neither manifest names, and any `.tmp` a
//! crash left. A crash before the last rename leaves `manifest` (or, after
//! the first rename, `manifest.prev`) the previous checkpoint, every chunk
//! it names still there; [`load`] reads `manifest`, else `manifest.prev`.

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use vtmux_snapshot as snap;

/// The latest checkpoint.
pub const MANIFEST: &str = "manifest";
/// The checkpoint before it, kept for a crash mid-write (decision 17).
pub const MANIFEST_PREV: &str = "manifest.prev";
/// The manifest being written.
pub const MANIFEST_TMP: &str = "manifest.tmp";
/// The history pages, by hash.
pub const CHUNKS: &str = "chunks";

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// `$XDG_STATE_HOME/vtmux/<socket-name>` for the server listening on
/// `socket`: `XDG_STATE_HOME` when it is an absolute path (the XDG base
/// directory rules ignore a relative one), else `<home>/.local/state`.
/// The variables are the server process's own, which it inherits from the
/// client that started it, so a restore at the next start looks in the
/// same place. `None` without a home or a socket file name.
#[must_use]
pub fn state_dir(
    socket: &Path,
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let name = socket.file_name()?;
    let base = match xdg_state_home.map(Path::new) {
        Some(x) if x.is_absolute() => x.to_path_buf(),
        _ => home?.join(".local/state"),
    };
    Some(base.join("vtmux").join(name))
}

/// Where [`write`] has got to, for a test to stop it there as a crash
/// would ([`write`]'s `at`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Every chunk the manifest names is on disk, the manifest not yet.
    ChunksWritten,
    /// `manifest.tmp` is written and synced, not renamed.
    ManifestWritten,
    /// `manifest` has become `manifest.prev`; `manifest.tmp` is not yet
    /// `manifest`.
    PrevKept,
}

/// What a [`write`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Written {
    /// Chunks written now.
    pub chunks_new: usize,
    /// Chunks the manifest names that were already on disk.
    pub chunks_kept: usize,
    /// Chunks (and leftover temp files) deleted.
    pub chunks_removed: usize,
    /// Bytes written: new chunks and the manifest.
    pub bytes: u64,
}

/// Write `snapshot` as the checkpoint in `dir` (above). `at` is called at
/// each [`Stage`]; an error from it stops the write there, as a crash
/// would, and is returned. The server passes `|_| Ok(())`.
///
/// # Errors
/// A directory, file, sync or rename failed, or `at` failed. Whatever
/// failed, the previous checkpoint stays readable.
pub fn write(
    dir: &Path,
    snapshot: &snap::Snapshot,
    at: &mut dyn FnMut(Stage) -> io::Result<()>,
) -> io::Result<Written> {
    let chunks = dir.join(CHUNKS);
    private_dir(dir)?;
    private_dir(&chunks)?;
    let mut done = Written::default();
    let mut names = BTreeSet::new();
    for page in snapshot.pages() {
        let name = snap::hex(&page.hash);
        if !names.insert(name.clone()) {
            continue;
        }
        let path = chunks.join(&name);
        if path.is_file() {
            done.chunks_kept += 1;
            continue;
        }
        if page.data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("history page {name} has no data and no chunk"),
            ));
        }
        replace(&chunks.join(format!("{name}.tmp")), &path, &page.data)?;
        done.chunks_new += 1;
        done.bytes += page.data.len() as u64;
    }
    sync_dir(&chunks)?;
    at(Stage::ChunksWritten)?;

    let manifest = snap::encode_manifest(snapshot);
    let tmp = dir.join(MANIFEST_TMP);
    write_synced(&tmp, &manifest)?;
    done.bytes += manifest.len() as u64;
    at(Stage::ManifestWritten)?;
    let latest = dir.join(MANIFEST);
    match fs::rename(&latest, dir.join(MANIFEST_PREV)) {
        Ok(()) => {}
        // The first checkpoint, or a crash after the rename below's
        // predecessor: `manifest.prev` already holds the previous one.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    at(Stage::PrevKept)?;
    fs::rename(&tmp, &latest)?;
    sync_dir(dir)?;

    done.chunks_removed = collect(dir, &names);
    Ok(done)
}

/// Delete every entry of `chunks/` that neither manifest names: `names`
/// (the one just written) and those `manifest.prev` names. Nothing is
/// deleted when `manifest.prev` exists but can't be listed (unreadable,
/// newer, or requiring a feature this build lacks): a chunk it names may
/// be among them. Errors deleting are left for the next checkpoint.
fn collect(dir: &Path, names: &BTreeSet<String>) -> usize {
    let mut keep = names.clone();
    match fs::read(dir.join(MANIFEST_PREV)) {
        Ok(bytes) => match snap::page_refs(&bytes) {
            Ok(refs) => keep.extend(refs.iter().map(snap::hex)),
            Err(_) => return 0,
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return 0,
    }
    let Ok(entries) = fs::read_dir(dir.join(CHUNKS)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(|n| keep.contains(n)) {
            continue;
        }
        if fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Read the checkpoint in `dir`: `manifest`, else `manifest.prev`, each
/// with its pages from `chunks/`. Returns the snapshot and the file it
/// came from.
///
/// # Errors
/// Neither reads (`NotFound` when neither exists; else `InvalidData`
/// naming each file's error).
pub fn load(dir: &Path) -> io::Result<(snap::Snapshot, &'static str)> {
    let chunks = dir.join(CHUNKS);
    let mut errors = Vec::new();
    let mut found = false;
    for name in [MANIFEST, MANIFEST_PREV] {
        let bytes = match fs::read(dir.join(name)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                found = true;
                errors.push(format!("{name}: {e}"));
                continue;
            }
        };
        found = true;
        match snap::decode_with(&bytes, |h| fs::read(chunks.join(snap::hex(h))).ok()) {
            Ok(s) => return Ok((s, name)),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    if !found {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no checkpoint"));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        errors.join("; "),
    ))
}

/// Make `dir` (and missing parents) mode 0700, and an existing `dir` 0700
/// too: the checkpoint is private however the directory came to be.
fn private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)?;
    let meta = fs::metadata(dir)?;
    if meta.permissions().mode() & 0o7777 != DIR_MODE {
        fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
    }
    Ok(())
}

/// Write `bytes` to a fresh mode-0600 `path` and sync it. A file a crash
/// left there goes first, so the mode is this one.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// `bytes` at `path`, through `tmp`: never a partial `path`.
fn replace(tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_synced(tmp, bytes)?;
    fs::rename(tmp, path)
}

/// Sync a directory, so the renames in it last.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests;
