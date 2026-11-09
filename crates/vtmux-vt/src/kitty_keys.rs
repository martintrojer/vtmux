//! D29, the kitty keyboard protocol on the pane side (spec, *Protocol
//! extensions (D28)*): a key the server decoded, encoded by libghostty's
//! key encoder under the flags of the pane's active screen (`encode` and
//! `kitty`, ghostty src/input/key_encode.zig:82-100, 103-317; Rust
//! `key::Encoder::set_kitty_flags`).
//!
//! The server only ever has key presses, and no associated text from the
//! outer terminal, so of the five flags only 1 (disambiguate), 4 (report
//! alternates) and 8 (report all keys) are applied ([`APPLIED`]); 2 and 16
//! wait for kitty input from the outer terminal (a later decision).

use libghostty_vt::key::{Action, Encoder, Event, Key, KittyKeyFlags, Mods};

/// The kitty flags vtmux applies: what a pane's `CSI ? u` reports and what
/// its keys are encoded with.
pub const APPLIED: u8 = 1 | 4 | 8;

/// The key itself, as the server decoded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyBase {
    /// A printable character (U+0020 and up, not DEL).
    Char(char),
    Enter,
    Tab,
    Escape,
    Backspace,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    /// F1 to F12.
    F(u8),
    /// A numeric keypad key in tmux's order (`KP/` 0 to `KP.` 15,
    /// tmux.h:392-407).
    Keypad(u8),
}

/// A key press with its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyKey {
    pub base: KittyBase,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// The keypad keys in [`KittyBase::Keypad`] order and the text each types.
const KEYPAD: [(Key, Option<char>); 16] = [
    (Key::NumpadDivide, Some('/')),
    (Key::NumpadMultiply, Some('*')),
    (Key::NumpadSubtract, Some('-')),
    (Key::Numpad7, Some('7')),
    (Key::Numpad8, Some('8')),
    (Key::Numpad9, Some('9')),
    (Key::NumpadAdd, Some('+')),
    (Key::Numpad4, Some('4')),
    (Key::Numpad5, Some('5')),
    (Key::Numpad6, Some('6')),
    (Key::Numpad1, Some('1')),
    (Key::Numpad2, Some('2')),
    (Key::Numpad3, Some('3')),
    (Key::NumpadEnter, None),
    (Key::Numpad0, Some('0')),
    (Key::NumpadDecimal, Some('.')),
];

const F_KEYS: [Key; 12] = [
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
];

/// The physical key of a character on a US layout, where it has one of
/// its own unshifted: letters, digits and space. Others are
/// `Unidentified`: the character alone is the key's code (the encoder's
/// `unshifted_codepoint`).
fn char_key(c: char) -> Key {
    const LETTERS: [Key; 26] = [
        Key::A,
        Key::B,
        Key::C,
        Key::D,
        Key::E,
        Key::F,
        Key::G,
        Key::H,
        Key::I,
        Key::J,
        Key::K,
        Key::L,
        Key::M,
        Key::N,
        Key::O,
        Key::P,
        Key::Q,
        Key::R,
        Key::S,
        Key::T,
        Key::U,
        Key::V,
        Key::W,
        Key::X,
        Key::Y,
        Key::Z,
    ];
    const DIGITS: [Key; 10] = [
        Key::Digit0,
        Key::Digit1,
        Key::Digit2,
        Key::Digit3,
        Key::Digit4,
        Key::Digit5,
        Key::Digit6,
        Key::Digit7,
        Key::Digit8,
        Key::Digit9,
    ];
    match c {
        'a'..='z' => LETTERS[usize::from(c as u8 - b'a')],
        '0'..='9' => DIGITS[usize::from(c as u8 - b'0')],
        ' ' => Key::Space,
        _ => Key::Unidentified,
    }
}

/// The bytes a program whose active screen has kitty `flags` gets for
/// `key`; `flags` is masked with [`APPLIED`], and with none of those left
/// the result is empty (the caller encodes as tmux).
///
/// An upper-case letter is its lower-case key with Shift, the Shift used
/// up by the text (the kitty spec's key codes are the unshifted ones), so
/// with flag 1 alone it is still typed as text.
///
/// # Panics
/// Only if libghostty fails to allocate.
#[must_use]
pub fn encode(key: &KittyKey, flags: u8) -> Vec<u8> {
    let flags = flags & APPLIED;
    let mut out = Vec::new();
    if flags == 0 {
        return out;
    }
    let mut mods = Mods::empty();
    let mut consumed = Mods::empty();
    let mut text = None;
    let mut unshifted = None;
    let physical = match key.base {
        KittyBase::Char(c) => {
            let lower = if c.is_ascii_uppercase() {
                mods |= Mods::SHIFT;
                consumed |= Mods::SHIFT;
                c.to_ascii_lowercase()
            } else {
                c
            };
            text = Some(c);
            unshifted = Some(lower);
            char_key(lower)
        }
        KittyBase::Enter => Key::Enter,
        KittyBase::Tab => Key::Tab,
        KittyBase::Escape => Key::Escape,
        KittyBase::Backspace => Key::Backspace,
        KittyBase::Insert => Key::Insert,
        KittyBase::Delete => Key::Delete,
        KittyBase::Home => Key::Home,
        KittyBase::End => Key::End,
        KittyBase::PageUp => Key::PageUp,
        KittyBase::PageDown => Key::PageDown,
        KittyBase::Up => Key::ArrowUp,
        KittyBase::Down => Key::ArrowDown,
        KittyBase::Left => Key::ArrowLeft,
        KittyBase::Right => Key::ArrowRight,
        KittyBase::F(n) => match F_KEYS.get(usize::from(n).wrapping_sub(1)) {
            Some(k) => *k,
            None => return out,
        },
        KittyBase::Keypad(i) => match KEYPAD.get(usize::from(i)) {
            Some(&(k, t)) => {
                text = t;
                k
            }
            None => return out,
        },
    };
    if key.shift {
        mods |= Mods::SHIFT;
    }
    if key.alt {
        mods |= Mods::ALT;
    }
    if key.ctrl {
        mods |= Mods::CTRL;
    }
    let mut event = Event::new().expect("libghostty key event");
    event
        .set_action(Action::Press)
        .set_key(physical)
        .set_mods(mods)
        .set_consumed_mods(consumed)
        .set_utf8(text.map(String::from));
    if let Some(c) = unshifted {
        event.set_unshifted_codepoint(c);
    }
    let mut encoder = Encoder::new().expect("libghostty key encoder");
    encoder.set_kitty_flags(KittyKeyFlags::from_bits_retain(flags));
    // A failure leaves nothing written, as for a key with no sequence.
    let _ = encoder.encode_to_vec(&event, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(base: KittyBase) -> KittyKey {
        KittyKey {
            base,
            shift: false,
            alt: false,
            ctrl: false,
        }
    }

    fn ctrl(base: KittyBase) -> KittyKey {
        KittyKey {
            ctrl: true,
            ..k(base)
        }
    }

    /// The spec's table (D29, *Tests*): Escape, C-c, Enter and text under
    /// flag 1, as Ghostty sends them as the outer terminal.
    #[test]
    fn disambiguate_encodes_escape_and_control_keys() {
        use KittyBase::{Char, Enter, Escape, Tab};
        let cases: [(KittyKey, &[u8]); 10] = [
            (k(Escape), b"\x1b[27u"),
            (ctrl(Char('c')), b"\x1b[99;5u"),
            (k(Enter), b"\r"),
            (k(Tab), b"\t"),
            (k(Char('a')), b"a"),
            (k(Char('A')), b"A"),
            (k(Char('\u{e9}')), "\u{e9}".as_bytes()),
            (
                KittyKey {
                    alt: true,
                    ..k(Char('a'))
                },
                b"\x1b[97;3u",
            ),
            (
                KittyKey {
                    shift: true,
                    ..k(Enter)
                },
                b"\x1b[13;2u",
            ),
            (ctrl(Char('i')), b"\x1b[105;5u"),
        ];
        for (key, want) in cases {
            assert_eq!(encode(&key, 1), want, "{key:?}");
        }
    }

    #[test]
    fn report_all_and_alternates() {
        use KittyBase::{Char, Enter, Up};
        assert_eq!(encode(&k(Char('a')), 8), b"\x1b[97u");
        assert_eq!(encode(&k(Enter), 8), b"\x1b[13u");
        assert_eq!(encode(&k(Char('A')), 8), b"\x1b[97;2u");
        assert_eq!(encode(&k(Char('A')), 8 | 4), b"\x1b[97:65;2u");
        assert_eq!(encode(&k(Up), 1), b"\x1b[A");
        assert_eq!(
            encode(
                &KittyKey {
                    ctrl: true,
                    ..k(Up)
                },
                1
            ),
            b"\x1b[1;5A"
        );
        assert_eq!(encode(&k(KittyBase::F(3)), 1), b"\x1b[13~");
        assert_eq!(encode(&k(KittyBase::Keypad(3)), 1), b"7");
        assert_eq!(encode(&k(KittyBase::Keypad(13)), 1), b"\x1b[57414u");
    }

    /// Flags 2 and 16 are not applied: alone they leave the key to tmux's
    /// encoding (empty), beside 1 they change nothing a press sends.
    #[test]
    fn release_events_and_text_flags_are_not_applied() {
        use KittyBase::{Char, Escape, Up};
        assert_eq!(encode(&k(Escape), 2), b"");
        assert_eq!(encode(&k(Escape), 16), b"");
        assert_eq!(encode(&k(Up), 1 | 2), b"\x1b[A");
        assert_eq!(encode(&k(Char('a')), 8 | 16), b"\x1b[97u");
        assert_eq!(encode(&k(Char('a')), 0), b"");
    }
}
