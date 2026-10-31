//! Key names to pane bytes for `send-keys`: the lookup of key-string.c
//! (`key_string_lookup_string`, 243-323) and the encoding of input-keys.c
//! (`input_key`, 574-708, in the standard, non-extended mode).

use vtmux_core::keys::Key as CoreKey;

/// A named key without modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    F(u8),
    Insert,
    Delete,
    Home,
    End,
    PageDown,
    PageUp,
    BackTab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Char(char),
    Named(Named),
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent key modifiers, as tmux's KEYC_* bits"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub base: Base,
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
    /// Meta came from an Escape the terminal sent before an unmodified key
    /// (tty-keys.c:118-131, no `KEYC_IMPLIED_META`): it reaches the pane
    /// as Escape plus the key's own sequence, not as an xterm modifier.
    pub esc_prefix: bool,
}

impl Key {
    /// The key as tmux numbers it (tty-keys.c `tty_keys_next1`, the table
    /// entries' `KEYC_*` values): what key tables are looked up with.
    #[must_use]
    pub fn to_core(&self) -> CoreKey {
        use Named::{
            BackTab, Backspace, Delete, Down, End, F, Home, Insert, Left, PageDown, PageUp, Right,
            Up,
        };
        let mut k = match self.base {
            Base::Char(c) => CoreKey::from_char(c),
            Base::Named(n) => match n {
                F(n) => CoreKey::function_key(n),
                Insert => CoreKey::IC,
                Delete => CoreKey::DC,
                Home => CoreKey::HOME,
                End => CoreKey::END,
                PageDown => CoreKey::NPAGE,
                PageUp => CoreKey::PPAGE,
                BackTab => CoreKey::BTAB,
                Backspace => CoreKey::BSPACE,
                Up => CoreKey::UP,
                Down => CoreKey::DOWN,
                Left => CoreKey::LEFT,
                Right => CoreKey::RIGHT,
            },
        };
        if self.ctrl {
            k = k.ctrl();
        }
        if self.meta {
            k = k.meta();
        }
        if self.shift {
            k = k.shift();
        }
        k
    }

    /// A core key as a key the pane encoder knows; `None` for keys it has no
    /// bytes for (mouse, user keys, `None`).
    #[must_use]
    pub fn from_core(key: CoreKey) -> Option<Self> {
        let base = key.base();
        let base = if let Some(c) = base.as_char() {
            Base::Char(c)
        } else {
            let named = [
                (CoreKey::IC, Named::Insert),
                (CoreKey::DC, Named::Delete),
                (CoreKey::HOME, Named::Home),
                (CoreKey::END, Named::End),
                (CoreKey::NPAGE, Named::PageDown),
                (CoreKey::PPAGE, Named::PageUp),
                (CoreKey::BTAB, Named::BackTab),
                (CoreKey::BSPACE, Named::Backspace),
                (CoreKey::UP, Named::Up),
                (CoreKey::DOWN, Named::Down),
                (CoreKey::LEFT, Named::Left),
                (CoreKey::RIGHT, Named::Right),
            ];
            let found = named.iter().find(|(k, _)| *k == base).map(|(_, n)| *n);
            Base::Named(found.or_else(|| {
                (1..=12)
                    .find(|n| CoreKey::function_key(*n) == base)
                    .map(Named::F)
            })?)
        };
        Some(Self {
            base,
            ctrl: key.has_ctrl(),
            meta: key.has_meta(),
            shift: key.has_shift(),
            esc_prefix: false,
        })
    }
}

/// The pane modes that change key encoding.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyModes {
    /// DECCKM (`MODE_KCURSOR`): cursor keys send `ESC O x`.
    pub app_cursor: bool,
}

/// `key_string_table` (key-string.c:33-110), the non-mouse, non-keypad part.
fn table(s: &str) -> Option<Base> {
    const NAMED: [(&str, Named); 23] = [
        ("IC", Named::Insert),
        ("Insert", Named::Insert),
        ("DC", Named::Delete),
        ("Delete", Named::Delete),
        ("Home", Named::Home),
        ("End", Named::End),
        ("NPage", Named::PageDown),
        ("PageDown", Named::PageDown),
        ("PgDn", Named::PageDown),
        ("PPage", Named::PageUp),
        ("PageUp", Named::PageUp),
        ("PgUp", Named::PageUp),
        ("BTab", Named::BackTab),
        ("BSpace", Named::Backspace),
        ("Up", Named::Up),
        ("Down", Named::Down),
        ("Left", Named::Left),
        ("Right", Named::Right),
        ("F1", Named::F(1)),
        ("F2", Named::F(2)),
        ("F3", Named::F(3)),
        ("F4", Named::F(4)),
        ("F5", Named::F(5)),
    ];
    const CHARS: [(&str, char); 4] = [
        ("Space", ' '),
        ("Tab", '\t'),
        ("Enter", '\r'),
        ("Escape", '\x1b'),
    ];
    if let Some((_, n)) = NAMED.iter().find(|(k, _)| k.eq_ignore_ascii_case(s)) {
        return Some(Base::Named(*n));
    }
    if let Some(n) = s
        .strip_prefix(['F', 'f'])
        .and_then(|n| n.parse::<u8>().ok())
        .filter(|n| (6..=12).contains(n))
    {
        return Some(Base::Named(Named::F(n)));
    }
    CHARS
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(s))
        .map(|(_, c)| Base::Char(*c))
}

/// `key_string_lookup_string`: `None` is `KEYC_UNKNOWN`/`KEYC_NONE` (the
/// argument is then sent literally).
#[must_use]
pub fn lookup(s: &str) -> Option<Key> {
    if s.eq_ignore_ascii_case("None") || s.eq_ignore_ascii_case("Any") {
        return None;
    }
    let mut key = Key {
        base: Base::Char('\0'),
        ctrl: false,
        meta: false,
        shift: false,
        esc_prefix: false,
    };
    let mut rest = s;
    if let Some(r) = rest.strip_prefix('^')
        && !r.is_empty()
    {
        let mut chars = r.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            key.base = Base::Char(c.to_ascii_lowercase());
            key.ctrl = true;
            return Some(key);
        }
        key.ctrl = true;
        rest = r;
    }
    // key_string_get_modifiers.
    while rest.len() >= 2 && rest.as_bytes()[1] == b'-' {
        match rest.as_bytes()[0] {
            b'C' | b'c' => key.ctrl = true,
            b'M' | b'm' => key.meta = true,
            b'S' | b's' => key.shift = true,
            _ => return None,
        }
        rest = &rest[2..];
    }
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (None, _) => None,
        (Some(c), None) if c.is_ascii() => {
            if (c as u32) < 32 {
                return None;
            }
            key.base = Base::Char(c);
            Some(key)
        }
        (Some(c), None) => {
            key.base = Base::Char(c);
            Some(key)
        }
        _ => {
            key.base = table(rest)?;
            Some(key)
        }
    }
}

fn csi_modifier(key: &Key) -> Option<u8> {
    let m = u8::from(key.shift) | (u8::from(key.meta) << 1) | (u8::from(key.ctrl) << 2);
    (m != 0).then_some(m + 1)
}

/// `input_key` for a looked-up key.
#[must_use]
pub fn encode(key: &Key, modes: KeyModes) -> Vec<u8> {
    // input-keys.c:658-671: no entry for a non-implied meta key, so write
    // Escape and then the key without meta. BTab drops its modifiers
    // first (input-keys.c:617-625).
    if key.esc_prefix && key.base != Base::Named(Named::BackTab) {
        let mut out = vec![0x1b];
        out.extend(encode(
            &Key {
                meta: false,
                esc_prefix: false,
                ..*key
            },
            modes,
        ));
        return out;
    }
    match key.base {
        Base::Named(n) => encode_named(n, key, modes),
        Base::Char(c) => encode_char(c, key),
    }
}

fn encode_named(n: Named, key: &Key, modes: KeyModes) -> Vec<u8> {
    use Named::{
        BackTab, Backspace, Delete, Down, End, F, Home, Insert, Left, PageDown, PageUp, Right, Up,
    };
    if n == Backspace {
        // The `backspace` option, default \177 (input-keys.c:592-614).
        let mut out = Vec::new();
        if key.meta {
            out.push(0x1b);
        }
        out.push(0x7f);
        return out;
    }
    if n == BackTab {
        return b"\x1b[Z".to_vec();
    }
    let modifier = csi_modifier(key);
    let (tilde, letter): (Option<&str>, Option<char>) = match n {
        Up => (None, Some('A')),
        Down => (None, Some('B')),
        Right => (None, Some('C')),
        Left => (None, Some('D')),
        Home => (Some("1"), Some('H')),
        End => (Some("4"), Some('F')),
        Insert => (Some("2"), None),
        Delete => (Some("3"), None),
        PageUp => (Some("5"), None),
        PageDown => (Some("6"), None),
        F(1) => (None, Some('P')),
        F(2) => (None, Some('Q')),
        F(3) => (None, Some('R')),
        F(4) => (None, Some('S')),
        F(k) => (
            Some(match k {
                5 => "15",
                6 => "17",
                7 => "18",
                8 => "19",
                9 => "20",
                10 => "21",
                11 => "23",
                _ => "24",
            }),
            None,
        ),
        Backspace | BackTab => unreachable!(),
    };
    let cursor = matches!(n, Up | Down | Left | Right);
    let function = matches!(n, F(1..=4));
    match modifier {
        Some(m) => match (n, letter) {
            (Home | End | Up | Down | Left | Right | F(1..=4), Some(l)) => {
                format!("\x1b[1;{m}{l}").into_bytes()
            }
            _ => format!("\x1b[{};{m}~", tilde.unwrap_or("")).into_bytes(),
        },
        None if cursor => {
            let l = letter.expect("cursor key letter");
            if modes.app_cursor {
                format!("\x1bO{l}").into_bytes()
            } else {
                format!("\x1b[{l}").into_bytes()
            }
        }
        None if function => format!("\x1bO{}", letter.expect("F1-F4 letter")).into_bytes(),
        None => format!("\x1b[{}~", tilde.expect("tilde key")).into_bytes(),
    }
}

/// `input_key_vt10x` (input-keys.c:482-541) and the trivial case before it.
fn encode_char(c: char, key: &Key) -> Vec<u8> {
    let mut out = Vec::new();
    if key.meta {
        out.push(0x1b);
    }
    if !c.is_ascii() {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return out;
    }
    let mut b = c as u8;
    if key.ctrl && !matches!(b, b'\r' | b'\n' | b'\t') {
        const FROM: &[u8] = b"1!9(0)=+;:'\",<.>/-8? 2";
        const TO: &[u8] = b"119900=+;;'',,..\x1f\x1f\x7f\x7f\0\0";
        if let Some(i) = FROM.iter().position(|x| *x == b) {
            b = TO[i];
        } else if (b'3'..=b'7').contains(&b) {
            b -= 0x18;
        } else if (b'@'..=b'~').contains(&b) {
            b &= 0x1f;
        } else {
            return Vec::new();
        }
    }
    out.push(b & 0x7f);
    out
}

/// The bytes `send-keys` writes for one argument: a key name if it is one,
/// otherwise the text itself, character by character
/// (cmd-send-keys.c `cmd_send_keys_inject_string`).
#[must_use]
pub fn send_keys_bytes(arg: &str, literal: bool, modes: KeyModes) -> Vec<u8> {
    if !literal && let Some(key) = lookup(arg) {
        return encode(&key, modes);
    }
    vtmux_core::rawstr::to_bytes(arg).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(arg: &str) -> Vec<u8> {
        send_keys_bytes(arg, false, KeyModes::default())
    }

    #[test]
    fn names_and_modifiers() {
        assert_eq!(b("Enter"), b"\r");
        assert_eq!(b("enter"), b"\r");
        assert_eq!(b("Space"), b" ");
        assert_eq!(b("Tab"), b"\t");
        assert_eq!(b("Escape"), b"\x1b");
        assert_eq!(b("C-c"), b"\x03");
        assert_eq!(b("C-d"), b"\x04");
        assert_eq!(b("^C"), b"\x03");
        assert_eq!(b("M-r"), b"\x1br");
        assert_eq!(b("BSpace"), b"\x7f");
        assert_eq!(b("Up"), b"\x1b[A");
        assert_eq!(b("Home"), b"\x1b[1~");
        assert_eq!(b("NPage"), b"\x1b[6~");
        assert_eq!(b("F1"), b"\x1bOP");
        assert_eq!(b("F5"), b"\x1b[15~");
        assert_eq!(b("C-Up"), b"\x1b[1;5A");
        assert_eq!(b("S"), b"S");
        assert_eq!(b("hello"), b"hello");
        assert_eq!(
            send_keys_bytes("Enter", true, KeyModes::default()),
            b"Enter"
        );
        assert_eq!(
            send_keys_bytes("Up", false, KeyModes { app_cursor: true }),
            b"\x1bOA"
        );
    }

    #[test]
    fn decoded_keys_are_tmux_key_codes() {
        let k = |base, ctrl, meta, shift| Key {
            base,
            ctrl,
            meta,
            shift,
            esc_prefix: false,
        };
        for (key, name) in [
            (k(Base::Char('b'), true, false, false), "C-b"),
            (k(Base::Char('%'), false, false, false), "%"),
            (k(Base::Char('x'), false, true, false), "M-x"),
            (k(Base::Char(' '), true, false, false), "C-Space"),
            (k(Base::Char('\r'), false, false, false), "Enter"),
            (k(Base::Char('\x1b'), false, false, false), "Escape"),
            (k(Base::Char('é'), false, false, false), "é"),
            (k(Base::Named(Named::Left), false, false, true), "S-Left"),
            (k(Base::Named(Named::F(5)), false, false, false), "F5"),
            (k(Base::Named(Named::PageUp), false, false, false), "PPage"),
            (
                k(Base::Named(Named::Backspace), false, false, false),
                "BSpace",
            ),
        ] {
            let core = key.to_core();
            assert_eq!(core.to_string(), name);
            assert_eq!(
                CoreKey::from_name(name).map(CoreKey::without_flags),
                Some(core),
                "{name}"
            );
            assert_eq!(Key::from_core(core), Some(key), "{name}");
        }
        assert_eq!(Key::from_core(CoreKey::NONE), None);
    }

    #[test]
    fn escape_prefixed_keys_keep_their_own_sequence() {
        let up = Key {
            base: Base::Named(Named::Up),
            ctrl: false,
            meta: true,
            shift: false,
            esc_prefix: true,
        };
        assert_eq!(encode(&up, KeyModes::default()), b"\x1b\x1b[A");
        assert_eq!(encode(&up, KeyModes { app_cursor: true }), b"\x1b\x1bOA");
        let implied = Key {
            esc_prefix: false,
            ..up
        };
        assert_eq!(encode(&implied, KeyModes::default()), b"\x1b[1;3A");
        let btab = Key {
            base: Base::Named(Named::BackTab),
            ..up
        };
        assert_eq!(encode(&btab, KeyModes::default()), b"\x1b[Z");
    }
}
