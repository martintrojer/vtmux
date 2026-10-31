//! A client's status-line prompt (status.c `status_prompt_set`,
//! `status_prompt_key`, `status_prompt_clear`): the editor lives in the
//! client's [`StatusLine`](crate::render::status::StatusLine), which draws
//! it; the command that opened it (its [`Owner`]) lives on the client and
//! is told when a line is finished.
//!
//! Keys reach the prompt before the key tables and are handled at once,
//! not queued (server-client.c:1826-1835), so a prompt works while the
//! client's queue waits on it.

use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;

use crate::cmdq::{End, NewBatch};
use crate::commands::{command_prompt, confirm_before};
use crate::prompt::{KeyEnv, KeyOutcome, Prompt};
use crate::render::status::StatusPrompt;
use crate::server::{ClientId, Server};
use crate::target::Found;

/// The command a prompt answers to (`inputcb`, `freecb` and their data).
#[derive(Debug)]
pub(crate) enum Owner {
    Command(command_prompt::Pending),
    Confirm(confirm_before::Pending),
}

/// What the owner wants after a line (`enum prompt_result`).
pub(crate) enum Reply {
    Close,
    /// `PROMPT_CONTINUE` with `status_prompt_update`: the next prompt.
    Next {
        label: String,
        input: String,
    },
}

impl Owner {
    /// The input callback with a finished line (`None`: cancelled).
    fn input(&mut self, server: &mut Server, c: ClientId, value: Option<String>) -> Reply {
        match self {
            Self::Command(p) => p.input(server, c, value),
            Self::Confirm(p) => p.input(server, value.as_deref()),
        }
    }

    /// The free callback: a command still waiting carries on.
    fn free(self, server: &mut Server) {
        match self {
            Self::Command(p) => p.free(server),
            // cmd_confirm_before_free does not continue the command
            // (cmd-confirm-before.c:157-163): a prompt replaced or lost
            // before an answer leaves it waiting, as in tmux.
            Self::Confirm(_) => {}
        }
    }
}

impl Server {
    /// `status_prompt_set`: `editor` on client `c`'s status line, its
    /// formats expanded for `state`. Any message goes, and any prompt
    /// already there is cleared (its owner told).
    pub(crate) fn status_prompt_set(
        &mut self,
        c: ClientId,
        editor: Prompt,
        state: Found,
        owner: Owner,
    ) {
        self.status_prompt_clear(c);
        let Some(client) = self.clients.get_mut(&c) else {
            owner.free(self);
            return;
        };
        client.status.set_prompt(StatusPrompt { editor, state });
        client.prompt_owner = Some(owner);
    }

    /// `status_prompt_clear`: the prompt goes and its owner is freed.
    pub(crate) fn status_prompt_clear(&mut self, c: ClientId) {
        let Some(client) = self.clients.get_mut(&c) else {
            return;
        };
        client.status.clear_prompt();
        if let Some(owner) = client.prompt_owner.take() {
            owner.free(self);
        }
    }

    /// A new editor with client `c`'s session options (`prompt_set_options`:
    /// `status-keys`, `word-separators`).
    pub(crate) fn prompt_editor(
        &self,
        c: ClientId,
        label: String,
        input: &str,
        kind: crate::prompt::Kind,
        ty: crate::prompt_history::PromptType,
    ) -> Prompt {
        let tree = self
            .client_session(c)
            .map_or(TreeId::GlobalSession, TreeId::Session);
        let vi = self.model.number_option(tree, "status-keys") == 1;
        let separators = self
            .model
            .options
            .get(tree, "word-separators")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        Prompt::new(label, input, kind, ty).with_options(vi, separators)
    }

    /// `cmdq_append(c, cmdq_get_error(error))`: an error for client `c`,
    /// which an attached client shows as its message.
    pub(crate) fn client_error_message(&mut self, c: ClientId, msg: &str) {
        let mut item = crate::commands::Item::new(Some(c), self.client_cwd(Some(c), None));
        item.error(self, msg.as_bytes().to_vec());
        self.cmdq_append(Some(c), NewBatch::new(item, Vec::new(), End::Quiet));
    }

    /// `cmdq_append(c, cmdq_get_command(cmdlist, NULL))`: commands at the
    /// end of client `c`'s queue, as a key binding's are.
    pub(crate) fn client_append(&mut self, c: ClientId, calls: Vec<vtmux_core::cmd::CommandCall>) {
        let Some(client) = self.clients.get(&c) else {
            return;
        };
        let mut item = crate::commands::Item::new(Some(c), self.client_cwd(Some(c), None));
        if let Some(hello) = client.hello.as_ref() {
            item.env.clone_from(&hello.env);
            item.utf8 = hello.flags.utf8;
        }
        item.origin.attached = client.attached;
        if client.control.is_some() {
            item.control = Some(0);
        }
        self.cmdq_append(Some(c), NewBatch::new(item, calls, End::Quiet));
    }

    /// The input of a prompt's next line, expanded for the prompt's target
    /// (`prompt_create`, `prompt_update`: `format_expand_time`).
    pub(crate) fn prompt_input(&self, state: Found, input: &str) -> String {
        vtmux_core::format::expand_time(input, &self.format_ctx(state, None))
    }

    /// `status_prompt_key`: `true` when client `c`'s prompt took the key;
    /// `false` (no prompt, or a `-N` prompt done by a non-digit) when it
    /// goes on to the key tables.
    pub(crate) fn status_prompt_key(&mut self, c: ClientId, key: Key) -> bool {
        let limit = usize::try_from(
            self.model
                .number_option(TreeId::Server, "prompt-history-limit"),
        )
        .unwrap_or(0);
        let paste = self.model.paste.top().map(|b| b.data.clone());
        let Some(prompt) = self
            .clients
            .get_mut(&c)
            .and_then(|cl| cl.status.prompt_mut())
        else {
            return false;
        };
        let mut env = KeyEnv {
            history: &mut self.prompt_history,
            history_limit: limit,
            paste: paste.as_deref(),
        };
        let (taken, value, always_close) = match prompt.editor.key(key, &mut env) {
            KeyOutcome::Handled => return true,
            KeyOutcome::Close => {
                self.status_prompt_clear(c);
                return true;
            }
            KeyOutcome::Done(value) => (true, value, false),
            KeyOutcome::DoneClose { value, pass_on } => (!pass_on, Some(value), true),
        };
        self.prompt_done(c, value, always_close);
        self.cmdq_process();
        taken
    }

    /// `prompt_fire_callback`: the owner gets the line and either closes
    /// the prompt or puts up the next one.
    fn prompt_done(&mut self, c: ClientId, value: Option<String>, always_close: bool) {
        let Some(mut owner) = self
            .clients
            .get_mut(&c)
            .and_then(|cl| cl.prompt_owner.take())
        else {
            return;
        };
        match owner.input(self, c, value) {
            Reply::Next { label, input } if !always_close => {
                let Some(state) = self
                    .clients
                    .get(&c)
                    .and_then(|cl| cl.status.prompt())
                    .map(|p| p.state)
                else {
                    owner.free(self);
                    return;
                };
                let input = self.prompt_input(state, &input);
                let client = self.clients.get_mut(&c).expect("client");
                if let Some(p) = client.status.prompt_mut() {
                    p.editor.update(label, &input);
                }
                client.prompt_owner = Some(owner);
            }
            Reply::Next { .. } | Reply::Close => {
                if let Some(client) = self.clients.get_mut(&c) {
                    client.status.clear_prompt();
                }
                owner.free(self);
            }
        }
    }
}
