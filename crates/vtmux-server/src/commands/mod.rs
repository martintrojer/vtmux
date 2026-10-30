//! Command execution: one file per command, dispatched by table entry.
//!
//! A batch of commands (one client's argv, or one config file) runs in
//! order and stops at the first error, as tmux's queue drops the rest of a
//! failed command list (cmd-queue.c `cmdq_remove_group`). Output goes to the
//! client in order; errors and prints follow `cmdq_error` / `cmdq_print`
//! (cmd-queue.c:820-886): a command client gets `text\n` on stderr /
//! stdout (sanitised when it has no UTF-8), a config run (no client)
//! records errors as causes and drops prints.

mod capture_pane;
mod display_message;
mod kill_server;
mod kill_session;
mod list_sessions;
mod new_session;
mod send_keys;
pub mod source_file;

use std::path::PathBuf;

use vtmux_core::cmd::{CmdError, CommandCall, NumError, strtonum};
use vtmux_core::error::EdgeError;
use vtmux_core::format;
use vtmux_core::protocol::TtyInfo;

use crate::format_ctx::Ctx;
use crate::names::utf8_sanitize;
use crate::server::{ClientId, Server};
use crate::target::{self, Found, Origin};

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

/// Run a parsed batch for `item`, stopping at the first error.
pub fn run(server: &mut Server, item: &mut Item, calls: &[CommandCall]) {
    for call in calls {
        if let Err(e) = run_one(server, item, call) {
            let line = match &e {
                CmdError::Edge(edge) => edge.to_bytes(),
                other => other.to_bytes(),
            };
            item.error(server, line);
            return;
        }
    }
}

fn run_one(server: &mut Server, item: &mut Item, call: &CommandCall) -> Result<(), CmdError> {
    call.entry.ensure_implemented()?;
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
        "list-sessions" => list_sessions::exec(server, item, args),
        "display-message" => display_message::exec(server, item, args, found),
        "capture-pane" => capture_pane::exec(server, item, args, found),
        "send-keys" => send_keys::exec(server, item, args, found),
        "source-file" => source_file::exec(server, item, args, found),
        "attach-session" => {
            let session = found.session.ok_or(EdgeError::NoCurrentTarget)?;
            attach(server, item, session)
        }
        "detach-client" => Err(EdgeError::NotImplemented("detach-client".into()).into()),
        other => Err(CmdError::NotImplemented(other.to_owned())),
    }
}

/// Attach the item's client to `session` through the server hook.
pub fn attach(
    server: &mut Server,
    item: &mut Item,
    session: vtmux_core::ids::SessionId,
) -> Result<(), CmdError> {
    let Some(client) = item.client else {
        return Ok(());
    };
    if item.tty.is_none() {
        return Err(EdgeError::OpenTerminalFailed("not a terminal".into()).into());
    }
    server.attach_client(client, session)?;
    item.after = After::Stay;
    Ok(())
}

/// `format_single` with the command's target.
pub fn expand(server: &Server, found: Found, template: &str) -> String {
    let ctx = Ctx::new(&server.model, &server.info, &server.global_env, found);
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
