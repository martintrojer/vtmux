//! OSC 7577 Program Resume Protocol, rev 0.5: the verifier and the record
//! store (D35). Pure: the clock, the token and the process table come in
//! as arguments, so nothing here touches the OS but [`generate_token`].
//!
//! A report is *ignored* (no effect at all) when it fails the `mac` or
//! `time` checks, and *rejected* (the owner's records go) when it passed
//! both and is still not acceptable.

#![allow(clippy::chunks_exact_to_as_chunks)]

use std::collections::HashSet;
use std::io::Read;

use crate::sys::pty::ProcessInfo;

/// The longest body a report may have (the scanner's limit too).
pub const MAX_BODY: usize = 4096;
/// The most `argv` elements a report may carry.
pub const MAX_ARGV: usize = 128;
/// The most records one pane keeps; the oldest goes first.
pub const MAX_RECORDS: usize = 64;
/// The most `pid`s whose last accepted `time` is remembered, independent
/// of [`MAX_RECORDS`]; the least recently updated goes first.
pub const MAX_LAST_TIMES: usize = 256;
/// How far a report's `time` may be from the clock, in seconds.
pub const TIME_WINDOW: u64 = 60;
/// Keys this implementation understands (what `need` may name).
const KNOWN_KEYS: [&str; 7] = ["pid", "time", "mac", "argv", "cwd", "clear", "need"];

// ---------------------------------------------------------------- SHA-256

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

fn compress(h: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 64];
    for (i, c) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let mut v = *h;
    for i in 0..64 {
        let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
        let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
        let t1 = v[7]
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
        let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let t2 = s0.wrapping_add(maj);
        v[7] = v[6];
        v[6] = v[5];
        v[5] = v[4];
        v[4] = v[3].wrapping_add(t1);
        v[3] = v[2];
        v[2] = v[1];
        v[1] = v[0];
        v[0] = t1.wrapping_add(t2);
    }
    for (a, b) in h.iter_mut().zip(v) {
        *a = a.wrapping_add(b);
    }
}

/// SHA-256 of `parts` concatenated.
#[must_use]
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut data: Vec<u8> = parts.concat();
    let bits = (data.len() as u64).wrapping_mul(8);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bits.to_be_bytes());
    for block in data.chunks_exact(64) {
        compress(&mut h, block);
    }
    let mut out = [0u8; 32];
    for (o, w) in out.chunks_exact_mut(4).zip(h) {
        o.copy_from_slice(&w.to_be_bytes());
    }
    out
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ----------------------------------------------------------------- base64

#[cfg(test)]
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Strict RFC 4648 §4 decoding. Padding is optional, but if present it
/// must be the right amount; the unused bits of the last character must be
/// zero; no whitespace, no URL-safe alphabet.
#[must_use]
pub fn base64_decode(s: &[u8]) -> Option<Vec<u8>> {
    let data_len = s.iter().position(|&c| c == b'=').unwrap_or(s.len());
    let (data, pad) = s.split_at(data_len);
    if pad.iter().any(|&c| c != b'=') {
        return None;
    }
    let rem = data.len() % 4;
    if rem == 1 {
        return None;
    }
    if !pad.is_empty() && (data.len() + pad.len()) % 4 != 0 || pad.len() > 2 {
        return None;
    }
    if !pad.is_empty() && data.len() + pad.len() != data.len().div_ceil(4) * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    for chunk in data.chunks(4) {
        let mut acc = 0u32;
        for &c in chunk {
            acc = (acc << 6) | b64_val(c)?;
        }
        acc <<= 6 * (4 - u32::try_from(chunk.len()).unwrap_or(4));
        let bytes = acc.to_be_bytes();
        let n = chunk.len() * 6 / 8;
        if bytes[1 + n..4].iter().any(|&b| b != 0) {
            return None;
        }
        out.extend_from_slice(&bytes[1..=n]);
    }
    Some(out)
}

// ------------------------------------------------------------------ token

/// A fresh pane token: 24 bytes from `/dev/urandom` (192 bits) as 32
/// characters of `[A-Za-z0-9_-]` (base64url without padding).
///
/// # Errors
/// When `/dev/urandom` cannot be read.
pub fn generate_token() -> std::io::Result<String> {
    let mut raw = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    Ok(token_from_bytes(&raw))
}

fn token_from_bytes(raw: &[u8; 24]) -> String {
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::with_capacity(32);
    for c in raw.chunks_exact(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        for shift in [18, 12, 6, 0] {
            s.push(char::from(URL[((n >> shift) & 63) as usize]));
        }
    }
    s
}

// ---------------------------------------------------------- shell quoting

/// `s` as one POSIX shell word: bare when it only holds characters no
/// shell treats specially, else single-quoted.
#[must_use]
pub fn shell_quote(s: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '%' | '+' | ':' | ',' | '.' | '/' | '-')
    };
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// An argument list as one shell command line.
#[must_use]
pub fn shell_command(argv: &[String]) -> String {
    const RESERVED: [&str; 22] = [
        "if", "then", "else", "elif", "fi", "do", "done", "case", "esac", "while", "until", "for",
        "in", "{", "}", "!", "function", "select", "time", "[[", "]]", "coproc",
    ];
    argv.iter()
        .enumerate()
        .map(|(i, a)| {
            if i == 0 && RESERVED.contains(&a.as_str()) {
                format!("'{a}'")
            } else {
                shell_quote(a)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ----------------------------------------------------------------- report

/// What a report did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No effect at all.
    Ignored,
    /// A record was stored.
    Accepted {
        /// The visible state differs from before.
        changed: bool,
    },
    /// `clear=1`.
    Cleared {
        /// A record was removed.
        changed: bool,
    },
    /// Verified but unacceptable: the owner's records were removed.
    Rejected {
        /// A record was removed.
        changed: bool,
    },
}

impl Outcome {
    /// Whether the records differ from before the report.
    #[must_use]
    pub fn changed(self) -> bool {
        match self {
            Self::Ignored => false,
            Self::Accepted { changed } | Self::Cleared { changed } | Self::Rejected { changed } => {
                changed
            }
        }
    }
}

/// One accepted report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The owning process.
    pub pid: u32,
    /// The owner's start time when accepted (reused-pid detection).
    pub start_time: u64,
    /// The command; `argv[0]` is a bare name or an absolute path.
    pub argv: Vec<String>,
    /// The report's `cwd`, else the owner's directory at report time.
    pub cwd: Option<String>,
    /// Arrival order, for "most recent".
    pub seq: u64,
}

/// A process lookup: `None` when the process is gone.
pub type Lookup<'a> = &'a dyn Fn(u32) -> Option<ProcessInfo>;

/// The records and replay state of one pane.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResumeStore {
    token: String,
    records: Vec<Record>,
    /// `(pid, last accepted time)`, least recently updated first.
    last_times: Vec<(u32, u64)>,
    seq: u64,
}

struct Parsed<'a> {
    pairs: Vec<(&'a str, &'a str)>,
}

fn value_ok(v: &[u8]) -> bool {
    v.iter().all(|&c| {
        c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
    })
}

fn parse_uint(v: &str) -> Option<u64> {
    (!v.starts_with('0') && !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()))
        .then(|| v.parse().ok())?
}

/// Pairs of `prefix`, `None` when any is malformed.
fn parse_pairs(prefix: &str) -> Option<Parsed<'_>> {
    let mut pairs = Vec::new();
    for pair in prefix.split(':') {
        let (k, v) = pair.split_once('=')?;
        if k.is_empty() || !k.bytes().all(|c| c.is_ascii_lowercase()) || !value_ok(v.as_bytes()) {
            return None;
        }
        pairs.push((k, v));
    }
    Some(Parsed { pairs })
}

fn no_control(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

fn decode_text(v: &str) -> Option<String> {
    let s = String::from_utf8(base64_decode(v.as_bytes())?).ok()?;
    no_control(&s).then_some(s)
}

fn argv_ok(argv: &[String]) -> bool {
    let first = &argv[0];
    !first.is_empty()
        && (first.starts_with('/') || (!first.contains('/') && !first.starts_with('-')))
}

enum Verdict {
    Accept(Vec<String>, Option<String>),
    Clear,
}

/// The strict part, after `mac` and `time` passed: `None` rejects.
fn validate(p: &Parsed<'_>) -> Option<Verdict> {
    let mut seen: Vec<&str> = Vec::new();
    for (k, _) in &p.pairs {
        if seen.contains(k) {
            return None;
        }
        seen.push(k);
    }
    let get = |k: &str| p.pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
    parse_uint(get("pid")?)?;
    parse_uint(get("time")?)?;
    if get("need").is_some_and(|need| need.split(',').any(|n| !KNOWN_KEYS.contains(&n))) {
        return None;
    }
    let cwd = match get("cwd") {
        Some(v) => {
            let c = decode_text(v)?;
            if !c.starts_with('/') {
                return None;
            }
            Some(c)
        }
        None => None,
    };
    match (get("argv"), get("clear")) {
        (Some(a), None) => {
            let elems: Vec<&str> = a.split(',').collect();
            if elems.len() > MAX_ARGV {
                return None;
            }
            let argv: Vec<String> = elems.into_iter().map(decode_text).collect::<Option<_>>()?;
            argv_ok(&argv).then_some(Verdict::Accept(argv, cwd))
        }
        (None, Some("1")) => Some(Verdict::Clear),
        _ => None,
    }
}

/// Constant-time equality.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// The owner of a report that may be malformed: the `pid` all its `pid`
/// pairs agree on, if valid. Read from raw bytes, so a non-UTF-8 byte in
/// another pair does not hide it.
fn owner_pid(prefix: &[u8]) -> Option<u32> {
    let mut found: Option<&[u8]> = None;
    for pair in prefix.split(|&c| c == b':') {
        if let Some(v) = pair.strip_prefix(b"pid=") {
            match found {
                Some(f) if f != v => return None,
                _ => found = Some(v),
            }
        }
    }
    let pid = u32::try_from(parse_uint(std::str::from_utf8(found?).ok()?)?).ok()?;
    i32::try_from(pid).ok()?;
    Some(pid)
}

fn first_time(prefix: &[u8]) -> Option<u64> {
    prefix
        .split(|&c| c == b':')
        .find_map(|p| p.strip_prefix(b"time="))
        .and_then(|v| parse_uint(std::str::from_utf8(v).ok()?))
}

/// Whether a `mac` key precedes the final one.
fn has_early_mac(prefix: &[u8]) -> bool {
    prefix.split(|&c| c == b':').any(|p| p.starts_with(b"mac="))
}

impl ResumeStore {
    /// A store verifying reports against `token`.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self {
            token,
            ..Self::default()
        }
    }

    /// The token (for the pane's initial environment only).
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The records, oldest first.
    #[must_use]
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// The remembered `(pid, time)` pairs, least recently updated first.
    #[must_use]
    pub fn last_times(&self) -> &[(u32, u64)] {
        &self.last_times
    }

    /// Rebuild a store (live upgrade).
    #[must_use]
    pub fn from_parts(token: String, records: Vec<Record>, last_times: Vec<(u32, u64)>) -> Self {
        let seq = records.iter().map(|r| r.seq + 1).max().unwrap_or(0);
        Self {
            token,
            records,
            last_times,
            seq,
        }
    }

    /// Drop every record and replay state, keep a new `token` (respawn).
    pub fn reset(&mut self, token: String) {
        *self = Self::new(token);
    }

    fn remove_pid(&mut self, pid: u32) -> bool {
        let n = self.records.len();
        self.records.retain(|r| r.pid != pid);
        self.records.len() != n
    }

    fn note_time(&mut self, pid: u32, time: u64) {
        self.last_times.retain(|&(p, _)| p != pid);
        if self.last_times.len() >= MAX_LAST_TIMES {
            // Never forget a pid that still has a record (replay guard).
            let at = self
                .last_times
                .iter()
                .position(|&(p, _)| !self.records.iter().any(|r| r.pid == p))
                .unwrap_or(0);
            self.last_times.remove(at);
        }
        self.last_times.push((pid, time));
    }

    /// Handle one report `body` (what follows `7577;`) at clock `now`
    /// (seconds since the epoch) for a terminal run by user `euid`.
    pub fn report(&mut self, body: &[u8], now: u64, euid: u32, lookup: Lookup<'_>) -> Outcome {
        let Some((prefix, time, owner)) = self.screen(body, now) else {
            return Outcome::Ignored;
        };
        let sel = |s: &Self| {
            s.best(lookup).map(|r| Record {
                seq: 0,
                ..r.clone()
            })
        };
        let before = sel(self);
        let out = self.report_inner(prefix, time, owner, euid, lookup);
        let moved = sel(self) != before;
        match out {
            Outcome::Ignored => out,
            Outcome::Accepted { changed } => Outcome::Accepted {
                changed: changed || moved,
            },
            Outcome::Cleared { changed } => Outcome::Cleared {
                changed: changed || moved,
            },
            Outcome::Rejected { changed } => Outcome::Rejected {
                changed: changed || moved,
            },
        }
    }

    /// The checks that need no process lookup: size, `?`, mac, time window
    /// and monotonic time. `None` means the report is ignored. Yields the
    /// signed prefix, its time and its owner.
    fn screen<'b>(&self, body: &'b [u8], now: u64) -> Option<(&'b [u8], u64, Option<u32>)> {
        if body.len() > MAX_BODY || body == b"?" || self.token.is_empty() {
            return None;
        }
        // mac: last key, 64 lowercase hex, over prefix ":" token.
        let at = body
            .len()
            .checked_sub(5 + 64)
            .filter(|&i| &body[i..i + 5] == b":mac=")?;
        let (prefix, mac) = (&body[..at], &body[at + 5..]);
        if !mac
            .iter()
            .all(|&c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return None;
        }
        let want = hex(&sha256(&[prefix, b":", self.token.as_bytes()]));
        if !ct_eq(want.as_bytes(), mac) {
            return None;
        }
        // time: window and monotonic per pid.
        let time = first_time(prefix)?;
        if now.abs_diff(time) > TIME_WINDOW {
            return None;
        }
        let owner = owner_pid(prefix);
        if let Some(pid) = owner
            && self.last_times.iter().any(|&(p, t)| p == pid && time < t)
        {
            return None;
        }
        Some((prefix, time, owner))
    }

    fn report_inner(
        &mut self,
        prefix: &[u8],
        time: u64,
        owner: Option<u32>,
        euid: u32,
        lookup: Lookup<'_>,
    ) -> Outcome {
        let reject = |s: &mut Self| Outcome::Rejected {
            changed: owner.is_some_and(|p| s.remove_pid(p)),
        };
        let Some(pid) = owner else {
            return Outcome::Rejected { changed: false };
        };
        let verdict = if has_early_mac(prefix) {
            None
        } else {
            std::str::from_utf8(prefix)
                .ok()
                .and_then(parse_pairs)
                .and_then(|p| validate(&p))
        };
        match verdict {
            None => reject(self),
            Some(Verdict::Clear) => {
                // D35: a dead or foreign owner is rejected as for accept; its
                // record, if any, still goes and its time is not noted.
                if lookup(pid).is_none_or(|i| i.uid != euid) {
                    return reject(self);
                }
                self.note_time(pid, time);
                Outcome::Cleared {
                    changed: self.remove_pid(pid),
                }
            }
            Some(Verdict::Accept(argv, cwd)) => {
                let Some(info) = lookup(pid).filter(|i| i.uid == euid) else {
                    return reject(self);
                };
                self.note_time(pid, time);
                let rec = Record {
                    pid,
                    start_time: info.start_time,
                    argv,
                    cwd: cwd.or(info.cwd),
                    seq: self.seq,
                };
                self.seq += 1;
                let same = self.records.iter().find(|r| r.pid == pid).is_some_and(|r| {
                    (r.start_time, &r.argv, &r.cwd) == (rec.start_time, &rec.argv, &rec.cwd)
                });
                self.remove_pid(pid);
                if self.records.len() >= MAX_RECORDS {
                    self.records.remove(0);
                }
                self.records.push(rec);
                Outcome::Accepted { changed: !same }
            }
        }
    }

    /// Drop records whose owner is gone or whose pid was reused. Returns
    /// whether any went.
    pub fn prune(&mut self, lookup: Lookup<'_>) -> bool {
        let n = self.records.len();
        self.records
            .retain(|r| lookup(r.pid).is_some_and(|i| i.start_time == r.start_time));
        self.records.len() != n
    }

    /// The record to resume: of those whose owner is not a descendant of
    /// another record's owner, the most recent.
    #[must_use]
    pub fn best(&self, lookup: Lookup<'_>) -> Option<&Record> {
        let descends = |r: &Record| {
            let mut pid = r.pid;
            let mut seen = HashSet::from([pid]);
            loop {
                let Some(info) = lookup(pid) else {
                    return false;
                };
                pid = info.ppid;
                if pid == 0 || !seen.insert(pid) {
                    return false;
                }
                if self.records.iter().any(|o| o.pid == pid) {
                    return true;
                }
            }
        };
        self.records
            .iter()
            .filter(|r| !descends(r))
            .max_by_key(|r| r.seq)
    }

    /// `#{pane_resume_command}`.
    #[must_use]
    pub fn command(&self, lookup: Lookup<'_>) -> Option<String> {
        self.best(lookup).map(|r| shell_command(&r.argv))
    }

    /// `#{pane_resume_cwd}`.
    #[must_use]
    pub fn cwd(&self, lookup: Lookup<'_>) -> Option<String> {
        self.best(lookup).and_then(|r| r.cwd.clone())
    }

    /// `#{pane_resume_pid}`.
    #[must_use]
    pub fn pid(&self, lookup: Lookup<'_>) -> Option<u32> {
        self.best(lookup).map(|r| r.pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const TOKEN: &str = "dummy-token-replace-me";
    const NOW: u64 = 1_795_543_200;
    const UID: u32 = 501;

    fn b64(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::new();
        for c in b.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for (i, shift) in [18, 12, 6, 0].iter().enumerate() {
                if i <= c.len() {
                    out.push(char::from(B64[((n >> shift) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn sign(prefix: &str) -> Vec<u8> {
        let mac = hex(&sha256(&[prefix.as_bytes(), b":", TOKEN.as_bytes()]));
        format!("{prefix}:mac={mac}").into_bytes()
    }

    fn info(ppid: u32, start: u64) -> ProcessInfo {
        ProcessInfo {
            uid: UID,
            ppid,
            start_time: start,
            cwd: Some("/os/cwd".into()),
        }
    }

    struct Procs(HashMap<u32, ProcessInfo>);
    impl Procs {
        fn run(&self, store: &mut ResumeStore, body: &[u8], now: u64) -> Outcome {
            store.report(body, now, UID, &|p| self.0.get(&p).cloned())
        }
    }

    fn procs() -> Procs {
        Procs([(100, info(1, 10)), (200, info(100, 20)), (300, info(1, 30))].into())
    }

    fn argv_prefix(pid: u32, time: u64, argv: &[&str]) -> String {
        let a: Vec<String> = argv.iter().map(|s| b64(s)).collect();
        format!("pid={pid}:time={time}:argv={}", a.join(","))
    }

    fn store() -> ResumeStore {
        ResumeStore::new(TOKEN.into())
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            hex(&sha256(&[b""])),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(&[b"abc"])),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(&[
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            ])),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&sha256(&[&million])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        assert_eq!(sha256(&[b"ab", b"c"]), sha256(&[b"abc"]));
    }

    #[test]
    fn spec_mac_example() {
        let body = sign("pid=74877:time=1795543200:clear=1");
        assert!(
            String::from_utf8(body)
                .unwrap()
                .ends_with(":mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93")
        );
    }

    #[test]
    fn spec_neovim_example_decodes() {
        let v = "bnZpbQ==,LVM=,L2hvbWUvdXNlci8ubG9jYWwvc3RhdGUvbnZpbS9zZXNzaW9ucy84ZjJjMWQudmlt";
        let got: Vec<String> = v.split(',').map(|e| decode_text(e).unwrap()).collect();
        assert_eq!(
            got,
            [
                "nvim",
                "-S",
                "/home/user/.local/state/nvim/sessions/8f2c1d.vim"
            ]
        );
        // And it verifies end to end with our token and a real owner.
        let body = sign(&format!("pid=100:time={NOW}:argv={v}"));
        let mut s = store();
        assert_eq!(
            procs().run(&mut s, &body, NOW),
            Outcome::Accepted { changed: true }
        );
        assert_eq!(
            s.records()[0].argv[2],
            "/home/user/.local/state/nvim/sessions/8f2c1d.vim"
        );
    }

    #[test]
    fn base64_strict() {
        assert_eq!(base64_decode(b""), Some(vec![]));
        assert_eq!(base64_decode(b"bnZpbQ=="), Some(b"nvim".to_vec()));
        assert_eq!(base64_decode(b"bnZpbQ"), Some(b"nvim".to_vec()));
        assert_eq!(base64_decode(b"LVM="), Some(b"-S".to_vec()));
        assert_eq!(base64_decode(b"LVM"), Some(b"-S".to_vec()));
        assert_eq!(base64_decode(b"Zm9v"), Some(b"foo".to_vec()));
        assert_eq!(base64_decode(b"Zg"), Some(b"f".to_vec()));
        for bad in [
            &b"bnZpbQ="[..], // too little padding
            b"bnZpbQ===",    // too much
            b"LVM==",        // too much for 3 chars
            b"Zg=a",         // data after pad
            b"Z",            // 1 mod 4
            b"Zm9v=",        // pad on a full group
            b"Zh==",         // nonzero trailing bits
            b"Zm9=",         // nonzero trailing bits
            b"Zm9v\n",       // whitespace
            b"Zm-_",         // URL alphabet
            b"=",
            b"====",
        ] {
            assert_eq!(base64_decode(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn token_shape() {
        let t = generate_token().unwrap();
        assert_eq!(t.len(), 32);
        assert!(
            t.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_ne!(t, generate_token().unwrap());
        assert_eq!(
            token_from_bytes(&[0xfb; 24]),
            "-_v7-_v7-_v7-_v7-_v7-_v7-_v7-_v7"
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("nvim"), "nvim");
        assert_eq!(shell_quote("/usr/bin/nvim"), "/usr/bin/nvim");
        assert_eq!(shell_quote("-S"), "-S");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("FOO=bar"), "'FOO=bar'");
        assert_eq!(shell_quote("~"), "'~'");
        assert_eq!(shell_quote("é"), "'é'");
        assert_eq!(
            shell_command(&["a".into(), "b c".into(), String::new()]),
            "a 'b c' ''"
        );
    }

    #[test]
    fn ignored_without_effect() {
        let p = procs();
        let mut s = store();
        let good = sign(&argv_prefix(100, NOW, &["nvim"]));
        assert!(matches!(
            p.run(&mut s, &good, NOW),
            Outcome::Accepted { .. }
        ));
        let before = s.clone();
        let mut cases: Vec<Vec<u8>> = vec![b"?".to_vec(), b"".to_vec()];
        // wrong mac, wrong token, uppercase hex, mac not last, no mac
        let mut bad = good.clone();
        *bad.last_mut().unwrap() ^= 1;
        cases.push(bad);
        let prefix = "pid=100:time=1795543200:clear=1";
        let mac = hex(&sha256(&[prefix.as_bytes(), b":", b"other-token"]));
        cases.push(format!("{prefix}:mac={mac}").into_bytes());
        cases.push(
            String::from_utf8(sign(prefix))
                .unwrap()
                .to_uppercase()
                .into_bytes(),
        );
        let mut moved = String::from_utf8(sign(prefix)).unwrap();
        moved.push_str(":x=1");
        cases.push(moved.into_bytes());
        cases.push(prefix.as_bytes().to_vec());
        // time: missing, zero, leading zero, non numeric, overflow, outside window
        for t in [
            "",
            "time=0",
            "time=01795543200",
            "time=abc",
            "time=99999999999999999999999",
        ] {
            let pre = format!("pid=100:{t}:clear=1");
            cases.push(sign(pre.trim_matches(':')));
            cases.push(sign(&pre.replace("::", ":")));
        }
        cases.push(sign(&format!("pid=100:time={}:clear=1", NOW - 61)));
        cases.push(sign(&format!("pid=100:time={}:clear=1", NOW + 61)));
        // too long
        cases.push(sign(&format!(
            "pid=100:time={NOW}:clear=1:x={}",
            "a".repeat(MAX_BODY)
        )));
        for c in cases {
            assert_eq!(
                p.run(&mut s, &c, NOW),
                Outcome::Ignored,
                "{}",
                String::from_utf8_lossy(&c)
            );
            assert_eq!(s, before);
        }
    }

    #[test]
    fn ignored_reports_do_no_lookups() {
        use std::cell::Cell;
        let p = procs();
        let mut s = store();
        for pid in 1000..1064 {
            let b = sign(&argv_prefix(pid, NOW, &["x"]));
            let seen = Cell::new(0);
            let l = |q: u32| {
                seen.set(seen.get() + 1);
                Some(info(1, u64::from(q)))
            };
            assert!(matches!(
                s.report(&b, NOW, UID, &l),
                Outcome::Accepted { .. }
            ));
        }
        assert_eq!(s.records().len(), 64);
        let fresh = sign(&argv_prefix(100, NOW, &["nvim"]));
        assert!(matches!(
            p.run(&mut s, &fresh, NOW),
            Outcome::Accepted { .. }
        ));
        let mut bad_mac = fresh.clone();
        *bad_mac.last_mut().unwrap() ^= 1;
        let cases = [
            b"?".to_vec(),
            bad_mac,
            sign(&argv_prefix(100, NOW - 61, &["nvim"])),
            sign(&argv_prefix(100, NOW + 61, &["nvim"])),
            sign(&argv_prefix(100, NOW - 1, &["nvim"])),
        ];
        let lookups = Cell::new(0);
        let l = |q: u32| {
            lookups.set(lookups.get() + 1);
            p.0.get(&q).cloned()
        };
        for c in &cases {
            assert_eq!(s.report(c, NOW, UID, &l), Outcome::Ignored);
        }
        assert_eq!(lookups.get(), 0);
    }

    #[test]
    fn time_window_edges() {
        let p = procs();
        for (t, ok) in [
            (NOW - 60, true),
            (NOW + 60, true),
            (NOW - 61, false),
            (NOW + 61, false),
        ] {
            let mut s = store();
            let o = p.run(&mut s, &sign(&argv_prefix(100, t, &["nvim"])), NOW);
            assert_eq!(matches!(o, Outcome::Accepted { .. }), ok, "{t}");
        }
    }

    #[test]
    fn mac_covers_the_exact_bytes() {
        let p = procs();
        let mut s = store();
        // Same text with an unknown key is a different body and verifies.
        let body = sign(&format!("{}:future=zz", argv_prefix(100, NOW, &["nvim"])));
        assert!(matches!(
            p.run(&mut s, &body, NOW),
            Outcome::Accepted { .. }
        ));
        // Tamper with the prefix, keep the mac.
        let mut t = body.clone();
        t[5] = b'9';
        assert_eq!(p.run(&mut s, &t, NOW), Outcome::Ignored);
        assert_eq!(s.records().len(), 1);
    }

    #[test]
    fn monotonic_time_per_pid() {
        let p = procs();
        let mut s = store();
        assert!(matches!(
            p.run(
                &mut s,
                &sign(&argv_prefix(100, NOW + 5, &["nvim", "a"])),
                NOW
            ),
            Outcome::Accepted { .. }
        ));
        // Earlier: ignored, record kept.
        assert_eq!(
            p.run(
                &mut s,
                &sign(&argv_prefix(100, NOW + 4, &["nvim", "b"])),
                NOW
            ),
            Outcome::Ignored
        );
        assert_eq!(
            p.run(
                &mut s,
                &sign(&format!("pid=100:time={}:clear=1", NOW + 4)),
                NOW
            ),
            Outcome::Ignored
        );
        assert_eq!(s.records()[0].argv, ["nvim", "a"]);
        // Equal is fine.
        assert!(matches!(
            p.run(
                &mut s,
                &sign(&argv_prefix(100, NOW + 5, &["nvim", "c"])),
                NOW
            ),
            Outcome::Accepted { changed: true }
        ));
        // Another pid is independent.
        assert!(matches!(
            p.run(&mut s, &sign(&argv_prefix(300, NOW, &["vim"])), NOW),
            Outcome::Accepted { .. }
        ));
        // clear updates the time too.
        assert_eq!(
            p.run(
                &mut s,
                &sign(&format!("pid=100:time={}:clear=1", NOW + 8)),
                NOW
            ),
            Outcome::Cleared { changed: true }
        );
        assert_eq!(
            p.run(&mut s, &sign(&argv_prefix(100, NOW + 7, &["nvim"])), NOW),
            Outcome::Ignored
        );
        assert_eq!(s.records().len(), 1);
    }

    #[test]
    fn last_times_are_capped_independently() {
        let p = Procs((1..=300).map(|i| (i, info(1, 1))).collect());
        let mut s = store();
        for pid in 1..=300u32 {
            assert!(matches!(
                p.run(&mut s, &sign(&format!("pid={pid}:time={NOW}:clear=1")), NOW),
                Outcome::Cleared { .. }
            ));
        }
        assert_eq!(s.last_times().len(), MAX_LAST_TIMES);
        assert_eq!(s.last_times()[0].0, 45);
        assert_eq!(s.last_times()[255].0, 300);
        assert_eq!(s.records(), []);
    }

    #[test]
    fn clear_removes_the_record() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        p.run(&mut s, &sign(&argv_prefix(300, NOW, &["vim"])), NOW);
        assert_eq!(
            p.run(&mut s, &sign(&format!("pid=100:time={NOW}:clear=1")), NOW),
            Outcome::Cleared { changed: true }
        );
        assert_eq!(s.records().len(), 1);
        assert_eq!(s.records()[0].pid, 300);
        // Nothing left to clear.
        assert_eq!(
            p.run(&mut s, &sign(&format!("pid=100:time={NOW}:clear=1")), NOW),
            Outcome::Cleared { changed: false }
        );
        // A dead owner's clear drops its record but is rejected.
        let gone = Procs(HashMap::new());
        assert_eq!(
            gone.run(&mut s, &sign(&format!("pid=300:time={NOW}:clear=1")), NOW),
            Outcome::Rejected { changed: true }
        );
    }

    #[test]
    fn clear_of_a_dead_owner_does_not_advance_the_time() {
        let gone = Procs(HashMap::new());
        let mut s = store();
        let late = NOW + 5;
        gone.run(&mut s, &sign(&format!("pid=100:time={late}:clear=1")), late);
        assert_eq!(s.last_times(), []);
    }

    fn assert_rejected(prefix: &str) {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW - 1, &["nvim"])), NOW);
        p.run(&mut s, &sign(&argv_prefix(300, NOW - 1, &["vim"])), NOW);
        let o = p.run(&mut s, &sign(prefix), NOW);
        assert_eq!(
            o,
            Outcome::Rejected {
                changed: prefix.contains("pid=100")
            },
            "{prefix}"
        );
        assert!(!s.records().iter().any(|r| r.pid == 100), "{prefix}");
        assert!(s.records().iter().any(|r| r.pid == 300), "{prefix}");
    }

    #[test]
    fn rejects_remove_the_owners_records() {
        let a = b64("nvim");
        let t = format!("pid=100:time={NOW}");
        let long: Vec<String> = (0..129).map(|_| a.clone()).collect();
        let ok128: Vec<String> = (0..128).map(|_| a.clone()).collect();
        for pre in [
            format!("{t}:argv={a}:clear=1"),                // both
            t.clone(),                                      // neither
            format!("{t}:argv={a}:argv={a}"),               // duplicate
            format!("{t}:pid=100:argv={a}"),                // duplicate pid
            format!("{t}:argv={a}:bad"),                    // no =
            format!("{t}:argv={a}:=x"),                     // empty key
            format!("{t}:argv={a}:Key=x"),                  // bad key
            format!("{t}:argv={a}:k=a b"),                  // bad value
            format!("{t}:argv={a}:k=é"),                    // bad value
            format!("{t}:argv={a}::k=1"),                   // empty pair
            format!("{t}:clear=2"),                         // clear value
            format!("{t}:clear="),                          // clear empty
            format!("{t}:argv="),                           // empty first element
            format!("{t}:argv=,{a}"),                       // empty first element
            format!("{t}:argv={}", long.join(",")),         // 129
            format!("{t}:argv={a}:need=argv,future"),       // unknown need
            format!("{t}:argv={a}:need="),                  // empty need
            format!("{t}:argv={}", b64("a") + "=="),        // bad padding
            format!("{t}:argv=Zh=="),                       // trailing bits
            format!("{t}:argv=!!!!"),                       // not base64
            format!("{t}:argv={}", b64("/bin/x\n")),        // newline
            format!("{t}:argv={}", b64("/bin/x\u{7f}")),    // DEL
            format!("{t}:argv={}", b64("/bin/x\u{85}")),    // C1
            format!("{t}:argv={}", b64("/bin/x\0")),        // NUL
            format!("{t}:argv={a},{}", b64("a\u{1b}[31m")), // ESC in arg
            format!("{t}:argv=/w=="),                       // invalid UTF-8 (0xFF)
            format!("{t}:argv={}", b64("bin/nvim")),        // relative
            format!("{t}:argv={}", b64("./nvim")),          // relative
            format!("{t}:argv={}", b64("../nvim")),         // relative
            format!("{t}:argv={}", b64("-nvim")),           // dash bare
            format!("{t}:argv={}", b64("-")),               // dash bare
            format!("{t}:argv={a}:cwd={}", b64("rel/dir")), // relative cwd
            format!("{t}:argv={a}:cwd="),                   // empty cwd
            format!("{t}:argv={a}:cwd={}", b64("/a\nb")),   // control in cwd
        ] {
            assert_rejected(&pre);
        }
        let mut s = store();
        let body = sign(&format!("{t}:argv={}", ok128.join(",")));
        assert!(matches!(
            procs().run(&mut s, &body, NOW),
            Outcome::Accepted { .. }
        ));
        assert_eq!(s.records()[0].argv.len(), 128);
    }

    #[test]
    fn rejects_for_the_owner() {
        let mut p = procs();
        p.0.insert(
            400,
            ProcessInfo {
                uid: UID + 1,
                ..info(1, 1)
            },
        );
        let mut s = store();
        // Dead owner, other user's owner: rejected, records for it removed.
        for pid in [999, 400] {
            let o = p.run(&mut s, &sign(&argv_prefix(pid, NOW, &["nvim"])), NOW);
            assert_eq!(o, Outcome::Rejected { changed: false });
            assert_eq!(s.records(), []);
        }
        // Previously accepted, then dies: the next report removes the record.
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        p.0.remove(&100);
        let o = p.run(&mut s, &sign(&argv_prefix(100, NOW + 1, &["nvim"])), NOW);
        assert_eq!(o, Outcome::Rejected { changed: true });
        assert_eq!(s.records(), []);
    }

    #[test]
    fn no_usable_pid_means_no_owner() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW - 1, &["nvim"])), NOW);
        let a = b64("nvim");
        for pre in [
            format!("time={NOW}:argv={a}"),
            format!("pid=0:time={NOW}:argv={a}"),
            format!("pid=0100:time={NOW}:argv={a}"),
            format!("pid=abc:time={NOW}:argv={a}"),
            format!("pid=99999999999999999999:time={NOW}:argv={a}"),
            format!("pid=4294967295:time={NOW}:argv={a}"),
            format!("pid=100:pid=300:time={NOW}:argv={a}"),
        ] {
            assert_eq!(
                p.run(&mut s, &sign(&pre), NOW),
                Outcome::Rejected { changed: false },
                "{pre}"
            );
            assert_eq!(s.records().len(), 1);
        }
    }

    #[test]
    fn accepts_bare_and_absolute_commands() {
        let p = procs();
        for argv in [
            vec!["nvim"],
            vec!["/usr/bin/nvim", "-S", "/x y"],
            vec!["nvim", "", "-", "a/b", "é"],
            vec!["a-b_c.d"],
        ] {
            let mut s = store();
            let o = p.run(&mut s, &sign(&argv_prefix(100, NOW, &argv)), NOW);
            assert_eq!(o, Outcome::Accepted { changed: true }, "{argv:?}");
            assert_eq!(s.records()[0].argv, argv);
        }
    }

    #[test]
    fn unknown_keys_and_need() {
        let p = procs();
        let a = b64("nvim");
        let mut s = store();
        let o = p.run(
            &mut s,
            &sign(&format!("pid=100:time={NOW}:argv={a}:zzz=1.2,x:future=")),
            NOW,
        );
        assert!(matches!(o, Outcome::Accepted { .. }));
        for need in ["pid", "argv,cwd", "pid,time,mac,argv,cwd,clear,need"] {
            let mut s = store();
            let o = p.run(
                &mut s,
                &sign(&format!("pid=100:time={NOW}:argv={a}:need={need}")),
                NOW,
            );
            assert!(matches!(o, Outcome::Accepted { .. }), "{need}");
        }
        // Key order is free.
        let o = p.run(&mut s, &sign(&format!("argv={a}:time={NOW}:pid=100")), NOW);
        assert_eq!(o, Outcome::Accepted { changed: false });
    }

    #[test]
    fn cwd_from_report_else_os() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        assert_eq!(s.records()[0].cwd.as_deref(), Some("/os/cwd"));
        let pre = format!(
            "{}:cwd={}",
            argv_prefix(100, NOW, &["nvim"]),
            b64("/home/me é")
        );
        assert_eq!(
            p.run(&mut s, &sign(&pre), NOW),
            Outcome::Accepted { changed: true }
        );
        assert_eq!(s.records()[0].cwd.as_deref(), Some("/home/me é"));
        assert_eq!(s.records()[0].start_time, 10);
    }

    #[test]
    fn record_cap_evicts_the_oldest() {
        let p = Procs((1..=70).map(|i| (i, info(1, 1))).collect());
        let mut s = store();
        for pid in 1..=70u32 {
            p.run(&mut s, &sign(&argv_prefix(pid, NOW, &["nvim"])), NOW);
        }
        assert_eq!(s.records().len(), MAX_RECORDS);
        assert_eq!(s.records()[0].pid, 7);
        assert_eq!(s.records()[63].pid, 70);
    }

    #[test]
    fn prune_drops_dead_and_reused_pids() {
        let mut p = procs();
        let mut s = store();
        for pid in [100, 200, 300] {
            p.run(&mut s, &sign(&argv_prefix(pid, NOW, &["x"])), NOW);
        }
        p.0.remove(&200);
        p.0.get_mut(&300).unwrap().start_time = 31;
        assert!(s.prune(&|q| p.0.get(&q).cloned()));
        assert_eq!(s.records().iter().map(|r| r.pid).collect::<Vec<_>>(), [100]);
        assert!(!s.prune(&|q| p.0.get(&q).cloned()));
    }

    #[test]
    fn ancestor_beats_descendant_else_most_recent() {
        let p = procs();
        let lookup = |q: u32| p.0.get(&q).cloned();
        let mut s = store();
        // Parent 100 reports first, child 200 later: parent wins.
        p.run(
            &mut s,
            &sign(&argv_prefix(100, NOW, &["parent", "a b"])),
            NOW,
        );
        p.run(&mut s, &sign(&argv_prefix(200, NOW, &["child"])), NOW);
        assert_eq!(s.pid(&lookup), Some(100));
        assert_eq!(s.command(&lookup).as_deref(), Some("parent 'a b'"));
        assert_eq!(s.cwd(&lookup).as_deref(), Some("/os/cwd"));
        // An unrelated record reported later is most recent among roots.
        p.run(&mut s, &sign(&argv_prefix(300, NOW, &["other"])), NOW);
        assert_eq!(s.pid(&lookup), Some(300));
        // Parent re-reports: now most recent root.
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["parent"])), NOW);
        assert_eq!(s.pid(&lookup), Some(100));
        assert_eq!(store().pid(&lookup), None);
        assert_eq!(store().command(&lookup), None);
    }

    #[test]
    fn rebuild_and_reset() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        let mut t = ResumeStore::from_parts(
            s.token().into(),
            s.records().to_vec(),
            s.last_times().to_vec(),
        );
        assert_eq!(t, s);
        assert_eq!(
            p.run(&mut t, &sign(&argv_prefix(100, NOW - 1, &["x"])), NOW),
            Outcome::Ignored
        );
        p.run(&mut t, &sign(&argv_prefix(300, NOW, &["vim"])), NOW);
        assert_eq!(t.records()[1].seq, 1);
        t.reset("new-token-new-token-new-token".into());
        assert!(t.records().is_empty() && t.last_times().is_empty());
        // The old token no longer verifies.
        assert_eq!(
            p.run(&mut t, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW),
            Outcome::Ignored
        );
    }

    fn sign_bytes(prefix: &[u8]) -> Vec<u8> {
        let mac = hex(&sha256(&[prefix, b":", TOKEN.as_bytes()]));
        let mut v = prefix.to_vec();
        v.extend_from_slice(format!(":mac={mac}").as_bytes());
        v
    }

    #[test]
    fn early_mac_key_rejects() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        let pre = format!("{}:mac=abc", argv_prefix(100, NOW, &["x"]));
        assert_eq!(
            p.run(&mut s, &sign(&pre), NOW),
            Outcome::Rejected { changed: true }
        );
        assert_eq!(s.records(), []);
    }

    #[test]
    fn signed_non_utf8_pair_rejects() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        let mut pre = format!("pid=100:time={NOW}:argv=bnZpbQ==:cwd=").into_bytes();
        pre.push(0xff);
        assert_eq!(
            p.run(&mut s, &sign_bytes(&pre), NOW),
            Outcome::Rejected { changed: true }
        );
        assert_eq!(s.records(), []);
    }

    #[test]
    fn rejected_report_does_not_advance_time() {
        let p = procs();
        let mut s = store();
        let bad = sign(&format!("pid=100:time={}:argv=!!", NOW + 10));
        assert!(matches!(p.run(&mut s, &bad, NOW), Outcome::Rejected { .. }));
        assert_eq!(s.last_times(), []);
        let ok = sign(&argv_prefix(100, NOW, &["nvim"]));
        assert_eq!(p.run(&mut s, &ok, NOW), Outcome::Accepted { changed: true });
    }

    #[test]
    fn eviction_keeps_pids_with_records() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        let many = Procs((1000..1000 + 266).map(|pid| (pid, info(1, 10))).collect());
        for pid in 1000..1000 + 266 {
            many.run(&mut s, &sign(&format!("pid={pid}:time={NOW}:clear=1")), NOW);
        }
        assert!(s.last_times().iter().any(|&(p, _)| p == 100));
        assert_eq!(s.last_times().len(), MAX_LAST_TIMES);
        assert_eq!(
            p.run(&mut s, &sign(&argv_prefix(100, NOW - 1, &["x"])), NOW),
            Outcome::Ignored
        );
    }

    #[test]
    fn promotion_to_winner_is_a_change() {
        let p = procs();
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW);
        p.run(&mut s, &sign(&argv_prefix(300, NOW, &["vim"])), NOW);
        assert_eq!(
            p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW),
            Outcome::Accepted { changed: true }
        );
        assert_eq!(
            p.run(&mut s, &sign(&argv_prefix(100, NOW, &["nvim"])), NOW),
            Outcome::Accepted { changed: false }
        );
    }

    #[test]
    fn deep_ancestry_and_cycles() {
        let mut m = HashMap::new();
        for i in 1..=200u32 {
            m.insert(i, info(i + 1, 5));
        }
        m.insert(201, info(0, 5));
        let p = Procs(m);
        let mut s = store();
        p.run(&mut s, &sign(&argv_prefix(201, NOW, &["top"])), NOW);
        p.run(&mut s, &sign(&argv_prefix(1, NOW, &["leaf"])), NOW);
        assert_eq!(s.pid(&|q| p.0.get(&q).cloned()), Some(201));
        // A cycle terminates.
        let c = Procs([(1, info(2, 5)), (2, info(1, 5))].into());
        assert!(s.pid(&|q| c.0.get(&q).cloned()).is_some());
    }

    #[test]
    fn reserved_word_argv0_is_quoted() {
        for w in [
            "if", "then", "done", "time", "{", "}", "!", "[[", "function",
        ] {
            let c = shell_command(&[w.to_string(), "x".into()]);
            assert_eq!(c, format!("'{w}' x"));
        }
        assert_eq!(shell_command(&["ls".into(), "if".into()]), "ls if");
    }

    #[test]
    fn module_has_no_unsafe() {
        let src = include_str!("program_resume.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        assert!(!code.contains(&["un", "safe"].concat()));
    }
}
