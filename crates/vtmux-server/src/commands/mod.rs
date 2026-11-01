//! Command execution: one file per command, dispatched by table entry.
//!
//! Commands run as steps of the command queue ([`crate::cmdq`]): [`fire`]
//! runs one, with its control guard. Output goes to the client in order;
//! errors and prints follow `cmdq_error` / `cmdq_print`
//! (cmd-queue.c:820-886): a command client gets `text\n` on stderr /
//! stdout (sanitised when it has no UTF-8), a config run (no client)
//! records errors as causes and drops prints.
//!
//! A command can wait (`CMD_RETURN_WAIT`: `wait-for`, file I/O,
//! `run-shell`, `if-shell`) by setting [`Item::blocked`]; its queue holds
//! until the step is continued.

mod break_pane;
mod capture_pane;
mod choose_tree;
mod clear_history;
pub(crate) mod clock_mode;
pub(crate) mod command_prompt;
pub(crate) mod confirm_before;
pub(crate) mod copy_mode;
mod delete_buffer;
mod detach_client;
mod display_message;
pub(crate) mod display_popup;
pub(crate) mod file_io;
mod join_pane;
mod keys;
mod kill_pane;
mod kill_server;
mod kill_session;
mod kill_window;
mod list_buffers;
mod list_clients;
mod list_commands;
mod list_panes;
mod list_sessions;
mod list_windows;
mod load_buffer;
mod move_window;
mod new_session;
mod new_window;
mod paste_buffer;
mod prompt_history;
mod refresh_client;
mod rename_session;
mod rename_window;
mod resize_pane;
mod resize_window;
mod respawn_pane;
mod respawn_window;
mod rotate_window;
mod run_shell;
pub(crate) use run_shell::make_commands;
mod save_buffer;
mod select_layout;
mod select_pane;
mod select_window;
mod send_keys;
mod set_buffer;
mod set_environment;
mod set_option;
mod show_environment;
mod show_messages;
mod show_options;
pub mod source_file;
mod split_window;
mod swap_pane;
mod swap_window;
mod unlink_window;
mod wait_for;

use std::path::PathBuf;

use vtmux_core::cmd::{CmdError, CommandCall, NumError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::format;
use vtmux_core::options::SetOutcome;
use vtmux_core::protocol::TtyInfo;

use crate::names::utf8_sanitize;
use crate::server::{ClientId, Server};
use crate::target::{self, Found, Origin};
use select_window::Which;

/// Output of a batch, in order.
#[derive(Debug)]
pub enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// What happens to the client after its commands ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// Send `Exit` with the batch's status.
    Exit,
    /// The client was attached (or will be); keep the connection open.
    Stay,
}

/// The client a batch runs for: tmux's `cmdq_item` client side.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent cmdq_item states"
)]
#[derive(Debug)]
pub struct Item {
    /// `None` for config files and other client-less runs.
    pub client: Option<ClientId>,
    pub origin: Origin,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub tty: Option<TtyInfo>,
    pub utf8: bool,
    pub output: Vec<Output>,
    /// `c->retval`.
    pub status: i32,
    pub after: After,
    /// A control client's command: the `%begin` guard flags (0 for the
    /// argv command, 1 for lines read from stdin, cmd-queue.c:594).
    pub control: Option<u8>,
    /// The command that just ran returned `CMD_RETURN_WAIT`.
    pub blocked: bool,
    /// Run from a repeating key binding (`CMDQ_STATE_REPEAT`):
    /// `switch-client` then keeps the client's key table.
    pub repeat: bool,
    /// The queue step running now (`item` in tmux's callbacks).
    pub step: Option<crate::cmdq::StepId>,
    /// The running command's file and line (`cmd_get_source`).
    pub source: Option<std::rc::Rc<crate::cmdq::Source>>,
    /// What the command client's stdout has been used for.
    pub stdout: Stdout,
    /// `load-buffer -` has used the client's stdin.
    pub stdin_used: bool,
    /// The running step's `CMDQ_STATE_NOHOOKS`.
    pub no_hooks: bool,
    /// The running command's resolved target (`item->target`).
    pub found: Option<Found>,
    /// A command that fires its own `after-` hook (new-session,
    /// new-window, split-window, select-pane, select-window) sets the
    /// hook's target here (`Some(None)`: an invalid current state, so the
    /// event has no target).
    pub hook_target: Option<Option<Found>>,
    /// A hook batch on the global queue: each command borrows the current
    /// client once its guard is written (cmd-queue.c:597-598), so its
    /// output reaches that client without guards.
    pub borrow_client: bool,
    /// The key whose binding this batch runs (`state->event.key`); the
    /// message log names it.
    pub key: Option<vtmux_core::keys::Key>,
}

/// The client's stdout can be used once (`file_write_open` dups it and
/// closes the original, client.c:698-701): by the print stream, which
/// stays open, or by one `save-buffer -` / `show-buffer`, after which
/// prints go nowhere and a second use fails with `EBADF`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stdout {
    #[default]
    Free,
    /// `file_print` opened the print stream.
    Printing,
    /// A file write took stdout and closed it.
    Closed,
}

impl Item {
    #[must_use]
    pub fn new(client: Option<ClientId>, cwd: PathBuf) -> Self {
        Self {
            client,
            origin: Origin::default(),
            cwd,
            env: Vec::new(),
            tty: None,
            utf8: true,
            output: Vec::new(),
            status: 0,
            after: After::Exit,
            control: None,
            blocked: false,
            repeat: false,
            step: None,
            source: None,
            stdout: Stdout::Free,
            stdin_used: false,
            no_hooks: false,
            found: None,
            hook_target: None,
            borrow_client: false,
            key: None,
        }
    }

    /// `cmdq_print`: a line on stdout.
    pub fn print(&mut self, text: &str) {
        self.print_line(&vtmux_core::rawstr::to_bytes(text));
    }

    /// `cmdq_print` of bytes that need not be UTF-8: a UTF-8 client gets
    /// them as they are (`server_client_print`, server-client.c:3243-3256),
    /// any other a sanitised line.
    pub fn print_line(&mut self, text: &[u8]) {
        if !self.can_print() {
            return;
        }
        let mut line = if self.utf8 {
            text.to_vec()
        } else {
            utf8_sanitize(&vtmux_core::rawstr::from_bytes(text)).into_bytes()
        };
        line.push(b'\n');
        self.output.push(Output::Stdout(line));
    }

    /// Raw bytes on stdout (`file_print_buffer`).
    pub fn print_raw(&mut self, bytes: Vec<u8>) {
        if self.can_print() {
            self.output.push(Output::Stdout(bytes));
        }
    }

    /// `file_print`: a client with stdout takes the print stream.
    fn can_print(&mut self) -> bool {
        if self.client.is_none() || self.stdout == Stdout::Closed {
            return false;
        }
        if self.control.is_none() {
            self.stdout = Stdout::Printing;
        }
        true
    }

    /// `file_write` of `-` for a command client: stdout is free only
    /// once. The data goes out in order with the prints.
    pub fn write_stdout(&mut self, bytes: Vec<u8>) -> bool {
        if self.stdout != Stdout::Free {
            return false;
        }
        self.stdout = Stdout::Closed;
        self.output.push(Output::Stdout(bytes));
        true
    }

    /// `cmdq_error` for a command client; config runs keep the cause.
    pub fn error(&mut self, server: &mut Server, line: Vec<u8>) {
        self.status = 1;
        if self.client.is_none() {
            // cmdq_error with no client: a cause while the config loads,
            // else the server message log (cmd-queue.c:850-868).
            let msg = vtmux_core::rawstr::from_bytes(&line);
            if server.cfg_finished {
                server.add_message(match &self.source {
                    Some(src) => format!("message: {}:{}: {msg}", src.file, src.line),
                    None => format!("message: {msg}"),
                });
            } else {
                let cause = match &self.source {
                    Some(src) => format!("{}:{}: {msg}", src.file, src.line),
                    None => msg,
                };
                server.causes.push(cause);
            }
            return;
        }
        // A client with no session, or a control client, gets the error
        // as it is (cmd-queue.c:869); an attached one gets a status
        // message, which logs itself.
        if let Some(c) = self.client
            && (server.client_session(c).is_none() || server.is_control(c))
        {
            let msg = vtmux_core::rawstr::from_bytes(&line);
            let text = format!("{} message: {msg}", server.client_name(c));
            server.add_message(text);
        }
        let mut line = if self.utf8 {
            line
        } else {
            utf8_sanitize(&vtmux_core::rawstr::from_bytes(&line)).into_bytes()
        };
        line.push(b'\n');
        self.output.push(Output::Stderr(line));
    }
}

/// `cmdq_fire_command` (cmd-queue.c:569-655): run one command for
/// `item` inside its control guard (flags 1 for a control client's stdin
/// line, cmd-queue.c:594). Whether it succeeded; a failure drops the rest
/// of its group. A waiting command (`CMD_RETURN_WAIT`) still closes its
/// guard with `%end`.
pub fn fire(server: &mut Server, item: &mut Item, call: &CommandCall) -> bool {
    let guard = server.control_begin(item);
    item.hook_target = None;
    if item.borrow_client {
        item.client = server.target_client(None, item.origin);
        item.control = item.client.filter(|&c| server.is_control(c)).map(|_| 0);
    }
    server.cmd_client = item.client;
    let result = run_one(server, item, call);
    server.cmd_client = None;
    // The events the command fired go to their sinks inside its guard
    // (control notifications are deferred until it closes), then its
    // after- or command-error hook (cmd-queue.c:625-653).
    server.drain_events();
    if !item.no_hooks {
        let hook = match &result {
            Ok(()) if call.entry.after_hook() || item.hook_target.is_some() => {
                Some(format!("after-{}", call.entry.name))
            }
            Err(CmdError::Quiet) | Ok(()) => None,
            Err(_) => Some("command-error".to_owned()),
        };
        if let Some(name) = hook {
            let target = item.hook_target.unwrap_or_else(|| {
                item.found
                    .filter(|f| f.still_valid(&server.model))
                    .or_else(|| target::current(&server.model, item.origin))
            });
            server.cmd_hook(&name, call, item.step, target);
        }
    }
    // A quiet failure (unbind-key -q) ends the group but never reached
    // cmdq_error: no message, no exit status.
    if let Err(e) = result.as_ref()
        && !matches!(e, CmdError::Quiet)
    {
        item.error(server, crate::cmdq::error_bytes(e));
    }
    server.control_end(item, guard, result.is_ok());
    server.update_focus_all();
    // send-keys -K: the keys are handled as queue items after this
    // command (server_client_handle_key_after), so after its guard.
    server.handle_injected_keys(item.step);
    result.is_ok()
}

fn run_one(server: &mut Server, item: &mut Item, call: &CommandCall) -> Result<(), CmdError> {
    call.entry.ensure_implemented()?;
    let source = resolve_source(server, item, call)?;
    // cmdq_find_flag (cmd-queue.c:514-530): resolve the target before the
    // command runs; commands without a target slot get the client's
    // current state, failures ignored.
    item.found = None;
    let found = match &call.entry.target {
        Some(spec) => {
            let value = call.args.target(spec.flag);
            target::resolve(
                &server.model,
                item.origin,
                value.as_ref(),
                spec.kind,
                spec.flags,
            )?
        }
        None => target::current(&server.model, item.origin).unwrap_or_default(),
    };
    item.found = Some(found);
    let result = dispatch(server, item, call, source, found);
    if result.is_ok() {
        status_redraws(server, call, source, found);
    }
    result
}

/// The status redraws a command asks for when it succeeds
/// (`server_redraw_*` and `server_status_*`, server-fn.c), for the
/// commands that do not ask from inside: select-pane, select-window and
/// the model's own changes (an active pane, a resize, a pane or window
/// gone) do.
fn status_redraws(server: &mut Server, call: &CommandCall, source: Found, found: Found) {
    use crate::model::Redraw;
    let detached = call.args.flag('d');
    let mut ask = |r: Option<Redraw>| {
        if let Some(r) = r {
            server.model.redraw(r);
        }
    };
    let session = |f: Found| f.session.map(Redraw::Session);
    let status = |f: Found| f.session.map(Redraw::SessionStatus);
    let window = |f: Found| f.window.map(Redraw::Window);
    match call.entry.name {
        // cmd-new-window.c:168-170: a detached new window only redraws the
        // status (found is the session's current window, not the new one).
        "new-window" if detached => ask(status(found)),
        // cmd-new-window.c:122,168; cmd-kill-session.c:70 (-C).
        // select-window and friends ask themselves.
        "new-window" | "kill-session" => ask(session(found)),
        // cmd-split-window.c:293-298.
        "split-window" => {
            ask(window(found));
            ask(session(found));
        }
        // cmd-resize-pane.c:91,188; cmd-select-layout.c:146.
        "resize-pane" | "select-layout" | "next-layout" | "previous-layout" => {
            ask(window(found));
        }
        // cmd-swap-pane.c:185-208.
        "swap-pane" => {
            ask(window(source));
            ask(window(found));
        }
        // cmd-break-pane.c:193-198.
        "break-pane" => {
            ask(session(source));
            ask(session(found));
        }
        // cmd-join-pane.c:520-529.
        "join-pane" | "move-pane" => {
            ask(window(source));
            ask(window(found));
            ask(if detached {
                status(found)
            } else {
                session(found)
            });
        }
        // cmd-rename-window.c:65; cmd-respawn-pane.c:94.
        "rename-window" | "respawn-pane" => ask(found.window.map(Redraw::WindowStatus)),
        _ => {}
    }
}

#[allow(clippy::too_many_lines)] // one dispatch arm per command
fn dispatch(
    server: &mut Server,
    item: &mut Item,
    call: &CommandCall,
    source: Found,
    found: Found,
) -> Result<(), CmdError> {
    let args = &call.args;
    match call.entry.name {
        "new-session" => new_session::exec(server, item, args, found),
        // has-session: the target lookup is the whole command
        // (cmd-new-session.c:89-95).
        "has-session" | "start-server" => Ok(()),
        "kill-session" => kill_session::exec(server, item, args, found),
        "kill-server" => {
            kill_server::exec(server);
            Ok(())
        }
        "list-clients" => list_clients::exec(server, item, args, found),
        "list-sessions" => list_sessions::exec(server, item, args),
        "split-window" => split_window::exec(server, item, args, found),
        "kill-pane" => kill_pane::exec(server, item, args, found),
        "select-pane" => select_pane::exec(server, item, args, found, false),
        "last-pane" => select_pane::exec(server, item, args, found, true),
        "list-panes" => list_panes::exec(server, item, args, found),
        "new-window" => new_window::exec(server, item, args, found),
        "kill-window" => kill_window::exec(server, item, args, found),
        "link-window" | "move-window" => {
            move_window::exec(server, item, args, source, call.entry.name == "move-window")
        }
        "unlink-window" => unlink_window::exec(server, item, args, found),
        "swap-window" => swap_window::exec(server, args, source, found),
        "rename-session" => rename_session::exec(server, item, args, found),
        "rename-window" => rename_window::exec(server, item, args, found),
        "list-windows" => list_windows::exec(server, item, args, found),
        "select-window" => select_window::exec(server, item, args, found, Which::Select),
        "next-window" => select_window::exec(server, item, args, found, Which::Next),
        "previous-window" => select_window::exec(server, item, args, found, Which::Previous),
        "last-window" => select_window::exec(server, item, args, found, Which::Last),
        "resize-pane" => resize_pane::exec(server, item, args, found),
        "resize-window" => resize_window::exec(server, item, args, found),
        "select-layout" => select_layout::exec(server, item, args, found, None),
        "next-layout" => select_layout::exec(server, item, args, found, Some(true)),
        "previous-layout" => select_layout::exec(server, item, args, found, Some(false)),
        "swap-pane" => swap_pane::exec(server, args, source, found),
        "break-pane" => break_pane::exec(server, item, args, source, found),
        "join-pane" | "move-pane" => {
            join_pane::exec(server, args, source, found, call.entry.name == "move-pane")
        }
        "respawn-pane" => respawn_pane::exec(server, item, args, found),
        "respawn-window" => respawn_window::exec(server, item, args, found),
        "rotate-window" => rotate_window::exec(server, args, found),
        "display-message" => display_message::exec(server, item, args, found),
        "capture-pane" => capture_pane::exec(server, item, args, found),
        "send-keys" => send_keys::exec(server, item, args, found),
        "clock-mode" => clock_mode::exec(server, found),
        "copy-mode" => copy_mode::exec(server, item, args, source, found),
        "choose-tree" => choose_tree::exec(server, item, args, found),
        "set-buffer" => set_buffer::exec(server, item, args),
        "delete-buffer" => delete_buffer::exec(server, args),
        "show-buffer" => save_buffer::exec(server, item, args, found, true),
        "save-buffer" => save_buffer::exec(server, item, args, found, false),
        "load-buffer" => load_buffer::exec(server, item, args, found),
        "paste-buffer" => paste_buffer::exec(server, args, found),
        "list-buffers" => list_buffers::exec(server, item, args),
        "list-commands" => list_commands::exec(server, item, args),
        "show-messages" => {
            show_messages::exec(server, item, args);
            Ok(())
        }
        "clear-history" => clear_history::exec(server, found),
        "bind-key" | "unbind-key" => keys::bind_or_unbind(server, call),
        "list-keys" => keys::list_keys(server, item, args, found),
        "send-prefix" => keys::send_prefix(server, item, args, found),
        "switch-client" => keys::switch_client(server, item, args),
        "refresh-client" => refresh_client::exec(server, item, args),
        "source-file" => source_file::exec(server, item, args, found),
        "wait-for" => wait_for::exec(server, item, args),
        "run-shell" => run_shell::exec(server, item, args, found),
        "if-shell" => run_shell::if_shell(server, item, args, found),
        "set-hook" => match set_option::exec(server, item, call, found)? {
            // hooks_run (cmd-set-option.c:253-258).
            SetOutcome::RunHook(name) => {
                server.hooks_run(item.step, &name, found);
                Ok(())
            }
            // cmd_set_hook_event_exec (cmd-set-option.c:91-131).
            SetOutcome::FireEvent(name) => {
                let mut ev = crate::events::Event::new(&name).target(found);
                if let Some(c) = item.client {
                    ev = ev.client("client", c, server.client_name(c));
                }
                if let Some(s) = found.session {
                    ev = ev.session(&server.model, "session", s);
                }
                if let Some(w) = found.window {
                    ev = ev.window(&server.model, "window", w);
                }
                if let Some(i) = found.index {
                    ev = ev.int("window_index", i);
                }
                if let Some(p) = found.pane {
                    ev = ev.pane("pane", p);
                }
                server.fire_event(&ev);
                Ok(())
            }
            SetOutcome::Changed(_) | SetOutcome::Unchanged => Ok(()),
        },
        "set-option" | "set-window-option" => {
            let result = set_option::exec(server, item, call, found);
            // options_push_changes (options.c:1437-1446): pane-border-status
            // and the scrollbar options re-run layout_fix_panes everywhere.
            let windows: Vec<_> = server.model.windows.keys().copied().collect();
            for &w in &windows {
                server.model.fix_panes(w);
            }
            if matches!(&result, Ok(SetOutcome::Changed(name)) if name == "automatic-rename") {
                // options.c:1380-1386: windows with it on look again.
                for w in windows {
                    server.model.mark_for_rename(w);
                }
            }
            if matches!(&result, Ok(SetOutcome::Changed(name)) if name == "monitor-silence") {
                // options.c:1422: alerts_reset_all.
                server.model.alerts_reset_all();
            }
            if matches!(&result, Ok(SetOutcome::Changed(name)) if name == "status" || name == "status-interval")
            {
                // options.c:1409-1411.
                server.status_timer_start_all();
            }
            if matches!(&result, Ok(SetOutcome::Changed(_))) {
                server.push_option_changes();
            }
            result.map(|_| ()).map_err(Into::into)
        }
        "set-environment" => set_environment::exec(server, item, args, found),
        "show-environment" => show_environment::exec(server, item, args, found),
        "show-options" | "show-window-options" | "show-hooks" => {
            show_options::exec(server, item, call, found).map_err(Into::into)
        }
        "attach-session" => {
            // cmd-attach-session.c:66-69: checked before the target.
            if server.model.sessions.is_empty() {
                return Err(EdgeError::NoSessions.into());
            }
            let spec = args.target('t');
            // cmd-attach-session.c:80-91: a pane-like target is a pane,
            // else a session, preferring one no client is attached to.
            let pane_like = args.value('t').is_some_and(|t| t.contains([':', '.']));
            let attached = server.attached_sessions();
            let found = target::resolve_preferring(
                &server.model,
                item.origin,
                spec.as_ref(),
                if pane_like {
                    vtmux_core::cmd::TargetKind::Pane
                } else {
                    vtmux_core::cmd::TargetKind::Session
                },
                vtmux_core::cmd::FindFlags::NONE,
                (!pane_like).then_some(&attached),
            )?;
            let session = found.session.ok_or(EdgeError::NoCurrentTarget)?;
            // cmd-attach-session.c:108-121: -f, then -r, before the client
            // takes the session (and with no client, nothing at all).
            if let Some(c) = item.client {
                if let Some(flags) = args.value('f') {
                    server.set_client_flags(c, flags);
                }
                if args.flag('r')
                    && let Some(client) = server.clients.get_mut(&c)
                {
                    client.flags.read_only = true;
                    client.flags.ignore_size = true;
                }
            }
            attach(server, item, session, !args.flag('E'), true)
        }
        "detach-client" => detach_client::exec(server, item, args),
        "command-prompt" => command_prompt::exec(server, item, args),
        "confirm-before" => confirm_before::exec(server, item, args),
        "display-popup" => display_popup::exec(server, item, args, found),
        "show-prompt-history" => prompt_history::show(server, item, args),
        "clear-prompt-history" => prompt_history::clear(server, args),
        other => Err(CmdError::NotImplemented(other.to_owned())),
    }
}

/// `cmdq_fire_command` (cmd-queue.c:618-623): the `-s` source is resolved
/// before the target. detach-client looks its `-s` up itself. With no
/// marked pane (vtmux has none yet), `CMD_FIND_DEFAULT_MARKED` starts from
/// the current state like any other lookup (cmd-find.c:992-1006).
fn resolve_source(server: &Server, item: &Item, call: &CommandCall) -> Result<Found, CmdError> {
    match &call.entry.source {
        Some(spec) if call.entry.name != "detach-client" => {
            let value = call.args.target(spec.flag);
            Ok(target::resolve(
                &server.model,
                item.origin,
                value.as_ref(),
                spec.kind,
                spec.flags,
            )?)
        }
        _ => Ok(Found::default()),
    }
}

/// Attach the item's client to `session` through the server hook.
/// `update_env` is attach-session's missing `-E`: the client's
/// `update-environment` variables go into the session first
/// (cmd-attach-session.c:135,159); a session just created has had them.
pub fn attach(
    server: &mut Server,
    item: &mut Item,
    session: vtmux_core::ids::SessionId,
    update_env: bool,
    fire_attached: bool,
) -> Result<(), CmdError> {
    let Some(client) = item.client else {
        return Ok(());
    };
    // The sinks hear what happened so far (a new session's window-linked)
    // while the client still has its old session, as in tmux, where events
    // fire as they happen.
    server.drain_events();
    if update_env && (item.control.is_some() || item.tty.is_some()) {
        server.update_session_environ(client, session);
    }
    if item.control.is_some() {
        // A control client needs no terminal and gets no Ready.
        server.control_attach(client, session);
        item.origin.attached = Some(session);
        reset_key_table(server, item, client);
    } else {
        if item.tty.is_none() {
            return Err(EdgeError::OpenTerminalFailed("not a terminal".into()).into());
        }
        server.attach_client(client, session)?;
        item.after = After::Stay;
        reset_key_table(server, item, client);
    }
    // cmd-attach-session.c:166-167 (attach-session only; new-session just
    // sets CLIENT_ATTACHED, cmd-new-session.c:356-357).
    if fire_attached {
        server.fire_client("client-attached", client);
    }
    if let Some(c) = server.clients.get_mut(&client) {
        c.was_attached = true;
    }
    Ok(())
}

/// `server_client_set_key_table(c, NULL)` after `server_client_set_session`
/// (cmd-attach-session.c:139,162, cmd-new-session.c:343-344): the client
/// starts in the session's `key-table`, unless a repeating binding ran it.
fn reset_key_table(server: &mut Server, item: &Item, client: ClientId) {
    if !item.repeat {
        server.set_key_table(client, None);
    }
}

/// `cmdq_get_client` with a session, for `s->curw->window->latest = c`:
/// the calling client, or for a run without one (a config file) the
/// current client (`cmd_find_client(item, NULL, 1)`, cmd-queue.c:597-598).
pub(crate) fn latest_client(server: &Server, item: &Item) -> Option<ClientId> {
    match item.client {
        Some(c) => server.client_session(c).is_some().then_some(c),
        None => server.target_client(None, item.origin),
    }
}

/// `buffer-limit`.
pub(crate) fn buffer_limit(server: &Server) -> i64 {
    server
        .model
        .number_option(vtmux_core::options::TreeId::Server, "buffer-limit")
}

/// `format_single` with the command's target.
pub fn expand(server: &Server, found: Found, template: &str) -> String {
    let ctx = server.format_ctx(found, None);
    format::expand_str(template, &ctx)
}

/// `args_strtonum_and_expand` (arguments.c:942-973).
pub fn number_expanded(
    server: &Server,
    found: Found,
    value: Option<&str>,
    min: i64,
    max: i64,
) -> Result<i64, NumError> {
    let value = value.ok_or(NumError::Missing)?;
    strtonum(&expand(server, found, value), min, max)
}
