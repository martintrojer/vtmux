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

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use vtmux_core::cmd::{self, CmdError, ParseOpts};
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::protocol::ServerMsg;

use crate::commands::{Item, Output};
use crate::server::{ClientId, GlobalEnv, Server};
use crate::target::Origin;

/// Why a control client leaves; decides its exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// EOF, an empty line, its session gone, or an unattached argv command
    /// (`CLIENT_EXIT_RETURN`): the status is `c->retval`.
    Return,
    /// `detach-client` (`MSG_DETACH`): the client exits 0 and, never having
    /// had `MSG_READY`, prints no reason (client.c `client_dispatch_wait`).
    Detach,
    /// The server is going away (`MSG_SHUTDOWN`): status 0, no reason.
    Shutdown,
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
pub struct ControlState {
    /// Unconsumed stdin bytes.
    inbuf: Vec<u8>,
    /// Stdin is read (`control_ready`, after the argv command attached).
    ready: bool,
    eof: bool,
    guard_depth: u32,
    deferred: Vec<Vec<u8>>,
    /// Protocol bytes not yet handed to the connection.
    out: Vec<u8>,
    /// `CLIENT_EXIT` with how the exit is reported.
    exit: Option<Exit>,
    /// `Exit` has been sent.
    finished: bool,
    /// `c->retval`: 1 once any command failed (cmd-queue.c `cmdq_error`).
    retval: i32,
}

impl ControlState {
    fn write_line(&mut self, line: &[u8]) {
        self.out.extend_from_slice(line);
        self.out.push(b'\n');
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
}

/// `%output %<pane> <data>`: bytes below space and `\` as `\ooo`, the rest
/// raw (control.c `control_append_data`).
#[must_use]
pub fn output_line(pane: PaneId, data: &[u8]) -> Vec<u8> {
    let mut line = format!("%output {pane} ").into_bytes();
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

/// One change to the session/window links.
#[derive(Clone, Copy)]
enum Link {
    SessionsChanged,
    Close(WindowId),
    Add(WindowId),
}

/// Which windows each session links: the before/after picture that
/// [`Server::control_links_changed`] turns into notifications.
pub type Links = BTreeMap<SessionId, BTreeSet<WindowId>>;

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

    /// Hand buffered protocol bytes to the connection.
    pub(crate) fn control_flush(&mut self, id: ClientId) {
        let Some(cs) = self.control_mut(id) else {
            return;
        };
        let out = std::mem::take(&mut cs.out);
        if !out.is_empty() {
            self.send(id, &ServerMsg::Stdout(out));
        }
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
            cs.write_line(line.as_bytes());
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

    /// Move an item's output into the control stream, as `control_write`
    /// lines (the output already ends in its newline).
    pub(crate) fn control_take_output(&mut self, id: ClientId, item: &mut Item) {
        let Some(cs) = self.control_mut(id) else {
            return;
        };
        for out in item.output.drain(..) {
            let (Output::Stdout(b) | Output::Stderr(b)) = out;
            cs.out.extend_from_slice(&b);
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
            // A blocked line holds the queue: the lines behind it wait.
            if self.is_parked(id) {
                break;
            }
            let Some(cs) = self.control_mut(id) else {
                return;
            };
            if !cs.ready || cs.finished {
                return;
            }
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
            self.control_line(id, &String::from_utf8_lossy(&line));
        }
        if let Some(cs) = self.control_mut(id)
            && cs.eof
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
        };
        match cmd::parse_config_with(
            line,
            // cmd_parse_and_append(line, NULL, ...): no parse input, so
            // a `%if` sees no client (control.c:719).
            &mut GlobalEnv::new(&mut self.global_env, &self.model, &self.info),
            ParseOpts::default(),
        ) {
            Ok(calls) => {
                self.run_batch(id, item, calls);
                return;
            }
            Err(e) => {
                let guard = self.control_begin(&item);
                let msg = match &e {
                    CmdError::Edge(edge) => edge.to_bytes(),
                    other => other.to_bytes(),
                };
                if let Some(cs) = self.control_mut(id) {
                    cs.out.extend_from_slice(b"parse error: ");
                    cs.write_line(&msg);
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
        for id in self.control_ids() {
            let shutdown = self.shutdown;
            let Some(cs) = self.control_mut(id) else {
                continue;
            };
            if cs.finished {
                continue;
            }
            if shutdown {
                cs.exit.get_or_insert(Exit::Shutdown);
            }
            let Some(exit) = cs.exit else { continue };
            let code = match exit {
                Exit::Return => cs.retval,
                Exit::Detach | Exit::Shutdown => 0,
            };
            cs.finished = true;
            cs.deferred.clear();
            if let Some(c) = self.clients.get_mut(&id) {
                c.attached = None;
            }
            self.control_flush(id);
            self.finish_client(id, code);
        }
    }

    /// `detach-client` on a control client.
    pub(crate) fn control_detach(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.attached = None;
            if let Some(cs) = c.control.as_mut() {
                cs.exit.get_or_insert(Exit::Detach);
            }
        }
    }

    /// Attach a control client: no terminal, no `MSG_READY`
    /// (cmd-new-session.c:337-339, cmd-attach-session.c:164-165), just
    /// `%session-changed` (control-notify.c:205-230).
    pub(crate) fn control_attach(&mut self, id: ClientId, s: SessionId) {
        let Some(name) = self.model.sessions.get(&s).map(|x| x.name.clone()) else {
            return;
        };
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        c.attached = Some(s);
        if let Some(cs) = c.control.as_mut()
            && cs.wants_notifications()
        {
            cs.notify(format!("%session-changed {s} {name}").into_bytes());
        }
        // server_client_set_session: session_update_activity (server-client.c:468).
        self.model.touch_session(s);
        self.session_attached_alerts(s);
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
    /// (window.c:1652-1655, control.c `control_write_output`).
    pub(crate) fn control_output(&mut self, pane: PaneId, data: &[u8]) {
        let Some(w) = self.model.panes.get(&pane).map(|p| p.window) else {
            return;
        };
        let linking: BTreeSet<SessionId> = self.model.sessions_with(w).map(|s| s.id).collect();
        let line = output_line(pane, data);
        self.control_notify(&|s| s.filter(|s| linking.contains(s)).map(|_| line.clone()));
    }

    /// The session-to-window links now.
    pub(crate) fn control_links(&self) -> Links {
        self.model
            .sessions
            .values()
            .map(|s| (s.id, s.windows.values().copied().collect()))
            .collect()
    }

    /// Notifications for what changed since `before`, in tmux's order:
    /// a closed session's clients exit first (`server_destroy_session`),
    /// then `%sessions-changed` and its windows' closes (`session_destroy`
    /// fires session-closed before window-unlinked); a new session's
    /// windows are linked before `%sessions-changed` (session-created).
    pub(crate) fn control_links_changed(&mut self, before: &Links) {
        let after = self.control_links();
        for id in self.control_ids() {
            if let Some(c) = self.clients.get_mut(&id)
                && let Some(s) = c.attached
                && !after.contains_key(&s)
                && let Some(cs) = c.control.as_mut()
            {
                cs.exit.get_or_insert(Exit::Return);
                c.attached = None;
            }
        }
        let mut events = Vec::new();
        for (s, windows) in before {
            match after.get(s) {
                None => {
                    events.push(Link::SessionsChanged);
                    events.extend(windows.iter().map(|&w| Link::Close(w)));
                }
                Some(now) => {
                    events.extend(windows.difference(now).map(|&w| Link::Close(w)));
                    events.extend(now.difference(windows).map(|&w| Link::Add(w)));
                }
            }
        }
        for (s, windows) in &after {
            if !before.contains_key(s) {
                events.extend(windows.iter().map(|&w| Link::Add(w)));
                events.push(Link::SessionsChanged);
            }
        }
        let linked = |links: &Links, s: SessionId, w: WindowId| {
            links.get(&s).is_some_and(|ws| ws.contains(&w))
        };
        for event in events {
            self.control_notify(&|s| {
                let line = match event {
                    Link::SessionsChanged => "%sessions-changed".to_owned(),
                    // control-notify.c:128-176: `window-` when the client's
                    // session links the window, else `unlinked-window-`.
                    Link::Close(w) if linked(before, s?, w) || linked(&after, s?, w) => {
                        format!("%window-close {w}")
                    }
                    Link::Close(w) => format!("%unlinked-window-close {w}"),
                    Link::Add(w) if linked(&after, s?, w) => format!("%window-add {w}"),
                    Link::Add(w) => format!("%unlinked-window-add {w}"),
                };
                Some(line.into_bytes())
            });
        }
    }

    /// `%session-renamed` (control-notify.c:250-267).
    pub(crate) fn control_session_renamed(&mut self, s: SessionId, name: &str) {
        let line = format!("%session-renamed {s} {name}").into_bytes();
        self.control_notify(&|_| Some(line.clone()));
    }

    /// `%window-renamed` / `%unlinked-window-renamed` (control-notify.c:178-203).
    pub(crate) fn control_window_renamed(&mut self, w: WindowId, name: &str) {
        let linking: BTreeSet<SessionId> = self.model.sessions_with(w).map(|s| s.id).collect();
        self.control_notify(&|s| {
            let s = s?;
            Some(if linking.contains(&s) {
                format!("%window-renamed {w} {name}").into_bytes()
            } else {
                format!("%unlinked-window-renamed {w} {name}").into_bytes()
            })
        });
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
        String::from_utf8(cs.out.clone()).unwrap()
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
