//! Jobs (job.c): shell commands the server runs for `run-shell` and
//! `if-shell`, with their output captured and their exit status reported
//! to a completion callback once both the output has closed and the
//! process has been reaped (`job_error_callback`, `job_check_died`), in
//! whichever order those happen.
//!
//! Also the one-shot timers `run-shell -d` waits on.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use mio::unix::SourceFd;
use mio::{Interest, Token};
use vtmux_core::ids::SessionId;

use crate::server::{ClientId, Server};
use crate::sys;

/// Job tokens are `JOB_BASE + id`, between the panes' and the clients'.
pub(crate) const JOB_BASE: usize = 1 << 31;

/// A job's ID (its token).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(pub usize);

/// How the process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Exited(i32),
    Signaled(i32),
}

impl JobStatus {
    /// `WIFEXITED(status) && WEXITSTATUS(status) == 0`.
    #[must_use]
    pub fn success(self) -> bool {
        self == Self::Exited(0)
    }
}

/// The completion callback (`job_complete_cb`): the output and status.
pub type Complete = Box<dyn FnOnce(&mut Server, Vec<u8>, JobStatus)>;

/// `job_update_cb` (job.c:309-323): the output so far, after each read that
/// brought bytes. The callback may take what it has used out of the buffer
/// (`evbuffer_readline`); what is left is the job's output at completion.
pub type Update = Rc<dyn Fn(&mut Server, &mut Vec<u8>)>;

/// A callback a timer runs.
pub type Deferred = Box<dyn FnOnce(&mut Server)>;

struct Job {
    /// The command line (`job->cmd`).
    command: String,
    /// The descriptor's number (`job->fd`), kept for the summary after
    /// the output has closed.
    raw_fd: i32,
    /// Our end of the socket pair, until the job closes its output.
    fd: Option<OwnedFd>,
    /// The process, until it is reaped.
    pid: Option<i32>,
    output: Vec<u8>,
    /// Bytes still to write to the job's stdin; its write side is shut
    /// once they are gone (`job_write_callback`).
    input: Vec<u8>,
    status: Option<JobStatus>,
    /// `JOB_NOWAIT`: the server may exit with it running.
    nowait: bool,
    update: Option<Update>,
    complete: Option<Complete>,
}

/// Every running job and pending timer.
#[derive(Default)]
pub struct Jobs {
    next: usize,
    running: BTreeMap<JobId, Job>,
    timers: Vec<(Instant, Deferred)>,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs")
            .field("next", &self.next)
            .field("running", &self.running.len())
            .field("timers", &self.timers.len())
            .finish()
    }
}

impl Jobs {
    /// `job_still_running`: a job the server waits for has not finished.
    #[must_use]
    pub fn still_running(&self) -> bool {
        self.running
            .values()
            .any(|j| !j.nowait && j.fd.is_some() && j.pid.is_some())
    }

    /// `job_print_summary` (job.c:436-450): the lines for every job,
    /// newest first, each `Job N: cmd [fd=, pid=, status=]`. A job that
    /// has been reaped has pid -1 (`job_check_died`) and its raw wait
    /// status.
    #[must_use]
    pub fn summary(&self) -> Vec<String> {
        self.running
            .values()
            .rev()
            .enumerate()
            .map(|(n, j)| {
                let status = match j.status {
                    None => 0,
                    Some(JobStatus::Exited(code)) => (code & 0xff) << 8,
                    Some(JobStatus::Signaled(sig)) => sig & 0x7f,
                };
                format!(
                    "Job {n}: {} [fd={}, pid={}, status={status}]",
                    j.command,
                    j.raw_fd,
                    j.pid.unwrap_or(-1)
                )
            })
            .collect()
    }

    /// The soonest timer.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|(at, _)| *at).min()
    }
}

/// What to run (`job_run`'s arguments).
pub struct JobSpec {
    pub command: String,
    /// The session whose environment the job gets (`environ_for_session`).
    pub session: Option<SessionId>,
    /// The directory it starts in; `None` (`job_run`'s NULL `cwd`) stays
    /// in the server's.
    pub cwd: Option<PathBuf>,
    /// What it reads on stdin (`bufferevent_write` after `job_run`).
    pub input: Vec<u8>,
    /// `JOB_SHOWSTDERR`.
    pub show_stderr: bool,
    /// `JOB_NOWAIT`.
    pub nowait: bool,
    /// `updatecb`: told of output as it arrives.
    pub update: Option<Update>,
}

impl Server {
    /// `job_run` (job.c:70-232): `/bin/sh -c command` (run-shell and
    /// if-shell do not ask for `JOB_DEFAULTSHELL`) in `cwd`, with the
    /// session's environment.
    pub(crate) fn job_run(&mut self, spec: &JobSpec, complete: Complete) -> io::Result<JobId> {
        let spawned = sys::job::spawn(&sys::job::JobSpec {
            shell: PathBuf::from("/bin/sh"),
            command: spec.command.clone(),
            cwd: spec.cwd.clone(),
            home: crate::server::find_home(&self.global_env),
            env: self.job_environ(spec.session),
            show_stderr: spec.show_stderr,
        })?;
        let id = JobId(self.jobs.next);
        self.jobs.next += 1;
        self.poll.registry().register(
            &mut SourceFd(&spawned.fd.as_raw_fd()),
            Token(JOB_BASE + id.0),
            Interest::READABLE | Interest::WRITABLE,
        )?;
        self.jobs.running.insert(
            id,
            Job {
                command: spec.command.clone(),
                raw_fd: spawned.fd.as_raw_fd(),
                fd: Some(spawned.fd),
                pid: Some(spawned.pid.as_raw_nonzero().get()),
                output: Vec::new(),
                input: spec.input.clone(),
                status: None,
                nowait: spec.nowait,
                update: spec.update.clone(),
                complete: Some(complete),
            },
        );
        // Output may already be waiting and the input may fit: edge-
        // triggered events only report what changes later.
        self.job_writable(id);
        self.job_readable(id);
        Ok(id)
    }

    /// `environ_for_session(s, !cfg_finished)` (environ.c:253-286): the
    /// global environment and the session's; once the config has run,
    /// `TERM` and friends as a pane gets them (a config's if-shell sees
    /// the outside `TERM`); `TMUX` with the session's ID or -1.
    pub(crate) fn job_environ(&self, session: Option<SessionId>) -> Vec<(String, String)> {
        let mut env = self.global_env.clone();
        if let Some(s) = session.and_then(|s| self.model.sessions.get(&s)) {
            s.env.copy_into(&mut env);
        }
        if self.cfg_finished {
            env.set("TERM", &self.default_terminal(), false);
            env.set("TERM_PROGRAM", "tmux", false);
            env.set("TERM_PROGRAM_VERSION", "3.8", false);
            env.set("COLORTERM", "truecolor", false);
        }
        let idx = session.map_or_else(|| "-1".to_owned(), |s| s.0.to_string());
        env.set(
            "TMUX",
            &format!("{},{},{idx}", self.socket.display(), std::process::id()),
            false,
        );
        env.for_process()
    }

    /// Write what the job's stdin takes of its input; once all of it is
    /// written (or the job stopped reading) the write side is shut, so the
    /// job sees EOF (`job_write_callback`, job.c:325-336).
    pub(crate) fn job_writable(&mut self, id: JobId) {
        let Some(job) = self.jobs.running.get_mut(&id) else {
            return;
        };
        let Some(fd) = job.fd.as_ref() else {
            return;
        };
        while !job.input.is_empty() {
            match rustix::io::write(fd, &job.input) {
                Ok(n) => {
                    job.input.drain(..n);
                }
                Err(rustix::io::Errno::AGAIN) => return,
                Err(rustix::io::Errno::INTR) => {}
                // The job closed its stdin: nothing more can be written.
                Err(_) => job.input.clear(),
            }
        }
        let _ = rustix::net::shutdown(fd, rustix::net::Shutdown::Write);
    }

    /// The job's output is readable: take all of it; at EOF the output is
    /// closed, and the job completes if it has also been reaped.
    pub(crate) fn job_readable(&mut self, id: JobId) {
        let Some(job) = self.jobs.running.get_mut(&id) else {
            return;
        };
        let Some(fd) = job.fd.as_ref() else {
            return;
        };
        let mut buf = [0u8; 16 * 1024];
        let mut got = false;
        let closed = loop {
            match rustix::io::read(fd, &mut buf) {
                Ok(0) => break true,
                Ok(n) => {
                    job.output.extend_from_slice(&buf[..n]);
                    got = true;
                }
                Err(rustix::io::Errno::AGAIN) => break false,
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => break true,
            }
        };
        // job_read_callback: the update callback sees what arrived.
        if got && let Some(update) = job.update.clone() {
            let mut output = std::mem::take(&mut job.output);
            update(self, &mut output);
            if let Some(job) = self.jobs.running.get_mut(&id) {
                job.output = output;
            }
        }
        if !closed {
            return;
        }
        let Some(job) = self.jobs.running.get_mut(&id) else {
            return;
        };
        if let Some(fd) = job.fd.take() {
            let _ = self
                .poll
                .registry()
                .deregister(&mut SourceFd(&fd.as_raw_fd()));
        }
        self.job_check_done(id);
    }

    /// `job_check_died`: whether `pid` was a job's; its status is kept and
    /// the job completes if its output has closed.
    pub(crate) fn job_died(&mut self, pid: i32, status: JobStatus) -> bool {
        let Some((&id, job)) = self
            .jobs
            .running
            .iter_mut()
            .find(|(_, j)| j.pid == Some(pid))
        else {
            return false;
        };
        job.pid = None;
        job.status = Some(status);
        self.job_check_done(id);
        true
    }

    fn job_check_done(&mut self, id: JobId) {
        let done = self
            .jobs
            .running
            .get(&id)
            .is_some_and(|j| j.fd.is_none() && j.pid.is_none());
        if !done {
            return;
        }
        let Some(mut job) = self.jobs.running.remove(&id) else {
            return;
        };
        if let Some(complete) = job.complete.take() {
            complete(self, job.output, job.status.unwrap_or(JobStatus::Exited(0)));
        }
    }

    /// `job_free` (job.c:269-287) on a running job: it is forgotten, its
    /// completion never runs, and SIGTERM goes to its process (only that
    /// pid, so what the shell started survives) unless it has been reaped.
    pub(crate) fn job_free(&mut self, id: JobId) {
        let Some(job) = self.jobs.running.remove(&id) else {
            return;
        };
        if let Some(pid) = job.pid.and_then(rustix::process::Pid::from_raw) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        if let Some(fd) = &job.fd {
            let _ = self
                .poll
                .registry()
                .deregister(&mut SourceFd(&fd.as_raw_fd()));
        }
    }

    /// `job_kill_all` (server.c:260): SIGTERM to every job still running.
    pub(crate) fn jobs_kill_all(&mut self) {
        for pid in self.jobs.running.values().filter_map(|j| j.pid) {
            if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
        }
    }

    /// Run `f` at `at` (`evtimer_add`, or `event_active` for now).
    pub(crate) fn timer_add(&mut self, at: Instant, f: Deferred) {
        self.jobs.timers.push((at, f));
    }

    /// Fire every timer that is due, in the order they were added.
    pub(crate) fn jobs_timers(&mut self, now: Instant) {
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.jobs.timers)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.jobs.timers = rest;
        for (_, f) in due {
            f(self);
        }
    }

    /// `server_client_get_cwd` (server-client.c:3082-3097): the startup
    /// client's directory while the config loads; an unattached client's
    /// own; the session's; the client's session's; home; `/`.
    pub(crate) fn client_cwd(&self, c: Option<ClientId>, s: Option<SessionId>) -> PathBuf {
        let hello_cwd = |c: ClientId| {
            self.clients
                .get(&c)
                .and_then(|c| c.hello.as_ref())
                .map(|h| h.cwd.clone())
        };
        if !self.cfg_finished
            && let Some(cwd) = self.cfg_client.and_then(hello_cwd)
        {
            return cwd;
        }
        if let Some(c) = c
            && self.client_session(c).is_none()
            && let Some(cwd) = hello_cwd(c)
        {
            return cwd;
        }
        let session_cwd = |s: SessionId| self.model.sessions.get(&s).map(|s| s.cwd.clone());
        if let Some(cwd) = s.and_then(session_cwd) {
            return cwd;
        }
        if let Some(cwd) = c.and_then(|c| self.client_session(c)).and_then(session_cwd) {
            return cwd;
        }
        crate::server::find_home(&self.global_env).unwrap_or_else(|| PathBuf::from("/"))
    }
}
