//! vtmux's own options (paper cuts, spec *Paper-cut fixes*; protocol
//! extensions, spec *Protocol extensions (D28)*), kept out of
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

use super::table::{DefaultValue, Kind, OptionDef, Scope, StringCheck};
use super::{Entry, value};

/// Every vtmux option, in name order (the order listings show them).
pub(super) static EXTENSIONS: [OptionDef; 6] = [
    // D29: the kitty keyboard protocol, pane side (spec, *Protocol
    // extensions (D28)*).
    OptionDef {
        name: "vtmux-kitty-keys",
        scope: Scope::Server,
        kind: Kind::Flag,
        array: false,
        hook: false,
        separator: None,
        default: DefaultValue::Flag(false),
        text: Some("Grant the kitty keyboard protocol to programs that ask for it."),
        unit: None,
    },
    // D30: desktop notifications (OSC 9, OSC 777) to Ghostty clients.
    OptionDef {
        name: "vtmux-notifications",
        scope: Scope::Server,
        kind: Kind::Flag,
        array: false,
        hook: false,
        separator: None,
        default: DefaultValue::Flag(false),
        text: Some("Send panes' desktop notifications (OSC 9, OSC 777) to Ghostty clients."),
        unit: None,
    },
    // D26: the pane blend.
    OptionDef {
        name: "vtmux-pane-active-blend",
        scope: Scope::Window,
        kind: Kind::String(StringCheck::Blend),
        array: false,
        hook: false,
        separator: None,
        default: DefaultValue::Str(""),
        text: Some("Colours every cell of the active pane is blended toward."),
        unit: None,
    },
    OptionDef {
        name: "vtmux-pane-inactive-blend",
        scope: Scope::Window,
        kind: Kind::String(StringCheck::Blend),
        array: false,
        hook: false,
        separator: None,
        default: DefaultValue::Str(""),
        text: Some("Colours every cell of the other panes is blended toward."),
        unit: None,
    },
    // D27: checkpoints on disk (docs/specs/2026-11-06-live-upgrade.md,
    // decision 17).
    OptionDef {
        name: "vtmux-persist",
        scope: Scope::Server,
        kind: Kind::Choice(&["off", "layout", "full"]),
        array: false,
        hook: false,
        separator: None,
        default: DefaultValue::Choice(0),
        text: Some("Checkpoint the server's state to disk: layout only, or with screens."),
        unit: None,
    },
    // D27: user options a restore leaves out (decision 30), globs as
    // `update-environment` lists names.
    OptionDef {
        name: "vtmux-persist-drop",
        scope: Scope::Session,
        kind: Kind::String(StringCheck::None),
        array: true,
        hook: false,
        separator: None,
        default: DefaultValue::Str(""),
        text: Some("User options (globs) not restored from a checkpoint."),
        unit: None,
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

#[cfg(test)]
mod tests {
    use super::EXTENSIONS;

    /// `docs/PAPERCUTS.md`, *vtmux options*, has a row for every vtmux
    /// option, and for none that does not exist.
    #[test]
    fn the_papercuts_table_lists_every_vtmux_option() {
        let doc = include_str!("../../../../docs/PAPERCUTS.md");
        let table = doc
            .split("## vtmux options")
            .nth(1)
            .and_then(|rest| rest.split("\n## ").next())
            .expect("the vtmux options section");
        let listed: Vec<&str> = table
            .lines()
            .filter_map(|l| l.strip_prefix("| `vtmux-"))
            .filter_map(|l| l.split_once('`'))
            .map(|(name, _)| name)
            .collect();
        let mut want: Vec<&str> = EXTENSIONS
            .iter()
            .map(|d| d.name.strip_prefix("vtmux-").unwrap())
            .collect();
        let mut got = listed;
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want);
    }
}
