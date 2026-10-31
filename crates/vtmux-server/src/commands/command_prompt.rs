//! `command-prompt` (cmd-command-prompt.c) on the status line: one or more
//! prompts (`-p a,b`, with `-I x,y` their inputs), whose answers fill the
//! template's `%1`, `%2`, `%%` (cmd.c `cmd_template_replace`), and the
//! result runs as commands. Without a template the answer is the command.
//!
//! Without `-b` the command waits until the prompt closes and the commands
//! run right after it; with `-b` they go at the end of the prompt client's
//! queue.
//!
//! Not here yet: `-P` (pane prompts) and `-i` (incremental).

use std::rc::Rc;

use vtmux_core::cmd::template::{replace, replace_list};
use vtmux_core::cmd::{ArgValue, Args, CmdError};
use vtmux_core::error::EdgeError;

use super::Item;
use crate::cmdq::{Source, StepId};
use crate::prompt::Kind;
use crate::prompt_history::PromptType;
use crate::server::{ClientId, Server};
use crate::status_prompt::{Owner, Reply};
use crate::target::{Found, Origin};

/// The template a prompt fills (`struct args_command_state`): a `{ }`
/// block, or text (expanded first with `-F`) parsed once filled.
#[derive(Debug)]
enum Template {
    Block(vtmux_core::cmd::CommandList),
    Text(String),
}

/// A prompt waiting for answers (`struct cmd_command_prompt_cdata`).
#[derive(Debug)]
pub(crate) struct Pending {
    /// The waiting command (`cdata->item`); `None` with `-b`.
    step: Option<StepId>,
    template: Template,
    /// The command's file and line, for parse errors.
    source: Option<Rc<Source>>,
    origin: Origin,
    /// The prompts after the first, with their inputs.
    next: std::vec::IntoIter<(String, String)>,
    answers: Vec<String>,
}

/// `cmd_command_prompt_exec`.
pub fn exec(server: &mut Server, item: &mut Item, args: &Args) -> Result<(), CmdError> {
    for flag in ['P', 'i'] {
        if args.flag(flag) {
            return Err(EdgeError::NotImplemented(format!("command-prompt -{flag}")).into());
        }
    }
    let tc = target_client(server, item, args.value('t'))?;
    if server
        .clients
        .get(&tc)
        .is_some_and(|c| c.status.prompt().is_some())
    {
        return Ok(());
    }
    let wait = !args.flag('b');
    // cmdq_find_flag with no target flag: the target client's current
    // state (cmd_find_from_client).
    let state = server
        .client_session(tc)
        .map(|s| Found::from_session(&server.model, s))
        .unwrap_or_default();
    let template = match args.positionals.first() {
        Some(ArgValue::Commands(list)) => Template::Block(list.clone()),
        Some(value) => Template::Text(expand_if(server, args, tc, state, &value.text())),
        // args_make_commands_prepare's default command.
        None => Template::Text("%1".to_owned()),
    };
    let (prompts, space) = match args.value('p') {
        Some(p) => (p.to_owned(), true),
        None if !args.positionals.is_empty() => (format!("({})", first_name(&template)), true),
        None => (":".to_owned(), false),
    };
    let mut list: Vec<(String, String)> = if args.flag('l') {
        vec![(prompts, args.value('I').unwrap_or_default().to_owned())]
    } else {
        let mut inputs = args.value('I').map(|i| i.split(','));
        prompts
            .split(',')
            .map(|p| {
                let label = if space { format!("{p} ") } else { p.to_owned() };
                let input = inputs.as_mut().and_then(Iterator::next).unwrap_or_default();
                (label, input.to_owned())
            })
            .collect()
    };
    let ty = match args.value('T') {
        Some(t) => {
            PromptType::parse(t).ok_or_else(|| EdgeError::Message(format!("unknown type: {t}")))?
        }
        None => PromptType::Command,
    };
    let kind = if args.flag('1') {
        Kind::Single
    } else if args.flag('N') {
        Kind::Numeric
    } else if args.flag('k') {
        Kind::Key
    } else if args.flag('e') {
        Kind::BackspaceExit
    } else {
        Kind::Line
    };
    let (label, input) = list.remove(0);
    let input = server.prompt_input(state, &input);
    let mut editor = server.prompt_editor(tc, label, &input, kind, ty);
    editor.no_freeze = args.flag('C');
    let pending = Pending {
        step: item.step.filter(|_| wait),
        template,
        source: item.source.clone(),
        origin: Origin {
            attached: server.client_session(tc),
            ..Origin::default()
        },
        next: list.into_iter(),
        answers: Vec::new(),
    };
    server.status_prompt_set(tc, editor, state, Owner::Command(pending));
    if wait {
        item.blocked = true;
    }
    Ok(())
}

/// `-F`: the template text expanded for the target client and state
/// (`format_single_from_target`).
fn expand_if(server: &Server, args: &Args, tc: ClientId, state: Found, text: &str) -> String {
    if args.flag('F') {
        vtmux_core::format::expand_str(text, &server.format_ctx(state, Some(tc)))
    } else {
        text.to_owned()
    }
}

/// `args_make_commands_get_command`: a block's first command, or the text
/// up to its first space or comma.
fn first_name(template: &Template) -> String {
    match template {
        Template::Block(list) => list.calls.first().map_or("", |c| c.entry.name).to_owned(),
        Template::Text(t) => t.split([' ', ',']).next().unwrap_or_default().to_owned(),
    }
}

/// `CMD_CLIENT_TFLAG`: the `-t` client, or the current one.
pub(crate) fn target_client(
    server: &Server,
    item: &Item,
    name: Option<&str>,
) -> Result<ClientId, EdgeError> {
    match name {
        None => server
            .target_client(item.client, item.origin)
            .ok_or(EdgeError::NoCurrentClient),
        Some(name) => server
            .find_client(item.client, item.origin, Some(name))
            .ok_or_else(|| {
                let name = name.strip_suffix(':').unwrap_or(name);
                EdgeError::Message(format!("can't find client: {name}"))
            }),
    }
}

impl Pending {
    /// `cmd_command_prompt_callback`: collect the answer; after the last
    /// prompt, fill the template and queue the commands.
    pub(crate) fn input(
        &mut self,
        server: &mut Server,
        c: ClientId,
        value: Option<String>,
    ) -> Reply {
        let Some(value) = value else {
            return Reply::Close;
        };
        self.answers.push(value);
        if let Some((label, input)) = self.next.next() {
            return Reply::Next { label, input };
        }
        let calls = match &self.template {
            Template::Block(list) => Ok(replace_list(list, &self.answers).calls),
            Template::Text(text) => {
                let filled = self
                    .answers
                    .iter()
                    .enumerate()
                    .fold(text.clone(), |t, (i, a)| {
                        replace(&t, a, u8::try_from(i + 1).unwrap_or(u8::MAX))
                    });
                super::run_shell::make_commands(
                    server,
                    Some(c),
                    self.origin,
                    self.source.as_deref(),
                    &ArgValue::Str(filled),
                )
            }
        };
        match calls {
            // cmdq_append(c, cmdq_get_error(error)).
            Err(msg) => server.client_error_message(c, &msg),
            Ok(calls) => match self.step {
                Some(step) => {
                    server.cmdq_insert_commands(step, vec![(self.source.clone(), calls)]);
                }
                None => server.client_append(c, calls),
            },
        }
        Reply::Close
    }

    /// `cmd_command_prompt_free`: a waiting command carries on.
    pub(crate) fn free(self, server: &mut Server) {
        if let Some(step) = self.step {
            server.cmdq_continue(step);
        }
    }
}
