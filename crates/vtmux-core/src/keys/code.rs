//! The key code: tmux's `key_code` (tmux.h:145-430), a 64-bit number.
//!
//! The layout is part of tmux's behaviour. Bindings are stored and listed in
//! numeric order (key-bindings.c `key_bindings_cmp`), so the order of
//! `list-keys` is the order of these numbers: bits 0-39 are the key (a
//! character, or a function key, or a mouse event), bits 32-39 within that
//! its type, bits 40-47 the modifiers, bits 48-55 flags that are not part of
//! a binding's identity.

use std::fmt;

const SHIFT_TYPE: u64 = 32;

pub(super) const META: u64 = 0x0000_1000_0000_0000;
pub(super) const CTRL: u64 = 0x0000_2000_0000_0000;
pub(super) const SHIFT: u64 = 0x0000_4000_0000_0000;

pub(super) const LITERAL: u64 = 0x0001_0000_0000_0000;
pub(super) const KEYPAD: u64 = 0x0002_0000_0000_0000;
pub(super) const CURSOR: u64 = 0x0004_0000_0000_0000;
pub(super) const IMPLIED_META: u64 = 0x0008_0000_0000_0000;
pub(super) const BUILD_MODIFIERS: u64 = 0x0010_0000_0000_0000;
pub(super) const SENT: u64 = 0x0040_0000_0000_0000;

pub(super) const MASK_MODIFIERS: u64 = 0x0000_ff00_0000_0000;
pub(super) const MASK_FLAGS: u64 = 0x00ff_0000_0000_0000;
pub(super) const MASK_KEY: u64 = 0x0000_00ff_ffff_ffff;

/// `enum key_code_type`.
pub(super) mod ty {
    pub const UNICODE: u64 = 0;
    pub const USER: u64 = 1;
    pub const FUNCTION: u64 = 2;
    pub const MOUSEMOVE: u64 = 3;
    pub const MOUSEDOWN: u64 = 4;
    pub const MOUSEUP: u64 = 5;
    pub const MOUSEDRAG: u64 = 6;
    pub const MOUSEDRAGEND: u64 = 7;
    pub const WHEELDOWN: u64 = 8;
    pub const WHEELUP: u64 = 9;
    pub const SECONDCLICK: u64 = 10;
    pub const DOUBLECLICK: u64 = 11;
    pub const TRIPLECLICK: u64 = 12;
}

/// `enum key_mouse_location` (tmux.h:184-205), in order; the number is the
/// location part of a mouse key.
pub(super) const LOCATIONS: [&str; 20] = [
    "Pane",
    "Status",
    "StatusLeft",
    "StatusRight",
    "StatusDefault",
    "Border",
    "ScrollbarUp",
    "ScrollbarSlider",
    "ScrollbarDown",
    "Empty",
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
];

/// `KEYC_NUSER`: how many `UserN` keys there are.
pub(super) const NUSER: u64 = 1000;

/// A key as tmux numbers it. Two keys are the same key when their codes are
/// equal, and the derived order is tmux's binding order.
///
/// The flag bits (`KEYC_KEYPAD`, `KEYC_CURSOR`, ...) say how a key arrived,
/// not which key it is; a binding never keeps them ([`Key::without_flags`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub(super) u64);

const fn function(n: u64) -> Key {
    Key(ty::FUNCTION << SHIFT_TYPE | n)
}

impl Key {
    /// `KEYC_NONE`: no key. Not bindable.
    pub const NONE: Self = function(0);
    /// `KEYC_UNKNOWN`: what a string that is not a key looks up as.
    pub const UNKNOWN: Self = function(1);
    pub const FOCUS_IN: Self = function(2);
    pub const FOCUS_OUT: Self = function(3);
    /// `KEYC_ANY`: matches any key not otherwise bound in a table.
    pub const ANY: Self = function(4);
    pub const PASTE_START: Self = function(5);
    pub const PASTE_END: Self = function(6);
    pub const BSPACE: Self = function(7);
    pub const F1: Self = function(8);
    pub const IC: Self = function(20);
    pub const DC: Self = function(21);
    pub const HOME: Self = function(22);
    pub const END: Self = function(23);
    pub const NPAGE: Self = function(24);
    pub const PPAGE: Self = function(25);
    pub const BTAB: Self = function(26);
    pub const UP: Self = function(27);
    pub const DOWN: Self = function(28);
    pub const LEFT: Self = function(29);
    pub const RIGHT: Self = function(30);
    pub const KP_SLASH: Self = function(31);
    pub const REPORT_DARK_THEME: Self = function(47);
    pub const REPORT_LIGHT_THEME: Self = function(48);
    /// `KEYC_MOUSE`: an unclassified mouse event.
    pub const MOUSE: Self = function(49);
    /// `KEYC_DRAGGING`: a drag is in progress.
    pub const DRAGGING: Self = function(50);

    /// The code as tmux numbers it.
    #[must_use]
    pub const fn code(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn from_code(code: u64) -> Self {
        Self(code)
    }

    /// An ASCII or control character key (`'a'`, `0x1b`).
    #[must_use]
    pub const fn ascii(byte: u8) -> Self {
        Self(byte as u64)
    }

    /// With `modifiers` set (`KEYC_CTRL`, ...): `Key::ascii(b'a').ctrl()`.
    #[must_use]
    pub const fn ctrl(self) -> Self {
        Self(self.0 | CTRL)
    }

    #[must_use]
    pub const fn meta(self) -> Self {
        Self(self.0 | META)
    }

    #[must_use]
    pub const fn shift(self) -> Self {
        Self(self.0 | SHIFT)
    }

    /// `key & ~KEYC_MASK_FLAGS`: how bindings store a key.
    #[must_use]
    pub const fn without_flags(self) -> Self {
        Self(self.0 & !MASK_FLAGS)
    }

    /// `key & (KEYC_MASK_KEY|KEYC_MASK_MODIFIERS)`: what `list-keys KEY`
    /// compares (cmd-list-keys.c:121).
    #[must_use]
    pub const fn key_and_modifiers(self) -> Self {
        Self(self.0 & (MASK_KEY | MASK_MODIFIERS))
    }

    /// The key without modifiers or flags (`key & KEYC_MASK_KEY`).
    #[must_use]
    pub const fn base(self) -> Self {
        Self(self.0 & MASK_KEY)
    }

    #[must_use]
    pub const fn has_ctrl(self) -> bool {
        self.0 & CTRL != 0
    }

    #[must_use]
    pub const fn has_meta(self) -> bool {
        self.0 & META != 0
    }

    #[must_use]
    pub const fn has_shift(self) -> bool {
        self.0 & SHIFT != 0
    }

    /// The `n`th function key, `F1` being 1 (`KEYC_F1 + n - 1`).
    #[must_use]
    pub const fn function_key(n: u8) -> Self {
        Self(Self::F1.0 + n as u64 - 1)
    }

    /// The modifier bits alone (`sort_key_binding_cmp`, `-O modifier`).
    #[must_use]
    pub const fn modifiers(self) -> u64 {
        self.0 & MASK_MODIFIERS
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({:#x}, {})", self.0, self.name(true))
    }
}

/// `key_string_lookup_key(key, 0)`.
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name(false))
    }
}
