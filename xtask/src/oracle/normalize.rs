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
static TTY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/dev/(pts/\d+|tty[A-Za-z0-9]*|pty[A-Za-z0-9]*)").unwrap());
static ENV_TMUX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(<SOCKET>),\d+,").unwrap());
static GUARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^%(begin|end|error) \d+ (\d+) (\d+)$").unwrap());

/// Per-run replacement table.
pub struct Normalizer {
    /// Literal replacements, longest first.
    literals: Vec<(String, String)>,
    hosts: Vec<String>,
}

impl Normalizer {
    /// `socket_dir` is the full `<root>/<prefix>-<uid>` path; `label` the `-L` name.
    pub fn new(root: &str, socket_dir: &str, label: &str, host: &str) -> Self {
        let mut literals = vec![
            (format!("{socket_dir}/{label}"), "<SOCKET>".to_string()),
            (socket_dir.to_string(), "<SOCKDIR>".to_string()),
            (root.to_string(), "<TMP>".to_string()),
            (label.to_string(), "<LABEL>".to_string()),
        ];
        literals.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        let mut hosts = Vec::new();
        if host.len() >= 3 {
            hosts.push(host.to_string());
            // The default status-right shows `#{=21:pane_title}`, the hostname.
            let short: String = host.chars().take(21).collect();
            if short != host {
                hosts.push(short);
            }
        }
        Self { literals, hosts }
    }

    /// Placeholders for paths, dates, clocks, tty names, hosts and the pid in `$TMUX`.
    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (from, to) in &self.literals {
            out = out.replace(from, to);
        }
        for h in &self.hosts {
            out = out.replace(h, "<HOST>");
        }
        out = ENV_TMUX.replace_all(&out, "$1,<PID>,").into_owned();
        out = DATE.replace_all(&out, "<DATE>").into_owned();
        out = CLOCK.replace_all(&out, "<CLOCK>").into_owned();
        out = TTY.replace_all(&out, "<TTY>").into_owned();
        out
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
        Normalizer::new("/tmp/r", "/tmp/r/tmux-1000", "lab", "myhost")
    }

    #[test]
    fn replaces_paths_dates_and_clocks() {
        let n = norm();
        assert_eq!(
            n.text("error connecting to /tmp/r/tmux-1000/lab (No such file or directory)"),
            "error connecting to <SOCKET> (No such file or directory)"
        );
        assert_eq!(
            n.text("a: 1 windows (created Thu Oct 29 16:56:06 2026)"),
            "a: 1 windows (created <DATE>)"
        );
        assert_eq!(n.text("\"myhost\" 20:23 29-Oct-26"), "\"<HOST>\" <CLOCK>");
        assert_eq!(n.text("/tmp/r/tmux-1000/lab,4242,0"), "<SOCKET>,<PID>,0");
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
}
