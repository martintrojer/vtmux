//! The server's model to and from the snapshot format (D27 phase 1,
//! docs/specs/2026-11-06-live-upgrade.md): [`Server::to_snapshot`] and
//! [`Server::from_snapshot`]. A model rebuilt from a snapshot answers every
//! `list-*` and `show-*` command and expands every format as the model it
//! came from did.
//!
//! Pane and popup terminal state (`Pane.term`, `Popup.term`) goes as
//! `Meta.purpose` says ([`Server::to_snapshot`]): rows for a full
//! checkpoint, rows or Ghostty's compressed pages for an upgrade, none for
//! a layout checkpoint, after which [`restore_term`] gives a pane a fresh
//! terminal of its size, history limit and title.
//!
//! What a snapshot leaves out, by design:
//!
//! - Modes: copy and clock mode are carried; view mode (its backing terminal
//!   is terminal state), the tree modes (`choose-tree`, `choose-buffer`,
//!   `choose-client`, `customize-mode`, `switch-mode`), `display-panes`,
//!   prompts (status line, pane and mode prompts), menus and popups are
//!   closed by the upgrade and re-opened by the user (spec, *Open
//!   questions*). The editor a tree mode's `e` opened stays a modal pane,
//!   but its edit goes with the mode (as `spawn_cancel_editor`) and its
//!   temporary file stays behind.
//! - Work in flight: command queues, jobs and file operations. The upgrade
//!   preflight refuses while any is pending (decisions 10 and 26).
//! - Caches and timers the loop rebuilds: compositor state, frames not yet
//!   acknowledged, `#()` job output, pane queries waiting on a terminal.
//! - Not yet carried: bytes still queued for a client socket the quiesce
//!   couldn't flush (the upgrade's quiesce must flush them or carry them,
//!   phase 2).
//!
//! Carried runtime state that is easy to miss: each pane's size changes
//! the program hasn't been told yet (`wp->resize_queue`), output a
//! `pipe-pane` command hasn't taken yet, each client's
//! `terminal-features`/`terminal-overrides` as its terminal opened with
//! them, the status-line message and its timer, `lock-after-time`'s and
//! `automatic-rename`'s timers, each pane's auto-hide scrollbar (shown,
//! hovered, its hide timer), where each client terminal's queries stand
//! (answered, and the start timer that gives up on them), the title each
//! client's terminal shows from its output, and config
//! errors waiting for a client.
//!
//! [`Server::from_snapshot`] refuses (an error, never a panic) a snapshot
//! that fails [`snap::Snapshot::check`] or the server's own checks
//! (`validate`: id counters, active panes, sizes, winlink trees).

pub(crate) mod rows;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener as StdListener;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mio::net::UnixStream;
use mio::{Interest, Token};
use vtmux_core::ids::{IdAllocator, PaneId, SessionId, WindowId};
use vtmux_core::layout::{self, NamedLayout, PaneStatus};
use vtmux_core::options::{self, ArrayKey, OptionValue, OptionsStore, StoreParts, TreeId, Value};
use vtmux_snapshot as snap;
use vtmux_vt::PaneTerm;

use super::{CLIENT_BASE, Client, ClientId, PANE_BASE, Server, ServerConfig, empty_pane_modes};
use crate::environ::Environ;
use crate::format_ctx::ServerInfo;
use crate::model::{Pane, Session, Window, Zoom};
use crate::pane_io::PaneIo;
use crate::snapshot_term::{Form, ghostty_pages_requirement, term_from_snapshot, term_to_snapshot};

/// A reply of a client terminal that changed its features, kept so the
/// terminal opened again after a restore has them too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TermReply {
    Sync,
    Da(Vec<u32>),
    Da2(u32),
    Xtversion(String),
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn until(now: Instant, t: Instant) -> u64 {
    ms(t.saturating_duration_since(now))
}

fn since(now: Instant, t: Instant) -> u64 {
    ms(now.saturating_duration_since(t))
}

fn after(now: Instant, ms: u64) -> Instant {
    now + Duration::from_millis(ms)
}

fn before(now: Instant, ms: u64) -> Instant {
    now.checked_sub(Duration::from_millis(ms)).unwrap_or(now)
}

/// How long ago an upgrade's snapshot `s` was written (`written_us`, the
/// wall clock), at most [`taken_limit`]; nothing for one with no time or a
/// checkpoint, whose timers a restore does not carry on.
fn since_taken(s: &snap::Snapshot) -> Duration {
    let meta = &s.meta;
    if meta.purpose != snap::Purpose::Upgrade || meta.written_us <= 0 {
        return Duration::ZERO;
    }
    let written = u64::try_from(meta.written_us).unwrap_or(0);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| now.checked_sub(Duration::from_micros(written)))
        .unwrap_or_default()
        .min(taken_limit(s))
}

/// More than the time from writing `s` to rebuilding from it can take, so
/// a clock that stepped meanwhile moves no timer further. Between them
/// are the encode, the notices' flush ([`super::upgrade::FLUSH_WAIT`]),
/// the exec and the new image's decode and import, which is the dry run's
/// work: the dry run's limit for `s` (3 s and a second per 100k rows it
/// imports as rows) twice, once for the encode, and the flush.
/// (`review_downgrade_fix`: a flat 10 s, "more than an upgrade takes", fell
/// short of a snapshot with much history in rows.)
fn taken_limit(s: &snap::Snapshot) -> Duration {
    2 * super::resume::dry_run_limit(s) + super::upgrade::FLUSH_WAIT
}

fn small(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

// ------------------------------------------------------------- options

/// A key by its name, or, for a key whose name reads back as another key
/// (`C-?` is both 0x7f and C-? with the control bit), as `0x` and its
/// code in tmux's numbering (`key_code`).
fn key_name_out(k: vtmux_core::keys::Key) -> String {
    let name = k.name(false);
    if vtmux_core::keys::Key::from_name(&name) == Some(k) {
        name
    } else {
        format!("0x{:x}", k.code())
    }
}

fn key_name_in(name: &str) -> Option<vtmux_core::keys::Key> {
    match name.strip_prefix("0x") {
        Some(hex) if !hex.is_empty() => u64::from_str_radix(hex, 16)
            .ok()
            .map(vtmux_core::keys::Key::from_code),
        _ => vtmux_core::keys::Key::from_name(name),
    }
}

fn value_out(def: Option<&options::OptionDef>, v: &Value) -> snap::Value {
    match v {
        Value::String(s) => snap::Value::String(s.clone()),
        Value::Number(n) => snap::Value::Number(*n),
        Value::Key(k) => snap::Value::Key(key_name_out(*k)),
        Value::Colour(c) => snap::Value::Colour(c.to_string()),
        Value::Flag(b) => snap::Value::Flag(*b),
        Value::Choice(_) => snap::Value::Choice(v.to_text(def, false)),
        Value::Command(list) => {
            snap::Value::Command(list.print(vtmux_core::cmd::PrintFlags::default()))
        }
    }
}

fn value_in(def: Option<&options::OptionDef>, v: &snap::Value) -> Option<Value> {
    Some(match v {
        snap::Value::String(s) => Value::String(s.clone()),
        snap::Value::Number(n) => Value::Number(*n),
        snap::Value::Key(k) => Value::Key(key_name_in(k)?),
        snap::Value::Colour(c) => Value::Colour(vtmux_core::colour::Colour::parse_option(c).ok()?),
        snap::Value::Flag(b) => Value::Flag(*b),
        snap::Value::Choice(name) => match def?.kind {
            options::Kind::Choice(names) => Value::Choice(names.iter().position(|n| n == name)?),
            _ => return None,
        },
        snap::Value::Command(text) => Value::Command(command_in(text)?),
    })
}

/// A command list printed by `cmd_list_print`, parsed again.
fn command_in(text: &str) -> Option<vtmux_core::cmd::CommandList> {
    vtmux_core::cmd::parse_string(text, &mut vtmux_core::cmd::NoEnv).ok()
}

fn key_out(k: &ArrayKey) -> snap::ArrayKey {
    match k {
        ArrayKey::Index(i) => snap::ArrayKey::Index(*i),
        ArrayKey::Name(n) => snap::ArrayKey::Name(n.clone()),
    }
}

fn key_in(k: &snap::ArrayKey) -> ArrayKey {
    match k {
        snap::ArrayKey::Index(i) => ArrayKey::Index(*i),
        snap::ArrayKey::Name(n) => ArrayKey::Name(n.clone()),
    }
}

fn options_out(entries: &[(String, OptionValue)]) -> Vec<snap::OptionEntry> {
    entries
        .iter()
        .map(|(name, value)| {
            let def = options::def_of(name);
            snap::OptionEntry {
                name: name.clone(),
                value: match value {
                    OptionValue::Single(v) => snap::OptionValue::Single(value_out(def, v)),
                    OptionValue::Array(items) => snap::OptionValue::Array(
                        items
                            .iter()
                            .map(|(k, v)| snap::ArrayItem {
                                key: key_out(k),
                                value: value_out(def, v),
                            })
                            .collect(),
                    ),
                },
            }
        })
        .collect()
}

/// The entries of one tree. An entry whose value this build can't read
/// (an option whose type changed) is dropped: the tree's parent, or the
/// default, shows through.
fn options_in(entries: &[snap::OptionEntry]) -> Vec<(String, OptionValue)> {
    entries
        .iter()
        .filter_map(|e| {
            let def = options::def_of(&e.name);
            let value = match &e.value {
                snap::OptionValue::Single(v) => OptionValue::Single(value_in(def, v)?),
                snap::OptionValue::Array(items) => OptionValue::Array(
                    items
                        .iter()
                        .map(|i| Some((key_in(&i.key), value_in(def, &i.value)?)))
                        .collect::<Option<_>>()?,
                ),
            };
            Some((e.name.clone(), value))
        })
        .collect()
}

fn scope_out(t: TreeId) -> snap::Scope {
    match t {
        TreeId::Server => snap::Scope::Server(()),
        TreeId::GlobalSession => snap::Scope::GlobalSession(()),
        TreeId::GlobalWindow => snap::Scope::GlobalWindow(()),
        TreeId::Session(s) => snap::Scope::Session(s.0),
        TreeId::Window(w) => snap::Scope::Window(w.0),
        TreeId::Pane(p) => snap::Scope::Pane(p.0),
    }
}

fn scope_in(s: &snap::Scope) -> TreeId {
    match s {
        snap::Scope::Server(()) => TreeId::Server,
        snap::Scope::GlobalSession(()) => TreeId::GlobalSession,
        snap::Scope::GlobalWindow(()) => TreeId::GlobalWindow,
        snap::Scope::Session(s) => TreeId::Session(SessionId(*s)),
        snap::Scope::Window(w) => TreeId::Window(WindowId(*w)),
        snap::Scope::Pane(p) => TreeId::Pane(PaneId(*p)),
    }
}

fn env_out(env: &Environ) -> Vec<snap::EnvVar> {
    env.iter()
        .map(|(name, e)| snap::EnvVar {
            name: name.to_owned(),
            value: e.value.clone(),
            hidden: e.hidden,
        })
        .collect()
}

fn env_in(vars: &[snap::EnvVar]) -> Environ {
    let mut env = Environ::default();
    for v in vars {
        env.put(
            &v.name,
            crate::environ::Entry {
                value: v.value.clone(),
                hidden: v.hidden,
            },
        );
    }
    env
}

fn binding_out(b: &vtmux_core::keys::Binding) -> snap::Binding {
    snap::Binding {
        key: key_name_out(b.key),
        note: b.note.clone(),
        repeat: b.repeat,
        command: b.commands.print(vtmux_core::cmd::PrintFlags::default()),
    }
}

fn binding_in(b: &snap::Binding) -> Option<vtmux_core::keys::Binding> {
    Some(vtmux_core::keys::Binding {
        key: key_name_in(&b.key)?.without_flags(),
        note: b.note.clone(),
        repeat: b.repeat,
        commands: command_in(&b.command)?,
    })
}

fn subparts_out(p: &crate::control_subs::SubParts) -> (Option<String>, Vec<snap::SubValue>) {
    (
        p.last.clone(),
        p.each
            .iter()
            .map(|((id, index), value)| snap::SubValue {
                id: *id,
                index: *index,
                value: value.clone(),
            })
            .collect(),
    )
}

fn each_in(each: &[snap::SubValue]) -> Vec<((u32, u32), String)> {
    each.iter()
        .map(|v| ((v.id, v.index), v.value.clone()))
        .collect()
}

// -------------------------------------------------------------- layouts

fn geometry_out(g: &layout::Geometry) -> snap::Geometry {
    snap::Geometry {
        sx: g.sx,
        sy: g.sy,
        xoff: g.xoff,
        yoff: g.yoff,
    }
}

fn geometry_in(g: &snap::Geometry) -> layout::Geometry {
    layout::Geometry::new(g.sx, g.sy, g.xoff, g.yoff)
}

fn leaf_out(l: &layout::Leaf) -> snap::Leaf {
    snap::Leaf {
        pane: l.pane.0,
        g: geometry_out(&l.g),
    }
}

fn leaf_in(l: &snap::Leaf) -> layout::Leaf {
    layout::Leaf {
        pane: PaneId(l.pane),
        g: geometry_in(&l.g),
    }
}

fn cell_out(c: &layout::Cell) -> snap::LayoutCell {
    match c {
        layout::Cell::Pane(l) => snap::LayoutCell::Pane(leaf_out(l)),
        layout::Cell::Floating(l) => snap::LayoutCell::Floating(leaf_out(l)),
        layout::Cell::Node(n) => snap::LayoutCell::Node(snap::Node {
            dir: match n.dir {
                layout::Dir::LeftRight => snap::Dir::LeftRight,
                layout::Dir::TopBottom => snap::Dir::TopBottom,
            },
            g: geometry_out(&n.g),
            children: n.children.iter().map(cell_out).collect(),
        }),
    }
}

fn cell_in(c: &snap::LayoutCell) -> layout::Cell {
    match c {
        snap::LayoutCell::Pane(l) => layout::Cell::Pane(leaf_in(l)),
        snap::LayoutCell::Floating(l) => layout::Cell::Floating(leaf_in(l)),
        snap::LayoutCell::Node(n) => layout::Cell::Node(layout::Node {
            dir: match n.dir {
                snap::Dir::LeftRight => layout::Dir::LeftRight,
                snap::Dir::TopBottom => layout::Dir::TopBottom,
            },
            g: geometry_in(&n.g),
            children: n.children.iter().map(cell_in).collect(),
        }),
    }
}

fn layout_out(l: &layout::Layout) -> snap::Layout {
    snap::Layout {
        sx: l.sx,
        sy: l.sy,
        root: cell_out(&l.root),
        pane_status: match l.opts.status {
            PaneStatus::Off => 0,
            PaneStatus::Top => 1,
            PaneStatus::Bottom => 2,
        },
        scrollbar_always: l.opts.scrollbar_always,
        last_new_x: l.last_new.0,
        last_new_y: l.last_new.1,
        saved_float: l
            .saved_float
            .iter()
            .map(|(p, g)| snap::Leaf {
                pane: p.0,
                g: geometry_out(g),
            })
            .collect(),
        borderless: l.borderless.iter().map(|p| p.0).collect(),
    }
}

fn layout_in(l: &snap::Layout) -> layout::Layout {
    layout::Layout {
        sx: l.sx,
        sy: l.sy,
        root: cell_in(&l.root),
        opts: layout::Opts {
            status: match l.pane_status {
                1 => PaneStatus::Top,
                2 => PaneStatus::Bottom,
                _ => PaneStatus::Off,
            },
            scrollbar_always: l.scrollbar_always,
        },
        last_new: (l.last_new_x, l.last_new_y),
        saved_float: l
            .saved_float
            .iter()
            .map(|f| (PaneId(f.pane), geometry_in(&f.g)))
            .collect(),
        borderless: l.borderless.iter().map(|&p| PaneId(p)).collect(),
    }
}

// --------------------------------------------------------------- panes

fn status_out(s: crate::jobs::JobStatus) -> snap::ExitStatus {
    match s {
        crate::jobs::JobStatus::Exited(code) => snap::ExitStatus {
            code: Some(code),
            ..snap::ExitStatus::default()
        },
        crate::jobs::JobStatus::Signaled(sig) => snap::ExitStatus {
            signal: Some(sig),
            ..snap::ExitStatus::default()
        },
        crate::jobs::JobStatus::Unknown => snap::ExitStatus::default(),
    }
}

/// Neither a code nor a signal is a status not known (a restored pane's).
fn status_in(s: &snap::ExitStatus) -> crate::jobs::JobStatus {
    match (s.code, s.signal) {
        (_, Some(sig)) => crate::jobs::JobStatus::Signaled(sig),
        (Some(code), None) => crate::jobs::JobStatus::Exited(code),
        (None, None) => crate::jobs::JobStatus::Unknown,
    }
}

/// The pane's terminal from the snapshot: `p.term` imported
/// ([`term_from_snapshot`], pages in Ghostty's form read only if
/// `ghostty_patches`, the writer's stack, is this binary's), which must be
/// the pane's size. Without one (a `vtmux-persist layout` checkpoint) a
/// fresh terminal the pane's size, with its history limit, title and an
/// empty pane's modes, made as `spawn_pane` makes one.
pub(crate) fn restore_term(
    server: &Server,
    p: &snap::Pane,
    ghostty_patches: &str,
) -> io::Result<PaneTerm> {
    let cols = u16::try_from(p.geometry.sx).unwrap_or(u16::MAX).max(1);
    let rows = u16::try_from(p.geometry.sy).unwrap_or(u16::MAX).max(1);
    if let Some(t) = &p.term {
        let mut term = term_from_snapshot(t, ghostty_patches)
            .map_err(|e| invalid(format!("%{}: {e}", p.id)))?;
        if (term.cols(), term.rows()) != (cols, rows) {
            return Err(invalid(format!("%{}: terminal not the pane's size", p.id)));
        }
        // The colour-name table is the server's, not terminal state; so
        // is the link counter.
        term.set_colour_names(vtmux_core::colour::x11_rgb);
        term.number_links_from(&server.link_numbers);
        return Ok(term);
    }
    fresh_term(server, p)
}

/// The terminal [`restore_term`] gives a pane without terminal state.
fn fresh_term(server: &Server, p: &snap::Pane) -> io::Result<PaneTerm> {
    let cols = u16::try_from(p.geometry.sx).unwrap_or(u16::MAX).max(1);
    let rows = u16::try_from(p.geometry.sy).unwrap_or(u16::MAX).max(1);
    let mut term = PaneTerm::try_new(
        cols,
        rows,
        usize::try_from(p.history_limit).unwrap_or(usize::MAX),
    )
    .ok_or_else(|| invalid(format!("%{}: no terminal of {cols}x{rows}", p.id)))?;
    term.set_colour_names(vtmux_core::colour::x11_rgb);
    term.number_links_from(&server.link_numbers);
    term.set_extended_keys(server.model.extended_keys());
    term.reset_key_mode();
    if p.empty {
        empty_pane_modes(&mut term);
    }
    // p.title is what `PaneTerm::title` read, cleaned already.
    term.restore_title(&p.title);
    term.take_events();
    Ok(term)
}

/// A popup's terminal: `p.term` imported, as [`restore_term`] does (its
/// size may lag the popup's, as `popup_resize` leaves it); without one a
/// fresh terminal of the job's size with no history, as `popup_display`
/// makes one.
fn restore_popup_term(
    server: &Server,
    popup: &crate::popup::Popup,
    p: &snap::Popup,
    ghostty_patches: &str,
) -> io::Result<PaneTerm> {
    if let Some(t) = &p.term {
        let mut term =
            term_from_snapshot(t, ghostty_patches).map_err(|e| invalid(format!("popup: {e}")))?;
        term.number_links_from(&server.link_numbers);
        return Ok(term);
    }
    fresh_popup_term(server, popup)
}

/// The terminal [`restore_popup_term`] gives a popup without terminal
/// state.
fn fresh_popup_term(server: &Server, popup: &crate::popup::Popup) -> io::Result<PaneTerm> {
    let (jx, jy) = popup.inner();
    let mut term = PaneTerm::try_new(jx.max(1), jy.max(1), 0)
        .ok_or_else(|| invalid(format!("no popup terminal of {jx}x{jy}")))?;
    term.number_links_from(&server.link_numbers);
    term.set_extended_keys(server.model.extended_keys());
    term.reset_key_mode();
    Ok(term)
}

fn invalid(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// tmux's `WINDOW_MAXIMUM` (tmux.h): no window, pane or popup is larger.
const SIZE_MAX: u32 = 10_000;

/// What [`snap::Snapshot::check`] leaves to the server: the id counters
/// are past every id in use (or ids would be handed out twice and a new
/// window or pane would replace an old one), every session has a window
/// and a unique name and index per window, every window has an active
/// pane, sizes are within `WINDOW_MAXIMUM`, every session's winlink tree
/// is a red-black tree of its window indexes, and client ids are unique.
pub(super) fn validate(s: &snap::Snapshot) -> io::Result<()> {
    s.check().map_err(|e| invalid(e.to_string()))?;
    let past = |what: &str, next: u32, ids: &mut dyn Iterator<Item = u32>| match ids.max() {
        Some(max) if next <= max => Err(invalid(format!("next {what} id {next} not past {max}"))),
        _ => Ok(()),
    };
    let sv = &s.server;
    past(
        "session",
        sv.next_session,
        &mut s.sessions.iter().map(|x| x.id),
    )?;
    past(
        "window",
        sv.next_window,
        &mut s.windows.iter().map(|x| x.id),
    )?;
    past("pane", sv.next_pane, &mut s.panes.iter().map(|x| x.id))?;
    past(
        "client",
        sv.next_client,
        &mut s.clients.iter().map(|x| x.id),
    )?;
    let mut names = BTreeSet::new();
    for x in &s.sessions {
        if !names.insert(x.name.as_str()) {
            return Err(invalid(format!("duplicate session name {}", x.name)));
        }
        let indexes: BTreeSet<u32> = x.windows.iter().map(|l| l.index).collect();
        if x.windows.is_empty() || indexes.len() != x.windows.len() {
            return Err(invalid(format!("${}: no or duplicate winlinks", x.id)));
        }
        let shape: Vec<(u32, bool)> = x.link_tree.iter().map(|n| (n.key, n.red)).collect();
        if !shape.is_empty()
            && crate::model::rb::Tree::from_shape(&shape)
                .is_none_or(|t| t.keys().into_iter().ne(indexes.iter().copied()))
        {
            return Err(invalid(format!("${}: winlink tree", x.id)));
        }
    }
    for w in &s.windows {
        if w.active.is_none() {
            return Err(invalid(format!("@{} has no active pane", w.id)));
        }
    }
    for p in &s.panes {
        if p.geometry.sx > SIZE_MAX || p.geometry.sy > SIZE_MAX {
            return Err(invalid(format!("%{} too large", p.id)));
        }
    }
    let mut clients = BTreeSet::new();
    for c in &s.clients {
        if !clients.insert(c.id) {
            return Err(invalid(format!("duplicate client {}", c.id)));
        }
        if let Some(p) = &c.popup
            && (p.sx > SIZE_MAX || p.sy > SIZE_MAX)
        {
            return Err(invalid(format!("client {} popup too large", c.id)));
        }
    }
    Ok(())
}

/// A pane mode as carried, `None` for one the snapshot closes. Without
/// `screens` (a `vtmux-persist layout` checkpoint) copy mode is closed
/// too: its frozen grid is the pane's screen.
fn mode_out(m: &crate::mode::Mode, now: Instant, screens: bool) -> Option<snap::PaneMode> {
    use crate::mode::Mode;
    match m {
        Mode::Copy(c) if screens => Some(snap::PaneMode::Copy(c.export(now))),
        Mode::Clock(c) => Some(snap::PaneMode::Clock(crate::mode::copy::key_mode_out(
            c.key_mode,
        ))),
        _ => None,
    }
}

fn mode_in(
    m: &snap::PaneMode,
    sx: usize,
    sy: usize,
    now: Instant,
    pane: &vtmux_vt::PaneTerm,
) -> Option<crate::mode::Mode> {
    use crate::mode::Mode;
    match m {
        snap::PaneMode::Copy(c) => Some(Mode::Copy(Box::new(
            crate::mode::copy::CopyMode::import(c, now, pane).ok()?,
        ))),
        snap::PaneMode::Clock(k) => {
            let mut clock = crate::mode::clock::ClockMode::new(sx, sy);
            clock.key_mode = crate::mode::copy::key_mode_in(*k);
            Some(Mode::Clock(clock))
        }
        snap::PaneMode::View(_) | snap::PaneMode::Closed(()) => None,
    }
}

// ------------------------------------------------------------- clients

fn rgb_out((r, g, b): vtmux_vt::Rgb) -> snap::Rgb {
    snap::Rgb { r, g, b }
}

fn rgb_in(c: &snap::Rgb) -> vtmux_vt::Rgb {
    (c.r, c.g, c.b)
}

fn flags_out(f: &crate::client_flags::ClientFlags) -> Vec<String> {
    let mut out = Vec::new();
    for (on, name) in [
        (f.read_only, "read-only"),
        (f.ignore_size, "ignore-size"),
        (f.no_detach_on_destroy, "no-detach-on-destroy"),
        (f.no_output, "no-output"),
        (f.wait_exit, "wait-exit"),
        (f.new_layouts, "new-layouts"),
        (f.size_changed, "size-changed"),
        (f.window_size_changed, "window-size-changed"),
    ] {
        if on {
            out.push(name.to_owned());
        }
    }
    if let Some(age) = f.pause_after {
        out.push(format!("pause-after-ms={age}"));
    }
    out
}

fn flags_in(names: &[String]) -> crate::client_flags::ClientFlags {
    let mut f = crate::client_flags::ClientFlags::default();
    for n in names {
        match n.as_str() {
            "read-only" => f.read_only = true,
            "ignore-size" => f.ignore_size = true,
            "no-detach-on-destroy" => f.no_detach_on_destroy = true,
            "no-output" => f.no_output = true,
            "wait-exit" => f.wait_exit = true,
            "new-layouts" => f.new_layouts = true,
            "size-changed" => f.size_changed = true,
            "window-size-changed" => f.window_size_changed = true,
            other => {
                if let Some(age) = other
                    .strip_prefix("pause-after-ms=")
                    .and_then(|a| a.parse().ok())
                {
                    f.pause_after = Some(age);
                }
            }
        }
    }
    f
}

fn reply_out(r: &TermReply) -> snap::TermReply {
    match r {
        TermReply::Sync => snap::TermReply::default(),
        TermReply::Da(params) => snap::TermReply {
            kind: 1,
            params: params.clone(),
            ..snap::TermReply::default()
        },
        TermReply::Da2(first) => snap::TermReply {
            kind: 2,
            params: vec![*first],
            ..snap::TermReply::default()
        },
        TermReply::Xtversion(text) => snap::TermReply {
            kind: 3,
            text: text.clone(),
            ..snap::TermReply::default()
        },
    }
}

fn reply_in(r: &snap::TermReply) -> Option<TermReply> {
    Some(match r.kind {
        0 => TermReply::Sync,
        1 => TermReply::Da(r.params.clone()),
        2 => TermReply::Da2(*r.params.first()?),
        3 => TermReply::Xtversion(r.text.clone()),
        _ => return None,
    })
}

fn control_out(
    cs: &crate::control::ControlState,
    streams: &crate::control_queue::Streams,
    level: u32,
    now: Instant,
) -> snap::ControlClient {
    use crate::control::PaneOutput;
    use crate::control_queue::Held;
    let p = cs.parts(streams);
    let (subs, due) = p.subs.parts();
    snap::ControlClient {
        level,
        subscriptions: subs
            .iter()
            .map(|(name, s)| {
                let (last, each) = subparts_out(s);
                snap::Subscription {
                    name: name.clone(),
                    target: s.what.to_string(),
                    format: s.format.clone(),
                    last,
                    each,
                }
            })
            .collect(),
        pause_after: None,
        panes: p
            .panes
            .iter()
            .map(|(pane, o)| snap::ControlPane {
                pane: pane.0,
                output: match o {
                    PaneOutput::Off => snap::PaneOutput::Off,
                    PaneOutput::Paused => snap::PaneOutput::Paused,
                },
                offset: 0,
            })
            .collect(),
        window_sizes: p
            .windows
            .iter()
            .map(|(w, (sx, sy))| snap::WindowSize {
                window: w.0,
                sx: *sx,
                sy: *sy,
            })
            .collect(),
        size: p.size,
        retval: p.retval,
        queued: p
            .held
            .iter()
            .map(|h| match h {
                Held::Line(line) => snap::ControlQueued {
                    pane: None,
                    data: line.clone(),
                    age_ms: 0,
                },
                Held::Output { pane, data, read } => snap::ControlQueued {
                    pane: Some(pane.0),
                    data: data.clone(),
                    age_ms: since(now, *read),
                },
            })
            .collect(),
        ready: p.ready,
        eof: p.eof,
        inbuf: p.inbuf,
        guard_depth: p.guard_depth,
        subs_due_ms: due.map(|d| until(now, d)),
    }
}

fn control_in(
    c: &snap::ControlClient,
    streams: &mut crate::control_queue::Streams,
    now: Instant,
) -> crate::control::ControlState {
    use crate::control::{ControlParts, PaneOutput};
    use crate::control_queue::Held;
    let subs = c
        .subscriptions
        .iter()
        .filter_map(|s| {
            let text = format!("{}:{}:{}", s.name, s.target, s.format);
            let (_, what, _) = vtmux_core::options::monitor::parse_monitor(&text)?;
            Some((
                s.name.clone(),
                crate::control_subs::SubParts {
                    what,
                    format: s.format.clone(),
                    last: s.last.clone(),
                    each: each_in(&s.each),
                },
            ))
        })
        .collect();
    let parts = ControlParts {
        inbuf: c.inbuf.clone(),
        ready: c.ready,
        eof: c.eof,
        guard_depth: c.guard_depth,
        retval: c.retval,
        panes: c
            .panes
            .iter()
            .filter_map(|p| {
                let o = match p.output {
                    snap::PaneOutput::On => return None,
                    snap::PaneOutput::Off => PaneOutput::Off,
                    snap::PaneOutput::Paused => PaneOutput::Paused,
                };
                Some((PaneId(p.pane), o))
            })
            .collect(),
        windows: c
            .window_sizes
            .iter()
            .map(|w| (WindowId(w.window), (w.sx, w.sy)))
            .collect(),
        size: c.size,
        subs: crate::control_subs::Subs::from_parts(subs, c.subs_due_ms.map(|m| after(now, m))),
        held: c
            .queued
            .iter()
            .map(|q| match q.pane {
                None => Held::Line(q.data.clone()),
                Some(p) => Held::Output {
                    pane: PaneId(p),
                    data: q.data.clone(),
                    read: before(now, q.age_ms),
                },
            })
            .collect(),
    };
    crate::control::ControlState::from_parts(parts, streams)
}

/// Each descriptor's role, for a caller that hands them back by role.
pub type Adopt<'a> = dyn FnMut(&snap::Role) -> Option<OwnedFd> + 'a;

impl Server {
    /// The whole server as a [`snap::Snapshot`] (D27). `meta` says who
    /// writes it and why; the descriptor table lists the listening socket,
    /// every client socket, pane PTY and `pipe-pane` descriptor by their
    /// numbers now, for a live upgrade to keep open across `execve`.
    /// Terminal state goes as `meta.purpose` says: an upgrade keeps pages
    /// Ghostty holds compressed in its form ([`Form::SamePin`], decision
    /// 23), a full checkpoint writes rows, a layout checkpoint none (see
    /// [`Server::to_snapshot_with`]).
    #[must_use]
    pub fn to_snapshot(&self, meta: snap::Meta) -> snap::Snapshot {
        let form = match meta.purpose {
            snap::Purpose::Upgrade => Some(Form::SamePin),
            snap::Purpose::CheckpointFull => Some(Form::Rows),
            snap::Purpose::CheckpointLayout => None,
        };
        self.to_snapshot_with(meta, form)
    }

    /// [`Server::to_snapshot`] with every pane's and popup's terminal in
    /// `form`, or none. `meta.ghostty_patches` becomes this binary's stack
    /// and, when a page went in Ghostty's form, `meta.requires` names
    /// [`ghostty_pages_requirement`], so a reader of another stack refuses
    /// the stream instead of losing that history: an upgrade to such a
    /// binary writes [`Form::Rows`] (decision 23; `Server::exec_upgrade`
    /// asks the binary its stack first).
    ///
    /// Each terminal's per-turn events (replies, bells, title changes)
    /// must have been drained: the end of a loop turn.
    #[must_use]
    #[expect(clippy::too_many_lines, reason = "one record after another")]
    pub fn to_snapshot_with(&self, mut meta: snap::Meta, form: Option<Form>) -> snap::Snapshot {
        let now = Instant::now();
        // Whether copy mode's frozen screen is carried: not in a `layout`
        // checkpoint, which holds no screen at all (D27 decision 17).
        let screens = meta.purpose != snap::Purpose::CheckpointLayout;
        let parts = self.model.options.export();
        let mut trees: BTreeMap<(u8, u32), Vec<snap::OptionEntry>> = BTreeMap::new();
        for (tree, entries) in &parts.trees {
            let key = match tree {
                TreeId::Server => (0, 0),
                TreeId::GlobalSession => (1, 0),
                TreeId::GlobalWindow => (2, 0),
                TreeId::Session(s) => (3, s.0),
                TreeId::Window(w) => (4, w.0),
                TreeId::Pane(p) => (5, p.0),
            };
            trees.insert(key, options_out(entries));
        }
        let mut tree = |k: (u8, u32)| trees.remove(&k).unwrap_or_default();
        let monitor_sets: BTreeMap<u64, (crate::control_subs::SubParts, Instant)> = self
            .hook_monitors
            .parts()
            .into_iter()
            .map(|(serial, p, due)| (serial, (p, due)))
            .collect();
        let (messages, message_next) = self.cmdq.message_log();
        let (buffers, buffer_next_index, buffer_next_order) = self.model.paste.parts();
        let (activity, active_points, link_stamp) = self.model.counters();
        let lists = self.prompt_history.lists();
        let marked = self.model.marked.and_then(|m| {
            Some(snap::Marked {
                session: m.session?.0,
                window_index: m.index?,
                window: m.window?.0,
                pane: m.pane?.0,
            })
        });
        let server = snap::Server {
            pid: i32::try_from(self.info.pid).unwrap_or(0),
            socket_path: self.info.socket_path.clone(),
            start_time_us: self.info.start_secs.saturating_mul(1_000_000),
            start_binary: self
                .start_binary
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            persist_hold: self.persist.kept.hold(),
            link_number: self.link_numbers.next(),
            server_options: tree((0, 0)),
            session_options: tree((1, 0)),
            window_options: tree((2, 0)),
            environment: env_out(&self.global_env),
            hook_events: parts.hook_events.clone(),
            hook_fires: parts
                .hook_fires
                .iter()
                .map(|(t, name, count, time)| snap::HookFire {
                    scope: scope_out(*t),
                    name: name.clone(),
                    count: *count,
                    time: *time,
                })
                .collect(),
            monitors: parts
                .monitors
                .iter()
                .map(|(t, name, m)| {
                    let set = monitor_sets.get(&m.serial);
                    let (last, each) = set.map(|(p, _)| subparts_out(p)).unwrap_or_default();
                    snap::Monitor {
                        scope: scope_out(*t),
                        name: name.clone(),
                        what: m.what.to_string(),
                        format: m.format.clone(),
                        notify_true: m.notify_true,
                        session: m.session.map(|s| s.0),
                        serial: m.serial,
                        fire_count: m.fires.0,
                        fire_time: m.fires.1,
                        last,
                        each,
                        due_in_ms: set.map(|(_, due)| until(now, *due)),
                    }
                })
                .collect(),
            monitor_serial: parts.monitor_serial,
            key_tables: self
                .key_bindings
                .tables()
                .map(|(name, t)| snap::KeyTable {
                    name: name.to_owned(),
                    bindings: t.bindings().map(binding_out).collect(),
                    defaults: t.defaults().map(binding_out).collect(),
                })
                .collect(),
            buffers: buffers
                .iter()
                .map(|b| snap::PasteBuffer {
                    name: b.name.clone(),
                    data: b.data.clone(),
                    automatic: b.automatic,
                    order: b.order,
                    created: b.created,
                })
                .collect(),
            buffer_next_index,
            buffer_next_order,
            messages: messages
                .iter()
                .map(|m| snap::Message {
                    number: m.number,
                    time_us: m.time.saturating_mul(1_000_000),
                    text: m.text.clone(),
                })
                .collect(),
            message_next,
            next_session: self.model.session_ids.peek().0,
            next_window: self.model.window_ids.peek().0,
            next_pane: self.model.pane_ids.peek().0,
            next_client: small(self.next_client),
            command_history: lists[0].clone(),
            search_history: lists[1].clone(),
            wait_channels: self
                .waits
                .parts()
                .into_iter()
                .map(|c| snap::WaitChannel {
                    name: c.name,
                    locked: c.locked,
                    woken: c.woken,
                    waiters: c.waiters.iter().map(|c| small(c.0)).collect(),
                    lockers: c.lockers.iter().map(|c| small(c.0)).collect(),
                })
                .collect(),
            marked,
            default_term: parts.term.clone(),
            activity,
            cmd_number: self.cmd_number,
            active_points,
            link_stamp,
            config_files: self
                .config
                .config_files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            config_causes: self.causes.clone(),
            marked_elsewhere: self.model.marked_link_elsewhere.map(|s| s.0),
            access: self
                .acl
                .entries()
                .map(|(s, read_only)| match s {
                    crate::acl::Subject::User(id) => snap::Access {
                        group: false,
                        id,
                        read_only,
                    },
                    crate::acl::Subject::Group(id) => snap::Access {
                        group: true,
                        id,
                        read_only,
                    },
                })
                .collect(),
        };

        // Group members in their group's order first: their order is
        // the group's order again.
        let mut order: Vec<SessionId> = Vec::new();
        for (_, members) in self.model.groups.all() {
            order.extend(
                members
                    .iter()
                    .filter(|s| self.model.sessions.contains_key(s)),
            );
        }
        let grouped: BTreeSet<SessionId> = order.iter().copied().collect();
        order.extend(
            self.model
                .sessions
                .keys()
                .filter(|s| !grouped.contains(s))
                .copied(),
        );
        let sessions = order
            .iter()
            .map(|id| {
                let s = &self.model.sessions[id];
                snap::Session {
                    id: s.id.0,
                    name: s.name.clone(),
                    created: s.created,
                    activity: s.activity,
                    activity_secs: s.activity_secs,
                    cwd: s.cwd.to_string_lossy().into_owned(),
                    windows: s
                        .windows
                        .iter()
                        .map(|(&index, w)| snap::Winlink {
                            index,
                            window: w.0,
                            alerts: u32::from(s.link_alerts.get(&index).copied().unwrap_or(0)),
                            seq: s.link_seq.get(&index).copied().unwrap_or(0),
                        })
                        .collect(),
                    current: s.current,
                    last: s.last.clone(),
                    environment: env_out(&s.env),
                    last_attached: s.last_attached,
                    options: tree((3, s.id.0)),
                    group: self.model.groups.name_of(s.id).map(str::to_owned),
                    link_tree: s
                        .link_tree
                        .shape()
                        .into_iter()
                        .map(|(key, red)| snap::RbNode { key, red })
                        .collect(),
                    lock_in_ms: self.lock_timers.get(&s.id).map(|&t| until(now, t)),
                }
            })
            .collect();

        let windows = self
            .model
            .windows
            .values()
            .map(|w| snap::Window {
                id: w.id.0,
                name: w.name.clone(),
                activity: w.activity,
                activity_secs: w.activity_secs,
                layout: layout_out(&w.layout),
                panes: w.panes.panes.iter().map(|p| p.0).collect(),
                active: w.panes.active.map(|p| p.0),
                last: w.panes.last.iter().map(|p| p.0).collect(),
                z_order: w.panes.z_order.iter().map(|p| p.0).collect(),
                base_index: w.panes.base_index,
                modal: w.panes.modal.map(|p| p.0),
                modal_last: w.panes.modal_last.map(|p| p.0),
                zoom: w.zoom.as_ref().map(|z| snap::Zoom {
                    pane: z.pane.0,
                    saved: layout_out(&z.saved),
                }),
                was_zoomed: w.was_zoomed.map(|p| p.0),
                last_layout: w.last_layout.map(|l| l.name().to_owned()),
                old_layout: w.old_layout.clone(),
                pending_size: w.pending_size,
                manual_size: w.manual_size,
                latest_client: w.latest.map(|c| small(c.0)),
                alerts: u32::from(w.alerts.flags),
                silence_in_ms: w.alerts.silence_deadline().map(|d| until(now, d)),
                options: tree((4, w.id.0)),
            })
            .collect();

        let mut descriptors = Vec::new();
        if let Some(l) = &self.listener {
            descriptors.push(snap::Descriptor {
                fd: l.as_raw_fd(),
                role: snap::Role::Listener(()),
            });
        }
        let panes: Vec<snap::Pane> = self
            .model
            .panes
            .values()
            .map(|p| {
                if let Some(io) = &p.io {
                    descriptors.push(snap::Descriptor {
                        fd: io.fd().as_raw_fd(),
                        role: snap::Role::PanePty(p.id.0),
                    });
                }
                if let Some(pipe) = &p.pipe {
                    descriptors.push(snap::Descriptor {
                        fd: pipe.raw_fd(),
                        role: snap::Role::PipePane(p.id.0),
                    });
                }
                snap::Pane {
                    id: p.id.0,
                    window: p.window.0,
                    pid: p.pid,
                    tty: p.tty.clone(),
                    argv: p.argv.clone(),
                    shell: p.shell.to_string_lossy().into_owned(),
                    cwd: p.cwd.to_string_lossy().into_owned(),
                    environment: Vec::new(),
                    empty: p.empty,
                    exited: p.exited,
                    status: p.status.map(status_out),
                    dead_time: p.dead_time,
                    output_generation: p.output_generation,
                    kitty_owner_primary: p.kitty_owner[0],
                    kitty_owner_alternate: p.kitty_owner[1],
                    last_prompt_time: p.shell_commands.last_prompt,
                    command_start_time: p.shell_commands.started,
                    command_end_time: p.shell_commands.ended,
                    command_running: p.shell_commands.running,
                    command_status: p.shell_commands.status,
                    changed: p.changed,
                    unseen_changes: p.unseen_changes,
                    geometry: geometry_out(&p.rect),
                    history_limit: p.history_limit as u64,
                    input_off: p.input_off,
                    focused: p.focused,
                    active_point: p.active_point,
                    options: tree((5, p.id.0)),
                    modes: p
                        .modes
                        .iter()
                        .filter_map(|m| mode_out(m, now, screens))
                        .collect(),
                    search: p.search.clone(),
                    pipe: p.pipe.as_ref().map(|pipe| snap::PipePane {
                        pid: pipe.pid,
                        command: String::new(),
                        input: pipe.input(),
                        output: pipe.output(),
                        pending: pipe.pending().to_vec(),
                    }),
                    term: form.map(|f| term_to_snapshot(&p.term, f)),
                    over_zoom: p.over_zoom,
                    capture_keys: p.capture_keys,
                    close_on_click: p.close_on_click,
                    close_on_cancel: p.close_on_cancel,
                    title: p.term.title(),
                    output_ago_ms: p.output_at.map(|t| since(now, t)),
                    resize_in_ms: p.resize_timer.map(|t| until(now, t)),
                    resizes: p.resizes.clone(),
                    rename_ago_ms: p.last_rename.map(|t| since(now, t)),
                    scrollbar_visible: p.sb.visible,
                    scrollbar_hover: p.sb.hover,
                    scrollbar_hide_in_ms: p.sb.hide_at.map(|t| until(now, t)),
                }
            })
            .collect();

        let clients: Vec<snap::Client> = self
            .clients
            .iter()
            .filter(|(_, c)| !c.dead && !c.closing)
            .filter_map(|(&id, c)| {
                let hello = c.hello.as_ref()?;
                descriptors.push(snap::Descriptor {
                    fd: c.stream.as_raw_fd(),
                    role: snap::Role::Client(small(id.0)),
                });
                if let Some(io) = c.popup.as_ref().and_then(crate::popup::Popup::pty) {
                    descriptors.push(snap::Descriptor {
                        fd: io.fd().as_raw_fd(),
                        role: snap::Role::PopupPty(small(id.0)),
                    });
                }
                Some(self.client_out(id, c, hello, form, now))
            })
            .collect();

        vtmux_vt::PATCH_STACK.clone_into(&mut meta.ghostty_patches);
        let terms = || {
            panes.iter().filter_map(|p| p.term.as_ref()).chain(
                clients
                    .iter()
                    .filter_map(|c| c.popup.as_ref()?.term.as_ref()),
            )
        };
        let need = ghostty_pages_requirement();
        if !meta.requires.contains(&need) && terms().any(crate::snapshot_term::has_ghostty_pages) {
            meta.requires.push(need);
        }
        let visible = snap::CURSOR_VERY_VISIBLE_FEATURE.to_string();
        if terms().any(crate::snapshot_term::has_very_visible_cursor) {
            meta.requires.push(visible);
        }
        // A program granted mode 2048 (D31) stopped handling SIGWINCH: a
        // build that drops the grant would leave it drawing at the old
        // size, so a reader must keep it (`snap::INBAND_RESIZE_FEATURE`).
        let inband = snap::INBAND_RESIZE_FEATURE.to_string();
        if !meta.requires.contains(&inband) && terms().any(|t| t.inband_resize) {
            meta.requires.push(inband);
        }
        let bar = snap::SCROLLBAR_FEATURE.to_string();
        if !meta.requires.contains(&bar)
            && panes.iter().any(|p| {
                p.scrollbar_visible || p.scrollbar_hover || p.scrollbar_hide_in_ms.is_some()
            })
        {
            meta.requires.push(bar);
        }
        // A client's queries and terminal title are not required: nearly
        // every attached client has them, so requiring them would refuse
        // every downgrade, and a reader that drops them loses no session
        // state (its client's decoder starts with no start timer, as
        // every build before them did after an upgrade, and its terminal
        // shows the next title set): `vtmux_snapshot`, *Changing the
        // format*.
        snap::Snapshot {
            meta,
            server,
            sessions,
            windows,
            panes,
            clients,
            descriptors,
        }
    }

    #[expect(clippy::too_many_lines, reason = "one field after another")]
    fn client_out(
        &self,
        id: ClientId,
        c: &Client,
        hello: &vtmux_core::protocol::Hello,
        form: Option<Form>,
        now: Instant,
    ) -> snap::Client {
        use vtmux_core::protocol::ControlMode;
        let tty = hello.tty.as_ref();
        let level = match hello.flags.control {
            ControlMode::None => 0,
            ControlMode::Control => 1,
            ControlMode::ControlControl => 2,
        };
        snap::Client {
            id: small(id.0),
            name: self.client_name(id),
            tty: tty.map(|t| t.name.clone()).unwrap_or_default(),
            pid: hello.pid,
            version: hello.version,
            argv: hello.argv.clone(),
            cwd: hello.cwd.to_string_lossy().into_owned(),
            environment: hello.env.clone(),
            term_name: tty.map(|t| t.term.clone()).unwrap_or_default(),
            term_features: tty.map(|t| t.features.clone()).unwrap_or_default(),
            utf8: hello.flags.utf8,
            session: c.attached.map(|s| s.0),
            last_session: c.last_session.map(|s| s.0),
            was_attached: c.was_attached,
            created: c.created_secs,
            activity: c.activity,
            activity_secs: c.activity_secs,
            size: tty.map_or((0, 0), |t| (t.cols, t.rows)),
            pixel_size: c.cell_pixels,
            flags: flags_out(&c.flags),
            key_table: c.keys.table.clone(),
            repeating: c.keys.repeating,
            last_key: c.keys.last_key.map(key_name_out),
            repeat_in_ms: c.keys.repeat_deadline.map(|d| until(now, d)),
            table_ago_ms: since(now, c.keys.table_since),
            focused: c.focused,
            suspended: c.suspended,
            title: c.title.clone(),
            terminal_title: c.terminal_title.clone(),
            pan: c.pan.map(|p| snap::ClientPan {
                window: p.window.0,
                ox: p.ox,
                oy: p.oy,
            }),
            view: Some(snap::ClientView {
                ox: c.view.ox,
                oy: c.view.oy,
                sx: c.view.sx,
                sy: c.view.sy,
                bigger: c.view.bigger,
            }),
            control: c.control.as_ref().map(|cs| {
                let mut out = control_out(cs, &self.control_streams, level, now);
                out.level = level;
                out
            }),
            popup: c.popup.as_ref().and_then(|p| {
                let mut out = p.export()?;
                out.term = form.map(|f| term_to_snapshot(&p.term, f));
                Some(out)
            }),
            token: c.reattach.map_or(0, |r| r.token),
            fg: c.reported.fg.map(rgb_out),
            bg: c.reported.bg.map(rgb_out),
            theme: c.reported.theme.map(|t| t.name().to_owned()),
            palette: c
                .reported
                .palette
                .iter()
                .enumerate()
                .filter_map(|(i, rgb)| {
                    Some(snap::PaletteEntry {
                        index: small(i),
                        rgb: rgb_out((*rgb)?),
                    })
                })
                .collect(),
            written: c.written as u64,
            term_replies: c.term_replies.iter().map(reply_out).collect(),
            tmux_invocation: hello.invocation == vtmux_core::socket::Invocation::Tmux,
            left_session: c.left_session.map(|s| s.0),
            has_tty: tty.is_some(),
            open_features: c.term_options.0.clone(),
            open_overrides: c.term_options.1.clone(),
            status_message: {
                let m = c.status.message_parts();
                (m != crate::render::status::MessageParts::default()).then(|| snap::StatusMessage {
                    text: m.text,
                    ignore_styles: m.ignore_styles,
                    ignore_keys: m.ignore_keys,
                    frozen: m.frozen,
                    timer_in_ms: m.timer.map(|t| until(now, t)),
                })
            },
            queries: {
                let q = c.decoder.queries();
                (q != crate::input::Queries::default()).then(|| snap::TermQueries {
                    start_in_ms: q.until.map(|t| until(now, t)),
                    da: q.da,
                    da2: q.da2,
                    xtversion: q.xtversion,
                    sync: q.sync,
                    colours_in_ms: q.colours_until.map(|t| until(now, t)),
                    fg: q.fg,
                    bg: q.bg,
                    winsize: q.winsize,
                })
            },
        }
    }

    /// A server rebuilt from `snapshot` on `listener` (D27). `fds` hands
    /// back the descriptor kept for each [`snap::Role`]: a client without
    /// its socket is left out, a pane without its PTY has none (it shows
    /// dead once its status is known), a pipe without its descriptor is
    /// closed. The config is taken as loaded: the server it came from had
    /// loaded it.
    ///
    /// # Errors
    /// The snapshot is inconsistent (`InvalidData`: it fails
    /// [`snap::Snapshot::check`] or `validate`), or the poll or a
    /// descriptor can't be set up.
    pub fn from_snapshot(
        listener: StdListener,
        config: ServerConfig,
        snapshot: &snap::Snapshot,
        fds: &mut Adopt<'_>,
    ) -> io::Result<Self> {
        Self::from_snapshot_in(listener, config, snapshot, fds, None)
    }

    /// [`Server::from_snapshot`] for the last resort (decision 7): a
    /// pane's or popup's terminal state that does not import (a page in
    /// Ghostty's form whose checksum fails, a grid not the pane's size)
    /// leaves it a blank terminal of its size, and a copy mode that does
    /// not import is left, instead of failing the whole rebuild. Each such
    /// part is added to `salvaged`.
    ///
    /// # Errors
    /// As [`Server::from_snapshot`], but for those parts.
    pub(super) fn from_snapshot_salvaging(
        listener: StdListener,
        config: ServerConfig,
        snapshot: &snap::Snapshot,
        fds: &mut Adopt<'_>,
        salvaged: &mut Vec<snap::Part>,
    ) -> io::Result<Self> {
        Self::from_snapshot_in(listener, config, snapshot, fds, Some(salvaged))
    }

    #[expect(clippy::too_many_lines, reason = "one record after another")]
    fn from_snapshot_in(
        listener: StdListener,
        config: ServerConfig,
        snapshot: &snap::Snapshot,
        fds: &mut Adopt<'_>,
        mut salvaged: Option<&mut Vec<snap::Part>>,
    ) -> io::Result<Self> {
        validate(snapshot)?;
        // Timers count on from when the snapshot was taken: the exec and
        // the decode since (and a switch back's attempt) are time their
        // clocks ran on, as with no upgrade. Not so the told clients' wait
        // to re-attach: the 10 s are theirs from the notice, which went
        // out after the snapshot (spec, *Clients*).
        let rebuilt = Instant::now();
        let now = rebuilt
            .checked_sub(since_taken(snapshot))
            .unwrap_or(rebuilt);
        let reattach_by = rebuilt + super::upgrade::REATTACH_WAIT;
        let s = &snapshot.server;
        let mut server = Self::new(listener, PathBuf::from(&s.socket_path), config)?;
        server.config_loaded = true;
        server.cfg_finished = true;

        let mut info = ServerInfo::new(s.socket_path.clone());
        info.pid = u32::try_from(s.pid).unwrap_or(info.pid);
        info.start_secs = s.start_time_us / 1_000_000;
        info.config_files = s.config_files.join(",");
        server.config.config_files = s.config_files.iter().map(PathBuf::from).collect();
        server.causes.clone_from(&s.config_causes);
        server.info = info;
        server.global_env = env_in(&s.environment);
        server.start_binary = (!s.start_binary.is_empty()).then(|| PathBuf::from(&s.start_binary));
        server.persist.kept = super::persist::Kept::from_hold(s.persist_hold);
        server.link_numbers.reach(s.link_number);

        // Options: every tree, then the hook bookkeeping.
        let mut trees = vec![
            (TreeId::Server, options_in(&s.server_options)),
            (TreeId::GlobalSession, options_in(&s.session_options)),
            (TreeId::GlobalWindow, options_in(&s.window_options)),
        ];
        trees.extend(
            snapshot
                .sessions
                .iter()
                .map(|x| (TreeId::Session(SessionId(x.id)), options_in(&x.options))),
        );
        trees.extend(
            snapshot
                .windows
                .iter()
                .map(|x| (TreeId::Window(WindowId(x.id)), options_in(&x.options))),
        );
        trees.extend(
            snapshot
                .panes
                .iter()
                .map(|x| (TreeId::Pane(PaneId(x.id)), options_in(&x.options))),
        );
        // Each monitor with what it watches parsed; one that doesn't
        // parse is dropped with its last values and timer.
        let parsed: Vec<_> = s
            .monitors
            .iter()
            .filter_map(|m| {
                let text = format!("x:{}:{}", m.what, m.format);
                let (_, what, _) = vtmux_core::options::monitor::parse_monitor(&text)?;
                Some((m, what))
            })
            .collect();
        server.hook_monitors = crate::control_subs::HookMonitors::from_parts(
            parsed
                .iter()
                .filter_map(|&(m, what)| {
                    let due = after(now, m.due_in_ms?);
                    Some((
                        m.serial,
                        crate::control_subs::SubParts {
                            what,
                            format: m.format.clone(),
                            last: m.last.clone(),
                            each: each_in(&m.each),
                        },
                        due,
                    ))
                })
                .collect(),
        );
        let monitors: Vec<_> = parsed
            .into_iter()
            .map(|(m, what)| {
                (
                    scope_in(&m.scope),
                    m.name.clone(),
                    vtmux_core::options::monitor::Monitor {
                        what,
                        format: m.format.clone(),
                        notify_true: m.notify_true,
                        session: m.session.map(SessionId),
                        serial: m.serial,
                        fires: (m.fire_count, m.fire_time),
                    },
                )
            })
            .collect();
        server.model.options = OptionsStore::import(StoreParts {
            trees,
            pane_window: snapshot
                .panes
                .iter()
                .map(|p| (PaneId(p.id), WindowId(p.window)))
                .collect(),
            hook_events: s.hook_events.clone(),
            hook_fires: s
                .hook_fires
                .iter()
                .map(|h| (scope_in(&h.scope), h.name.clone(), h.count, h.time))
                .collect(),
            monitors,
            monitor_serial: s.monitor_serial,
            term: s.default_term.clone(),
        });

        // Key tables, wholesale: a table the snapshot lacks is gone.
        let mut keys = vtmux_core::keys::KeyBindings::empty();
        for t in &s.key_tables {
            keys.insert_table(
                &t.name,
                vtmux_core::keys::KeyTable::from_parts(
                    t.bindings.iter().filter_map(binding_in),
                    t.defaults.iter().filter_map(binding_in),
                ),
            );
        }
        server.key_bindings = keys;

        server.model.paste = crate::paste::PasteStore::from_parts(
            s.buffers
                .iter()
                .map(|b| crate::paste::PasteBuffer {
                    name: b.name.clone(),
                    data: b.data.clone(),
                    automatic: b.automatic,
                    order: b.order,
                    created: b.created,
                })
                .collect(),
            s.buffer_next_index,
            s.buffer_next_order,
        );
        server.cmdq.set_message_log(
            s.messages
                .iter()
                .map(|m| crate::cmdq::Message {
                    number: m.number,
                    time: m.time_us / 1_000_000,
                    text: m.text.clone(),
                })
                .collect::<VecDeque<_>>(),
            s.message_next,
        );
        server.model.session_ids = IdAllocator::starting_at(s.next_session);
        server.model.window_ids = IdAllocator::starting_at(s.next_window);
        server.model.pane_ids = IdAllocator::starting_at(s.next_pane);
        server.next_client = s.next_client as usize;
        server.prompt_history = crate::prompt_history::History::from_lists([
            s.command_history.clone(),
            s.search_history.clone(),
        ]);
        server.waits = crate::wait::WaitChannels::from_parts(
            s.wait_channels
                .iter()
                .map(|c| crate::wait::ChannelParts {
                    name: c.name.clone(),
                    locked: c.locked,
                    woken: c.woken,
                    waiters: c.waiters.iter().map(|&w| ClientId(w as usize)).collect(),
                    lockers: c.lockers.iter().map(|&w| ClientId(w as usize)).collect(),
                })
                .collect(),
        );
        server
            .model
            .set_counters((s.activity, s.active_points, s.link_stamp));
        server.cmd_number = s.cmd_number;
        server.acl = crate::acl::Acl::from_entries(s.access.iter().map(|a| {
            let subject = if a.group {
                crate::acl::Subject::Group(a.id)
            } else {
                crate::acl::Subject::User(a.id)
            };
            (subject, a.read_only)
        }));

        // Sessions, in the snapshot's order: group members come in their
        // group's order.
        let mut groups: BTreeMap<String, Vec<SessionId>> = BTreeMap::new();
        for x in &snapshot.sessions {
            let id = SessionId(x.id);
            if let Some(g) = &x.group {
                groups.entry(g.clone()).or_default().push(id);
            }
            let session = Session {
                id,
                name: x.name.clone(),
                created: x.created,
                activity: x.activity,
                activity_secs: x.activity_secs,
                cwd: PathBuf::from(&x.cwd),
                windows: x
                    .windows
                    .iter()
                    .map(|l| (l.index, WindowId(l.window)))
                    .collect(),
                current: x.current,
                last: x.last.clone(),
                env: env_in(&x.environment),
                link_alerts: x
                    .windows
                    .iter()
                    .filter(|l| l.alerts != 0)
                    .map(|l| (l.index, u8::try_from(l.alerts).unwrap_or(0)))
                    .collect(),
                link_seq: x.windows.iter().map(|l| (l.index, l.seq)).collect(),
                // Checked by `validate`.
                link_tree: crate::model::rb::Tree::from_shape(
                    &x.link_tree
                        .iter()
                        .map(|n| (n.key, n.red))
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_default(),
                last_attached: x.last_attached,
            };
            server.model.by_name.insert(session.name.clone(), id);
            server.model.sessions.insert(id, session);
        }
        server.model.groups = crate::groups::Groups::from_parts(groups);

        for x in &snapshot.windows {
            let id = WindowId(x.id);
            let window = Window {
                id,
                name: x.name.clone(),
                activity: x.activity,
                activity_secs: x.activity_secs,
                layout: layout_in(&x.layout),
                panes: layout::WindowPanes {
                    panes: x.panes.iter().map(|&p| PaneId(p)).collect(),
                    active: x.active.map(PaneId),
                    last: x.last.iter().map(|&p| PaneId(p)).collect(),
                    z_order: x.z_order.iter().map(|&p| PaneId(p)).collect(),
                    base_index: x.base_index,
                    modal: x.modal.map(PaneId),
                    modal_last: x.modal_last.map(PaneId),
                },
                zoom: x.zoom.as_ref().map(|z| Zoom {
                    pane: PaneId(z.pane),
                    saved: layout_in(&z.saved),
                }),
                was_zoomed: x.was_zoomed.map(PaneId),
                last_layout: x
                    .last_layout
                    .as_deref()
                    .and_then(|n| NamedLayout::ALL.into_iter().find(|l| l.name() == n)),
                old_layout: x.old_layout.clone(),
                pending_size: x.pending_size,
                manual_size: x.manual_size,
                latest: x.latest_client.map(|c| ClientId(c as usize)),
                alerts: crate::alerts::WindowAlerts::restored(
                    u8::try_from(x.alerts).unwrap_or(0),
                    x.silence_in_ms.map(|m| after(now, m)),
                ),
                menu: None,
                menu_last: (0, 0),
            };
            server.model.windows.insert(id, window);
        }

        for x in &snapshot.panes {
            let pane = server.pane_in(
                x,
                fds,
                &snapshot.meta.ghostty_patches,
                now,
                salvaged.as_deref_mut(),
            )?;
            server.model.panes.insert(pane.id, pane);
        }

        if let Some(m) = &s.marked {
            server.model.marked = Some(crate::target::Found {
                session: Some(SessionId(m.session)),
                index: Some(m.window_index),
                window: Some(WindowId(m.window)),
                pane: Some(PaneId(m.pane)),
                new_index: None,
            });
        }
        server.model.marked_link_elsewhere = s.marked_elsewhere.map(SessionId);

        for x in &snapshot.clients {
            let Some(fd) = fds(&snap::Role::Client(x.id)) else {
                continue;
            };
            let id = ClientId(x.id as usize);
            let popup = fds(&snap::Role::PopupPty(x.id));
            let client = server.client_in(
                x,
                fd,
                popup,
                &snapshot.meta.ghostty_patches,
                now,
                reattach_by,
                salvaged.as_deref_mut(),
            )?;
            server.clients.insert(id, client);
        }

        server.model.format_base = Some(Rc::new((server.info.clone(), server.global_env.clone())));
        vtmux_core::cmd::set_command_aliases(server.array_option(TreeId::Server, "command-alias"));
        server.update_width_cache();
        server.refresh_client_targets();
        // `lock-after-time` goes on from where it was.
        let attached: BTreeSet<SessionId> = server.attached_sessions();
        for x in &snapshot.sessions {
            let id = SessionId(x.id);
            if let Some(ms) = x.lock_in_ms
                && attached.contains(&id)
            {
                server.lock_timers.insert(id, after(now, ms));
            }
        }
        // A client from a snapshot without its view has it worked out
        // afresh.
        for x in &snapshot.clients {
            if x.view.is_none() {
                server.update_view(ClientId(x.id as usize));
            }
        }
        server.status_timer_start_all();
        Ok(server)
    }

    fn pane_in(
        &mut self,
        x: &snap::Pane,
        fds: &mut Adopt<'_>,
        patches: &str,
        now: Instant,
        mut salvaged: Option<&mut Vec<snap::Part>>,
    ) -> io::Result<Pane> {
        let id = PaneId(x.id);
        let io = match fds(&snap::Role::PanePty(x.id)) {
            Some(fd) => {
                let io = PaneIo::new(fd);
                io.register(self.poll.registry(), Token(PANE_BASE + id.0 as usize))?;
                Some(io)
            }
            None => None,
        };
        let pipe = match (&x.pipe, fds(&snap::Role::PipePane(x.id))) {
            (Some(p), Some(fd)) => {
                Some(self.pipe_adopt(id, fd, p.pid, (p.input, p.output), p.pending.clone())?)
            }
            _ => None,
        };
        let (sx, sy) = (x.geometry.sx as usize, x.geometry.sy as usize);
        let term = match (restore_term(self, x, patches), salvaged.as_deref_mut()) {
            (Ok(term), _) => term,
            (Err(_), Some(salvaged)) if x.term.is_some() => {
                salvaged.push(snap::Part::PaneTerm(x.id));
                fresh_term(self, x)?
            }
            (Err(e), _) => return Err(e),
        };
        let mut modes = Vec::new();
        for (n, m) in x.modes.iter().enumerate() {
            match mode_in(m, sx, sy, now, &term) {
                Some(mode) => modes.push(mode),
                // A copy mode whose grid does not import is left, as
                // modes the snapshot closes are.
                None => {
                    if let (snap::PaneMode::Copy(_), Some(salvaged)) = (m, salvaged.as_deref_mut())
                    {
                        salvaged.push(snap::Part::PaneMode(x.id, n, snap::ModeKind::Copy));
                    }
                }
            }
        }
        let mut pane = Pane {
            id,
            window: WindowId(x.window),
            term,
            io,
            pid: x.pid,
            tty: x.tty.clone(),
            argv: x.argv.clone(),
            shell: PathBuf::from(&x.shell),
            cwd: PathBuf::from(&x.cwd),
            empty: x.empty,
            wait_step: None,
            editor: None,
            exited: x.exited,
            status: x.status.as_ref().map(status_in),
            dead_time: x.dead_time,
            changed: x.changed,
            unseen_changes: x.unseen_changes,
            output_at: x.output_ago_ms.map(|m| before(now, m)),
            last_rename: x.rename_ago_ms.map(|m| before(now, m)),
            rect: geometry_in(&x.geometry),
            history_limit: usize::try_from(x.history_limit).unwrap_or(usize::MAX),
            input_off: x.input_off,
            focused: x.focused,
            active_point: x.active_point,
            resizes: x.resizes.clone(),
            resize_timer: x.resize_in_ms.map(|m| after(now, m)),
            modes,
            search: x.search.clone(),
            prompt: None,
            prompt_owner: None,
            mode_prompt: None,
            mode_prompt_owner: None,
            compress: crate::compress::PaneCompress::default(),
            pipe,
            sb: crate::scrollbar::AutoHide {
                visible: x.scrollbar_visible,
                hover: x.scrollbar_hover,
                hide_at: x.scrollbar_hide_in_ms.map(|m| after(now, m)),
            },
            over_zoom: x.over_zoom,
            capture_keys: x.capture_keys,
            close_on_click: x.close_on_click,
            close_on_cancel: x.close_on_cancel,
            requests: crate::term_queries::PaneRequests::default(),
            output_generation: x.output_generation,
            kitty_owner: [x.kitty_owner_primary, x.kitty_owner_alternate],
            shell_commands: crate::model::ShellCommands {
                last_prompt: x.last_prompt_time,
                started: x.command_start_time,
                ended: x.command_end_time,
                running: x.command_running,
                status: x.command_status,
            },
        };
        // History imported as rows is resident: due for the compressor at
        // once (decision 23).
        if x.term.is_some() && !salvaged.is_some_and(|s| s.contains(&snap::Part::PaneTerm(x.id))) {
            pane.compress.restored(&pane.term, now);
        }
        pane.settle_imported_kitty_grants();
        Ok(pane)
    }

    #[expect(clippy::too_many_lines, reason = "one field after another")]
    #[expect(
        clippy::too_many_arguments,
        reason = "the record, its descriptors and the rebuild's context"
    )]
    fn client_in(
        &mut self,
        x: &snap::Client,
        fd: OwnedFd,
        popup_fd: Option<OwnedFd>,
        patches: &str,
        now: Instant,
        reattach_by: Instant,
        salvaged: Option<&mut Vec<snap::Part>>,
    ) -> io::Result<Client> {
        use vtmux_core::protocol::{ClientFlags, ControlMode, Hello, TtyInfo};
        let std = std::os::unix::net::UnixStream::from(fd);
        std.set_nonblocking(true)?;
        let mut stream = UnixStream::from_std(std);
        self.poll.registry().register(
            &mut stream,
            Token(CLIENT_BASE + x.id as usize),
            Interest::READABLE,
        )?;
        let mut c = Client::new(stream, x.activity);
        let level = x.control.as_ref().map_or(0, |cs| cs.level);
        let hello = Hello {
            version: x.version,
            invocation: if x.tmux_invocation {
                vtmux_core::socket::Invocation::Tmux
            } else {
                vtmux_core::socket::Invocation::Vtmux
            },
            pid: x.pid,
            argv: x.argv.clone(),
            cwd: PathBuf::from(&x.cwd),
            env: x.environment.clone(),
            tty: x.has_tty.then(|| TtyInfo {
                name: x.tty.clone(),
                term: x.term_name.clone(),
                cols: x.size.0,
                rows: x.size.1,
                features: x.term_features.clone(),
            }),
            flags: ClientFlags {
                control: match level {
                    1 => ControlMode::Control,
                    2 => ControlMode::ControlControl,
                    _ => ControlMode::None,
                },
                utf8: x.utf8,
                no_start: false,
            },
            // Only a client that follows upgrades was given a token.
            ext: vtmux_core::protocol::HelloExt {
                follows_upgrade: x.token != 0,
                ..Default::default()
            },
        };
        // A control client stays connected across an upgrade (live-upgrade
        // spec, decision 26): one of another protocol version than this
        // image's reads the frozen frames only, as when it connects
        // (`Hello::check_version`). A terminal client of the image before
        // comes back as this image's binary, or is detached.
        if level != 0 && x.version != vtmux_core::protocol::PROTOCOL_VERSION {
            c.speaks = vtmux_core::protocol::Speaks::Frozen;
        }
        c.environ = hello.env.clone().into();
        c.attached = x.session.map(SessionId);
        c.activity_secs = x.activity_secs;
        c.cell_pixels = x.pixel_size;
        c.created_secs = x.created;
        c.focused = x.focused;
        c.keys.table.clone_from(&x.key_table);
        c.keys.repeating = x.repeating;
        c.keys.last_key = x.last_key.as_deref().and_then(key_name_in);
        c.keys.repeat_deadline = x.repeat_in_ms.map(|m| after(now, m));
        c.keys.table_since = before(now, x.table_ago_ms);
        c.last_session = x.last_session.map(SessionId);
        c.was_attached = x.was_attached;
        c.left_session = x.left_session.map(SessionId);
        c.written = usize::try_from(x.written).unwrap_or(usize::MAX);
        c.flags = flags_in(&x.flags);
        c.suspended = x.suspended;
        c.title.clone_from(&x.title);
        c.terminal_title.clone_from(&x.terminal_title);
        c.pan = x.pan.as_ref().map(|p| crate::view::Pan {
            window: WindowId(p.window),
            ox: p.ox,
            oy: p.oy,
        });
        c.view = x
            .view
            .as_ref()
            .map_or_else(crate::view::View::default, |v| crate::view::View {
                ox: v.ox,
                oy: v.oy,
                sx: v.sx,
                sy: v.sy,
                bigger: v.bigger,
            });
        let mut palette = crate::term_queries::Reported::default().palette;
        for e in &x.palette {
            if let Some(slot) = palette.get_mut(e.index as usize) {
                *slot = Some(rgb_in(&e.rgb));
            }
        }
        c.reported = crate::term_queries::Reported::restored(
            x.fg.as_ref().map(rgb_in),
            x.bg.as_ref().map(rgb_in),
            x.theme.as_deref().and_then(|t| match t {
                "light" => Some(vtmux_core::colour::Theme::Light),
                "dark" => Some(vtmux_core::colour::Theme::Dark),
                _ => None,
            }),
            palette,
        );
        c.term_replies = x.term_replies.iter().filter_map(reply_in).collect();
        c.control = x
            .control
            .as_ref()
            .map(|cs| control_in(cs, &mut self.control_streams, now));
        // An attached terminal client's terminal is opened again as at
        // attach (`tty_open`), with the `terminal-features` and
        // `terminal-overrides` it was opened with (tty-term.c:457, 619
        // read them only then), then told what it replied.
        if c.attached.is_some() && c.control.is_none() {
            let features = x.open_features.clone();
            let overrides = x.open_overrides.clone();
            if let Some(Ok((mut term, caps))) =
                crate::render::Caps::open_term(&hello, &features, &overrides)
            {
                term.set_utf8(hello.flags.utf8);
                for r in &c.term_replies {
                    match r {
                        TermReply::Sync => term.sync_supported(),
                        TermReply::Da(p) => term.device_attributes(p),
                        TermReply::Da2(first) => term.secondary_device_attributes(*first),
                        TermReply::Xtversion(text) => term.extended_device_attributes(text),
                    }
                }
                c.decoder.set_terminal(&term);
                c.caps = Some(caps);
                c.compositor
                    .set_cursor_codes(crate::render::CursorCodes::from_term(&term));
                c.term = Some(Rc::new(term));
                // The style and colour the old image gave the terminal are
                // not in the snapshot.
                c.cursor.forget();
                c.term_options = (features, overrides);
                c.update_theme();
            }
        }
        if let Some(m) = &x.status_message {
            c.status
                .restore_message(crate::render::status::MessageParts {
                    text: m.text.clone(),
                    ignore_styles: m.ignore_styles,
                    ignore_keys: m.ignore_keys,
                    frozen: m.frozen,
                    timer: m.timer_in_ms.map(|ms| after(now, ms)),
                });
        }
        if let Some(q) = &x.queries {
            c.decoder.set_queries(crate::input::Queries {
                until: q.start_in_ms.map(|ms| after(now, ms)),
                da: q.da,
                da2: q.da2,
                xtversion: q.xtversion,
                sync: q.sync,
                colours_until: q.colours_in_ms.map(|ms| after(now, ms)),
                fg: q.fg,
                bg: q.bg,
                winsize: q.winsize,
            });
        }
        c.hello = Some(hello);
        // Told to re-execute before the exec: drawn nothing until it
        // re-attaches with its token (`upgrade.rs`, decision 21).
        if x.token != 0 && c.attached.is_some() && c.control.is_none() {
            c.reattach = Some(super::upgrade::Reattach {
                token: x.token,
                deadline: Some(reattach_by),
                gone: false,
            });
        }
        if let Some(p) = &x.popup {
            let io = match popup_fd {
                Some(fd) => {
                    let io = PaneIo::new(fd);
                    io.register(
                        self.poll.registry(),
                        Token(crate::popup::POPUP_BASE + x.id as usize),
                    )?;
                    Some(io)
                }
                None => None,
            };
            let mut popup = crate::popup::Popup::import(p, io, PaneTerm::new(1, 1, 0));
            popup.term = match (restore_popup_term(self, &popup, p, patches), salvaged) {
                (Ok(term), _) => term,
                (Err(_), Some(salvaged)) if p.term.is_some() => {
                    salvaged.push(snap::Part::PopupTerm(x.id));
                    fresh_popup_term(self, &popup)?
                }
                (Err(e), _) => return Err(e),
            };
            c.popup = Some(popup);
        }
        Ok(c)
    }
}
