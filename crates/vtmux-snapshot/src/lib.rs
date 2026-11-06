//! The vtmux snapshot format (D27, docs/specs/2026-11-06-live-upgrade.md):
//! the whole server state as one versioned stream, which a live upgrade
//! hands to the new binary and persistence writes to disk.
//!
//! A stream is [`MAGIC`], the format version as a little-endian `u32`, then
//! the [`Snapshot`] record. Records are length-prefixed tagged fields
//! ([`wire`]); unknown tags are skipped. A reader takes every version from
//! [`MIN_VERSION`] to [`FORMAT_VERSION`] and refuses a newer one (decision
//! 6); an older stream is decoded with its version in hand and then
//! brought forward by the migration steps (`migrate.rs`).
//!
//! Rows use the vtmux row format ([`row`]), not Ghostty's page memory
//! (decision 11). History pages are LZ4-compressed and named by the BLAKE3
//! hash of their compressed bytes: [`encode`] writes them inline (the
//! upgrade stream), [`encode_manifest`] writes only their names for a
//! checkpoint whose pages live in `chunks/<hash>` (decision 20), and
//! [`decode_with`] reads them back from there.
//!
//! The types here are the format's own, not the server's (decision 18).
//!
//! # Changing the format
//!
//! Adding a field under a new tag is compatible within a version. Anything
//! else (a tag's meaning, the row codec) bumps [`FORMAT_VERSION`], adds a
//! migration step and a golden file `tests/golden/v<N>.snap`; earlier
//! golden files are never edited once released.

#![forbid(unsafe_code)]

mod migrate;
pub mod model;
pub mod row;
pub mod term;
pub mod wire;

pub use model::*;
pub use row::{
    Cell, Color, Grapheme, Hash, HistoryPage, Hyperlink, Osc133, PageBuilder, Row, RowPrompt,
    Semantic, Style, Tab, Underline, Wide,
};
pub use term::TermState;

use wire::{Ctx, record};

/// The first bytes of every snapshot.
pub const MAGIC: &[u8; 8] = b"VTMUXSNP";

/// The version this build writes.
pub const FORMAT_VERSION: u32 = 1;

/// The oldest version this build reads.
pub const MIN_VERSION: u32 = 1;

/// Why a snapshot can't be read.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a snapshot.
    BadMagic,
    /// Written by a newer vtmux (decision 6: older refuses newer).
    Newer {
        found: u32,
        supported: u32,
    },
    /// Older than any version this build can migrate.
    TooOld {
        found: u32,
        oldest: u32,
    },
    /// The stream ends inside a value.
    Truncated,
    Malformed(&'static str),
    /// A history page whose bytes are missing or don't match its hash.
    MissingChunk(Hash),
    /// The records don't describe a consistent server.
    Inconsistent(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => f.write_str("not a vtmux snapshot"),
            Self::Newer { found, supported } => write!(
                f,
                "snapshot format {found} is newer than this binary's {supported}"
            ),
            Self::TooOld { found, oldest } => write!(
                f,
                "snapshot format {found} is older than the oldest supported, {oldest}"
            ),
            Self::Truncated => f.write_str("snapshot is truncated"),
            Self::Malformed(what) => write!(f, "malformed snapshot: {what}"),
            Self::MissingChunk(h) => write!(f, "history chunk {} missing or corrupt", hex(h)),
            Self::Inconsistent(what) => write!(f, "inconsistent snapshot: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// A hash as the lowercase hex a chunk file is named by.
#[must_use]
pub fn hex(h: &Hash) -> String {
    use std::fmt::Write;
    h.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

wire::value_enum! {
    /// Why the snapshot was written.
    pub enum Purpose {
        0 => Upgrade,
        /// `vtmux-persist layout`: no terminal state.
        1 => CheckpointLayout,
        /// `vtmux-persist full`.
        2 => CheckpointFull,
    }
}

record! {
    /// Who wrote the snapshot and why.
    pub struct Meta {
        1 => one purpose: Purpose,
        /// The writer's `#{version}`.
        2 => one writer_version: String,
        /// The writer's executable: where a failed resume switches back to
        /// (decision 7).
        3 => one binary: String,
        /// The writer's Ghostty patch-stack hash (decision 23).
        4 => one ghostty_patches: String,
        /// Microseconds since the epoch.
        5 => one written_us: i64,
    }
}

record! {
    /// The whole server state.
    pub struct Snapshot {
        1 => one meta: Meta,
        2 => one server: Server,
        3 => many sessions: Vec<Session>,
        4 => many windows: Vec<Window>,
        5 => many panes: Vec<Pane>,
        6 => many clients: Vec<Client>,
        /// Descriptors kept open across `execve` (upgrade only).
        7 => many descriptors: Vec<Descriptor>,
    }
}

/// The stream for a live upgrade: every history page inline.
#[must_use]
pub fn encode(s: &Snapshot) -> Vec<u8> {
    let mut out = header();
    wire::Value::put(s, &mut out);
    out
}

/// A checkpoint manifest: history pages by name only. The caller stores
/// each page of [`Snapshot::pages`] as `chunks/<hex(hash)>` (its `data`).
#[must_use]
pub fn encode_manifest(s: &Snapshot) -> Vec<u8> {
    let mut by_ref = s.clone();
    by_ref.pages_mut(&mut |p| p.data = Vec::new());
    let mut out = header();
    wire::Value::put(&by_ref, &mut out);
    out
}

fn header() -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out
}

/// The format version of a stream, without decoding it.
pub fn version(bytes: &[u8]) -> Result<u32, Error> {
    let head = bytes.get(..12).ok_or(Error::Truncated)?;
    if &head[..8] != MAGIC {
        return Err(Error::BadMagic);
    }
    Ok(u32::from_le_bytes(head[8..12].try_into().expect("4 bytes")))
}

/// Decode a stream whose history pages are inline.
pub fn decode(bytes: &[u8]) -> Result<Snapshot, Error> {
    decode_with(bytes, |_| None)
}

/// Decode a stream, fetching each page that is not inline from `chunk`
/// (by its hash). Every page is checked against its hash.
pub fn decode_with(
    bytes: &[u8],
    mut chunk: impl FnMut(&Hash) -> Option<Vec<u8>>,
) -> Result<Snapshot, Error> {
    let found = version(bytes)?;
    if found > FORMAT_VERSION {
        return Err(Error::Newer {
            found,
            supported: FORMAT_VERSION,
        });
    }
    if found < MIN_VERSION {
        return Err(Error::TooOld {
            found,
            oldest: MIN_VERSION,
        });
    }
    let cx = Ctx { version: found };
    let mut s = <Snapshot as wire::Value>::get(&bytes[12..], &cx)?;
    let mut missing = None;
    s.pages_mut(&mut |p| {
        if missing.is_some() {
            return;
        }
        if p.data.is_empty() {
            p.data = chunk(&p.hash).unwrap_or_default();
        }
        if !p.verified() {
            missing = Some(p.hash);
        }
    });
    if let Some(h) = missing {
        return Err(Error::MissingChunk(h));
    }
    migrate::migrate(&mut s, found)?;
    Ok(s)
}

impl Snapshot {
    /// Every history page, wherever it is: pane screens, the history frozen
    /// for an alternate screen, copy and view modes, popups.
    #[must_use]
    pub fn pages(&self) -> Vec<&HistoryPage> {
        fn term<'a>(t: &'a TermState, out: &mut Vec<&'a HistoryPage>) {
            for screen in std::iter::once(&t.primary).chain(&t.alternate) {
                out.extend(&screen.history);
            }
            if let Some(a) = &t.alt_entry {
                out.extend(&a.history);
            }
        }
        let mut out = Vec::new();
        for p in &self.panes {
            if let Some(t) = &p.term {
                term(t, &mut out);
            }
            for m in &p.modes {
                if let PaneMode::Copy(c) | PaneMode::View(c) = m {
                    out.extend(&c.history);
                }
            }
        }
        for c in &self.clients {
            if let Some(t) = c.popup.as_ref().and_then(|p| p.term.as_ref()) {
                term(t, &mut out);
            }
        }
        out
    }

    /// [`Snapshot::pages`], mutably.
    pub fn pages_mut(&mut self, f: &mut impl FnMut(&mut HistoryPage)) {
        fn term(t: &mut TermState, f: &mut impl FnMut(&mut HistoryPage)) {
            for screen in std::iter::once(&mut t.primary).chain(&mut t.alternate) {
                screen.history.iter_mut().for_each(&mut *f);
            }
            if let Some(a) = &mut t.alt_entry {
                a.history.iter_mut().for_each(&mut *f);
            }
        }
        for p in &mut self.panes {
            if let Some(t) = &mut p.term {
                term(t, f);
            }
            for m in &mut p.modes {
                if let PaneMode::Copy(c) | PaneMode::View(c) = m {
                    c.history.iter_mut().for_each(&mut *f);
                }
            }
        }
        for c in &mut self.clients {
            if let Some(t) = c.popup.as_mut().and_then(|p| p.term.as_mut()) {
                term(t, f);
            }
        }
    }

    /// Everything the dry run (`--vtmux-check-snapshot`) verifies beyond
    /// decoding: every page decompresses into its rows, and the records
    /// refer to each other consistently.
    pub fn check(&self) -> Result<(), Error> {
        for p in self.pages() {
            if !p.verified() {
                return Err(Error::MissingChunk(p.hash));
            }
            p.each_row(|_| {})?;
        }
        let bad = |what: String| Err(Error::Inconsistent(what));
        let windows: std::collections::BTreeMap<u32, &Window> =
            self.windows.iter().map(|w| (w.id, w)).collect();
        let panes: std::collections::BTreeMap<u32, &Pane> =
            self.panes.iter().map(|p| (p.id, p)).collect();
        let sessions: std::collections::BTreeSet<u32> =
            self.sessions.iter().map(|s| s.id).collect();
        if windows.len() != self.windows.len()
            || panes.len() != self.panes.len()
            || sessions.len() != self.sessions.len()
        {
            return bad("duplicate id".into());
        }
        for s in &self.sessions {
            for l in &s.windows {
                if !windows.contains_key(&l.window) {
                    return bad(format!("${} links missing @{}", s.id, l.window));
                }
            }
            if !s.windows.is_empty() && !s.windows.iter().any(|l| l.index == s.current) {
                return bad(format!("${} current window {} not linked", s.id, s.current));
            }
        }
        for w in &self.windows {
            let mut leaves = Vec::new();
            leaves_of(&w.layout.root, &mut leaves);
            for &p in w.panes.iter().chain(&leaves) {
                match panes.get(&p) {
                    Some(pane) if pane.window == w.id => {}
                    _ => return bad(format!("@{} names %{p}, not its pane", w.id)),
                }
            }
            if w.active.is_some_and(|a| !w.panes.contains(&a)) {
                return bad(format!("@{} active pane not in it", w.id));
            }
        }
        for p in &self.panes {
            if !windows
                .get(&p.window)
                .is_some_and(|w| w.panes.contains(&p.id))
            {
                return bad(format!("%{} not in its window @{}", p.id, p.window));
            }
        }
        for c in &self.clients {
            if c.session.is_some_and(|s| !sessions.contains(&s)) {
                return bad(format!("client {} attached to a missing session", c.id));
            }
        }
        Ok(())
    }
}

fn leaves_of(cell: &LayoutCell, out: &mut Vec<u32>) {
    match cell {
        LayoutCell::Pane(l) | LayoutCell::Floating(l) => out.push(l.pane),
        LayoutCell::Node(n) => n.children.iter().for_each(|c| leaves_of(c, out)),
    }
}

#[cfg(test)]
mod tests;
