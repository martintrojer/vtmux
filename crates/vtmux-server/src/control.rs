//! Control mode (`-C`): commands read line by line from the client's stdin,
//! each command's output wrapped in `%begin`/`%end`/`%error` guards, and
//! `%`-notifications for model changes and pane output (control.c,
//! control-notify.c).
//!
//! A control client is an ordinary [`Client`](crate::server) with a
//! [`ControlState`]. Its argv runs first (guard flags 0); if that left it
//! attached, stdin is read (guard flags 1, cmd-queue.c:594), otherwise it
//! exits at once without reading (server-client.c:2827-2831). Notifications
//! never appear inside an open guard: they are deferred until the outermost
//! guard closes (control.c `control_notify_write`, `control_write_guard`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vtmux_core::cmd::{self, ParseOpts};
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::protocol::ServerMsg;

use crate::commands::Item;
use crate::control_queue as queue;
use crate::server::{ClientId, GlobalEnv, Server};
use crate::target::Origin;

/// Why a control client leaves; decides its exit status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// EOF, an empty line, its session gone, or an unattached argv command
    /// (`CLIENT_EXIT_RETURN`): the status is `c->retval`.
    Return,
    /// `detach-client` (`MSG_DETACH`): the client exits 0 and, never having
    /// had `MSG_READY`, prints no reason (client.c `client_dispatch_wait`).
    Detach,
    /// The server is going away (`MSG_SHUTDOWN`): status 0, no reason.
    Shutdown,
    /// `detach-client -E` (`MSG_EXEC`): the client runs the command in
    /// place of itself.
    Exec { command: String, shell: PathBuf },
}

/// The timestamp, number and flags a guard pair shares.
#[derive(Clone, Copy, Debug)]
pub struct Guard {
    time: i64,
    number: u32,
    flags: u8,
}

/// Per-client control-mode state (control.c `struct control_state`).
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent states (control_ready, EOF, CLIENT_EXITED, CLIENT_CONTROL_DISCARD)"
)]
pub struct ControlState {
    /// Unconsumed stdin bytes.
    pub(crate) inbuf: Vec<u8>,
    /// Stdin is read (`control_ready`, after the argv command attached).
    ready: bool,
    eof: bool,
    guard_depth: u32,
    deferred: Vec<Vec<u8>>,
    /// Lines (one `control_write` each) not yet handed to the connection.
    out: Pending,
    /// `CLIENT_EXIT` with how the exit is reported.
    exit: Option<Exit>,
    /// `Exit` has been sent.
    finished: bool,
    /// `c->retval`: 1 once any command failed (cmd-queue.c `cmdq_error`).
    retval: i32,
    /// `cs->panes`: panes the client turned off or paused
    /// (`refresh-client -A`); a pane not here sends its output.
    panes: std::collections::BTreeMap<PaneId, PaneOutput>,
    /// `cs->windows` (`refresh-client -C @N:XxY`).
    windows: std::collections::BTreeMap<WindowId, (u32, u32)>,
    /// `c->tty.sx`, `c->tty.sy` (`refresh-client -C XxY`).
    size: Option<(u32, u32)>,
    /// `cs->subs` (`refresh-client -B`).
    pub(crate) subs: crate::control_subs::Subs,
    /// Lines and pane output not yet handed to the connection, in order
    /// (`cs->all_blocks` and each pane's `cp->blocks`).
    queue: queue::Queue,
    /// `CLIENT_CONTROL_DISCARD`: the replies held outgrew
    /// [`queue::MAXIMUM_REPLY_BUFFER`]; nothing more is written.
    discard: bool,
    /// `c->exit_message` with [`Exit::Return`]: `too far behind`.
    exit_message: Option<String>,
}

/// Protocol bytes not yet handed to the connection, kept by line: each
/// `control_write` is checked against the reply cap by itself
/// (`control_check_reply_buffer`), so a stream can stop between any two.
#[derive(Debug, Default)]
struct Pending {
    bytes: Vec<u8>,
    /// Where each line ends in `bytes`.
    ends: Vec<usize>,
}

impl Pending {
    /// A line: `text` as it is, which ends in its newline.
    fn add(&mut self, text: &[u8]) {
        self.bytes.extend_from_slice(text);
        self.ends.push(self.bytes.len());
    }

    fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The lines, each with its newline.
    fn lines(&self) -> impl Iterator<Item = &[u8]> {
        let mut from = 0;
        self.ends.iter().map(move |&end| {
            let line = &self.bytes[from..end];
            from = end;
            line
        })
    }

    /// Lines before these.
    fn prepend(&mut self, lines: &[Vec<u8>]) {
        let mut all = Self::default();
        for line in lines {
            all.add(line);
        }
        for line in self.lines() {
            all.add(line);
        }
        *self = all;
    }
}

/// `CONTROL_PANE_OFF` and `CONTROL_PANE_PAUSED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneOutput {
    Off,
    Paused,
}

/// What a snapshot carries of a control client (D27). Its exit, held
/// notifications and unflushed bytes are not carried: an upgrade runs
/// between commands, with every guard closed and the output flushed.
#[derive(Debug, Default)]
pub struct ControlParts {
    pub inbuf: Vec<u8>,
    pub ready: bool,
    pub eof: bool,
    pub guard_depth: u32,
    pub retval: i32,
    pub panes: Vec<(PaneId, PaneOutput)>,
    pub windows: Vec<(WindowId, (u32, u32))>,
    pub size: Option<(u32, u32)>,
    pub subs: crate::control_subs::Subs,
    pub held: Vec<queue::Held>,
}

impl ControlState {
    /// [`ControlParts`] of this client; its pane bytes from `streams`.
    #[must_use]
    pub fn parts(&self, streams: &queue::Streams) -> ControlParts {
        let (items, due) = self.subs.parts();
        ControlParts {
            inbuf: self.inbuf.clone(),
            ready: self.ready,
            eof: self.eof,
            guard_depth: self.guard_depth,
            retval: self.retval,
            panes: self.panes.iter().map(|(p, o)| (*p, *o)).collect(),
            windows: self.windows.iter().map(|(w, s)| (*w, *s)).collect(),
            size: self.size,
            subs: crate::control_subs::Subs::from_parts(items, due),
            held: self.queue.held(streams),
        }
    }

    /// A control client's state from [`Self::parts`]; its pane bytes go
    /// back into `streams`.
    #[must_use]
    pub fn from_parts(p: ControlParts, streams: &mut queue::Streams) -> Self {
        Self {
            inbuf: p.inbuf,
            ready: p.ready,
            eof: p.eof,
            guard_depth: p.guard_depth,
            retval: p.retval,
            panes: p.panes.into_iter().collect(),
            windows: p.windows.into_iter().collect(),
            size: p.size,
            subs: p.subs,
            queue: queue::Queue::from_held(p.held, streams),
            ..Self::default()
        }
    }

    /// The client's size: what `refresh-client -C` set, else 80x24
    /// (server-client.c:299).
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.size.unwrap_or((80, 24))
    }

    /// `control_get_window_size`: a size `refresh-client -C @N:XxY` gave.
    #[must_use]
    pub fn window_size(&self, w: WindowId) -> Option<(u32, u32)> {
        self.windows.get(&w).copied()
    }

    fn write_line(&mut self, line: &[u8]) {
        let mut text = Vec::with_capacity(line.len() + 1);
        text.extend_from_slice(line);
        text.push(b'\n');
        self.out.add(&text);
    }

    /// `control_write_guard`: the only place guard depth changes.
    fn guard(&mut self, kind: &str, g: Guard) {
        if kind == "begin" {
            self.guard_depth += 1;
        }
        let line = format!("%{kind} {} {} {}", g.time, g.number, g.flags);
        self.write_line(line.as_bytes());
        if kind != "begin" && self.guard_depth > 0 {
            self.guard_depth -= 1;
            if self.guard_depth == 0 {
                for line in std::mem::take(&mut self.deferred) {
                    self.write_line(&line);
                }
            }
        }
    }

    /// `control_notify_write`: deferred while a guard is open.
    fn notify(&mut self, line: Vec<u8>) {
        if self.guard_depth == 0 {
            self.write_line(&line);
        } else {
            self.deferred.push(line);
        }
    }

    /// `CONTROL_SHOULD_NOTIFY_CLIENT`: not once the client is exiting.
    fn wants_notifications(&self) -> bool {
        self.exit.is_none() && !self.finished
    }

    /// The client is let go: `too far behind` (`c->exit_message`), and
    /// its pane output goes (`control_discard`).
    fn too_far_behind(&mut self) {
        if self.exit.is_none() {
            self.exit = Some(Exit::Return);
            self.exit_message = Some("too far behind".to_owned());
        }
        self.queue.discard_output();
    }

    /// `control_check_reply_buffer` (control.c:429-451): whether `added`
    /// bytes of lines are dropped, because the client (`unwritten` bytes
    /// unwritten) holds too many already; the first time, it is let go.
    fn check_reply_buffer(&mut self, unwritten: usize, added: usize) -> bool {
        if self.discard {
            return true;
        }
        if unwritten + self.queue.line_bytes() + added < queue::MAXIMUM_REPLY_BUFFER {
            return false;
        }
        self.too_far_behind();
        self.discard = true;
        true
    }
}

/// `%output %<pane> <data>`: bytes below space and `\` as `\ooo`, the rest
/// raw (control.c `control_append_data`).
#[must_use]
pub fn output_line(pane: PaneId, data: &[u8]) -> Vec<u8> {
    escape_output(format!("%output {pane} ").into_bytes(), data)
}

/// `%extended-output %<pane> <age> : <data>` for a client with
/// `pause-after`: `age` is how long, in milliseconds, the oldest of the
/// data waited (control.c `control_append_data`).
#[must_use]
pub fn extended_output_line(pane: PaneId, age: Duration, data: &[u8]) -> Vec<u8> {
    let head = format!("%extended-output {pane} {} : ", age.as_millis());
    escape_output(head.into_bytes(), data)
}

fn escape_output(mut line: Vec<u8>, data: &[u8]) -> Vec<u8> {
    for &b in data {
        if b < b' ' || b == b'\\' {
            line.extend_from_slice(format!("\\{b:03o}").as_bytes());
        } else {
            line.push(b);
        }
    }
    line
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

impl Server {
    /// The control clients, in client order.
    fn control_ids(&self) -> Vec<ClientId> {
        self.clients
            .iter()
            .filter_map(|(&id, c)| c.control.is_some().then_some(id))
            .collect()
    }

    fn control_mut(&mut self, id: ClientId) -> Option<&mut ControlState> {
        self.clients.get_mut(&id)?.control.as_mut()
    }

    /// Hand buffered protocol bytes to the connection, or queue them
    /// behind the pane output held (`control_write_line`).
    pub(crate) fn control_flush(&mut self, id: ClientId) {
        if self.control_flush_lines(id) {
            self.control_pump(id);
        }
    }

    /// [`Self::control_flush`] without writing what is queued: whether
    /// the lines were queued. Each line is checked against the reply cap
    /// by itself (`control_write_line`), with what the connection holds
    /// and what this flush has handed it, so the stream can stop at any
    /// line: the rest of the flush is dropped.
    fn control_flush_lines(&mut self, id: ClientId) -> bool {
        let Some(c) = self.clients.get_mut(&id) else {
            return false;
        };
        let unwritten = c.unwritten();
        let Some(cs) = c.control.as_mut() else {
            return false;
        };
        if cs.out.is_empty() {
            return false;
        }
        let out = std::mem::take(&mut cs.out);
        let queued = !cs.queue.is_empty();
        let mut sent = Vec::new();
        for line in out.lines() {
            if cs.check_reply_buffer(unwritten + sent.len(), line.len()) {
                break;
            }
            if queued {
                cs.queue.push_line(line.to_vec());
            } else {
                sent.extend_from_slice(line);
            }
        }
        if !sent.is_empty() {
            self.send(id, &ServerMsg::Stdout(sent));
        }
        queued
    }

    /// `control_write_callback` (control.c:925-958): move what is queued
    /// for client `id` to its connection until [`queue::BUFFER_HIGH`] is
    /// unwritten there. Pane output goes a little at a time, the panes
    /// with output queued taking turns ([`queue::Queue::next_pane`]), and
    /// is escaped only now, so a slow reader costs the pane bytes it has
    /// not been sent, not their copies. A line goes as soon as nothing
    /// queued before it is left (`control_flush_all_blocks`), whatever the
    /// room.
    pub(crate) fn control_pump(&mut self, id: ClientId) {
        let now = Instant::now();
        if let Some(cs) = self.control_mut(id) {
            cs.queue.begin();
        }
        loop {
            self.control_flush_lines(id);
            let Some(c) = self.clients.get_mut(&id).filter(|c| !c.dead && !c.closing) else {
                return;
            };
            let unwritten = c.unwritten();
            let extended = c.flags.pause_after.is_some();
            let Some(cs) = c.control.as_mut() else {
                return;
            };
            if let Some(line) = cs.queue.take_line() {
                self.send(id, &ServerMsg::Stdout(line));
                continue;
            }
            let Some(p) = cs.queue.next_pane(unwritten) else {
                return;
            };
            // control_write_pending, control_window_pane: the output of a
            // pane gone, closed or no longer in the client's session goes.
            let present = self.model.panes.get(&p).is_some_and(|x| {
                x.io.is_some()
                    && c.attached
                        .is_some_and(|s| self.model.index_of(s, x.window).is_some())
            });
            if !present {
                cs.queue.discard_pane(p);
                cs.queue.end_turn();
                continue;
            }
            if self.control_check_age(id, p, now) {
                if let Some(cs) = self.control_mut(id) {
                    cs.queue.end_turn();
                }
                continue;
            }
            let Some(cs) = self.clients.get_mut(&id).and_then(|c| c.control.as_mut()) else {
                return;
            };
            let Some((pane, data, age)) = cs.queue.take_output(&self.control_streams, now) else {
                cs.queue.end_turn();
                continue;
            };
            let mut line = if extended {
                extended_output_line(pane, age, &data)
            } else {
                output_line(pane, &data)
            };
            line.push(b'\n');
            self.send_as(
                id,
                &ServerMsg::Stdout(line),
                crate::server::OutKind::PaneOutput,
            );
        }
    }

    /// `control_check_age` (control.c:583-616): when the oldest output of
    /// `pane` held for client `id` is older than `pause-after` allows, the
    /// pane is paused (`%pause`); without `pause-after`, when it is
    /// [`queue::maximum_age`] old, the client is let go. Whether either
    /// happened.
    fn control_check_age(&mut self, id: ClientId, pane: PaneId, now: Instant) -> bool {
        let Some(c) = self.clients.get_mut(&id) else {
            return false;
        };
        let pause = c.flags.pause_after;
        let Some(cs) = c.control.as_mut() else {
            return false;
        };
        let Some(read) = cs.queue.oldest_of(pane) else {
            return false;
        };
        // get_timer() counts milliseconds: output read this millisecond
        // has no age.
        let age = now.saturating_duration_since(read);
        if age.as_millis() == 0 {
            return false;
        }
        if let Some(pause) = pause {
            if age < Duration::from_millis(pause) {
                return false;
            }
            cs.panes.insert(pane, PaneOutput::Paused);
            cs.queue.discard_pane(pane);
            cs.notify(format!("%pause {pane}").into_bytes());
        } else {
            if age < queue::maximum_age() {
                return false;
            }
            cs.too_far_behind();
        }
        true
    }

    /// End of a loop turn: write what each control client's connection
    /// takes, drop pane bytes every client has been sent, then
    /// `server_client_check_pane_buffer`'s drain.
    pub(crate) fn control_buffers(&mut self) {
        for id in self.control_ids() {
            self.control_pump(id);
        }
        let mut owed: BTreeMap<PaneId, u64> = BTreeMap::new();
        for cs in self.clients.values().filter_map(|c| c.control.as_ref()) {
            for (p, offset) in cs.queue.owed() {
                owed.entry(p)
                    .and_modify(|o| *o = (*o).min(offset))
                    .or_insert(offset);
            }
        }
        self.control_streams.trim(&owed);
    }

    /// `server_client_check_pane_buffer` (server-client.c:2044-2125):
    /// pane `p` (in window `w`) is not read while there are attached
    /// clients and every one is a control client that is behind
    /// ([`queue::BUFFER_LOW`] unwritten) or has the pane off. A terminal
    /// client, a control client with `no-output`, the pane paused or not
    /// in its session (no `control_pane` for it) keeps it read.
    pub(crate) fn control_holds_pane(&self, p: PaneId, w: WindowId) -> bool {
        let mut attached = false;
        for c in self.clients.values() {
            let Some(s) = c.attached else { continue };
            attached = true;
            let Some(cs) = c.control.as_ref() else {
                return false;
            };
            if c.flags.no_output || self.model.index_of(s, w).is_none() {
                return false;
            }
            let keeps = match cs.panes.get(&p) {
                Some(PaneOutput::Off) => false,
                Some(PaneOutput::Paused) => true,
                None => c.unwritten() < queue::BUFFER_LOW,
            };
            if keeps {
                return false;
            }
        }
        attached
    }

    /// `control_start`: the client became a control client.
    pub(crate) fn control_start(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.control = Some(ControlState::default());
        }
    }

    /// `server_client_print` to control client `id` outside the command
    /// it runs: a line in its stream (`control_write`).
    pub(crate) fn control_print(&mut self, id: ClientId, line: &str) {
        if let Some(cs) = self.control_mut(id) {
            cs.write_line(&vtmux_core::rawstr::to_bytes(line));
            self.control_flush(id);
        }
    }

    /// `control_notify_write(c, "%%config-error %s", cause)` (cfg.c:231,
    /// 253): deferred while a guard is open, whatever the client's state.
    pub(crate) fn control_config_error(&mut self, id: ClientId, cause: &str) {
        if let Some(cs) = self.control_mut(id) {
            cs.notify(vtmux_core::rawstr::to_bytes(&format!("%config-error {cause}")).into_owned());
            self.control_flush(id);
        }
    }

    /// Is `id` a control client?
    pub(crate) fn is_control(&self, id: ClientId) -> bool {
        self.clients.get(&id).is_some_and(|c| c.control.is_some())
    }

    /// `cmdq_guard(item, "begin", flags)` as a command fires. Every fired
    /// command takes a number, control or not (cmd-queue.c:745-746).
    pub(crate) fn control_begin(&mut self, item: &Item) -> Option<Guard> {
        self.cmd_number = self.cmd_number.wrapping_add(1);
        let flags = item.control?;
        let g = Guard {
            time: now_epoch(),
            number: self.cmd_number,
            flags,
        };
        self.control_mut(item.client?)?.guard("begin", g);
        Some(g)
    }

    /// The command finished: its output goes into the stream (errors too,
    /// cmd-queue.c `cmdq_error` -> `control_write`), then `%end` or
    /// `%error`, then any notifications it deferred.
    pub(crate) fn control_end(&mut self, item: &mut Item, guard: Option<Guard>, ok: bool) {
        if item.control.is_none() {
            return;
        }
        let Some(id) = item.client else { return };
        self.control_take_output(id, item);
        let Some(cs) = self.control_mut(id) else {
            return;
        };
        if !ok {
            cs.retval = 1;
        }
        if let Some(g) = guard {
            cs.guard(if ok { "end" } else { "error" }, g);
        }
        self.control_flush(id);
    }

    /// `cmdq_error` for a command whose guard has closed (it waited):
    /// `text` as a line in the stream and `c->retval = 1`.
    pub(crate) fn control_error_line(&mut self, id: ClientId, text: &str) {
        let Some(cs) = self.control_mut(id) else {
            return;
        };
        cs.write_line(text.as_bytes());
        cs.retval = 1;
        self.control_flush(id);
    }

    /// `text` as a line in the stream, outside any guard, not as an error:
    /// an upgrade's warnings to the control client that asked for it.
    pub(crate) fn control_plain_line(&mut self, id: ClientId, text: &str) {
        let Some(cs) = self.control_mut(id) else {
            return;
        };
        cs.write_line(text.as_bytes());
        self.control_flush(id);
    }

    /// `c->retval = 1` from a `cmdq_error` outside the command's guard.
    pub(crate) fn control_retval(&mut self, id: ClientId, status: i32) {
        if let Some(cs) = self.control_mut(id) {
            cs.retval = cs.retval.max(status);
        }
    }

    /// Move an item's output into the control stream, as `control_write`
    /// lines (the output already ends in its newline).
    pub(crate) fn control_take_output(&mut self, id: ClientId, item: &mut Item) {
        let Some(cs) = self.control_mut(id) else {
            return;
        };
        for out in item.output.drain(..) {
            cs.out.add(&out.into_bytes(item.utf8).0);
        }
    }

    /// The argv batch is done (`server_client_command_done`): an attached
    /// control client starts reading stdin; an unattached one exits.
    pub(crate) fn control_command_done(&mut self, id: ClientId, mut item: Item) {
        self.control_take_output(id, &mut item);
        let attached = self.clients.get(&id).and_then(|c| c.attached).is_some();
        if let Some(cs) = self.control_mut(id) {
            cs.retval = cs.retval.max(item.status);
            if attached && cs.exit.is_none() {
                cs.ready = true;
            } else {
                cs.exit.get_or_insert(Exit::Return);
            }
        }
        self.control_flush(id);
        self.control_read(id);
    }

    pub(crate) fn control_stdin(&mut self, id: ClientId, bytes: &[u8]) {
        if let Some(cs) = self.control_mut(id) {
            cs.inbuf.extend_from_slice(bytes);
        }
        self.control_read(id);
    }

    /// stdin closed (`control_error_callback`): run what is complete, then
    /// leave; a partial last line is dropped.
    pub(crate) fn control_stdin_eof(&mut self, id: ClientId) {
        if let Some(cs) = self.control_mut(id) {
            cs.eof = true;
        }
        self.control_read(id);
    }

    /// `control_read_callback`: every complete line is a command; an empty
    /// line asks to exit and stops reading. Lines already read still run
    /// after a command made the client exit, as in tmux.
    pub(crate) fn control_read(&mut self, id: ClientId) {
        loop {
            // Stdin is not read before the argv command is done
            // (control_ready): an EOF or a line meanwhile waits for it.
            if self
                .control_mut(id)
                .is_none_or(|cs| !cs.ready || cs.finished)
            {
                return;
            }
            // A blocked line holds the queue: the lines behind it wait.
            // tmux reads them all the same (they queue behind it), so
            // an empty line among them makes the client exit now, with
            // the blocked command and those lines never run.
            if self.is_parked(id) {
                if let Some(cs) = self.control_mut(id)
                    && (cs.inbuf.first() == Some(&b'\n')
                        || cs.inbuf.windows(2).any(|w| w == b"\n\n"))
                {
                    cs.exit.get_or_insert(Exit::Return);
                    cs.inbuf.clear();
                }
                break;
            }
            let Some(cs) = self.control_mut(id) else {
                return;
            };
            let Some(nl) = cs.inbuf.iter().position(|&b| b == b'\n') else {
                break;
            };
            let mut line: Vec<u8> = cs.inbuf.drain(..=nl).collect();
            line.pop();
            if line.is_empty() {
                cs.exit.get_or_insert(Exit::Return);
                cs.inbuf.clear();
                break;
            }
            self.control_line(id, &vtmux_core::rawstr::from_vec(line));
        }
        // EOF counts only once stdin is read (`control_ready`): before
        // that tmux has not enabled the read event (control.c
        // `control_start`), so a client whose command still waits (on the
        // startup config's jobs, say) runs it first.
        if let Some(cs) = self.control_mut(id)
            && cs.eof
            && cs.ready
        {
            cs.exit.get_or_insert(Exit::Return);
        }
    }

    /// One stdin line: parsed like a config line (cmd-parse.y via
    /// `cmd_parse_and_append`), run with guard flags 1. A parse error is
    /// its own guarded item, prefixed `parse error: ` (control.c:672-680),
    /// and does not change the exit status.
    fn control_line(&mut self, id: ClientId, line: &str) {
        let Some(client) = self.clients.get(&id) else {
            return;
        };
        let Some(hello) = client.hello.as_ref() else {
            return;
        };
        let mut item = Item::new(Some(id), hello.cwd.clone());
        item.env.clone_from(&hello.env);
        item.utf8 = hello.flags.utf8;
        item.control = Some(1);
        item.origin = Origin {
            attached: client.attached,
            inside_pane: None,
            current: None,
            mouse: None,
        };
        match cmd::parse_config_with(
            line,
            // cmd_parse_and_append(line, NULL, ...): no parse input, so
            // a `%if` sees no client (control.c:719).
            &mut GlobalEnv::new(&mut self.global_env, &self.model, &self.info),
            ParseOpts::default(),
        ) {
            Ok(calls) => {
                let batch = crate::cmdq::NewBatch::new(item, calls, crate::cmdq::End::ControlLine);
                self.cmdq_append(Some(id), batch);
                self.cmdq_process();
                return;
            }
            Err(e) => {
                let guard = self.control_begin(&item);
                let msg = crate::cmdq::error_bytes(&e);
                if let Some(cs) = self.control_mut(id) {
                    cs.write_line(&[b"parse error: ", msg.as_slice()].concat());
                    if let Some(g) = guard {
                        cs.guard("error", g);
                    }
                }
            }
        }
        self.control_take_output(id, &mut item);
        self.control_flush(id);
    }

    /// End of a loop turn (`server_client_check_exit`): send `Exit` to
    /// control clients that are leaving; on shutdown, to all of them.
    pub(crate) fn control_tick(&mut self) {
        // server_send_exit (server.c): every client is marked exiting
        // before any goes, so one leaving (client-detached) is heard by
        // none of the others.
        if self.shutdown {
            for id in self.control_ids() {
                if let Some(cs) = self.control_mut(id) {
                    cs.exit.get_or_insert(Exit::Shutdown);
                }
            }
        }
        for id in self.control_ids() {
            let Some(cs) = self.control_mut(id) else {
                continue;
            };
            if cs.finished {
                continue;
            }
            let Some(exit) = cs.exit.clone() else {
                continue;
            };
            let (code, message) = match exit {
                Exit::Return => (cs.retval, cs.exit_message.take()),
                Exit::Detach | Exit::Shutdown | Exit::Exec { .. } => (0, None),
            };
            cs.finished = true;
            cs.deferred.clear();
            // control_discard: the pane output held goes; the lines
            // queued behind it go out before the exit.
            let lines = cs.queue.take_lines();
            cs.out.prepend(&lines);
            self.discard_pane_output(id);
            if let Some(c) = self.clients.get_mut(&id) {
                c.left_session = c.attached.or(c.left_session);
                c.attached = None;
            }
            self.control_flush(id);
            if let Exit::Exec { command, shell } = exit {
                self.send(id, &ServerMsg::Exec { command, shell });
                if let Some(c) = self.clients.get_mut(&id) {
                    c.closing = true;
                }
                self.flush_client(id);
            } else {
                self.finish_client_with(id, code, message);
            }
        }
    }

    /// `detach-client -E` on a control client.
    pub(crate) fn control_exec(&mut self, id: ClientId, command: String, shell: PathBuf) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.left_session = c.attached.or(c.left_session);
            c.attached = None;
            if let Some(cs) = c.control.as_mut() {
                cs.exit.get_or_insert(Exit::Exec { command, shell });
            }
        }
    }

    /// `detach-client` on a control client.
    pub(crate) fn control_detach(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.left_session = c.attached.or(c.left_session);
            c.attached = None;
            if let Some(cs) = c.control.as_mut() {
                cs.exit.get_or_insert(Exit::Detach);
            }
        }
    }

    /// Attach a control client: no terminal, no `MSG_READY`
    /// (cmd-new-session.c:337-339, cmd-attach-session.c:164-165); its
    /// client-session-changed is `%session-changed` to it
    /// (control-notify.c:205-230).
    pub(crate) fn control_attach(&mut self, id: ClientId, s: SessionId) {
        if !self.model.sessions.contains_key(&s) {
            return;
        }
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let old = c.attached;
        c.attached = Some(s);
        // server_client_set_session: the latest client (server-client.c:465),
        // then session_update_activity (468), the alerts, then
        // client-session-changed, which control mode writes as
        // %session-changed to the client itself.
        self.set_latest(s, id);
        self.model.session_attached_now(s);
        self.arm_lock_timer(s);
        self.session_attached_alerts(s);
        self.fire_session_changed(id, old);
        self.control_flush(id);
    }

    /// Send `line(client's session)` to every control client that should
    /// hear notifications; `None` skips a client.
    fn control_notify(&mut self, line: &dyn Fn(Option<SessionId>) -> Option<Vec<u8>>) {
        for id in self.control_ids() {
            let Some(c) = self.clients.get_mut(&id) else {
                continue;
            };
            let session = c.attached;
            let Some(cs) = c.control.as_mut() else {
                continue;
            };
            if !cs.wants_notifications() {
                continue;
            }
            if let Some(l) = line(session) {
                cs.notify(l);
                self.control_flush(id);
            }
        }
    }

    /// Pane output for clients whose session links the pane's window
    /// (window.c:1652-1655, control.c `control_write_output`): the bytes
    /// are kept once and each client is owed a block of them, written as
    /// its connection takes it ([`Self::control_pump`]).
    pub(crate) fn control_output(&mut self, pane: PaneId, data: &[u8]) {
        let Some(w) = self.model.panes.get(&pane).map(|p| p.window) else {
            return;
        };
        if data.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut range = None;
        for id in self.control_ids() {
            let Some(c) = self.clients.get(&id) else {
                continue;
            };
            // control_pane_offset: no output at all with `no-output`, none
            // for a pane turned off or paused (control.c:330-345), none
            // once the client is exiting (CONTROL_IGNORE_FLAGS).
            let wanted = !c.flags.no_output
                && c.control.as_ref().is_some_and(|cs| {
                    cs.wants_notifications() && !cs.discard && !cs.panes.contains_key(&pane)
                })
                && c.attached
                    .is_some_and(|s| self.model.index_of(s, w).is_some());
            if !wanted {
                continue;
            }
            if self.control_check_age(id, pane, now) {
                self.control_flush(id);
                continue;
            }
            let (start, end) =
                *range.get_or_insert_with(|| self.control_streams.append(pane, data));
            if let Some(cs) = self.control_mut(id) {
                cs.queue.push_output(pane, start, end, now);
            }
            self.control_pump(id);
        }
    }

    /// `control_set_pane_off`, `control_set_pane_on`, `control_pause_pane`
    /// and `control_continue_pane` for `refresh-client -A %N:state`
    /// (control.c:349-402): `on` and `continue` only act on a pane that is
    /// off or paused, and `continue` says so.
    pub(crate) fn control_pane_state(&mut self, id: ClientId, pane: PaneId, state: &str) {
        let Some(cs) = self.control_mut(id) else {
            return;
        };
        let had = cs.panes.get(&pane).copied();
        let line = match (state, had) {
            ("on", Some(PaneOutput::Off)) => {
                cs.panes.remove(&pane);
                None
            }
            ("continue", Some(PaneOutput::Paused)) => {
                cs.panes.remove(&pane);
                Some(format!("%continue {pane}"))
            }
            ("off", _) => {
                cs.panes.insert(pane, PaneOutput::Off);
                cs.queue.discard_pane(pane);
                None
            }
            ("pause", Some(PaneOutput::Paused)) => None,
            ("pause", _) => {
                cs.panes.insert(pane, PaneOutput::Paused);
                cs.queue.discard_pane(pane);
                Some(format!("%pause {pane}"))
            }
            _ => None,
        };
        if let Some(line) = line {
            self.control_notify_one(id, false, line.into_bytes());
        }
    }

    /// `control_set_window_size` / `control_clear_window_size`.
    pub(crate) fn control_window_size(
        &mut self,
        id: ClientId,
        w: WindowId,
        size: Option<(u32, u32)>,
    ) {
        if let Some(cs) = self.control_mut(id) {
            match size {
                Some(size) => {
                    cs.windows.insert(w, size);
                }
                None => {
                    cs.windows.remove(&w);
                }
            }
        }
    }

    /// `control_reset_offsets`: every pane's on/off/paused state is
    /// forgotten (`refresh-client -f no-output`).
    pub(crate) fn control_reset_panes(&mut self, id: ClientId) {
        if let Some(cs) = self.control_mut(id) {
            cs.panes.clear();
            cs.queue.reset_panes();
        }
    }

    /// `control_reset_pane` (control.c:404-421) on every control client:
    /// pane `p` was respawned, so the output of the old process they have
    /// not been sent goes.
    pub(crate) fn control_reset_pane(&mut self, p: PaneId) {
        for c in self.clients.values_mut() {
            if let Some(cs) = c.control.as_mut() {
                cs.queue.discard_pane(p);
            }
        }
    }

    /// `tty_set_size` for a control client.
    pub(crate) fn control_set_size(&mut self, id: ClientId, size: (u32, u32)) {
        if let Some(cs) = self.control_mut(id) {
            cs.size = Some(size);
        }
    }

    /// The control-mode sink of the event layer (control-notify.c
    /// `control_build_events`): one notification per event, to every
    /// control client that should hear it.
    #[allow(clippy::too_many_lines)] // one arm per notification
    pub(crate) fn control_event(&mut self, ev: &crate::events::Event) {
        let window_line = |kind: &str, w: WindowId, rest: &str, linked: bool| {
            let prefix = if linked { "" } else { "unlinked-" };
            format!("%{prefix}window-{kind} {w}{rest}").into_bytes()
        };
        match ev.name.as_str() {
            // control-notify.c:62-104.
            "window-layout-changed" => {
                let Some(w) = ev.get_window("window") else {
                    return;
                };
                self.control_layout_changed(w, ev.layouts.as_deref());
            }
            // control-notify.c:107-123.
            "window-pane-changed" => {
                let Some(w) = ev.get_window("window") else {
                    return;
                };
                let Some(p) = self.model.windows.get(&w).and_then(|x| x.panes.active) else {
                    return;
                };
                let line = format!("%window-pane-changed {w} {p}").into_bytes();
                self.control_notify(&|_| Some(line.clone()));
            }
            // control-notify.c:126-203: `window-` to clients whose session
            // links the window, `unlinked-window-` to the rest.
            kind @ ("window-unlinked" | "window-linked" | "window-renamed") => {
                let Some(w) = ev.get_window("window") else {
                    return;
                };
                let (word, rest) = match kind {
                    "window-unlinked" => ("close", String::new()),
                    "window-linked" => ("add", String::new()),
                    _ => (
                        "renamed",
                        format!(
                            " {}",
                            self.model.windows.get(&w).map_or("", |x| x.name.as_str())
                        ),
                    ),
                };
                // The links when it fired (tmux's events are synchronous):
                // a window new in a grouped session is unlinked for a
                // client of another member, as session_group_synchronize_from
                // links it there only after (spawn.c:235-238).
                let linked = ev.linked.clone();
                self.control_notify(&|s| Some(window_line(word, w, &rest, linked.contains(&s?))));
            }
            // control-notify.c:205-230.
            "client-session-changed" => {
                let Some((cc, cname)) = ev.get_client("client") else {
                    return;
                };
                let Some(s) = self.clients.get(&cc).and_then(|c| c.attached) else {
                    return;
                };
                let Some(name) = self.model.sessions.get(&s).map(|x| x.name.clone()) else {
                    return;
                };
                let cname = cname.to_owned();
                for id in self.control_ids() {
                    let line = if id == cc {
                        format!("%session-changed {s} {name}")
                    } else {
                        format!("%client-session-changed {cname} {s} {name}")
                    };
                    self.control_notify_one(id, true, line.into_bytes());
                }
            }
            // control-notify.c:233-247.
            "client-detached" => {
                let Some((_, cname)) = ev.get_client("client") else {
                    return;
                };
                let line = format!("%client-detached {cname}").into_bytes();
                self.control_notify_all(&line);
            }
            // control-notify.c:250-267.
            "session-renamed" => {
                let Some((s, name)) = ev.get_session("session") else {
                    return;
                };
                let line = format!("%session-renamed {s} {name}").into_bytes();
                self.control_notify_all(&line);
            }
            // control-notify.c:270-296.
            "session-created" => self.control_notify_all(b"%sessions-changed"),
            "session-closed" => {
                // server_destroy_session ran first: a control client of the
                // session with nowhere to go exits (CLIENT_EXIT), and so
                // hears nothing more (server-fn.c:513-525).
                if let Some((s, _)) = ev.get_session("session") {
                    for id in self.control_ids() {
                        if let Some(c) = self.clients.get_mut(&id)
                            && c.attached == Some(s)
                        {
                            c.attached = None;
                            if let Some(cs) = c.control.as_mut() {
                                cs.exit.get_or_insert(Exit::Return);
                            }
                        }
                    }
                }
                self.control_notify_all(b"%sessions-changed");
            }
            // control-notify.c:299-318.
            "session-window-changed" => {
                let Some((s, _)) = ev.get_session("session") else {
                    return;
                };
                let Some((_, w, _)) = self.model.current_of(s) else {
                    return;
                };
                let line = format!("%session-window-changed {s} {w}").into_bytes();
                self.control_notify_all(&line);
            }
            // control-notify.c:321-345.
            "paste-buffer-changed" | "paste-buffer-deleted" => {
                let Some(name) = ev.get_text("paste_buffer") else {
                    return;
                };
                let line = format!("%{} {name}", ev.name).into_bytes();
                self.control_notify_all(&line);
            }
            // control-notify.c:34-59.
            "pane-mode-changed" => {
                let Some(p) = ev.get_pane("pane") else { return };
                let line = format!("%pane-mode-changed {p}").into_bytes();
                self.control_notify_all(&line);
            }
            _ => {}
        }
    }

    /// `%layout-change` (control-notify.c:62-104): to each client whose
    /// session has the window, unless the window has gone or has no layout.
    fn control_layout_changed(&mut self, w: WindowId, layouts: Option<&crate::events::Layouts>) {
        const TEMPLATE: &str = "%layout-change #{window_id} #{window_layout} \
                                #{window_visible_layout} #{window_raw_flags}";
        if !self.model.windows.contains_key(&w) || self.model.sessions_with(w).next().is_none() {
            return;
        }
        for id in self.control_ids() {
            let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
                continue;
            };
            let Some(index) = self.model.index_of(s, w) else {
                continue;
            };
            let found = crate::target::Found::from_winlink(&self.model, s, index);
            let mut ctx = self.format_ctx(found, Some(id));
            // control_notify_layout_change (control-notify.c:99): the
            // client's own format tree.
            ctx.jobs = Some(self.job_handle(Some(id)));
            ctx.old_layouts = self.old_layouts(Some(id));
            // The layouts when the event fired, not now.
            if let Some(l) = layouts {
                let (layout, visible) = if ctx.old_layouts {
                    (Some(l.layout_v1.clone()), Some(l.visible_v1.clone()))
                } else {
                    (l.layout.clone(), l.visible.clone())
                };
                ctx.extra
                    .extend(layout.map(|v| ("window_layout".into(), v)));
                ctx.extra
                    .extend(visible.map(|v| ("window_visible_layout".into(), v)));
                // `Z` is the last of the flags (window.c window_printable_flags).
                let now = vtmux_core::format::expand_str("#{window_raw_flags}", &ctx);
                let mut flags = now.strip_suffix('Z').unwrap_or(&now).to_owned();
                if l.zoomed {
                    flags.push('Z');
                }
                ctx.extra.push(("window_raw_flags".into(), flags));
            }
            let line =
                vtmux_core::rawstr::to_bytes(&vtmux_core::format::expand_str(TEMPLATE, &ctx))
                    .into_owned();
            self.control_notify_one(id, true, line);
        }
    }

    /// One notification to every control client that should hear it,
    /// attached or not.
    fn control_notify_all(&mut self, line: &[u8]) {
        for id in self.control_ids() {
            self.control_notify_one(id, false, line.to_vec());
        }
    }

    /// `control_notify_write` of `line` to client `id`, if it still hears
    /// notifications.
    pub(crate) fn control_notify_line(&mut self, id: ClientId, line: Vec<u8>) {
        self.control_notify_one(id, false, line);
    }

    /// `control_notify_write` to client `id` if it should hear
    /// notifications (and, with `attached`, has a session).
    fn control_notify_one(&mut self, id: ClientId, attached: bool, line: Vec<u8>) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        if attached && c.attached.is_none() {
            return;
        }
        let Some(cs) = c.control.as_mut() else { return };
        if cs.wants_notifications() {
            cs.notify(line);
            self.control_flush(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(number: u32) -> Guard {
        Guard {
            time: 7,
            number,
            flags: 1,
        }
    }

    fn text(cs: &ControlState) -> String {
        String::from_utf8(cs.out.bytes.clone()).unwrap()
    }

    #[test]
    fn output_escapes_controls_and_backslash_as_octal() {
        let line = output_line(PaneId(3), b"a\\b\r\n\x1b[m\x7f\xc3\xa9");
        assert_eq!(
            line,
            b"%output %3 a\\134b\\015\\012\\033[m\x7f\xc3\xa9".to_vec()
        );
    }

    #[test]
    fn extended_output_carries_the_age_in_milliseconds() {
        let line = extended_output_line(PaneId(2), Duration::from_millis(1500), b"a\n");
        assert_eq!(line, b"%extended-output %2 1500 : a\\012".to_vec());
    }

    #[test]
    fn the_reply_cap_lets_the_client_go_once_and_drops_the_rest() {
        let mut cs = ControlState::default();
        assert!(!cs.check_reply_buffer(0, 10));
        assert!(cs.check_reply_buffer(queue::MAXIMUM_REPLY_BUFFER - 5, 10));
        assert_eq!(cs.exit, Some(Exit::Return));
        assert_eq!(cs.exit_message.as_deref(), Some("too far behind"));
        // CLIENT_CONTROL_DISCARD: everything after is dropped too.
        assert!(cs.check_reply_buffer(0, 1));
    }

    #[test]
    fn notifications_wait_for_the_outermost_guard() {
        let mut cs = ControlState::default();
        cs.guard("begin", g(1));
        cs.notify(b"%sessions-changed".to_vec());
        cs.guard("begin", g(2));
        cs.write_line(b"hello");
        cs.guard("end", g(2));
        assert!(!text(&cs).contains("%sessions"));
        cs.guard("error", g(1));
        cs.notify(b"%window-add @1".to_vec());
        assert_eq!(
            text(&cs),
            "%begin 7 1 1\n%begin 7 2 1\nhello\n%end 7 2 1\n%error 7 1 1\n%sessions-changed\n%window-add @1\n"
        );
    }
}
