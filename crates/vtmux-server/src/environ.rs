//! Environments: the server's global environment and per-session ones
//! (environ.c). A session entry may be a removal (`environ_clear`, value
//! `None`), which hides a global variable from that session's panes.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub value: Option<String>,
    /// `%hidden` / `setenv -h`: not passed to processes.
    pub hidden: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Environ(BTreeMap<String, Entry>);

impl Environ {
    /// From `NAME=value` pairs, as `environ_put` for each.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut env = Self::default();
        for (k, v) in pairs {
            env.set(&k, &v, false);
        }
        env
    }

    pub fn set(&mut self, name: &str, value: &str, hidden: bool) {
        self.0.insert(
            name.to_owned(),
            Entry {
                value: Some(value.to_owned()),
                hidden,
            },
        );
    }

    /// `environ_clear`: present but removed.
    pub fn clear(&mut self, name: &str) {
        self.0.insert(
            name.to_owned(),
            Entry {
                value: None,
                hidden: false,
            },
        );
    }

    /// `environ_unset`: remove the entry outright.
    pub fn unset(&mut self, name: &str) {
        self.0.remove(name);
    }

    /// `environ_first` / `environ_next`: entries in name (`strcmp`) order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.0.iter().map(|(k, e)| (k.as_str(), e))
    }

    /// `environ_find`: the entry, removals included.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.0.get(name)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name)?.value.as_deref()
    }

    /// `environ_copy(self, dst)`: values overwrite, removals remove.
    pub fn copy_into(&self, dst: &mut Self) {
        for (k, e) in &self.0 {
            match &e.value {
                Some(_) => {
                    dst.0.insert(k.clone(), e.clone());
                }
                None => dst.clear(k),
            }
        }
    }

    /// `environ_push`: what a child process gets (no removals, no hidden,
    /// no empty names).
    #[must_use]
    pub fn for_process(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .filter(|(k, e)| !k.is_empty() && !e.hidden)
            .filter_map(|(k, e)| e.value.clone().map(|v| (k.clone(), v)))
            .collect()
    }
}

/// `environ_update` (environ.c:186-212): for each `update-environment`
/// pattern, copy every matching variable of `src` into `dst`; a pattern
/// that matches nothing marks its own text removed in `dst`.
pub fn update(patterns: &[String], src: &Environ, dst: &mut Environ) {
    for pattern in patterns {
        let mut found = false;
        for (name, entry) in &src.0 {
            // environ_set with the source value; a removed source entry has
            // none, and tmux would format a NULL string.
            let Some(value) = &entry.value else { continue };
            if vtmux_core::fnmatch::matches(pattern, name, false) {
                dst.set(name, value, false);
                found = true;
            }
        }
        if !found {
            dst.clear(pattern);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_copies_matches_and_clears_misses() {
        let src = Environ::from_pairs([
            ("MYVAR".into(), "c".into()),
            ("TEST_A".into(), "a".into()),
            ("TEST_B".into(), "b".into()),
            ("OTHER".into(), "o".into()),
        ]);
        let mut dst = Environ::default();
        dst.set("ABSENT", "old", false);
        let pats: Vec<String> = ["MYVAR", "ABSENT", "TEST_*"].map(Into::into).into();
        update(&pats, &src, &mut dst);
        assert_eq!(dst.get("MYVAR"), Some("c"));
        assert_eq!(dst.get("TEST_A"), Some("a"));
        assert_eq!(dst.get("TEST_B"), Some("b"));
        assert_eq!(dst.find("ABSENT").unwrap().value, None);
        assert!(dst.find("OTHER").is_none());
    }

    #[test]
    fn removals_hide_globals() {
        let mut global = Environ::from_pairs([("A".into(), "1".into()), ("B".into(), "2".into())]);
        global.set("H", "x", true);
        let mut s = Environ::default();
        s.clear("A");
        s.set("C", "3", false);
        s.copy_into(&mut global);
        assert_eq!(
            global.for_process(),
            vec![("B".into(), "2".into()), ("C".into(), "3".into())]
        );
        assert_eq!(global.get("H"), Some("x"));
    }
}
