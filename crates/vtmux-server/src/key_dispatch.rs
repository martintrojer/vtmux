//! An attached client's keys through the key tables
//! (server-client.c `server_client_key_callback`, 1435-1700): the prefix
//! and `prefix2` options, the client's table (`root`, `prefix`, or one
//! chosen by `switch-client -T`), `bind -r` repeat within `repeat-time`,
//! `prefix-timeout`, and `Any`.
//!
//! Not here yet: pane modes (copy mode's tables), mouse keys, read-only
//! clients, and `assume-paste-time` (every terminal vtmux attaches to has
//! bracketed paste, which turns it off: server-client.c:1352).

use std::time::{Duration, Instant};

use vtmux_core::cmd::CommandList;
use vtmux_core::ids::SessionId;
use vtmux_core::keys::Key;
use vtmux_core::options::{OptionValue, TreeId, Value};

use crate::commands::{self, Output};
use crate::server::{ClientId, Server};
use crate::target::Origin;

/// A client's key state: `c->keytable`, `CLIENT_REPEAT`, `c->last_key`,
/// `c->repeat_timer`, and the two times `prefix-timeout` compares.
#[derive(Clone, Debug)]
pub struct ClientKeys {
    /// The table the next key is looked up in.
    pub table: String,
    /// `CLIENT_REPEAT`: a `bind -r` key ran and `repeat-time` has not passed.
    pub repeating: bool,
    /// The last repeating binding's key (`c->last_key`).
    pub last_key: Option<Key>,
    /// When the repeat timer fires.
    pub repeat_deadline: Option<Instant>,
    /// The client's last key (`c->activity_time`).
    pub activity: Instant,
    /// When the client entered its table (`keytable->activity_time`; tmux
    /// keeps it on the table, shared by the clients in it).
    pub table_since: Instant,
}

impl Default for ClientKeys {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            table: "root".to_owned(),
            repeating: false,
            last_key: None,
            repeat_deadline: None,
            activity: now,
            table_since: now,
        }
    }
}

impl Server {
    fn session_key_option(&self, s: SessionId, name: &str) -> Key {
        match self
            .model
            .options
            .get(TreeId::Session(s), name)
            .map(|e| &e.value)
        {
            Some(OptionValue::Single(Value::Key(k))) => *k,
            _ => Key::NONE,
        }
    }

    /// `server_client_get_key_table`: the session's `key-table`, or `root`.
    pub(crate) fn default_key_table(&self, id: ClientId) -> String {
        let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
            return "root".to_owned();
        };
        let name = self
            .model
            .options
            .get(TreeId::Session(s), "key-table")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        if name.is_empty() {
            "root".to_owned()
        } else {
            name
        }
    }

    /// `server_client_set_key_table`: `None` is the default table. The
    /// table is created if missing (`key_bindings_get_table(name, 1)`).
    pub(crate) fn set_key_table(&mut self, id: ClientId, name: Option<&str>) {
        let name = name.map_or_else(|| self.default_key_table(id), str::to_owned);
        self.key_bindings.ensure_table(&name);
        if let Some(c) = self.clients.get_mut(&id) {
            c.keys.table = name;
            c.keys.table_since = Instant::now();
        }
    }

    /// `server_client_repeat_time`: how long a repeating binding keeps
    /// the table, 0 for none.
    fn repeat_time(&self, id: ClientId, s: SessionId, key: Key, repeat: bool) -> u64 {
        if !repeat {
            return 0;
        }
        let tree = TreeId::Session(s);
        let mut time = self.model.number_option(tree, "repeat-time");
        if time == 0 {
            return 0;
        }
        let keys = &self.clients[&id].keys;
        if !keys.repeating || keys.last_key != Some(key) {
            let initial = self.model.number_option(tree, "initial-repeat-time");
            if initial != 0 {
                time = initial;
            }
        }
        u64::try_from(time).unwrap_or(0)
    }

    /// `server_client_repeat_timer`: repeat ends, back to the default table.
    pub(crate) fn expire_repeat(&mut self, now: Instant) {
        let due: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.keys.repeat_deadline.is_some_and(|d| d <= now))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            let c = self.clients.get_mut(&id).expect("listed");
            c.keys.repeat_deadline = None;
            if c.keys.repeating {
                c.keys.repeating = false;
                self.set_key_table(id, None);
            }
        }
    }

    /// `server_client_key_callback` for a key from client `id`, after the
    /// activity update. `true` when the key was taken by the tables (a
    /// binding ran, or the prefix or a table switch swallowed it); `false`
    /// when it goes to the pane.
    pub(crate) fn handle_key(&mut self, id: ClientId, key: Key) -> bool {
        let Some(s) = self.clients.get(&id).and_then(|c| c.attached) else {
            return true;
        };
        if key == Key::FOCUS_IN || key == Key::FOCUS_OUT {
            return false;
        }
        let mut table = self.clients[&id].keys.table.clone();
        let mut first = table.clone();
        'table_changed: loop {
            // The prefix always switches to the prefix table, unless there.
            let prefix = self.session_key_option(s, "prefix");
            let prefix2 = self.session_key_option(s, "prefix2");
            let pressed = key.key_and_modifiers();
            if (pressed == prefix.key_and_modifiers() || pressed == prefix2.key_and_modifiers())
                && table != "prefix"
            {
                self.set_key_table(id, Some("prefix"));
                return true;
            }
            let was_repeating = self.clients[&id].keys.repeating;
            let mut lookup = pressed;
            loop {
                let binding = self
                    .key_bindings
                    .get(&table, lookup)
                    .map(|b| (b.key, b.repeat, b.commands.clone()));

                // prefix-timeout: a prefix table older than the timeout
                // reverts to the default table, unless a repeat holds it.
                let delay = self.model.number_option(TreeId::Server, "prefix-timeout");
                let keys = &self.clients[&id].keys;
                let age = keys.activity.saturating_duration_since(keys.table_since);
                if delay > 0
                    && table == "prefix"
                    && age > Duration::from_millis(u64::try_from(delay).unwrap_or(0))
                    && !(binding.as_ref().is_some_and(|b| b.1) && keys.repeating)
                {
                    self.set_key_table(id, None);
                    table.clone_from(&self.clients[&id].keys.table);
                    first.clone_from(&table);
                    continue 'table_changed;
                }

                if let Some((bkey, repeat, commands)) = binding {
                    // Repeating, but this binding does not: stop and try
                    // again from the default table.
                    if self.clients[&id].keys.repeating && !repeat {
                        self.clients.get_mut(&id).expect("client").keys.repeating = false;
                        self.set_key_table(id, None);
                        table.clone_from(&self.clients[&id].keys.table);
                        first.clone_from(&table);
                        continue 'table_changed;
                    }
                    let time = self.repeat_time(id, s, bkey, repeat);
                    if time == 0 {
                        self.clients.get_mut(&id).expect("client").keys.repeating = false;
                        self.set_key_table(id, None);
                    } else {
                        let keys = &mut self.clients.get_mut(&id).expect("client").keys;
                        keys.repeating = true;
                        keys.last_key = Some(bkey);
                        keys.repeat_deadline = Some(Instant::now() + Duration::from_millis(time));
                    }
                    self.run_binding(id, &commands, repeat, key);
                    return true;
                }
                if lookup == Key::ANY {
                    break;
                }
                lookup = Key::ANY;
            }

            // Not in this table: if it is not the default table, or a
            // repeat is on, go back to the default table and try again.
            let repeating = self.clients[&id].keys.repeating;
            if table != self.default_key_table(id) || repeating {
                self.set_key_table(id, None);
                table.clone_from(&self.clients[&id].keys.table);
                if repeating {
                    first.clone_from(&table);
                }
                self.clients.get_mut(&id).expect("client").keys.repeating = false;
                continue 'table_changed;
            }
            // Not in the default table either. A key that started in
            // another table is swallowed, not passed to the pane.
            if first != table && !was_repeating {
                self.set_key_table(id, None);
                return true;
            }
            return false;
        }
    }

    /// `key_bindings_dispatch`: the binding's commands run for the client,
    /// as a new queue item (`CMDQ_STATE_REPEAT` for a repeating binding).
    /// A control client sees them in guards with flags 0; an attached
    /// terminal client gets a failure as its status-line message
    /// (`cmdq_error` -> `status_message_set`, cmd-queue.c:882), and output
    /// would go to view mode, which vtmux has not got yet, so it is dropped.
    /// A binding whose command blocks (`wait-for`) drops what follows it.
    fn run_binding(&mut self, id: ClientId, commands: &CommandList, repeat: bool, _key: Key) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let Some(hello) = c.hello.as_ref() else {
            return;
        };
        let control = c.control.is_some();
        let mut item = commands::Item::new(Some(id), hello.cwd.clone());
        item.env.clone_from(&hello.env);
        item.utf8 = hello.flags.utf8;
        item.repeat = repeat;
        item.origin = Origin {
            attached: c.attached,
            inside_pane: None,
        };
        if control {
            item.control = Some(0);
        }
        // The key callback is a queue item of its own and takes a number.
        self.cmd_number = self.cmd_number.wrapping_add(1);
        let _ = commands::run(self, &mut item, &commands.calls);
        if control {
            self.control_take_output(id, &mut item);
            self.control_flush(id);
            return;
        }
        for out in std::mem::take(&mut item.output) {
            if let Output::Stderr(line) = out {
                let text = String::from_utf8_lossy(&line);
                let mut msg = text.trim_end_matches('\n').to_owned();
                // cmdq_error: `*msg = toupper(*msg)`.
                if let Some(first) = msg.get(..1) {
                    let upper = first.to_ascii_uppercase();
                    msg.replace_range(..1, &upper);
                }
                if let Some(c) = self.clients.get_mut(&id) {
                    c.message = Some(msg);
                }
            }
        }
    }

    /// `server_client_set_session` for a client already attached
    /// (switch-client): the old session becomes `last_session`, the new
    /// one's activity is bumped, the client is redrawn and sized.
    pub(crate) fn switch_session(&mut self, id: ClientId, s: SessionId) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let old = c.attached;
        if c.control.is_some() {
            self.control_attach(id, s);
        } else {
            let c = self.clients.get_mut(&id).expect("client");
            c.attached = Some(s);
            c.compositor.invalidate();
            self.model.touch_session(s);
            if let Some(tty) = self
                .clients
                .get(&id)
                .and_then(|c| c.hello.as_ref())
                .and_then(|h| h.tty.clone())
            {
                self.resize_client(id, tty.cols, tty.rows);
            }
        }
        if let Some(c) = self.clients.get_mut(&id)
            && old.is_some_and(|o| o != s)
        {
            c.last_session = old;
        }
        // server_client_set_session: the old window loses the focus, the
        // new one gains it (server-client.c:462-467).
        if let Some(w) = old.and_then(|o| self.model.current_of(o)).map(|c| c.1) {
            self.update_focus(Some(w));
        }
        self.update_focus(self.client_window(id));
        self.session_attached_alerts(s);
    }

    /// Keys `send-keys -K` gave a client: handled as typed, after the
    /// command (`server_client_handle_key_after`), with `KEYC_SENT`.
    pub(crate) fn handle_injected_keys(&mut self) {
        for (id, key) in std::mem::take(&mut self.injected_keys) {
            if self.clients.get(&id).and_then(|c| c.attached).is_none() {
                continue;
            }
            self.touch_client(id);
            if !self.handle_key(id, key)
                && let (Some(p), Some(k)) = (self.client_pane(id), crate::keys::Key::from_core(key))
            {
                self.forward_key(p, &k);
            }
        }
    }
}
