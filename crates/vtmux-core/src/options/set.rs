//! `set-option`, `set-window-option` and `set-hook` (cmd-set-option.c
//! `cmd_set_option_exec`), over an [`OptionsStore`].

use std::collections::BTreeMap;

use super::table::{ArrayKey, Kind, Level, MatchError, Matched, OptionDef, match_name};
use super::value::{self, OptionValue, ShellCheck, Value};
use super::{Entry, OptionError, OptionHost, OptionsStore, Target, TreeId, resolve_tree};
use crate::cmd::CommandCall;

/// What a successful `set` asks the server to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// Nothing changed (`-q` swallowed an error, `-o` found it set, `-u` on
    /// an option that was not set here).
    Unchanged,
    /// The option changed: the server applies `options_push_changes(name)`.
    Changed(String),
    /// `set-hook -R`: run this hook now.
    RunHook(String),
    /// `set-hook -E`: fire this user event now.
    FireEvent(String),
}

struct Shell<'a>(&'a dyn OptionHost);

impl ShellCheck for Shell<'_> {
    fn usable_shell(&self, path: &str) -> bool {
        self.0.usable_shell(path)
    }
}

/// Run one `set-option`, `set-window-option` or `set-hook` (or an alias of
/// one) against `store`. `target` is what the server resolved `-t` to.
pub fn apply_set(
    store: &mut OptionsStore,
    call: &CommandCall,
    target: &Target,
    host: &mut dyn OptionHost,
) -> Result<SetOutcome, OptionError> {
    let args = &call.args;
    let name = call.entry.name;
    let window_cmd = name == "set-window-option";
    let hook_cmd = name == "set-hook";
    let quiet = args.flag('q');

    if hook_cmd && args.flag('E') {
        return fire_event(store, args, host);
    }
    if hook_cmd && args.flag('B') {
        return set_monitor(store, args, target);
    }
    let Some(first) = args.positional_text(0) else {
        return Err(OptionError::MissingArgument);
    };
    let argument = host.expand(store, &first, &[]);
    if hook_cmd && args.flag('R') {
        return Ok(SetOutcome::RunHook(argument));
    }

    let matched = match match_name(&argument) {
        Ok(m) => m,
        Err(_) if quiet => return Ok(SetOutcome::Unchanged),
        // options_match leaves `ambiguous` unset when options_parse fails;
        // in the reference build `set` reads a non-zero value there and
        // says "ambiguous" (`set -g 'x[' 1` -> `ambiguous option: x[`).
        Err(MatchError::Ambiguous | MatchError::Unparsable) => {
            return Err(OptionError::AmbiguousOption(argument));
        }
        Err(MatchError::Unknown) => return Err(OptionError::InvalidOption(argument)),
    };
    let (def, opt_name, key) = match matched {
        Matched::Table(def, key) => (Some(def), def.name.to_owned(), key),
        Matched::User(name, key) => (None, name, key),
    };

    let mut value = args.positional_text(1).map(std::borrow::Cow::into_owned);
    if args.flag('F')
        && let Some(v) = &value
    {
        value = Some(host.expand(store, v, &[]));
    }

    let (tree, level) = match resolve_tree(def, args, window_cmd, target) {
        Ok(t) => t,
        Err(_) if quiet => return Ok(SetOutcome::Unchanged),
        Err(e) => return Err(e),
    };
    let here = store.tree(tree).get_only(&opt_name).is_some();
    let parent_is_array = store.get(tree, &opt_name).is_some_and(Entry::is_array);

    // Array options and keys must match up (even with -q).
    if key.is_some() && (def.is_none() || !parent_is_array) {
        return Err(OptionError::NotAnArray(argument));
    }

    // -o: only if not already set here.
    if !args.flag('u') && args.flag('o') {
        let already = match (&key, store.tree(tree).get_only(&opt_name)) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(k), Some(e)) => {
                matches!(&e.value, OptionValue::Array(items) if items.contains_key(k))
            }
        };
        if already {
            if quiet {
                return Ok(SetOutcome::Unchanged);
            }
            return Err(OptionError::AlreadySet(argument));
        }
    }

    if args.flag('u') || args.flag('U') {
        // -U on a window option also unsets it in the window's panes.
        if args.flag('U')
            && level == Level::Window
            && let Some(w) = target.window
        {
            for pane in store.panes_of(w) {
                store.remove_or_default(TreeId::Pane(pane), &opt_name, key.as_ref());
            }
        }
        if !here {
            return Ok(SetOutcome::Unchanged);
        }
        store.remove_or_default(tree, &opt_name, key.as_ref());
        return Ok(SetOutcome::Changed(opt_name));
    }

    let append = args.flag('a');
    let Some(def) = def else {
        // A user option: a string, appended without a separator.
        let value = value.ok_or(OptionError::EmptyValue)?;
        store.set_string(tree, &opt_name, None, &value, append);
        if hook_cmd {
            store.hook_events.insert(opt_name.clone());
        }
        return Ok(SetOutcome::Changed(opt_name));
    };

    if key.is_none() && !parent_is_array {
        store.set_from_string(tree, def, value.as_deref(), append, host)?;
        return Ok(SetOutcome::Changed(opt_name));
    }

    let value = value.ok_or(OptionError::EmptyValue)?;
    store.set_array(tree, def, key, &value, append, host)?;
    Ok(SetOutcome::Changed(opt_name))
}

/// `cmd_set_hook_monitor_exec` (cmd-set-option.c:135-218): `set-hook -B
/// name:what:format [command]` gives the user hook `name` a monitor that
/// runs it when the format's value changes; `-u -B name` takes the monitor
/// away and leaves the option. A command sets (`-a` appends to) the
/// option's text first.
fn set_monitor(
    store: &mut OptionsStore,
    args: &crate::cmd::Args,
    target: &Target,
) -> Result<SetOutcome, OptionError> {
    if args.positionals.len() > 1 {
        return Err(OptionError::TooManyArguments);
    }
    let spec = args.value('B').unwrap_or_default();
    let parsed = super::monitor::parse_monitor(spec);
    let name = match parsed {
        Some((name, ..)) => name,
        None if args.flag('u') => spec,
        None => return Err(OptionError::InvalidSubscription(spec.to_owned())),
    };
    if !name.starts_with('@') {
        return Err(OptionError::MonitorName);
    }
    let (tree, _) = resolve_tree(None, args, false, target)?;
    let Some((name, what, format)) = parsed.filter(|_| !args.flag('u')) else {
        store.monitors.remove(&(tree, name.to_owned()));
        return Ok(SetOutcome::Unchanged);
    };
    let mut outcome = SetOutcome::Unchanged;
    if let Some(value) = args.positional_text(0) {
        store.set_string(tree, name, None, &value, args.flag('a'));
        outcome = SetOutcome::Changed(name.to_owned());
    }
    // hooks_monitor_add: a global tree's monitor watches whichever session
    // is first when it looks.
    let session = if tree.is_global() {
        None
    } else {
        target.session
    };
    if store.tree(tree).get_only(name).is_none() {
        store.set_string(tree, name, None, "", false);
    }
    store.monitor_serial += 1;
    let monitor = super::monitor::Monitor {
        what,
        format: format.to_owned(),
        notify_true: args.flag('T'),
        session,
        serial: store.monitor_serial,
        fires: (0, 0),
    };
    store.monitors.insert((tree, name.to_owned()), monitor);
    Ok(outcome)
}

/// `cmd_set_hook_event_exec` (cmd-set-option.c:91): `set-hook -E @name`.
fn fire_event(
    store: &OptionsStore,
    args: &crate::cmd::Args,
    host: &mut dyn OptionHost,
) -> Result<SetOutcome, OptionError> {
    match args.positionals.len() {
        0 => return Err(OptionError::MissingArgument),
        1 => {}
        _ => return Err(OptionError::TooManyArguments),
    }
    let text = args.positional_text(0).unwrap_or_default();
    let event = host.expand(store, &text, &[]);
    if !event.starts_with('@') {
        return Err(OptionError::EventName);
    }
    Ok(SetOutcome::FireEvent(event))
}

impl OptionsStore {
    /// `options_remove_or_default` (options.c:1475): unset an option here,
    /// or one item of it. The global trees keep every table option, so
    /// there "unset" is "back to the default"; a vtmux option is removed
    /// everywhere, so it leaves the listings again.
    pub(super) fn remove_or_default(&mut self, tree: TreeId, name: &str, key: Option<&ArrayKey>) {
        let term = self.term.clone();
        if key.is_none() {
            // A new entry (options_default / options_remove): no fires,
            // and no monitor (options_remove frees it, options.c:415).
            let full = super::table::find(name).map_or(name, |d| d.name);
            self.hook_fires.remove(&(tree, full.to_owned()));
            self.monitors.remove(&(tree, full.to_owned()));
        }
        let options = self.tree_mut(tree);
        let Some(entry) = options.get_only_mut(name) else {
            return;
        };
        match key {
            Some(k) => {
                if let OptionValue::Array(items) = &mut entry.value {
                    items.remove(k);
                }
            }
            None => match entry.def {
                Some(def) if tree.is_global() && !super::vtmux::is_extension(def) => {
                    options.set_default(def, &term);
                }
                _ => {
                    let name = entry
                        .def
                        .map_or_else(|| name.to_owned(), |d| d.name.to_owned());
                    options.remove(&name);
                }
            },
        }
    }

    /// `options_set_string` (options.c:917): `-a` appends to the value set
    /// in this tree (not an inherited one), with the table's separator.
    pub(super) fn set_string(
        &mut self,
        tree: TreeId,
        name: &str,
        def: Option<&'static OptionDef>,
        value: &str,
        append: bool,
    ) {
        let options = self.tree_mut(tree);
        let new = match options.get_only(name) {
            Some(Entry {
                value: OptionValue::Single(Value::String(old)),
                ..
            }) if append => {
                let separator = def.and_then(|d| d.separator).unwrap_or("");
                format!("{old}{separator}{value}")
            }
            _ => value.to_owned(),
        };
        let value = OptionValue::Single(Value::String(new));
        if let Some(entry) = options.get_only_mut(name) {
            entry.value = value;
        } else {
            options.insert(def.map_or(name, |d| d.name), Entry { def, value });
        }
    }

    /// The server setting a table option itself (`options_set_number(w->options,
    /// "automatic-rename", 0)` in spawn.c:223, `options_set_string(oo,
    /// "default-size", ...)` in cmd-new-session.c:279). Unknown names are
    /// ignored.
    pub fn set_table_value(&mut self, tree: TreeId, name: &str, value: Value) {
        if let Some(def) = super::table::find(name) {
            self.set_single(tree, def, value);
        }
    }

    /// `options_set_number` / `options_set_command`: replace the value
    /// here, creating the entry if needed.
    fn set_single(&mut self, tree: TreeId, def: &'static OptionDef, value: Value) {
        self.tree_mut(tree).insert(
            def.name,
            Entry {
                def: Some(def),
                value: OptionValue::Single(value),
            },
        );
    }

    /// The items of an array option here, creating it empty if needed
    /// (`options_empty`).
    fn array_mut(
        &mut self,
        tree: TreeId,
        def: &'static OptionDef,
    ) -> &mut BTreeMap<ArrayKey, Value> {
        let options = self.tree_mut(tree);
        if !matches!(
            options.get_only(def.name),
            Some(Entry {
                value: OptionValue::Array(_),
                ..
            })
        ) {
            options.insert(
                def.name,
                Entry {
                    def: Some(def),
                    value: OptionValue::Array(BTreeMap::new()),
                },
            );
        }
        match &mut options.get_only_mut(def.name).expect("just inserted").value {
            OptionValue::Array(items) => items,
            OptionValue::Single(_) => unreachable!("checked above"),
        }
    }

    /// An array option, whole (`options_array_assign`, replacing the items
    /// unless `append`) or one item (`options_array_set`).
    pub(super) fn set_array(
        &mut self,
        tree: TreeId,
        def: &'static OptionDef,
        key: Option<ArrayKey>,
        value: &str,
        append: bool,
        host: &mut dyn OptionHost,
    ) -> Result<(), OptionError> {
        let env = host.config_env();
        let items = self.array_mut(tree, def);
        match key {
            None => {
                if !append {
                    items.clear();
                }
                value::assign_array(def, items, value, env)
            }
            Some(k) => {
                let new = value::parse_item(def, value, items.get(&k), append, env)?;
                items.insert(k, new);
                Ok(())
            }
        }
    }

    /// `options_from_string` (options.c:1286) for a table option that is
    /// not an array.
    pub(super) fn set_from_string(
        &mut self,
        tree: TreeId,
        def: &'static OptionDef,
        value: Option<&str>,
        append: bool,
        host: &mut dyn OptionHost,
    ) -> Result<(), OptionError> {
        if let Kind::String(check) = def.kind {
            let value = value.ok_or(OptionError::EmptyValue)?;
            let old = self
                .get(tree, def.name)
                .map(|e| e.value.to_text(e.def, None, false))
                .unwrap_or_default();
            self.set_string(tree, def.name, Some(def), value, append);
            let new = self
                .tree(tree)
                .get_only(def.name)
                .map(|e| e.value.to_text(e.def, None, false))
                .unwrap_or_default();
            if let Err(e) = value::check_string(check, &new, &Shell(host)) {
                // The old (possibly inherited) value is put back *here*.
                self.set_string(tree, def.name, Some(def), &old, false);
                return Err(e);
            }
            return Ok(());
        }
        let current = match self.get(tree, def.name).map(|e| &e.value) {
            Some(OptionValue::Single(v)) => Some(v.clone()),
            _ => None,
        };
        let new = value::parse_scalar(def, value, current.as_ref(), host.config_env())?;
        self.set_single(tree, def, new);
        Ok(())
    }
}
