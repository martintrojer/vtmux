//! vtmux's own options (paper cuts, spec *Paper-cut fixes*), kept out of
//! the generated tmux table so nothing tmux observes changes while they are
//! off:
//!
//! - they are matched by exact name only, never as a prefix, so `set -g vt
//!   on` and `show -g vtm` keep tmux's `invalid option`;
//! - their defaults are not stored in the global trees: a tree holds one
//!   only once it was set there, and listings show it only then
//!   ([`super::OptionsStore::get`] falls back to the default for lookups);
//! - `TABLE`, its 272 rows and the goldens are tmux's alone.

use std::sync::LazyLock;

use super::table::{DefaultValue, Kind, OptionDef, Scope};
use super::{Entry, value};

/// Every vtmux option, in name order (the order listings show them).
pub(super) static EXTENSIONS: [OptionDef; 1] = [
    // D23: the focus ring.
    OptionDef {
        name: "vtmux-focus-ring",
        scope: Scope::Window,
        kind: Kind::Flag,
        array: false,
        hook: false,
        separator: None,
        default: DefaultValue::Flag(false),
    },
];

static DEFAULTS: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    EXTENSIONS
        .iter()
        .map(|def| Entry {
            def: Some(def),
            value: value::default_value(def, ""),
        })
        .collect()
});

/// The vtmux option named exactly `name`.
pub(super) fn find(name: &str) -> Option<&'static OptionDef> {
    EXTENSIONS.iter().find(|d| d.name == name)
}

/// Whether `def` is one of vtmux's options rather than tmux's.
pub(super) fn is_extension(def: &OptionDef) -> bool {
    EXTENSIONS.iter().any(|d| std::ptr::eq(d, def))
}

/// The default of the vtmux option `name`, as an entry no tree holds.
pub(super) fn default_entry(name: &str) -> Option<&'static Entry> {
    DEFAULTS
        .iter()
        .find(|e| e.def.is_some_and(|d| d.name == name))
}
