//! Normalisation of volatile values before storing or diffing an observation.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

static DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(Mon|Tue|Wed|Thu|Fri|Sat|Sun) [A-Z][a-z]{2} [ 0-9]\d \d\d:\d\d:\d\d \d{4}\b")
        .unwrap()
});
static CLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d\d:\d\d \d\d-[A-Z][a-z]{2}-\d\d\b").unwrap());
/// A clock-mode glyph row as a screen row is stored (`\e` for ESC): only
/// spaces, SGR sequences and at least one `#` that has a colour set.
static CLOCK_GLYPHS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?: |#|\\e\[[0-9;]*m)*#(?: |#|\\e\[[0-9;]*m)*$").unwrap());
static SGR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\e\[([0-9;]*)m").unwrap());
/// A compact clock-mode row: the time alone on the row (`%l:%M `+AM/PM,
/// or `%H:%M`, with or without seconds).
static CLOCK_COMPACT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^((?:\\e\[[0-9;]*m| )*?)[ 0-9]?\d:\d\d(?::\d\d)?(?: [AP]M)?((?:\\e\[[0-9;]*m)*)$")
        .unwrap()
});
static TTY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/dev/(pts/\d+|tty[A-Za-z0-9]*|pty[A-Za-z0-9]*)").unwrap());
static ENV_TMUX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(<SOCKET>),\d+,").unwrap());
static GUARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^%(begin|end|error) \d+ (\d+) (\d+)$").unwrap());

/// Per-run replacement table.
pub struct Normalizer {
    /// Literal replacements, longest first.
    literals: Vec<(String, String)>,
}

impl Normalizer {
    /// `root` is the run's temp root, `socket` the `-S` path inside it.
    pub fn new(root: &str, socket: &str) -> Self {
        let mut literals = vec![
            (socket.to_string(), "<SOCKET>".to_string()),
            (root.to_string(), "<TMP>".to_string()),
        ];
        literals.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Self { literals }
    }

    /// Placeholders for paths, dates, clocks, tty names and the pid in `$TMUX`.
    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (from, to) in &self.literals {
            out = out.replace(from, to);
        }
        out = ENV_TMUX.replace_all(&out, "$1,<PID>,").into_owned();
        out = DATE.replace_all(&out, "<DATE>").into_owned();
        out = CLOCK.replace_all(&out, "<CLOCK>").into_owned();
        out = TTY.replace_all(&out, "<TTY>").into_owned();
        out
    }

    /// One screen row, stored with `\e` for ESC: [`Self::text`], then
    /// clock mode's rows, whose digits depend on the time of day. A glyph
    /// row becomes `<CLOCKGLYPHS fg bg>` (the colours it was drawn in, so a
    /// wrong `clock-mode-colour` still shows); a compact clock keeps its
    /// padding and colour and loses the time, `<CLOCK>`. The cursor, which
    /// the screen observation carries, pins where the compact text ends.
    pub fn row(&self, r: &str) -> String {
        let r = self.text(r);
        if CLOCK_GLYPHS.is_match(&r) && r.contains("\\e[") {
            let mut colours: Vec<&str> = SGR
                .captures_iter(&r)
                .filter_map(|c| c.get(1))
                .map(|m| m.as_str())
                .filter(|c| !matches!(*c, "" | "0" | "39" | "49"))
                .collect();
            colours.dedup();
            colours.sort_unstable();
            colours.dedup();
            return format!("<CLOCKGLYPHS {}>", colours.join(" "));
        }
        if let Some(c) = CLOCK_COMPACT.captures(&r) {
            // The padding's SGRs first, then its spaces: where the colour
            // starts within the blanks depends on the time's own width.
            let sgrs: String = SGR.find_iter(&c[1]).map(|m| m.as_str()).collect();
            let spaces = SGR.replace_all(&c[1], "").len();
            return format!("{sgrs}{}<CLOCK>{}", " ".repeat(spaces), &c[2]);
        }
        r
    }

    /// A control-mode stream (or any stdout): `text`, guard lines with
    /// timestamps dropped and command numbers renumbered by first appearance
    /// (absolute numbers count internal queue items, cmd-queue.c:713,745-746),
    /// and runs of `%output` coalesced per pane into one decoded payload
    /// (chunking depends on buffering, control.c:807-855,936-948).
    pub fn stream(&self, s: &str) -> Vec<String> {
        let text = self.text(s);
        let mut numbers: HashMap<String, usize> = HashMap::new();
        let mut out = Vec::new();
        let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
        let flush = |pending: &mut Vec<(String, Vec<u8>)>, out: &mut Vec<String>| {
            for (pane, bytes) in pending.drain(..) {
                out.push(format!("%output {pane} {}", bytes.escape_ascii()));
            }
        };
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("%output ") {
                let (pane, payload) = rest.split_once(' ').unwrap_or((rest, ""));
                let bytes = unescape_octal(payload);
                match pending.iter_mut().find(|(p, _)| p == pane) {
                    Some((_, acc)) => acc.extend(bytes),
                    None => pending.push((pane.to_string(), bytes)),
                }
                continue;
            }
            flush(&mut pending, &mut out);
            if let Some(c) = GUARD.captures(line) {
                let next = numbers.len() + 1;
                let n = *numbers.entry(c[2].to_string()).or_insert(next);
                let mut l = String::new();
                write!(l, "%{} <TIME> #{n} {}", &c[1], &c[3]).unwrap();
                out.push(l);
            } else {
                out.push(line.to_string());
            }
        }
        flush(&mut pending, &mut out);
        // `split` leaves one empty tail after the final newline.
        if out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        out
    }
}

/// Replace each of `hosts` with `<HOST>` where it is a whole token: not
/// adjacent to `[A-Za-z0-9_.-]`, so a host called `server` leaves `no server
/// running` alone only when it is part of a longer word, never inside one.
pub fn replace_hosts(s: &str, hosts: &[&str]) -> String {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'scan: while i < s.len() {
        for h in hosts.iter().filter(|h| !h.is_empty()) {
            if s[i..].starts_with(h)
                && !s[..i].chars().next_back().is_some_and(word)
                && !s[i + h.len()..].chars().next().is_some_and(word)
            {
                out.push_str("<HOST>");
                i += h.len();
                continue 'scan;
            }
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (u32::from(b[i + 1] - b'0') << 6)
                | (u32::from(b[i + 2] - b'0') << 3)
                | u32::from(b[i + 3] - b'0');
            out.push(u8::try_from(v & 0xff).unwrap());
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm() -> Normalizer {
        Normalizer::new("/tmp/r", "/tmp/r/s")
    }

    #[test]
    fn replaces_paths_dates_and_clocks() {
        let n = norm();
        assert_eq!(
            n.text("error connecting to /tmp/r/s (No such file or directory)"),
            "error connecting to <SOCKET> (No such file or directory)"
        );
        assert_eq!(
            n.text("a: 1 windows (created Thu Oct 29 16:56:06 2026)"),
            "a: 1 windows (created <DATE>)"
        );
        assert_eq!(n.text("\"myhost\" 20:23 29-Oct-26"), "\"myhost\" <CLOCK>");
        assert_eq!(n.text("/tmp/r/s,4242,0"), "<SOCKET>,<PID>,0");
    }

    #[test]
    fn clock_mode_rows_lose_their_time() {
        let n = norm();
        // Large clock (5x5 glyphs, `#` in fg and bg colour red).
        assert_eq!(
            n.row("      \\e[31m\\e[41m#####\\e[39m\\e[49m \\e[31m\\e[41m#\\e[39m\\e[49m"),
            "<CLOCKGLYPHS 31 41>"
        );
        // A row of `#` with no colour is the pane's own text.
        assert_eq!(n.row("#### ####"), "#### ####");
        // Compact clock: 12h and 24h, with and without seconds.
        assert_eq!(
            n.row("\\e[31m       5:54 PM\\e[39m"),
            "\\e[31m      <CLOCK>\\e[39m"
        );
        assert_eq!(
            n.row("\\e[34m      12:07 AM\\e[39m"),
            "\\e[34m      <CLOCK>\\e[39m"
        );
        assert_eq!(n.row("   17:54"), "   <CLOCK>");
        assert_eq!(
            n.row("      \\e[31m10:05 PM\\e[39m"),
            n.row("\\e[31m      10:05 PM\\e[39m")
        );
        assert_eq!(n.row("   17:54:09"), "   <CLOCK>");
        // Text that merely has a time in it stays.
        assert_eq!(n.row("meeting at 12:30"), "meeting at 12:30");
        assert_eq!(n.row("$ date"), "$ date");
    }

    #[test]
    fn stream_renumbers_guards_and_coalesces_output() {
        let n = norm();
        let raw = "%begin 1793305120 313 0\n%end 1793305120 313 0\n%output %0 ab\n%output %1 x\n%output %0 c\\015\n%begin 1 315 1\nhello\n%end 1 315 1\n%exit\n";
        assert_eq!(
            n.stream(raw),
            [
                "%begin <TIME> #1 0",
                "%end <TIME> #1 0",
                "%output %0 abc\\r",
                "%output %1 x",
                "%begin <TIME> #2 1",
                "hello",
                "%end <TIME> #2 1",
                "%exit",
            ]
        );
    }

    #[test]
    fn hosts_replace_whole_tokens_only() {
        let h = ["abc"];
        assert_eq!(
            replace_hosts("\"abc\" abc, x-abc abc.d abcd abc", &h),
            "\"<HOST>\" <HOST>, x-abc abc.d abcd <HOST>"
        );
        assert_eq!(
            replace_hosts("%output %0 abc\\r", &h),
            "%output %0 <HOST>\\r"
        );
        assert_eq!(
            replace_hosts("no server running", &["server"]),
            "no <HOST> running"
        );
        assert_eq!(replace_hosts("héllo abc", &h), "héllo <HOST>");
        assert_eq!(replace_hosts("abc", &[""]), "abc");
    }
}
