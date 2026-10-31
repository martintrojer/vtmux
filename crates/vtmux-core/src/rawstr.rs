//! Text that may hold bytes which are not UTF-8.
//!
//! tmux's arguments, option values, formats and buffer names are C strings:
//! any byte but NUL. vtmux's core is `String` all the way, so a byte that is
//! not part of a UTF-8 character travels as one reserved character,
//! U+10FF00 plus the byte (`U+10FF80..=U+10FFFF`; a real character in that
//! range is escaped as its four bytes, so the mapping is exact in both
//! directions). The edges convert: [`from_bytes`] on the way in (argv, config
//! text, control-mode lines), [`to_bytes`] on the way out (stdout, stderr,
//! the environment and argv of a spawned program, a lexer reading text).

use std::borrow::Cow;

const BASE: u32 = 0x10_FF00;
const FIRST: u32 = 0x10_FF80;

/// The reserved character for a raw byte (`byte >= 0x80`).
fn raw(byte: u8) -> char {
    char::from_u32(BASE + u32::from(byte)).expect("in the Unicode range")
}

/// Whether `c` stands for one raw byte.
#[must_use]
pub fn is_raw(c: char) -> bool {
    u32::from(c) >= FIRST
}

/// The byte a raw character stands for.
fn byte_of(c: char) -> u8 {
    u8::try_from(u32::from(c) - BASE).expect("a raw character is 0x80..=0xff")
}

/// The text for `bytes`: UTF-8 stays as it is, every other byte becomes a
/// raw character. [`to_bytes`] gives `bytes` back.
#[must_use]
pub fn from_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if is_raw(c) {
                let mut buf = [0; 4];
                out.extend(c.encode_utf8(&mut buf).bytes().map(raw));
            } else {
                out.push(c);
            }
        }
        out.extend(chunk.invalid().iter().copied().map(raw));
    }
    out
}

/// [`from_bytes`] for a vector (no copy when it is plain UTF-8).
#[must_use]
pub fn from_vec(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) if !s.chars().any(is_raw) => s,
        Ok(s) => from_bytes(s.as_bytes()),
        Err(e) => from_bytes(e.as_bytes()),
    }
}

/// The bytes `s` stands for.
#[must_use]
pub fn to_bytes(s: &str) -> Cow<'_, [u8]> {
    if !s.chars().any(is_raw) {
        return Cow::Borrowed(s.as_bytes());
    }
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if is_raw(c) {
            out.push(byte_of(c));
        } else {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    Cow::Owned(out)
}

/// `strlen`: how many bytes `s` stands for.
#[must_use]
pub fn byte_len(s: &str) -> usize {
    s.chars()
        .map(|c| if is_raw(c) { 1 } else { c.len_utf8() })
        .sum()
}

/// `utf8_isvalid` for the non-ASCII half: no raw byte in `s`.
#[must_use]
pub fn is_utf8(s: &str) -> bool {
    !s.chars().any(is_raw)
}

/// `s` as real text, a raw byte as U+FFFD (what a lossy decode gives).
#[must_use]
pub fn to_lossy(s: &str) -> Cow<'_, str> {
    if is_utf8(s) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| if is_raw(c) { '\u{fffd}' } else { c })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_byte_string_shape() {
        let cases: [&[u8]; 8] = [
            b"plain",
            "é中".as_bytes(),
            b"a\xffb",
            b"\xc3",
            b"\xe4\xb8",
            b"\xf0\x9f\x98",
            // U+10FF80 itself: four bytes that must come back as four bytes.
            "\u{10ff80}x".as_bytes(),
            b"\xed\xa0\x80\xc0\xaf\xf4\x90\x80\x80",
        ];
        for bytes in cases {
            let s = from_bytes(bytes);
            assert_eq!(&*to_bytes(&s), bytes, "{bytes:?}");
            assert_eq!(byte_len(&s), bytes.len(), "{bytes:?}");
            assert_eq!(from_vec(bytes.to_vec()), s);
        }
    }

    #[test]
    fn valid_text_is_untouched() {
        assert_eq!(from_bytes("a é 中 😀".as_bytes()), "a é 中 😀");
        assert!(matches!(to_bytes("é"), Cow::Borrowed(_)));
        assert!(is_utf8("é"));
        assert!(!is_utf8(&from_bytes(b"\xff")));
        assert_eq!(to_lossy(&from_bytes(b"a\xffb")), "a\u{fffd}b");
    }
}
