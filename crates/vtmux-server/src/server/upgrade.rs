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
//! 2. **The dry run** ([`Server::upgrade_check`], `resume.rs`): a snapshot
//!    for it to read, taken inside one loop turn, then the loop serves on
//!    (panes, clients, commands) while the binary reads it; each turn
//!    looks whether it has ended. A hanging binary holds only the
//!    requester, until the dry run's limit.
//! 3. Once it has passed, the wait for work in flight again (the server
//!    ran on), then **quiesce** and the `execve`
//!    ([`Server::exec_upgrade`]): in one go inside one loop turn, so no
//!    pane or client is read until it ends (what they send waits in the
//!    kernel), every client's queued output written out, the snapshot
//!    taken again with everything since the dry run's in it, the
//!    attached clients told, the exec. Any step failing leaves the old
//!    server running, and the requester is told why.
//!
//! The new image's half is [`super::resume`].

use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mio::{Interest, Token};
use vtmux_core::error::strerror;
use vtmux_core::protocol::{Hello, HelloExt, ServerMsg, UPGRADED_DETACH};
use vtmux_snapshot as snap;

use super::resume::{Check, Exec, Passed};
use super::{ClientId, Server};
use crate::cmdq::{QueueId, StepId};

/// What replaces the server image: [`crate::sys::upgrade::exec_in_place`]
/// (tests swap it). Returns only on failure.
/// The last argument is the `-S` for the new image, when it takes one.
pub type ExecFn = fn(&Path, OwnedFd, &[RawFd], Option<&Path>) -> io::Error;

/// How long the preflight waits for jobs and commands in flight
/// (decisions 10 and 26: "a few seconds", 3 s).
pub const PREFLIGHT_WAIT: Duration = Duration::from_secs(3);

/// A test hook read from the server's environment (`1`), which
/// `cargo xtask oracle replay --upgrade-every` sets: it inserts upgrades
/// between a scenario's steps and needs each one to leave everything a
/// scenario can see as it was. With it, an upgrade that would wait for
/// work in flight, or change what a client sees ([`Server::upgrade_shows`])
/// is refused at once with a message starting [`STRICT_REFUSAL`], so the
/// oracle tries after the next step instead. A `VTMUX_TEST_*` variable is a hook for the server
/// process, never part of the global environment (`global_environ`).
const STRICT_HOOK: &str = "VTMUX_TEST_UPGRADE_STRICT";

/// How a refusal under [`STRICT_HOOK`] starts.
pub const STRICT_REFUSAL: &str = "test hook: not now: ";

fn strict() -> bool {
    std::env::var_os(STRICT_HOOK).is_some_and(|v| v == "1")
}

/// How long the quiesce waits for clients to take their queued output.
pub const FLUSH_WAIT: Duration = Duration::from_secs(1);

/// How often a waiting preflight looks again, besides on every event.
const RECHECK: Duration = Duration::from_millis(50);

/// How often the loop looks at a dry run running, besides on SIGCHLD:
/// reading its stderr, so a child that writes much is not held long on a
/// full pipe.
const DRY_RUN_RECHECK: Duration = Duration::from_millis(20);

/// A test hook read from the server's environment: whole seconds that
/// replace the 3 s of both [`PREFLIGHT_WAIT`] and the dry run's limit
/// (`sys::DRY_RUN_LIMIT`, before what history adds). A test that stops
/// the server during either (`kill-server`, SIGTERM) sets it long, so the
/// wait cannot end first on a host that stalls the test for 3 s between
/// the request and the stop.
const WAIT_HOOK: &str = "VTMUX_TEST_UPGRADE_WAIT";

/// [`WAIT_HOOK`]'s value, if it is set to a number.
pub(super) fn test_wait() -> Option<Duration> {
    std::env::var(WAIT_HOOK)
        .ok()?
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// How long the wait for work in flight lasts: [`PREFLIGHT_WAIT`] (or
/// [`WAIT_HOOK`]'s), none under [`STRICT_HOOK`].
fn preflight_wait() -> Duration {
    if strict() {
        Duration::ZERO
    } else {
        test_wait().unwrap_or(PREFLIGHT_WAIT)
    }
}

/// How long a client told to re-execute has to re-attach (live-upgrade
/// spec, *Clients*): long enough for a busy host to start a binary.
pub const REATTACH_WAIT: Duration = Duration::from_secs(10);

/// An attached client between its connection and the next, across an
/// upgrade (decisions 4 and 21): sent an `UpgradeNotice` with `token`, it
/// re-executes and re-attaches presenting it, and is bound back to its
/// record ([`Server::reattach`]). Meanwhile it is drawn nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reattach {
    pub(crate) token: u64,
    /// When it is given up on; `None` while its notice is not sent yet.
    pub(crate) deadline: Option<Instant>,
    /// Its old connection has closed: it is re-executing.
    pub(crate) gone: bool,
}

impl Reattach {
    /// The token as the notice carries it ([`vtmux_core::protocol::is_upgrade_token`]).
    pub(crate) fn text(self) -> String {
        token_text(self.token)
    }
}

fn token_text(token: u64) -> String {
    format!("{token:016x}")
}

/// A token nobody can guess: the process's random hash keys (std seeds
/// them from the OS), never 0 (a snapshot's "no token").
fn fresh_token(salt: usize) -> u64 {
    use std::hash::BuildHasher as _;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::collections::hash_map::RandomState::new()
        .hash_one((salt, now))
        .max(1)
}

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

/// An upgrade past its first checks.
#[derive(Debug)]
pub(crate) struct Pending {
    requester: Requester,
    binary: PathBuf,
    /// When the wait for work in flight gives up.
    deadline: Instant,
    stage: Stage,
}

/// Where a pending upgrade stands.
#[derive(Debug)]
enum Stage {
    /// Waiting for the work in flight before the dry run's snapshot.
    Preflight,
    /// The dry run, running while the server serves.
    Checking(Check),
    /// The dry run passed: waiting for the work in flight again (the server
    /// ran on meanwhile) before the snapshot the exec gets.
    Passed(Passed),
}

/// How the upgrade went, held by the image that serves after the exec for
/// its requester until every client told to re-attach has, or has been
/// given up on ([`Server::upgrade_answer_due`]), so the answer names the
/// ones that did not come back.
#[derive(Debug)]
pub(crate) struct LaterAnswer {
    pub(crate) requester: snap::Requester,
    /// The frozen handshake's connection, kept across the exec.
    pub(crate) handshake: Option<OwnedFd>,
    /// `Ok` once the new image serves; `Err` with why when it does not.
    pub(crate) outcome: Result<(), String>,
}

/// The server's upgrade state.
#[derive(Debug)]
pub(crate) struct Upgrade {
    pub(crate) pending: Option<Pending>,
    pub(crate) exec: ExecFn,
    /// What the requester is warned of once the new image serves: the
    /// clients it detached ([`Server::detach_unfollowing`]) and those that
    /// did not come back ([`Server::reattach_timers`]).
    pub(crate) warnings: Vec<String>,
    /// The requester's answer, waiting for the clients re-attaching.
    pub(crate) answer: Option<LaterAnswer>,
}

impl Default for Upgrade {
    fn default() -> Self {
        Self {
            pending: None,
            exec: crate::sys::upgrade::exec_in_place,
            warnings: Vec::new(),
            answer: None,
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
            self.upgrade_start(requester, binary).err()
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
        binary: &Path,
    ) -> Result<(), String> {
        if self.upgrade.pending.is_some() {
            return Err("an upgrade is already running".to_owned());
        }
        if self.upgrade.answer.is_some() {
            // Its requester is not answered yet ([`Self::upgrade_answer_due`]).
            return Err(
                "the last upgrade is still waiting for its clients to re-attach".to_owned(),
            );
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
        self.upgrade_inside_job(requester)?;
        let binary = clean_path(binary);
        check_binary(&binary)?;
        self.upgrade.pending = Some(Pending {
            requester,
            binary,
            deadline: Instant::now() + preflight_wait(),
            stage: Stage::Preflight,
        });
        Ok(())
    }

    /// A requester running inside a `run-shell` or `if-shell` job
    /// (`run-shell 'tmux vtmux-upgrade'`) would wait for the upgrade
    /// while the upgrade waits for the job (decision 10): refused at once.
    /// run-shell shows only the job's output, not its stderr (without
    /// `-E`, cmd-run-shell.c and job.c `JOB_SHOWSTDERR`), so the refusal
    /// also goes into the job's output, where it is shown.
    ///
    /// # Errors
    /// The refusal, naming the job.
    fn upgrade_inside_job(&mut self, requester: Requester) -> Result<(), String> {
        let Some(pid) = requester
            .client()
            .and_then(|id| self.clients.get(&id)?.hello.as_ref())
            .and_then(|h| i32::try_from(h.pid).ok())
        else {
            return Ok(());
        };
        let jobs = self.jobs.upgrade_blocker_pids();
        let server = i32::try_from(std::process::id()).unwrap_or(0);
        let mut at = pid;
        // The client is the job's process or a descendant of it (a shell
        // that forks rather than execs its last command); a few levels
        // are enough, the bound only stops a loop.
        for _ in 0..32 {
            if at <= 1 || at == server {
                return Ok(());
            }
            if let Some((job, _, command)) = jobs.iter().find(|(_, p, _)| *p == at) {
                let message = format!(
                    "job '{command}' runs this vtmux-upgrade, and an upgrade waits for \
                     run-shell and if-shell jobs to finish: run vtmux-upgrade outside them"
                );
                let job = *job;
                self.jobs.note(job, &message);
                return Err(message);
            }
            let Some(parent) = crate::sys::upgrade::parent_pid(at) else {
                return Ok(());
            };
            at = parent;
        }
        Ok(())
    }

    /// When the loop must look at a waiting upgrade again: a dry run
    /// running is looked at every [`DRY_RUN_RECHECK`] (its end also
    /// wakes the loop, with SIGCHLD).
    pub(crate) fn upgrade_deadline(&self) -> Option<Instant> {
        let p = self.upgrade.pending.as_ref()?;
        Some(match &p.stage {
            Stage::Checking(c) => c.run.deadline().min(Instant::now() + DRY_RUN_RECHECK),
            Stage::Preflight | Stage::Passed(_) => p.deadline.min(Instant::now() + RECHECK),
        })
    }

    /// The dry run's child while it runs, for the loop's reaping to leave
    /// to it ([`crate::sys::upgrade::next_exited`]).
    pub(crate) fn upgrade_dry_run_pid(&self) -> Option<i32> {
        match &self.upgrade.pending.as_ref()?.stage {
            Stage::Checking(c) => c.run.pid(),
            Stage::Preflight | Stage::Passed(_) => None,
        }
    }

    /// The dry run's child has ended: reap it, its group killed. How it
    /// went is kept for [`Self::upgrade_turn`].
    pub(crate) fn upgrade_dry_run_ended(&mut self) {
        if let Some(Pending {
            stage: Stage::Checking(c),
            ..
        }) = self.upgrade.pending.as_mut()
        {
            let _ = c.run.poll();
        }
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
    /// went, refuse it if work is still in flight at the deadline, start
    /// the dry run once nothing is, see whether it has ended, and once it
    /// has passed and nothing is in flight again, run the rest.
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
            // A dry run running is killed with it.
            self.upgrade.pending = None;
            self.upgrade_tokens_void();
            return;
        }
        if self.shutdown {
            self.upgrade.pending = None;
            self.upgrade_tokens_void();
            self.upgrade_refuse(requester, "the server is exiting");
            return;
        }
        if !self.upgrade_checked(now) {
            return;
        }
        let Some(p) = self.upgrade.pending.as_ref() else {
            return;
        };
        let busy = self.upgrade_blockers(requester);
        if !busy.is_empty() && now < p.deadline {
            return;
        }
        let Some(p) = self.upgrade.pending.take() else {
            return;
        };
        let closes = if strict() {
            self.upgrade_shows()
        } else {
            Vec::new()
        };
        if !busy.is_empty() || !closes.is_empty() {
            self.upgrade_tokens_void();
            let message = if strict() {
                let mut what = busy;
                what.extend(closes);
                format!("{STRICT_REFUSAL}{}", what.join(", "))
            } else {
                format!(
                    "still running after {} s: {}",
                    preflight_wait().as_secs(),
                    busy.join(", ")
                )
            };
            self.upgrade_refuse(requester, &message);
            return;
        }
        let snap_requester = self.snap_requester(requester);
        let refused = match p.stage {
            Stage::Preflight => match self.upgrade_check(&p.binary, snap_requester) {
                Ok(check) => {
                    self.upgrade.pending = Some(Pending {
                        stage: Stage::Checking(check),
                        ..p
                    });
                    return;
                }
                Err(message) => message,
            },
            Stage::Passed(passed) => match self.upgrade_run(&p.binary, snap_requester, &passed) {
                Exec::Failed(message) => message,
                Exec::Again(check) => {
                    self.upgrade.pending = Some(Pending {
                        stage: Stage::Checking(check),
                        ..p
                    });
                    return;
                }
            },
            // Not reached: `upgrade_checked` let it through.
            stage @ Stage::Checking(_) => {
                self.upgrade.pending = Some(Pending { stage, ..p });
                return;
            }
        };
        self.upgrade_refuse(requester, &refused);
    }

    /// Whether a pending upgrade is past its dry run, or has none running:
    /// `false` while it runs, and once it has failed (the upgrade refused
    /// and dropped). One that passed waits for the work in flight again,
    /// from `now`.
    fn upgrade_checked(&mut self, now: Instant) -> bool {
        let Some(p) = self.upgrade.pending.as_mut() else {
            return false;
        };
        let Stage::Checking(check) = &mut p.stage else {
            return true;
        };
        let outcome = match check.run.poll() {
            Ok(None) => return false,
            Ok(Some(outcome)) => outcome,
            Err(e) => Err(format!("{}: {}", p.binary.display(), strerror(&e))),
        };
        let Some(p) = self.upgrade.pending.take() else {
            return false;
        };
        let Stage::Checking(check) = p.stage else {
            return false;
        };
        match outcome {
            Ok(checked) => {
                self.upgrade.pending = Some(Pending {
                    deadline: now + preflight_wait(),
                    stage: Stage::Passed(check.passed(checked)),
                    ..p
                });
                true
            }
            Err(message) => {
                self.upgrade_tokens_void();
                self.upgrade_refuse(p.requester, &message);
                false
            }
        }
    }

    /// Who asked, as the snapshot names them.
    fn snap_requester(&self, requester: Requester) -> Option<snap::Requester> {
        match requester {
            Requester::Handshake(id) => self
                .clients
                .get(&id)
                .map(|c| snap::Requester::Handshake(c.stream.as_raw_fd())),
            Requester::Command { client, .. } => client
                .and_then(|id| u32::try_from(id.0).ok())
                .map(snap::Requester::Client),
        }
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
            } else if let Some(what) = self.open_overlay(id) {
                // A key binding's command-prompt, confirm-before, menu or
                // popup waits in the client's queue until it closes
                // (cmd-command-prompt.c, cmd-display-menu.c).
                format!("client {name} has {what} open: close it, then run vtmux-upgrade again")
            } else {
                format!("client {name}")
            });
        }
        out
    }

    /// What attached client `id` has open that its queue waits for: a
    /// prompt, a popup or a menu.
    fn open_overlay(&self, id: ClientId) -> Option<&'static str> {
        let c = self.clients.get(&id)?;
        c.attached?;
        if c.popup.is_some() {
            Some("a popup")
        } else if c.status.prompt().is_some()
            || c.prompt_owner.is_some()
            || self.pane_prompt_waits_on(id)
        {
            Some("a prompt")
        } else if self
            .client_window(id)
            .is_some_and(|w| self.window_menu(w).is_some())
        {
            Some("a menu")
        } else {
            None
        }
    }

    /// Whether a prompt in a pane (`command-prompt -P`, as copy-mode-vi's
    /// `/` opens; cmd-command-prompt.c:94-110) holds a command in client
    /// `id`'s queue.
    fn pane_prompt_waits_on(&self, id: ClientId) -> bool {
        self.model
            .panes
            .values()
            .filter_map(|p| p.prompt_owner.as_ref()?.step())
            .any(|step| self.cmdq.queue_of(step) == Some(Some(id)))
    }

    /// What an upgrade now would change for a client to see, for
    /// [`STRICT_HOOK`] only. What the snapshot closes by design
    /// (`snapshot.rs`, `mode_out`; live-upgrade spec, *Open questions*):
    /// every pane mode but copy and clock mode, pane and mode prompts,
    /// status-line prompts, menus, and a popup running an editor. And what
    /// the re-attach can't show as it was (spec, *Clients*): a status
    /// message (the screen frozen under it is redrawn as it is now, and its
    /// timer runs on through the upgrade), a key the client's input holds
    /// for `escape-time` (its decoder starts again), a mouse drag (of a
    /// popup too, or a button held over one) or a click waiting for a
    /// second one (its mouse state starts again), and
    /// a locked or suspended client (it can't re-execute until it is
    /// back).
    fn upgrade_shows(&self) -> Vec<String> {
        use crate::mode::{Mode, PaneMode as _};
        let mut out = Vec::new();
        for p in self.model.panes.values() {
            for m in &p.modes {
                if !matches!(m, Mode::Copy(_) | Mode::Clock(_)) {
                    out.push(format!("would close {} in %{}", m.name(), p.id.0));
                }
            }
            if p.prompt.is_some() || p.mode_prompt.is_some() {
                out.push(format!("would close a prompt in %{}", p.id.0));
            }
        }
        for (id, w) in &self.model.windows {
            if w.menu.is_some() {
                out.push(format!("would close a menu in @{}", id.0));
            }
        }
        for &id in self.clients.keys() {
            let c = &self.clients[&id];
            if c.status.prompt().is_some() {
                out.push(format!("would close a prompt on {}", self.client_name(id)));
            }
            if c.popup.as_ref().is_some_and(|p| p.export().is_none()) {
                out.push(format!(
                    "would close an editor popup on {}",
                    self.client_name(id)
                ));
            }
            if c.status.message().is_some() {
                out.push(format!("a message on {}", self.client_name(id)));
            }
            if c.decoder.deadline().is_some() {
                out.push(format!("a key held on {}", self.client_name(id)));
            }
            // The re-attach starts the terminal again, which ends a drag
            // (tty_start_tty clears mouse_drag_flag and the callbacks)
            // and, with the client's mouse state new, a click's wait for
            // a second one.
            if c.mouse.dragging() {
                out.push(format!("a mouse drag on {}", self.client_name(id)));
            }
            // A popup's drag, or a button held over a popup: its drag
            // starts from the report before (popup_key_cb's `m->lb`),
            // which the re-attach's input starts again without.
            if c.popup
                .as_ref()
                .is_some_and(|p| p.dragging() || c.decoder.mouse_held())
            {
                out.push(format!("a popup drag on {}", self.client_name(id)));
            }
            if c.mouse.deadline().is_some() {
                out.push(format!("a click held on {}", self.client_name(id)));
            }
            if c.suspended {
                out.push(format!("{} locked or suspended", self.client_name(id)));
            }
        }
        out
    }

    /// The quiesce, then the snapshot again and the exec
    /// ([`Server::exec_upgrade`]). Returns only if the server is still
    /// this image: why, for the requester, or the dry run of a snapshot
    /// that needs one of its own.
    fn upgrade_run(
        &mut self,
        binary: &Path,
        requester: Option<snap::Requester>,
        passed: &Passed,
    ) -> Exec {
        // Quiesce: nothing is read again before this returns.
        if let Err(message) = self.upgrade_flush(FLUSH_WAIT) {
            self.upgrade_tokens_void();
            return Exec::Failed(message);
        }
        self.exec_upgrade(binary, requester, passed)
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

    /// In the image that ends up serving after the exec: tell client `id`,
    /// which ran `vtmux-upgrade`, how it went (`resume.rs`, `answer`). Its
    /// queue did not survive the exec, so the command ends here: a command
    /// client exits, 0 or 1 with why on stderr; a control client gets why
    /// as a line (as a waiting command's error after its `%end`) and reads
    /// on; an attached client gets why as a status message. A success
    /// carries the warnings (`Ok` with their lines, empty without), told
    /// the same way but as no error.
    pub(crate) fn upgrade_answer(&mut self, id: ClientId, outcome: Result<String, String>) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let (control, attached) = (c.control.is_some(), c.attached.is_some());
        if control {
            match outcome {
                Err(message) => self.control_error_line(id, &message),
                Ok(warnings) if !warnings.is_empty() => self.control_plain_line(id, &warnings),
                Ok(_) => {}
            }
            self.control_read(id);
        } else if attached {
            if let Err(message) | Ok(message) = outcome
                && !message.is_empty()
            {
                self.status_message_set(id, message);
            }
        } else {
            let (code, message) = match outcome {
                Ok(warnings) => (0, warnings),
                Err(message) => (1, message),
            };
            if !message.is_empty() {
                let mut line = message.into_bytes();
                line.push(b'\n');
                self.send(id, &ServerMsg::Stderr(line));
            }
            self.finish_client(id, code);
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

/// Attached clients across the exec (live-upgrade spec, *Clients*,
/// decisions 4, 21 and 25). In the old image: a token for every attached
/// terminal client whose binary follows upgrades
/// ([`vtmux_core::protocol::HelloExt::follows_upgrade`]), written into
/// the snapshot, and once the dry run has passed, the `UpgradeNotice`
/// carrying it. In whichever image serves next: the clients without one
/// are detached and named to the requester; each with one is drawn
/// nothing until it re-attaches with its token, which binds the new
/// connection to the record.
impl Server {
    /// Before the snapshot: a token for each attached terminal client that
    /// follows upgrades. A client still re-attaching from an upgrade that
    /// failed at the exec keeps the token it was sent.
    pub(crate) fn upgrade_tokens(&mut self) {
        for (&id, c) in &mut self.clients {
            let follows = c
                .hello
                .as_ref()
                .is_some_and(|h| h.tty.is_some() && h.ext.follows_upgrade);
            if follows
                && c.attached.is_some()
                && c.control.is_none()
                && !c.dead
                && !c.closing
                && c.reattach.is_none()
            {
                c.reattach = Some(Reattach {
                    token: fresh_token(id.0),
                    deadline: None,
                    gone: false,
                });
            }
        }
    }

    /// The dry run failed: no notice goes out, so the tokens are void.
    pub(crate) fn upgrade_tokens_void(&mut self) {
        for c in self.clients.values_mut() {
            if c.reattach.is_some_and(|r| r.deadline.is_none()) {
                c.reattach = None;
            }
        }
    }

    /// Sequence step 6: every client with a token is told to re-execute
    /// `binary` (the frozen `UpgradeNotice`), and the notices are written
    /// out before the exec, so none is cut in half. Each then has
    /// [`REATTACH_WAIT`] to re-attach, to this image too if the exec fails.
    ///
    /// Each terminal is restored first, as for a detach (`tty_stop_tty`):
    /// the client writes it out before the exec, so a client that does
    /// not come back (its exec fails, the new binary can't re-attach, its
    /// token is refused) leaves the terminal as it found it, and the
    /// re-attach starts it again ([`Server::reattach`]), so each upgrade
    /// pops the title it pushes.
    pub(crate) fn upgrade_notify(&mut self, binary: &Path) {
        let told: Vec<(ClientId, String)> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                let r = c.reattach.filter(|r| r.deadline.is_none())?;
                Some((id, r.text()))
            })
            .collect();
        let deadline = Instant::now() + REATTACH_WAIT;
        for (id, token) in told {
            self.send_teardown(id);
            if let Some(r) = self.clients.get_mut(&id).and_then(|c| c.reattach.as_mut()) {
                r.deadline = Some(deadline);
            }
            self.send(
                id,
                &ServerMsg::UpgradeNotice {
                    binary: binary.to_path_buf(),
                    token,
                },
            );
        }
        // A client that does not take a frame this small is stuck; it is
        // given up on at its deadline like any other.
        let _ = self.upgrade_flush(FLUSH_WAIT);
    }

    /// Decisions 4 and 25, in the image that serves after the exec: an
    /// attached terminal client without a token (its binary is from
    /// before clients follow upgrades, or the image before had no tokens)
    /// can't follow. It is detached with `[server upgraded: re-attach]`
    /// and named in the requester's answer.
    pub(crate) fn detach_unfollowing(&mut self) {
        let ids: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| {
                c.attached.is_some()
                    && c.control.is_none()
                    && c.reattach.is_none()
                    && !c.closing
                    && !c.dead
            })
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let name = self.client_name(id);
            self.exit_attached(id, 1, Some(UPGRADED_DETACH.to_owned()), false);
            self.upgrade.warnings.push(format!(
                "warning: client {name} can't follow an upgrade: detached with \
                 [{UPGRADED_DETACH}]"
            ));
        }
    }

    /// A `Hello` re-attaching with `token` on connection `new` (decision
    /// 21): the connection takes the place of the old one in the record
    /// the token was sent to, which keeps its id, creation time, flags,
    /// key table and session. No hook fires and no control client hears
    /// of it: to them the client never left. Its terminal is started again
    /// (`tty_start_tty`, as after `MSG_WAKEUP`: its features are the
    /// record's, so it is not asked again) and redrawn in full. A token
    /// no record holds (given up on, or its session went) is told to
    /// attach again.
    #[allow(clippy::too_many_lines)] // one step per record field; two units each added one
    pub(crate) fn reattach(&mut self, new: ClientId, hello: &Hello, token: &str) {
        let old = self
            .clients
            .iter()
            .find(|&(&id, c)| id != new && !c.dead && c.reattach.is_some_and(|r| r.text() == token))
            .map(|(&id, _)| id);
        let Some(old) = old else {
            self.finish_client_with(new, 1, Some(UPGRADED_DETACH.to_owned()));
            return;
        };
        // The token is the record's only to the user it was sent to: a
        // connection of another user is refused, and leaves it waiting.
        // Access to the record is as the access list gives it now
        // (`on_hello` checked it): one made read-only since stays so.
        let uid = |id: ClientId| self.clients.get(&id).and_then(|c| c.peer).map(|p| p.0);
        if uid(new).is_none() || uid(new) != uid(old) {
            self.refuse_client(new);
            return;
        }
        let read_only = self.clients.get(&new).is_some_and(|c| c.flags.read_only);
        // What the old connection still holds (keys typed before the
        // notice was read) is taken first.
        self.client_readable(old);
        let (Some(mut fresh), true) = (self.clients.remove(&new), self.clients.contains_key(&old))
        else {
            return;
        };
        let _ = self.poll.registry().deregister(&mut fresh.stream);
        let Some(c) = self.clients.get_mut(&old) else {
            return;
        };
        let mut stale = std::mem::replace(&mut c.stream, fresh.stream);
        let _ = self.poll.registry().deregister(&mut stale);
        drop(stale);
        let registered = self.poll.registry().register(
            &mut c.stream,
            Token(super::CLIENT_BASE + old.0),
            Interest::READABLE,
        );
        c.inbuf = fresh.inbuf;
        c.out = super::Outbox::default();
        c.unacked = super::Unacked::default();
        c.peer = fresh.peer;
        c.flags.read_only |= read_only;
        c.speaks = vtmux_core::protocol::Speaks::Full;
        c.reattach = None;
        c.exit_deadline = None;
        c.last_write = Instant::now();
        let size = c.rebind_hello(hello);
        // A new decoder for the new connection's bytes; the queries stand
        // as they were: those the terminal answered stay answered, and the
        // start timer runs on (tty_start_tty would start it again, but a
        // live upgrade asks the terminal nothing, so it changes nothing).
        let queries = c.decoder.queries();
        c.decoder = crate::input::Decoder::default();
        if let Some(term) = c.term.as_deref() {
            c.decoder.set_terminal(term);
        }
        c.decoder.set_queries(queries);
        c.mouse = crate::mouse::ClientMouse::default();
        c.title = None;
        c.passthrough.clear();
        c.pasting = false;
        c.window_cache = crate::render::WindowCache::default();
        c.compositor.invalidate();
        let term = c.term.clone();
        if registered.is_err() {
            self.drop_client(old);
            return;
        }
        self.send(old, &ServerMsg::Ready);
        if let Some(term) = term {
            let rows = self.tty_size(old).map_or(24, |(_, r)| u32::from(r));
            let clear = self.clear_on_attach();
            self.send(
                old,
                &ServerMsg::Frame(crate::render::restart(&term, clear, rows)),
            );
        }
        // The title the teardown popped, given again: with no upgrade the
        // terminal would still show it.
        let title = self
            .clients
            .get(&old)
            .and_then(|c| c.term.as_ref()?.set_title(c.terminal_title.as_deref()?));
        if let Some(bytes) = title {
            self.send(old, &ServerMsg::Frame(bytes));
        }
        // A resize while it was re-executing; or no cell size yet.
        if let Some((cols, rows, pixels, changed)) = size {
            self.tty_resize(old, cols, rows, pixels);
            if changed {
                self.resize_client(old, cols, rows);
            }
        }
        // Extended keys and focus reports asked for again, then the redraw.
        self.update_features(old);
        // Sizes worked out again, but not the views: tmux has no such event,
        // so a view that its last event left behind what it follows stays.
        // A window the sizes change works its own out when its event is
        // drained, and a resize meanwhile (above) did already.
        let views: Vec<_> = self.clients.iter().map(|(&id, c)| (id, c.view)).collect();
        self.recalculate_sizes();
        for (id, view) in views {
            if let Some(c) = self.clients.get_mut(&id) {
                c.view = view;
            }
        }
    }

    /// The old connection of a client sent its notice closed: it is
    /// re-executing, so its record stays for the re-attach. `false` for
    /// any other client.
    pub(crate) fn reattach_closed(&mut self, id: ClientId) -> bool {
        let Some(c) = self.clients.get_mut(&id) else {
            return false;
        };
        let Some(r) = c.reattach.as_mut().filter(|r| r.deadline.is_some()) else {
            return false;
        };
        r.gone = true;
        let _ = self.poll.registry().deregister(&mut c.stream);
        c.out = super::Outbox::default();
        true
    }

    /// When the loop must look at a client re-attaching again: its
    /// deadline, and while one is re-executing, every [`RECHECK`] to see
    /// whether its process has exited instead.
    pub(crate) fn reattach_deadline(&self) -> Option<Instant> {
        let reexecuting = self
            .clients
            .values()
            .any(|c| c.reattach.is_some_and(|r| r.gone));
        self.clients
            .values()
            .filter_map(|c| c.reattach?.deadline)
            .chain(reexecuting.then(|| Instant::now() + RECHECK))
            .min()
    }

    /// Clients that did not re-attach: within [`REATTACH_WAIT`], or one
    /// whose old connection closed and whose process has exited (an exec
    /// that failed, a binary that can't re-attach, a refused token: the
    /// process is the same across its exec, so it is not coming back).
    /// One whose old connection closed is lost as any client; one still on
    /// it did not act on its notice and is detached with
    /// `[server upgraded: re-attach]`. Each is named in the requester's
    /// answer while it waits ([`Self::upgrade_answer_due`]) and in the
    /// message log.
    pub(crate) fn reattach_timers(&mut self, now: Instant) {
        let due: Vec<(ClientId, bool)> = self
            .clients
            .iter_mut()
            .filter_map(|(&id, c)| {
                let r = c.reattach?;
                let exited = r.gone
                    && c.hello
                        .as_ref()
                        .and_then(|h| rustix::process::Pid::from_raw(i32::try_from(h.pid).ok()?))
                        .is_some_and(|pid| {
                            rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)
                        });
                if !exited && r.deadline.is_none_or(|d| d > now) {
                    return None;
                }
                c.reattach = None;
                Some((id, r.gone))
            })
            .collect();
        for (id, gone) in due {
            let name = self.client_name(id);
            let warning = if gone {
                self.drop_client(id);
                format!(
                    "warning: client {name} did not re-attach after the upgrade: its \
                     session is still there, attach again"
                )
            } else {
                self.exit_attached(id, 1, Some(UPGRADED_DETACH.to_owned()), false);
                format!(
                    "warning: client {name} did not act on the upgrade: detached with \
                     [{UPGRADED_DETACH}]"
                )
            };
            self.add_message(warning.clone());
            if self.upgrade.answer.is_some() {
                self.upgrade.warnings.push(warning);
            }
        }
    }

    /// In the image that serves after the exec: answer the requester
    /// (`resume.rs`) once no client told to re-attach is still on its
    /// way, so the answer names those that did not come back.
    pub(crate) fn upgrade_answer_later(&mut self, answer: LaterAnswer) {
        self.upgrade.answer = Some(answer);
        self.upgrade_answer_due();
    }

    /// The held answer goes out when no client is re-attaching any more:
    /// `Ok` with the warnings, one a line, or `Err` with why. The frozen
    /// handshake gets `UpgradeResult` on its connection, a client is told
    /// by [`Self::upgrade_answer`].
    pub(crate) fn upgrade_answer_due(&mut self) {
        if self.upgrade.answer.is_none()
            || self
                .clients
                .values()
                .any(|c| !c.dead && c.reattach.is_some_and(|r| r.deadline.is_some()))
        {
            return;
        }
        let Some(answer) = self.upgrade.answer.take() else {
            return;
        };
        let warnings = std::mem::take(&mut self.upgrade.warnings).join("\n");
        let outcome = answer.outcome.map(|()| warnings);
        match answer.requester {
            snap::Requester::Handshake(_) => {
                if let Some(fd) = answer.handshake {
                    write_result(fd, outcome);
                }
            }
            snap::Requester::Client(id) => {
                self.upgrade_answer(ClientId(id as usize), outcome);
            }
        }
    }
}

/// The frozen `UpgradeResult` for `outcome` on the handshake connection
/// `fd`, written at once, the connection closed after: the requester is
/// waiting for this one small frame, so the socket has room; a requester
/// that went is no matter.
pub(crate) fn write_result(fd: OwnedFd, outcome: Result<String, String>) {
    let (ok, message) = match outcome {
        Ok(warnings) => (true, warnings),
        Err(m) => (false, m),
    };
    let mut stream = std::os::unix::net::UnixStream::from(fd);
    let _ = vtmux_core::protocol::write_msg(&mut stream, &ServerMsg::UpgradeResult { ok, message });
}

/// An absolute `path` with no `.` and no `..` component, lexically (`/a/./b/../c` is
/// `/a/c`; `..` at the root stays at the root): the path the new image's
/// `argv[0]` shows. Symlinks are not resolved, so a link given as the
/// binary keeps its own path in `argv[0]` and `comm` (while `/proc/exe` and
/// the shim name its target), and a `..` after a link goes up from the
/// link's own directory, not its target's.
#[must_use]
pub(crate) fn clean_path(path: &Path) -> PathBuf {
    use std::path::Component;
    if !path.is_absolute() {
        return path.to_owned();
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
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
/// relative entry from `cwd`), cleaned lexically ([`clean_path`]). Symlinks stay: they are resolved when an
/// upgrade is requested, so the server execs whatever a relink pointed the
/// name at since. `None` when the name is on no `PATH` entry.
#[must_use]
pub fn start_binary(argv0: &std::ffi::OsStr, path: Option<&str>, cwd: &Path) -> Option<PathBuf> {
    if argv0.as_bytes().contains(&b'/') {
        return Some(clean_path(&cwd.join(argv0)));
    }
    path.unwrap_or(crate::shim::DEFAULT_PATH)
        .split(':')
        .map(|entry| clean_path(&cwd.join(entry).join(argv0)))
        .find(|candidate| check_binary(candidate).is_ok())
}

impl super::Client {
    /// The new connection's hello in the record: pid, version and the
    /// extension area. `(cols, rows, pixels, changed)` for the
    /// [`Server::tty_resize`] it needs: the terminal's size changed while
    /// the client re-executed, or its cell size is still 0 (an older writer,
    /// or a terminal that gave none), which asks as a fresh attach's
    /// would: the pixels the hello carries, else the size queries.
    fn rebind_hello(&mut self, hello: &Hello) -> Option<(u32, u32, (u32, u32), bool)> {
        let no_pixels = self.cell_pixels.0 == 0 || self.cell_pixels.1 == 0;
        let h = self.hello.as_mut()?;
        h.pid = hello.pid;
        h.version = hello.version;
        h.ext = HelloExt {
            follows_upgrade: hello.ext.follows_upgrade,
            reattach: None,
            pixels: hello.ext.pixels,
        };
        let (was, now) = (h.tty.as_ref()?, hello.tty.as_ref()?);
        let changed = (was.cols, was.rows) != (now.cols, now.rows);
        (changed || no_pixels).then_some((now.cols, now.rows, hello.ext.pixels, changed))
    }
}

#[cfg(test)]
mod tests;
