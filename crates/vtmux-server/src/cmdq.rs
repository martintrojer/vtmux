//! The command queue (cmd-queue.c): one queue per client plus a global one
//! for work with no client (the config files, `run-shell -b` with no
//! client). Each queue runs its steps in order; a step that waits
//! (`CMD_RETURN_WAIT`) holds its queue until something continues it (a
//! job finishing, a `wait-for` signal, file I/O completing).
//!
//! A *batch* is what tmux calls the item's state and client: the
//! [`Item`] its commands share (output, exit status, client details). A
//! client's argv command, a control line, a key binding and a config load
//! are each a batch. Commands a running command adds (`if-shell`,
//! `run-shell -C`, `source-file`) join its batch and go right after it
//! (`cmdq_insert_after`); commands for another client start a batch of
//! their own at the end of that client's queue (`cmdq_append`). A batch
//! ends with one end step, which hands its results back.
//!
//! An error drops the rest of the failed command's group
//! (`cmdq_remove_group`): a config line, or a whole argv or control line.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

use vtmux_core::cmd::{CmdError, CommandCall};

use crate::commands::{self, Item, Output};
use crate::server::{ClientId, Server};

/// Which queue: a client's, or the global one (`None`).
pub type QueueId = Option<ClientId>;

/// Command lists to queue, each with where it came from.
pub type Groups = Vec<(Option<Rc<Source>>, Vec<CommandCall>)>;

/// A step's unique ID, for continuing it or inserting after it later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StepId(u64);

/// A batch's ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BatchId(u64);

/// Where a command came from (`cmd_get_source`): config-file errors with
/// no client carry `file:line: `.
#[derive(Debug, PartialEq, Eq)]
pub struct Source {
    pub file: String,
    pub line: u32,
}

/// What a step does.
#[derive(Debug)]
pub enum Work {
    Command {
        call: CommandCall,
        source: Option<Rc<Source>>,
    },
    /// `cfg_client_done`: the first client waits for the config.
    CfgClientWait,
    /// `cmd_source_file_complete_cb`: report the causes, one level less
    /// deep; `failed` if a file did not load.
    SourceDone { failed: bool },
    /// A batch's last step.
    End(End),
}

/// How a batch hands back its results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// `server_client_command_done` for a command client: output, then
    /// `Exit` unless it attached.
    Client,
    /// The same for a control client's argv command.
    ControlArgv,
    /// A control client's stdin line: read the next one.
    ControlLine,
    /// `cfg_done`: the config files have run.
    Config,
    /// Nothing to hand back (key bindings, appended commands).
    Quiet,
}

/// What a step carries besides its batch (`struct cmdq_state`): the
/// current target its commands start from, whether hooks are off, and
/// the formats a hook adds (`hook`, `hook_*`). Commands a step inserts
/// share its state (`cmdq_get_command(cmdlist, cmdq_get_state(item))`).
#[derive(Debug, Default)]
pub struct StepState {
    /// `state->current`.
    pub current: Option<crate::target::Found>,
    /// `CMDQ_STATE_NOHOOKS`: a hook's commands fire no hooks.
    pub no_hooks: bool,
    pub formats: Vec<(String, String)>,
}

#[derive(Debug)]
struct Step {
    id: StepId,
    batch: BatchId,
    state: Option<Rc<StepState>>,
    /// Global group number; 0 for steps no error removes.
    group: u64,
    work: Option<Work>,
    fired: bool,
    waiting: bool,
    /// Continued before the step said it waits: it does not wait.
    woken: bool,
}

/// Every queue and the batches their steps belong to.
#[derive(Debug, Default)]
pub struct Cmdq {
    queues: BTreeMap<QueueId, VecDeque<Step>>,
    batches: HashMap<BatchId, Item>,
    next_step: u64,
    next_batch: u64,
    next_group: u64,
    /// Inside [`Server::cmdq_process`]: new work is picked up by the
    /// running loop instead of a nested one.
    running: bool,
    /// The step firing now (`after` for `cmdq_insert_after`).
    current: Option<(QueueId, StepId)>,
    /// Something became runnable since the last pass.
    dirty: bool,
    /// The queue and state of the step firing now.
    current_state: Option<(QueueId, Rc<StepState>)>,
}

impl Cmdq {
    fn step_id(&mut self) -> StepId {
        self.next_step += 1;
        StepId(self.next_step)
    }

    /// Steps for `groups` in `batch`: each group (a config line, or one
    /// parsed list) with its source, every group of every list a fresh
    /// global group.
    fn command_steps(
        &mut self,
        batch: BatchId,
        groups: Groups,
        state: Option<&Rc<StepState>>,
    ) -> Vec<Step> {
        let mut steps = Vec::new();
        for (source, calls) in groups {
            let base = self.next_group;
            let mut top = 0;
            for call in calls {
                top = top.max(u64::from(call.group));
                let group = base + u64::from(call.group) + 1;
                let id = self.step_id();
                steps.push(Step {
                    id,
                    batch,
                    state: state.cloned(),
                    group,
                    work: Some(Work::Command {
                        call,
                        source: source.clone(),
                    }),
                    fired: false,
                    waiting: false,
                    woken: false,
                });
            }
            self.next_group = base + top + 2;
        }
        steps
    }

    fn work_step(&mut self, batch: BatchId, work: Work) -> Step {
        Step {
            id: self.step_id(),
            batch,
            state: None,
            group: 0,
            work: Some(work),
            fired: false,
            waiting: false,
            woken: false,
        }
    }

    fn find(&mut self, id: StepId) -> Option<(QueueId, usize)> {
        self.queues
            .iter()
            .find_map(|(&q, steps)| steps.iter().position(|s| s.id == id).map(|i| (q, i)))
    }

    /// `cmdq_insert_after`: `steps` go right after `after`, in order.
    /// Returns the last one inserted (the next `after` for a chain).
    fn insert_after(&mut self, after: StepId, steps: Vec<Step>) -> Option<StepId> {
        let (q, i) = self.find(after)?;
        let last = steps.last().map(|s| s.id);
        let queue = self.queues.get_mut(&q)?;
        for (n, s) in steps.into_iter().enumerate() {
            queue.insert(i + 1 + n, s);
        }
        self.dirty = true;
        last.or(Some(after))
    }

    /// `cmdq_continue`.
    fn continue_step(&mut self, id: StepId) {
        if let Some((q, i)) = self.find(id)
            && let Some(step) = self.queues.get_mut(&q).and_then(|s| s.get_mut(i))
        {
            // An item not fired yet has nothing to continue.
            if step.fired {
                step.waiting = false;
                step.woken = true;
                self.dirty = true;
            }
        }
    }

    /// The batch a step belongs to.
    fn batch_of(&mut self, id: StepId) -> Option<BatchId> {
        let (q, i) = self.find(id)?;
        Some(self.queues.get(&q)?.get(i)?.batch)
    }

    /// The state of a step.
    fn state_of(&mut self, id: StepId) -> Option<Rc<StepState>> {
        let (q, i) = self.find(id)?;
        self.queues.get(&q)?.get(i)?.state.clone()
    }

    /// `cmdq_running(NULL)` is a step whose state turns hooks off: the
    /// global queue is firing a hook's command.
    #[must_use]
    pub fn global_no_hooks(&self) -> bool {
        self.current_state
            .as_ref()
            .is_some_and(|(q, st)| q.is_none() && st.no_hooks)
    }

    /// `cmdq_remove_group`: later steps of `q` in `group`.
    fn remove_group(&mut self, q: QueueId, after: StepId, group: u64) {
        if group == 0 {
            return;
        }
        if let Some(steps) = self.queues.get_mut(&q) {
            let mut seen = false;
            steps.retain(|s| {
                if s.id == after {
                    seen = true;
                    return true;
                }
                !(seen && s.group == group)
            });
        }
    }

    /// Whether `q` has anything queued besides the step firing now.
    #[must_use]
    pub fn busy(&self, q: QueueId) -> bool {
        let current = self.current.map(|(_, s)| s);
        self.queues
            .get(&q)
            .is_some_and(|s| s.iter().any(|s| Some(s.id) != current))
    }

    /// Whether a pass would do something now.
    #[must_use]
    pub fn runnable(&self) -> bool {
        self.dirty
    }
}

/// How a batch is built: its item, the commands (and their source), and
/// the steps around them.
pub struct NewBatch {
    pub item: Item,
    pub groups: Groups,
    /// The commands' state (`None`: a fresh one).
    pub state: Option<Rc<StepState>>,
    /// Steps before the commands.
    pub before: Vec<Work>,
    pub end: End,
}

impl NewBatch {
    #[must_use]
    pub fn new(item: Item, calls: Vec<CommandCall>, end: End) -> Self {
        Self {
            item,
            groups: vec![(None, calls)],
            state: None,
            before: Vec::new(),
            end,
        }
    }
}

impl Server {
    /// `cmdq_append`: a new batch at the end of queue `q`. Returns the ID of
    /// its first step, `None` if `q`'s client has gone.
    pub(crate) fn cmdq_append(&mut self, q: QueueId, batch: NewBatch) -> Option<StepId> {
        // A client that has gone has no queue (`cmdq_fire_command` refuses
        // a dead client's items).
        if q.is_some_and(|c| !self.clients.contains_key(&c)) {
            return None;
        }
        let steps = self.batch_steps(batch);
        let first = steps[0].id;
        self.cmdq.queues.entry(q).or_default().extend(steps);
        self.cmdq.dirty = true;
        Some(first)
    }

    fn batch_steps(&mut self, b: NewBatch) -> Vec<Step> {
        let q = &mut self.cmdq;
        q.next_batch += 1;
        let id = BatchId(q.next_batch);
        q.batches.insert(id, b.item);
        let mut steps: Vec<Step> = b.before.into_iter().map(|w| q.work_step(id, w)).collect();
        steps.extend(q.command_steps(id, b.groups, b.state.as_ref()));
        steps.push(q.work_step(id, Work::End(b.end)));
        steps
    }

    /// `cmdq_insert_after(after, ...)`: commands that join `after`'s batch
    /// and run right after it. Returns the last step inserted.
    pub(crate) fn cmdq_insert_commands(&mut self, after: StepId, groups: Groups) -> Option<StepId> {
        let state = self.cmdq.state_of(after);
        self.cmdq_insert_with_state(after, groups, state.as_ref())
    }

    /// [`Self::cmdq_insert_commands`] with a state of their own (a hook's).
    pub(crate) fn cmdq_insert_with_state(
        &mut self,
        after: StepId,
        groups: Groups,
        state: Option<&Rc<StepState>>,
    ) -> Option<StepId> {
        let batch = self.cmdq.batch_of(after)?;
        let steps = self.cmdq.command_steps(batch, groups, state);
        self.cmdq.insert_after(after, steps)
    }

    /// A non-command step in `after`'s batch, right after it.
    pub(crate) fn cmdq_insert_work(&mut self, after: StepId, work: Work) -> Option<StepId> {
        let batch = self.cmdq.batch_of(after)?;
        let step = self.cmdq.work_step(batch, work);
        self.cmdq.insert_after(after, vec![step])
    }

    /// A whole new batch right after `after` (a key a command handed to a
    /// client, `server_client_handle_key_after`).
    pub(crate) fn cmdq_insert_batch(&mut self, after: StepId, batch: NewBatch) -> Option<StepId> {
        let steps = self.batch_steps(batch);
        self.cmdq.insert_after(after, steps)
    }

    /// `cmdq_continue`: a waiting step may finish.
    pub(crate) fn cmdq_continue(&mut self, id: StepId) {
        self.cmdq.continue_step(id);
    }

    /// Run `f` on the batch item of step `id` (gone if its client left),
    /// then hand its output on.
    pub(crate) fn cmdq_with_item<R>(
        &mut self,
        id: StepId,
        f: impl FnOnce(&mut Self, &mut Item) -> R,
    ) -> Option<R> {
        let batch = self.cmdq.batch_of(id)?;
        let mut item = self.cmdq.batches.remove(&batch)?;
        let r = f(self, &mut item);
        self.flush_item(&mut item);
        self.cmdq.batches.insert(batch, item);
        Some(r)
    }

    /// Continue whatever client `id`'s queue waits on (`wait-for`, file
    /// I/O): the first step of its queue.
    pub fn wake(&mut self, id: ClientId) {
        if let Some(step) = self.cmdq.queues.get(&Some(id)).and_then(|s| s.front()) {
            let step = step.id;
            self.cmdq.continue_step(step);
        }
    }

    /// `cmdq_free` for a client that is gone: its steps never run.
    pub(crate) fn cmdq_client_lost(&mut self, id: ClientId) {
        if let Some(steps) = self.cmdq.queues.remove(&Some(id)) {
            for s in steps {
                self.cmdq.batches.remove(&s.batch);
            }
        }
        self.cmdq.batches.retain(|_, item| item.client != Some(id));
    }

    /// `server_loop`'s `cmdq_next` passes: run every queue until none can
    /// go further. New work queued while it runs joins this loop.
    pub(crate) fn cmdq_process(&mut self) {
        if self.cmdq.running {
            return;
        }
        self.cmdq.running = true;
        loop {
            self.cmdq.dirty = false;
            let mut progressed = false;
            let queues: Vec<QueueId> = self.cmdq.queues.keys().copied().collect();
            for q in queues {
                progressed |= self.cmdq_next(q);
            }
            if !progressed && !self.cmdq.dirty {
                break;
            }
        }
        self.cmdq.queues.retain(|_, s| !s.is_empty());
        self.cmdq.running = false;
    }

    /// `cmdq_next`: run queue `q` until it is empty or waits.
    fn cmdq_next(&mut self, q: QueueId) -> bool {
        let mut progressed = false;
        loop {
            let Some(front) = self.cmdq.queues.get_mut(&q).and_then(|s| s.front_mut()) else {
                return progressed;
            };
            if front.waiting {
                return progressed;
            }
            let (id, batch, group) = (front.id, front.batch, front.group);
            let state = front.state.clone();
            if front.fired {
                // Continued after a wait: a client's file operation
                // reports first, or keeps waiting while it is open.
                front.woken = false;
                if let Some(c) = q
                    && let Some(mut item) = self.cmdq.batches.remove(&batch)
                {
                    let done = self.file_resume(c, &mut item);
                    self.flush_item(&mut item);
                    self.cmdq.batches.insert(batch, item);
                    if !done {
                        self.set_waiting(q, id);
                        return progressed;
                    }
                }
            } else {
                front.fired = true;
                front.woken = false;
                let work = front.work.take();
                progressed = true;
                self.cmdq.current = Some((q, id));
                self.cmdq.current_state = state.clone().map(|st| (q, st));
                let wait = match work {
                    Some(w) => self.fire(q, id, batch, group, w, state.as_deref()),
                    None => false,
                };
                self.cmdq.current = None;
                self.cmdq.current_state = None;
                if wait {
                    let woken = self
                        .cmdq
                        .queues
                        .get(&q)
                        .and_then(|s| s.iter().find(|s| s.id == id))
                        .is_some_and(|s| s.woken);
                    if !woken {
                        self.set_waiting(q, id);
                        return progressed;
                    }
                    // Already continued (file I/O done at once): finish
                    // it on the next pass, after its guard.
                    continue;
                }
            }
            if let Some(steps) = self.cmdq.queues.get_mut(&q)
                && let Some(i) = steps.iter().position(|s| s.id == id)
            {
                steps.remove(i);
            }
        }
    }

    fn set_waiting(&mut self, q: QueueId, id: StepId) {
        if let Some(s) = self
            .cmdq
            .queues
            .get_mut(&q)
            .and_then(|s| s.iter_mut().find(|s| s.id == id))
        {
            s.waiting = true;
        }
    }

    /// Fire one step; whether it waits.
    fn fire(
        &mut self,
        q: QueueId,
        id: StepId,
        batch: BatchId,
        group: u64,
        work: Work,
        state: Option<&StepState>,
    ) -> bool {
        match work {
            Work::Command { call, source } => {
                let Some(mut item) = self.cmdq.batches.remove(&batch) else {
                    return false;
                };
                item.step = Some(id);
                // load_cfg: the file's commands (and what they queue)
                // carry `current_file` in their state.
                let outer = std::mem::replace(
                    &mut self.info.current_file,
                    source.as_ref().map(|s| s.file.clone()),
                );
                item.source = source;
                // The step's state over the batch's item for this command:
                // a hook's commands start from the event's target, fire no
                // hooks, see its formats and, having no CMDQ_STATE_CONTROL,
                // get guard flags 0.
                let saved = (item.origin.current, item.control, item.no_hooks);
                if let Some(st) = state {
                    item.origin.current = st.current;
                    item.no_hooks = st.no_hooks;
                    if st.no_hooks {
                        item.control = item.control.map(|_| 0);
                    }
                    self.cmd_formats.clone_from(&st.formats);
                }
                // cmdq_merge_formats (cmd-queue.c:294-303): `command` is the
                // running command's name, under the state's own formats (an
                // after-* hook's `command` is the hooked command's).
                if !self.cmd_formats.iter().any(|(k, _)| k == "command") {
                    self.cmd_formats
                        .push(("command".to_owned(), call.entry.name.to_owned()));
                }
                let ok = commands::fire(self, &mut item, &call);
                self.info.current_file = outer;
                self.cmd_formats.clear();
                if item.borrow_client {
                    // The borrowed client's output, as end_quiet hands it.
                    self.end_quiet(&mut item);
                    item.client = None;
                    item.control = None;
                }
                (item.origin.current, item.control, item.no_hooks) = saved;
                self.flush_item(&mut item);
                let wait = std::mem::take(&mut item.blocked);
                if self.cmdq.queues.contains_key(&q) {
                    self.cmdq.batches.insert(batch, item);
                }
                if !ok {
                    self.cmdq.remove_group(q, id, group);
                    return false;
                }
                wait
            }
            Work::CfgClientWait => {
                if self.cfg_finished {
                    return false;
                }
                self.cfg_wait = Some(id);
                true
            }
            Work::SourceDone { failed } => {
                if let Some(mut item) = self.cmdq.batches.remove(&batch) {
                    commands::source_file::done(self, &mut item, failed);
                    self.flush_item(&mut item);
                    self.cmdq.batches.insert(batch, item);
                }
                false
            }
            Work::End(end) => {
                if let Some(item) = self.cmdq.batches.remove(&batch) {
                    self.end_batch(end, item);
                }
                false
            }
        }
    }

    fn end_batch(&mut self, end: End, mut item: Item) {
        let client = item.client;
        match (end, client) {
            (End::Client, Some(id)) => self.deliver(id, item),
            (End::ControlArgv, Some(id)) => self.control_command_done(id, item),
            (End::ControlLine, Some(id)) => {
                // An error after the guard (file I/O, cmdq_error from a
                // done callback) still sets c->retval.
                self.control_retval(id, item.status);
                self.control_take_output(id, &mut item);
                self.control_flush(id);
                self.control_read(id);
            }
            (End::Config, _) => self.cfg_done(),
            _ => self.end_quiet(&mut item),
        }
    }

    /// Hand an item's output to its client now, as tmux writes each
    /// command's output as it runs (`cmdq_print`, `cmdq_error`): a control
    /// client's stream, or a command client's stdout and stderr. An
    /// attached terminal client's output waits for the end of the batch.
    pub(crate) fn flush_item(&mut self, item: &mut Item) {
        let Some(id) = item.client else {
            item.output.clear();
            return;
        };
        if item.control.is_some() {
            self.control_take_output(id, item);
            self.control_flush(id);
        } else if self.client_session(id).is_none() {
            self.send_output(id, item);
        }
    }

    /// The end of a batch with no one to report to (a key binding): an
    /// attached terminal client gets its errors as status messages
    /// (`cmdq_error` -> `status_message_set`, cmd-queue.c:882); prints
    /// would go to view mode, which vtmux has not got, so they are dropped.
    fn end_quiet(&mut self, item: &mut Item) {
        self.flush_item(item);
        let Some(id) = item.client else { return };
        for out in std::mem::take(&mut item.output) {
            if let Output::Stderr(line) = out {
                let text = vtmux_core::rawstr::from_bytes(&line);
                let mut msg =
                    vtmux_core::rawstr::to_lossy(text.trim_end_matches('\n')).into_owned();
                // cmdq_error: `*msg = toupper(*msg)`.
                if let Some(first) = msg.get(..1) {
                    let upper = first.to_ascii_uppercase();
                    msg.replace_range(..1, &upper);
                }
                self.status_message_set(id, msg);
            }
        }
    }
}

/// The error line of a failed command, as `cmdq_error` gets it.
#[must_use]
pub fn error_bytes(e: &CmdError) -> Vec<u8> {
    match e {
        CmdError::Edge(edge) => edge.to_bytes(),
        other => other.to_bytes(),
    }
}
