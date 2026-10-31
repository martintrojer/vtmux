//! `confirm-before` (cmd-confirm-before.c): a one-key prompt on the status
//! line; the confirm key (`y`, or `-c`), or Enter with `-y`, runs the
//! command. Without `-b` the command waits for the answer and the command
//! runs right after it; with `-b` it goes at the end of the prompt
//! client's queue.

use vtmux_core::cmd::{ArgValue, Args, CmdError, CommandCall};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::cmdq::StepId;
use crate::prompt::Kind;
use crate::prompt_history::PromptType;
use crate::server::{ClientId, Server};
use crate::status_prompt::{Owner, Reply};
use crate::target::Found;

/// `struct cmd_confirm_before_data`.
#[derive(Debug)]
pub(crate) struct Pending {
    step: Option<StepId>,
    /// The prompt's client, whose queue `-b` appends to.
    client: ClientId,
    /// The waiting command's client (`cmdq_get_client(item)`).
    caller: Option<ClientId>,
    calls: Vec<CommandCall>,
    confirm_key: char,
    default_yes: bool,
}

/// `cmd_confirm_before_exec`.
pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    // args_make_commands_now: the command is parsed before anything else.
    let value = args
        .positionals
        .first()
        .cloned()
        .unwrap_or_else(|| ArgValue::Str(String::new()));
    let calls = super::run_shell::make_commands(
        server,
        item.client,
        item.origin,
        item.source.as_deref(),
        &value,
    )
    .map_err(EdgeError::Message)?;
    let tc = super::command_prompt::target_client(server, item, args.value('t'))?;
    let confirm_key = match args.value('c') {
        None => 'y',
        Some(k) => match k.as_bytes() {
            [b] if (32..127).contains(b) => char::from(*b),
            _ => return Err(EdgeError::Message("invalid confirm key".into()).into()),
        },
    };
    let label = if let Some(p) = args.value('p') {
        format!("{p} ")
    } else {
        let name = calls.first().map_or("", |c| c.entry.name);
        format!("Confirm '{name}'? ({confirm_key}/n) ")
    };
    let wait = !args.flag('b');
    let state = server
        .client_session(tc)
        .map(|s| Found::from_session(&server.model, s))
        .unwrap_or_default();
    let editor = server.prompt_editor(tc, label, "", Kind::Single, PromptType::Command);
    let pending = Pending {
        step: item.step.filter(|_| wait),
        client: tc,
        caller: item.client,
        calls,
        confirm_key,
        default_yes: args.flag('y'),
    };
    server.status_prompt_set(tc, editor, state, Owner::Confirm(pending));
    if wait {
        item.blocked = true;
    }
    Ok(())
}

impl Pending {
    /// `cmd_confirm_before_callback`: the confirm key, or Enter with `-y`,
    /// queues the command. A waiting command client exits 0 if it ran,
    /// else 1 (`retcode`).
    pub(crate) fn input(&mut self, server: &mut Server, value: Option<&str>) -> Reply {
        let first = value.and_then(|v| v.chars().next());
        let yes = first == Some(self.confirm_key) || (first == Some('\r') && self.default_yes);
        if yes {
            let calls = std::mem::take(&mut self.calls);
            match self.step {
                Some(step) => {
                    server.cmdq_insert_commands(step, vec![(None, calls)]);
                }
                None => server.client_append(self.client, calls),
            }
        }
        // out: the waiting command carries on, a command client (no
        // session) with the answer as its exit status (retcode,
        // cmd-confirm-before.c:143-149).
        if let Some(step) = self.step.take() {
            if self
                .caller
                .is_some_and(|c| server.client_session(c).is_none())
            {
                server.cmdq_with_item(step, |_, item| item.status = i32::from(!yes));
            }
            server.cmdq_continue(step);
        }
        Reply::Close
    }
}
