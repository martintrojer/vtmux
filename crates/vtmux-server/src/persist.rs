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
//! A checkpoint ([`write`], or [`Writer`] a step at a time) is
//! crash-safe at every point: it writes each missing chunk as
//! `chunks/<hash>.tmp`, syncs it and renames it, syncs `chunks/`, then
//! writes `manifest.tmp`, flushes it to stable storage, renames `manifest`
//! to `manifest.prev` (unless `manifest` does not read and `manifest.prev`
//! does: that one stays, the last good checkpoint) and `manifest.tmp` to
//! `manifest`, and syncs the directory. Only then does it delete the
//! chunks neither manifest names, and any `.tmp` a crash left. A crash
//! before the last rename leaves `manifest` (or, after the first rename,
//! `manifest.prev`) the previous checkpoint, every chunk it names still
//! there; [`load`] reads `manifest`, else `manifest.prev`.
//!
//! Nothing on disk is taken on trust: a chunk already there is named by
//! the new manifest, and `manifest` rotated without reading it through,
//! only when this process wrote it or read it back and found it to check
//! ([`Known`]); else it is read back first, a chunk per piece of a step,
//! so the first checkpoint of a new process (a live upgrade's image) does
//! not hold the loop either. A restore seeds what it read.
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
    /// A new chunk's `.tmp` is written and synced, not yet renamed: once
    /// per chunk.
    ChunkTmp,
    /// Every chunk the manifest names is on disk, the manifest not yet.
    ChunksWritten,
    /// `manifest.tmp` is written and synced, not renamed.
    ManifestWritten,
    /// `manifest` has become `manifest.prev`; `manifest.tmp` is not yet
    /// `manifest`.
    PrevKept,
    /// `manifest.tmp` is `manifest`; the directory is not yet synced.
    Renamed,
    /// About to delete an entry of `chunks/` that neither manifest names:
    /// once per entry.
    Collecting,
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
    /// Chunks read back to check them ([`Known`]): reused ones this
    /// process had not written or checked, and those of the `manifest`
    /// about to become `manifest.prev`.
    pub chunks_checked: usize,
    /// Bytes written: new chunks and the manifest.
    pub bytes: u64,
}

/// What a process knows of its checkpoint directory from its own writes
/// and reads, so that a checkpoint reads back only what it does not
/// ([`Writer::new`]). It goes with the process: a new one (a live
/// upgrade's image too) starts knowing nothing, or what its restore read
/// ([`read_file`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Known {
    /// A `manifest` this process wrote or read whole: while `manifest`
    /// holds these bytes, it reads.
    pub manifest: Option<Vec<u8>>,
    /// Chunks this process wrote, or read back and found to check.
    pub chunks: BTreeSet<String>,
}

/// Write `snapshot` as the checkpoint in `dir` (above), all at once,
/// knowing nothing of the directory. `at` is called at each [`Stage`]; an
/// error from it stops the write there, as a crash would, and is returned.
///
/// # Errors
/// A directory, file, sync or rename failed, or `at` failed. Whatever
/// failed, the previous checkpoint stays readable.
pub fn write(
    dir: &Path,
    snapshot: &snap::Snapshot,
    at: &mut dyn FnMut(Stage) -> io::Result<()>,
) -> io::Result<Written> {
    let mut w = Writer::new(dir, snapshot, &Known::default())?;
    while !w.step(None, at)? {}
    Ok(w.done)
}

/// A checkpoint written a step at a time ([`Writer::step`]), so a first
/// `full` checkpoint (every history page a chunk, each synced: hundreds
/// of milliseconds at 10 panes x 100k lines), or the first of a process
/// that must read back the chunks it did not write, never holds the
/// server's loop for long. It owns what it writes: the manifest, encoded
/// when it starts, and the pages not yet on disk.
///
/// Before the manifest names a chunk already on disk that this process
/// neither wrote nor checked ([`Known`]), the chunk is read back and
/// checked against its page, a chunk a step, and written again if it does
/// not check: a manifest never names a chunk only assumed good. Before
/// `manifest` becomes `manifest.prev`, it is read through the same way
/// unless it is the [`Known`] one: a `manifest` that does not read, over a
/// `manifest.prev` that does, is replaced instead, so `manifest.prev`
/// stays the one checkpoint to fall back to.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    manifest: Vec<u8>,
    /// Every chunk the manifest names.
    names: BTreeSet<String>,
    /// The chunks on disk the manifest names that this process does not
    /// know, with their pages, still to check, last first.
    unchecked: Vec<(String, snap::HistoryPage)>,
    /// The chunks still to write, name and data, last first.
    todo: Vec<(String, Vec<u8>)>,
    done: Written,
    /// `manifest.prev` becomes a copy of this manifest, not the one it
    /// replaces ([`Writer::replacing_prev`]).
    replace_prev: bool,
    known: Known,
    /// `chunks/` is synced: every chunk the manifest names is on disk.
    synced: bool,
    rotation: Rotation,
    /// The manifest is in place.
    finished: bool,
}

/// Whether `manifest` becomes `manifest.prev` at a checkpoint: yes,
/// unless it does not read ([`load_file`]) and `manifest.prev` does.
#[derive(Debug)]
enum Rotation {
    /// Not looked at yet.
    Unknown,
    /// Reading through manifest `.0`: its terminals' pages still to check,
    /// last first.
    Reading(&'static str, Vec<snap::HistoryPage>),
    Decided(bool),
}

impl Writer {
    /// Start writing `snapshot` into `dir`, which this process knows as
    /// `known`: make the directories and find the pages already there.
    ///
    /// # Errors
    /// A directory can't be made private, or a page not on disk has no
    /// data.
    pub fn new(dir: &Path, snapshot: &snap::Snapshot, known: &Known) -> io::Result<Self> {
        let chunks = dir.join(CHUNKS);
        private_dir(dir)?;
        private_dir(&chunks)?;
        let mut done = Written::default();
        let mut names = BTreeSet::new();
        let mut unchecked = Vec::new();
        let mut todo = Vec::new();
        for page in snapshot.pages() {
            let name = snap::hex(&page.hash);
            if !names.insert(name.clone()) {
                continue;
            }
            if chunks.join(&name).is_file() {
                if known.chunks.contains(&name) {
                    done.chunks_kept += 1;
                } else {
                    unchecked.push((name, page.clone()));
                }
                continue;
            }
            if page.data.is_empty() {
                return Err(no_data(&name));
            }
            todo.push((name, page.data.clone()));
        }
        todo.reverse();
        unchecked.reverse();
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest: snap::encode_manifest(snapshot),
            names,
            unchecked,
            todo,
            done,
            replace_prev: false,
            known: known.clone(),
            synced: false,
            rotation: Rotation::Unknown,
            finished: false,
        })
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

    /// What the process knows of the directory now: what it knew, the
    /// chunks this write wrote or checked (less those it deleted), and
    /// the manifest once it is in place. Sound whenever the write stopped.
    #[must_use]
    pub fn into_known(mut self) -> Known {
        if self.finished {
            self.known.manifest = Some(self.manifest);
        }
        self.known
    }

    /// Work until `budget` is spent (at least one piece; `None`: no
    /// limit). A piece is a chunk checked or written, the sync of
    /// `chunks/`, the decode of `manifest` (or `manifest.prev`) or one
    /// of its chunks checked, or the finish: write the manifest, rotate,
    /// and collect. `true` once finished.
    ///
    /// # Errors
    /// As [`write`]; the previous checkpoint stays readable.
    pub fn step(
        &mut self,
        budget: Option<Duration>,
        at: &mut dyn FnMut(Stage) -> io::Result<()>,
    ) -> io::Result<bool> {
        let start = Instant::now();
        loop {
            if self.piece(at)? {
                return Ok(true);
            }
            if budget.is_some_and(|b| start.elapsed() >= b) {
                return Ok(false);
            }
        }
    }

    /// One piece of [`Writer::step`]; `true` once finished.
    fn piece(&mut self, at: &mut dyn FnMut(Stage) -> io::Result<()>) -> io::Result<bool> {
        let chunks = self.dir.join(CHUNKS);
        if let Some((name, page)) = self.unchecked.pop() {
            self.check_reused(name, page)?;
        } else if let Some((name, data)) = self.todo.pop() {
            let tmp = chunks.join(format!("{name}.tmp"));
            write_file(&tmp, &data, Flush::Data)?;
            at(Stage::ChunkTmp)?;
            fs::rename(tmp, chunks.join(&name))?;
            self.done.chunks_new += 1;
            self.done.bytes += data.len() as u64;
            self.known.chunks.insert(name);
        } else if !self.synced {
            // Only renames in `chunks/` need it; the manifest's full flush
            // orders the chunks' data before it.
            if self.done.chunks_new > 0 {
                sync_dir(&chunks)?;
            }
            self.synced = true;
            at(Stage::ChunksWritten)?;
        } else if !self.replace_prev && !matches!(self.rotation, Rotation::Decided(_)) {
            self.decide_rotation();
        } else {
            self.finish(at)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Read back `name`, on disk and named by the manifest, and keep it if
    /// it checks against `page`; else write it again from `page`'s data.
    fn check_reused(&mut self, name: String, mut page: snap::HistoryPage) -> io::Result<()> {
        self.done.chunks_checked += 1;
        let on_disk = fs::read(self.dir.join(CHUNKS).join(&name)).unwrap_or_default();
        let good = (!page.data.is_empty() && on_disk == page.data) || {
            let data = std::mem::replace(&mut page.data, on_disk);
            let good = page_checks(&page);
            page.data = data;
            good
        };
        if good {
            self.done.chunks_kept += 1;
            self.known.chunks.insert(name);
        } else if page.data.is_empty() {
            return Err(no_data(&name));
        } else {
            self.todo.push((name, page.data));
        }
        Ok(())
    }

    /// One piece of deciding [`Rotation`] (see [`Writer`]).
    fn decide_rotation(&mut self) {
        self.rotation = match std::mem::replace(&mut self.rotation, Rotation::Unknown) {
            Rotation::Unknown => {
                let latest = fs::read(self.dir.join(MANIFEST));
                if self
                    .known
                    .manifest
                    .as_ref()
                    .is_some_and(|k| latest.as_ref().is_ok_and(|b| b == k))
                {
                    Rotation::Decided(true)
                } else {
                    self.start_reading(MANIFEST, latest.ok())
                }
            }
            Rotation::Reading(name, mut pages) => match pages.pop() {
                None => self.read_through(name, true),
                Some(mut page) => {
                    let hex = snap::hex(&page.hash);
                    let inline = !page.data.is_empty();
                    if !inline {
                        self.done.chunks_checked += 1;
                        page.data = fs::read(self.dir.join(CHUNKS).join(&hex)).unwrap_or_default();
                    }
                    if page_checks(&page) {
                        if !inline {
                            self.known.chunks.insert(hex);
                        }
                        Rotation::Reading(name, pages)
                    } else {
                        self.read_through(name, false)
                    }
                }
            },
            decided @ Rotation::Decided(_) => decided,
        };
    }

    /// Start reading manifest `name` (`bytes`, if it could be read)
    /// through: decode it, and list its terminals' pages this process
    /// does not know (a copy mode's do not count, as for [`load_file`]).
    fn start_reading(&mut self, name: &'static str, bytes: Option<Vec<u8>>) -> Rotation {
        let decoded = bytes.and_then(|b| {
            std::panic::catch_unwind(|| {
                snap::decode_partial(&b, &snap::Features::default(), |_| None).ok()
            })
            .ok()
            .flatten()
        });
        let Some(s) = decoded else {
            return self.read_through(name, false);
        };
        let mut seen = BTreeSet::new();
        let mut pages: Vec<snap::HistoryPage> = s
            .term_pages()
            .into_iter()
            .filter(|p| !p.is_ghostty())
            .filter(|p| {
                let hex = snap::hex(&p.hash);
                !self.known.chunks.contains(&hex) && seen.insert(hex)
            })
            .cloned()
            .collect();
        pages.reverse();
        Rotation::Reading(name, pages)
    }

    /// Manifest `name` read through: whether it `reads`. `manifest` that
    /// reads rotates; one that does not leaves it to `manifest.prev`,
    /// which stays when it reads.
    fn read_through(&mut self, name: &'static str, reads: bool) -> Rotation {
        match (name, reads) {
            (MANIFEST, true) => Rotation::Decided(true),
            (MANIFEST, false) => {
                let prev = fs::read(self.dir.join(MANIFEST_PREV)).ok();
                self.start_reading(MANIFEST_PREV, prev)
            }
            (_, reads) => Rotation::Decided(!reads),
        }
    }

    /// Write the manifest, rotate, and collect.
    fn finish(&mut self, at: &mut dyn FnMut(Stage) -> io::Result<()>) -> io::Result<()> {
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
        } else if matches!(self.rotation, Rotation::Decided(true)) {
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
        self.finished = true;
        at(Stage::Renamed)?;
        sync_dir(dir)?;
        self.done.chunks_removed = collect(dir, &self.names, &mut self.known.chunks, at)?;
        Ok(())
    }
}

fn no_data(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("history page {name} has no data and no chunk that checks"),
    )
}

/// Whether `page`, its data its chunk's, checks as a restore checks it
/// ([`snap::Snapshot::unreadable`]): decompresses into its rows within the
/// page limits and matches its name. A panic reading it is a no.
fn page_checks(page: &snap::HistoryPage) -> bool {
    std::panic::catch_unwind(|| page.each_row_verified(|_| {}).is_ok()).unwrap_or(false)
}

/// Delete every entry of `chunks/` that neither manifest names: `names`
/// (the one just written) and those `manifest.prev` names, and forget it
/// in `known`. Nothing is deleted when `manifest.prev` exists but can't be
/// listed (unreadable, newer, or requiring a feature this build lacks): a
/// chunk it names may be among them. Errors deleting are left for the
/// next checkpoint; only `at`'s are returned.
fn collect(
    dir: &Path,
    names: &BTreeSet<String>,
    known: &mut BTreeSet<String>,
    at: &mut dyn FnMut(Stage) -> io::Result<()>,
) -> io::Result<usize> {
    let mut keep = names.clone();
    match fs::read(dir.join(MANIFEST_PREV)) {
        Ok(bytes) => match snap::page_refs(&bytes) {
            Ok(refs) => keep.extend(refs.iter().map(snap::hex)),
            Err(_) => return Ok(0),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Ok(0),
    }
    let Ok(entries) = fs::read_dir(dir.join(CHUNKS)) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_str();
        if name.is_some_and(|n| keep.contains(n)) {
            continue;
        }
        at(Stage::Collecting)?;
        if fs::remove_file(entry.path()).is_ok() {
            removed += 1;
            if let Some(n) = name {
                known.remove(n);
            }
        }
    }
    Ok(removed)
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
    read_file(dir, name).map(|(s, why, _)| (s, why))
}

/// [`load_file`], and what reading it taught ([`Known`]): every chunk the
/// snapshot names checks, and, for [`MANIFEST`], the file reads.
///
/// # Errors
/// As [`load_file`].
pub fn read_file(dir: &Path, name: &str) -> io::Result<(snap::Snapshot, Vec<String>, Known)> {
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
    let known = Known {
        manifest: (name == MANIFEST).then_some(bytes),
        chunks: s
            .pages()
            .iter()
            .filter(|p| !p.is_ghostty())
            .map(|p| snap::hex(&p.hash))
            .collect(),
    };
    Ok((s, why, known))
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

/// How long [`Lock::take`] waits for a lock another holds, and how long a
/// periodic checkpoint retries [`Lock::try_take`] before it says the
/// directory is in use.
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// The lock a server holds on its checkpoint directory while it writes
/// there ([`LOCK`], `flock`), so two servers never write one directory.
/// Released when dropped, or when the process ends however it ends.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Take the lock on `dir` (made private first), waiting at most
    /// [`LOCK_WAIT`]: long enough for `cargo xtask reap`'s lock to go (it
    /// takes it exclusive, without waiting, and holds it only while it
    /// empties that one dead directory), too short for a server that
    /// holds it to matter.
    ///
    /// # Errors
    /// `WouldBlock` while another holds it (another process, or another
    /// [`Lock`] in this one); else the directory or lock file failed.
    pub fn take(dir: &Path) -> io::Result<Self> {
        let file = Self::open(dir)?;
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(rustix::io::Errno::WOULDBLOCK) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// [`Lock::take`] without the wait: one attempt, for the event loop
    /// (a periodic checkpoint retries on a later turn).
    ///
    /// # Errors
    /// As [`Lock::take`].
    pub fn try_take(dir: &Path) -> io::Result<Self> {
        let file = Self::open(dir)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self { _file: file })
    }

    /// The lock file of `dir`, the directory made private first.
    fn open(dir: &Path) -> io::Result<File> {
        private_dir(dir)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(dir.join(LOCK))
    }
}

#[cfg(test)]
mod tests;
