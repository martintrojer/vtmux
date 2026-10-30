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
#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
    pending: Option<Instant>,
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|t| t + Duration::from_millis(10))
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
    }
}
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
    // Escape followed by a key is Meta. A second escape can introduce a
    // named key; normal/application cursor forms share the same base.
    let (event, n) = decode(&bytes[1..], expired)?;
    let event = match event {
        Event::Key(mut k) => {
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
            k.base = Base::Named(Named::Backspace)
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
        assert!(d.drain(now).is_empty());
        d.feed(b"A");
        assert_eq!(d.drain(now), vec![Event::Key(key(Base::Named(Named::Up)))]);
        d.feed(b"\xe4\xb8");
        assert!(d.drain(now).is_empty());
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
        assert!(d.drain(now).is_empty());
        assert_eq!(
            d.drain(now + Duration::from_millis(11)),
            vec![Event::Key(key(Base::Char('\x1b')))]
        );
    }
}
