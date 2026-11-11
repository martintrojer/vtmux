//! Streaming basics from tty-keys.c (`tty_keys_next`/`tty_keys_next1`).
//! Unknown terminal reports never become pane keys; the decoder bounds
//! incomplete control strings. Mouse reports (`tty_keys_mouse`) are decoded
//! into [`MouseInput`]: X10 (`ESC [ M` and three bytes) and SGR (`ESC [ <`).
use crate::keys::{Base, Key, Named};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Paste(bool),
    Sync(bool),
    /// `KEYC_FOCUS_IN` / `KEYC_FOCUS_OUT` (`CSI I` / `CSI O`).
    Focus(bool),
    /// An OSC 52 clipboard reply (`tty_keys_clipboard`): the clipboard it
    /// names (when it names one) and the decoded text; what a
    /// `refresh-client -l` or a pane's OSC 52 query asked for.
    Clipboard(Option<u8>, Vec<u8>),
    /// An OSC 10 (`true`) or OSC 11 reply (`tty_keys_colours`): the
    /// terminal's default foreground or background.
    Colour(bool, vtmux_vt::Rgb),
    /// An OSC 4 reply (`tty_keys_palette`): a palette entry.
    Palette(u8, vtmux_vt::Rgb),
    /// `CSI ? 997 ; 1|2 n`: the terminal's theme (`KEYC_REPORT_DARK_THEME`,
    /// `KEYC_REPORT_LIGHT_THEME`).
    Theme(vtmux_core::colour::Theme),
    /// The terminal's replies to the startup queries, each only while it
    /// is awaited: primary device attributes (`CSI ? Ps ; ... c`) with
    /// their parameters, secondary ones (`CSI > Pp ; ... c`) with the
    /// first, and XTVERSION (`DCS > | text ST`).
    DeviceAttributes(Vec<u32>),
    DeviceAttributes2(u32),
    ExtendedDeviceAttributes(String),
    /// The reply to the pixel size query (`CSI 4 ; height ; width t`,
    /// `tty_keys_winsz`) while it is awaited: the text area's width and
    /// height in pixels.
    PixelSize(u32, u32),
    /// A mouse report (`KEYC_MOUSE` with its `struct mouse_event`), and the
    /// bytes it came in (`event->buf`, written to a pane while pasting).
    Mouse(MouseInput, Vec<u8>),
    Ignore,
}

/// The startup queries a reply can answer (`TTY_HAVEDA`, `TTY_HAVEDA2`,
/// `TTY_HAVEXDA`, `TTY_HAVESYNC`).
const QUERY_DA: u8 = 1;
const QUERY_DA2: u8 = 2;
const QUERY_XDA: u8 = 4;
const QUERY_SYNC: u8 = 8;
const ALL_QUERIES: u8 = QUERY_DA | QUERY_DA2 | QUERY_XDA | QUERY_SYNC;

/// A decoded mouse report as `tty_keys_mouse` (tty-keys.c:1210-1347) fills
/// `struct mouse_event`: 0-based cell position and button byte, with the
/// previous report's (`tty->mouse_last_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseInput {
    pub x: u32,
    pub y: u32,
    /// The button byte: buttons, modifiers, drag and wheel bits; 3 is a
    /// release.
    pub b: u32,
    pub lx: u32,
    pub ly: u32,
    pub lb: u32,
    /// An SGR report (`sgr_type` not `' '`): its own button number and
    /// whether it was a release (`m`). `None` for X10.
    pub sgr: Option<Sgr>,
}

/// The SGR part of a report (`sgr_b`, `sgr_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sgr {
    pub b: u32,
    pub release: bool,
}

/// `MOUSE_PARAM_BTN_OFF` and `MOUSE_PARAM_POS_OFF` (tmux.h:710-711).
const X10_BUTTON_OFFSET: u32 = 0x20;
const X10_POSITION_OFFSET: u32 = 0x21;

/// What `tty_keys_mouse` makes of the start of the buffer.
#[derive(Debug, PartialEq, Eq)]
enum MouseParse {
    /// Not a mouse report.
    No,
    /// Could still become one.
    Partial,
    /// A report of `usize` bytes that is dropped (`-2`).
    Discard(usize),
    /// A report: position, button, SGR part, and its length. The previous
    /// position is filled in by the [`Decoder`].
    Report(MouseInput, usize),
}

/// `tty_keys_mouse` (tty-keys.c:1210-1347). There is no UTF-8 (1005)
/// decoding: tmux reads the three X10 bytes as they are.
fn mouse(bytes: &[u8]) -> MouseParse {
    let Some(rest) = bytes.strip_prefix(b"\x1b[") else {
        return if bytes == b"\x1b" {
            MouseParse::Partial
        } else {
            MouseParse::No
        };
    };
    match rest.first() {
        None => MouseParse::Partial,
        Some(b'M') => {
            let [b, x, y] = match rest.get(1..4) {
                Some(&[b, x, y]) => [b, x, y].map(u32::from),
                _ => return MouseParse::Partial,
            };
            if b < X10_BUTTON_OFFSET || x < X10_POSITION_OFFSET || y < X10_POSITION_OFFSET {
                return MouseParse::Discard(6);
            }
            let input = MouseInput {
                x: x - X10_POSITION_OFFSET,
                y: y - X10_POSITION_OFFSET,
                b: b - X10_BUTTON_OFFSET,
                ..MouseInput::default()
            };
            MouseParse::Report(input, 6)
        }
        Some(b'<') => {
            // Three decimal numbers, `;`, `;`, then `M` or `m`; anything
            // else is not a mouse report.
            let mut at = 3;
            let mut numbers = [0u32; 3];
            let mut end = 0;
            for (i, n) in numbers.iter_mut().enumerate() {
                loop {
                    let Some(&c) = bytes.get(at) else {
                        return MouseParse::Partial;
                    };
                    at += 1;
                    let ends = if i == 2 {
                        c == b'M' || c == b'm'
                    } else {
                        c == b';'
                    };
                    if ends {
                        end = c;
                        break;
                    }
                    if !c.is_ascii_digit() {
                        return MouseParse::No;
                    }
                    *n = n.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
                }
            }
            let [sgr_b, x, y] = numbers;
            if x < 1 || y < 1 {
                return MouseParse::Discard(at);
            }
            let release = end == b'm';
            // Some terminals (PuTTY 0.63) send a release for a wheel press.
            if release && mouse_wheel(sgr_b) {
                return MouseParse::Discard(at);
            }
            let input = MouseInput {
                x: x - 1,
                y: y - 1,
                b: if release { 3 } else { sgr_b },
                sgr: Some(Sgr { b: sgr_b, release }),
                ..MouseInput::default()
            };
            MouseParse::Report(input, at)
        }
        Some(_) => MouseParse::No,
    }
}

/// `MOUSE_WHEEL` (tmux.h:1679).
#[must_use]
pub fn mouse_wheel(b: u32) -> bool {
    matches!(b & MOUSE_MASK_BUTTONS, 64 | 65)
}

/// `MOUSE_MASK_BUTTONS` (tmux.h:1655).
pub const MOUSE_MASK_BUTTONS: u32 = 195;
/// `escape-time` default (options-table.c).
const ESCAPE_TIME: Duration = Duration::from_millis(10);
/// The partial-key floor while a reply may still be in flight
/// (tty-keys.c:996-1012).
const REPLY_DELAY: Duration = Duration::from_millis(500);
/// `TTY_QUERY_TIMEOUT` (tty.c:85): queries unanswered by then are given up.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the terminal's startup queries (DA, DA2, XTVERSION, `DECRQM
/// 2026`) and colour queries (OSC 10/11) stand: which are answered (the
/// `TTY_HAVE*` flags, `TTY_WAITFG|TTY_WAITBG` clear) and, while some are
/// out, when the start timer gives up on them. A live upgrade carries it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "tmux's TTY_HAVE* flags")]
pub struct Queries {
    pub until: Option<Instant>,
    pub da: bool,
    pub da2: bool,
    pub xtversion: bool,
    pub sync: bool,
    pub colours_until: Option<Instant>,
    pub fg: bool,
    pub bg: bool,
    /// `TTY_WINSIZEQUERY`: the size queries are out.
    pub winsize: bool,
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools, reason = "tmux's TTY_* flags")]
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
    /// The key tree for the client's terminal (`tty_keys_build`); `None`
    /// until it is known, which decodes as `xterm`.
    keys: Option<std::rc::Rc<KeyTable>>,
    /// `tty->mouse_last_x`, `_y`, `_b`: the previous mouse report.
    mouse_last: (u32, u32, u32),
    /// `TTY_WAITFG|TTY_WAITBG`: the colour queries are out until this
    /// instant (the start timer clears them), and which are answered.
    colours_until: Option<Instant>,
    colours_answered: (bool, bool),
    /// `TTY_WINSIZEQUERY`: the size queries (`CSI 18 t`, `CSI 14 t`) went
    /// to the terminal and its pixel size is not in yet. No timer gives up
    /// on it (tty.c `tty_resize`, tty-keys.c `tty_keys_winsz`).
    winsize_query: bool,
    /// A pane's request went to the terminal and is not answered yet
    /// (`!TAILQ_EMPTY(&c->input_requests)`); the server keeps it current.
    pub requests_out: bool,
    /// The `escape-time` option, which the server keeps current; `None`
    /// is its default.
    pub escape_time: Option<Duration>,
}
impl Decoder {
    /// The startup queries went to the terminal (DA, DA2, XTVERSION and
    /// `DECRQM 2026`, tty.c `tty_send_requests`); their replies may arrive
    /// split, so partial sequences wait longer until all are answered or
    /// `QUERY_TIMEOUT` passes.
    pub fn query_sent(&mut self, now: Instant) {
        self.queries_until = Some(now + QUERY_TIMEOUT);
        self.answered = 0;
        self.colours_sent(now);
    }
    /// The OSC 10 and 11 queries went to the terminal (`tty_send_requests`,
    /// `tty_repeat_requests`): `TTY_WAITFG|TTY_WAITBG`.
    pub fn colours_sent(&mut self, now: Instant) {
        self.colours_until = Some(now + QUERY_TIMEOUT);
        self.colours_answered = (false, false);
    }
    /// The size queries went to the terminal (tty.c `tty_resize`).
    pub fn winsize_sent(&mut self) {
        self.winsize_query = true;
    }
    /// Whether the size queries are out (`TTY_WINSIZEQUERY`).
    #[must_use]
    pub fn winsize_query(&self) -> bool {
        self.winsize_query
    }
    /// Where the queries stand.
    #[must_use]
    pub fn queries(&self) -> Queries {
        Queries {
            until: self.queries_until,
            da: self.answered & QUERY_DA != 0,
            da2: self.answered & QUERY_DA2 != 0,
            xtversion: self.answered & QUERY_XDA != 0,
            sync: self.answered & QUERY_SYNC != 0,
            colours_until: self.colours_until,
            fg: self.colours_answered.0,
            bg: self.colours_answered.1,
            winsize: self.winsize_query,
        }
    }
    /// The queries stand as `q` says (a client carried across an upgrade).
    pub fn set_queries(&mut self, q: Queries) {
        self.queries_until = q.until;
        self.answered = [
            (q.da, QUERY_DA),
            (q.da2, QUERY_DA2),
            (q.xtversion, QUERY_XDA),
            (q.sync, QUERY_SYNC),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .fold(0, |bits, (_, bit)| bits | bit);
        self.colours_until = q.colours_until;
        self.colours_answered = (q.fg, q.bg);
        self.winsize_query = q.winsize;
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
        self.colours_until = None;
        none
    }
    /// The client's terminal is known (`tty_keys_build` with its terminfo).
    pub fn set_terminal(&mut self, term: &vtmux_core::term_features::Term) {
        self.keys = Some(std::rc::Rc::new(build_table(&term.code_keys())));
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
    /// The bytes fed and not decoded yet (an ESC waiting out
    /// `escape-time`, a split sequence).
    #[must_use]
    pub fn held(&self) -> &[u8] {
        &self.bytes
    }
    /// [`Self::held`], taken out to be fed again later.
    pub fn take_held(&mut self) -> Vec<u8> {
        self.pending = None;
        std::mem::take(&mut self.bytes)
    }
    /// The bytes held (an ESC waiting out `escape-time`, a split sequence)
    /// moved to `next` with their wait, which runs on there, and the
    /// `escape-time` it is measured by (a live upgrade's re-attach gives
    /// the client a new decoder: the ESC still fires alone once its wait
    /// is out, or combines with the key that comes before then).
    pub fn hand_held_to(&mut self, next: &mut Self) {
        next.bytes = std::mem::take(&mut self.bytes);
        next.pending = self.pending.take();
        next.escape_time = self.escape_time;
    }
    /// The last mouse report held a button down (no release, no wheel):
    /// the next report's `lb` says so (a popup starts its drag from it).
    #[must_use]
    pub fn mouse_held(&self) -> bool {
        let b = self.mouse_last.2;
        b & MOUSE_MASK_BUTTONS != 3 && !mouse_wheel(b) && self.mouse_last != (0, 0, 0)
    }
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        let start = self.pending?;
        let waiting = self.queries_until.is_some_and(|until| start < until)
            || self.colours_until.is_some_and(|until| start < until)
            || self.requests_out
            // tty-keys.c:1002-1008: TTY_WINSIZEQUERY, for as long as the
            // terminal leaves it unanswered.
            || self.winsize_query
            || (self.pasting && b"\x1b[201~".starts_with(&self.bytes) && self.bytes.len() < 6);
        // tty-keys.c:990-1012: escape-time, 1 ms for 0, at least 500 ms
        // while a reply may still come.
        let delay = self
            .escape_time
            .unwrap_or(ESCAPE_TIME)
            .max(Duration::from_millis(1));
        Some(
            start
                + if waiting {
                    delay.max(REPLY_DELAY)
                } else {
                    delay
                },
        )
    }
    /// The events in what has come so far, as [`Self::drain_with_bytes`]
    /// without the bytes.
    #[cfg(test)]
    pub fn drain(&mut self, now: Instant) -> Vec<Event> {
        self.drain_with_bytes(now)
            .into_iter()
            .map(|(e, _)| e)
            .collect()
    }
    /// The events in what has come so far, each key with the bytes it came
    /// in (`event->buf`, tty-keys.c:1047-1049), which a bracketed paste
    /// writes to the pane as they are (`window_pane_paste`). Other events
    /// carry no bytes.
    pub fn drain_with_bytes(&mut self, now: Instant) -> Vec<(Event, Vec<u8>)> {
        let mut events = Vec::new();
        while !self.bytes.is_empty() {
            let expired = self.deadline().is_some_and(|t| now >= t);
            // tty_keys_next: mouse reports come before the key tree; once
            // the key timer has expired the buffer goes straight to the key
            // tree (`goto first_key`).
            let parsed = if expired {
                MouseParse::No
            } else {
                mouse(&self.bytes)
            };
            let decoded = match parsed {
                MouseParse::Partial if self.bytes.len() > 2 => {
                    self.pending.get_or_insert(now);
                    break;
                }
                MouseParse::Discard(n) => Some((Event::Ignore, n)),
                MouseParse::Report(mut input, n) => {
                    (input.lx, input.ly, input.lb) = self.mouse_last;
                    self.mouse_last = (input.x, input.y, input.b);
                    Some((Event::Mouse(input, self.bytes[..n].to_vec()), n))
                }
                // `ESC` and `ESC [` are partial keys as well.
                MouseParse::Partial | MouseParse::No => decode(
                    &self.bytes,
                    expired,
                    self.keys.as_deref().map_or(xterm_keys(), Vec::as_slice),
                ),
            };
            let Some((event, mut used)) = decoded else {
                self.pending.get_or_insert(now);
                break;
            };
            let reply = match event {
                Event::Sync(_) => QUERY_SYNC,
                Event::DeviceAttributes(_) => QUERY_DA,
                Event::DeviceAttributes2(_) => QUERY_DA2,
                Event::ExtendedDeviceAttributes(_) => QUERY_XDA,
                _ => 0,
            };
            // tty_keys_winsz (tty-keys.c:686-688): a size report nobody
            // asked for is no reply. vtmux swallows it all the same, as
            // it does every unknown terminal report.
            let event = match event {
                Event::PixelSize(..) if !self.winsize_query => Event::Ignore,
                other => other,
            };
            // A reply to a query already answered (or given up on) is no
            // reply: each tty_keys_* parser returns -1 once its TTY_HAVE*
            // flag is set, no key matches, so Escape and the byte after it
            // are that byte with Meta and the rest are keys of their own
            // (tty-keys.c:924-933): `ESC [ ? 6 2 ; 2 2 c` is M-[, `?`, ...
            let event = if reply != 0 && self.answered & reply != 0 {
                used = 2;
                let mut k = key(Base::Char(char::from(self.bytes[1])));
                k.meta = true;
                k.esc_prefix = true;
                Event::Key(k)
            } else {
                event
            };
            let bytes = if matches!(event, Event::Key(_)) {
                self.bytes[..used].to_vec()
            } else {
                Vec::new()
            };
            self.bytes.drain(..used);
            self.pending = None;
            match event {
                Event::Sync(_)
                | Event::DeviceAttributes(_)
                | Event::DeviceAttributes2(_)
                | Event::ExtendedDeviceAttributes(_) => {
                    // Partial keys wait until all four are in.
                    self.answered |= reply;
                    if self.answered == ALL_QUERIES {
                        self.queries_until = None;
                    }
                }
                Event::Paste(start) => self.pasting = start,
                // tty-keys.c:733: the pixel size ends the query.
                Event::PixelSize(..) => self.winsize_query = false,
                Event::Colour(fg, _) => {
                    if fg {
                        self.colours_answered.0 = true;
                    } else {
                        self.colours_answered.1 = true;
                    }
                    if self.colours_answered == (true, true) {
                        self.colours_until = None;
                    }
                }
                _ => {}
            }
            events.push((event, bytes));
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
    // tty-keys.c:1412-1413: one name before the `;`.
    let clip = (body.len() >= 2 && body[0] != b';' && body[1] == b';').then(|| body[0]);
    let Some(at) = body.iter().position(|&b| b == b';') else {
        return Event::Ignore;
    };
    let text = &body[at + 1..];
    if text.len() <= 1 {
        return Event::Ignore;
    }
    base64_decode(text).map_or(Event::Ignore, |data| Event::Clipboard(clip, data))
}

/// `tty_keys_colours` (tty-keys.c:1755-1826): the text of an OSC 10 or 11
/// reply (127 bytes at most) as a colour; one that does not parse is no
/// event.
fn colour(fg: bool, body: &[u8]) -> Event {
    if body.len() >= 127 {
        return Event::Ignore;
    }
    vtmux_vt::palette::parse_x11(body, vtmux_core::colour::x11_rgb)
        .map_or(Event::Ignore, |c| Event::Colour(fg, c))
}

/// `tty_keys_palette` (tty-keys.c:1830-1893): `index ; colour`.
fn palette(body: &[u8]) -> Event {
    let Some(at) = body.iter().position(|&b| b == b';') else {
        return Event::Ignore;
    };
    let Some(index) = std::str::from_utf8(&body[..at])
        .ok()
        .and_then(|i| i.parse::<u8>().ok())
    else {
        return Event::Ignore;
    };
    vtmux_vt::palette::parse_x11(&body[at + 1..], vtmux_core::colour::x11_rgb)
        .map_or(Event::Ignore, |c| Event::Palette(index, c))
}

/// `b64_pton`: padded standard base64.
#[must_use]
pub fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
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
fn decode(bytes: &[u8], expired: bool, table: &[(Vec<u8>, Key)]) -> Option<(Event, usize)> {
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
                    b"\x1b]10;" => colour(true, body),
                    b"\x1b]11;" => colour(false, body),
                    b"\x1b]4;" => palette(body),
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
    match next1(bytes, expired, table) {
        Next1::Found(event, n) => return Some((event, n)),
        Next1::Partial => return None,
        Next1::Missing => {}
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
                // tty-keys.c:220-221, the theme reports.
                if final_byte == b'n' {
                    match body {
                        "?997;1" => {
                            return Some((Event::Theme(vtmux_core::colour::Theme::Dark), end + 1));
                        }
                        "?997;2" => {
                            return Some((Event::Theme(vtmux_core::colour::Theme::Light), end + 1));
                        }
                        _ => {}
                    }
                }
                if final_byte == b't' {
                    return Some((window_size(body), end + 1));
                }
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
        } else if !expired && csi_may_continue(&bytes[2..]) {
            return None;
        }
    }
    // tty_keys_next (tty-keys.c:900-924): Escape followed by a key of the
    // key tree (or a UTF-8 character) is that key with Meta. If the key
    // already carries an xterm Meta modifier (or is a paste marker), the
    // Escape is a key of its own. Only the tree: `ESC ESC x` is M-Escape
    // and `x`, `ESC ESC [97;5u` M-Escape and text.
    let rest = &bytes[1..];
    match next1(rest, expired, table) {
        Next1::Partial => return None,
        Next1::Found(Event::Key(k), _) if k.meta && !k.esc_prefix => {
            return Some((Event::Key(key(Base::Char('\x1b'))), 1));
        }
        Next1::Found(Event::Paste(_), _) => {
            return Some((Event::Key(key(Base::Char('\x1b'))), 1));
        }
        Next1::Found(Event::Key(mut k), n) => {
            k.esc_prefix = true;
            k.meta = true;
            return Some((Event::Key(k), n + 1));
        }
        Next1::Found(other, n) => return Some((other, n + 1)),
        Next1::Missing => {}
    }
    // tty-keys.c:926-933: no key with or without the Escape, partial or
    // not, so Escape and the byte after it are that byte with Meta.
    let (mut k, _) = character(&rest[..1], true)?;
    k.meta = true;
    k.esc_prefix = true;
    Some((Event::Key(k), 2))
}

/// What `tty_keys_next1` (tty-keys.c:603-665) finds at the start of the
/// buffer.
enum Next1 {
    Found(Event, usize),
    /// A key that a longer one continues, or the start of a UTF-8
    /// character: wait for more until the key timer expires.
    Partial,
    Missing,
}

/// `tty_keys_next1`: the key tree, then a UTF-8 character. A sequence that
/// only starts a key is no key and no partial key (`tty_keys_find1` stops
/// on a node without a key), so `ESC O` alone is M-O, not a wait; what
/// waits for more is a CSI the other parsers still read
/// ([`csi_may_continue`]) or a key a longer key continues (`tk->next`).
fn next1(bytes: &[u8], expired: bool, table: &[(Vec<u8>, Key)]) -> Next1 {
    for (seq, event) in [
        (&b"\x1b[I"[..], Event::Focus(true)),
        (b"\x1b[O", Event::Focus(false)),
        (b"\x1b[200~", Event::Paste(true)),
        (b"\x1b[201~", Event::Paste(false)),
    ] {
        if bytes.starts_with(seq) {
            return Next1::Found(event, seq.len());
        }
    }
    if let Some((seq, k)) = table.iter().find(|(seq, _)| bytes.starts_with(seq)) {
        let continued = || {
            table
                .iter()
                .any(|(longer, _)| longer.len() > seq.len() && longer.starts_with(seq))
        };
        if !expired && seq.len() == bytes.len() && continued() {
            return Next1::Partial;
        }
        return Next1::Found(Event::Key(*k), seq.len());
    }
    let size = match bytes[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Next1::Missing,
    };
    if bytes.len() < size {
        return if expired {
            Next1::Missing
        } else {
            Next1::Partial
        };
    }
    match std::str::from_utf8(&bytes[..size])
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(c) => Next1::Found(Event::Key(key(Base::Char(c))), size),
        None => Next1::Missing,
    }
}

/// Whether a CSI with no final byte yet (`bytes` after `ESC [`) can still
/// be something tmux reads: an extended key or a window size report
/// (digits and `;`, `tty_keys_extended_key`, `tty_keys_winsz`), or a reply
/// (`?` or `>`, `tty_keys_device_attributes`, `tty_keys_sync`; vtmux waits
/// for these whether or not the query was answered). Anything else is not
/// partial, so `ESC [ 1 1 $` is M-[ and text at once.
fn csi_may_continue(bytes: &[u8]) -> bool {
    match bytes.first() {
        Some(b'?' | b'>') => true,
        _ => bytes.iter().all(|b| b.is_ascii_digit() || *b == b';'),
    }
}
/// `tty_keys_winsz` (tty-keys.c:676-737): `4 ; height ; width` is the
/// text area in pixels. `8 ; rows ; columns` is taken too, but where tmux
/// sets the terminal's size from it (`tty_set_size`, without fitting any
/// window to it), vtmux keeps the size the client's terminal gave; the two
/// agree. Anything else is no event.
fn window_size(body: &str) -> Event {
    // sscanf's `4;%u;%u`: the two numbers, whatever follows them.
    let mut fields = body.split(';');
    let (Some("4"), Some(h), Some(w)) = (fields.next(), fields.next(), fields.next()) else {
        return Event::Ignore;
    };
    let number = |n: &str| {
        (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .then(|| n.parse().unwrap_or(u32::MAX))
    };
    match (number(w), number(h)) {
        (Some(w), Some(h)) => Event::PixelSize(w, h),
        _ => Event::Ignore,
    }
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
/// modifier digit 2-9, `tty_default_raw_keys`, then the terminal's terminfo
/// keys (`tty_default_code_keys`, [`Term::code_keys`]).
///
/// [`Term::code_keys`]: vtmux_core::term_features::Term::code_keys
type KeyTable = Vec<(Vec<u8>, Key)>;

/// The key tree of `xterm`'s terminfo keys, for a client whose terminal
/// is not known.
fn xterm_keys() -> &'static [(Vec<u8>, Key)] {
    static TABLE: std::sync::OnceLock<KeyTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        build_table(&[
            ("kf1", "\x1bOP"),
            ("kf2", "\x1bOQ"),
            ("kf3", "\x1bOR"),
            ("kf4", "\x1bOS"),
            ("kf5", "\x1b[15~"),
            ("kf6", "\x1b[17~"),
            ("kf7", "\x1b[18~"),
            ("kf8", "\x1b[19~"),
            ("kf9", "\x1b[20~"),
            ("kf10", "\x1b[21~"),
            ("kf11", "\x1b[23~"),
            ("kf12", "\x1b[24~"),
            ("kich1", "\x1b[2~"),
            ("kdch1", "\x1b[3~"),
            ("khome", "\x1bOH"),
            ("kend", "\x1bOF"),
            ("knp", "\x1b[6~"),
            ("kpp", "\x1b[5~"),
            ("kcbt", "\x1b[Z"),
        ])
    })
}

#[expect(clippy::too_many_lines, reason = "tty-keys.c's key tables")]
fn build_table(code_keys: &[(&str, &str)]) -> KeyTable {
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
    for &(cap, seq) in code_keys {
        let n = match cap {
            "kich1" => Insert,
            "kdch1" => Delete,
            "khome" => Home,
            "kend" => End,
            "knp" => PageDown,
            "kpp" => PageUp,
            "kcbt" => BackTab,
            _ => match cap.strip_prefix("kf").and_then(|n| n.parse().ok()) {
                Some(n) => F(n),
                None => continue,
            },
        };
        add(seq, key(named(n)));
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
    // tty-keys.c:930-937: not a key and not UTF-8, so the first byte is
    // the key as it is.
    Some((key(Base::Byte(b)), 1))
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

    /// tty-keys.c:990-1012: a partial key waits `escape-time` (1 ms for
    /// 0), and at least 500 ms while a startup reply may still come.
    #[test]
    fn partial_keys_wait_escape_time() {
        let now = Instant::now();
        let ms = Duration::from_millis;
        for (option, want) in [
            (None, ms(10)),
            (Some(ms(0)), ms(1)),
            (Some(ms(3000)), ms(3000)),
        ] {
            let mut d = Decoder {
                escape_time: option,
                ..Decoder::default()
            };
            d.feed(b"\x1b");
            assert_eq!(d.drain(now), Vec::new());
            assert_eq!(d.deadline(), Some(now + want), "{option:?}");
            assert_eq!(d.drain(now + want.saturating_sub(ms(1))), Vec::new());
            assert_eq!(
                d.drain(now + want),
                vec![Event::Key(key(Base::Char('\x1b')))]
            );
        }
        let mut d = Decoder {
            escape_time: Some(ms(0)),
            ..Decoder::default()
        };
        d.query_sent(now);
        d.feed(b"\x1b");
        assert_eq!(d.drain(now), Vec::new());
        assert_eq!(d.deadline(), Some(now + ms(500)));
    }

    /// A held ESC handed to a new decoder (a live upgrade's re-attach)
    /// keeps its wait and `escape-time`: it fires alone when the wait is
    /// out, or combines with a key that comes before then.
    #[test]
    fn a_held_escape_handed_on_keeps_its_wait() {
        let now = Instant::now();
        let ms = Duration::from_millis;
        let handed = || {
            let mut old = Decoder {
                escape_time: Some(ms(1500)),
                ..Decoder::default()
            };
            old.feed(b"\x1b");
            assert_eq!(old.drain(now), Vec::new());
            let mut new = Decoder::default();
            old.hand_held_to(&mut new);
            assert!(old.held().is_empty() && old.deadline().is_none());
            assert_eq!(new.deadline(), Some(now + ms(1500)));
            new
        };
        let mut d = handed();
        assert_eq!(d.drain(now + ms(1499)), Vec::new());
        assert_eq!(
            d.drain(now + ms(1500)),
            vec![Event::Key(key(Base::Char('\x1b')))]
        );
        let mut d = handed();
        d.feed(b"x");
        assert_eq!(
            d.drain(now + ms(100)),
            vec![Event::Key(Key {
                meta: true,
                esc_prefix: true,
                ..key(Base::Char('x'))
            })]
        );
    }

    /// `tty_keys_next` (tty-keys.c:900-933): after an Escape only the key
    /// tree (or a UTF-8 character) is looked for, and a sequence that only
    /// starts a key is not partial: `ESC ESC x` is M-Escape then `x`, and
    /// `ESC ESC`, `ESC O`, `ESC [ 1 1 $` are keys at once, with no wait.
    #[test]
    fn escape_prefix_takes_the_key_tree_only() {
        let meta = |c: char| Key {
            meta: true,
            esc_prefix: true,
            ..key(Base::Char(c))
        };
        let now = Instant::now();
        for (input, want) in [
            (&b"\x1b\x1bx"[..], vec![meta('\x1b'), key(Base::Char('x'))]),
            (b"\x1b\x1b", vec![meta('\x1b')]),
            (b"\x1bO", vec![meta('O')]),
            (
                b"\x1b[11$",
                vec![
                    meta('['),
                    key(Base::Char('1')),
                    key(Base::Char('1')),
                    key(Base::Char('$')),
                ],
            ),
            (
                b"\x1b\x1b[9;5u",
                vec![
                    meta('\x1b'),
                    key(Base::Char('[')),
                    key(Base::Char('9')),
                    key(Base::Char(';')),
                    key(Base::Char('5')),
                    key(Base::Char('u')),
                ],
            ),
            (b"\x1b\xc3\xa9", vec![meta('é')]),
            (
                b"\x1b\x1b[A",
                vec![Key {
                    meta: true,
                    esc_prefix: true,
                    ..key(Base::Named(Named::Up))
                }],
            ),
        ] {
            let mut d = Decoder::default();
            d.feed(input);
            let got = d.drain(now);
            let want: Vec<Event> = want.into_iter().map(Event::Key).collect();
            assert_eq!(got, want, "{input:?}");
        }
        // What can still be a key or a reply waits: an extended key, a
        // reply, a UTF-8 character after the Escape.
        for input in [&b"\x1b[1;5"[..], b"\x1b[", b"\x1b[?62", b"\x1b\xc3"] {
            let mut d = Decoder::default();
            d.feed(input);
            assert_eq!(d.drain(now), Vec::new(), "{input:?}");
        }
    }

    /// tty-keys.c:930-937: bytes that are not UTF-8 are keys one byte at
    /// a time, of their value, which no character is: a status prompt
    /// drops them (prompt.c:1492-1501).
    #[test]
    fn invalid_utf8_is_one_raw_key_per_byte() {
        let mut d = Decoder::default();
        d.feed(b"\xe6\x85_");
        assert_eq!(
            d.drain(Instant::now()),
            vec![
                Event::Key(key(Base::Byte(0xe6))),
                Event::Key(key(Base::Byte(0x85))),
                Event::Key(key(Base::Char('_')))
            ]
        );
        // Each key keeps the bytes it came in (a paste writes them), and
        // nothing else carries any.
        d.feed(b"\xe6\xf0\x9f\x98\x80\x1b[A\x1b[200~");
        assert_eq!(
            d.drain_with_bytes(Instant::now())
                .into_iter()
                .map(|(_, b)| b)
                .collect::<Vec<_>>(),
            [&b"\xe6"[..], b"\xf0\x9f\x98\x80", b"\x1b[A", b""]
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
                Event::Clipboard(Some(b'c'), b"hello".to_vec()),
                Event::Ignore,
                Event::Clipboard(None, b"foo".to_vec())
            ]
        );
    }

    /// tty-keys.c:1755-1893 and 220-221: colour, palette and theme
    /// replies.
    #[test]
    fn colour_palette_and_theme_replies() {
        use vtmux_core::colour::Theme;
        let mut d = Decoder::default();
        d.feed(b"\x1b]10;rgb:ffff/8080/0000\x1b\\\x1b]11;#102030\x07\x1b]4;99;rgb:0101/0202/0303\x1b\\");
        d.feed(b"\x1b]4;300;red\x07\x1b]11;nope\x07\x1b[?997;2n\x1b[?997;1n\x1b[?997;3n");
        assert_eq!(
            d.drain(Instant::now()),
            vec![
                Event::Colour(true, (0xff, 0x80, 0)),
                Event::Colour(false, (0x10, 0x20, 0x30)),
                Event::Palette(99, (1, 2, 3)),
                Event::Ignore,
                Event::Ignore,
                Event::Theme(Theme::Light),
                Event::Theme(Theme::Dark),
                Event::Ignore,
            ]
        );
    }

    /// tty-keys.c:1210-1347: SGR and X10 reports, the previous position,
    /// and the reports tmux drops.
    #[test]
    fn mouse_reports() {
        let now = Instant::now();
        let mut d = Decoder::default();
        d.feed(b"\x1b[<0;5;3M\x1b[<0;5;3m\x1b[M\x20\x2a\x22\x1b[<64;1;1m\x1b[<0;0;1M");
        let events = d.drain(now);
        let inputs: Vec<MouseInput> = events
            .iter()
            .filter_map(|e| match e {
                Event::Mouse(m, _) => Some(*m),
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 5);
        assert_eq!(events[3], Event::Ignore);
        assert_eq!(events[4], Event::Ignore);
        let sgr = |b, release| Some(Sgr { b, release });
        assert_eq!(
            inputs,
            vec![
                MouseInput {
                    x: 4,
                    y: 2,
                    b: 0,
                    sgr: sgr(0, false),
                    ..MouseInput::default()
                },
                MouseInput {
                    x: 4,
                    y: 2,
                    b: 3,
                    lx: 4,
                    ly: 2,
                    lb: 0,
                    sgr: sgr(0, true)
                },
                MouseInput {
                    x: 9,
                    y: 1,
                    b: 0,
                    lx: 4,
                    ly: 2,
                    lb: 3,
                    sgr: None
                },
            ]
        );
        // X10 bytes past 0x7f are positions, not UTF-8; below the offsets
        // the report is dropped.
        d.feed(b"\x1b[M\x23\xc0\x80\x1b[M\x1f\x30\x30");
        let events = d.drain(now);
        assert!(matches!(
            events[0],
            Event::Mouse(
                MouseInput {
                    x: 159,
                    y: 95,
                    b: 3,
                    ..
                },
                _
            )
        ));
        assert_eq!(events[1], Event::Ignore);
        assert_eq!(events.len(), 2);
        // A split report waits; one that stays partial past escape-time is
        // keys.
        d.feed(b"\x1b[<0;1");
        assert_eq!(d.drain(now), Vec::new());
        d.feed(b"2;3M");
        assert!(matches!(
            d.drain(now)[..],
            [Event::Mouse(MouseInput { x: 11, y: 2, .. }, _)]
        ));
        d.feed(b"\x1b[<0;1");
        assert_eq!(d.drain(now), Vec::new());
        let keys = d.drain(now + Duration::from_millis(20));
        assert!(keys.iter().all(|e| matches!(e, Event::Key(_))), "{keys:?}");
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
        // All four answered and both colours (TTY_ALL_REQUEST_FLAGS,
        // TTY_WAITFG|TTY_WAITBG): back to escape-time.
        let mut d = Decoder::default();
        d.query_sent(now);
        d.feed(b"\x1b[?2026;2$y\x1b");
        assert_eq!(d.drain(now), vec![Event::Sync(true)]);
        assert_eq!(d.deadline(), Some(now + ms(500)));
        d.feed(b"[?62;4c\x1b[>1;2c\x1bP>|xterm\x1b\\\x1b]10;#ffffff\x07\x1b");
        assert_eq!(
            d.drain(now),
            vec![
                Event::DeviceAttributes(vec![62, 4]),
                Event::DeviceAttributes2(1),
                Event::ExtendedDeviceAttributes("xterm".into()),
                Event::Colour(true, (0xff, 0xff, 0xff)),
            ]
        );
        assert_eq!(
            d.deadline(),
            Some(now + ms(500)),
            "the background is still out"
        );
        d.feed(b"]11;#000000\x07\x1b");
        assert_eq!(d.drain(now), vec![Event::Colour(false, (0, 0, 0))]);
        assert_eq!(d.deadline(), Some(now + ms(10)));
        assert_eq!(d.start_deadline(), None);
        // A second reply is no reply (tty-keys.c:1476-1477): M-[ and keys.
        let meta_bracket = || {
            let mut k = key(Base::Char('['));
            k.meta = true;
            k.esc_prefix = true;
            Event::Key(k)
        };
        let chars = |s: &'static str| s.chars().map(|c| Event::Key(key(Base::Char(c))));
        d.feed(b"[?62;52c");
        assert_eq!(
            d.drain(now),
            std::iter::once(meta_bracket())
                .chain(chars("?62;52c"))
                .collect::<Vec<_>>()
        );
        // Unanswered queries time out after TTY_QUERY_TIMEOUT.
        let mut d = Decoder::default();
        d.query_sent(now);
        d.feed(b"\x1b");
        assert_eq!(d.drain(now + ms(5000)), Vec::new());
        assert_eq!(d.deadline(), Some(now + ms(5010)));
        // The start timer (tty_start_timer_callback): due at the same time,
        // it says whether none of DA, DA2 and XTVERSION was answered, and
        // later replies are keys (TTY_ALL_REQUEST_FLAGS).
        let mut d = Decoder::default();
        d.query_sent(now);
        assert_eq!(d.start_deadline(), Some(now + ms(5000)));
        assert!(d.queries_expired());
        assert_eq!(d.start_deadline(), None);
        d.feed(b"\x1b[?62c");
        assert_eq!(
            d.drain(now),
            std::iter::once(meta_bracket())
                .chain(chars("?62c"))
                .collect::<Vec<_>>()
        );
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

    /// D-014: while the size queries are out (`TTY_WINSIZEQUERY`, tty.c
    /// `tty_resize`), a lone Escape waits 500 ms (tty-keys.c:1002-1008),
    /// with no timer giving up on them; the pixel size reply ends it
    /// (tty-keys.c `tty_keys_winsz`), the cell size reply does not.
    #[test]
    fn escape_waits_on_the_size_query_until_the_pixel_reply() {
        let now = Instant::now();
        let ms = Duration::from_millis;
        // No startup query out: escape-time, but for the size query.
        let mut d = Decoder::default();
        d.winsize_sent();
        let later = now + ms(60_000);
        d.feed(b"\x1b");
        assert_eq!(d.drain(later), Vec::new());
        assert_eq!(d.deadline(), Some(later + ms(500)));
        assert_eq!(d.drain(later + ms(499)), Vec::new());
        assert_eq!(
            d.drain(later + ms(500)),
            vec![Event::Key(key(Base::Char('\x1b')))]
        );
        // The reply split: it waits as a partial key, then is taken.
        d.feed(b"\x1b[8;24;80t\x1b[4;4");
        assert_eq!(d.drain(later), vec![Event::Ignore]);
        assert!(d.winsize_query(), "the cell size reply leaves it out");
        assert_eq!(d.deadline(), Some(later + ms(500)));
        d.feed(b"80;800t\x1b");
        assert_eq!(d.drain(later), vec![Event::PixelSize(800, 480)]);
        assert!(!d.winsize_query());
        assert_eq!(d.deadline(), Some(later + ms(10)));
        assert!(!d.queries().winsize);
        // Unasked for, a size reply is nothing (vtmux swallows it).
        d.feed(b"[4;480;800t");
        assert_eq!(d.drain(later), vec![Event::Ignore]);
        // Not `4 ; number ; number`: nothing.
        d.winsize_sent();
        for bad in [&b"\x1b[4;480t"[..], b"\x1b[4;;800t", b"\x1b[5;480;800t"] {
            d.feed(bad);
            assert_eq!(d.drain(later), vec![Event::Ignore], "{bad:?}");
            assert!(d.winsize_query());
        }
        // A live upgrade carries it.
        let mut e = Decoder::default();
        e.set_queries(d.queries());
        assert!(e.winsize_query());
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
        let name = |b: &[u8], term: Option<&str>| {
            let mut d = Decoder::default();
            if let Some(term) = term {
                let term =
                    vtmux_core::term_features::Term::open(term, &[], None, &[], &[]).unwrap();
                d.set_terminal(&term);
            }
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
            assert_eq!(name(seq, None), want, "{seq:?}");
            assert_eq!(name(seq, Some("xterm-256color")), want, "{seq:?}");
        }
        // xterm's terminfo has no ESC [1~ / ESC [4~: Meta-[ then the text.
        assert_eq!(name(b"\x1b[1~", None), "M-[ 1 ~");
        assert_eq!(name(b"\x1b[1~", Some("screen")), "Home");
        assert_eq!(name(b"\x1b[4~", Some("tmux-256color")), "End");
        // The console's F1-F5 and rxvt's Home/End come from their entries.
        assert_eq!(name(b"\x1b[[A", Some("linux")), "F1");
        assert_eq!(name(b"\x1b[[E", Some("linux")), "F5");
        assert_eq!(name(b"\x1b[[A", None), "M-[ [ A");
        assert_eq!(name(b"\x1b[7~", Some("rxvt-unicode-256color")), "Home");
        assert_eq!(name(b"\x1b[8~", Some("rxvt-unicode-256color")), "End");
    }
}
