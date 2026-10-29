//! Syntax-only parse of `-t` / `-s` target strings.
//!
//! tmux splits a target into session, window and pane parts and then
//! resolves them against live state (`cmd_find_target`, cmd-find.c:943-1244).
//! Only the splitting is pure, and it depends on what the command asked for:
//! a bare `a` is a session for `has-session` but a window for
//! `display-message` (cmd-find.c:1098-1128). So [`TargetSpec`] keeps the raw
//! text and [`TargetSpec::resolve`] applies the requested [`TargetKind`].
//! Looking anything up is the server's job. Client targets
//! (`detach-client -t`) are not `TargetSpec`s; they are plain strings
//! (cmd-find.c `cmd_find_client`).

/// What the command wants to find (`enum cmd_find_type`, tmux.h:1972-1974).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Pane,
    Window,
    Session,
}

/// One part of a target (`session`, `window` or `pane`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// The text with a leading `=` removed and `{last}`-style names mapped
    /// (cmd-find.c:48-76). It still starts with `$`, `@` or `%` for IDs.
    pub text: String,
    /// The part started with `=`: match names exactly, no prefix or fnmatch
    /// (`CMD_FIND_EXACT_SESSION` / `_WINDOW`, cmd-find.c:1130-1137). Never
    /// set for panes: tmux does not strip `=` from a pane part.
    pub exact: bool,
}

/// The parts of a target after splitting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetParts {
    pub session: Option<Part>,
    pub window: Option<Part>,
    pub pane: Option<Part>,
    /// A `:` was present, so the window part is not also tried as a session
    /// (`window_only`, cmd-find.c:1090).
    pub window_only: bool,
    /// A `.` was present, so the pane part is not also tried as a window
    /// (`pane_only`).
    pub pane_only: bool,
}

/// How a target string reads once the command's kind is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// Empty, or every part empty (`:`, `=:`): the current target.
    Current,
    /// `@`, `{active}`, `{current}`: the attached client's window
    /// (cmd-find.c:1012).
    ClientWindow,
    /// `=` or `{mouse}` (cmd-find.c:1027).
    Mouse,
    /// `~` or `{marked}` (cmd-find.c:1058).
    Marked,
    Parts(TargetParts),
}

/// A `-t` / `-s` value, syntax only. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSpec {
    raw: String,
}

/// cmd-find.c:48-76. Session names have no table.
const WINDOW_TABLE: [(&str, &str); 5] = [
    ("{start}", "^"),
    ("{last}", "!"),
    ("{end}", "$"),
    ("{next}", "+"),
    ("{previous}", "-"),
];
const PANE_TABLE: [(&str, &str); 15] = [
    ("{last}", "!"),
    ("{next}", "+"),
    ("{previous}", "-"),
    ("{top}", "top"),
    ("{bottom}", "bottom"),
    ("{left}", "left"),
    ("{right}", "right"),
    ("{top-left}", "top-left"),
    ("{top-right}", "top-right"),
    ("{bottom-left}", "bottom-left"),
    ("{bottom-right}", "bottom-right"),
    ("{up-of}", "{up-of}"),
    ("{down-of}", "{down-of}"),
    ("{left-of}", "{left-of}"),
    ("{right-of}", "{right-of}"),
];

/// `cmd_find_map_table` (cmd-find.c:249-260).
fn map_table(table: &[(&str, &str)], s: &str) -> String {
    table
        .iter()
        .find(|(from, _)| *from == s)
        .map_or(s, |(_, to)| to)
        .to_owned()
}

impl TargetSpec {
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        Self {
            raw: raw.to_owned(),
        }
    }

    /// The text exactly as given.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Split for a command that wants a `kind` (`cmd_find_target`,
    /// cmd-find.c:1011-1160).
    #[must_use]
    pub fn resolve(&self, kind: TargetKind) -> Resolved {
        let target = self.raw.as_str();
        if target.is_empty() {
            return Resolved::Current;
        }
        match target {
            "@" | "{active}" | "{current}" => return Resolved::ClientWindow,
            "=" | "{mouse}" => return Resolved::Mouse,
            "~" | "{marked}" => return Resolved::Marked,
            _ => {}
        }

        // Separators (cmd-find.c:1069-1080): the first ':' splits off the
        // session; the first '.' after it (or in the whole text when there
        // is no ':') splits off the pane.
        let (head, colon_rest) = match target.split_once(':') {
            Some((h, r)) => (h, Some(r)),
            None => (target, None),
        };
        let (window_text, period_rest) = match colon_rest {
            Some(rest) => split_period(rest),
            None => split_period(head),
        };
        let (mut session, mut window, mut pane, mut window_only, mut pane_only) =
            (None, None, None, false, false);
        match (colon_rest.is_some(), period_rest) {
            (true, Some(p)) => {
                session = Some(head);
                window = Some(window_text);
                window_only = true;
                pane = Some(p);
                pane_only = true;
            }
            (true, None) => {
                session = Some(head);
                window = Some(window_text);
                window_only = true;
            }
            (false, Some(p)) => {
                window = Some(window_text);
                pane = Some(p);
                pane_only = true;
            }
            (false, None) => {
                // No separators: a sigil decides, else the command's kind
                // (cmd-find.c:1098-1128).
                let copy = window_text;
                if copy.starts_with('$') {
                    session = Some(copy);
                } else if copy.starts_with('@') {
                    window = Some(copy);
                } else if copy.starts_with('%') {
                    pane = Some(copy);
                } else {
                    match kind {
                        TargetKind::Session => session = Some(copy),
                        TargetKind::Window => window = Some(copy),
                        TargetKind::Pane => pane = Some(copy),
                    }
                }
            }
        }

        // '=' exact flags, then "empty is the same as NULL", then the
        // conversion tables (cmd-find.c:1130-1143).
        let session = strip_exact(session).and_then(|(s, exact)| non_empty(s, exact, &[]));
        let window = strip_exact(window).and_then(|(s, exact)| non_empty(s, exact, &WINDOW_TABLE));
        let pane = pane.filter(|p| !p.is_empty()).map(|p| Part {
            text: map_table(&PANE_TABLE, p),
            exact: false,
        });
        if session.is_none() && window.is_none() && pane.is_none() {
            return Resolved::Current;
        }
        Resolved::Parts(TargetParts {
            session,
            window,
            pane,
            window_only,
            pane_only,
        })
    }
}

fn split_period(s: &str) -> (&str, Option<&str>) {
    match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    }
}

fn strip_exact(part: Option<&str>) -> Option<(&str, bool)> {
    part.map(|p| match p.strip_prefix('=') {
        Some(rest) => (rest, true),
        None => (p, false),
    })
}

fn non_empty(text: &str, exact: bool, table: &[(&str, &str)]) -> Option<Part> {
    (!text.is_empty()).then(|| Part {
        text: map_table(table, text),
        exact,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unnecessary_wraps)] // compared against Option fields
    fn part(text: &str, exact: bool) -> Option<Part> {
        Some(Part {
            text: text.to_owned(),
            exact,
        })
    }

    fn parts(raw: &str, kind: TargetKind) -> TargetParts {
        match TargetSpec::parse(raw).resolve(kind) {
            Resolved::Parts(p) => p,
            other => panic!("{raw:?} resolved to {other:?}"),
        }
    }

    use TargetKind::{Pane, Session, Window};

    /// Source: cmd-find.c:1098-1128. Reference: after `new -d -s a`,
    /// `has -t a` succeeds (a session lookup) while `display -p -t a x` does
    /// not find session a: a bare name is a pane for it (cmd-find.c
    /// `cmd_find_get_pane` then tries a window, then a session).
    #[test]
    fn bare_name_follows_the_requested_kind() {
        let s = parts("a", Session);
        assert_eq!(
            (s.session, s.window, s.pane),
            (part("a", false), None, None)
        );
        let w = parts("a", Window);
        assert_eq!(
            (w.session, w.window, w.pane),
            (None, part("a", false), None)
        );
        let p = parts("a", Pane);
        assert_eq!(
            (p.session, p.window, p.pane),
            (None, None, part("a", false))
        );
        assert!(!p.window_only && !p.pane_only);
        assert_eq!(TargetSpec::parse("a").raw(), "a");
    }

    #[test]
    fn equals_marks_exact_for_session_and_window_only() {
        // cmd-find.c:1130-1137: '=' is stripped from session and window
        // parts; a pane part keeps it.
        assert_eq!(parts("=a", Session).session, part("a", true));
        assert_eq!(parts("=a", Window).window, part("a", true));
        assert_eq!(parts("=a", Pane).pane, part("=a", false));
        let both = parts("=a:=b", Pane);
        assert_eq!(both.session, part("a", true));
        assert_eq!(both.window, part("b", true));
    }

    #[test]
    fn session_window_pane_split() {
        // `a:1.2` -> session a, window 1, pane 2 (cmd-find.c:1069-1090).
        let p = parts("a:1.2", Pane);
        assert_eq!(p.session, part("a", false));
        assert_eq!(p.window, part("1", false));
        assert_eq!(p.pane, part("2", false));
        assert!(p.window_only && p.pane_only);
        // `a:1`: window_only, no pane.
        let p = parts("a:1", Pane);
        assert!(p.window_only && !p.pane_only && p.pane.is_none());
        // `1.2` with no colon: window 1 pane 2, no session.
        let p = parts("1.2", Session);
        assert_eq!(
            (p.session, p.window, p.pane),
            (None, part("1", false), part("2", false))
        );
        // The period is searched after the colon only: `a.b:1` has no pane.
        let p = parts("a.b:1", Pane);
        assert_eq!(p.session, part("a.b", false));
        assert_eq!(p.window, part("1", false));
        assert!(p.pane.is_none());
    }

    #[test]
    fn id_sigils_pick_their_own_part_whatever_the_kind() {
        // cmd-find.c:1099-1105.
        for kind in [Session, Window, Pane] {
            assert_eq!(parts("$0", kind).session, part("$0", false));
            assert_eq!(parts("@1", kind).window, part("@1", false));
            assert_eq!(parts("%2", kind).pane, part("%2", false));
        }
    }

    /// Reference: `:.+` is a pane `+` (window and session part empty):
    /// cmd-find.c:1086-1092 sets window "" and pane "+", then 1139-1143
    /// turn empty into NULL.
    #[test]
    fn empty_parts_are_absent() {
        let p = parts(":.+", Pane);
        assert_eq!((p.session, p.window), (None, None));
        assert_eq!(p.pane, part("+", false));
        assert!(p.window_only && p.pane_only);
        let p = parts("a:", Window);
        assert_eq!(p.session, part("a", false));
        assert_eq!(p.window, None);
    }

    #[test]
    fn all_empty_is_the_current_target() {
        // cmd-find.c:1009-1010 and the `current:` label at 1224.
        for raw in ["", ":", "=:", ".", ":."] {
            assert_eq!(
                TargetSpec::parse(raw).resolve(Pane),
                Resolved::Current,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn named_positions_map_through_the_tables() {
        // cmd-find.c:51-76.
        assert_eq!(parts("{last}", Window).window, part("!", false));
        assert_eq!(parts("{last}", Pane).pane, part("!", false));
        assert_eq!(parts("a:{end}", Window).window, part("$", false));
        assert_eq!(parts("{top-left}", Pane).pane, part("top-left", false));
        assert_eq!(parts("{up-of}", Pane).pane, part("{up-of}", false));
        // Sessions have no table.
        assert_eq!(parts("{last}", Session).session, part("{last}", false));
    }

    #[test]
    fn special_targets() {
        let r = |s: &str| TargetSpec::parse(s).resolve(Pane);
        for s in ["@", "{active}", "{current}"] {
            assert_eq!(r(s), Resolved::ClientWindow);
        }
        for s in ["=", "{mouse}"] {
            assert_eq!(r(s), Resolved::Mouse);
        }
        for s in ["~", "{marked}"] {
            assert_eq!(r(s), Resolved::Marked);
        }
        // Only the whole string is special.
        assert!(matches!(r("@x"), Resolved::Parts(_)));
    }
}
