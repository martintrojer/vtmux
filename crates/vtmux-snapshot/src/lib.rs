//! The vtmux snapshot format (D27, docs/specs/2026-11-06-live-upgrade.md):
//! the whole server state as one versioned stream, which a live upgrade
//! hands to the new binary. (`vtmux-persist` also wrote it to disk until
//! 2026-11-12; what only those checkpoints set is still read, never
//! written: [`Server::persist_hold`], [`Pane::restored_from`],
//! [`Pane::restart_from`], [`Purpose::CheckpointLayout`] and
//! [`Purpose::CheckpointFull`].)
//!
//! A stream is a header of [`HEADER_LEN`] bytes, then the [`Snapshot`]
//! record (the body):
//!
//! ```text
//! MAGIC (8) | version (u32 LE) | body length (u64 LE) | BLAKE3 of the body (32) | body
//! ```
//!
//! A reader refuses a stream whose body is shorter or longer than its
//! length or doesn't match its hash, so a short memory file or a torn
//! manifest is an error, never a smaller snapshot. Records are
//! length-prefixed tagged fields ([`wire`]); unknown tags are skipped. A
//! reader takes every version from [`MIN_VERSION`] to [`FORMAT_VERSION`]
//! and refuses a newer one (decision 6); an older stream is decoded with
//! its version in hand and then brought forward by the migration steps
//! (`migrate.rs`). Within a version, [`Meta::requires`] names what a
//! reader must understand to read the stream (below).
//!
//! Rows use the vtmux row format ([`row`]), not Ghostty's page memory
//! (decision 11). History pages are LZ4-compressed and named by the BLAKE3
//! hash of their rows' encoding (before compression, so the names don't
//! depend on the compressor), and [`encode`] writes them inline.
//!
//! The types here are the format's own, not the server's (decision 18).
//!
//! A decoder bounds what a stream with a valid hash but crafted contents
//! can cost: records nest at most [`wire::MAX_DEPTH`] deep, a decode builds
//! at most a budget of memory relative to its input
//! ([`wire::BUDGET_BASE`]), and a page holds at most [`row::MAX_PAGE_ROWS`]
//! rows, [`row::MAX_PAGE_CELLS`] cells and [`row::MAX_PAGE_BYTES`] bytes.
//!
//! # Changing the format
//!
//! - A field under a new tag that a reader may lose without changing what
//!   it restores (a hint it can work out again) needs nothing more: older
//!   readers of the version skip it.
//! - So does a promise the builds before it never kept, whose loss only
//!   puts a reader back to what those builds did ([`Server::persist_hold`]:
//!   a reader without it may write over a checkpoint it would keep).
//! - So does state an ordinary attached client produces whose loss costs
//!   no session state, only how its terminal is shown until it changes
//!   again ([`Client::queries`], [`Client::terminal_title`]): naming it
//!   in [`Meta::requires`] would make nearly every server refuse a
//!   downgrade to the build before (decision 6 is about the format
//!   version, not about a feature a fix adds).
//! - So does state an ordinary session produces when the writer also puts
//!   what an older reader takes for it in the fields that reader reads:
//!   [`TermState::sized_primary`] beside the primary screen at the
//!   terminal's size, [`TermState::alternate_saved`] beside a copy in the
//!   primary screen's DECSC slot ([`term::AltSaveLayout`]), and the
//!   history an alternate screen shows, read in place in `primary`,
//!   beside the copy [`term::AltEntry::history`] that rows carry. The
//!   older reader restores what its own build would have held.
//! - A field under a new tag that carries state (losing it would change a
//!   format, a screen or a `show-*`/`list-*` output after the restore)
//!   needs either a new [`FORMAT_VERSION`] or a feature name: the writer
//!   lists the name in [`Meta::requires`] whenever it writes the field, and
//!   the reader that understands it lists it in [`KNOWN_FEATURES`] (or
//!   [`Features`], for a name that depends on the binary). A reader refuses
//!   a stream that requires a name it doesn't know ([`Error::Unsupported`]),
//!   so the dry run (decision 6) fails and the old server keeps running.
//!   Decision 23's pages in Ghostty's own form are such a field:
//!   [`ghostty_pages_feature`].
//! - A new `oneof!` variant or `value_enum!` number ([`wire`]) is
//!   state by that rule, unless the enum names an `unknown` fallback
//!   ([`PaneMode::Closed`]).
//! - A new row flag (the row codec's flags, which readers of the version
//!   refuse when unknown) is such a field too. What an ordinary session
//!   produces goes beside the rows under a new tag instead, where an older
//!   reader skips it: tmux's line flags a row keeps where its cells no
//!   longer say so ([`Row::kept`], [`KeptRows`]: `echo café` and two
//!   backspaces in bash), and those a row lacks though its cells say so
//!   ([`Row::lacks`]: Ghostty's scroll in an RGB background with no
//!   history). The older reader derives them from the cells, as its own
//!   build held them.
//! - Anything else (a tag's meaning, the row codec, the header) bumps
//!   [`FORMAT_VERSION`] and adds a migration step and a golden file
//!   `tests/golden/v<N>.snap`.
//!
//! A golden file holds what its version wrote when it was released, and
//! is never edited after that. A field added within a version stays out
//! of the test sample the golden file pins (it gets its own round trip),
//! so the golden file keeps pinning every reader of the version.

#![forbid(unsafe_code)]

mod migrate;
pub mod model;
pub mod row;
pub mod term;
pub mod wire;

pub use model::*;
pub use row::{
    Cell, Color, Grapheme, Hash, HistoryPage, Hyperlink, KeptLineFlags, KeptRows, Osc133,
    PageBuilder, Row, RowPrompt, Semantic, Style, Tab, Underline, Wide,
};
pub use term::TermState;

use wire::{Ctx, record};

/// The first bytes of every snapshot.
pub const MAGIC: &[u8; 8] = b"VTMUXSNP";

/// The version this build writes.
pub const FORMAT_VERSION: u32 = 1;

/// The oldest version this build reads.
pub const MIN_VERSION: u32 = 1;

/// Length of a stream's header: magic, version, body length, body hash.
pub const HEADER_LEN: usize = 8 + 4 + 8 + 32;

/// The [`Meta::requires`] names every reader of this build understands.
/// It keeps the names no writer lists any more, for the streams of the
/// builds that did.
pub const KNOWN_FEATURES: &[&str] = &[
    CURSOR_VERY_VISIBLE_FEATURE,
    GRAPHICS_GRANT_FEATURE,
    INBAND_RESIZE_FEATURE,
    SCROLLBAR_FEATURE,
    TERM_QUERIES_FEATURE,
    TERMINAL_TITLE_FEATURE,
];

/// The [`Meta::requires`] name of a pane terminal's very visible cursor
/// ([`term::CursorStyle::very_visible`]), listed when some terminal's is
/// set.
pub const CURSOR_VERY_VISIBLE_FEATURE: &str = "cursor-very-visible";

/// The [`Meta::requires`] name of a kitty graphics grant (D33,
/// [`TermState::graphics_primary`], [`TermState::graphics_alternate`]),
/// listed when some terminal's program holds one. Required: a build
/// without D33 would take the granted program's next graphics command as
/// a title and answer none of them. An ordinary session can't produce it
/// (`vtmux-graphics` is off by default), so a downgrade is refused only
/// while a granted program runs.
pub const GRAPHICS_GRANT_FEATURE: &str = "graphics-grant";

/// The [`Meta::requires`] name of a mode 2048 grant (D31,
/// [`TermState::inband_resize`]), listed when some terminal's program
/// holds one. Required, though a reader that skips the field reads the
/// rest: a granted program (neovim) stops handling `SIGWINCH`, so a build
/// that drops the grant leaves it drawing at the old size, where tmux
/// would never have granted it. An ordinary session can't produce it
/// (`vtmux-inband-resize` is off by default), so the downgrade is refused
/// only while a program relies on it.
pub const INBAND_RESIZE_FEATURE: &str = "inband-resize-grant";

/// The [`Meta::requires`] name of a pane's auto-hide scrollbar state
/// ([`Pane::scrollbar_visible`] and the fields after it), listed when
/// some pane's is not the default (hidden, no timer).
pub const SCROLLBAR_FEATURE: &str = "scrollbar-auto-hide";

/// The [`Meta::requires`] name of where a client terminal's queries stand
/// ([`Client::queries`]), which builds from 69aa4cc to 91b8f49 listed
/// when some client's was written. No longer listed: a reader may drop
/// the field (*Changing the format*).
pub const TERM_QUERIES_FEATURE: &str = "client-term-queries";

/// The [`Meta::requires`] name of the title a client's terminal shows
/// ([`Client::terminal_title`]), which builds from 471193e to 91b8f49
/// listed when some client's was written. No longer listed, like
/// [`TERM_QUERIES_FEATURE`].
pub const TERMINAL_TITLE_FEATURE: &str = "client-terminal-title";

/// The [`Meta::requires`] name of history pages in Ghostty's own
/// compressed form ([`HistoryPage::ghostty`]), which only a binary with
/// the same Ghostty patch stack (`patches`, its hash) can read (decision
/// 23). A writer lists it whenever it writes such a page, so that a reader
/// that can't take them refuses the stream rather than lose the history
/// they hold.
#[must_use]
pub fn ghostty_pages_feature(patches: &str) -> String {
    format!("ghostty-pages-{patches}")
}

/// What a reader understands beyond [`KNOWN_FEATURES`], from the binary it
/// runs in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Features {
    /// The reading binary's Ghostty patch-stack hash: it understands
    /// [`ghostty_pages_feature`] of that hash.
    pub ghostty_patches: Option<String>,
}

impl Features {
    /// Whether a stream that requires `name` can be read.
    #[must_use]
    pub fn understands(&self, name: &str) -> bool {
        KNOWN_FEATURES.contains(&name)
            || self
                .ghostty_patches
                .as_deref()
                .is_some_and(|h| name == ghostty_pages_feature(h))
    }
}

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
    /// The stream ends inside a value, or before the length its header
    /// gives.
    Truncated,
    /// The body doesn't match the hash in the header.
    Corrupt,
    /// The stream requires a feature this reader doesn't understand
    /// ([`Meta::requires`]).
    Unsupported(String),
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
            Self::Corrupt => f.write_str("snapshot is corrupt: its hash does not match"),
            Self::Unsupported(name) => {
                write!(f, "snapshot requires `{name}`, which this binary lacks")
            }
            Self::Malformed(what) => write!(f, "malformed snapshot: {what}"),
            Self::MissingChunk(h) => write!(f, "history chunk {} missing or corrupt", hex(h)),
            Self::Inconsistent(what) => write!(f, "inconsistent snapshot: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Whether the stream is refused for having been written by a newer
    /// vtmux: a newer format, or a feature this build doesn't know. Such
    /// a stream is fine, only not for this binary, so a reader must leave
    /// it as it is (decision 6), unlike a corrupt or truncated one.
    #[must_use]
    pub fn from_newer_binary(&self) -> bool {
        matches!(self, Self::Newer { .. } | Self::Unsupported(_))
    }
}

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
        /// `vtmux-persist layout` (3.8-beta3 and before): no terminal
        /// state. Never written since 2026-11-12; `--vtmux-check-snapshot`
        /// refuses it.
        1 => CheckpointLayout,
        /// `vtmux-persist full` (3.8-beta3 and before), likewise.
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
        /// Features a reader must understand to read the stream, beyond
        /// its version ([`Features::understands`]; the crate docs,
        /// *Changing the format*).
        6 => many requires: Vec<String>,
        /// Upgrade only: the descriptor the writer's executable is open
        /// on across `execve`, where a failed resume switches back to
        /// whatever its path holds by then (decision 7). A hint: a reader
        /// that skips it switches back by [`Meta::binary`].
        7 => opt binary_fd: Option<i32>,
        /// Upgrade only: who asked for the upgrade and waits to hear how
        /// it went, answered by the image that ends up serving (the new
        /// one, the one switched back to, or the last resort). A hint: a
        /// reader that skips it restores the same server and leaves the
        /// requester unanswered.
        8 => opt requester: Option<Requester>,
    }
}

wire::oneof! {
    /// Who asked for a live upgrade ([`Meta::requester`]).
    pub enum Requester {
        /// The frozen `UpgradeRequest` on this descriptor, kept across
        /// `execve` and in no descriptor table: answered with
        /// `UpgradeResult`, then closed.
        1 => Handshake(i32),
        /// `vtmux-upgrade` run for this client, carried like any other: a
        /// command client is told to exit (with the failure on stderr), a
        /// control client or an attached one is told of a failure.
        2 => Client(u32),
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
    let mut out = Vec::with_capacity(4096);
    out.resize(HEADER_LEN, 0);
    wire::Value::put(s, &mut out);
    seal(&mut out);
    out
}

/// Fill in the header of `out`, whose body follows [`HEADER_LEN`] bytes
/// of room for it.
fn seal(out: &mut [u8]) {
    let (head, body) = out.split_at_mut(HEADER_LEN);
    head[..8].copy_from_slice(MAGIC);
    head[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    head[12..20].copy_from_slice(&(body.len() as u64).to_le_bytes());
    head[20..].copy_from_slice(blake3::hash(body).as_bytes());
}

/// `body` behind its header.
#[cfg(test)]
fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = vec![0; HEADER_LEN];
    out.extend_from_slice(body);
    seal(&mut out);
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

/// The BLAKE3 hash of a stream's body, as its header records it, without
/// checking it: `None` when `bytes` do not start with a header. A live
/// upgrade's dry run that has read and rebuilt the stream answers with it
/// (`sys::upgrade::CHECKED` in vtmux-server), and the image asking
/// compares it with the one of the stream it wrote.
#[must_use]
pub fn digest(bytes: &[u8]) -> Option<[u8; 32]> {
    let head = bytes.get(..HEADER_LEN)?;
    (&head[..8] == MAGIC).then(|| head[20..].try_into().expect("32 bytes"))
}

/// The body of a stream of a version this build reads, checked against
/// the length and hash in its header.
fn body(bytes: &[u8]) -> Result<(u32, &[u8]), Error> {
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
    let head = bytes.get(..HEADER_LEN).ok_or(Error::Truncated)?;
    let len = u64::from_le_bytes(head[12..20].try_into().expect("8 bytes"));
    let body = &bytes[HEADER_LEN..];
    match (body.len() as u64).cmp(&len) {
        std::cmp::Ordering::Less => return Err(Error::Truncated),
        std::cmp::Ordering::Greater => {
            return Err(Error::Malformed("bytes after the snapshot"));
        }
        std::cmp::Ordering::Equal => {}
    }
    if blake3::hash(body).as_bytes() != &head[20..HEADER_LEN] {
        return Err(Error::Corrupt);
    }
    Ok((found, body))
}

/// Decode a stream. A stream that requires a feature beyond
/// [`KNOWN_FEATURES`] is refused.
pub fn decode(bytes: &[u8]) -> Result<Snapshot, Error> {
    decode_for(bytes, &Features::default())
}

/// [`decode`] for a reader that understands `features` too. Every page
/// is inline: one in the vtmux row format without its bytes (a
/// `vtmux-persist` manifest of 3.8-beta3 and before, which named them
/// only) is [`Error::MissingChunk`].
pub fn decode_for(bytes: &[u8], features: &Features) -> Result<Snapshot, Error> {
    let (found, body) = body(bytes)?;
    let cx = Ctx::for_input(found, body.len());
    check_requires(body, &cx, features)?;
    let mut s = <Snapshot as wire::Value>::get(body, &cx)?;
    let mut missing = None;
    s.pages_mut(&mut |p| {
        if missing.is_none() && p.data.is_empty() && !p.is_ghostty() {
            missing = Some(p.hash);
        }
    });
    if let Some(h) = missing {
        return Err(Error::MissingChunk(h));
    }
    migrate::migrate(&mut s, found)?;
    Ok(s)
}

/// A part of a [`Snapshot`] a restore can do without and still serve
/// every session, window and pane: what [`Snapshot::without`] drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Part {
    /// Pane `id`'s terminal state (its screens and history).
    PaneTerm(u32),
    /// Pane `id`'s copy or view mode (which `kind`), the `n`th of its
    /// modes.
    PaneMode(u32, usize, ModeKind),
    /// The terminal state of client `id`'s popup.
    PopupTerm(u32),
}

/// Which of the modes holding a frozen grid a [`Part::PaneMode`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModeKind {
    /// [`PaneMode::Copy`].
    Copy,
    /// [`PaneMode::View`].
    View,
}

impl std::fmt::Display for ModeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Copy => "copy mode",
            Self::View => "view mode",
        })
    }
}

impl std::fmt::Display for Part {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PaneTerm(id) => write!(f, "%{id}'s terminal state"),
            Self::PaneMode(id, _, kind) => write!(f, "%{id}'s {kind}"),
            Self::PopupTerm(id) => write!(f, "client {id}'s popup terminal state"),
        }
    }
}

/// Refuse a body that requires a feature beyond `features`. What the
/// stream requires is known before anything that depends on it is decoded.
fn check_requires(body: &[u8], cx: &Ctx, features: &Features) -> Result<(), Error> {
    let mut r = wire::Reader::new(body);
    while let Some((tag, field)) = r.field()? {
        if tag == 1 {
            let meta = <Meta as wire::Value>::get(field, cx)?;
            if let Some(name) = meta.requires.iter().find(|n| !features.understands(n)) {
                return Err(Error::Unsupported(name.clone()));
            }
        }
    }
    Ok(())
}

impl Snapshot {
    /// Every history page, wherever it is: pane screens, the history frozen
    /// for an alternate screen, copy and view modes, popups. Pages in
    /// Ghostty's form ([`HistoryPage::is_ghostty`]) have no chunk.
    #[must_use]
    pub fn pages(&self) -> Vec<&HistoryPage> {
        self.pages_of(true)
    }

    /// The pages of every pane's and popup's terminal state:
    /// [`Snapshot::pages`] without those of copy and view modes, so the
    /// ones a restore can't do without ([`Snapshot::unreadable`]'s
    /// [`Part::PaneTerm`] and [`Part::PopupTerm`]).
    #[must_use]
    pub fn term_pages(&self) -> Vec<&HistoryPage> {
        self.pages_of(false)
    }

    fn pages_of(&self, modes: bool) -> Vec<&HistoryPage> {
        fn term<'a>(t: &'a TermState, out: &mut Vec<&'a HistoryPage>) {
            let sized = t.sized_primary.as_ref().and_then(|p| p.screen.as_ref());
            for screen in std::iter::once(&t.primary).chain(sized).chain(&t.alternate) {
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
            for m in p.modes.iter().filter(|_| modes) {
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
            let sized = t.sized_primary.as_mut().and_then(|p| p.screen.as_mut());
            for screen in std::iter::once(&mut t.primary)
                .chain(sized)
                .chain(&mut t.alternate)
            {
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

    /// The parts holding a history page that does not decompress into
    /// its rows within the page limits or does not match its name, in
    /// the order [`Snapshot::pages`] goes, each with its first bad page's
    /// error. A page in Ghostty's form is covered by the stream's hash;
    /// only Ghostty can read further into it. A page equal to one already
    /// read is not read again: the copy of the history an alternate screen
    /// shows ([`term::AltEntry::history`]) is mostly the primary's pages.
    #[must_use]
    pub fn unreadable(&self) -> Vec<(Part, Error)> {
        type Read<'a> = std::collections::HashMap<Hash, &'a HistoryPage>;
        fn first_bad<'a>(
            read: &mut Read<'a>,
            pages: impl IntoIterator<Item = &'a HistoryPage>,
        ) -> Option<Error> {
            for p in pages {
                if p.is_ghostty() || read.get(&p.hash).is_some_and(|q| *q == p) {
                    continue;
                }
                if let Err(e) = p.each_row_verified(|_| {}) {
                    return Some(e);
                }
                read.insert(p.hash, p);
            }
            None
        }
        fn term<'a>(read: &mut Read<'a>, t: &'a TermState) -> Option<Error> {
            let sized = t.sized_primary.as_ref().and_then(|p| p.screen.as_ref());
            let screens = std::iter::once(&t.primary).chain(sized).chain(&t.alternate);
            first_bad(
                read,
                screens
                    .flat_map(|s| &s.history)
                    .chain(t.alt_entry.iter().flat_map(|a| &a.history)),
            )
        }
        let mut read = Read::new();
        let mut out = Vec::new();
        for p in &self.panes {
            if let Some(e) = p.term.as_ref().and_then(|t| term(&mut read, t)) {
                out.push((Part::PaneTerm(p.id), e));
            }
            for (n, m) in p.modes.iter().enumerate() {
                let (c, kind) = match m {
                    PaneMode::Copy(c) => (c, ModeKind::Copy),
                    PaneMode::View(c) => (c, ModeKind::View),
                    PaneMode::Clock(_) | PaneMode::Closed(()) => continue,
                };
                if let Some(e) = first_bad(&mut read, &c.history) {
                    out.push((Part::PaneMode(p.id, n, kind), e));
                }
            }
        }
        for c in &self.clients {
            if let Some(e) = c
                .popup
                .as_ref()
                .and_then(|p| p.term.as_ref())
                .and_then(|t| term(&mut read, t))
            {
                out.push((Part::PopupTerm(c.id), e));
            }
        }
        out
    }

    /// Drop `parts` ([`Snapshot::unreadable`]'s): a pane's or popup's
    /// terminal state goes (the pane comes back with a blank terminal of
    /// its size, as from a `layout` checkpoint), a mode goes from its
    /// pane's modes, the rest of the pane staying as it was.
    pub fn without(&mut self, parts: &[Part]) {
        let mut parts = parts.to_vec();
        // Modes by index, the last first, so each index still holds.
        parts.sort_unstable();
        for part in parts.iter().rev() {
            match *part {
                Part::PaneTerm(id) => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == id) {
                        p.term = None;
                    }
                }
                Part::PaneMode(id, n, _) => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == id)
                        && n < p.modes.len()
                    {
                        p.modes.remove(n);
                    }
                }
                Part::PopupTerm(id) => {
                    if let Some(popup) = self
                        .clients
                        .iter_mut()
                        .find(|c| c.id == id)
                        .and_then(|c| c.popup.as_mut())
                    {
                        popup.term = None;
                    }
                }
            }
        }
    }

    /// Everything the dry run (`--vtmux-check-snapshot`) verifies beyond
    /// decoding: every page decompresses into its rows within the page
    /// limits and matches its name ([`Snapshot::unreadable`]), and the
    /// records refer to each other consistently.
    pub fn check(&self) -> Result<(), Error> {
        if let Some((_, e)) = self.unreadable().into_iter().next() {
            return Err(e);
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
