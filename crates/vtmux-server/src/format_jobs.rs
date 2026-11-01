//! `#()` in formats (format.c `format_job_get`, 394-460): shell commands whose
//! last line of output a format shows, run in the background and cached.
//!
//! Formats are expanded through a shared borrow of the server, so the cache
//! ([`FormatJobs`], format.c's `format_jobs` and each client's `c->jobs`) sits
//! behind an `Rc<RefCell<..>>` that a [`JobHandle`] in the format context
//! reaches. The hook only decides and records; starting and killing processes
//! needs the server and waits in [`FormatJobs::actions`] until
//! [`Server::format_jobs_flush`] runs at the end of the loop turn.
//!
//! What tmux does, which is not what one would guess:
//! - a job runs in the *client's* working directory with the *global*
//!   environment (`server_client_get_cwd(ft->client, NULL)`, no session), not
//!   the pane's;
//! - each client has its own tree, keyed by the format tag (window or pane ID)
//!   and the command as written; a format with no client uses the global one.
//!   So `display -p '#(echo hi)'` from a fresh client prints nothing, ever;
//!   only chained after a delay in one client does the output show;
//! - a job starts again only when it is not running and the second changed, or
//!   when its expanded command changed, or when forced (`refresh-client -S`);
//! - `<'cmd' not ready>` shows while the job runs, started over a second ago,
//!   with no output yet;
//! - freeing a job SIGTERMs its process only (`job_free`), and a client's jobs
//!   go with the client (`format_lost_client`); the rest go after an hour
//!   unused (`format_tidy_jobs`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::jobs::{JobId, JobSpec, JobStatus, Update};
use crate::server::{ClientId, Server};

/// `FORMAT_STATUS`: the expansion is for a status line, so the client redraws
/// it when the job reports.
pub const STATUS: u8 = 1;
/// `FORMAT_FORCE`: run the jobs again even if they ran this second.
pub const FORCE: u8 = 2;

/// `server_ev_tidy`'s period (server.c:156-170) and a job's idle limit.
const TIDY: Duration = Duration::from_secs(3600);

/// `FORMAT_WINDOW`: the tag of a window loop's items is this plus the ID.
pub const TAG_WINDOW: u32 = 0x4000_0000;
/// `FORMAT_PANE`: likewise for panes.
pub const TAG_PANE: u32 = 0x8000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FjId(u64);

/// The process behind a `format_job`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    None,
    /// A start waiting in [`FormatJobs::actions`], by its token.
    Pending(u64),
    Running(JobId),
}

/// One `#()` command's state (`struct format_job`).
#[derive(Debug)]
struct Fj {
    client: Option<ClientId>,
    /// The command as written (`fj->cmd`).
    cmd: String,
    /// What it expanded to when last looked at (`fj->expanded`).
    expanded: Option<String>,
    /// The second it last started or reported (`fj->last`).
    last: i64,
    /// The last line seen (`fj->out`).
    out: Option<String>,
    updated: bool,
    /// Some expansion for a status line used it (`fj->status`).
    status: bool,
    job: Slot,
}

/// Something only the server can do.
#[derive(Debug)]
pub(crate) enum Action {
    /// `job_free` on a running job.
    Free(JobId),
    Start(Start),
}

/// A job to start (`job_run` in `format_job_get`).
#[derive(Debug)]
pub(crate) struct Start {
    fj: FjId,
    token: u64,
    client: Option<ClientId>,
    command: String,
}

/// Where a job's cache entry lives: the client's tree, the tag, the command.
type Key = (Option<ClientId>, u32, String);

/// Every `#()` seen and its job.
#[derive(Debug, Default)]
pub struct FormatJobs {
    fjs: BTreeMap<FjId, Fj>,
    index: BTreeMap<Key, FjId>,
    next: u64,
    pub(crate) actions: Vec<Action>,
}

impl FormatJobs {
    /// `format_job_get` (format.c:394-460) up to the job itself: the text to
    /// put in the format (before it is expanded again), with the start and kill
    /// the caller must carry out recorded in `actions`.
    pub(crate) fn get(
        &mut self,
        client: Option<ClientId>,
        tag: u32,
        flags: u8,
        raw: &str,
        expanded: &str,
        now: i64,
    ) -> String {
        let key = (client, tag, raw.to_owned());
        let id = *self.index.entry(key).or_insert_with(|| {
            self.next += 1;
            let id = FjId(self.next);
            self.fjs.insert(
                id,
                Fj {
                    client,
                    cmd: raw.to_owned(),
                    expanded: None,
                    last: 0,
                    out: None,
                    updated: false,
                    status: false,
                    job: Slot::None,
                },
            );
            id
        });
        let token = self.next_token();
        let fj = self.fjs.get_mut(&id).expect("indexed");
        let force = if fj.expanded.as_deref() == Some(expanded) {
            flags & FORCE != 0
        } else {
            fj.expanded = Some(expanded.to_owned());
            true
        };
        let mut free = None;
        if force || (fj.job == Slot::None && fj.last != now) {
            free = Some(std::mem::replace(&mut fj.job, Slot::Pending(token)));
            fj.last = now;
            fj.updated = false;
        } else if fj.job != Slot::None && now - fj.last > 1 && fj.out.is_none() {
            fj.out = Some(format!("<'{}' not ready>", fj.cmd));
        }
        if flags & STATUS != 0 {
            fj.status = true;
        }
        let out = fj.out.clone().unwrap_or_default();
        match free {
            // A start that never ran is simply dropped; a running job is
            // killed (`if (force && fj->job != NULL) job_free(fj->job)`).
            Some(Slot::Pending(old)) => self
                .actions
                .retain(|a| !matches!(a, Action::Start(s) if s.token == old)),
            Some(Slot::Running(job)) => self.actions.push(Action::Free(job)),
            Some(Slot::None) | None => {}
        }
        if free.is_some() {
            self.actions.push(Action::Start(Start {
                fj: id,
                token,
                client,
                command: expanded.to_owned(),
            }));
        }
        out
    }

    fn next_token(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Forget every entry `drop` picks (`format_job_tidy`), returning the
    /// running jobs to kill; a start not yet made is cancelled.
    fn remove_where(&mut self, drop: impl Fn(&Fj) -> bool) -> Vec<JobId> {
        let gone: Vec<FjId> = self
            .fjs
            .iter()
            .filter(|(_, fj)| drop(fj))
            .map(|(&id, _)| id)
            .collect();
        let mut kill = Vec::new();
        for id in &gone {
            if let Some(fj) = self.fjs.remove(id)
                && let Slot::Running(job) = fj.job
            {
                kill.push(job);
            }
        }
        self.index.retain(|_, id| !gone.contains(id));
        self.actions
            .retain(|a| !matches!(a, Action::Start(s) if gone.contains(&s.fj)));
        kill
    }

    /// `format_tidy_jobs`: whatever has not run for an hour.
    pub(crate) fn tidy(&mut self, now: i64) -> Vec<JobId> {
        let hour = i64::try_from(TIDY.as_secs()).unwrap_or(i64::MAX);
        self.remove_where(|fj| !(fj.last > now || now - fj.last < hour))
    }

    /// `format_lost_client`: all of the client's.
    pub(crate) fn lost_client(&mut self, c: ClientId) -> Vec<JobId> {
        self.remove_where(|fj| fj.client == Some(c))
    }

    /// `format_job_update`'s output handling: the last complete line of
    /// `buf`, which is consumed up to the partial line left over.
    ///
    /// Returns the client whose status line to redraw: for a status format,
    /// at most once a second (`fj->last`).
    fn update(&mut self, id: FjId, buf: &mut Vec<u8>, now: i64) -> Option<ClientId> {
        let mut line = None;
        while let Some(next) = readline(buf) {
            line = Some(next);
        }
        let line = line?;
        let fj = self.fjs.get_mut(&id)?;
        fj.updated = true;
        fj.out = Some(vtmux_core::rawstr::from_vec(line));
        if fj.status && fj.last != now {
            fj.last = now;
            return fj.client;
        }
        None
    }

    /// `format_job_complete`: the job is over; what it left is its answer
    /// unless that is empty after output had been seen.
    fn complete(&mut self, id: FjId, mut buf: Vec<u8>) -> Option<ClientId> {
        let fj = self.fjs.get_mut(&id)?;
        fj.job = Slot::None;
        let line = readline(&mut buf).unwrap_or(buf);
        if !line.is_empty() || !fj.updated {
            fj.out = Some(vtmux_core::rawstr::from_vec(line));
        }
        if std::mem::take(&mut fj.status) {
            return fj.client;
        }
        None
    }
}

/// `evbuffer_readline` (`EVBUFFER_EOL_ANY`): up to the first CR or LF, which
/// with the CRs and LFs after it is consumed. `None` without one.
fn readline(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let eol = buf.iter().position(|&b| b == b'\r' || b == b'\n')?;
    let end = eol
        + buf[eol..]
            .iter()
            .take_while(|&&b| b == b'\r' || b == b'\n')
            .count();
    let mut line: Vec<u8> = buf.drain(..end).collect();
    line.truncate(eol);
    Some(line)
}

/// What a format context carries to reach the cache: who it is for
/// (`ft->client`), its tag and its flags.
#[derive(Clone, Debug)]
pub struct JobHandle {
    state: Rc<RefCell<FormatJobs>>,
    pub client: Option<ClientId>,
    pub tag: u32,
    pub flags: u8,
}

impl JobHandle {
    pub(crate) fn new(state: &Rc<RefCell<FormatJobs>>, client: Option<ClientId>) -> Self {
        Self {
            state: Rc::clone(state),
            client,
            tag: 0,
            flags: 0,
        }
    }

    /// `FORMAT_STATUS`, and `FORMAT_FORCE` for `refresh-client -S`.
    #[must_use]
    pub fn status(mut self, force: bool) -> Self {
        self.flags |= STATUS | if force { FORCE } else { 0 };
        self
    }

    /// The handle of a loop item or a window's own format tree.
    #[must_use]
    pub fn with_tag(mut self, tag: u32) -> Self {
        self.tag = tag;
        self
    }

    #[must_use]
    pub fn with_client(mut self, client: Option<ClientId>) -> Self {
        self.client = client;
        self
    }

    /// The text for `#(raw)` whose command expanded to `expanded`.
    pub(crate) fn get(&self, raw: &str, expanded: &str) -> String {
        self.state.borrow_mut().get(
            self.client,
            self.tag,
            self.flags,
            raw,
            expanded,
            crate::model::now_epoch(),
        )
    }
}

impl Server {
    /// Carry out what formats asked for since the last turn: kill the jobs
    /// to restart, start the new ones (`job_run` with the client's cwd, the
    /// global environment, and `JOB_NOWAIT`).
    pub(crate) fn format_jobs_flush(&mut self) {
        let actions = std::mem::take(&mut self.format_jobs.borrow_mut().actions);
        for action in actions {
            match action {
                Action::Free(job) => self.job_free(job),
                Action::Start(start) => self.format_job_start(start),
            }
        }
    }

    fn format_job_start(&mut self, start: Start) {
        let Start {
            fj,
            token,
            client,
            command,
        } = start;
        let update: Update = Rc::new(move |server, buf| {
            let now = crate::model::now_epoch();
            let redraw = server.format_jobs.borrow_mut().update(fj, buf, now);
            if let Some(c) = redraw {
                server.status_redraw_client(c);
            }
        });
        let complete_state = Rc::clone(&self.format_jobs);
        let complete = Box::new(move |server: &mut Server, buf: Vec<u8>, _: JobStatus| {
            let redraw = complete_state.borrow_mut().complete(fj, buf);
            if let Some(c) = redraw {
                server.status_redraw_client(c);
            }
        });
        let spec = JobSpec {
            command: command.clone(),
            session: None,
            cwd: Some(self.client_cwd(client, None)),
            input: Vec::new(),
            show_stderr: false,
            nowait: true,
            update: Some(update),
        };
        let started = self.job_run(&spec, complete);
        let mut jobs = self.format_jobs.borrow_mut();
        let Some(entry) = jobs.fjs.get_mut(&fj) else {
            return;
        };
        if entry.job != Slot::Pending(token) {
            return;
        }
        if let Ok(id) = started {
            entry.job = Slot::Running(id);
        } else {
            entry.job = Slot::None;
            entry.out = Some(format!("<'{}' didn't start>", entry.cmd));
        }
    }

    /// A handle on the jobs for formats made for `client`
    /// (`format_create(client, ...)`).
    pub(crate) fn job_handle(&self, client: Option<ClientId>) -> JobHandle {
        JobHandle::new(&self.format_jobs, client)
    }

    /// The first `server_tidy_event` runs an hour after the server starts.
    pub(crate) fn format_jobs_start_tidy(&mut self) {
        self.timer_add(
            Instant::now() + TIDY,
            Box::new(Server::format_jobs_tidy_event),
        );
    }

    /// `format_lost_client`: the client's jobs go with it.
    pub(crate) fn format_jobs_lost_client(&mut self, c: ClientId) {
        let kill = self.format_jobs.borrow_mut().lost_client(c);
        for job in kill {
            self.job_free(job);
        }
    }

    /// `server_tidy_event` (server.c:157-172): the idle jobs go, then it runs
    /// again in an hour.
    pub(crate) fn format_jobs_tidy_event(&mut self) {
        let kill = self
            .format_jobs
            .borrow_mut()
            .tidy(crate::model::now_epoch());
        for job in kill {
            self.job_free(job);
        }
        self.timer_add(
            Instant::now() + TIDY,
            Box::new(Server::format_jobs_tidy_event),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Option<ClientId> = Some(ClientId(1));

    fn starts(j: &FormatJobs) -> usize {
        j.actions
            .iter()
            .filter(|a| matches!(a, Action::Start(_)))
            .count()
    }

    #[test]
    fn readline_takes_lines_and_leaves_the_partial_one() {
        let mut b = b"a\r\nb\n\nc".to_vec();
        assert_eq!(readline(&mut b), Some(b"a".to_vec()));
        assert_eq!(readline(&mut b), Some(b"b".to_vec()));
        assert_eq!(readline(&mut b), None);
        assert_eq!(b, b"c");
    }

    #[test]
    fn first_look_starts_a_job_and_shows_nothing() {
        let mut j = FormatJobs::default();
        assert_eq!(j.get(C, 0, 0, "echo hi", "echo hi", 100), "");
        assert_eq!(starts(&j), 1);
    }

    #[test]
    fn a_job_runs_again_only_once_it_is_over_and_the_second_changed() {
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "c", "c", 100);
        let Some(Action::Start(s)) = j.actions.pop() else {
            panic!("no start");
        };
        j.fjs.get_mut(&s.fj).unwrap().job = Slot::Running(JobId(7));
        // Running: never again.
        j.get(C, 0, 0, "c", "c", 101);
        assert!(j.actions.is_empty());
        // Over, same second: not yet.
        j.complete(s.fj, b"x".to_vec());
        j.get(C, 0, 0, "c", "c", 100);
        assert!(j.actions.is_empty());
        // Over, a later second.
        assert_eq!(j.get(C, 0, 0, "c", "c", 101), "x");
        assert_eq!(starts(&j), 1);
    }

    #[test]
    fn not_ready_shows_after_a_second_with_no_output() {
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "sleep 5", "sleep 5", 100);
        let Some(Action::Start(s)) = j.actions.pop() else {
            panic!("no start");
        };
        j.fjs.get_mut(&s.fj).unwrap().job = Slot::Running(JobId(7));
        assert_eq!(j.get(C, 0, 0, "sleep 5", "sleep 5", 101), "");
        assert_eq!(
            j.get(C, 0, 0, "sleep 5", "sleep 5", 102),
            "<'sleep 5' not ready>"
        );
        // Output replaces the placeholder.
        j.update(s.fj, &mut b"hi\n".to_vec(), 102);
        assert_eq!(j.get(C, 0, 0, "sleep 5", "sleep 5", 103), "hi");
    }

    #[test]
    fn a_new_expansion_or_force_restarts_the_job_and_kills_the_old_one() {
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "c", "c1", 100);
        let Some(Action::Start(s)) = j.actions.pop() else {
            panic!("no start");
        };
        j.fjs.get_mut(&s.fj).unwrap().job = Slot::Running(JobId(7));
        j.get(C, 0, 0, "c", "c2", 100);
        assert!(matches!(j.actions[0], Action::Free(JobId(7))));
        assert_eq!(starts(&j), 1);
        j.actions.clear();
        j.fjs.get_mut(&s.fj).unwrap().job = Slot::Running(JobId(8));
        j.get(C, 0, FORCE, "c", "c2", 100);
        assert!(matches!(j.actions[0], Action::Free(JobId(8))));
    }

    #[test]
    fn a_restart_before_the_flush_cancels_the_pending_start() {
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "c", "c1", 100);
        j.get(C, 0, 0, "c", "c2", 100);
        assert_eq!(starts(&j), 1);
        assert!(!j.actions.iter().any(|a| matches!(a, Action::Free(_))));
    }

    #[test]
    fn clients_and_tags_have_their_own_jobs() {
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "c", "c", 100);
        j.get(None, 0, 0, "c", "c", 100);
        j.get(Some(ClientId(2)), 0, 0, "c", "c", 100);
        j.get(C, TAG_PANE | 3, 0, "c", "c", 100);
        assert_eq!(starts(&j), 4);
        j.get(C, 0, 0, "c", "c", 100);
        assert_eq!(starts(&j), 4);
    }

    #[test]
    fn complete_keeps_the_last_update_when_the_rest_is_empty() {
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "c", "c", 100);
        let Some(Action::Start(s)) = j.actions.pop() else {
            panic!("no start");
        };
        j.update(s.fj, &mut b"a\nb\n".to_vec(), 100);
        j.complete(s.fj, Vec::new());
        assert_eq!(j.get(C, 0, 0, "c", "c", 100), "b");
        // Never any output: the empty answer replaces nothing.
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "c", "c", 100);
        let Some(Action::Start(s)) = j.actions.pop() else {
            panic!("no start");
        };
        j.complete(s.fj, b"x\ny\nz".to_vec());
        // Only the first line of what is left: format_job_complete.
        assert_eq!(j.get(C, 0, 0, "c", "c", 100), "x");
    }

    #[test]
    fn tidy_and_lost_client() {
        let mut j = FormatJobs::default();
        j.get(C, 0, 0, "c", "c", 100);
        j.get(None, 0, 0, "d", "d", 100);
        // Nothing is an hour old yet.
        let kill = j.tidy(100 + 3599);
        assert!(kill.is_empty(), "{kill:?}");
        assert_eq!(j.fjs.len(), 2);
        // The client's go at once, with their pending starts.
        j.lost_client(ClientId(1));
        assert_eq!(j.fjs.len(), 1);
        assert_eq!(starts(&j), 1);
        j.tidy(100 + 3600);
        assert!(j.fjs.is_empty());
        assert_eq!(starts(&j), 0);
    }

    #[test]
    fn status_is_remembered_and_cleared_on_completion() {
        let mut j = FormatJobs::default();
        j.get(C, 0, STATUS, "c", "c", 100);
        let Some(Action::Start(s)) = j.actions.pop() else {
            panic!("no start");
        };
        // The second the job started: no redraw yet; a later one: redraw once.
        assert_eq!(j.update(s.fj, &mut b"a\n".to_vec(), 100), None);
        assert_eq!(j.update(s.fj, &mut b"b\n".to_vec(), 101), C);
        assert_eq!(j.update(s.fj, &mut b"c\n".to_vec(), 101), None);
        assert_eq!(j.complete(s.fj, Vec::new()), C);
        assert_eq!(j.complete(s.fj, Vec::new()), None);
    }
}
