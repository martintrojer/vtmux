//! Key tables and the `bind-key` / `unbind-key` commands (key-bindings.c,
//! cmd-bind-key.c, cmd-unbind-key.c).
//!
//! Tables are ordered by name (`key_table_cmp` is `strcmp`), bindings inside
//! one by key code (`key_bindings_cmp`), which is what a `BTreeMap` gives.
//! The commands are pure functions over parsed [`Args`] that return what
//! tmux's `cmdq_error` would print; the server calls them and prints.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

use super::code::Key;
use super::defaults::DEFAULT_BINDINGS;
use crate::cmd::{
    ArgValue, Args, CmdError, CommandCall, CommandList, ConfigEnv, NoEnv, parse_argv_values,
    parse_string,
};

/// One bound key (`struct key_binding`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// Without flag bits (`key & ~KEYC_MASK_FLAGS`).
    pub key: Key,
    pub note: Option<String>,
    /// `bind -r`: the prefix stays active for `repeat-time`.
    pub repeat: bool,
    pub commands: CommandList,
}

/// One table (`struct key_table`): the bindings now, and the bindings the
/// table started with (`default_key_bindings`, for `key_bindings_reset`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyTable {
    bindings: BTreeMap<Key, Binding>,
    defaults: BTreeMap<Key, Binding>,
}

impl KeyTable {
    /// Bindings in key-code order (`key_bindings_first`/`next`).
    pub fn bindings(&self) -> impl Iterator<Item = &Binding> {
        self.bindings.values()
    }

    #[must_use]
    pub fn get(&self, key: Key) -> Option<&Binding> {
        self.bindings.get(&key.without_flags())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

/// A key command failed: tmux's `cmdq_error` text, if it prints one. A
/// quiet (`-q`) failure still fails the command, so the rest of its group
/// does not run, but says nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdFailed {
    pub message: Option<String>,
}

impl CmdFailed {
    pub(super) fn say_text(message: impl Into<String>) -> Self {
        Self::say(message)
    }

    fn say(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
        }
    }

    fn quiet_or(quiet: bool, message: impl Into<String>) -> Self {
        Self {
            message: (!quiet).then(|| message.into()),
        }
    }
}

impl fmt::Display for CmdFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message.as_deref().unwrap_or(""))
    }
}

impl std::error::Error for CmdFailed {}

impl From<CmdError> for CmdFailed {
    fn from(e: CmdError) -> Self {
        Self::say(e.to_string())
    }
}

/// All key tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyBindings {
    tables: BTreeMap<String, KeyTable>,
}

static DEFAULTS: LazyLock<KeyBindings> = LazyLock::new(KeyBindings::build_defaults);

impl KeyBindings {
    /// No tables at all.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// tmux 3.8's start-up state: `key_bindings_init`.
    #[must_use]
    pub fn with_defaults() -> Self {
        DEFAULTS.clone()
    }

    /// Runs each default string through the command parser and the `bind`
    /// command, as tmux does at start-up, then records the result as the
    /// tables' defaults (`key_bindings_init_done`).
    fn build_defaults() -> Self {
        let mut kb = Self::empty();
        for text in DEFAULT_BINDINGS {
            let list = parse_string(text, &mut NoEnv)
                .unwrap_or_else(|e| panic!("bad default key: {text}: {e}"));
            for call in &list.calls {
                kb.execute(call, &mut NoEnv)
                    .expect("a bind command")
                    .unwrap_or_else(|e| panic!("bad default key: {text}: {e}"));
            }
        }
        for table in kb.tables.values_mut() {
            table.defaults = table.bindings.clone();
        }
        kb
    }

    /// `key_bindings_get_table(name, 0)`.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&KeyTable> {
        self.tables.get(name)
    }

    /// Tables in name order (`key_bindings_first_table`).
    pub fn tables(&self) -> impl Iterator<Item = (&str, &KeyTable)> {
        self.tables.iter().map(|(n, t)| (n.as_str(), t))
    }

    #[must_use]
    pub fn get(&self, table: &str, key: Key) -> Option<&Binding> {
        self.table(table)?.get(key)
    }

    /// `key_bindings_add`. `commands: None` is `bind KEY` with no command:
    /// the table is created, and an existing binding only gets the note and
    /// the repeat flag.
    pub fn add(
        &mut self,
        table: &str,
        key: Key,
        note: Option<&str>,
        repeat: bool,
        commands: Option<CommandList>,
    ) {
        let key = key.without_flags();
        let table = self.tables.entry(table.to_owned()).or_default();
        let Some(commands) = commands else {
            if let Some(binding) = table.bindings.get_mut(&key) {
                if let Some(note) = note {
                    binding.note = Some(note.to_owned());
                }
                if repeat {
                    binding.repeat = true;
                }
            }
            return;
        };
        table.bindings.insert(
            key,
            Binding {
                key,
                note: note.map(str::to_owned),
                repeat,
                commands,
            },
        );
    }

    /// `key_bindings_remove`: the table goes when nothing is left in it,
    /// defaults included.
    pub fn remove(&mut self, table: &str, key: Key) {
        let Some(t) = self.tables.get_mut(table) else {
            return;
        };
        if t.bindings.remove(&key.without_flags()).is_none() {
            return;
        }
        if t.bindings.is_empty() && t.defaults.is_empty() {
            self.tables.remove(table);
        }
    }

    /// `key_bindings_remove_table`. A client that is in the table is moved
    /// back into a table of the same name, created empty, by
    /// `server_client_set_key_table` (so `unbind -a -n` leaves `root`
    /// listable): the server calls [`KeyBindings::ensure_table`] for every
    /// client's key table after running a key command.
    pub fn remove_table(&mut self, table: &str) {
        self.tables.remove(table);
    }

    /// `key_bindings_get_table(name, 1)`: the table, created if missing.
    pub fn ensure_table(&mut self, name: &str) {
        self.tables.entry(name.to_owned()).or_default();
    }

    /// `key_bindings_reset`: back to the default binding, or gone if it has
    /// none.
    pub fn reset(&mut self, table: &str, key: Key) {
        let key = key.without_flags();
        let Some(t) = self.tables.get_mut(table) else {
            return;
        };
        if !t.bindings.contains_key(&key) {
            return;
        }
        match t.defaults.get(&key).cloned() {
            Some(default) => {
                t.bindings.insert(key, default);
            }
            None => self.remove(table, key),
        }
    }

    /// `key_bindings_reset_table`.
    pub fn reset_table(&mut self, table: &str) {
        let Some(t) = self.tables.get(table) else {
            return;
        };
        if t.defaults.is_empty() {
            self.remove_table(table);
            return;
        }
        let keys: Vec<Key> = t.bindings.keys().copied().collect();
        for key in keys {
            self.reset(table, key);
        }
    }

    /// Run `bind-key` or `unbind-key`; `None` for any other command.
    pub fn execute(
        &mut self,
        call: &CommandCall,
        env: &mut dyn ConfigEnv,
    ) -> Option<Result<(), CmdFailed>> {
        match call.entry.name {
            "bind-key" => Some(self.bind_key(&call.args, env)),
            "unbind-key" => Some(self.unbind_key(&call.args)),
            _ => None,
        }
    }

    /// `cmd_bind_key_exec` (cmd-bind-key.c:54-107). `env` is for `$VAR` and
    /// `~` in a command given as one string (`bind x 'display $HOME'`).
    pub fn bind_key(&mut self, args: &Args, env: &mut dyn ConfigEnv) -> Result<(), CmdFailed> {
        let key_text = args.positional_text(0).unwrap_or_default();
        let key = Key::from_name(&key_text)
            .filter(|k| *k != Key::NONE)
            .ok_or_else(|| CmdFailed::say(format!("unknown key: {key_text}")))?;

        let table = table_name(args);
        let note = args.value('N');
        let repeat = args.flag('r');
        let count = args.positionals.len();

        if count == 1 {
            self.add(&table, key, note, repeat, None);
            return Ok(());
        }
        let commands = match (&args.positionals[1], count) {
            (ArgValue::Commands(list), 2) => list.clone(),
            (ArgValue::Str(text), 2) => parse_string(text, env)?,
            _ => CommandList {
                calls: parse_argv_values(&args.positionals[1..])?,
            },
        };
        self.add(&table, key, note, repeat, Some(commands));
        Ok(())
    }

    /// `cmd_unbind_key_exec` (cmd-unbind-key.c:45-104).
    pub fn unbind_key(&mut self, args: &Args) -> Result<(), CmdFailed> {
        let quiet = args.flag('q');
        let key_text = args.positional(0);

        if args.flag('a') {
            if key_text.is_some() {
                return Err(CmdFailed::quiet_or(quiet, "key given with -a"));
            }
            let table = table_name(args);
            if self.table(&table).is_none() {
                return Err(CmdFailed::quiet_or(
                    quiet,
                    format!("table {table} doesn't exist"),
                ));
            }
            self.remove_table(&table);
            return Ok(());
        }

        let Some(key_text) = key_text else {
            return Err(CmdFailed::quiet_or(quiet, "missing key"));
        };
        let key = Key::from_name(key_text)
            .filter(|k| *k != Key::NONE)
            .ok_or_else(|| CmdFailed::quiet_or(quiet, format!("unknown key: {key_text}")))?;

        let table = table_name(args);
        if args.flag('T') && self.table(&table).is_none() {
            return Err(CmdFailed::quiet_or(
                quiet,
                format!("table {table} doesn't exist"),
            ));
        }
        self.remove(&table, key);
        Ok(())
    }
}

/// `-T table`, else `root` for `-n`, else `prefix`.
fn table_name(args: &Args) -> String {
    if let Some(table) = args.value('T') {
        table.to_owned()
    } else if args.flag('n') {
        "root".to_owned()
    } else {
        "prefix".to_owned()
    }
}
