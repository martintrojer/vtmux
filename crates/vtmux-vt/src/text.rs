//! [`CellText`]: a cell's grapheme cluster without a heap allocation for
//! the usual one.
//!
//! Every grid cell carries its text, and copy mode keeps a row of cells
//! per line it has read, so a `String` per cell (24 bytes and a heap
//! block even for one ASCII byte) made a read history several times the
//! size tmux's grid takes (`grid_cell_entry`, 5 bytes for a plain
//! character). Up to [`INLINE`] bytes are stored in place.

use std::fmt;
use std::ops::Deref;

/// The longest text stored in place: every character up to four bytes,
/// and most clusters (a character with a few combining marks).
const INLINE: usize = 14;

/// A cell's text: a grapheme cluster, or a TAB cell's spaces; empty for a
/// blank cell. Reads as a `&str`.
#[derive(Clone)]
pub struct CellText(Repr);

#[derive(Clone)]
enum Repr {
    /// `buf[..len]` is UTF-8.
    Inline { len: u8, buf: [u8; INLINE] },
    /// Boxed twice so the type stays 16 bytes: longer text is rare.
    #[expect(
        clippy::box_collection,
        reason = "a thin pointer keeps CellText at 16 bytes"
    )]
    Heap(Box<String>),
}

impl CellText {
    #[must_use]
    pub const fn new() -> Self {
        Self(Repr::Inline {
            len: 0,
            buf: [0; INLINE],
        })
    }

    #[must_use]
    #[inline]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Inline { len, buf } => {
                let bytes = &buf[..usize::from(*len)];
                // SAFETY: `buf[..len]` is only ever written whole from a
                // `&str` (`from` and `push_str`), so it is valid UTF-8.
                unsafe { std::str::from_utf8_unchecked(bytes) }
            }
            Repr::Heap(s) => s,
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    pub fn push(&mut self, c: char) {
        self.push_str(c.encode_utf8(&mut [0; 4]));
    }

    pub fn push_str(&mut self, s: &str) {
        match &mut self.0 {
            Repr::Inline { len, buf } if usize::from(*len) + s.len() <= INLINE => {
                let at = usize::from(*len);
                buf[at..at + s.len()].copy_from_slice(s.as_bytes());
                *len = u8::try_from(at + s.len()).expect("inline length");
            }
            Repr::Inline { .. } => {
                let mut long = String::with_capacity(self.len() + s.len());
                long.push_str(self.as_str());
                long.push_str(s);
                self.0 = Repr::Heap(Box::new(long));
            }
            Repr::Heap(long) => long.push_str(s),
        }
    }
}

impl Default for CellText {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for CellText {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for CellText {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for CellText {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for CellText {
    fn from(s: &str) -> Self {
        let mut t = Self::new();
        t.push_str(s);
        t
    }
}

impl From<String> for CellText {
    fn from(s: String) -> Self {
        if s.len() <= INLINE {
            Self::from(s.as_str())
        } else {
            Self(Repr::Heap(Box::new(s)))
        }
    }
}

impl From<&String> for CellText {
    fn from(s: &String) -> Self {
        Self::from(s.as_str())
    }
}

impl From<char> for CellText {
    #[inline]
    fn from(c: char) -> Self {
        let mut buf = [0; INLINE];
        let len = c.encode_utf8(&mut buf).len();
        Self(Repr::Inline {
            len: u8::try_from(len).expect("a char is at most 4 bytes"),
            buf,
        })
    }
}

impl From<CellText> for String {
    fn from(t: CellText) -> Self {
        match t.0 {
            Repr::Inline { .. } => t.as_str().to_owned(),
            Repr::Heap(s) => *s,
        }
    }
}

impl PartialEq for CellText {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for CellText {}

impl std::hash::Hash for CellText {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Debug for CellText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for CellText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for CellText {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for CellText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for CellText {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<CellText> for str {
    fn eq(&self, other: &CellText) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<CellText> for &str {
    fn eq(&self, other: &CellText) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<CellText> for String {
    fn eq(&self, other: &CellText) -> bool {
        self == other.as_str()
    }
}

impl Extend<char> for CellText {
    fn extend<I: IntoIterator<Item = char>>(&mut self, iter: I) {
        for c in iter {
            self.push(c);
        }
    }
}

impl FromIterator<char> for CellText {
    fn from_iter<I: IntoIterator<Item = char>>(iter: I) -> Self {
        let mut t = Self::new();
        t.extend(iter);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_inline_and_long_text_spills() {
        assert_eq!(std::mem::size_of::<CellText>(), 16);
        let mut t = CellText::from('a');
        assert!(matches!(t.0, Repr::Inline { .. }));
        t.push('\u{301}');
        assert_eq!(t, "a\u{301}");
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let mut long = CellText::from("x");
        long.push_str(family);
        assert!(matches!(long.0, Repr::Heap(_)));
        assert_eq!(long.as_str(), format!("x{family}"));
        assert_eq!(CellText::from(family.to_owned()), family);
        long.clear();
        assert!(long.is_empty());
        assert_eq!(String::from(CellText::from("ok")), "ok");
    }
}
