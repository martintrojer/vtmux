//! Key strings: `key_string_lookup_string` and `key_string_lookup_key`
//! (key-string.c), both directions.

use std::fmt::Write as _;
use std::sync::LazyLock;

use super::code::{
    BUILD_MODIFIERS, CTRL, CURSOR, IMPLIED_META, KEYPAD, Key, LITERAL, LOCATIONS, MASK_FLAGS,
    MASK_KEY, META, NUSER, SENT, SHIFT, ty,
};
use super::width::char_width;

/// `KEYC_MAKE_MOUSE_KEY` (tmux.h:273).
const fn mouse(kind: u64, button: u64, location: u64) -> u64 {
    kind << 32 | button << 8 | location
}

const C0_NAMES: [&str; 32] = [
    "[NUL]", "[SOH]", "[STX]", "[ETX]", "[EOT]", "[ENQ]", "[ASC]", "[BEL]", "[BS]", "Tab", "[LF]",
    "[VT]", "[FF]", "Enter", "[SO]", "[SI]", "[DLE]", "[DC1]", "[DC2]", "[DC3]", "[DC4]", "[NAK]",
    "[SYN]", "[ETB]", "[CAN]", "[EM]", "[SUB]", "Escape", "[FS]", "[GS]", "[RS]", "[US]",
];

/// `key_string_table` (key-string.c:33-216), in its order: lookup by name
/// takes the first match, and lookup by key the first match too (`IC` before
/// `Insert`, `NPage` before `PgDn`).
static TABLE: LazyLock<Vec<(String, u64)>> = LazyLock::new(|| {
    let f = |n: u64| Key::F1.0 + n;
    let mut t: Vec<(String, u64)> = Vec::new();
    let mut add = |name: &str, code: u64| t.push((name.to_owned(), code));
    // Function keys.
    for n in 0..12 {
        add(&format!("F{}", n + 1), f(n) | IMPLIED_META);
    }
    for (name, key) in [
        ("IC", Key::IC),
        ("Insert", Key::IC),
        ("DC", Key::DC),
        ("Delete", Key::DC),
        ("Home", Key::HOME),
        ("End", Key::END),
        ("NPage", Key::NPAGE),
        ("PageDown", Key::NPAGE),
        ("PgDn", Key::NPAGE),
        ("PPage", Key::PPAGE),
        ("PageUp", Key::PPAGE),
        ("PgUp", Key::PPAGE),
    ] {
        add(name, key.0 | IMPLIED_META);
    }
    add("BTab", Key::BTAB.0);
    add("Space", u64::from(b' '));
    add("BSpace", Key::BSPACE.0);
    // C0 control characters.
    for (n, name) in C0_NAMES.iter().enumerate() {
        add(name, n as u64);
    }
    // Arrow keys.
    for (name, key) in [
        ("Up", Key::UP),
        ("Down", Key::DOWN),
        ("Left", Key::LEFT),
        ("Right", Key::RIGHT),
    ] {
        add(name, key.0 | CURSOR | IMPLIED_META);
    }
    // Numeric keypad: the codes run KP_SLASH.. in this order.
    for (n, name) in [
        "KP/", "KP*", "KP-", "KP7", "KP8", "KP9", "KP+", "KP4", "KP5", "KP6", "KP1", "KP2", "KP3",
        "KPEnter", "KP0", "KP.",
    ]
    .iter()
    .enumerate()
    {
        add(name, (Key::KP_SLASH.0 + n as u64) | KEYPAD);
    }
    // Mouse keys. Buttons 4 and 5 are the wheel, which has its own types.
    let buttons = [1, 2, 3, 6, 7, 8, 9, 10, 11];
    let mut mouse_keys = |prefix: &str, kind: u64, buttons: &[u64]| {
        for &button in buttons {
            let name = if button == 0 {
                prefix.to_owned()
            } else {
                format!("{prefix}{button}")
            };
            // KEYC_MOUSE_STRING order: Border comes after Empty only in the
            // enum; the strings list it after Empty too.
            for suffix in [
                "Pane",
                "Status",
                "StatusLeft",
                "StatusRight",
                "StatusDefault",
                "ScrollbarUp",
                "ScrollbarSlider",
                "ScrollbarDown",
                "Empty",
                "Border",
                "Control0",
                "Control1",
                "Control2",
                "Control3",
                "Control4",
                "Control5",
                "Control6",
                "Control7",
                "Control8",
                "Control9",
            ] {
                let location = LOCATIONS
                    .iter()
                    .position(|l| *l == suffix)
                    .expect("known location") as u64;
                t.push((format!("{name}{suffix}"), mouse(kind, button, location)));
            }
        }
    };
    mouse_keys("MouseDown", ty::MOUSEDOWN, &buttons);
    mouse_keys("MouseUp", ty::MOUSEUP, &buttons);
    mouse_keys("MouseDrag", ty::MOUSEDRAG, &buttons);
    mouse_keys("MouseDragEnd", ty::MOUSEDRAGEND, &buttons);
    mouse_keys("WheelUp", ty::WHEELUP, &[0]);
    mouse_keys("WheelDown", ty::WHEELDOWN, &[0]);
    mouse_keys("SecondClick", ty::SECONDCLICK, &buttons);
    mouse_keys("DoubleClick", ty::DOUBLECLICK, &buttons);
    mouse_keys("TripleClick", ty::TRIPLECLICK, &buttons);
    t
});

/// `key_string_search_table`: `strcasecmp` over the table, then `UserN`.
fn search_table(s: &str) -> Option<u64> {
    if let Some((_, key)) = TABLE.iter().find(|(name, _)| name.eq_ignore_ascii_case(s)) {
        return Some(*key);
    }
    // sscanf(string, "User%u", &user) == 1 && user <= KEYC_NUSER
    let user = sscanf_unsigned(s.strip_prefix("User")?, 10)?;
    (u64::from(user) <= NUSER).then(|| Key::USER_BASE + u64::from(user))
}

impl Key {
    pub(super) const USER_BASE: u64 = ty::USER << 32;
}

/// The part of C `sscanf("%u")` / `sscanf("%x")` that matters here: skip
/// leading whitespace, an optional sign, for base 16 an optional `0x`, then
/// digits (at least one). The value is `strtoul`'s (a negative number wraps,
/// an overflow is `ULONG_MAX`) truncated to `unsigned int`.
fn sscanf_unsigned(s: &str, base: u32) -> Option<u32> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\u{b}', '\u{c}', '\r']);
    let (negative, s) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let s = if base == 16 {
        s.strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s)
    } else {
        s
    };
    let digits: Vec<u32> = s.chars().map_while(|c| c.to_digit(base)).collect();
    if digits.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    let mut overflow = false;
    for d in digits {
        match value
            .checked_mul(u64::from(base))
            .and_then(|v| v.checked_add(u64::from(d)))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
    }
    let value = if overflow {
        u64::MAX
    } else if negative {
        value.wrapping_neg()
    } else {
        value
    };
    Some(
        value.to_le_bytes()[..4]
            .iter()
            .rev()
            .fold(0, |acc, b| acc << 8 | u32::from(*b)),
    )
}

/// `utf8_from_data`: the `utf8_char` for a character, which is the key code
/// of a Unicode key. Size in bits 24-28, width+1 in bits 29-31, and either
/// the bytes (up to 3) or tmux's index for the character.
///
/// tmux hands out indexes in the order it first sees each four-byte
/// character; here the code point is the index (see the module docs of
/// `keys`).
fn utf8_char(c: char) -> u64 {
    let cp = u32::from(c);
    let width = if c.is_ascii() { 1 } else { char_width(cp) };
    let size = u32::try_from(c.len_utf8()).expect("at most four bytes");
    let data = if size <= 3 {
        let mut buf = [0u8; 4];
        c.encode_utf8(&mut buf);
        u32::from(buf[0]) | u32::from(buf[1]) << 8 | u32::from(buf[2]) << 16
    } else {
        cp
    };
    u64::from(size << 24 | (u32::from(width) + 1) << 29 | data)
}

/// `utf8_to_data` for a Unicode key's code: the character, or what tmux
/// prints for one it cannot (a space per byte).
fn utf8_text(uc: u64) -> String {
    let size = (uc >> 24 & 0x1f) as usize;
    if size <= 3 {
        let bytes = [byte(uc, 0), byte(uc, 1), byte(uc, 2)];
        String::from_utf8_lossy(&bytes[..size]).into_owned()
    } else {
        // The four-byte index; a code point above U+10FFFF has no UTF-8
        // string form in Rust (tmux writes its four bytes).
        char::from_u32((uc & 0xff_ffff) as u32).map_or_else(|| "\u{fffd}".to_owned(), String::from)
    }
}

/// Byte `n` (0 is the least significant) of `v`.
const fn byte(v: u64, n: u32) -> u8 {
    v.to_le_bytes()[n as usize]
}

/// `key_string_get_modifiers`: leading `C-`, `M-`, `S-` (either case).
/// `Err` is tmux's `*string = NULL`: a letter other than those before `-`.
fn get_modifiers(s: &mut &str) -> Result<u64, ()> {
    let mut modifiers = 0;
    while let [first, b'-', ..] = s.as_bytes() {
        modifiers |= match first {
            b'C' | b'c' => CTRL,
            b'M' | b'm' => META,
            b'S' | b's' => SHIFT,
            _ => return Err(()),
        };
        *s = &s[2..];
    }
    Ok(modifiers)
}

impl Key {
    /// `key_string_lookup_string`. `None` is `KEYC_UNKNOWN`; `"None"` is
    /// `Some(Key::NONE)`, which callers reject themselves.
    ///
    /// Nothing is normalised: `C-i` is not `Tab`, `C-A` is not `C-a`
    /// (`^A` is `C-a`: key-string.c:310), `0x41` is not `A`.
    #[must_use]
    pub fn from_name(string: &str) -> Option<Self> {
        if string.eq_ignore_ascii_case("None") {
            return Some(Self::NONE);
        }
        if string.eq_ignore_ascii_case("Any") {
            return Some(Self::ANY);
        }
        if let Some(hex) = string.strip_prefix("0x") {
            return from_hex(hex);
        }

        let mut string = string;
        let mut modifiers = 0;
        // Short Ctrl key: `^x`.
        if let [b'^', second, rest @ ..] = string.as_bytes() {
            if rest.is_empty() {
                return Some(Self(u64::from(second.to_ascii_lowercase()) | CTRL));
            }
            modifiers |= CTRL;
            string = &string[1..];
        }
        modifiers |= get_modifiers(&mut string).ok()?;
        if string.is_empty() {
            return None;
        }

        let mut chars = string.chars();
        let first = chars.next()?;
        let single = chars.next().is_none();
        let key = if first.is_ascii() && single {
            // A standard ASCII key; controls are not keys.
            if u32::from(first) < 32 {
                return None;
            }
            u64::from(first as u8)
        } else if !first.is_ascii() {
            // A UTF-8 key: exactly one character, or unknown
            // (`strlen(string) != ud.size`).
            if !single {
                return None;
            }
            return Some(Self(utf8_char(first) | modifiers));
        } else {
            let mut key = search_table(string)?;
            if modifiers & META == 0 {
                key &= !IMPLIED_META;
            }
            key
        };
        Some(Self(key | modifiers))
    }

    /// `key_string_lookup_key`: the name of a key, with its flags as
    /// `[LKCIBS]` when `with_flags` and any are set.
    #[must_use]
    pub fn name(self, with_flags: bool) -> String {
        let saved = self.0;
        let mut out = String::new();
        let mut key = saved;
        'name: {
            // Literal keys are themselves.
            if key & LITERAL != 0 {
                out.push(char::from((key & 0xff) as u8));
                break 'name;
            }
            if key & CTRL != 0 {
                out.push_str("C-");
            }
            if key & META != 0 {
                out.push_str("M-");
            }
            if key & SHIFT != 0 {
                out.push_str("S-");
            }
            key &= MASK_KEY;

            let special = [
                (Self::NONE, "None"),
                (Self::UNKNOWN, "Unknown"),
                (Self::ANY, "Any"),
                (Self::FOCUS_IN, "FocusIn"),
                (Self::FOCUS_OUT, "FocusOut"),
                (Self::PASTE_START, "PasteStart"),
                (Self::PASTE_END, "PasteEnd"),
                (Self::REPORT_DARK_THEME, "ReportDarkTheme"),
                (Self::REPORT_LIGHT_THEME, "ReportLightTheme"),
                (Self::MOUSE, "Mouse"),
                (Self::DRAGGING, "Dragging"),
                (Self(mouse(ty::MOUSEMOVE, 0, 0)), "MouseMovePane"),
                (Self(mouse(ty::MOUSEMOVE, 0, 1)), "MouseMoveStatus"),
                (Self(mouse(ty::MOUSEMOVE, 0, 2)), "MouseMoveStatusLeft"),
                (Self(mouse(ty::MOUSEMOVE, 0, 3)), "MouseMoveStatusRight"),
                (Self(mouse(ty::MOUSEMOVE, 0, 5)), "MouseMoveBorder"),
            ];
            if let Some((_, name)) = special.iter().find(|(k, _)| k.0 == key) {
                out.push_str(name);
                break 'name;
            }
            if key >> 32 & 0xff == ty::USER {
                // snprintf into `char tmp[8]` (key-string.c:386): seven
                // characters at most, so `User1000` prints as `User100`.
                let mut user = format!("User{}", key - Self::USER_BASE);
                user.truncate(7);
                out.push_str(&user);
                break 'name;
            }
            if let Some((name, _)) = TABLE.iter().find(|(_, k)| k & MASK_KEY == key) {
                out.push_str(name);
                break 'name;
            }
            // A Unicode key.
            if key >> 32 & 0xff == ty::UNICODE && key & MASK_KEY > 0x7f {
                out.push_str(&utf8_text(key));
                break 'name;
            }
            // Invalid keys are errors.
            if key > 255 {
                out.clear();
                let _ = write!(out, "Invalid#{saved:x}");
                break 'name;
            }
            // Printable ASCII keys.
            match key {
                33..=126 => out.push(char::from(byte(key, 0))),
                127 => out.push_str("C-?"),
                128.. => {
                    let _ = write!(out, "\\{key:o}");
                }
                _ => {}
            }
        }
        if with_flags && saved & MASK_FLAGS != 0 {
            out.push('[');
            for (bit, c) in [
                (LITERAL, 'L'),
                (KEYPAD, 'K'),
                (CURSOR, 'C'),
                (IMPLIED_META, 'I'),
                (BUILD_MODIFIERS, 'B'),
                (SENT, 'S'),
            ] {
                if saved & bit != 0 {
                    out.push(c);
                }
            }
            out.push(']');
        }
        out
    }
}

/// The `0x` branch of `key_string_lookup_string` (key-string.c:268-290),
/// after the `0x`. A value under 32 is that control character; otherwise it
/// is the Unicode character (which `wctomb` then `utf8_fromcstr` must turn
/// into exactly one character).
fn from_hex(hex: &str) -> Option<Key> {
    let u = sscanf_unsigned(hex, 16)?;
    if u < 32 {
        return Some(Key(u64::from(u)));
    }
    // Surrogates fail wctomb; past U+13FFFF the first byte is not a UTF-8
    // lead byte tmux accepts (`utf8_open`: F0-F4).
    if (0xd800..0xe000).contains(&u) || u > 0x13_ffff {
        return None;
    }
    match char::from_u32(u) {
        Some(c) => Some(Key(utf8_char(c))),
        // U+110000..U+13FFFF: four bytes, width 1, index = value.
        None => Some(Key(4 << 24 | 2 << 29 | u64::from(u))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_and_hex_scanf() {
        assert_eq!(sscanf_unsigned("  41", 16), Some(0x41));
        assert_eq!(sscanf_unsigned("0x41", 16), Some(0x41));
        assert_eq!(sscanf_unsigned("0x", 16), None);
        assert_eq!(sscanf_unsigned("-1", 10), Some(u32::MAX));
        assert_eq!(sscanf_unsigned("-4294967295", 10), Some(1));
        assert_eq!(sscanf_unsigned("ffffffffffffffffff", 16), Some(u32::MAX));
        assert_eq!(sscanf_unsigned("g", 16), None);
    }

    #[test]
    fn code_layout_matches_tmux() {
        // tmux.h: KEYC_NONE is type FUNCTION << 32, F1 is 8 after it.
        assert_eq!(Key::NONE.0, 0x2_0000_0000);
        assert_eq!(Key::F1.0, 0x2_0000_0008);
        assert_eq!(Key::from_name("C-a").unwrap().0, 0x61 | 1 << 45);
        assert_eq!(Key::from_name("M-a").unwrap().0, 0x61 | 1 << 44);
        assert_eq!(Key::from_name("S-a").unwrap().0, 0x61 | 1 << 46);
        // Up carries CURSOR|IMPLIED_META, which M- keeps and others drop.
        assert_eq!(Key::from_name("Up").unwrap().0, Key::UP.0 | CURSOR);
        assert_eq!(
            Key::from_name("M-Up").unwrap().0,
            Key::UP.0 | CURSOR | IMPLIED_META | META
        );
        assert_eq!(
            Key::from_name("MouseDown1Pane").unwrap().0,
            4 << 32 | 1 << 8
        );
        assert_eq!(Key::from_name("WheelUpStatus").unwrap().0, 9 << 32 | 1);
        assert_eq!(Key::from_name("User5").unwrap().0, 1 << 32 | 5);
        // A character: size, width+1 and the bytes.
        assert_eq!(Key::from_name("é").unwrap().0, 2 << 24 | 2 << 29 | 0xa9c3);
        assert_eq!(Key::from_name("0x41").unwrap().0, 1 << 24 | 2 << 29 | 0x41);
    }

    #[test]
    fn names_with_flags() {
        let up = Key::from_name("Up").unwrap();
        assert_eq!(up.name(false), "Up");
        assert_eq!(up.name(true), "Up[C]");
        assert_eq!(Key(0x61 | LITERAL).name(true), "a[L]");
    }
}
