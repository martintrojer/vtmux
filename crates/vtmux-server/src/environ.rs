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

/// The default `update-environment` (options-table.c:1210-1217).
pub const UPDATE_ENVIRONMENT: [&str; 12] = [
    "DISPLAY",
    "KRB5CCNAME",
    "MSYSTEM",
    "SSH_ASKPASS",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_CONNECTION",
    "WAYLAND_DISPLAY",
    "WINDOWID",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
];

/// `environ_update` (environ.c:186-212) with the default list: copy each
/// listed variable from the client, or mark it removed.
#[must_use]
pub fn session_environ(client: &[(String, String)]) -> Environ {
    let mut env = Environ::default();
    for name in UPDATE_ENVIRONMENT.iter().chain(&["XDG_SESSION_TYPE"]) {
        match client.iter().find(|(k, _)| k == name) {
            Some((_, v)) => env.set(name, v, false),
            None => env.clear(name),
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

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
