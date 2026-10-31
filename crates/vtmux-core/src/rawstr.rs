//! Text that may hold bytes which are not UTF-8.
//!
//! tmux's arguments, option values, formats and buffer names are C strings:
//! any byte but NUL. vtmux's core is `String` all the way, so a byte that is
//! not part of a UTF-8 character travels as a pair: a NUL, then U+10FF00
//! plus the byte. A C string never holds a NUL, so the pair cannot collide
//! with text: valid UTF-8 is itself, every character (U+10FF80 included)
//! stays one ordinary character. A real NUL (text read whole, such as a
//! file) is itself too, except right before a real U+10FF00..=U+10FFFF,
//! where it is written NUL U+10FF00 so the mapping stays exact both ways.
//!
//! The edges convert: [`from_bytes`] on the way in (argv, config text,
//! control-mode lines), [`to_bytes`] on the way out (stdout, stderr, the
//! environment and argv of a spawned program, a lexer reading text). Code
//! that cares what a byte is walks [`Unit`]s ([`unit_at`], [`units`]).

use std::borrow::Cow;

const MARK: char = '\0';
const BASE: u32 = 0x10_FF00;

/// One element of text: a character, or a byte that is not UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Char(char),
    Byte(u8),
}

impl Unit {
    /// The character, `None` for a byte.
    #[must_use]
    pub fn char(self) -> Option<char> {
        match self {
            Self::Char(c) => Some(c),
            Self::Byte(_) => None,
        }
    }

    /// How many bytes the unit stands for.
    #[must_use]
    pub fn byte_len(self) -> usize {
        match self {
            Self::Char(c) => c.len_utf8(),
            Self::Byte(_) => 1,
        }
    }

    fn push_bytes(self, out: &mut Vec<u8>) {
        match self {
            Self::Char(c) => {
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Self::Byte(b) => out.push(b),
        }
    }
}

/// Whether `c` is the second half of a pair (after a NUL).
fn reserved(c: char) -> bool {
    u32::from(c) >= BASE
}

/// The unit at byte `i` of `s` (a unit boundary) and its length in `s`.
#[must_use]
pub fn unit_at(s: &str, i: usize) -> Option<(Unit, usize)> {
    let mut chars = s.get(i..)?.chars();
    let c = chars.next()?;
    if c == MARK
        && let Some(r) = chars.next().filter(|&r| reserved(r))
    {
        let len = 1 + r.len_utf8();
        let b = u8::try_from(u32::from(r) - BASE).expect("reserved is BASE..BASE+0xff");
        return Some((
            if b == 0 {
                Unit::Char(MARK)
            } else {
                Unit::Byte(b)
            },
            len,
        ));
    }
    Some((Unit::Char(c), c.len_utf8()))
}

/// The unit that ends at byte `i` of `s` (a unit boundary).
#[must_use]
pub fn unit_before(s: &str, i: usize) -> Option<Unit> {
    let head = s.get(..i)?;
    let c = head.chars().next_back()?;
    let start = i - c.len_utf8();
    if reserved(c) && head[..start].ends_with(MARK) {
        return unit_at(s, start - 1).map(|(u, _)| u);
    }
    Some(Unit::Char(c))
}

/// The first unit boundary at or after byte `i` of `s`.
#[must_use]
pub fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    if i > 0 && i < s.len() && s[..i].ends_with(MARK) && s[i..].chars().next().is_some_and(reserved)
    {
        i += s[i..].chars().next().map_or(0, char::len_utf8);
    }
    i
}

/// The units of `s` with their byte offsets in `s`.
pub fn unit_indices(s: &str) -> impl Iterator<Item = (usize, Unit)> + Clone + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        let (u, len) = unit_at(s, i)?;
        let at = i;
        i += len;
        Some((at, u))
    })
}

/// The units of `s`.
pub fn units(s: &str) -> impl Iterator<Item = Unit> + Clone + '_ {
    unit_indices(s).map(|(_, u)| u)
}

/// Append `unit` to `out` as text.
pub fn push_unit(out: &mut String, unit: Unit) {
    match unit {
        Unit::Char(c) => {
            // A bare NUL (always a real one) before a reserved character
            // would read as a pair: write it as NUL U+10FF00.
            if reserved(c) && out.ends_with(MARK) {
                out.push(raw(0));
            }
            out.push(c);
        }
        Unit::Byte(b) => {
            out.push(MARK);
            out.push(raw(b));
        }
    }
}

fn raw(byte: u8) -> char {
    char::from_u32(BASE + u32::from(byte)).expect("in the Unicode range")
}

/// The text for `bytes`: UTF-8 stays as it is, every other byte becomes a
/// pair. [`to_bytes`] gives `bytes` back.
#[must_use]
pub fn from_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            push_unit(&mut out, Unit::Char(c));
        }
        for &b in chunk.invalid() {
            push_unit(&mut out, Unit::Byte(b));
        }
    }
    out
}

/// [`from_bytes`] for a vector (no copy when it holds no NUL).
#[must_use]
pub fn from_vec(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) if !s.contains(MARK) => s,
        Ok(s) => from_bytes(s.as_bytes()),
        Err(e) => from_bytes(e.as_bytes()),
    }
}

/// `s` as a C string: up to its first real NUL.
#[must_use]
pub fn c_str(s: &str) -> &str {
    if !s.contains(MARK) {
        return s;
    }
    let end = unit_indices(s)
        .find(|&(_, u)| u == Unit::Char(MARK))
        .map_or(s.len(), |(i, _)| i);
    &s[..end]
}

/// The bytes `s` stands for.
#[must_use]
pub fn to_bytes(s: &str) -> Cow<'_, [u8]> {
    if !s.contains(MARK) {
        return Cow::Borrowed(s.as_bytes());
    }
    let mut out = Vec::with_capacity(s.len());
    for u in units(s) {
        u.push_bytes(&mut out);
    }
    Cow::Owned(out)
}

/// `strlen`: how many bytes `s` stands for.
#[must_use]
pub fn byte_len(s: &str) -> usize {
    if !s.contains(MARK) {
        return s.len();
    }
    units(s).map(Unit::byte_len).sum()
}

/// `utf8_isvalid` for the non-ASCII half: no byte in `s` that is not UTF-8.
#[must_use]
pub fn is_utf8(s: &str) -> bool {
    !s.contains(MARK) || units(s).all(|u| matches!(u, Unit::Char(_)))
}

/// `s` as real text, a byte that is not UTF-8 as U+FFFD (a lossy decode).
#[must_use]
pub fn to_lossy(s: &str) -> Cow<'_, str> {
    if is_utf8(s) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(units(s).map(|u| u.char().unwrap_or('\u{fffd}')).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_byte_string_shape() {
        let cases: [&[u8]; 11] = [
            b"plain",
            "é中".as_bytes(),
            b"a\xffb",
            b"\xc3",
            b"\xe4\xb8",
            b"\xf0\x9f\x98",
            "\u{10ff80}x\u{10ffff}".as_bytes(),
            b"\xed\xa0\x80\xc0\xaf\xf4\x90\x80\x80",
            b"a\0b",
            // A NUL before a character of the pair range, and before a byte.
            "\0\u{10ff80}\0\u{10ff00}".as_bytes(),
            b"\0\xff\0",
        ];
        for bytes in cases {
            let s = from_bytes(bytes);
            assert_eq!(&*to_bytes(&s), bytes, "{bytes:?}");
            assert_eq!(byte_len(&s), bytes.len(), "{bytes:?}");
            assert_eq!(from_vec(bytes.to_vec()), s);
            let back: Vec<u8> = {
                let mut v = Vec::new();
                for u in units(&s) {
                    u.push_bytes(&mut v);
                }
                v
            };
            assert_eq!(back, bytes);
        }
    }

    /// Valid UTF-8, plane-16 private use included, is itself: one ordinary
    /// character each (review: U+10FF80..=U+10FFFF were read as bytes).
    #[test]
    fn valid_text_is_untouched() {
        for text in ["a é 中 😀", "\u{10ff80}", "\u{10ffff}", "x\u{10ff00}y"] {
            assert_eq!(from_bytes(text.as_bytes()), text);
            assert!(is_utf8(text));
            assert!(units(text).all(|u| matches!(u, Unit::Char(_))));
        }
        assert!(matches!(to_bytes("é"), Cow::Borrowed(_)));
        assert!(!is_utf8(&from_bytes(b"\xff")));
        assert_eq!(to_lossy(&from_bytes(b"a\xffb")), "a\u{fffd}b");
        assert_eq!(to_lossy("\u{10ff80}"), "\u{10ff80}");
    }

    #[test]
    fn units_and_boundaries() {
        let s = from_bytes(b"a\xffb");
        let u: Vec<_> = unit_indices(&s).collect();
        assert_eq!(
            u,
            [
                (0, Unit::Char('a')),
                (1, Unit::Byte(0xff)),
                (6, Unit::Char('b'))
            ]
        );
        assert_eq!(unit_before(&s, 6), Some(Unit::Byte(0xff)));
        assert_eq!(ceil_boundary(&s, 2), 6);
        assert_eq!(ceil_boundary(&s, 3), 6);
        assert_eq!(ceil_boundary(&s, 1), 1);
        let n = from_bytes("\0\u{10ff80}".as_bytes());
        assert_eq!(
            units(&n).collect::<Vec<_>>(),
            [Unit::Char('\0'), Unit::Char('\u{10ff80}')]
        );
        assert_eq!(unit_before(&n, n.len()), Some(Unit::Char('\u{10ff80}')));
    }
}
