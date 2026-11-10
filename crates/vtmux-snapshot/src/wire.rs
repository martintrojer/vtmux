//! The record encoding: varints, length-prefixed tagged fields, and the
//! macros that declare record types with their field tags.
//!
//! A record is a run of fields. Each field is `tag` (varint), `len`
//! (varint) and `len` bytes of payload. A decoder skips the tags it does not
//! know, so an older reader of the same format version reads a stream with
//! a new field, without it; whether that is allowed is the crate's
//! versioning rule (*Changing the format* in the crate docs). Changing what
//! an existing tag means needs a new [`crate::FORMAT_VERSION`] and a
//! decoder that looks at [`Ctx::version`] or a migration step
//! (`crate::migrate`).
//!
//! Field modes, chosen per field where the record is declared:
//!
//! - `one`: written unless it equals its default; absent reads as the default.
//! - `opt`: an `Option`, written when `Some` (so `Some(default)` survives).
//! - `many`: a `Vec`, one field per element in order.
//!
//! A `many` field of rows may name a second tag (`1 / 13 => many rows`):
//! the rows' kept line flags go under it, after them
//! ([`crate::row::KeptRows`]); and a third (`1 / 13 / 14`) for the flags
//! they lack ([`crate::row::Row::lacks`]).

use crate::Error;
use std::cell::Cell;

/// Deepest nesting of records a decoder accepts. A snapshot's own records
/// nest about eight deep; the rest is for the layout tree, two records a
/// level (a cell and its node), so a tree may be some 120 levels deep. A
/// crafted stream nests deeper only to overflow the decoder's stack (and
/// `check`'s and `drop`'s after it).
pub const MAX_DEPTH: u32 = 256;

/// Memory a decode may build, beyond the input it copies: this much,
/// plus [`BUDGET_PER_BYTE`] for each byte of input. A real snapshot comes
/// nowhere near it (visible rows, the densest records, are at most a few
/// hundred bytes decoded per byte encoded and few); an empty record
/// repeated two bytes at a time costs its full size each time.
pub const BUDGET_BASE: u64 = 128 << 20;

/// See [`BUDGET_BASE`].
pub const BUDGET_PER_BYTE: u64 = 64;

/// What a decoder may need to know about the stream it reads, and the
/// limits that keep a crafted stream from costing more than a real one
/// ([`MAX_DEPTH`], [`BUDGET_BASE`]).
#[derive(Debug)]
pub struct Ctx {
    /// The format version in the stream's header.
    pub version: u32,
    depth: Cell<u32>,
    budget: Cell<u64>,
}

impl Ctx {
    /// A context for a value decoded on its own: the base budget.
    #[must_use]
    pub fn new(version: u32) -> Self {
        Self::for_input(version, 0)
    }

    /// A context for decoding `len` bytes of input.
    #[must_use]
    pub fn for_input(version: u32, len: usize) -> Self {
        Self {
            version,
            depth: Cell::new(0),
            budget: Cell::new(
                BUDGET_BASE.saturating_add((len as u64).saturating_mul(BUDGET_PER_BYTE)),
            ),
        }
    }

    /// Enter a nested record; the guard leaves it.
    pub fn enter(&self) -> Result<Nested<'_>, Error> {
        let d = self.depth.get() + 1;
        if d > MAX_DEPTH {
            return Err(Error::Malformed("records nest too deeply"));
        }
        self.depth.set(d);
        Ok(Nested(&self.depth))
    }

    /// Account for `bytes` of decoded memory.
    pub fn charge(&self, bytes: usize) -> Result<(), Error> {
        let left = self
            .budget
            .get()
            .checked_sub(bytes as u64)
            .ok_or(Error::Malformed(
                "snapshot decodes far larger than its bytes",
            ))?;
        self.budget.set(left);
        Ok(())
    }
}

/// A level of nesting entered with [`Ctx::enter`].
#[derive(Debug)]
pub struct Nested<'a>(&'a Cell<u32>);

impl Drop for Nested<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// A type with a fixed wire encoding.
pub trait Value: Sized {
    fn put(&self, out: &mut Vec<u8>);
    fn get(buf: &[u8], cx: &Ctx) -> Result<Self, Error>;
}

#[inline]
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        #[expect(clippy::cast_possible_truncation, reason = "seven bits at a time")]
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    #[expect(clippy::cast_possible_truncation, reason = "below 0x80")]
    out.push(v as u8);
}

#[must_use]
pub fn zigzag(v: i64) -> u64 {
    #[expect(clippy::cast_sign_loss, reason = "zigzag mapping")]
    let r = ((v << 1) ^ (v >> 63)) as u64;
    r
}

#[must_use]
pub fn unzigzag(v: u64) -> i64 {
    #[expect(clippy::cast_possible_wrap, reason = "zigzag mapping")]
    let r = ((v >> 1) as i64) ^ -((v & 1) as i64);
    r
}

/// A cursor over a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    #[inline]
    pub fn varint(&mut self) -> Result<u64, Error> {
        if let Some(&b) = self.buf.get(self.pos)
            && b < 0x80
        {
            self.pos += 1;
            return Ok(u64::from(b));
        }
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let &b = self.buf.get(self.pos).ok_or(Error::Truncated)?;
            self.pos += 1;
            let bits = u64::from(b & 0x7f);
            if shift == 63 && bits > 1 {
                return Err(Error::Malformed("varint overflows 64 bits"));
            }
            v |= bits << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Error::Malformed("varint overflows 64 bits"))
    }

    /// A varint that must fit in `u32`.
    pub fn u32(&mut self) -> Result<u32, Error> {
        u32::try_from(self.varint()?).map_err(|_| Error::Malformed("value out of range"))
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    pub fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }

    /// A varint length, then that many bytes.
    pub fn chunk(&mut self) -> Result<&'a [u8], Error> {
        let n = usize::try_from(self.varint()?).map_err(|_| Error::Truncated)?;
        self.bytes(n)
    }

    /// The next field of a record: its tag and payload.
    pub fn field(&mut self) -> Result<Option<(u64, &'a [u8])>, Error> {
        if self.is_empty() {
            return Ok(None);
        }
        let tag = self.varint()?;
        Ok(Some((tag, self.chunk()?)))
    }
}

/// Write `tag`, then `f`'s output prefixed with its length.
pub fn put_field(out: &mut Vec<u8>, tag: u64, f: impl FnOnce(&mut Vec<u8>)) {
    put_varint(out, tag);
    // One byte holds the length of most fields; a longer one shifts the
    // payload right to make room.
    let at = out.len();
    out.push(0);
    f(out);
    let len = out.len() - at - 1;
    let mut prefix = Vec::with_capacity(10);
    put_varint(&mut prefix, len as u64);
    if prefix.len() == 1 {
        out[at] = prefix[0];
    } else {
        out.splice(at..=at, prefix);
    }
}

/// Field mode `one`.
pub mod one {
    use super::{Ctx, Value, put_field};
    use crate::Error;

    pub fn put<T: Value + Default + PartialEq>(out: &mut Vec<u8>, tag: u64, v: &T) {
        if *v != T::default() {
            put_field(out, tag, |o| v.put(o));
        }
    }

    pub fn get<T: Value>(slot: &mut T, buf: &[u8], cx: &Ctx) -> Result<(), Error> {
        *slot = T::get(buf, cx)?;
        Ok(())
    }
}

/// Field mode `opt`.
pub mod opt {
    use super::{Ctx, Value, put_field};
    use crate::Error;

    pub fn put<T: Value>(out: &mut Vec<u8>, tag: u64, v: &Option<T>) {
        if let Some(v) = v {
            put_field(out, tag, |o| v.put(o));
        }
    }

    pub fn get<T: Value>(slot: &mut Option<T>, buf: &[u8], cx: &Ctx) -> Result<(), Error> {
        *slot = Some(T::get(buf, cx)?);
        Ok(())
    }
}

/// Field mode `many`.
pub mod many {
    use super::{Ctx, Value, put_field};
    use crate::Error;

    pub fn put<T: Value>(out: &mut Vec<u8>, tag: u64, v: &[T]) {
        for item in v {
            put_field(out, tag, |o| item.put(o));
        }
    }

    pub fn get<T: Value>(slot: &mut Vec<T>, buf: &[u8], cx: &Ctx) -> Result<(), Error> {
        // Each element costs its size, however few bytes encode it.
        cx.charge(size_of::<T>())?;
        slot.push(T::get(buf, cx)?);
        Ok(())
    }
}

fn whole_varint(buf: &[u8]) -> Result<u64, Error> {
    let mut r = Reader::new(buf);
    let v = r.varint()?;
    if !r.is_empty() {
        return Err(Error::Malformed("trailing bytes after a number"));
    }
    Ok(v)
}

macro_rules! unsigned {
    ($($t:ty),*) => {$(
        impl Value for $t {
            fn put(&self, out: &mut Vec<u8>) {
                put_varint(out, u64::from(*self));
            }
            fn get(buf: &[u8], _: &Ctx) -> Result<Self, Error> {
                <$t>::try_from(whole_varint(buf)?)
                    .map_err(|_| Error::Malformed("number out of range"))
            }
        }
    )*};
}
unsigned!(u8, u16, u32, u64);

macro_rules! signed {
    ($($t:ty),*) => {$(
        impl Value for $t {
            fn put(&self, out: &mut Vec<u8>) {
                put_varint(out, zigzag(i64::from(*self)));
            }
            fn get(buf: &[u8], _: &Ctx) -> Result<Self, Error> {
                <$t>::try_from(unzigzag(whole_varint(buf)?))
                    .map_err(|_| Error::Malformed("number out of range"))
            }
        }
    )*};
}
signed!(i32, i64);

impl Value for bool {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }
    fn get(buf: &[u8], _: &Ctx) -> Result<Self, Error> {
        match buf {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(Error::Malformed("bad boolean")),
        }
    }
}

impl Value for String {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.as_bytes());
    }
    fn get(buf: &[u8], _: &Ctx) -> Result<Self, Error> {
        String::from_utf8(buf.to_vec()).map_err(|_| Error::Malformed("string is not UTF-8"))
    }
}

/// Raw bytes (paste buffers, argv words, pending input).
impl Value for Vec<u8> {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
    fn get(buf: &[u8], _: &Ctx) -> Result<Self, Error> {
        Ok(buf.to_vec())
    }
}

impl Value for [u8; 32] {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
    fn get(buf: &[u8], _: &Ctx) -> Result<Self, Error> {
        buf.try_into()
            .map_err(|_| Error::Malformed("hash is not 32 bytes"))
    }
}

/// An empty payload: the variant of a [`oneof`] that carries nothing.
impl Value for () {
    fn put(&self, _: &mut Vec<u8>) {}
    fn get(_: &[u8], _: &Ctx) -> Result<Self, Error> {
        Ok(())
    }
}

/// A pair as a two-field record (tags 1 and 2).
impl<A, B> Value for (A, B)
where
    A: Value + Default + PartialEq,
    B: Value + Default + PartialEq,
{
    fn put(&self, out: &mut Vec<u8>) {
        one::put(out, 1, &self.0);
        one::put(out, 2, &self.1);
    }
    fn get(buf: &[u8], cx: &Ctx) -> Result<Self, Error> {
        let mut v = (A::default(), B::default());
        let mut r = Reader::new(buf);
        while let Some((tag, body)) = r.field()? {
            match tag {
                1 => one::get(&mut v.0, body, cx)?,
                2 => one::get(&mut v.1, body, cx)?,
                _ => {}
            }
        }
        Ok(v)
    }
}

/// Declare a record: a struct whose fields each carry a wire tag and mode.
///
/// ```ignore
/// record! {
///     /// Docs.
///     pub struct Example {
///         1 => one name: String,
///         2 => opt parent: Option<u32>,
///         3 => many children: Vec<u32>,
///         4 / 5 => many rows: Vec<Row>,
///     }
/// }
/// ```
///
/// A tag is never reused for another meaning; a removed field keeps its
/// tag reserved in a comment. Two fields with the same tag fail to compile
/// (an unreachable match arm under `-D warnings`).
macro_rules! record {
    (
        $(#[$m:meta])*
        pub struct $name:ident {
            $( $(#[$fm:meta])* $tag:literal $(/ $kept:literal $(/ $lacks:literal)?)? => $mode:ident $field:ident : $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct $name {
            $( $(#[$fm])* pub $field: $ty, )*
        }

        impl $crate::wire::Value for $name {
            fn put(&self, out: &mut Vec<u8>) {
                $(
                    $crate::wire::$mode::put(out, $tag, &self.$field);
                    $(
                        $crate::row::kept::put(out, $kept, &self.$field);
                        $( $crate::row::lacks::put(out, $lacks, &self.$field); )?
                    )?
                )*
            }

            fn get(buf: &[u8], cx: &$crate::wire::Ctx) -> Result<Self, $crate::Error> {
                let _nested = cx.enter()?;
                let mut v = Self::default();
                let mut r = $crate::wire::Reader::new(buf);
                while let Some((tag, body)) = r.field()? {
                    match tag {
                        $(
                            $tag => $crate::wire::$mode::get(&mut v.$field, body, cx)?,
                            $(
                                $kept => $crate::row::kept::get(&mut v.$field, body, cx)?,
                                $( $lacks => $crate::row::lacks::get(&mut v.$field, body, cx)?, )?
                            )?
                        )*
                        _ => {}
                    }
                }
                Ok(v)
            }
        }
    };
}

/// Declare a field-less enum stored as a varint. The first variant is the
/// default. An unknown number is an error: a reader of the same version
/// knows every number that version writes, so a new number needs a new
/// [`crate::FORMAT_VERSION`] or a [`crate::Meta::requires`] entry.
macro_rules! value_enum {
    (
        $(#[$m:meta])*
        pub enum $name:ident {
            $( $(#[$vm:meta])* $num:literal => $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vm])* $variant, )+
        }

        impl $name {
            #[must_use]
            pub const fn number(self) -> u64 {
                match self { $( Self::$variant => $num, )+ }
            }

            pub fn from_number(n: u64) -> Result<Self, $crate::Error> {
                match n {
                    $( $num => Ok(Self::$variant), )+
                    _ => Err($crate::Error::Malformed(concat!("unknown ", stringify!($name)))),
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                $crate::wire::first!($( Self::$variant ),+)
            }
        }

        impl $crate::wire::Value for $name {
            fn put(&self, out: &mut Vec<u8>) {
                $crate::wire::put_varint(out, self.number());
            }

            fn get(buf: &[u8], cx: &$crate::wire::Ctx) -> Result<Self, $crate::Error> {
                Self::from_number(<u64 as $crate::wire::Value>::get(buf, cx)?)
            }
        }
    };
}

/// Declare an enum whose variants each carry one value: written as a
/// one-field record whose tag names the variant. The first variant (with
/// its payload's default) is the default.
///
/// A variant's tag is not like a record's: a reader that knows none of the
/// tags in a value can't decode it, and the whole decode fails. So a new
/// variant needs a new [`crate::FORMAT_VERSION`] or a
/// [`crate::Meta::requires`] entry, unless the enum names a fallback with
/// `unknown => Variant` after its body: an unknown tag then decodes as
/// that variant with its payload's default (`PaneMode::Closed`: a mode
/// this reader doesn't have is closed).
macro_rules! oneof {
    (
        $(#[$m:meta])*
        pub enum $name:ident {
            $( $(#[$vm:meta])* $tag:literal => $variant:ident($ty:ty) ),+ $(,)?
        }
        $(unknown => $fallback:ident)?
    ) => {
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum $name {
            $( $(#[$vm])* $variant($ty), )+
        }

        impl Default for $name {
            fn default() -> Self {
                $crate::wire::first!($( Self::$variant(Default::default()) ),+)
            }
        }

        impl $crate::wire::Value for $name {
            fn put(&self, out: &mut Vec<u8>) {
                match self {
                    $( Self::$variant(v) => $crate::wire::put_field(out, $tag, |o| {
                        $crate::wire::Value::put(v, o);
                    }), )+
                }
            }

            fn get(buf: &[u8], cx: &$crate::wire::Ctx) -> Result<Self, $crate::Error> {
                let _nested = cx.enter()?;
                let mut r = $crate::wire::Reader::new(buf);
                let mut found = None;
                while let Some((tag, body)) = r.field()? {
                    match tag {
                        $( $tag => found = Some(Self::$variant(
                            <$ty as $crate::wire::Value>::get(body, cx)?,
                        )), )+
                        _ => {
                            $( if found.is_none() {
                                found = Some(Self::$fallback(Default::default()));
                            } )?
                        }
                    }
                }
                found.ok_or($crate::Error::Malformed(concat!("no known ", stringify!($name))))
            }
        }
    };
}

macro_rules! first {
    ($x:expr $(, $rest:expr)*) => {
        $x
    };
}

pub(crate) use {first, oneof, record, value_enum};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_the_edges() {
        for v in [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            let mut r = Reader::new(&out);
            assert_eq!(r.varint().unwrap(), v);
            assert!(r.is_empty());
        }
        for v in [0, 1, -1, i64::MIN, i64::MAX] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn varint_rejects_overflow_and_truncation() {
        let mut long = vec![0xff; 9];
        long.push(0x02);
        assert!(Reader::new(&long).varint().is_err());
        assert!(matches!(
            Reader::new(&[0x80]).varint(),
            Err(Error::Truncated)
        ));
    }

    #[test]
    fn long_fields_get_a_longer_length_prefix() {
        let payload = vec![7u8; 300];
        let mut out = Vec::new();
        put_field(&mut out, 5, |o| o.extend_from_slice(&payload));
        let mut r = Reader::new(&out);
        let (tag, body) = r.field().unwrap().unwrap();
        assert_eq!((tag, body), (5, &payload[..]));
        assert!(r.is_empty());
    }

    #[test]
    fn scalars_reject_bad_payloads() {
        assert!(bool::get(&[2], &Ctx::new(1)).is_err());
        assert!(u8::get(&[0x80, 0x02], &Ctx::new(1)).is_err());
        assert!(String::get(&[0xff], &Ctx::new(1)).is_err());
        assert!(<[u8; 32]>::get(&[0; 31], &Ctx::new(1)).is_err());
        // A number field with bytes after its varint.
        assert!(u32::get(&[1, 2], &Ctx::new(1)).is_err());
    }
}
