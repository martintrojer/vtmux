//! Open prompts and the commands they answer to: a client's status-line
//! prompt (status.c `status_prompt_set`, `status_prompt_key`,
//! `status_prompt_clear`) and a pane's prompt (window.c
//! `window_pane_set_prompt`, `window_pane_prompt_key`,
//! `window_pane_clear_prompt`). The editor lives where it is drawn, in the
//! client's [`StatusLine`](crate::render::status::StatusLine) or on the
//! [`Pane`](crate::model::Pane); the command that opened it (its
//! [`Owner`]) lives beside it and is told when a line is finished, and on
//! every edit of an incremental prompt.
//!
//! Keys reach the prompts before the key tables and are handled at once,
//! not queued (server-client.c:1826-1857), so a prompt works while the
//! client's queue waits on it.

use vtmux_core::ids::PaneId;
use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;

use crate::cmdq::{End, NewBatch};
use crate::commands::{command_prompt, confirm_before};
use crate::prompt::{KeyEnv, KeyOutcome, OpenPrompt, Prompt};
use crate::server::{ClientId, Server};
use crate::target::Found;

/// The command a prompt answers to (`inputcb`, `freecb` and their data).
#[derive(Debug)]
pub(crate) enum Owner {
    Command(command_prompt::Pending),
    Confirm(confirm_before::Pending),
}

/// What the owner is told (`enum prompt_key_result` with the line).
pub(crate) enum Line {
    /// The line is finished (`PROMPT_KEY_CLOSE`); `None`: cancelled.
    Done(Option<String>),
    /// An incremental prompt's edit (`PROMPT_KEY_HANDLED`), with its
    /// direction prefix.
    Changed(String),
    /// An incremental prompt's movement key (`PROMPT_KEY_MOVE`).
    Move,
}

/// What the owner wants after a line (`enum prompt_result`).
pub(crate) enum Reply {
    Close,
    /// `PROMPT_CONTINUE`: the prompt stays.
    Continue,
    /// `PROMPT_CONTINUE` with `status_prompt_update` /
    /// `window_pane_update_prompt`: the next prompt.
    Next {
        label: String,
        input: String,
    },
}

/// Where a prompt is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    Status(ClientId),
    Pane(PaneId),
}

impl Owner {
    /// The input callback (`c` is the client the key came from).
    fn input(&mut self, server: &mut Server, c: Option<ClientId>, line: Line) -> Reply {
        match self {
            Self::Command(p) => p.input(server, c, line),
            Self::Confirm(p) => match line {
                Line::Done(value) => p.input(server, value.as_deref()),
                Line::Changed(_) | Line::Move => Reply::Continue,
            },
        }
    }

    /// The free callback: a command still waiting carries on.
    pub(crate) fn free(self, server: &mut Server) {
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
        let start = editor.incremental_start();
        let Some(client) = self.clients.get_mut(&c) else {
            owner.free(self);
            return;
        };
        client.status.set_prompt(OpenPrompt { editor, state });
        client.prompt_owner = Some(owner);
        if let Some(start) = start {
            self.prompt_fire(Place::Status(c), Some(c), Line::Changed(start), false);
        }
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

    /// `window_pane_set_prompt`: `editor` over pane `p`, opened by client
    /// `c`. Any prompt already there is cleared; pane-prompt-opened fires.
    pub(crate) fn pane_prompt_set(
        &mut self,
        p: PaneId,
        c: ClientId,
        mut editor: Prompt,
        state: Found,
        owner: Owner,
    ) {
        self.pane_prompt_clear(p);
        editor.is_pane = true;
        let start = editor.incremental_start();
        let ty = editor.ty;
        let Some(pane) = self.model.panes.get_mut(&p) else {
            owner.free(self);
            return;
        };
        pane.prompt = Some(OpenPrompt { editor, state });
        pane.prompt_owner = Some(owner);
        self.model.redraw_panes.insert(p);
        if let Some(start) = start {
            self.prompt_fire(Place::Pane(p), Some(c), Line::Changed(start), false);
        }
        self.fire_pane_prompt("pane-prompt-opened", p, ty.name());
    }

    /// `window_pane_clear_prompt`: the prompt goes, its owner is freed and
    /// pane-prompt-closed fires.
    pub(crate) fn pane_prompt_clear(&mut self, p: PaneId) {
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        let Some(prompt) = pane.prompt.take() else {
            return;
        };
        let owner = pane.prompt_owner.take();
        self.model.redraw_panes.insert(p);
        if let Some(owner) = owner {
            owner.free(self);
        }
        self.fire_pane_prompt("pane-prompt-closed", p, prompt.editor.ty.name());
    }

    /// `window_fire_pane_prompt` (window.c:181-195).
    fn fire_pane_prompt(&mut self, name: &str, p: PaneId, ty: &str) {
        let Some(w) = self.model.panes.get(&p).map(|x| x.window) else {
            return;
        };
        let ev = crate::events::Event::new(name)
            .target(Found::from_pane(&self.model, p))
            .pane("pane", p)
            .window(&self.model, "window", w)
            .text("prompt_type", ty);
        self.model.push_event(ev);
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
    pub(crate) fn client_error_message(&mut self, c: Option<ClientId>, msg: &str) {
        let mut item = crate::commands::Item::new(c, self.client_cwd(c, None));
        item.error(self, msg.as_bytes().to_vec());
        self.cmdq_append(c, NewBatch::new(item, Vec::new(), End::Quiet));
    }

    /// `cmdq_append(c, cmdq_get_command(cmdlist, NULL))`: commands at the
    /// end of client `c`'s queue, as a key binding's are; with no client,
    /// the global queue.
    pub(crate) fn client_append(
        &mut self,
        c: Option<ClientId>,
        calls: Vec<vtmux_core::cmd::CommandCall>,
    ) {
        let mut item = crate::commands::Item::new(c, self.client_cwd(c, None));
        if let Some(client) = c.and_then(|c| self.clients.get(&c)) {
            if let Some(hello) = client.hello.as_ref() {
                item.env.clone_from(&hello.env);
                item.utf8 = hello.flags.utf8;
            }
            item.origin.attached = client.attached;
            if client.control.is_some() {
                item.control = Some(0);
            }
        }
        self.cmdq_append(c, NewBatch::new(item, calls, End::Quiet));
    }

    /// The input of a prompt's next line, expanded for the prompt's target
    /// (`prompt_create`, `prompt_update`: `format_expand_time`).
    pub(crate) fn prompt_input(&self, state: Found, input: &str) -> String {
        vtmux_core::format::expand_time(input, &self.format_ctx(state, None))
    }

    fn open_prompt_mut(&mut self, place: Place) -> Option<&mut OpenPrompt> {
        match place {
            Place::Status(c) => self.clients.get_mut(&c)?.status.prompt_mut(),
            Place::Pane(p) => {
                self.model.redraw_panes.insert(p);
                self.model.panes.get_mut(&p)?.prompt.as_mut()
            }
        }
    }

    fn take_owner(&mut self, place: Place) -> Option<Owner> {
        match place {
            Place::Status(c) => self.clients.get_mut(&c)?.prompt_owner.take(),
            Place::Pane(p) => self.model.panes.get_mut(&p)?.prompt_owner.take(),
        }
    }

    /// Put `owner` back at `place`, unless the prompt went meanwhile:
    /// then it is freed.
    fn restore_owner(&mut self, place: Place, owner: Owner) {
        match place {
            Place::Status(c) => {
                if let Some(client) = self.clients.get_mut(&c)
                    && client.status.prompt().is_some()
                {
                    client.prompt_owner = Some(owner);
                    return;
                }
            }
            Place::Pane(p) => {
                if let Some(pane) = self.model.panes.get_mut(&p)
                    && pane.prompt.is_some()
                {
                    pane.prompt_owner = Some(owner);
                    return;
                }
            }
        }
        owner.free(self);
    }

    /// Close the prompt at `place` without telling its owner again.
    fn close_place(&mut self, place: Place) {
        match place {
            Place::Status(c) => self.status_prompt_clear(c),
            Place::Pane(p) => self.pane_prompt_clear(p),
        }
    }

    /// `server_client_handle_key0` for the prompts (server-client.c:
    /// 1826-1857): client `c`'s status-line prompt, then a pane prompt in
    /// its current window (the active pane's, else the first visible
    /// pane's). `true` when a prompt took the key.
    pub(crate) fn prompts_key(&mut self, c: ClientId, key: Key) -> bool {
        if self
            .clients
            .get(&c)
            .is_some_and(|x| x.status.prompt().is_some())
        {
            match self.prompt_key(Place::Status(c), c, key) {
                Taken::Yes => return true,
                Taken::No | Taken::Move => {}
            }
        }
        let Some(p) = self.client_pane_prompt(c) else {
            return false;
        };
        // A movement key a pane prompt handled is used up too.
        !matches!(self.prompt_key(Place::Pane(p), c, key), Taken::No)
    }

    /// The pane prompt client `c`'s keys go to (server-client.c:1837-1847).
    fn client_pane_prompt(&self, c: ClientId) -> Option<PaneId> {
        let s = self.clients.get(&c)?.attached?;
        let (_, w, active) = self.model.current_of(s)?;
        let window = self.model.windows.get(&w)?;
        let has = |p: &PaneId| self.model.panes.get(p).is_some_and(|x| x.prompt.is_some());
        if has(&active) {
            return window.is_visible(active).then_some(active);
        }
        window
            .panes
            .panes
            .iter()
            .copied()
            .find(|p| has(p) && window.is_visible(*p))
    }

    /// `prompt_key` on the prompt at `place` for a key from client `c`,
    /// and what follows from it (`status_prompt_key`,
    /// `window_pane_prompt_key`).
    fn prompt_key(&mut self, place: Place, c: ClientId, key: Key) -> Taken {
        let limit = usize::try_from(
            self.model
                .number_option(TreeId::Server, "prompt-history-limit"),
        )
        .unwrap_or(0);
        let paste = self.model.paste.top().map(|b| b.data.clone());
        let history = &mut self.prompt_history;
        let editor = match place {
            Place::Status(c) => self
                .clients
                .get_mut(&c)
                .and_then(|cl| cl.status.prompt_mut()),
            Place::Pane(p) => {
                self.model.redraw_panes.insert(p);
                self.model.panes.get_mut(&p).and_then(|x| x.prompt.as_mut())
            }
        };
        let Some(prompt) = editor else {
            return Taken::No;
        };
        let mut env = KeyEnv {
            history,
            history_limit: limit,
            paste: paste.as_deref(),
        };
        let outcome = prompt.editor.key(key, &mut env);
        let taken = match outcome {
            KeyOutcome::Handled => return Taken::Yes,
            KeyOutcome::Close => {
                self.close_place(place);
                return Taken::Yes;
            }
            KeyOutcome::Done(value) => {
                self.prompt_fire(place, Some(c), Line::Done(value), false);
                Taken::Yes
            }
            KeyOutcome::DoneClose { value, pass_on } => {
                self.prompt_fire(place, Some(c), Line::Done(Some(value)), true);
                if pass_on { Taken::No } else { Taken::Yes }
            }
            KeyOutcome::Changed(line) => {
                self.prompt_fire(place, Some(c), Line::Changed(line), false);
                Taken::Yes
            }
            // prompt_check_move: a movement key the owner closes the
            // prompt on is PROMPT_KEY_CLOSE, used up.
            KeyOutcome::Move => {
                if self.prompt_fire(place, Some(c), Line::Move, false) {
                    Taken::Yes
                } else {
                    Taken::Move
                }
            }
        };
        self.cmdq_process();
        taken
    }

    /// `prompt_fire_callback`: the owner gets the line and closes the
    /// prompt, keeps it, or puts up the next one. `always_close`: `-k`
    /// and `-N` close whatever the owner says. `true` when the prompt
    /// closed.
    fn prompt_fire(
        &mut self,
        place: Place,
        c: Option<ClientId>,
        line: Line,
        always_close: bool,
    ) -> bool {
        let Some(mut owner) = self.take_owner(place) else {
            return false;
        };
        match owner.input(self, c, line) {
            Reply::Continue if !always_close => {
                self.restore_owner(place, owner);
                false
            }
            Reply::Next { label, input } if !always_close => {
                let state = self.open_prompt_mut(place).map(|p| p.state);
                let Some(state) = state else {
                    owner.free(self);
                    return true;
                };
                let input = self.prompt_input(state, &input);
                if let Some(p) = self.open_prompt_mut(place) {
                    p.editor.update(label, &input);
                }
                self.restore_owner(place, owner);
                false
            }
            Reply::Continue | Reply::Next { .. } | Reply::Close => {
                // The owner is freed once, then the prompt goes with
                // nothing left to tell.
                owner.free(self);
                self.close_place(place);
                true
            }
        }
    }
}

/// Whether a prompt took a key (`PROMPT_KEY_*` as the key path sees it).
enum Taken {
    Yes,
    No,
    /// `PROMPT_KEY_MOVE`: the status prompt passes it on to the panes'
    /// prompts and the key tables; a pane prompt uses it up.
    Move,
}
