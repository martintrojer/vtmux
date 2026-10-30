//! Keys, key tables and the key-binding commands.
//!
//! - [`Key`]: tmux's numeric key code, bit for bit (tmux.h:145-430), and
//!   both string conversions, `key_string_lookup_string` and
//!   `key_string_lookup_key` (key-string.c). Nothing is normalised beyond
//!   what those functions do: `C-i` and `Tab` are different keys, `C-A` and
//!   `C-a` are different keys, `0x41` and `A` are different keys.
//! - [`KeyBindings`]: the tables (key-bindings.c), ordered as tmux orders
//!   them (tables by name, bindings by numeric key code), with the 3.8
//!   defaults, and `bind-key` / `unbind-key` as functions over parsed
//!   arguments (cmd-bind-key.c, cmd-unbind-key.c).
//! - [`KeyBindings::list_keys`]: `list-keys` (cmd-list-keys.c), the lines it
//!   prints.
//!
//! The defaults are the strings tmux itself parses at start-up
//! (`key_bindings_init`), generated into `keys/defaults.in` by
//! `keys/gen_defaults.sh` and run through this crate's own command parser, so
//! parsing and printing are exercised by the 308 default bindings.
//!
//! Known divergences:
//!
//! - After `unbind -a` removes the `prefix` table, tmux 3.8 crashes in
//!   `list-keys -N` (`cmd_list_keys_get_root_and_prefix` passes the missing
//!   table to `sort_get_key_bindings_table`). [`KeyBindings::list_keys`]
//!   lists what exists instead.
//!
//! The rest are in how a key *prints*:
//!
//! - A Unicode key's code carries the character's display width, which
//!   decides its place in a table. Widths come from the default tables
//!   (`keys/widths.in`); the `codepoint-widths` option is not applied here.
//! - A four-byte character's code carries an index tmux hands out in the
//!   order it first meets each character (`utf8_put_item`), which in a
//!   running server depends on what panes have printed. Here the index is
//!   the code point, so four-byte keys order by code point.
//! - `0x110000`-`0x13ffff` are keys in tmux (it encodes them as four bytes,
//!   which are not UTF-8). [`Key::from_name`] accepts them too, but their
//!   name prints as U+FFFD, not tmux's raw bytes.

mod bindings;
mod code;
mod defaults;
mod list;
mod string;
mod width;

pub use bindings::{Binding, CmdFailed, KeyBindings, KeyTable};
pub use code::Key;
pub use list::{ListKeys, display_width};

#[cfg(test)]
mod golden;
