//! Key names to pane bytes for `send-keys`: the lookup of key-string.c
//! (`key_string_lookup_string`, 243-323) and the encoding of input-keys.c
//! (`input_key`, 574-708), in the pane's key mode (standard, or xterm's
//! modifyOtherKeys 1 or 2) and `extended-keys-format`.

use vtmux_core::keys::Key as CoreKey;
use vtmux_vt::KeyMode;

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
    /// A numeric keypad key, numbered in tmux's order (`KP/` 0 to `KP.`
    /// 15, tmux.h:392-407).
    Keypad(u8),
}

/// The keypad keys in [`Named::Keypad`] order: the final byte of their
/// application-mode `ESC O x` sequence (tty-keys.c:87-102) and what
/// they send in numeric mode (input-keys.c:200-247).
pub const KEYPAD_SS3: &[u8; 16] = b"ojmwxyktuvqrsMpn";
const KEYPAD_NUMERIC: &[u8; 16] = b"/*-789+456123\n0.";

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
            BackTab, Backspace, Delete, Down, End, F, Home, Insert, Keypad, Left, PageDown, PageUp,
            Right, Up,
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
                Keypad(n) => CoreKey::keypad(n),
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
            Base::Named(
                found
                    .or_else(|| {
                        (1..=12)
                            .find(|n| CoreKey::function_key(*n) == base)
                            .map(Named::F)
                    })
                    .or_else(|| base.keypad_index().map(Named::Keypad))?,
            )
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
    /// DECKPAM (`MODE_KKEYPAD`): keypad keys send `ESC O x`.
    pub app_keypad: bool,
    /// The extended key mode the program asked for (`#{pane_key_mode}`).
    pub key_mode: KeyMode,
    /// `extended-keys-format csi-u`: extended keys are `CSI key ; mod u`,
    /// not xterm's `CSI 27 ; mod ; key ~`.
    pub csi_u: bool,
}

impl KeyModes {
    /// The modes of a pane's terminal, with the server's
    /// `extended-keys-format` (`csi_u`).
    #[must_use]
    pub fn of(term: &vtmux_vt::PaneTerm, csi_u: bool) -> Self {
        Self {
            app_cursor: term.app_cursor(),
            app_keypad: term.app_keypad(),
            key_mode: term.key_mode(),
            csi_u,
        }
    }
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
    // The keypad names (key-string.c:108-124).
    if s.len() >= 3
        && let Some(n) = CoreKey::from_name(s).and_then(CoreKey::keypad_index)
    {
        return Some(Base::Named(Named::Keypad(n)));
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
    // key-string.c:268-290: `0xNN` is a character by number.
    if rest.starts_with("0x") {
        let core = CoreKey::from_name(rest).filter(|k| *k != CoreKey::NONE)?;
        let found = Key::from_core(core)?;
        key.base = found.base;
        return Some(key);
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

/// `input_key` for a looked-up key: the bytes for the pane, written even
/// when tmux gives up on the key part way (an Escape for Meta may already
/// be out).
#[must_use]
pub fn encode(key: &Key, modes: KeyModes) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(key, modes, &mut out);
    out
}

/// `input_key` into `out`; `false` where it returns -1: a key with no
/// sequence in the pane's mode (`C-BSpace` in standard mode, a control
/// character in an extended mode). What was written by then stays.
pub fn encode_into(key: &Key, modes: KeyModes, out: &mut Vec<u8>) -> bool {
    let mut key = *key;
    // input-keys.c:592-614: an unmodified Backspace is the `backspace`
    // option (default \177); a modified one is that character modified.
    if key.base == Base::Named(Named::Backspace) {
        if !key.ctrl && !key.meta && !key.shift {
            out.push(0x7f);
            return true;
        }
        key.base = Base::Char('\x7f');
    }
    // input-keys.c:617-625: BTab is S-Tab in mode 2, and otherwise loses
    // its modifiers.
    if key.base == Base::Named(Named::BackTab) {
        if modes.key_mode == KeyMode::Extended2 {
            key.base = Base::Char('\t');
            key.shift = true;
        } else {
            key.ctrl = false;
            key.meta = false;
            key.shift = false;
            key.esc_prefix = false;
        }
    }
    match key.base {
        Base::Char(c) => encode_char(c, &key, modes, out),
        Base::Named(n) => {
            // input-keys.c:658-671: no entry for a non-implied meta key, so
            // write Escape and then the key without meta.
            if key.esc_prefix {
                out.push(0x1b);
                key.meta = false;
            }
            out.extend(encode_named(n, &key, modes));
            true
        }
    }
}

fn encode_named(n: Named, key: &Key, modes: KeyModes) -> Vec<u8> {
    use Named::{
        BackTab, Backspace, Delete, Down, End, F, Home, Insert, Keypad, Left, PageDown, PageUp,
        Right, Up,
    };
    if let Keypad(i) = n {
        // input_key: application mode's `ESC O x`, else the character;
        // Meta is an Escape before either, other modifiers send nothing
        // (input_key_vt10x has no mapping for them).
        if key.ctrl || key.shift {
            return Vec::new();
        }
        let mut out = Vec::new();
        if key.meta {
            out.push(0x1b);
        }
        if modes.app_keypad {
            out.extend_from_slice(b"\x1bO");
            out.push(KEYPAD_SS3[usize::from(i)]);
        } else {
            out.push(KEYPAD_NUMERIC[usize::from(i)]);
        }
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
        Backspace | BackTab | Keypad(_) => unreachable!(),
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

/// `input_key` for a character (input-keys.c:627-708): unmodified text is
/// itself, the rest goes by the pane's key mode.
fn encode_char(c: char, key: &Key, modes: KeyModes, out: &mut Vec<u8>) -> bool {
    if !key.ctrl
        && !key.meta
        && !key.shift
        && (matches!(c, '\t' | '\r' | '\x1b') || ('\x20'..='\x7f').contains(&c) || !c.is_ascii())
    {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return true;
    }
    match modes.key_mode {
        KeyMode::Extended2 => encode_extended(c, key, modes.csi_u, out),
        KeyMode::Extended1 if !mode1_is_standard(c, key) => {
            encode_extended(c, key, modes.csi_u, out)
        }
        KeyMode::Extended1 | KeyMode::Standard => encode_vt10x(c, key, out),
    }
}

/// `input_key_mode1` (input-keys.c:544-568): the keys modifyOtherKeys 1
/// still sends as standard keys, per xterm's modified-keys table.
fn mode1_is_standard(c: char, key: &Key) -> bool {
    if key.meta && !key.ctrl {
        return true;
    }
    key.ctrl && matches!(c, ' ' | '/' | '^' | '2'..='8' | '@'..='~')
}

/// `input_key_extended` (input-keys.c:424-473): `CSI 27 ; mod ; key ~`, or
/// `CSI key ; mod u` under `extended-keys-format csi-u`; nothing for an
/// unmodified key.
fn encode_extended(c: char, key: &Key, csi_u: bool, out: &mut Vec<u8>) -> bool {
    let Some(m) = csi_modifier(key) else {
        return false;
    };
    let n = u32::from(c);
    let seq = if csi_u {
        format!("\x1b[{n};{m}u")
    } else {
        format!("\x1b[27;{m};{n}~")
    };
    out.extend_from_slice(seq.as_bytes());
    true
}

/// `input_key_vt10x` (input-keys.c:482-541).
fn encode_vt10x(c: char, key: &Key, out: &mut Vec<u8>) -> bool {
    if key.meta {
        out.push(0x1b);
    }
    // No way to send a modified non-ASCII character: the modifiers go.
    if !c.is_ascii() {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return true;
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
            return false;
        }
    }
    out.push(b & 0x7f);
    true
}

/// Text sent as characters (`cmd_send_keys_inject_string`'s literal
/// path): each character is a key of its own, and a byte that is not
/// UTF-8 goes as it is.
fn literal_bytes(arg: &str, modes: KeyModes, out: &mut Vec<u8>) {
    for unit in vtmux_core::rawstr::units(arg) {
        match unit {
            vtmux_core::rawstr::Unit::Byte(b) => out.push(b),
            vtmux_core::rawstr::Unit::Char(c) => {
                let key = Key {
                    base: Base::Char(c),
                    ctrl: false,
                    meta: false,
                    shift: false,
                    esc_prefix: false,
                };
                encode_into(&key, modes, out);
            }
        }
    }
}

/// The bytes `send-keys` writes for one argument: a key name if it is one,
/// otherwise the text itself, character by character
/// (cmd-send-keys.c `cmd_send_keys_inject_string`).
/// A key `input_key` gives up on is sent as text after all
/// (cmd-send-keys.c:131-141), after whatever it wrote first.
#[must_use]
pub fn send_keys_bytes(arg: &str, literal: bool, modes: KeyModes) -> Vec<u8> {
    let mut out = Vec::new();
    if !literal
        && let Some(key) = lookup(arg)
        && encode_into(&key, modes, &mut out)
    {
        return out;
    }
    literal_bytes(arg, modes, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtmux_vt::KeyMode;

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
            send_keys_bytes(
                "Up",
                false,
                KeyModes {
                    app_cursor: true,
                    ..KeyModes::default()
                }
            ),
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
        assert_eq!(
            encode(
                &up,
                KeyModes {
                    app_cursor: true,
                    ..KeyModes::default()
                }
            ),
            b"\x1b\x1bOA"
        );
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

    /// The bytes a pane gets from `send-keys KEY` in each key mode and
    /// `extended-keys-format`. From tmux 3.8, isolated (`-L`, `-f
    /// /dev/null`): `set -g extended-keys on; set -g extended-keys-format
    /// F; new -d "stty raw -echo; printf MODE; exec cat > OUT"`, then
    /// `send-keys KEY; send-keys -H 7c` per key, OUT read with `od` (tmp
    /// probe.sh). Standard mode is the same in both formats. Columns:
    /// standard, mode 1 csi-u, mode 1 xterm, mode 2 csi-u, mode 2 xterm.
    #[test]
    fn send_keys_in_each_key_mode_and_format() {
        #[rustfmt::skip]
        let table: [(&str, [&str; 5]); 51] = [
            ("C-a", ["\x01", "\x01", "\x01", "\\e[97;5u", "\\e[27;5;97~"]),
            ("C-S-a", ["\x01", "\x01", "\x01", "\\e[97;6u", "\\e[27;6;97~"]),
            ("M-a", ["\\ea", "\\ea", "\\ea", "\\e[97;3u", "\\e[27;3;97~"]),
            ("C-M-a", ["\\e\x01", "\\e\x01", "\\e\x01", "\\e[97;7u", "\\e[27;7;97~"]),
            ("S-a", ["a", "\\e[97;2u", "\\e[27;2;97~", "\\e[97;2u", "\\e[27;2;97~"]),
            ("A", ["A", "A", "A", "A", "A"]),
            ("Enter", ["\x0d", "\x0d", "\x0d", "\x0d", "\x0d"]),
            ("S-Enter", ["\x0d", "\\e[13;2u", "\\e[27;2;13~", "\\e[13;2u", "\\e[27;2;13~"]),
            ("C-Enter", ["\x0d", "\\e[13;5u", "\\e[27;5;13~", "\\e[13;5u", "\\e[27;5;13~"]),
            ("M-Enter", ["\\e\x0d", "\\e\x0d", "\\e\x0d", "\\e[13;3u", "\\e[27;3;13~"]),
            ("Tab", ["\x09", "\x09", "\x09", "\x09", "\x09"]),
            ("C-Tab", ["\x09", "\\e[9;5u", "\\e[27;5;9~", "\\e[9;5u", "\\e[27;5;9~"]),
            ("BTab", ["\\e[Z", "\\e[Z", "\\e[Z", "\\e[9;2u", "\\e[27;2;9~"]),
            ("S-Tab", ["\x09", "\\e[9;2u", "\\e[27;2;9~", "\\e[9;2u", "\\e[27;2;9~"]),
            ("C-1", ["1", "\\e[49;5u", "\\e[27;5;49~", "\\e[49;5u", "\\e[27;5;49~"]),
            ("C-/", ["\x1f", "\x1f", "\x1f", "\\e[47;5u", "\\e[27;5;47~"]),
            ("C-Space", ["\x00", "\x00", "\x00", "\\e[32;5u", "\\e[27;5;32~"]),
            ("C-@", ["\x00", "\x00", "\x00", "\\e[64;5u", "\\e[27;5;64~"]),
            ("C-é", ["é", "\\e[233;5u", "\\e[27;5;233~", "\\e[233;5u", "\\e[27;5;233~"]),
            ("M-é", ["\\eé", "\\eé", "\\eé", "\\e[233;3u", "\\e[27;3;233~"]),
            ("é", ["é", "é", "é", "é", "é"]),
            ("S-Up", ["\\e[1;2A", "\\e[1;2A", "\\e[1;2A", "\\e[1;2A", "\\e[1;2A"]),
            ("C-Left", ["\\e[1;5D", "\\e[1;5D", "\\e[1;5D", "\\e[1;5D", "\\e[1;5D"]),
            ("F5", ["\\e[15~", "\\e[15~", "\\e[15~", "\\e[15~", "\\e[15~"]),
            ("C-F5", ["\\e[15;5~", "\\e[15;5~", "\\e[15;5~", "\\e[15;5~", "\\e[15;5~"]),
            ("BSpace", ["\x7f", "\x7f", "\x7f", "\x7f", "\x7f"]),
            ("C-BSpace", ["C-BSpace", "\\e[127;5u", "\\e[27;5;127~", "\\e[127;5u", "\\e[27;5;127~"]),
            ("M-BSpace", ["\\e\x7f", "\\e\x7f", "\\e\x7f", "\\e[127;3u", "\\e[27;3;127~"]),
            ("S-BSpace", ["\x7f", "\\e[127;2u", "\\e[27;2;127~", "\\e[127;2u", "\\e[27;2;127~"]),
            ("C-M-BSpace", ["\\eC-M-BSpace", "\\e[127;7u", "\\e[27;7;127~", "\\e[127;7u", "\\e[27;7;127~"]),
            ("Escape", ["\\e", "\\e", "\\e", "\\e", "\\e"]),
            ("M-Escape", ["\\e\\e", "\\e\\e", "\\e\\e", "\\e[27;3u", "\\e[27;3;27~"]),
            ("C-Escape", ["C-Escape", "\\e[27;5u", "\\e[27;5;27~", "\\e[27;5u", "\\e[27;5;27~"]),
            ("C-M-Escape", ["\\eC-M-Escape", "\\e[27;7u", "\\e[27;7;27~", "\\e[27;7u", "\\e[27;7;27~"]),
            ("C-i", ["\x09", "\x09", "\x09", "\\e[105;5u", "\\e[27;5;105~"]),
            ("KP1", ["1", "1", "1", "1", "1"]),
            ("C-KP1", ["", "", "", "", ""]),
            ("M-KP1", ["\\e1", "\\e1", "\\e1", "\\e1", "\\e1"]),
            ("C-[", ["\\e", "\\e", "\\e", "\\e[91;5u", "\\e[27;5;91~"]),
            ("C-?", ["\x7f", "\\e[63;5u", "\\e[27;5;63~", "\\e[63;5u", "\\e[27;5;63~"]),
            ("C-3", ["\\e", "\\e", "\\e", "\\e[51;5u", "\\e[27;5;51~"]),
            ("C-8", ["\x7f", "\x7f", "\x7f", "\\e[56;5u", "\\e[27;5;56~"]),
            ("C-~", ["\x1e", "\x1e", "\x1e", "\\e[126;5u", "\\e[27;5;126~"]),
            ("C-M-S-x", ["\\e\x18", "\\e\x18", "\\e\x18", "\\e[120;8u", "\\e[27;8;120~"]),
            ("Space", [" ", " ", " ", " ", " "]),
            ("S-Space", [" ", "\\e[32;2u", "\\e[27;2;32~", "\\e[32;2u", "\\e[27;2;32~"]),
            ("M-Space", ["\\e ", "\\e ", "\\e ", "\\e[32;3u", "\\e[27;3;32~"]),
            ("M-BTab", ["\\e[Z", "\\e[Z", "\\e[Z", "\\e[9;4u", "\\e[27;4;9~"]),
            ("C-BTab", ["\\e[Z", "\\e[Z", "\\e[Z", "\\e[9;6u", "\\e[27;6;9~"]),
            ("M-S-Tab", ["\\e\x09", "\\e\x09", "\\e\x09", "\\e[9;4u", "\\e[27;4;9~"]),
            ("M-Up", ["\\e[1;3A", "\\e[1;3A", "\\e[1;3A", "\\e[1;3A", "\\e[1;3A"]),
        ];
        let column = [
            (KeyMode::Standard, true),
            (KeyMode::Extended1, true),
            (KeyMode::Extended1, false),
            (KeyMode::Extended2, true),
            (KeyMode::Extended2, false),
        ];
        for (key, want) in table {
            for ((key_mode, csi_u), want) in column.into_iter().zip(want) {
                let modes = KeyModes {
                    key_mode,
                    csi_u,
                    ..KeyModes::default()
                };
                let got = send_keys_bytes(key, false, modes);
                assert_eq!(
                    String::from_utf8_lossy(&got),
                    want.replace("\\e", "\x1b"),
                    "{key} {key_mode:?} csi-u={csi_u}"
                );
                // Standard mode is the same in either format.
                if key_mode == KeyMode::Standard {
                    let xterm = KeyModes {
                        csi_u: false,
                        ..modes
                    };
                    assert_eq!(send_keys_bytes(key, false, xterm), got, "{key}");
                }
            }
        }
    }

    /// Text is sent a character at a time, each a key: in modes 1 and 2
    /// control characters have no sequence and are dropped; a byte that is not
    /// UTF-8 goes as it is. tmux 3.8 as above with `send-keys -l
    /// 'a<LF>b<SOH>c<DEL>'` and `send-keys 'x<SOH>'`; a lone 0xff with
    /// `printf '\377'`.
    #[test]
    fn text_is_sent_as_keys() {
        let mode = |key_mode| KeyModes {
            key_mode,
            csi_u: true,
            ..KeyModes::default()
        };
        for (key_mode, want) in [
            (KeyMode::Standard, &b"a\nb\x01c\x7fx\x01"[..]),
            (KeyMode::Extended1, b"abc\x7fx"),
            (KeyMode::Extended2, b"abc\x7fx"),
        ] {
            let mut got = send_keys_bytes("a\nb\x01c\x7f", true, mode(key_mode));
            got.extend(send_keys_bytes("x\x01", false, mode(key_mode)));
            assert_eq!(got, want, "{key_mode:?}");
        }
        let raw = vtmux_core::rawstr::from_bytes(b"a\xffb");
        assert_eq!(
            send_keys_bytes(&raw, true, mode(KeyMode::Extended2)),
            b"a\xffb"
        );
    }
}
