//! `vtmux-persist` checkpoints on disk (D27 phase 3,
//! docs/specs/2026-11-06-live-upgrade.md, decisions 8, 17 and 20; *Design >
//! Persistence > Writing*). One directory per socket, private to the user
//! (scrollback may hold secrets):
//!
//! ```text
//! $XDG_STATE_HOME/vtmux/<socket-name>-<hash>/   0700  (default ~/.local/state)
//!   lock           held by the server writing here  0600
//!   manifest       the latest checkpoint            0600
//!   manifest.prev  the one before it                0600
//!   chunks/        0700
//!     <hash>       a history page, LZ4              0600
//! ```
//!
//! The directory's `<hash>` is of the socket's absolute path
//! ([`state_dir`]); the server writing it holds `lock` ([`Lock`]), so no
//! two servers share a checkpoint, race on `manifest.tmp` or collect each
//! other's chunks.
//!
//! A manifest is a snapshot stream whose history pages are named, not
//! inline ([`vtmux_snapshot::encode_manifest`]); each page lives in
//! `chunks/<hex of its hash>`, written once and shared by every manifest
//! that names it, so after the first checkpoint only new pages are written
//! (decision 16).
//!
//! A checkpoint ([`write`], or [`Writer`] a few chunks at a time) is
//! crash-safe at every point: it writes each missing chunk as
//! `chunks/<hash>.tmp`, syncs it and renames it, syncs `chunks/`, then
//! writes `manifest.tmp`, flushes it to stable storage, renames `manifest`
//! to `manifest.prev` (unless `manifest` does not read and `manifest.prev`
//! does: that one stays, the last good checkpoint) and `manifest.tmp` to
//! `manifest`, and syncs the directory. Only then does it delete the chunks neither manifest names,
//! and any `.tmp` a crash left. A crash before the last rename leaves
//! `manifest` (or, after the first rename, `manifest.prev`) the previous
//! checkpoint, every chunk it names still there; [`load`] reads
//! `manifest`, else `manifest.prev`.
//!
//! One flush to stable storage per checkpoint ([`Flush`]): on macOS
//! `fsync` stops at the drive's cache and `F_FULLFSYNC` costs a
//! cache flush of the whole drive, so only the manifest gets it, and it
//! carries the chunks synced before it to storage too.

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vtmux_snapshot as snap;

/// The latest checkpoint.
pub const MANIFEST: &str = "manifest";
/// The checkpoint before it, kept for a crash mid-write (decision 17).
pub const MANIFEST_PREV: &str = "manifest.prev";
/// The manifest being written.
pub const MANIFEST_TMP: &str = "manifest.tmp";
/// `manifest.prev` being written ([`Writer::replacing_prev`]).
pub const MANIFEST_PREV_TMP: &str = "manifest.prev.tmp";
/// The history pages, by hash.
pub const CHUNKS: &str = "chunks";
/// Held (`flock`) by the server writing the directory ([`Lock`]).
pub const LOCK: &str = "lock";

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// `$XDG_STATE_HOME/vtmux/<socket-name>-<hash>` for the server listening
/// on `socket` (absolute): `XDG_STATE_HOME` when it is an absolute path
/// (the XDG base directory rules ignore a relative one), else
/// `<home>/.local/state`. `<hash>` is [`path_hash`] of `socket`, so
/// servers on sockets of one name in two directories (`-S a/s` and
/// `-S b/s`, one `-L` under two `TMUX_TMPDIR`s) keep apart, while the
/// name still says whose it is. The variables are the server process's
/// own, which it inherits from the client that started it, so a restore
/// at the next start looks in the same place. `None` without a home or a
/// socket file name.
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
    let mut leaf = name.to_os_string();
    leaf.push(format!("-{}", path_hash(socket)));
    Some(base.join("vtmux").join(leaf))
}

/// 12 hex digits of the 64-bit FNV-1a hash of `path`'s bytes: short, and
/// fixed forever (a restore finds its directory by it), unlike std's
/// `DefaultHasher`.
#[must_use]
pub fn path_hash(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in path.as_os_str().as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:012x}", h >> 16)
}

/// Where a checkpoint write has got to, for a test to stop it there as a
/// crash would ([`write`]'s `at`).
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

/// Write `snapshot` as the checkpoint in `dir` (above), all at once.
/// `at` is called at each [`Stage`]; an error from it stops the write
/// there, as a crash would, and is returned.
///
/// # Errors
/// A directory, file, sync or rename failed, or `at` failed. Whatever
/// failed, the previous checkpoint stays readable.
pub fn write(
    dir: &Path,
    snapshot: &snap::Snapshot,
    at: &mut dyn FnMut(Stage) -> io::Result<()>,
) -> io::Result<Written> {
    let mut w = Writer::new(dir, snapshot)?;
    while !w.step(None, at)? {}
    Ok(w.done)
}

/// A checkpoint written a few chunks at a time ([`Writer::step`]), so a
/// first `full` checkpoint (every history page a chunk, each synced:
/// hundreds of milliseconds at 10 panes x 100k lines) never holds the
/// server's loop for long. It owns what it writes: the manifest, encoded
/// when it starts, and the pages not yet on disk.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    manifest: Vec<u8>,
    /// Every chunk the manifest names.
    names: BTreeSet<String>,
    /// The chunks still to write, name and data, last first.
    todo: Vec<(String, Vec<u8>)>,
    done: Written,
    /// `manifest.prev` becomes a copy of this manifest, not the one it
    /// replaces ([`Writer::replacing_prev`]).
    replace_prev: bool,
    /// What `manifest` holds when it is the checkpoint this process last
    /// wrote ([`Writer::after`]).
    known: Option<Vec<u8>>,
}

impl Writer {
    /// Start writing `snapshot` into `dir`: make the directories and find
    /// the pages already there.
    ///
    /// # Errors
    /// A directory can't be made private, or a page not on disk has no
    /// data.
    pub fn new(dir: &Path, snapshot: &snap::Snapshot) -> io::Result<Self> {
        let chunks = dir.join(CHUNKS);
        private_dir(dir)?;
        private_dir(&chunks)?;
        let mut done = Written::default();
        let mut names = BTreeSet::new();
        let mut todo = Vec::new();
        for page in snapshot.pages() {
            let name = snap::hex(&page.hash);
            if !names.insert(name.clone()) {
                continue;
            }
            if chunks.join(&name).is_file() {
                done.chunks_kept += 1;
                continue;
            }
            if page.data.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("history page {name} has no data and no chunk"),
                ));
            }
            todo.push((name, page.data.clone()));
        }
        todo.reverse();
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest: snap::encode_manifest(snapshot),
            names,
            todo,
            done,
            replace_prev: false,
            known: None,
        })
    }

    /// `manifest` should hold `bytes` ([`Writer::manifest`] of the last
    /// checkpoint this process wrote): when it does, it becomes
    /// `manifest.prev` without being read through first ([`rotates`]).
    #[must_use]
    pub fn after(mut self, bytes: Option<Vec<u8>>) -> Self {
        self.known = bytes;
        self
    }

    /// The manifest this writes.
    #[must_use]
    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Make `manifest.prev` a copy of this checkpoint instead of keeping
    /// the one it replaces, so nothing of the old content stays on disk
    /// once the chunks no manifest names are collected: for a checkpoint
    /// that holds less than its predecessors (`vtmux-persist full` set to
    /// `layout`: the screens and history are not to stay in
    /// `manifest.prev` and `chunks/` until the next change). Still crash
    /// safe: `manifest.prev` is replaced (atomically) before `manifest`
    /// is, and a crash between them leaves `manifest`, the old checkpoint,
    /// with every chunk it names.
    #[must_use]
    pub fn replacing_prev(mut self) -> Self {
        self.replace_prev = true;
        self
    }

    /// Chunks still to write.
    #[must_use]
    pub fn chunks_left(&self) -> usize {
        self.todo.len()
    }

    /// What the write has done so far; all of it once [`Writer::step`]
    /// returned `true`.
    #[must_use]
    pub fn written(&self) -> &Written {
        &self.done
    }

    /// Write chunks until `budget` is spent (at least one; `None`: no
    /// limit). With none left, finish: sync `chunks/`, write the manifest,
    /// rotate, and collect. `true` once finished.
    ///
    /// # Errors
    /// As [`write`]; the previous checkpoint stays readable.
    pub fn step(
        &mut self,
        budget: Option<Duration>,
        at: &mut dyn FnMut(Stage) -> io::Result<()>,
    ) -> io::Result<bool> {
        let start = Instant::now();
        let chunks = self.dir.join(CHUNKS);
        while let Some((name, data)) = self.todo.pop() {
            replace(
                &chunks.join(format!("{name}.tmp")),
                &chunks.join(&name),
                &data,
            )?;
            self.done.chunks_new += 1;
            self.done.bytes += data.len() as u64;
            if budget.is_some_and(|b| start.elapsed() >= b) {
                break;
            }
        }
        if !self.todo.is_empty() {
            return Ok(false);
        }
        // Only renames in `chunks/` need it; the manifest's full flush
        // below orders the chunks' data before it.
        if self.done.chunks_new > 0 {
            sync_dir(&chunks)?;
        }
        at(Stage::ChunksWritten)?;
        let dir = &self.dir;
        let tmp = dir.join(MANIFEST_TMP);
        write_file(&tmp, &self.manifest, Flush::Full)?;
        self.done.bytes += self.manifest.len() as u64;
        at(Stage::ManifestWritten)?;
        let latest = dir.join(MANIFEST);
        if self.replace_prev {
            replace(
                &dir.join(MANIFEST_PREV_TMP),
                &dir.join(MANIFEST_PREV),
                &self.manifest,
            )?;
        } else if rotates(dir, self.known.as_deref()) {
            match fs::rename(&latest, dir.join(MANIFEST_PREV)) {
                Ok(()) => {}
                // The first checkpoint, or a crash after the rename below's
                // predecessor: `manifest.prev` already holds the previous one.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        at(Stage::PrevKept)?;
        fs::rename(&tmp, &latest)?;
        sync_dir(dir)?;
        self.done.chunks_removed = collect(dir, &self.names);
        Ok(true)
    }
}

/// Whether `manifest` is to become `manifest.prev` at a checkpoint: yes,
/// unless it does not read ([`load_file`]) and `manifest.prev` does. Then
/// `manifest.prev` is the one checkpoint to fall back to, kept until the
/// new `manifest` is in place, and the unreadable `manifest` is replaced.
/// A `manifest` holding `known` (the checkpoint this process last wrote,
/// synced with its chunks) is not read through: that is a decode of every
/// chunk, done only at a server's first checkpoint.
fn rotates(dir: &Path, known: Option<&[u8]>) -> bool {
    let reads =
        |name| std::panic::catch_unwind(|| load_file(dir, name)).is_ok_and(|loaded| loaded.is_ok());
    known.is_some_and(|k| fs::read(dir.join(MANIFEST)).is_ok_and(|b| b == k))
        || reads(MANIFEST)
        || !reads(MANIFEST_PREV)
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
    let mut errors = Vec::new();
    let mut found = false;
    for name in [MANIFEST, MANIFEST_PREV] {
        match load_file(dir, name) {
            Ok((s, _)) => return Ok((s, name)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                found = true;
                errors.push(format!("{name}: {e}"));
            }
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

/// Read the checkpoint manifest `name` in `dir` ([`MANIFEST`] or
/// [`MANIFEST_PREV`]) with its pages from `chunks/`. A copy mode holding
/// a page that does not check (its chunk missing or corrupt, or past a
/// reader's limits: a writer before 8c94c6e put copy mode over a long
/// history in one page) is left out, the pane restored without it, as a
/// live upgrade's last resort does: copy mode is a view of the pane, whose
/// own screen and history are in its terminal state. Returns the snapshot
/// and what was left out, with why.
///
/// # Errors
/// `NotFound` when the file does not exist; `InvalidData` when it does not
/// decode (a pane's or popup's page missing from `chunks/` is one way);
/// else the read's.
pub fn load_file(dir: &Path, name: &str) -> io::Result<(snap::Snapshot, Vec<String>)> {
    let invalid = |e: snap::Error| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
    let bytes = fs::read(dir.join(name))?;
    let chunks = dir.join(CHUNKS);
    let mut s = snap::decode_partial(&bytes, &snap::Features::default(), |h| {
        fs::read(chunks.join(snap::hex(h))).ok()
    })
    .map_err(invalid)?;
    let mut modes = Vec::new();
    let mut why = Vec::new();
    for (part, e) in s.unreadable() {
        match part {
            snap::Part::PaneMode(..) => {
                modes.push(part);
                why.push(format!("{part}: {e}"));
            }
            snap::Part::PaneTerm(_) | snap::Part::PopupTerm(_) => return Err(invalid(e)),
        }
    }
    s.without(&modes);
    Ok((s, why))
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

/// How [`write_file`] syncs.
#[derive(Clone, Copy)]
enum Flush {
    /// `fsync`: to the drive. On macOS that leaves it in the drive's
    /// cache, so it is cheap there.
    Data,
    /// To stable storage: on macOS `F_FULLFSYNC`, which also empties the
    /// drive's cache of everything synced before it (fcntl(2)); elsewhere
    /// `fsync` already does. One per checkpoint, on the manifest: the
    /// chunks synced before it are then on storage before the manifest
    /// naming them can be.
    Full,
}

/// Write `bytes` to a fresh mode-0600 `path` and sync it. A file a crash
/// left there goes first, so the mode is this one.
fn write_file(path: &Path, bytes: &[u8], flush: Flush) -> io::Result<()> {
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
    match flush {
        Flush::Data => rustix::fs::fsync(&f)?,
        Flush::Full => full_sync(&f)?,
    }
    Ok(())
}

/// [`Flush::Full`]. A file system without `F_FULLFSYNC` (network ones
/// answer `ENOTSUP`) gets `fsync`, as SQLite does (`unixSync`).
#[cfg(target_vendor = "apple")]
fn full_sync(f: &File) -> io::Result<()> {
    rustix::fs::fcntl_fullfsync(f).or_else(|_| rustix::fs::fsync(f))?;
    Ok(())
}

/// [`Flush::Full`]: `fsync` reaches stable storage here.
#[cfg(not(target_vendor = "apple"))]
fn full_sync(f: &File) -> io::Result<()> {
    rustix::fs::fsync(f)?;
    Ok(())
}

/// `bytes` at `path`, through `tmp`: never a partial `path`.
fn replace(tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_file(tmp, bytes, Flush::Data)?;
    fs::rename(tmp, path)
}

/// Sync a directory, so the renames in it reach the drive (plain `fsync`:
/// a rename lost with the drive's cache leaves the previous manifest,
/// which [`load`] reads).
fn sync_dir(dir: &Path) -> io::Result<()> {
    rustix::fs::fsync(File::open(dir)?)?;
    Ok(())
}

/// The lock a server holds on its checkpoint directory while it writes
/// there ([`LOCK`], `flock`), so two servers never write one directory.
/// Released when dropped, or when the process ends however it ends.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Take the lock on `dir` (made private first) without waiting.
    ///
    /// # Errors
    /// `WouldBlock` while another holds it (another process, or another
    /// [`Lock`] in this one); else the directory or lock file failed.
    pub fn take(dir: &Path) -> io::Result<Self> {
        private_dir(dir)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(dir.join(LOCK))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests;
