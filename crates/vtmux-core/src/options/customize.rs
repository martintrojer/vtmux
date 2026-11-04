//! What `customize-mode` (window-customize.c) does to options outside
//! `set-option`: set one value from a prompt, unset or reset one, add a
//! user option, rename an array item, and the questions it asks while
//! drawing (is this changed from the default, what is the default).

use super::table::{ArrayKey, Kind, OptionDef};
use super::value::{self, OptionValue, Value};
use super::{Entry, OptionError, OptionHost, OptionsStore, TreeId};

impl OptionsStore {
    /// `options_remove_or_default` on the entry `name` holds in `tree`:
    /// one array item, or the whole option (back to its default in a
    /// global tree).
    pub fn unset(&mut self, tree: TreeId, name: &str, key: Option<&ArrayKey>) {
        self.remove_or_default(tree, name, key);
    }

    /// `window_customize_reset_option`: the option is unset here and in
    /// every tree above.
    pub fn reset(&mut self, tree: TreeId, name: &str) {
        let mut at = Some(tree);
        while let Some(t) = at {
            if self.tree(t).get_only(name).is_some() {
                self.remove_or_default(t, name, None);
            }
            at = self.parent(t);
        }
    }

    /// `window_customize_set_option_value` and its prompt callback: `text`
    /// set on the option `name` holds in `tree` (found by `options_get`,
    /// so an inherited entry is the one changed), one array item when it
    /// is an array (`key`, or the first free index).
    ///
    /// # Errors
    /// The value does not parse; nothing changed then.
    pub fn set_from_prompt(
        &mut self,
        tree: TreeId,
        name: &str,
        key: Option<&ArrayKey>,
        text: &str,
        host: &mut dyn OptionHost,
    ) -> Result<(), OptionError> {
        let Some(holder) = self.holder(tree, name) else {
            return Err(OptionError::InvalidOption(name.to_owned()));
        };
        let def = self.tree(holder).get_only(name).and_then(|e| e.def);
        let Some(def) = def else {
            self.set_string(tree, name, None, text, false);
            return Ok(());
        };
        if def.array {
            let key = key.cloned().unwrap_or_else(|| {
                let items = self.array_items(holder, def.name);
                ArrayKey::Index(
                    (0..=u32::MAX)
                        .find(|i| !items.is_some_and(|m| m.contains_key(&ArrayKey::Index(*i))))
                        .unwrap_or(u32::MAX),
                )
            });
            return self.set_array(holder, def, Some(key), text, false, host);
        }
        self.set_from_string(tree, def, Some(text), false, host)
    }

    fn array_items(
        &self,
        tree: TreeId,
        name: &str,
    ) -> Option<&std::collections::BTreeMap<ArrayKey, Value>> {
        match &self.tree(tree).get_only(name)?.value {
            OptionValue::Array(items) => Some(items),
            OptionValue::Single(_) => None,
        }
    }

    /// `options_set_number` for a flag or choice: `!flag`, or the next
    /// choice (wrapping), in `tree`.
    pub fn toggle(&mut self, tree: TreeId, def: &'static OptionDef) {
        let current = match self.get(tree, def.name).map(|e| &e.value) {
            Some(OptionValue::Single(v)) => Some(v.clone()),
            _ => None,
        };
        let new = match (def.kind, current) {
            (Kind::Flag, Some(Value::Flag(on))) => Value::Flag(!on),
            (Kind::Flag, _) => Value::Flag(true),
            (Kind::Choice(names), Some(Value::Choice(n))) => {
                Value::Choice(if n + 1 >= names.len() { 0 } else { n + 1 })
            }
            (Kind::Choice(_), _) => Value::Choice(0),
            _ => return,
        };
        self.tree_mut(tree).insert(
            def.name,
            Entry {
                def: Some(def),
                value: OptionValue::Single(new),
            },
        );
    }

    /// `options_set_string` for a new user option, optionally a hook
    /// event (`hooks_add_event`).
    pub fn add_user(&mut self, tree: TreeId, name: &str, value: &str, hook: bool) {
        self.set_string(tree, name, None, value, false);
        if hook {
            self.hook_events.insert(name.to_owned());
        }
    }

    /// `set-hook` on a user option name (`hooks_add_event`).
    pub fn add_hook_event(&mut self, name: &str) {
        self.hook_events.insert(name.to_owned());
    }

    /// `window_customize_set_array_key_callback`: the item at `from` moves
    /// to `to`. Nothing when `to` is taken or `from` is gone; `Ok(false)`
    /// when nothing moved.
    ///
    /// # Errors
    /// The item's value does not parse at its new key.
    pub fn rename_array_key(
        &mut self,
        tree: TreeId,
        name: &str,
        from: &ArrayKey,
        to: &str,
        host: &mut dyn OptionHost,
    ) -> Result<bool, OptionError> {
        let Some(holder) = self.holder(tree, name) else {
            return Ok(false);
        };
        let Some(def) = self.tree(holder).get_only(name).and_then(|e| e.def) else {
            return Ok(false);
        };
        let to = ArrayKey::parse(to).ok_or_else(|| OptionError::BadKey(to.to_owned()))?;
        let Some(items) = self.array_items(holder, def.name) else {
            return Ok(false);
        };
        if items.contains_key(&to) {
            return Ok(false);
        }
        let text = items
            .get(from)
            .map(|v| v.to_text(Some(def), false))
            .unwrap_or_default();
        self.set_array(holder, def, Some(to), &text, false, host)?;
        self.remove_or_default(holder, def.name, Some(from));
        Ok(true)
    }
}

/// `options_default_to_string` (options.c:359): the table default as
/// text. An array's default is its whole default string.
#[must_use]
pub fn default_text(def: &OptionDef) -> String {
    use super::table::DefaultValue as D;
    match def.default {
        D::Str(s) => s.to_owned(),
        D::List(list) => list.join(" "),
        D::Number(n) => n.to_string(),
        D::Key(k) => k.name(false),
        D::Colour(c) => c.to_string(),
        D::Flag(b) => if b { "on" } else { "off" }.to_owned(),
        D::Choice(n) => match def.kind {
            Kind::Choice(names) => names.get(n).copied().unwrap_or_default().to_owned(),
            _ => n.to_string(),
        },
    }
}

/// `window_customize_option_is_changed` (window-customize.c:567-611) for
/// a table option's entry: its value (or one item's) differs from the
/// default.
#[must_use]
pub fn is_changed(entry: &Entry, key: Option<&ArrayKey>) -> bool {
    let Some(def) = entry.def else {
        return true;
    };
    if def.array {
        let defaults = value::default_value(def, "");
        if let Some(k) = key {
            let (OptionValue::Array(now), OptionValue::Array(was)) = (&entry.value, &defaults)
            else {
                return true;
            };
            match (now.get(k), was.get(k)) {
                (Some(a), Some(b)) => a.to_text(Some(def), false) != b.to_text(Some(def), false),
                (a, b) => a.is_some() != b.is_some(),
            }
        } else {
            entry.value.to_text(Some(def), None, false) != defaults.to_text(Some(def), None, false)
        }
    } else {
        entry.value.to_text(Some(def), None, false) != default_text(def)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{ConfigEnv, NoEnv};
    use crate::ids::{PaneId, SessionId, WindowId};
    use crate::options::{Defaults, find};

    struct Host(NoEnv);

    impl OptionHost for Host {
        fn expand(&mut self, _: &OptionsStore, template: &str, _: &[(&str, String)]) -> String {
            template.to_owned()
        }
        fn config_env(&mut self) -> &mut dyn ConfigEnv {
            &mut self.0
        }
        fn usable_shell(&self, _: &str) -> bool {
            true
        }
    }

    fn text(store: &OptionsStore, tree: TreeId, name: &str) -> Option<String> {
        store
            .tree(tree)
            .get_only(name)
            .map(|e| e.value.to_text(e.def, None, false))
    }

    /// `window_customize_reset_option`: unset here and in every tree above,
    /// the global one back to its default.
    #[test]
    fn reset_clears_every_level() {
        let mut s = OptionsStore::new(&Defaults::default());
        let (w, p) = (WindowId(1), PaneId(2));
        s.add_window(w);
        s.add_pane(p, w);
        let mut h = Host(NoEnv);
        for t in [TreeId::GlobalWindow, TreeId::Window(w), TreeId::Pane(p)] {
            s.set_from_prompt(t, "remain-on-exit-format", None, "x", &mut h)
                .unwrap();
        }
        s.reset(TreeId::Pane(p), "remain-on-exit-format");
        assert_eq!(text(&s, TreeId::Pane(p), "remain-on-exit-format"), None);
        assert_eq!(text(&s, TreeId::Window(w), "remain-on-exit-format"), None);
        let def = find("remain-on-exit-format").unwrap();
        assert_eq!(
            text(&s, TreeId::GlobalWindow, "remain-on-exit-format"),
            Some(default_text(def))
        );
    }

    /// The prompt sets the tree that holds the option: an inherited value
    /// is changed where it lives; a bad value changes nothing.
    #[test]
    fn prompt_sets_the_holder() {
        let mut s = OptionsStore::new(&Defaults::default());
        let sess = SessionId(1);
        s.add_session(sess);
        let mut h = Host(NoEnv);
        s.set_from_prompt(TreeId::Session(sess), "repeat-time", None, "42", &mut h)
            .unwrap();
        assert_eq!(
            text(&s, TreeId::Session(sess), "repeat-time").as_deref(),
            Some("42")
        );
        assert!(
            s.set_from_prompt(TreeId::Session(sess), "repeat-time", None, "zz", &mut h)
                .is_err()
        );
        assert_eq!(
            text(&s, TreeId::Session(sess), "repeat-time").as_deref(),
            Some("42")
        );
    }

    /// An array without a key gets the first free index; a key renamed
    /// moves the item, unless the new key is taken.
    #[test]
    fn array_items() {
        let mut s = OptionsStore::new(&Defaults::default());
        let mut h = Host(NoEnv);
        let k = |n| ArrayKey::Index(n);
        s.set_from_prompt(TreeId::Server, "user-keys", Some(&k(3)), "a", &mut h)
            .unwrap();
        s.set_from_prompt(TreeId::Server, "user-keys", None, "b", &mut h)
            .unwrap();
        let shown = |s: &OptionsStore, key| {
            s.tree(TreeId::Server)
                .get_only("user-keys")
                .map(|e| e.value.to_text(e.def, Some(&key), false))
        };
        assert_eq!(shown(&s, k(0)).as_deref(), Some("b"));
        assert!(
            s.rename_array_key(TreeId::Server, "user-keys", &k(3), "7", &mut h)
                .unwrap()
        );
        assert_eq!(shown(&s, k(3)).as_deref(), Some(""));
        assert_eq!(shown(&s, k(7)).as_deref(), Some("a"));
        assert!(
            !s.rename_array_key(TreeId::Server, "user-keys", &k(7), "0", &mut h)
                .unwrap()
        );
    }

    /// Flags flip and choices step, wrapping.
    #[test]
    fn toggles() {
        let mut s = OptionsStore::new(&Defaults::default());
        let flag = find("exit-empty").unwrap();
        s.toggle(TreeId::Server, flag);
        assert_eq!(
            text(&s, TreeId::Server, "exit-empty").as_deref(),
            Some("off")
        );
        let choice = find("set-clipboard").unwrap();
        for want in ["on", "off", "external"] {
            s.toggle(TreeId::Server, choice);
            assert_eq!(
                text(&s, TreeId::Server, "set-clipboard").as_deref(),
                Some(want)
            );
        }
    }

    /// `window_customize_option_is_changed` against the table default.
    #[test]
    fn changed() {
        let s = OptionsStore::new(&Defaults::default());
        let e = s.tree(TreeId::Server).get_only("buffer-limit").unwrap();
        assert!(!is_changed(e, None));
        let e = s
            .tree(TreeId::GlobalSession)
            .get_only("status-format")
            .unwrap();
        assert!(!is_changed(e, None));
        assert!(!is_changed(e, Some(&ArrayKey::Index(1))));
    }
}
