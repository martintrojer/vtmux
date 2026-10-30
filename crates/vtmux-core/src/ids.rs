//! Session, window and pane IDs.
//!
//! IDs display as tmux shows them (`$0`, `@0`, `%0`) and parse the way tmux
//! parses them: `session_find_by_id_str` (session.c:81-93) and
//! `window_find_by_id_str` (window.c:379-390, pane variant 1412-1423) strip
//! the sigil and hand the rest to `strtonum(s, 0, UINT_MAX)`. That is
//! `strtoll(s, &ep, 10)` followed by "the whole string was consumed" and a
//! range check, so leading whitespace and one sign are accepted. The
//! reference prints `$0` for `$00`, `$+0`, `$ 0` and `$-0` alike.
//!
//! Allocation (session.c:30,137; window.c:61-62,445,1448): one counter per
//! kind, starting at 0, incremented on every allocation and never reused
//! while the server lives.

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

/// An ID kind: its sigil and its raw number.
pub trait Id: Copy {
    /// The leading character tmux prints and expects (`$`, `@`, `%`).
    const SIGIL: char;
    fn new(raw: u32) -> Self;
    fn get(self) -> u32;
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $sigil:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);

        impl Id for $name {
            const SIGIL: char = $sigil;
            fn new(raw: u32) -> Self {
                Self(raw)
            }
            fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil, self.0)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;
            fn from_str(s: &str) -> Result<Self, ParseIdError> {
                parse_id::<Self>(s).map(Self)
            }
        }
    };
}

id_type!(
    /// A session ID, `$N`.
    SessionId,
    '$'
);
id_type!(
    /// A window ID, `@N`.
    WindowId,
    '@'
);
id_type!(
    /// A pane ID, `%N`.
    PaneId,
    '%'
);

/// The string is not an ID of this kind: wrong sigil, or `strtonum` rejects
/// the rest (empty, trailing junk, negative, above `u32::MAX`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseIdError;

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid id")
    }
}

impl std::error::Error for ParseIdError {}

/// C `isspace` in the C locale, which `strtoll` skips.
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn parse_id<I: Id>(s: &str) -> Result<u32, ParseIdError> {
    let rest = s.strip_prefix(I::SIGIL).ok_or(ParseIdError)?.as_bytes();
    let rest = &rest[rest.iter().take_while(|&&b| is_c_space(b)).count()..];
    let (negative, digits) = match rest.split_first() {
        Some((b'-', d)) => (true, d),
        Some((b'+', d)) => (false, d),
        _ => (false, rest),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(ParseIdError);
    }
    // Saturate: anything past u32::MAX is "too large" whatever its size.
    let mut value: u64 = 0;
    for &d in digits {
        value = (value * 10 + u64::from(d - b'0')).min(u64::from(u32::MAX) + 1);
    }
    if negative && value != 0 {
        return Err(ParseIdError);
    }
    u32::try_from(value).map_err(|_| ParseIdError)
}

/// Hands out IDs of one kind: monotonic, never reused. Use one allocator per
/// kind; they are independent.
#[derive(Debug)]
pub struct IdAllocator<I: Id> {
    next: u32,
    kind: PhantomData<I>,
}

impl<I: Id> IdAllocator<I> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: 0,
            kind: PhantomData,
        }
    }

    /// The ID [`Self::allocate`] would hand out next, without taking it
    /// (tmux numbers a pane only once its layout cell exists,
    /// window.c `window_pane_create`).
    #[must_use]
    pub fn peek(&self) -> I {
        I::new(self.next)
    }

    /// The next ID.
    ///
    /// # Panics
    /// After 2^32 allocations; reusing an ID would be worse.
    pub fn allocate(&mut self) -> I {
        let id = I::new(self.next);
        self.next = self.next.checked_add(1).expect("id space exhausted");
        id
    }
}

impl<I: Id> Default for IdAllocator<I> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected strings from the reference (tmux 3.8, `new -d -s a`, then
    // `display -p -t '<id>' '#{session_id}'` / `'#{pane_id}'`):
    //   $0 $00 $+0 '$ 0' $-0 '$ +0' -> $0       '$0 ' $ $-1 $4294967296 -> no match
    //   $00000000000000000000 -> $0              $0x0 $1e0 -> no match
    //   @00 @+0 '@ 0' @-0 %00 %+0 %-0 '% 0' -> found
    //   `has -t '$4294967295'` -> can't find session (parsed, nothing there)

    #[test]
    fn display_matches_tmux() {
        assert_eq!(SessionId(0).to_string(), "$0");
        assert_eq!(WindowId(12).to_string(), "@12");
        assert_eq!(PaneId(u32::MAX).to_string(), "%4294967295");
    }

    #[test]
    fn strtonum_style_parse() {
        for s in [
            "$0",
            "$00",
            "$+0",
            "$ 0",
            "$-0",
            "$ +0",
            "$ \t0",
            "$00000000000000000000",
        ] {
            assert_eq!(s.parse(), Ok(SessionId(0)), "{s:?}");
        }
        assert_eq!("@+7".parse(), Ok(WindowId(7)));
        assert_eq!("%-0".parse(), Ok(PaneId(0)));
        assert_eq!("$4294967295".parse(), Ok(SessionId(u32::MAX)));
    }

    #[test]
    fn strtonum_style_rejects() {
        for s in [
            "",
            "$",
            "$ ",
            "$+",
            "$-",
            "$-1",
            "$0 ",
            "$0x0",
            "$1e0",
            "$4294967296",
            "$99999999999999999999999",
            "0",
            "@0",
            "$+-1",
            "$ -",
        ] {
            assert_eq!(s.parse::<SessionId>(), Err(ParseIdError), "{s:?}");
        }
        assert_eq!("$0".parse::<PaneId>(), Err(ParseIdError));
    }

    #[test]
    fn allocators_are_independent_and_monotonic() {
        let mut s = IdAllocator::<SessionId>::new();
        let mut w = IdAllocator::<WindowId>::new();
        let mut p = IdAllocator::<PaneId>::new();
        assert_eq!(s.allocate().to_string(), "$0");
        assert_eq!(w.allocate().to_string(), "@0");
        assert_eq!(p.allocate().to_string(), "%0");
        assert_eq!(p.allocate(), PaneId(1));
        assert_eq!(p.allocate(), PaneId(2));
        // Other kinds are untouched by pane allocation.
        assert_eq!(s.allocate(), SessionId(1));
        assert_eq!(w.allocate(), WindowId(1));
    }
}
