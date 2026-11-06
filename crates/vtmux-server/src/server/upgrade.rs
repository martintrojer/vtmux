//! `vtmux-upgrade`: moving the running server onto another binary (D27,
//! docs/specs/2026-11-06-live-upgrade.md, *Live upgrade*).
//!
//! A request comes from the command (`vtmux-upgrade [-b binary]`, in a
//! queue) or from the frozen handshake (`UpgradeRequest`, from a client of
//! any version). Either way it runs the old image's half of the sequence:
//!
//! 1. **Preflight** ([`Server::upgrade_start`], then each loop turn in
//!    [`Server::upgrade_turn`]): refuse a relative, missing or
//!    non-executable binary, a second upgrade, or a server holding
//!    descriptors from a failed one. Then wait up to [`PREFLIGHT_WAIT`]
//!    for the work the snapshot can't carry: `run-shell` and `if-shell`
//!    jobs (decision 10), commands in any queue but the requester's and
//!    command clients still being served (decision 26). Refuse if any is
//!    left.
//! 2. **Quiesce**: the rest runs in one go inside one loop turn, so no pane
//!    or client is read until it ends (what they send waits in the kernel);
//!    every client's queued output is written out first.
//! 3. The snapshot, the dry run and the `execve`
//!    ([`Server::exec_upgrade`], `resume.rs`). Any of them failing leaves
//!    the old server running, and the requester is told why.
//!
//! The new image's half is [`super::resume`].

use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vtmux_core::error::strerror;
use vtmux_core::protocol::ServerMsg;

use super::{ClientId, Server};
use crate::cmdq::{QueueId, StepId};

/// What replaces the server image: [`crate::sys::upgrade::exec_in_place`]
/// (tests swap it). Returns only on failure.
pub type ExecFn = fn(&Path, OwnedFd, &[RawFd]) -> io::Error;

/// How long the preflight waits for jobs and commands in flight
/// (decisions 10 and 26: "a few seconds", 3 s).
pub const PREFLIGHT_WAIT: Duration = Duration::from_secs(3);

/// How long the quiesce waits for clients to take their queued output.
pub const FLUSH_WAIT: Duration = Duration::from_secs(1);

/// How often a waiting preflight looks again, besides on every event.
const RECHECK: Duration = Duration::from_millis(50);

/// Who asked for the upgrade, and so who hears how it went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Requester {
    /// The frozen `UpgradeRequest` on this connection: answered with
    /// `UpgradeResult`.
    Handshake(ClientId),
    /// A `vtmux-upgrade` command waiting at `step`, run for `client`
    /// (none: the global queue).
    Command {
        step: StepId,
        client: Option<ClientId>,
    },
}

impl Requester {
    fn client(self) -> Option<ClientId> {
        match self {
            Self::Handshake(id) => Some(id),
            Self::Command { client, .. } => client,
        }
    }
}

/// An upgrade past its first checks, waiting for the work in flight.
#[derive(Debug)]
pub(crate) struct Pending {
    requester: Requester,
    binary: PathBuf,
    /// When the preflight gives up waiting.
    deadline: Instant,
}

/// The server's upgrade state.
#[derive(Debug)]
pub(crate) struct Upgrade {
    pub(crate) pending: Option<Pending>,
    pub(crate) exec: ExecFn,
}

impl Default for Upgrade {
    fn default() -> Self {
        Self {
            pending: None,
            exec: crate::sys::upgrade::exec_in_place,
        }
    }
}

impl Server {
    /// The frozen upgrade handshake (live-upgrade spec, *Handshake*): a
    /// client of any version asks the server to move onto `binary`, with no
    /// `Hello` and no version check, and gets one `UpgradeResult` before
    /// the connection closes. Only a peer the access list lets write may
    /// ask: the request runs `binary` as the server's owner
    /// (server-acl.c `server_acl_check`).
    pub(crate) fn upgrade_request(&mut self, id: ClientId, binary: &Path) {
        let peer = self.clients.get(&id).and_then(|c| c.peer);
        let requester = Requester::Handshake(id);
        let refused = if self.acl.check(peer) == crate::acl::Access::Write {
            self.upgrade_start(requester, binary.to_path_buf()).err()
        } else {
            Some("access denied".to_owned())
        };
        if let Some(message) = refused {
            self.upgrade_refuse(requester, &message);
        }
    }

    /// Step 1's first checks: `binary` absolute, a regular file this user
    /// may run, no other upgrade running and nothing held from a failed
    /// one. Then the upgrade waits for [`Server::upgrade_turn`].
    ///
    /// # Errors
    /// Why the upgrade is refused at once.
    pub(crate) fn upgrade_start(
        &mut self,
        requester: Requester,
        binary: PathBuf,
    ) -> Result<(), String> {
        if self.upgrade.pending.is_some() {
            return Err("an upgrade is already running".to_owned());
        }
        if !self.held.is_empty() {
            // `resume.rs`, `serve_held`: the snapshot can't name them, so
            // the exec would close them, and the panes' terminals with
            // them.
            return Err(format!(
                "{} descriptors held from a failed upgrade: kill-server releases them",
                self.held.len()
            ));
        }
        check_binary(&binary)?;
        self.upgrade.pending = Some(Pending {
            requester,
            binary,
            deadline: Instant::now() + PREFLIGHT_WAIT,
        });
        Ok(())
    }

    /// When the loop must look at a waiting upgrade again.
    pub(crate) fn upgrade_deadline(&self) -> Option<Instant> {
        let p = self.upgrade.pending.as_ref()?;
        Some(p.deadline.min(Instant::now() + RECHECK))
    }

    /// The binary `vtmux-upgrade` without `-b` moves the server onto
    /// (decision 22): the path the server was started as
    /// ([`start_binary`], kept across upgrades), symlinks resolved now, so
    /// a relink since the start is followed; without one, the running
    /// binary.
    ///
    /// # Errors
    /// The path no longer leads to a file.
    pub(crate) fn upgrade_default_binary(&self) -> Result<PathBuf, String> {
        let Some(start) = &self.start_binary else {
            return crate::shim::own_exe()
                .ok_or_else(|| "can't find the server's binary".to_owned());
        };
        std::fs::canonicalize(start).map_err(|e| format!("{}: {}", start.display(), strerror(&e)))
    }

    /// A loop turn's look at a waiting upgrade: forget it if the requester
    /// went, refuse it if work is still in flight at the deadline, and run
    /// it once nothing is.
    pub(crate) fn upgrade_turn(&mut self, now: Instant) {
        let Some(p) = self.upgrade.pending.as_ref() else {
            return;
        };
        let requester = p.requester;
        let gone = match requester {
            Requester::Handshake(id) => self.clients.get(&id).is_none_or(|c| c.dead),
            Requester::Command { step, .. } => self.cmdq.queue_of(step).is_none(),
        };
        if gone {
            self.upgrade.pending = None;
            return;
        }
        if self.shutdown {
            self.upgrade.pending = None;
            self.upgrade_refuse(requester, "the server is exiting");
            return;
        }
        let busy = self.upgrade_blockers(requester);
        if !busy.is_empty() && now < p.deadline {
            return;
        }
        let Some(p) = self.upgrade.pending.take() else {
            return;
        };
        if !busy.is_empty() {
            let message = format!(
                "still running after {} s: {}",
                PREFLIGHT_WAIT.as_secs(),
                busy.join(", ")
            );
            self.upgrade_refuse(requester, &message);
            return;
        }
        let message = self.upgrade_run(&p);
        self.upgrade_refuse(requester, &message);
    }

    /// The work in flight a snapshot can't carry (decisions 10 and 26):
    /// `run-shell` and `if-shell` jobs, commands queued anywhere but the
    /// requester's queue, and command clients still being served (a
    /// client that has not said `Hello` yet among them). Control and
    /// attached clients are carried, so only their queues count.
    fn upgrade_blockers(&self, requester: Requester) -> Vec<String> {
        let mut out: Vec<String> = self
            .jobs
            .upgrade_blockers()
            .into_iter()
            .map(|command| format!("job '{command}'"))
            .collect();
        let own_queue: Option<QueueId> = match requester {
            Requester::Command { step, .. } => self.cmdq.queue_of(step),
            Requester::Handshake(_) => None,
        };
        let mut clients = std::collections::BTreeSet::new();
        for q in self.cmdq.busy_queues() {
            if Some(q) == own_queue {
                continue;
            }
            match q {
                None => out.push("commands on the global queue".to_owned()),
                Some(id) => {
                    clients.insert(id);
                }
            }
        }
        for (&id, c) in &self.clients {
            let served = c.hello.is_none() || (c.control.is_none() && c.attached.is_none());
            if served && !c.dead && !c.closing && Some(id) != requester.client() {
                clients.insert(id);
            }
        }
        for id in clients {
            let name = self.client_name(id);
            out.push(if name.is_empty() {
                "a client still connecting".to_owned()
            } else {
                format!("client {name}")
            });
        }
        out
    }

    /// Step 2, then the snapshot, dry run and exec
    /// ([`Server::exec_upgrade`]). Returns only if the server is still
    /// this image: why, for the requester.
    fn upgrade_run(&mut self, p: &Pending) -> String {
        // 2. Quiesce: nothing is read again before this returns.
        if let Err(message) = self.upgrade_flush(FLUSH_WAIT) {
            return message;
        }
        self.exec_upgrade(&p.binary)
    }

    /// Write every client's queued output out, waiting up to `wait` for
    /// slow sockets: a frame cut in half would garble the stream the new
    /// image takes over. A control client's lines go out first.
    ///
    /// # Errors
    /// The clients still behind at the end.
    fn upgrade_flush(&mut self, wait: Duration) -> Result<(), String> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let deadline = Instant::now() + wait;
        let control: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.control.is_some())
            .map(|(&id, _)| id)
            .collect();
        loop {
            for &id in &control {
                self.control_flush(id);
            }
            let behind = |s: &Self| -> Vec<ClientId> {
                s.clients
                    .iter()
                    .filter(|(_, c)| !c.dead && !c.out.is_empty())
                    .map(|(&id, _)| id)
                    .collect()
            };
            for id in behind(self) {
                self.flush_client(id);
            }
            let behind = behind(self);
            if behind.is_empty() {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let names: Vec<String> = behind.iter().map(|&id| self.client_name(id)).collect();
                return Err(format!("not taking its output: {}", names.join(", ")));
            }
            let mut fds: Vec<PollFd<'_>> = behind
                .iter()
                .filter_map(|id| self.clients.get(id))
                .map(|c| PollFd::new(&c.stream, PollFlags::OUT))
                .collect();
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: i64::from(left.min(RECHECK).subsec_nanos()),
            };
            let _ = poll(&mut fds, Some(&timeout));
        }
    }

    /// The upgrade is refused with `message`; the server carries on.
    fn upgrade_refuse(&mut self, requester: Requester, message: &str) {
        match requester {
            Requester::Handshake(id) => self.upgrade_result(id, false, message),
            Requester::Command { step, .. } => {
                let line = vtmux_core::rawstr::to_bytes(message).into_owned();
                self.cmdq_with_item(step, |server, item| item.error(server, line));
                self.cmdq_continue(step);
            }
        }
    }

    /// The frozen `UpgradeResult`, the last frame on the connection.
    fn upgrade_result(&mut self, id: ClientId, ok: bool, message: &str) {
        self.send(
            id,
            &ServerMsg::UpgradeResult {
                ok,
                message: message.to_owned(),
            },
        );
        if let Some(c) = self.clients.get_mut(&id) {
            c.closing = true;
        }
        self.flush_client(id);
    }
}

/// Step 1's check of the binary: absolute (the server's working directory
/// is not the requester's), a regular file, executable by this user.
fn check_binary(binary: &Path) -> Result<(), String> {
    if !binary.is_absolute() {
        return Err("binary must be an absolute path".to_owned());
    }
    let says = |e: &io::Error| format!("{}: {}", binary.display(), strerror(e));
    let meta = std::fs::metadata(binary).map_err(|e| says(&e))?;
    if !meta.is_file() {
        return Err(format!("{}: not a regular file", binary.display()));
    }
    rustix::fs::access(binary, rustix::fs::Access::EXEC_OK).map_err(|e| says(&e.into()))
}

/// What the server was started as (decision 22): `argv0` made absolute
/// the way `execvp` found it, a path (one with a `/`) from `cwd`, a name on
/// `path` (`PATH`, [`crate::shim::DEFAULT_PATH`] without one; an empty or
/// relative entry from `cwd`). Symlinks stay: they are resolved when an
/// upgrade is requested, so the server execs whatever a relink pointed the
/// name at since. `None` when the name is on no `PATH` entry.
#[must_use]
pub fn start_binary(argv0: &std::ffi::OsStr, path: Option<&str>, cwd: &Path) -> Option<PathBuf> {
    if argv0.as_bytes().contains(&b'/') {
        return Some(cwd.join(argv0));
    }
    path.unwrap_or(crate::shim::DEFAULT_PATH)
        .split(':')
        .map(|entry| cwd.join(entry).join(argv0))
        .find(|candidate| check_binary(candidate).is_ok())
}

#[cfg(test)]
mod tests;
