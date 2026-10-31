//! Command execution: one file per command, dispatched by table entry.
//!
//! A batch of commands (one client's argv, or one config file) runs in
//! order and stops at the first error, as tmux's queue drops the rest of a
//! failed command list (cmd-queue.c `cmdq_remove_group`). Output goes to the
//! client in order; errors and prints follow `cmdq_error` / `cmdq_print`
//! (cmd-queue.c:820-886): a command client gets `text\n` on stderr /
//! stdout (sanitised when it has no UTF-8), a config run (no client)
//! records errors as causes and drops prints.
//!
//! A command can block its client's queue (`CMD_RETURN_WAIT`, only
//! `wait-for` so far): [`run`] stops after it and reports where to resume;
//! the server parks the rest of the batch until the client is woken.

mod break_pane;
mod capture_pane;
mod delete_buffer;
mod display_message;
pub(crate) mod file_io;
mod join_pane;
mod keys;
mod kill_pane;
mod kill_server;
mod kill_session;
mod kill_window;
mod list_buffers;
mod list_clients;
mod list_panes;
mod list_sessions;
mod list_windows;
mod load_buffer;
mod move_window;
mod new_session;
mod new_window;
mod paste_buffer;
mod rename_session;
mod rename_window;
mod resize_pane;
mod resize_window;
mod respawn_pane;
mod respawn_window;
mod rotate_window;
mod save_buffer;
mod select_layout;
mod select_pane;
mod select_window;
mod send_keys;
mod set_buffer;
mod set_environment;
mod set_option;
mod show_environment;
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
    /// The command that just ran blocked the client (`CMD_RETURN_WAIT`).
    pub blocked: bool,
    /// Run from a repeating key binding (`CMDQ_STATE_REPEAT`):
    /// `switch-client` then keeps the client's key table.
    pub repeat: bool,
    /// The batch can park (`Server::run_batch`): a command may return
    /// `CMD_RETURN_WAIT` and finish later. Key bindings and config files
    /// cannot yet, so their file I/O completes at once.
    pub can_wait: bool,
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
            can_wait: false,
        }
    }

    /// `cmdq_print`: a line on stdout.
    pub fn print(&mut self, text: &str) {
        if self.client.is_none() {
            return;
        }
        let mut line = if self.utf8 {
            text.to_owned()
        } else {
            utf8_sanitize(text)
        };
        line.push('\n');
        self.output.push(Output::Stdout(line.into_bytes()));
    }

    /// Raw bytes on stdout (`file_print_buffer`).
    pub fn print_raw(&mut self, bytes: Vec<u8>) {
        if self.client.is_some() {
            self.output.push(Output::Stdout(bytes));
        }
    }

    /// `cmdq_error` for a command client; config runs keep the cause.
    pub fn error(&mut self, server: &mut Server, line: Vec<u8>) {
        self.status = 1;
        if self.client.is_none() {
            server
                .causes
                .push(String::from_utf8_lossy(&line).into_owned());
            return;
        }
        let mut line = if self.utf8 {
            line
        } else {
            utf8_sanitize(&String::from_utf8_lossy(&line)).into_bytes()
        };
        line.push(b'\n');
        self.output.push(Output::Stderr(line));
    }
}

/// Run a parsed batch for `item`, stopping at the first error. For a
/// control client each command is wrapped in its own guard
/// (`cmdq_fire_command`, cmd-queue.c:594-655). A command that blocks the
/// client ends the run early: the result is the index of the command to
/// resume at. A blocked command still closes its guard with `%end`, as
/// `CMD_RETURN_WAIT` is not an error.
pub fn run(server: &mut Server, item: &mut Item, calls: &[CommandCall]) -> Option<usize> {
    for (i, call) in calls.iter().enumerate() {
        let guard = server.control_begin(item);
        let result = run_one(server, item, call);
        // A quiet failure (unbind-key -q) ends the group but never reached
        // cmdq_error: no message, no exit status.
        if let Err(e) = result.as_ref()
            && !matches!(e, CmdError::Quiet)
        {
            let line = match e {
                CmdError::Edge(edge) => edge.to_bytes(),
                other => other.to_bytes(),
            };
            item.error(server, line);
        }
        server.control_end(item, guard, result.is_ok());
        server.update_focus_all();
        // send-keys -K: the keys are handled as queue items after this
        // command (server_client_handle_key_after), so after its guard.
        server.handle_injected_keys();
        if result.is_err() {
            return None;
        }
        if std::mem::take(&mut item.blocked) {
            return Some(i + 1);
        }
    }
    None
}

fn run_one(server: &mut Server, item: &mut Item, call: &CommandCall) -> Result<(), CmdError> {
    call.entry.ensure_implemented()?;
    let source = resolve_source(server, item, call)?;
    // cmdq_find_flag (cmd-queue.c:514-530): resolve the target before the
    // command runs; commands without a target slot get the client's
    // current state, failures ignored.
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
        "set-buffer" => set_buffer::exec(server, item, args),
        "delete-buffer" => delete_buffer::exec(server, args),
        "show-buffer" => save_buffer::exec(server, item, args, found, true),
        "save-buffer" => save_buffer::exec(server, item, args, found, false),
        "load-buffer" => load_buffer::exec(server, item, args, found),
        "paste-buffer" => paste_buffer::exec(server, args, found),
        "list-buffers" => list_buffers::exec(server, item, args),
        "bind-key" | "unbind-key" => keys::bind_or_unbind(server, call),
        "list-keys" => keys::list_keys(server, item, args, found),
        "send-prefix" => keys::send_prefix(server, args, found),
        "switch-client" => keys::switch_client(server, item, args),
        "source-file" => source_file::exec(server, item, args, found),
        "wait-for" => wait_for::exec(server, item, args),
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
        "show-options" | "show-window-options" => {
            show_options::exec(server, item, call, found).map_err(Into::into)
        }
        "attach-session" => {
            // cmd-attach-session.c:66-69: checked before the target.
            if server.model.sessions.is_empty() {
                return Err(EdgeError::NoSessions.into());
            }
            let spec = args.target('t');
            let found = target::resolve(
                &server.model,
                item.origin,
                spec.as_ref(),
                vtmux_core::cmd::TargetKind::Session,
                vtmux_core::cmd::FindFlags::NONE,
            )?;
            let session = found.session.ok_or(EdgeError::NoCurrentTarget)?;
            attach(server, item, session, !args.flag('E'))
        }
        "detach-client" => {
            if args.flag('s') {
                let spec = args.target('s');
                let source = target::resolve(
                    &server.model,
                    item.origin,
                    spec.as_ref(),
                    vtmux_core::cmd::TargetKind::Session,
                    vtmux_core::cmd::FindFlags::CANFAIL,
                )?;
                if let Some(s) = source.session {
                    server.detach_session_clients(s);
                }
                Ok(())
            } else {
                // cmd_find_current_client: the calling client when it is
                // attached (cmd-find.c).
                let client = item
                    .client
                    .filter(|&c| server.client_session(c).is_some())
                    .or_else(|| server.current_client())
                    .ok_or(EdgeError::NoCurrentClient)?;
                server.detach_client(client);
                Ok(())
            }
        }
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
) -> Result<(), CmdError> {
    let Some(client) = item.client else {
        return Ok(());
    };
    if update_env && (item.control.is_some() || item.tty.is_some()) {
        server.update_session_environ(client, session);
    }
    if item.control.is_some() {
        // A control client needs no terminal and gets no Ready.
        server.control_attach(client, session);
        item.origin.attached = Some(session);
        reset_key_table(server, item, client);
        return Ok(());
    }
    if item.tty.is_none() {
        return Err(EdgeError::OpenTerminalFailed("not a terminal".into()).into());
    }
    server.attach_client(client, session)?;
    item.after = After::Stay;
    reset_key_table(server, item, client);
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
fn buffer_limit(server: &Server) -> i64 {
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
