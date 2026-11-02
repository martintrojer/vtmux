//! Streaming basics from tty-keys.c (`tty_keys_next`/`tty_keys_next1`).
//! Unknown terminal reports never become pane keys. Extended keys and mouse
//! are deferred to M1; the decoder bounds incomplete control strings.
use crate::keys::{Base, Key, Named};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Paste(bool),
    Sync(bool),
    /// `KEYC_FOCUS_IN` / `KEYC_FOCUS_OUT` (`CSI I` / `CSI O`).
    Focus(bool),
    /// An OSC 52 clipboard reply (`tty_keys_clipboard`), decoded; what a
    /// `refresh-client -l` asked for.
    Clipboard(Vec<u8>),
    /// The terminal's replies to the startup queries, each only while it
    /// is awaited: primary device attributes (`CSI ? Ps ; ... c`) with
    /// their parameters, secondary ones (`CSI > Pp ; ... c`) with the
    /// first, and XTVERSION (`DCS > | text ST`).
    DeviceAttributes(Vec<u32>),
    DeviceAttributes2(u32),
    ExtendedDeviceAttributes(String),
    Ignore,
}

/// The startup queries a reply can answer (`TTY_HAVEDA`, `TTY_HAVEDA2`,
/// `TTY_HAVEXDA`, `TTY_HAVESYNC`).
const QUERY_DA: u8 = 1;
const QUERY_DA2: u8 = 2;
const QUERY_XDA: u8 = 4;
const QUERY_SYNC: u8 = 8;
const ALL_QUERIES: u8 = QUERY_DA | QUERY_DA2 | QUERY_XDA | QUERY_SYNC;
/// `escape-time` default (options-table.c).
const ESCAPE_TIME: Duration = Duration::from_millis(10);
/// The partial-key floor while a reply may still be in flight
/// (tty-keys.c:996-1012).
const REPLY_DELAY: Duration = Duration::from_millis(500);
/// `TTY_QUERY_TIMEOUT` (tty.c:85): queries unanswered by then are given up.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
    pending: Option<Instant>,
    /// The terminal was queried and has not answered before this instant
    /// (`TTY_ALL_REQUEST_FLAGS` not all set, tty.c `tty_start_timer_callback`).
    queries_until: Option<Instant>,
    /// The startup queries answered, or given up on (`QUERY_*` bits, the
    /// `TTY_HAVE*` flags).
    answered: u8,
    /// Inside a bracketed paste (`TTY_BRACKETPASTE`).
    pasting: bool,
    /// The terminal's `khome`/`kend` are `ESC [1~`/`ESC [4~`.
    tilde_home_end: bool,
}
impl Decoder {
    /// The startup queries went to the terminal (DA, DA2, XTVERSION and
    /// `DECRQM 2026`, tty.c `tty_send_requests`); their replies may arrive
    /// split, so partial sequences wait longer until all are answered or
    /// `QUERY_TIMEOUT` passes.
    pub fn query_sent(&mut self, now: Instant) {
        self.queries_until = Some(now + QUERY_TIMEOUT);
        self.answered = 0;
    }
    /// When the start timer fires, while queries are unanswered.
    #[must_use]
    pub fn start_deadline(&self) -> Option<Instant> {
        self.queries_until.filter(|_| self.answered != ALL_QUERIES)
    }
    /// The start timer fired (`tty_start_timer_callback`): later replies
    /// are no longer taken as answers. Whether none of DA, DA2 and
    /// XTVERSION had been answered.
    pub fn queries_expired(&mut self) -> bool {
        let none = self.answered & (QUERY_DA | QUERY_DA2 | QUERY_XDA) == 0;
        self.queries_until = None;
        self.answered = ALL_QUERIES;
        none
    }
    /// The client's terminal is known (`tty_keys_build` with its terminfo).
    pub fn set_terminal(&mut self, term: &vtmux_core::term_features::Term) {
        self.tilde_home_end = term.tilde_home_end();
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        let start = self.pending?;
        let waiting = self.queries_until.is_some_and(|until| start < until)
            || (self.pasting && b"\x1b[201~".starts_with(&self.bytes) && self.bytes.len() < 6);
        Some(start + if waiting { REPLY_DELAY } else { ESCAPE_TIME })
    }
    pub fn drain(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        while !self.bytes.is_empty() {
            let expired = self.deadline().is_some_and(|t| now >= t);
            let Some((mut event, used)) = decode(&self.bytes, expired, self.tilde_home_end) else {
                self.pending.get_or_insert(now);
                break;
            };
            self.bytes.drain(..used);
            self.pending = None;
            let answers = match event {
                Event::Sync(_) => QUERY_SYNC,
                Event::DeviceAttributes(_) => QUERY_DA,
                Event::DeviceAttributes2(_) => QUERY_DA2,
                Event::ExtendedDeviceAttributes(_) => QUERY_XDA,
                Event::Paste(start) => {
                    self.pasting = start;
                    0
                }
                _ => 0,
            };
            if answers != 0 {
                // A second reply is swallowed and changes nothing (each
                // tty_keys_* parser returns -1 once its TTY_HAVE* flag is
                // set); partial keys wait until all four are in.
                if self.answered & answers != 0 {
                    event = Event::Ignore;
                }
                self.answered |= answers;
                if self.answered == ALL_QUERIES {
                    self.queries_until = None;
                }
            }
            events.push(event);
        }
        events
    }
}
fn key(base: Base) -> Key {
    Key {
        base,
        ctrl: false,
        meta: false,
        shift: false,
        esc_prefix: false,
    }
}
/// `tty_keys_clipboard` (tty-keys.c:1394-1445): the body after `ESC ] 52 ;`
/// is the clipboard name, `;` and base64 text; no text, or text that does
/// not decode, is no event.
fn clipboard(body: &[u8]) -> Event {
    let Some(at) = body.iter().position(|&b| b == b';') else {
        return Event::Ignore;
    };
    let text = &body[at + 1..];
    if text.len() <= 1 {
        return Event::Ignore;
    }
    base64_decode(text).map_or(Event::Ignore, Event::Clipboard)
}

/// `b64_pton`: padded standard base64.
fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut pad = 0;
    for &b in text {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            _ => return None,
        };
        if pad > 0 {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    (pad <= 2 && text.len().is_multiple_of(4)).then_some(out)
}

#[allow(clippy::too_many_lines)] // terminal sequence dispatch table
fn decode(bytes: &[u8], expired: bool, tilde_home_end: bool) -> Option<(Event, usize)> {
    if bytes[0] != 0x1b {
        return character(bytes, expired).map(|(k, n)| (Event::Key(k), n));
    }
    if bytes.len() == 1 {
        return expired.then(|| (Event::Key(key(Base::Char('\x1b'))), 1));
    }
    // OSC colour/clipboard and DCS version replies (tty-keys.c
    // tty_keys_clipboard, tty_keys_colours, tty_keys_palette,
    // tty_keys_extended_device_attributes) start with these; anything else
    // after Escape is a key with Meta (`ESC P` is M-P).
    let reply = [
        &b"\x1bP>|"[..],
        b"\x1b]52;",
        b"\x1b]10;",
        b"\x1b]11;",
        b"\x1b]4;",
    ]
    .into_iter()
    .find(|p| p.starts_with(&bytes[..bytes.len().min(p.len())]));
    if let Some(prefix) = reply.filter(|p| bytes.len() >= p.len()) {
        for i in prefix.len()..bytes.len() {
            let end = bytes[i] == 7 || (bytes[i] == b'\\' && bytes[i - 1] == 0x1b);
            if end {
                let body_end = if bytes[i] == 7 { i } else { i - 1 };
                let body = &bytes[prefix.len()..body_end];
                let event = match prefix {
                    b"\x1b]52;" => clipboard(body),
                    // tty_keys_extended_device_attributes: the text up to
                    // ST (tmp[128]); BEL does not end it there, but no
                    // terminal sends one.
                    b"\x1bP>|" if bytes[i] == b'\\' && body.len() < 127 => {
                        Event::ExtendedDeviceAttributes(String::from_utf8_lossy(body).into_owned())
                    }
                    _ => Event::Ignore,
                };
                return Some((event, i + 1));
            }
        }
        if bytes.len() > 4096 || expired {
            return Some((Event::Ignore, bytes.len()));
        }
        return None;
    }
    if reply.is_some() && !expired {
        return None;
    }
    // The key tree (tty-keys.c tty_keys_find): a whole sequence is that
    // key; the start of one waits for more until escape-time.
    for (seq, event) in [
        (&b"\x1b[I"[..], Event::Focus(true)),
        (b"\x1b[O", Event::Focus(false)),
        (b"\x1b[200~", Event::Paste(true)),
        (b"\x1b[201~", Event::Paste(false)),
    ] {
        if bytes.starts_with(seq) {
            return Some((event, seq.len()));
        }
    }
    let table = key_table(tilde_home_end);
    if let Some((seq, k)) = table.iter().find(|(seq, _)| bytes.starts_with(seq)) {
        return Some((Event::Key(*k), seq.len()));
    }
    if !expired
        && (table.iter().any(|(seq, _)| seq.starts_with(bytes))
            || [&b"\x1b[200~"[..], b"\x1b[201~"]
                .iter()
                .any(|s| s.starts_with(bytes)))
    {
        return None;
    }
    if bytes[1] == b'[' {
        let final_index = bytes
            .iter()
            .enumerate()
            .skip(2)
            .find(|(_, b)| (0x40..=0x7e).contains(*b))
            .map(|(i, _)| i);
        if let Some(end) = final_index {
            let final_byte = bytes[end];
            let body = std::str::from_utf8(&bytes[2..end]).unwrap_or("");
            if body.starts_with('?')
                || body.starts_with('>')
                || body.starts_with('<')
                || matches!(final_byte, b'c' | b't' | b'n' | b'y')
            {
                if final_byte == b'y' && body.starts_with("?2026;") {
                    return Some((
                        Event::Sync(matches!(body.as_bytes().get(6), Some(b'1' | b'2' | b'3'))),
                        end + 1,
                    ));
                }
                return Some((device_attributes(body, final_byte), end + 1));
            }
            if let Some(k) = extended_key(body, final_byte) {
                return Some((Event::Key(k), end + 1));
            }
        } else if !expired {
            return None;
        }
    }
    // tty_keys_next (tty-keys.c:900-924): Escape followed by a key is that
    // key with Meta. If the key already carries an xterm Meta modifier (or
    // is a paste marker), the Escape is a key of its own.
    let (event, n) = decode(&bytes[1..], expired, tilde_home_end)?;
    let event = match event {
        Event::Key(k) if k.meta && !k.esc_prefix => {
            return Some((Event::Key(key(Base::Char('\x1b'))), 1));
        }
        Event::Paste(_) => return Some((Event::Key(key(Base::Char('\x1b'))), 1)),
        Event::Key(mut k) => {
            k.esc_prefix = true;
            k.meta = true;
            Event::Key(k)
        }
        other => other,
    };
    Some((event, n + 1))
}
/// `tty_keys_device_attributes` and `tty_keys_device_attributes2`
/// (tty-keys.c:1467-1670): the parameters of `CSI ? ... c` or `CSI > ...
/// c`, each `strtoul`'d (0 if not all digits), at most 32.
fn device_attributes(body: &str, final_byte: u8) -> Event {
    if final_byte != b'c' {
        return Event::Ignore;
    }
    let params = |rest: &str| -> Vec<u32> {
        rest.split(';')
            .take(32)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    if let Some(rest) = body.strip_prefix('?') {
        Event::DeviceAttributes(params(rest))
    } else if let Some(rest) = body.strip_prefix('>') {
        Event::DeviceAttributes2(params(rest).first().copied().unwrap_or(0))
    } else {
        Event::Ignore
    }
}

/// A table row: the key with its modifiers.
fn row(base: Base, ctrl: bool, meta: bool, shift: bool) -> Key {
    Key {
        base,
        ctrl,
        meta,
        shift,
        esc_prefix: false,
    }
}

/// The key tree `tty_keys_build` makes: `tty_default_xterm_keys` with each
/// modifier digit 2-9, `tty_default_raw_keys`, then the terminfo keys
/// (`tty_default_code_keys`) of the terminals vtmux knows, which only
/// differ in `khome`/`kend` (and agree with the rows above elsewhere).
type KeyTable = Vec<(Vec<u8>, Key)>;

fn key_table(tilde_home_end: bool) -> &'static [(Vec<u8>, Key)] {
    static TABLES: std::sync::OnceLock<[KeyTable; 2]> = std::sync::OnceLock::new();
    &TABLES.get_or_init(|| [build_table(false), build_table(true)])[usize::from(tilde_home_end)]
}

#[expect(clippy::too_many_lines, reason = "tty-keys.c's key tables")]
fn build_table(tilde_home_end: bool) -> KeyTable {
    use Named::{
        BackTab, Delete, Down, End, F, Home, Insert, Keypad, Left, PageDown, PageUp, Right, Up,
    };
    let named = |n| Base::Named(n);
    let mut t: Vec<(Vec<u8>, Key)> = Vec::new();
    let mut add = |seq: &str, k: Key| {
        let seq = seq.as_bytes().to_vec();
        match t.iter_mut().find(|(s, _)| *s == seq) {
            Some(entry) => entry.1 = k,
            None => t.push((seq, k)),
        }
    };
    // tty_default_xterm_keys (tty-keys.c:222-253) with
    // tty_default_xterm_modifiers: 2 S, 3 M, 4 S-M, 5 C, 6 S-C, 7 M-C,
    // 8 S-M-C, 9 M; the Meta is KEYC_IMPLIED_META.
    let xterm: [(&str, Named); 30] = [
        ("\x1b[1;_P", F(1)),
        ("\x1bO1;_P", F(1)),
        ("\x1bO_P", F(1)),
        ("\x1b[1;_Q", F(2)),
        ("\x1bO1;_Q", F(2)),
        ("\x1bO_Q", F(2)),
        ("\x1b[1;_R", F(3)),
        ("\x1bO1;_R", F(3)),
        ("\x1bO_R", F(3)),
        ("\x1b[1;_S", F(4)),
        ("\x1bO1;_S", F(4)),
        ("\x1bO_S", F(4)),
        ("\x1b[15;_~", F(5)),
        ("\x1b[17;_~", F(6)),
        ("\x1b[18;_~", F(7)),
        ("\x1b[19;_~", F(8)),
        ("\x1b[20;_~", F(9)),
        ("\x1b[21;_~", F(10)),
        ("\x1b[23;_~", F(11)),
        ("\x1b[24;_~", F(12)),
        ("\x1b[1;_A", Up),
        ("\x1b[1;_B", Down),
        ("\x1b[1;_C", Right),
        ("\x1b[1;_D", Left),
        ("\x1b[1;_H", Home),
        ("\x1b[1;_F", End),
        ("\x1b[5;_~", PageUp),
        ("\x1b[6;_~", PageDown),
        ("\x1b[2;_~", Insert),
        ("\x1b[3;_~", Delete),
    ];
    for (template, n) in xterm {
        for digit in 2u8..=9 {
            let m = if digit == 9 { 2 } else { digit - 1 };
            let k = row(named(n), m & 4 != 0, m & 2 != 0, m & 1 != 0);
            add(
                &template.replace('_', &char::from(b'0' + digit).to_string()),
                k,
            );
        }
    }
    // tty_default_raw_keys (tty-keys.c:81-219).
    add("\x1bO[", key(Base::Char('\x1b')));
    for (i, c) in crate::keys::KEYPAD_SS3.iter().enumerate() {
        let n = u8::try_from(i).expect("16 keypad keys");
        add(&format!("\x1bO{}", char::from(*c)), key(named(Keypad(n))));
    }
    for (c, n) in [('A', Up), ('B', Down), ('C', Right), ('D', Left)] {
        add(&format!("\x1bO{c}"), key(named(n)));
        add(&format!("\x1b[{c}"), key(named(n)));
    }
    // Escape+arrow stays Escape+arrow (no KEYC_IMPLIED_META), but
    // Escape+Home/End are M-Home/M-End.
    for (c, n) in [('A', Up), ('B', Down), ('C', Right), ('D', Left)] {
        let k = Key {
            esc_prefix: true,
            ..row(named(n), false, true, false)
        };
        add(&format!("\x1b\x1bO{c}"), k);
        add(&format!("\x1b\x1b[{c}"), k);
    }
    for (c, n) in [('H', Home), ('F', End)] {
        add(&format!("\x1bO{c}"), key(named(n)));
        add(&format!("\x1b\x1bO{c}"), row(named(n), false, true, false));
        add(&format!("\x1b[{c}"), key(named(n)));
        add(&format!("\x1b\x1b[{c}"), row(named(n), false, true, false));
    }
    // rxvt arrow keys.
    for (c, n) in [('a', Up), ('b', Down), ('c', Right), ('d', Left)] {
        add(&format!("\x1bO{c}"), row(named(n), true, false, false));
        add(&format!("\x1b[{c}"), row(named(n), false, false, true));
    }
    // rxvt function keys.
    let numbers = [
        "11", "12", "13", "14", "15", "17", "18", "19", "20", "21", "23", "24",
    ];
    for (i, num) in numbers.iter().enumerate().take(10) {
        add(
            &format!("\x1b[{num}~"),
            key(named(F(u8::try_from(i + 1).expect("F key")))),
        );
    }
    let shifted = ["23", "24", "25", "26", "28", "29", "31", "32", "33", "34"];
    for (i, num) in shifted.iter().enumerate() {
        let f = F(u8::try_from(i + 1).expect("F key"));
        add(&format!("\x1b[{num}~"), row(named(f), false, false, true));
    }
    add("\x1b[23$", row(named(F(11)), false, false, true));
    add("\x1b[24$", row(named(F(12)), false, false, true));
    for (i, num) in numbers.iter().enumerate() {
        let f = F(u8::try_from(i + 1).expect("F key"));
        add(&format!("\x1b[{num}^"), row(named(f), true, false, false));
        add(&format!("\x1b[{num}@"), row(named(f), true, false, true));
    }
    add("\x1b[1;5Z", row(Base::Char('\t'), true, false, true));
    // tty_default_code_keys from terminfo: kf1-kf12, kich1, kdch1, khome,
    // kend, knp, kpp, kcbt (the arrows and modified keys repeat rows above).
    for (i, seq) in [
        "\x1bOP", "\x1bOQ", "\x1bOR", "\x1bOS", "\x1b[15~", "\x1b[17~", "\x1b[18~", "\x1b[19~",
        "\x1b[20~", "\x1b[21~", "\x1b[23~", "\x1b[24~",
    ]
    .iter()
    .enumerate()
    {
        add(seq, key(named(F(u8::try_from(i + 1).expect("F key")))));
    }
    add("\x1b[2~", key(named(Insert)));
    add("\x1b[3~", key(named(Delete)));
    add("\x1b[6~", key(named(PageDown)));
    add("\x1b[5~", key(named(PageUp)));
    add("\x1b[Z", key(named(BackTab)));
    if tilde_home_end {
        add("\x1b[1~", key(named(Home)));
        add("\x1b[4~", key(named(End)));
    }
    t
}

/// `tty_keys_extended_key` (tty-keys.c:1089-1202): `CSI number;mods u` or
/// `CSI 27;mods;number ~`.
fn extended_key(body: &str, final_byte: u8) -> Option<Key> {
    let parts: Vec<u32> = body
        .split(';')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let (number, modifiers) = match (final_byte, parts.as_slice()) {
        (b'u', [n, m, ..]) | (b'~', [27, m, n, ..]) => (*n, *m),
        _ => return None,
    };
    let base = if number == 0x7f {
        Base::Named(Named::Backspace)
    } else {
        Base::Char(char::from_u32(number)?)
    };
    let m = modifiers.saturating_sub(1);
    let mut k = row(base, m & 4 != 0, m & 0b1010 != 0, m & 1 != 0);
    // S-Tab is BTab.
    if k.base == Base::Char('\t') && k.shift {
        k.base = Base::Named(Named::BackTab);
        k.shift = false;
    }
    // A shifted printable key loses the Shift.
    if let Base::Char(c) = k.base
        && (('\x21'..'\x7f').contains(&c) || !c.is_ascii())
        && k.shift
        && !k.ctrl
        && !k.meta
    {
        k.shift = false;
    }
    Some(k)
}

fn character(bytes: &[u8], expired: bool) -> Option<(Key, usize)> {
    let b = bytes[0];
    if b < 128 {
        let mut k = key(Base::Char(char::from(b)));
        if b == 127 {
            k.base = Base::Named(Named::Backspace);
        } else if b < 32 && !matches!(b, 9 | 13 | 27) {
            k.base = Base::Char(if b == 0 {
                ' '
            } else {
                char::from((b | 0x40).to_ascii_lowercase())
            });
            k.ctrl = true;
        }
        return Some((k, 1));
    }
    let len = match b {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    if bytes.len() < len && !expired {
        return None;
    }
    if let Ok(s) = std::str::from_utf8(bytes.get(..len).unwrap_or(bytes)) {
        return Some((key(Base::Char(s.chars().next()?)), len));
    }
    Some((key(Base::Char(char::REPLACEMENT_CHARACTER)), 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_keys_utf8_reports_and_paste() {
        let now = Instant::now();
        let mut d = Decoder::default();
        d.feed(b"\x1b[");
        assert_eq!(d.drain(now), Vec::new());
        d.feed(b"A");
        assert_eq!(d.drain(now), vec![Event::Key(key(Base::Named(Named::Up)))]);
        d.feed(b"\xe4\xb8");
        assert_eq!(d.drain(now), Vec::new());
        d.feed(b"\x96");
        assert_eq!(d.drain(now), vec![Event::Key(key(Base::Char('世')))]);
        d.feed(b"\x1b[?62;4c\x1b[>84;0;0c\x1b]11;rgb:0/0/0\x1b\\\x1b[?2026;2$y\x1b[200~\x1b[201~");
        assert_eq!(
            d.drain(now),
            vec![
                Event::DeviceAttributes(vec![62, 4]),
                Event::DeviceAttributes2(84),
                Event::Ignore,
                Event::Sync(true),
                Event::Paste(true),
                Event::Paste(false)
            ]
        );
        d.feed(b"\x1b");
        assert_eq!(d.drain(now), Vec::new());
        assert_eq!(
            d.drain(now + Duration::from_millis(11)),
            vec![Event::Key(key(Base::Char('\x1b')))]
        );
    }

    /// tty-keys.c:1394-1445: an OSC 52 reply is its decoded text; a name
    /// with no text is nothing.
    #[test]
    fn clipboard_replies() {
        let mut d = Decoder::default();
        d.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;;\x07\x1b]52;;Zm9v\x1b\\");
        assert_eq!(
            d.drain(Instant::now()),
            vec![
                Event::Clipboard(b"hello".to_vec()),
                Event::Ignore,
                Event::Clipboard(b"foo".to_vec())
            ]
        );
    }

    #[test]
    fn focus_reports_are_events() {
        let mut d = Decoder::default();
        d.feed(b"\x1b[I\x1b[O");
        assert_eq!(
            d.drain(Instant::now()),
            vec![Event::Focus(true), Event::Focus(false)]
        );
    }

    #[test]
    fn split_reply_waits_while_a_query_is_outstanding() {
        let now = Instant::now();
        let ms = Duration::from_millis;
        for reply in [
            &b"\x1b[?2026;2$y"[..],
            b"\x1b[?62;4c",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
        ] {
            let mut d = Decoder::default();
            d.query_sent(now);
            d.feed(&reply[..4]);
            assert_eq!(d.drain(now), Vec::new());
            assert_eq!(d.deadline(), Some(now + ms(500)));
            assert_eq!(d.drain(now + ms(50)), Vec::new());
            d.feed(&reply[4..]);
            let events = d.drain(now + ms(50));
            assert_eq!(events.len(), 1);
            assert!(!matches!(events[0], Event::Key(_)), "{events:?}");
        }
        // All four answered: back to escape-time (TTY_ALL_REQUEST_FLAGS).
        let mut d = Decoder::default();
        d.query_sent(now);
        d.feed(b"\x1b[?2026;2$y\x1b");
        assert_eq!(d.drain(now), vec![Event::Sync(true)]);
        assert_eq!(d.deadline(), Some(now + ms(500)));
        d.feed(b"[?62;4c\x1b[>1;2c\x1bP>|xterm\x1b\\\x1b");
        assert_eq!(
            d.drain(now),
            vec![
                Event::DeviceAttributes(vec![62, 4]),
                Event::DeviceAttributes2(1),
                Event::ExtendedDeviceAttributes("xterm".into()),
            ]
        );
        assert_eq!(d.deadline(), Some(now + ms(10)));
        assert_eq!(d.start_deadline(), None);
        // A second reply changes nothing.
        d.feed(b"[?62;52c");
        assert_eq!(d.drain(now), vec![Event::Ignore]);
        // Unanswered queries time out after TTY_QUERY_TIMEOUT.
        let mut d = Decoder::default();
        d.query_sent(now);
        d.feed(b"\x1b");
        assert_eq!(d.drain(now + ms(5000)), Vec::new());
        assert_eq!(d.deadline(), Some(now + ms(5010)));
        // The start timer (tty_start_timer_callback): due at the same time,
        // it says whether none of DA, DA2 and XTVERSION was answered, and
        // later replies are swallowed.
        let mut d = Decoder::default();
        d.query_sent(now);
        assert_eq!(d.start_deadline(), Some(now + ms(5000)));
        assert!(d.queries_expired());
        assert_eq!(d.start_deadline(), None);
        d.feed(b"\x1b[?62c");
        assert_eq!(d.drain(now), vec![Event::Ignore]);
        let mut d = Decoder::default();
        d.query_sent(now);
        d.feed(b"\x1b[>0c");
        assert_eq!(d.drain(now), vec![Event::DeviceAttributes2(0)]);
        assert!(!d.queries_expired());
        // A partial paste end inside a paste waits too.
        let mut d = Decoder::default();
        d.feed(b"\x1b[200~ab\x1b[20");
        assert_eq!(d.drain(now).len(), 3);
        assert_eq!(d.deadline(), Some(now + ms(500)));
    }

    #[test]
    fn escape_before_a_key_is_escape_plus_key() {
        let now = Instant::now();
        let decode1 = |b: &[u8]| {
            let mut d = Decoder::default();
            d.feed(b);
            d.drain(now + Duration::from_millis(20))
        };
        let meta = |base, esc_prefix| {
            Event::Key(Key {
                meta: true,
                esc_prefix,
                ..key(base)
            })
        };
        assert_eq!(
            decode1(b"\x1b\x1b[A"),
            vec![meta(Base::Named(Named::Up), true)]
        );
        assert_eq!(
            decode1(b"\x1b\x1bOA"),
            vec![meta(Base::Named(Named::Up), true)]
        );
        assert_eq!(
            decode1(b"\x1b\x1b[H"),
            vec![meta(Base::Named(Named::Home), false)]
        );
        assert_eq!(decode1(b"\x1bx"), vec![meta(Base::Char('x'), true)]);
        // Escape before an xterm M-Left is Escape, then M-Left.
        let mut m_left = key(Base::Named(Named::Left));
        m_left.meta = true;
        assert_eq!(
            decode1(b"\x1b\x1b[1;3D"),
            vec![Event::Key(key(Base::Char('\x1b'))), Event::Key(m_left)]
        );
    }

    #[test]
    fn tty_keys_rows_name_like_tmux() {
        let now = Instant::now();
        let name = |b: &[u8], tilde: bool| {
            let mut d = Decoder {
                tilde_home_end: tilde,
                ..Decoder::default()
            };
            d.feed(b);
            d.drain(now + Duration::from_millis(20))
                .into_iter()
                .map(|e| match e {
                    Event::Key(k) => k.to_core().to_string(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        for (seq, want) in [
            (&b"\x1bOM"[..], "KPEnter"),
            (b"\x1bOp", "KP0"),
            (b"\x1bOj", "KP*"),
            (b"\x1bOy", "KP9"),
            (b"\x1b\x1bOp", "M-KP0"),
            (b"\x1bOa", "C-Up"),
            (b"\x1b[d", "S-Left"),
            (b"\x1b[25~", "S-F3"),
            (b"\x1b[24$", "S-F12"),
            (b"\x1b[11^", "C-F1"),
            (b"\x1b[24@", "C-S-F12"),
            (b"\x1b[1;9A", "M-Up"),
            (b"\x1b[123;5u", "C-{"),
            (b"\x1b[32;2u", "S-Space"),
            (b"\x1b[9;5u", "C-Tab"),
            (b"\x1b[1;5Z", "C-S-Tab"),
            (b"\x1b[97;7u", "C-M-a"),
            (b"\x1b\x1b[H", "M-Home"),
            (b"\x1b\x1b[A", "M-Up"),
        ] {
            assert_eq!(name(seq, false), want, "{seq:?}");
        }
        // xterm's terminfo has no ESC [1~ / ESC [4~: Meta-[ then the text.
        assert_eq!(name(b"\x1b[1~", false), "M-[ 1 ~");
        assert_eq!(name(b"\x1b[1~", true), "Home");
        assert_eq!(name(b"\x1b[4~", true), "End");
    }
}
