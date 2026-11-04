//! Monitor hooks (`set-hook -B name:what:format`, hooks.c
//! `hooks_monitor_*`, monitor.c `monitor_parse`): what each option entry
//! watches. The store keeps what `show-hooks -B` prints and the monitor's
//! firing count; the server owns the timer and the last values it compares
//! against, and finds a replaced monitor by its [`Monitor::serial`].

use std::fmt;

use crate::ids::SessionId;

/// What a monitor watches (`enum monitor_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorWhat {
    Session,
    Pane(u32),
    AllPanes,
    Window(u32),
    AllWindows,
}

impl fmt::Display for MonitorWhat {
    /// `hook_monitor_target` (cmd-show-options.c:287-303): what `what`
    /// was written as, the number normalised.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session => Ok(()),
            Self::Pane(n) => write!(f, "%{n}"),
            Self::AllPanes => f.write_str("%*"),
            Self::Window(n) => write!(f, "@{n}"),
            Self::AllWindows => f.write_str("@*"),
        }
    }
}

/// `monitor_parse` (monitor.c:585-626): `name:what:format`, `what` one of
/// empty, `%N`, `%*`, `@N`, `@*` (`N` as `sscanf("%d")` reads it, not
/// negative). The format is everything after the second colon.
#[must_use]
pub fn parse_monitor(value: &str) -> Option<(&str, MonitorWhat, &str)> {
    let (name, rest) = value.split_once(':')?;
    let (what, format) = rest.split_once(':')?;
    let id = |s: &str| {
        let s = s.trim_start();
        let end = s
            .char_indices()
            .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '+' || c == '-'))))
            .map_or(s.len(), |(i, _)| i);
        s[..end]
            .parse::<i64>()
            .ok()
            .and_then(|n| u32::try_from(n).ok())
    };
    let what = match what {
        "%*" => MonitorWhat::AllPanes,
        "@*" => MonitorWhat::AllWindows,
        "" => MonitorWhat::Session,
        w if w.starts_with('%') => MonitorWhat::Pane(id(&w[1..])?),
        w if w.starts_with('@') => MonitorWhat::Window(id(&w[1..])?),
        _ => return None,
    };
    Some((name, what, format))
}

/// One option entry's monitor (`struct hooks_monitor` and its
/// `monitor_item`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub what: MonitorWhat,
    pub format: String,
    /// `-T` (`MONITOR_NOTIFY_TRUE`): only changes to a true value fire.
    pub notify_true: bool,
    /// The session it watches (`monitor_create_session`): the target's,
    /// or `None` in a global tree, where it is whichever session sorts
    /// first when it looks.
    pub session: Option<SessionId>,
    /// Which monitor this is: a new one for every `set-hook -B`.
    pub serial: u64,
    /// `fire_count` and `fire_time` (epoch seconds).
    pub fires: (u32, i64),
}

impl Monitor {
    /// `hooks_monitor_to_string`: `name:what:format`.
    #[must_use]
    pub fn describe(&self, name: &str) -> String {
        format!("{name}:{}:{}", self.what, self.format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        assert_eq!(
            parse_monitor("sw::#{x}"),
            Some(("sw", MonitorWhat::Session, "#{x}"))
        );
        assert_eq!(
            parse_monitor("a:%3:f:g"),
            Some(("a", MonitorWhat::Pane(3), "f:g"))
        );
        assert_eq!(
            parse_monitor("a:%*:f"),
            Some(("a", MonitorWhat::AllPanes, "f"))
        );
        assert_eq!(
            parse_monitor("a:@2x:f"),
            Some(("a", MonitorWhat::Window(2), "f"))
        );
        assert_eq!(
            parse_monitor("a:@ 4:f"),
            Some(("a", MonitorWhat::Window(4), "f"))
        );
        assert_eq!(
            parse_monitor("a:@*:f"),
            Some(("a", MonitorWhat::AllWindows, "f"))
        );
        assert_eq!(parse_monitor("a:%-1:f"), None);
        assert_eq!(parse_monitor("a:x:f"), None);
        assert_eq!(parse_monitor("sw"), None);
        assert_eq!(parse_monitor("sw:"), None);
    }

    #[test]
    fn describe_normalises_the_id() {
        let m = Monitor {
            what: MonitorWhat::Pane(3),
            format: "#{pane_id}".into(),
            notify_true: false,
            session: None,
            serial: 1,
            fires: (0, 0),
        };
        assert_eq!(m.describe("@m"), "@m:%3:#{pane_id}");
    }
}
