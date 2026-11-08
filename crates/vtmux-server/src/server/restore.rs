//! Restoring a `vtmux-persist` checkpoint at server start (D27 phase 3,
//! docs/specs/2026-11-06-live-upgrade.md, decisions 15, 27, 28 and 30;
//! *Design > Persistence > Restoring*). tmux has nothing like it: with the
//! option `off` (the default) nothing here runs.
//!
//! [`start`] builds the server a starting client's command will find:
//!
//! 1. A fresh server, as tmux's `server_start` makes one. If the socket's
//!    checkpoint directory has no manifest, that is the server.
//! 2. What the config says about `vtmux-persist` and `vtmux-persist-drop`
//!    ([`config_says`]): its `set-option` commands for them, read before
//!    it runs (the spec: "the option is read from the config before the
//!    first session is created"). A config that sets `vtmux-persist off`
//!    gets no restore (the files stay); a config that says nothing leaves
//!    it to the checkpoint's own setting, a runtime one (decision 27).
//! 3. The checkpoint: `manifest`, else `manifest.prev`, decoded, made into
//!    what a restore imports ([`prepare`]), then checked in a child process
//!    by the dry run a live upgrade uses (`--vtmux-check-snapshot`, which
//!    rebuilds the whole server from it), so a checkpoint that would crash
//!    or wedge the rebuild costs a child, not the server.
//! 4. The server rebuilt from it ([`rebuild`]): sessions, windows, layout,
//!    options, key bindings, (for `full`) buffers; every pane dead with
//!    its start command and cwd. The config has not run: it runs when the
//!    first client says hello, on top (decision 27), so config edits since
//!    the save win, and a `new-session` of a restored name fails as tmux's
//!    `duplicate session` does. Its first set of each option or hook
//!    starts from the default (`Server::config_sets`), as on a fresh
//!    server, so its `-a` appends once rather than once per restart.
//!
//! A checkpoint that can't be read, checked or rebuilt leaves the fresh
//! server, with why in `show-messages`; the files are kept, and this
//! server writes no checkpoint over them while they are there
//! (`Persist::kept`). A corrupt or truncated `manifest` of a format this
//! binary reads falls back to `manifest.prev`, and once restored from it
//! the server checkpoints as usual.
//!
//! The reason is also told to the client that started the server, when
//! it ends with no session to show for it (`start-server`, `attach`: the
//! server exits at once under `exit-empty`, and `show-messages` with it):
//! on its stderr, with exit status 1 where its command would have
//! succeeded ([`Server::tell_starter`]).
//!
//! A newer binary's checkpoint is another matter (decision 6: older
//! refuses newer and must not destroy it): when `manifest` or
//! `manifest.prev` is of a newer format, or requires a feature this
//! binary lacks ([`snap::Error::from_newer_binary`]), the directory is
//! left alone: no restore (not from the other file either, whose next
//! rotation would delete the newer one), no lock taken, and nothing
//! written there for this server's lifetime, with why in
//! `show-messages`. A server that did not restore (its config set
//! `vtmux-persist off`, turned on later by `set -s`) or was started by a
//! live upgrade checks the same before it first writes there. A live
//! upgrade carries what the start found (`Persist::kept`, the snapshot's
//! `persist_hold`), so the new image keeps a refused checkpoint as the
//! old one did.
//!
//! Restored panes come back dead (`#{pane_dead}` 1, exit status unknown:
//! `JobStatus::Unknown`), showing `remain-on-exit-format`'s dead line
//! (server-fn.c `server_destroy_pane`) on their saved screen: empty for
//! `layout`; for `full`, the screen and history pane terminal state
//! carries, which `snapshot::restore_term` imports (the hook
//! `lu_pane_state` fills). `respawn-pane` and `respawn-window` start the
//! saved command in the saved cwd, as for any dead pane. Nothing is
//! relaunched by itself (decision 8).

use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};

use vtmux_core::cmd::{self, ParseOpts};
use vtmux_snapshot as snap;

use super::persist::{Kept, Mode};
use super::{GlobalEnv, Server, ServerConfig, global_environ};
use crate::persist::{self as files, Lock, MANIFEST, MANIFEST_PREV};

/// What the config sets of the persistence options, read before it runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConfigSays {
    /// `vtmux-persist`, when the config sets it.
    pub persist: Option<Setting>,
    /// The global `vtmux-persist-drop`, when the config sets it.
    pub drop: Option<Vec<String>>,
}

/// A `vtmux-persist` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Setting {
    Off,
    On(Mode),
}

impl Setting {
    fn mode(self) -> Option<Mode> {
        match self {
            Self::Off => None,
            Self::On(m) => Some(m),
        }
    }
}

const PERSIST: &str = "vtmux-persist";
const DROP: &str = "vtmux-persist-drop";

/// `vtmux-persist`'s value by name.
fn mode_named(name: &str) -> Option<Setting> {
    match name {
        "off" => Some(Setting::Off),
        "layout" => Some(Setting::On(Mode::Layout)),
        "full" => Some(Setting::On(Mode::Full)),
        _ => None,
    }
}

/// An array option assigned from one string: split at spaces and commas
/// (options.c `options_array_assign`, with no separator).
fn split_list(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split([' ', ','])
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
}

/// Fold one parsed command into `says`: a `set-option` of
/// `vtmux-persist` (any scope: it is a server option) or of the global
/// `vtmux-persist-drop` (`-g`), with `-u` and `-a` as `set-option` treats
/// them. `-F` values are expanded by `expand`; `-o` (only if unset)
/// counts only when the config has not set it yet. Anything else,
/// including a value `set-option` would refuse, changes nothing.
fn fold(says: &mut ConfigSays, call: &cmd::CommandCall, expand: &dyn Fn(&str) -> String) {
    let args = &call.args;
    if call.entry.name != "set-option" {
        return;
    }
    let Some(name) = args.positional(0) else {
        return;
    };
    let value = args.positional_text(1).map(|v| {
        if args.flag('F') {
            expand(&v)
        } else {
            v.into_owned()
        }
    });
    let unset = args.flag('u') || args.flag('U');
    match name {
        PERSIST => {
            if args.flag('o') && says.persist.is_some() {
                return;
            }
            let set = if unset {
                Some(Setting::Off)
            } else {
                value.as_deref().and_then(mode_named)
            };
            if set.is_some() {
                says.persist = set;
            }
        }
        DROP if args.flag('g') && args.target('t').is_none() => {
            if args.flag('o') && says.drop.is_some() {
                return;
            }
            if unset {
                says.drop = Some(Vec::new());
            } else if let Some(v) = value {
                let mut list = if args.flag('a') {
                    says.drop.take().unwrap_or_default()
                } else {
                    Vec::new()
                };
                list.extend(split_list(&v));
                says.drop = Some(list);
            }
        }
        _ => {}
    }
}

/// [`ConfigSays`] of the config files `server` will run: each parsed as
/// the startup config is (`%if` over the empty server, assignments into a
/// copy of the global environment), its top-level commands folded in
/// order. Files sourced from them are not followed; a file that can't be
/// read or parsed says nothing (the config's own run reports it).
pub(crate) fn config_says(server: &Server) -> ConfigSays {
    let mut says = ConfigSays::default();
    let mut env = server.global_env.clone();
    for file in &server.config.config_files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let text = vtmux_core::rawstr::from_vec(bytes);
        let mut genv = GlobalEnv::new(&mut env, &server.model, &server.info);
        let opts = ParseOpts {
            from_file: true,
            ..ParseOpts::default()
        };
        let Ok(calls) = cmd::parse_file(&text, &mut genv, opts, None) else {
            continue;
        };
        let expand = |t: &str| vtmux_core::cmd::ConfigEnv::expand(&genv, t);
        for call in &calls {
            fold(&mut says, call, &expand);
        }
    }
    says
}

/// The value `name` holds in `entries`, as a list of strings (an array's
/// items, or one value).
fn list_option(entries: &[snap::OptionEntry], name: &str) -> Option<Vec<String>> {
    let text = |v: &snap::Value| match v {
        snap::Value::String(s)
        | snap::Value::Choice(s)
        | snap::Value::Key(s)
        | snap::Value::Colour(s)
        | snap::Value::Command(s) => s.clone(),
        snap::Value::Number(n) => n.to_string(),
        snap::Value::Flag(b) => if *b { "on" } else { "off" }.to_owned(),
    };
    let e = entries.iter().find(|e| e.name == name)?;
    Some(match &e.value {
        snap::OptionValue::Single(v) => vec![text(v)],
        snap::OptionValue::Array(items) => items.iter().map(|i| text(&i.value)).collect(),
    })
}

/// The `vtmux-persist` a restore goes by: the config's when it sets it,
/// else the checkpoint's own.
fn effective_mode(s: &snap::Snapshot, says: &ConfigSays) -> Option<Mode> {
    says.persist
        .or_else(|| {
            list_option(&s.server.server_options, PERSIST)
                .and_then(|v| v.first().and_then(|n| mode_named(n)))
        })
        .and_then(Setting::mode)
}

/// Leave out of `entries` the user options matching a glob of `globs`.
fn drop_user_options(entries: &mut Vec<snap::OptionEntry>, globs: &[String]) {
    if globs.is_empty() {
        return;
    }
    entries.retain(|e| {
        !(e.name.starts_with('@') && globs.iter().any(|g| crate::target::fnmatch(g, &e.name)))
    });
}

/// `vtmux-persist-drop` (decision 30): the user options matching its globs
/// are not restored. A session's own value applies to its options, else
/// the global one: the config's when it sets it (it runs after the
/// restore and wins anyway), else the checkpoint's. The global value
/// applies to the server and global trees; a window's options (and its
/// panes') go by every session it is linked into.
fn apply_drop(s: &mut snap::Snapshot, says: &ConfigSays) {
    let global = says
        .drop
        .clone()
        .or_else(|| list_option(&s.server.session_options, DROP))
        .unwrap_or_default();
    drop_user_options(&mut s.server.server_options, &global);
    drop_user_options(&mut s.server.session_options, &global);
    drop_user_options(&mut s.server.window_options, &global);
    let mut by_window: std::collections::BTreeMap<u32, Vec<String>> =
        std::collections::BTreeMap::new();
    for x in &mut s.sessions {
        let globs = list_option(&x.options, DROP).unwrap_or_else(|| global.clone());
        drop_user_options(&mut x.options, &globs);
        for l in &x.windows {
            by_window
                .entry(l.window)
                .or_default()
                .extend(globs.iter().cloned());
        }
    }
    for w in &mut s.windows {
        if let Some(globs) = by_window.get(&w.id) {
            drop_user_options(&mut w.options, globs);
        }
    }
    for p in &mut s.panes {
        if let Some(globs) = by_window.get(&p.window) {
            drop_user_options(&mut p.options, globs);
        }
    }
}

/// The checkpoint `s` made into what a restore imports: the server is
/// this one (`socket`, this process, started now, `config`'s files), the
/// user options `vtmux-persist-drop` names are gone, and every pane is
/// dead, its process (which ran in the server before) unknown: no pid, no
/// tty, no `pipe-pane`. A pane that was dead already keeps its status
/// and time of death; the others died now, as far as this server knows,
/// and get their time of death with their dead line in [`rebuild`].
pub(crate) fn prepare(
    mut s: snap::Snapshot,
    says: &ConfigSays,
    socket: &Path,
    config: &ServerConfig,
) -> snap::Snapshot {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let x = &mut s.server;
    x.pid = i32::try_from(std::process::id()).unwrap_or(x.pid);
    x.start_time_us = i64::try_from(now.as_micros()).unwrap_or(i64::MAX);
    x.socket_path = socket.to_string_lossy().into_owned();
    x.config_files = config
        .config_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    x.config_causes.clear();
    // What this start finds decides what it may write (`start`).
    x.persist_hold = 0;
    s.descriptors.clear();
    s.clients.clear();
    s.server.next_client = 0;
    apply_drop(&mut s, says);
    for p in &mut s.panes {
        p.exited = true;
        p.status.get_or_insert_with(snap::ExitStatus::default);
        p.pid = 0;
        p.tty.clear();
        p.pipe = None;
        p.resizes.clear();
        p.resize_in_ms = None;
    }
    s
}

/// The server `s` (a [`prepare`]d checkpoint) describes, on `listener`:
/// [`Server::from_snapshot`] with no descriptor kept, then what makes it
/// a starting server rather than a resumed one: the global environment is
/// the starting client's (the checkpoint has none), the config is still
/// to run, every pane shows its dead line, and (`layout`, which keeps no
/// titles) is titled as a new pane is.
///
/// tmux draws a pane's dead line once, when it dies (server-fn.c
/// `server_destroy_pane`, `PANE_STATUSDRAWN`; vtmux's `dead_time`). A
/// pane alive in the checkpoint dies now and gets it now; one dead
/// already shows it on the screen a `full` checkpoint carries, so it gets
/// none again (each restart of a restored server would scroll the screen
/// once more and stack another); with `layout`'s empty screen it is drawn
/// again. It is drawn on the last row when that is blank, without tmux's
/// scroll, so the saved screen shows as it was
/// ([`Server::draw_restored_dead_line`]). The check in the child
/// ([`check`]) builds it with a `dry_run` config only to drop it.
pub(crate) fn rebuild(
    listener: StdListener,
    config: ServerConfig,
    s: &snap::Snapshot,
) -> io::Result<Server> {
    listener.set_nonblocking(true)?;
    let mut server = Server::from_snapshot(listener, config, s, &mut |_| None)?;
    server.global_env = global_environ(&server.config);
    server.model.format_base = Some(std::rc::Rc::new((
        server.info.clone(),
        server.global_env.clone(),
    )));
    server.config_loaded = false;
    server.cfg_finished = false;
    // The config runs on top as on a fresh server: its first set of each
    // option starts from the default, so its `-a` appends once, not once
    // more per restart, and `-o` is not refused by its own earlier value.
    server.config_sets = Some(std::collections::HashSet::new());
    // A `layout` checkpoint keeps no title (programs set them): each pane
    // has the one a new pane gets, the host name (window.c
    // `window_pane_create`).
    let host = server.info.host.clone();
    let untitled =
        s.meta.purpose == snap::Purpose::CheckpointLayout && !host.chars().any(char::is_control);
    let shown: std::collections::HashSet<u32> = s
        .panes
        .iter()
        .filter(|p| p.dead_time.is_some() && p.term.is_some())
        .map(|p| p.id)
        .collect();
    let panes: Vec<_> = server.model.panes.keys().copied().collect();
    for p in panes {
        let Some(pane) = server.model.panes.get_mut(&p) else {
            continue;
        };
        if untitled {
            pane.term.set_title(&host);
        }
        if shown.contains(&p.0) {
            continue;
        }
        pane.dead_time.get_or_insert_with(crate::model::now_epoch);
        server.draw_restored_dead_line(p);
    }
    Ok(server)
}

/// A checkpoint ready to import, or why there is none.
enum Found {
    /// No checkpoint, or `vtmux-persist` off: start fresh, quietly.
    None,
    /// A newer binary's checkpoint, and why: leave the directory alone.
    Newer(String),
    /// One that can't be restored, and why.
    Refused(String),
    /// One to import, the file it came from, the directory's lock, and
    /// what was left out of it ([`files::load_file`]).
    Ready(Box<snap::Snapshot>, &'static str, Lock, Vec<String>),
}

/// Why `dir` holds a newer binary's checkpoint: the first of `manifest`
/// and `manifest.prev` that is refused for that
/// ([`snap::Error::from_newer_binary`]). Only read, before the lock is
/// taken, so the directory is not touched; a file that can't be read or
/// fails for another reason is [`find`]'s to report. A server checks
/// again before it first writes there (`Server::checkpoint_target`): one
/// whose config had `vtmux-persist off`, or the image a live upgrade
/// started (which carries whether the old one held it back, not why).
pub(super) fn written_by_newer(dir: &Path) -> Option<String> {
    [MANIFEST, MANIFEST_PREV].into_iter().find_map(|name| {
        let bytes = std::fs::read(dir.join(name)).ok()?;
        let refused = std::panic::catch_unwind(|| snap::page_refs(&bytes).err()).ok()??;
        refused
            .from_newer_binary()
            .then(|| format!("{name}: {refused}"))
    })
}

/// What `show-messages` says of `dir`, a newer binary's checkpoint.
pub(super) fn left_alone(dir: &Path, why: &str) -> String {
    format!(
        "vtmux-persist: {}: {why}; written by a newer vtmux, the directory is left as it is \
         and this server writes no checkpoint",
        dir.display()
    )
}

/// The checkpoint `server` (fresh) restores from `dir`: the first of
/// `manifest` and `manifest.prev` that decodes, is a checkpoint, and
/// passes the check in a child process; none when either is a newer
/// binary's.
fn find(server: &Server, dir: &Path) -> Found {
    if ![MANIFEST, MANIFEST_PREV]
        .iter()
        .any(|m| dir.join(m).exists())
    {
        return Found::None;
    }
    if let Some(why) = written_by_newer(dir) {
        return Found::Newer(why);
    }
    let says = config_says(server);
    if says.persist == Some(Setting::Off) {
        return Found::None;
    }
    let lock = match Lock::take(dir) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            return Found::Refused("in use by another server".to_owned());
        }
        Err(e) => return Found::Refused(e.to_string()),
    };
    let mut errors = Vec::new();
    for name in [MANIFEST, MANIFEST_PREV] {
        let loaded = std::panic::catch_unwind(|| files::load_file(dir, name))
            .unwrap_or_else(|_| Err(io::Error::other("decoding panicked")));
        let (s, left_out) = match loaded {
            Ok(loaded) => loaded,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                errors.push(format!("{name}: {e}"));
                continue;
            }
        };
        if !matches!(
            s.meta.purpose,
            snap::Purpose::CheckpointLayout | snap::Purpose::CheckpointFull
        ) {
            errors.push(format!("{name}: not a checkpoint"));
            continue;
        }
        if effective_mode(&s, &says).is_none() {
            return Found::None;
        }
        let s = prepare(s, &says, &server.socket, &server.config);
        match check_in_child(&s, &server.socket) {
            Ok(()) => return Found::Ready(Box::new(s), name, lock, left_out),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    Found::Refused(errors.join("; "))
}

/// How long the dry run of a checkpoint may take: the 30 s `dry_run` had
/// before an upgrade's own limit (3 s) became an argument.
const CHECK_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The dry run of a live upgrade on `s` (`--vtmux-check-snapshot`, by this
/// server's own executable), which decodes it and rebuilds the server
/// from it in a child process ([`super::resume::check_snapshot`]).
fn check_in_child(s: &snap::Snapshot, socket: &Path) -> Result<(), String> {
    let exe = crate::shim::own_exe().ok_or_else(|| "no executable to check it with".to_owned())?;
    let dir = socket.parent().unwrap_or(Path::new("/tmp"));
    let file = crate::sys::upgrade::memfile(dir, &snap::encode(s))
        .map_err(|e| format!("snapshot: {}", vtmux_core::error::strerror(&e)))?;
    match crate::sys::upgrade::dry_run(&exe, file.as_fd(), CHECK_LIMIT) {
        Ok(r) => r.map(drop),
        Err(e) => Err(format!(
            "{}: {}",
            exe.display(),
            vtmux_core::error::strerror(&e)
        )),
    }
}

/// The check in the child for a checkpoint (`resume::check_bytes`):
/// [`rebuild`] on a placeholder socket.
pub(super) fn check(s: &snap::Snapshot) -> io::Result<Server> {
    let (ours, _) = std::os::unix::net::UnixStream::pair()?;
    let config = ServerConfig {
        dry_run: true,
        ..ServerConfig::default()
    };
    rebuild(StdListener::from(OwnedFd::from(ours)), config, s)
}

/// The server for a client starting one on `listener` (module docs).
///
/// # Errors
/// The fresh server can't be set up (the poll, signals, the listener).
pub fn start(listener: StdListener, socket: PathBuf, config: ServerConfig) -> io::Result<Server> {
    let mut fresh = Server::new(listener.try_clone()?, socket.clone(), config.clone())?;
    let Some(dir) = fresh.persist_dir() else {
        return Ok(fresh);
    };
    let why = match find(&fresh, &dir) {
        Found::None => return Ok(fresh),
        Found::Newer(why) => {
            fresh.persist.kept = Kept::Always;
            let said = left_alone(&dir, &format!("not restored: {why}"));
            fresh.persist.refusal = Some(said.clone());
            fresh.add_message(said);
            return Ok(fresh);
        }
        Found::Refused(why) => why,
        Found::Ready(s, name, lock, left_out) => {
            // The fresh server's shim goes before the restored one makes
            // its own in the same place.
            drop(fresh);
            match super::resume::guarded(|| rebuild(listener.try_clone()?, config.clone(), &s)) {
                Ok(mut server) => {
                    server.persist.lock = Some((dir.clone(), lock));
                    server.persist.dir = Some(dir.clone());
                    let without = if left_out.is_empty() {
                        String::new()
                    } else {
                        format!(", without {}", left_out.join(", "))
                    };
                    server.add_message(format!(
                        "vtmux-persist: restored {} sessions from {}{without}",
                        s.sessions.len(),
                        dir.join(name).display()
                    ));
                    return Ok(server);
                }
                Err(why) => {
                    drop(lock);
                    fresh = Server::new(listener, socket, config)?;
                    fresh.persist.dir = Some(dir.clone());
                    why
                }
            }
        }
    };
    fresh.persist.kept = Kept::WhileThere;
    let said = format!(
        "vtmux-persist: {}: not restored: {why}; the files are kept and not written over",
        dir.display()
    );
    fresh.persist.refusal = Some(said.clone());
    fresh.add_message(said);
    Ok(fresh)
}

#[cfg(test)]
mod tests;
