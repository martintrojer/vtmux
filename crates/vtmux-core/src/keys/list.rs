//! `list-keys` (cmd-list-keys.c).

use std::cmp::Ordering;
use std::collections::HashMap;

use super::bindings::{Binding, CmdFailed, KeyBindings, KeyTable};
use super::code::Key;
use super::width::char_width;
use crate::cmd::{Args, PrintFlags, args_escape};
use crate::format::{self, FormatContext};

/// What `list-keys` produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListKeys {
    /// One line per binding, in order, as the template expanded.
    pub lines: Vec<String>,
    /// `-1`: one line only. An attached, non-control client shows it as a
    /// status message instead of printing it (cmd-list-keys.c:238).
    pub single: bool,
}

impl ListKeys {
    /// The lines `cmdq_print` writes: empty ones are skipped
    /// (cmd-list-keys.c:241).
    pub fn printed(&self) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .map(String::as_str)
            .filter(|l| !l.is_empty())
    }
}

/// `utf8_cstrwidth` (utf8.c:964): columns a string takes.
#[must_use]
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if c.is_ascii() {
                usize::from(c > '\u{1f}' && c != '\u{7f}')
            } else {
                usize::from(char_width(u32::from(c)))
            }
        })
        .sum()
}

/// `utf8_padcstr`: pad on the right to `width` columns.
fn pad(s: &str, width: usize) -> String {
    let n = display_width(s);
    let mut out = s.to_owned();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(n)));
    out
}

/// `enum sort_order` as `list-keys -O` can name it (sort.c:371-394).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortOrder {
    Activity,
    Creation,
    Index,
    Modifier,
    Name,
    /// `-O order`: the order the bindings are stored in.
    Stored,
    Size,
    Z,
}

fn sort_order_from_string(s: &str) -> Option<SortOrder> {
    let eq = |name: &str| s.eq_ignore_ascii_case(name);
    Some(if eq("activity") {
        SortOrder::Activity
    } else if eq("creation") {
        SortOrder::Creation
    } else if eq("index") || eq("key") {
        SortOrder::Index
    } else if eq("modifier") {
        SortOrder::Modifier
    } else if eq("name") || eq("title") {
        SortOrder::Name
    } else if eq("order") {
        SortOrder::Stored
    } else if eq("size") {
        SortOrder::Size
    } else if eq("z") {
        SortOrder::Z
    } else {
        return None;
    })
}

/// `strcasecmp` on table names.
fn casecmp(a: &str, b: &str) -> Ordering {
    let lower = |s: &str| {
        s.bytes()
            .map(|c| c.to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    lower(a).cmp(&lower(b))
}

/// `sort_key_binding_cmp` (sort.c:309-346), without the reversal.
fn compare(order: SortOrder, a: &Row<'_>, b: &Row<'_>) -> Ordering {
    let primary = match order {
        SortOrder::Index => a.1.key.cmp(&b.1.key),
        SortOrder::Modifier => a.1.key.modifiers().cmp(&b.1.key.modifiers()),
        SortOrder::Name => casecmp(a.0, b.0),
        // These have no meaning for key bindings; they fall through to the
        // table and key tie-breaks.
        SortOrder::Activity
        | SortOrder::Creation
        | SortOrder::Stored
        | SortOrder::Size
        | SortOrder::Z => Ordering::Equal,
    };
    primary
        .then_with(|| casecmp(a.0, b.0))
        .then_with(|| a.1.key.cmp(&b.1.key))
}

/// The variables `list-keys` adds to its format tree, over a base context.
struct Vars<'a> {
    vars: HashMap<&'static str, String>,
    base: &'a dyn FormatContext,
}

impl FormatContext for Vars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned().or_else(|| self.base.get(name))
    }
}

/// `format_true`: a value is true unless it is empty or `0`.
fn is_true(s: &str) -> bool {
    !s.is_empty() && s != "0"
}

/// One row to print: the table it is in and the binding.
type Row<'a> = (&'a str, &'a Binding);

/// The columns and per-listing variables `cmd_list_keys_exec` computes
/// (cmd-list-keys.c:236-241) before it prints a line.
struct Layout {
    key_width: usize,
    table_width: usize,
    has_repeat: bool,
    notes_only: bool,
    prefix: String,
}

impl Layout {
    fn new(rows: &[Row<'_>], notes_only: bool, prefix: String) -> Self {
        Self {
            key_width: rows
                .iter()
                .map(|(_, b)| display_width(&b.key.name(false)))
                .max()
                .unwrap_or(0),
            table_width: rows
                .iter()
                .map(|(t, _)| display_width(t))
                .max()
                .unwrap_or(0),
            has_repeat: rows.iter().any(|(_, b)| b.repeat),
            notes_only,
            prefix,
        }
    }

    /// The built-in template, `LIST_KEYS_TEMPLATE` (cmd-list-keys.c:30-39),
    /// expanded directly.
    fn default_line(&self, (table, binding): Row<'_>) -> String {
        let key = binding.key.name(false);
        let command = binding.commands.print(PrintFlags::LIST_KEYS);
        if self.notes_only {
            // "#{key_prefix} #{p|W:key_string} #{?key_note,#{key_note},#{key_command}}"
            let note = binding.note.as_deref().filter(|n| is_true(n));
            return format!(
                "{} {} {}",
                self.prefix,
                pad(&key, self.key_width),
                note.unwrap_or(&command)
            );
        }
        // "bind-key #{?key_has_repeat,#{?key_repeat,-r,  },} -T
        //  #{p|W:key_table} #{p|W:#{q|a:key_string}} #{key_command}"
        let repeat = match (self.has_repeat, binding.repeat) {
            (false, _) => "",
            (true, true) => "-r",
            (true, false) => "  ",
        };
        format!(
            "bind-key {repeat} -T {} {} {command}",
            pad(table, self.table_width),
            pad(&args_escape(&key), self.key_width),
        )
    }

    /// A `-F` template over the variables `cmd_list_keys_format_add_key_binding`
    /// sets.
    fn custom_line(&self, template: &str, row: Row<'_>, base: &dyn FormatContext) -> String {
        let (table, binding) = row;
        let flag = |b: bool| if b { "1" } else { "0" }.to_owned();
        let vars = Vars {
            vars: HashMap::from([
                ("notes_only", flag(self.notes_only)),
                ("key_has_repeat", flag(self.has_repeat)),
                ("key_string_width", self.key_width.to_string()),
                ("key_table_width", self.table_width.to_string()),
                ("key_repeat", flag(binding.repeat)),
                ("key_note", binding.note.clone().unwrap_or_default()),
                ("key_prefix", self.prefix.clone()),
                ("key_table", table.to_owned()),
                ("key_string", binding.key.name(false)),
                ("key_command", binding.commands.print(PrintFlags::LIST_KEYS)),
            ]),
            base,
        };
        format::expand_str(template, &vars)
    }
}

impl KeyBindings {
    /// `cmd_list_keys_exec` (cmd-list-keys.c:156-258) over parsed arguments.
    ///
    /// `prefix` is the `prefix` option (`Key::NONE` when unset), which
    /// `-N` output starts each line with unless `-P` is given. `base`
    /// supplies the other variables a custom `-F` can use (`format_defaults`);
    /// the default templates need none.
    ///
    /// A custom `-F` is expanded by [`crate::format`], which does not do the
    /// `p|` padding modifier yet, so a template using it expands to less
    /// than tmux's; the default templates are produced directly.
    pub fn list_keys(
        &self,
        args: &Args,
        prefix: Key,
        base: &dyn FormatContext,
    ) -> Result<ListKeys, CmdFailed> {
        let key_text = args.positional(0);
        let only = match key_text {
            None => None,
            Some(text) => Some(
                Key::from_name(text)
                    .ok_or_else(|| CmdFailed::say_text(format!("invalid key: {text}")))?
                    .key_and_modifiers(),
            ),
        };
        // `sort_order_from_string(NULL)` is SORT_END: no sort, and `-r` has
        // nothing to reverse.
        let order = match args.value('O') {
            None => None,
            Some(name) => Some(
                sort_order_from_string(name)
                    .ok_or_else(|| CmdFailed::say_text("invalid sort order"))?,
            ),
        };
        let table = match args.value('T') {
            None => None,
            Some(name) => Some((
                name,
                self.table(name)
                    .ok_or_else(|| CmdFailed::say_text(format!("table {name} doesn't exist")))?,
            )),
        };
        let prefix = match args.value('P') {
            Some(p) => p.to_owned(),
            None if prefix == Key::NONE => String::new(),
            None => prefix.name(false),
        };
        let single = args.flag('1');
        let notes_only = args.flag('N');

        let mut rows = self.rows(table, notes_only);
        sort_rows(&mut rows, order, args.flag('r'));
        let filter_notes = notes_only && !args.flag('a');
        rows.retain(|(_, b)| {
            only.is_none_or(|k| b.key.key_and_modifiers() == k)
                && (!filter_notes || b.note.is_some())
        });
        if only.is_some() && rows.is_empty() {
            return Err(CmdFailed::say_text(format!(
                "unknown key: {}",
                key_text.unwrap_or_default()
            )));
        }
        if single {
            rows.truncate(1);
        }

        let layout = Layout::new(&rows, notes_only, prefix);
        let lines = rows
            .into_iter()
            .map(|row| match args.value('F') {
                None => layout.default_line(row),
                Some(template) => layout.custom_line(template, row, base),
            })
            .collect();
        Ok(ListKeys { lines, single })
    }

    /// The bindings to consider, in table order: the one table, `prefix` then
    /// `root` for `-N` (`cmd_list_keys_get_root_and_prefix`), or all.
    fn rows<'a>(
        &'a self,
        table: Option<(&'a str, &'a KeyTable)>,
        notes_only: bool,
    ) -> Vec<Row<'a>> {
        let mut rows = Vec::new();
        if let Some((name, t)) = table {
            rows.extend(t.bindings().map(|b| (name, b)));
        } else if notes_only {
            for name in ["prefix", "root"] {
                if let Some(t) = self.table(name) {
                    rows.extend(t.bindings().map(|b| (name, b)));
                }
            }
        } else {
            for (name, t) in self.tables() {
                rows.extend(t.bindings().map(|b| (name, b)));
            }
        }
        rows
    }
}

/// `sort_qsort` (sort.c:37-60) for key bindings: `-O order` only reverses,
/// every other order sorts, and no `-O` leaves the order alone.
fn sort_rows(rows: &mut [Row<'_>], order: Option<SortOrder>, reversed: bool) {
    match order {
        None => {}
        Some(SortOrder::Stored) => {
            if reversed {
                rows.reverse();
            }
        }
        Some(order) => rows.sort_by(|a, b| {
            let o = compare(order, a, b);
            if reversed { o.reverse() } else { o }
        }),
    }
}
