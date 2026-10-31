//! `show-options`, `show-window-options` and `show-hooks`
//! (cmd-show-options.c), over an [`OptionsStore`].

use std::fmt::Write as _;

use super::table::{ArrayKey, Level, MatchError, Matched, match_name, table};
use super::value::OptionValue;
use super::{Entry, OptionError, OptionHost, OptionsStore, Target, TreeId, resolve_tree};
use crate::cmd::{CommandCall, args_escape};

/// How one line is printed: the default template's fields, or `-F`.
struct Printer<'a> {
    template: Option<&'a str>,
    value_only: bool,
}

/// Run one `show-options`, `show-window-options` or `show-hooks` (or an
/// alias of one) against `store`: the lines it prints, in order.
pub fn apply_show(
    store: &OptionsStore,
    call: &CommandCall,
    target: &Target,
    host: &mut dyn OptionHost,
) -> Result<Vec<String>, OptionError> {
    let args = &call.args;
    let window_cmd = call.entry.name == "show-window-options";
    let hooks_cmd = call.entry.name == "show-hooks";
    let quiet = args.flag('q');
    if hooks_cmd && args.flag('B') {
        return Err(OptionError::Unsupported("show-hooks -B"));
    }
    let printer = Printer {
        template: args.value('F'),
        value_only: args.flag('v'),
    };
    let mut out = Vec::new();

    let Some(first) = args.positional_text(0) else {
        let (tree, level) = match resolve_tree(None, args, window_cmd, target) {
            Ok(t) => t,
            Err(_) if quiet => return Ok(out),
            Err(e) => return Err(e),
        };
        show_all(
            store, tree, level, hooks_cmd, args, &printer, host, &mut out,
        );
        return Ok(out);
    };

    let argument = host.expand(store, &first, &[]);
    let matched = match match_name(&argument) {
        Ok(m) => m,
        Err(_) if quiet => return Ok(out),
        Err(MatchError::Ambiguous) => return Err(OptionError::AmbiguousOption(argument)),
        // See MatchError::Unparsable: the reference's `show` says
        // "invalid" here.
        Err(MatchError::Unknown | MatchError::Unparsable) => {
            return Err(OptionError::InvalidOption(argument));
        }
    };
    let (def, name, key) = match matched {
        Matched::Table(def, key) => (Some(def), def.name.to_owned(), key),
        Matched::User(name, key) => (None, name, key),
    };
    let (tree, _) = match resolve_tree(def, args, window_cmd, target) {
        Ok(t) => t,
        Err(_) if quiet => return Ok(out),
        Err(e) => return Err(e),
    };
    let (entry, parent) = match store.tree(tree).get_only(&name) {
        Some(e) => (Some(e), false),
        // A global tree holds every tmux option; a vtmux one it does not
        // hold reads as its default when named. Nothing is above a global
        // tree, so `-A` marks nothing inherited there, as for a tmux option.
        None if tree.is_global() => (super::vtmux::default_entry(&name), false),
        None if args.flag('A') => (store.get(tree, &name), true),
        None => (None, false),
    };
    match entry {
        Some(entry) => {
            let empty_array = matches!(&entry.value, OptionValue::Array(items) if items.is_empty());
            let parent = parent && !(key.is_none() && entry.is_array() && empty_array);
            print(
                store,
                &name,
                entry,
                key.as_ref(),
                parent,
                hooks_cmd,
                &printer,
                host,
                &mut out,
            );
        }
        None if def.is_none() && !quiet => return Err(OptionError::InvalidOption(argument)),
        None => {}
    }
    Ok(out)
}

/// `cmd_show_options_all` (cmd-show-options.c:344): the user options set in
/// the tree, then every table option of the level in table order, then the
/// vtmux options set in the tree (or, with `-A`, above it): one never set
/// is not listed, so a fresh server lists exactly what tmux does.
#[allow(clippy::too_many_arguments)]
fn show_all(
    store: &OptionsStore,
    tree: TreeId,
    level: Level,
    hooks_cmd: bool,
    args: &crate::cmd::Args,
    printer: &Printer<'_>,
    host: &mut dyn OptionHost,
    out: &mut Vec<String>,
) {
    let with_hooks = args.flag('H');
    for (name, entry) in store.tree(tree).iter() {
        if entry.def.is_some() {
            continue;
        }
        let user_hook = store.is_hook_event(name);
        let shown = if hooks_cmd {
            user_hook
        } else {
            !user_hook || with_hooks
        };
        if shown {
            print(
                store, name, entry, None, false, hooks_cmd, printer, host, out,
            );
        }
    }
    let extensions = super::vtmux::EXTENSIONS.iter().map(|def| (def, true));
    for (def, extension) in table().iter().map(|def| (def, false)).chain(extensions) {
        if !def.scope.includes(level) {
            continue;
        }
        if (!hooks_cmd && !with_hooks && def.hook) || (hooks_cmd && !def.hook) {
            continue;
        }
        let (entry, parent) = match store.tree(tree).get_only(def.name) {
            Some(e) => (e, false),
            None if args.flag('A') => match store.get_inherited(tree, def.name, extension) {
                Some(e) => (e, true),
                None => continue,
            },
            None => continue,
        };
        print(
            store, def.name, entry, None, parent, hooks_cmd, printer, host, out,
        );
    }
}

/// `cmd_show_options_print` (cmd-show-options.c:200): one line, or one per
/// item of an array shown without a key.
#[allow(clippy::too_many_arguments)]
fn print(
    store: &OptionsStore,
    name: &str,
    entry: &Entry,
    key: Option<&ArrayKey>,
    parent: bool,
    hooks_cmd: bool,
    printer: &Printer<'_>,
    host: &mut dyn OptionHost,
    out: &mut Vec<String>,
) {
    let mut has_value = true;
    let value = match (&entry.value, key) {
        (OptionValue::Array(items), None) => {
            if !items.is_empty() {
                for k in items.keys() {
                    print(
                        store,
                        name,
                        entry,
                        Some(k),
                        parent,
                        hooks_cmd,
                        printer,
                        host,
                        out,
                    );
                }
                return;
            }
            if printer.template.is_none() && printer.value_only {
                return;
            }
            has_value = false;
            String::new()
        }
        (v, key) => v.to_text(entry.def, key, false),
    };

    let Some(template) = printer.template else {
        // SHOW_OPTIONS_TEMPLATE (cmd-show-options.c:30).
        if printer.value_only {
            out.push(value);
            return;
        }
        let mut line = name.to_owned();
        if let Some(k) = key {
            let _ = write!(line, "[{k}]");
        }
        if parent {
            line.push('*');
        }
        if has_value {
            line.push(' ');
            line.push_str(&if entry.is_string() {
                args_escape(&value)
            } else {
                value
            });
        }
        out.push(line);
        return;
    };

    let flag = |b: bool| String::from(if b { "1" } else { "0" });
    let mut vars = vec![
        ("option_name", name.to_owned()),
        ("option_value", value),
        ("option_value_only", flag(printer.value_only)),
        ("option_is_parent", flag(parent)),
        ("option_is_array", flag(entry.is_array())),
        ("option_is_string", flag(entry.is_string())),
        ("option_is_hook", flag(entry.def.is_some_and(|d| d.hook))),
        ("option_is_user", flag(entry.def.is_none())),
        ("option_has_value", flag(has_value)),
        (
            "option_array_key",
            key.map(ToString::to_string).unwrap_or_default(),
        ),
        ("option_has_array_key", flag(key.is_some())),
    ];
    if hooks_cmd {
        // Fire counts are server state; a server that tracks them adds its
        // own through `expand`.
        vars.push(("hook_fire_count", "0".to_owned()));
    }
    out.push(host.expand(store, template, &vars));
}
