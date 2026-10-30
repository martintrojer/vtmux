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
    Ignore,
}
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
    /// Inside a bracketed paste (`TTY_BRACKETPASTE`).
    pasting: bool,
}
impl Decoder {
    /// A query went to the terminal (the startup `DECRQM 2026`); its reply
    /// may arrive split, so partial sequences wait longer until it is
    /// answered or `QUERY_TIMEOUT` passes.
    pub fn query_sent(&mut self, now: Instant) {
        self.queries_until = Some(now + QUERY_TIMEOUT);
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
            let Some((event, used)) = decode(&self.bytes, expired) else {
                self.pending.get_or_insert(now);
                break;
            };
            self.bytes.drain(..used);
            self.pending = None;
            match event {
                Event::Sync(_) => self.queries_until = None,
                Event::Paste(start) => self.pasting = start,
                _ => {}
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
#[allow(clippy::too_many_lines)] // terminal sequence dispatch table
fn decode(bytes: &[u8], expired: bool) -> Option<(Event, usize)> {
    if bytes[0] != 0x1b {
        return character(bytes, expired).map(|(k, n)| (Event::Key(k), n));
    }
    if bytes.len() == 1 {
        return expired.then(|| (Event::Key(key(Base::Char('\x1b'))), 1));
    }
    if bytes[1] == b']' || bytes[1] == b'P' {
        // OSC colour/clipboard and DCS version replies (tty-keys.c
        // tty_keys_colours/extended_device_attributes). Never forward.
        for i in 2..bytes.len() {
            if bytes[i] == 7 {
                return Some((Event::Ignore, i + 1));
            }
            if bytes[i] == b'\\' && bytes[i - 1] == 0x1b {
                return Some((Event::Ignore, i + 1));
            }
        }
        if bytes.len() > 4096 || expired {
            return Some((Event::Ignore, bytes.len()));
        }
        return None;
    }
    if matches!(bytes[1], b'[' | b'O') {
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
                || matches!(final_byte, b'c' | b't' | b'n' | b'y' | b'u')
            {
                if final_byte == b'y' && body.starts_with("?2026;") {
                    return Some((
                        Event::Sync(matches!(body.as_bytes().get(6), Some(b'1' | b'2' | b'3'))),
                        end + 1,
                    ));
                }
                return Some((Event::Ignore, end + 1));
            }
            let mut args = body.split(';');
            let number = args.next().unwrap_or("");
            let modifier = args.next().and_then(|s| s.parse::<u8>().ok()).unwrap_or(1);
            let named = match final_byte {
                b'A' => Some(Named::Up),
                b'B' => Some(Named::Down),
                b'C' => Some(Named::Right),
                b'D' => Some(Named::Left),
                b'H' => Some(Named::Home),
                b'F' => Some(Named::End),
                b'Z' => Some(Named::BackTab),
                b'P' => Some(Named::F(1)),
                b'Q' => Some(Named::F(2)),
                b'R' => Some(Named::F(3)),
                b'S' => Some(Named::F(4)),
                b'~' => match number {
                    "200" => return Some((Event::Paste(true), end + 1)),
                    "201" => return Some((Event::Paste(false), end + 1)),
                    "1" | "7" => Some(Named::Home),
                    "4" | "8" => Some(Named::End),
                    "2" => Some(Named::Insert),
                    "3" => Some(Named::Delete),
                    "5" => Some(Named::PageUp),
                    "6" => Some(Named::PageDown),
                    "11" => Some(Named::F(1)),
                    "12" => Some(Named::F(2)),
                    "13" => Some(Named::F(3)),
                    "14" => Some(Named::F(4)),
                    "15" => Some(Named::F(5)),
                    "17" => Some(Named::F(6)),
                    "18" => Some(Named::F(7)),
                    "19" => Some(Named::F(8)),
                    "20" => Some(Named::F(9)),
                    "21" => Some(Named::F(10)),
                    "23" => Some(Named::F(11)),
                    "24" => Some(Named::F(12)),
                    _ => None,
                },
                _ => None,
            };
            if let Some(named) = named {
                let mut k = key(Base::Named(named));
                let m = modifier.saturating_sub(1);
                k.shift = m & 1 != 0;
                k.meta = m & 2 != 0;
                k.ctrl = m & 4 != 0;
                return Some((Event::Key(k), end + 1));
            }
            if matches!(final_byte, b'I' | b'O') {
                return Some((Event::Ignore, end + 1));
            }
        } else if !expired {
            return None;
        }
    }
    // tty_keys_next (tty-keys.c:900-924): Escape followed by a key is that
    // key with Meta. If the key already carries an xterm Meta modifier (or
    // is a paste marker), the Escape is a key of its own. The meta forms in
    // the table (tty-keys.c:118-144) keep Escape+arrow as Escape+arrow,
    // but `ESC ESC [H`/`ESC ESC O F` are M-Home/M-End.
    let (event, n) = decode(&bytes[1..], expired)?;
    let event = match event {
        Event::Key(k) if k.meta && !k.esc_prefix => {
            return Some((Event::Key(key(Base::Char('\x1b'))), 1));
        }
        Event::Paste(_) => return Some((Event::Key(key(Base::Char('\x1b'))), 1)),
        Event::Key(mut k) => {
            let implied = n == 3
                && bytes[1] == 0x1b
                && matches!(bytes[2], b'[' | b'O')
                && matches!(bytes[3], b'H' | b'F');
            k.esc_prefix = k.esc_prefix || !implied;
            k.meta = true;
            Event::Key(k)
        }
        other => other,
    };
    Some((event, n + 1))
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
                Event::Ignore,
                Event::Ignore,
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
        // Answered: back to escape-time.
        let mut d = Decoder::default();
        d.query_sent(now);
        d.feed(b"\x1b[?2026;2$y\x1b");
        assert_eq!(d.drain(now), vec![Event::Sync(true)]);
        assert_eq!(d.deadline(), Some(now + ms(10)));
        // Unanswered queries time out after TTY_QUERY_TIMEOUT.
        let mut d = Decoder::default();
        d.query_sent(now);
        d.feed(b"\x1b");
        assert_eq!(d.drain(now + ms(5000)), Vec::new());
        assert_eq!(d.deadline(), Some(now + ms(5010)));
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
}
