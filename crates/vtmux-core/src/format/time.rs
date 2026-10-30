//! The time modifiers of `format_find` (format.c:4515-4710): `t` (ctime), `t/p` (pretty),
//! `t/r` (relative), `t/d` (difference in seconds) and `t/f/<fmt>` (strftime), and the
//! strftime pass of `format_expand1` (format.c:6728-6741) that `T` and the time-aware
//! expansion of `display-message` run over text.

use super::{Eval, Format, num};
use crate::time::Tm;

/// One `t` modifier as written: its flags argument and its second argument (the
/// strftime format of `t/f/<fmt>`), both templates expanded when used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TimeMod {
    pub flags: Option<Format>,
    pub fmt: Option<Format>,
}

/// `FORMAT_EXPAND_TIME`'s output buffer (format.c:6702 `expanded[8192]`) and
/// `format_find`'s `s[512]` (format.c:4620).
const EXPAND_MAX: usize = 8192;
const FIND_MAX: usize = 512;

/// How `format_find` renders the time (format.c:4690-4705): the first of these in
/// this order that the modifiers selected.
enum Mode {
    Relative,
    Difference,
    Pretty,
    Strftime(String),
    Ctime,
}

/// `format_strip` (format.c:4800): remove the escaping `#` of `#,` `##` `#{` `#}` `#:`
/// outside braces, keeping it inside.
fn strip(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut brackets = 0i32;
    let mut i = 0;
    while i < b.len() {
        let next = b.get(i + 1).copied();
        if b[i] == b'#' && next == Some(b'{') {
            brackets += 1;
        }
        if b[i] == b'#' && next.is_some_and(|n| b",#{}:".contains(&n)) {
            if brackets != 0 {
                out.push(b[i]);
            }
            i += 1;
            continue;
        }
        if b[i] == b'}' {
            brackets -= 1;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// `format_relative_time` (format.c:4559): `None` for a time in the future.
#[allow(clippy::many_single_char_names)]
fn relative(t: i64, now: i64) -> Option<String> {
    if t > now {
        return None;
    }
    if t == now {
        return Some("0s".to_owned());
    }
    let age = now - t;
    let (d, h, m, s) = (age / 86400, age % 86400 / 3600, age % 3600 / 60, age % 60);
    Some(if d != 0 {
        if h != 0 {
            format!("{d}d{h}h")
        } else {
            format!("{d}d")
        }
    } else if h != 0 {
        if m != 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        }
    } else if m != 0 {
        if s != 0 {
            format!("{m}m{s}s")
        } else {
            format!("{m}m")
        }
    } else {
        format!("{s}s")
    })
}

/// `format_pretty_time` (format.c:4515), with `t` and `now` already broken down.
fn pretty(tm: &Tm, now_tm: &Tm, age: i64) -> String {
    let (year, month) = (tm.year, tm.month);
    let f = |fmt: &str| tm.strftime(fmt);
    if age < 24 * 3600 {
        f("%H:%M")
    } else if (year == now_tm.year && month == now_tm.month) || age < 28 * 24 * 3600 {
        f("%a%d")
    } else if (year == now_tm.year && month < now_tm.month)
        || (year == now_tm.year - 1 && month > now_tm.month)
    {
        f("%d%b")
    } else {
        f("%h%y")
    }
}

impl Eval<'_> {
    /// The strftime pass of `format_expand1` over text (format.c:6728-6741), with the
    /// time taken once per expansion (`es->time`). `None` is "format is too long",
    /// which makes the whole expansion empty.
    pub(super) fn strftime_pass(&self, text: &str) -> Option<String> {
        if !text.contains('%') {
            return Some(text.to_owned());
        }
        match self.wall.local_time(self.now) {
            Some(tm) => tm.strftime_max(text, EXPAND_MAX),
            None => Some(text.to_owned()),
        }
    }

    /// `format_find`'s time branch (format.c:4681-4709) for variable `name`.
    /// `None` is "not found".
    pub(super) fn find_time(
        &mut self,
        name: &str,
        mods: &[TimeMod],
        depth: usize,
    ) -> Option<String> {
        // format_find consults the environment only without FORMAT_TIMESTRING
        // (format.c:4666), so `#{t:TS}` never sees an environment variable.
        let t = self.ctx.time(name).or_else(|| {
            let v = self.ctx.get_without_environ(name)?;
            num::strtonum(&v, 0, i64::MAX)
        })?;
        if t <= 0 {
            return None;
        }
        let now = self.now;
        Some(match self.time_mode_of(mods, depth) {
            Mode::Relative => relative(t, now)?,
            Mode::Difference => (now - t).to_string(),
            Mode::Pretty => {
                let now = now.max(t);
                match (self.wall.local_time(t), self.wall.local_time(now)) {
                    (Some(tm), Some(now_tm)) => pretty(&tm, &now_tm, now - t),
                    _ => String::new(),
                }
            }
            // A failed strftime leaves the buffer unspecified; empty is the sane reading.
            Mode::Strftime(fmt) => self
                .wall
                .local_time(t)?
                .strftime_max(&fmt, FIND_MAX)
                .unwrap_or_default(),
            Mode::Ctime => self.wall.local_time(t)?.ctime()?,
        })
    }

    /// The flags of every `t` modifier (format.c:6150-6170): `p`, `r`, `d` and `f`
    /// are an else-if chain within one modifier; the last `f` supplies the format and
    /// the flags of different modifiers accumulate.
    fn time_mode_of(&mut self, mods: &[TimeMod], depth: usize) -> Mode {
        let (mut pretty, mut relative, mut diff) = (false, false, false);
        let mut fmt = None;
        for m in mods {
            let Some(flags) = &m.flags else { continue };
            let flags = self.expand(flags, depth);
            if flags.contains('p') {
                pretty = true;
            } else if flags.contains('r') {
                relative = true;
            } else if flags.contains('d') {
                diff = true;
            } else if flags.contains('f')
                && let Some(f) = &m.fmt
            {
                fmt = Some(strip(&self.expand(f, depth)));
            }
        }
        if relative {
            Mode::Relative
        } else if diff {
            Mode::Difference
        } else if pretty {
            Mode::Pretty
        } else if let Some(f) = fmt {
            Mode::Strftime(f)
        } else {
            Mode::Ctime
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_ages() {
        assert_eq!(relative(10, 9), None);
        assert_eq!(relative(10, 10).as_deref(), Some("0s"));
        assert_eq!(relative(0, 90_000).as_deref(), Some("1d1h"));
        assert_eq!(relative(0, 86_400).as_deref(), Some("1d"));
        assert_eq!(relative(0, 7_260).as_deref(), Some("2h1m"));
        assert_eq!(relative(0, 3_600).as_deref(), Some("1h"));
        assert_eq!(relative(0, 61).as_deref(), Some("1m1s"));
        assert_eq!(relative(0, 60).as_deref(), Some("1m"));
        assert_eq!(relative(0, 5).as_deref(), Some("5s"));
    }

    #[test]
    fn strip_keeps_escapes_inside_braces() {
        assert_eq!(strip("%H#:%M"), "%H:%M");
        assert_eq!(strip("a##b#,c"), "a#b,c");
        assert_eq!(strip("#{a#,b}"), "#{a#,b}");
    }
}

/// Reference goldens: `tests/time_cases.sh` regenerates `time_rows.in`.
#[cfg(test)]
mod golden {
    use crate::format::{FormatContext, expand_at, expand_time_at, parse};
    use crate::time::{FixedWallClock, WallClock};
    use std::collections::HashMap;

    #[derive(Clone, Copy)]
    enum Kind {
        /// `set -gF @out`: `format_expand`.
        Plain,
        /// `display-message -p`: `format_expand_time`.
        Time,
    }

    type Row = (&'static str, i64, i64, Kind, &'static str, &'static str);

    include!("time_rows.in");

    /// The fixture of `start()` in `time_cases.sh`.
    struct Vars(HashMap<&'static str, String>);

    impl FormatContext for Vars {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn ctx(ts: i64) -> Vars {
        Vars(HashMap::from([
            ("@ts", ts.to_string()),
            ("@fmt", "%Y|%%|#S".to_owned()),
            ("@nested", "#{T:@fmt}#{E:@fmt}".to_owned()),
            ("@pct", "%H".to_owned()),
            ("@nopct", "plain".to_owned()),
            ("@zero", "0".to_owned()),
            ("session_name", "s".to_owned()),
        ]))
    }

    #[test]
    fn time_rows_match_reference() {
        let mut failures = Vec::new();
        let rows = rows();
        assert!(rows.len() > 3000, "{} rows", rows.len());
        for (tz, now, ts, kind, template, expected) in rows {
            let wall = FixedWallClock::new(now, tz).expect("zoneinfo has the zone");
            let ctx = ctx(ts);
            let got = match kind {
                Kind::Plain => expand_at(&parse(template).ast, &ctx, &wall),
                Kind::Time => expand_time_at(template, &ctx, &wall),
            };
            if got != expected {
                failures.push(format!(
                    "{tz} now={now} ts={ts} {template:?}: expected {expected:?}, got {got:?}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of the rows differ:\n{}",
            failures.len(),
            failures[..failures.len().min(60)].join("\n")
        );
    }

    /// The conversions the task names must be in the goldens under both zones.
    #[test]
    fn goldens_cover_the_named_conversions() {
        let rows = rows();
        for tz in ["UTC", "Europe/London"] {
            for c in "abdeHMSYypZzjU%".chars() {
                let t = format!("#{{t/f/%{c}:@ts}}");
                assert!(
                    rows.iter().any(|r| r.0 == tz && r.4 == t),
                    "{tz} lacks %{c}"
                );
            }
            let n = rows
                .iter()
                .filter(|r| r.0 == tz && r.4.starts_with("#{t/f/%"))
                .map(|r| r.4)
                .collect::<std::collections::HashSet<_>>()
                .len();
            assert!(n >= 20, "{tz}: {n} conversions");
        }
    }

    #[test]
    fn wall_clock_is_injected() {
        let wall = FixedWallClock::new(5, "UTC").expect("zoneinfo");
        assert_eq!(wall.now(), 5);
    }
}
