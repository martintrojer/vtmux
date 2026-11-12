//! Format terminal content as plain text, VT sequences, or HTML.
//!
//! A formatter captures a reference to a terminal and formatting options.
//! It can be used repeatedly to produce output that reflects the current
//! terminal state at the time of each format call.
use std::{marker::PhantomData, ptr::NonNull};

use crate::{
    alloc::{Allocator, Bytes, Object},
    error::{Error, Result, from_result},
    ffi,
    selection::Selection,
    terminal::Terminal,
};

/// Formatter that formats terminal content.
#[derive(Debug)]
pub struct Formatter<'t, 'alloc: 'cb, 'cb: 't> {
    inner: Object<'alloc, ffi::FormatterImpl>,
    _terminal: PhantomData<&'t Terminal<'alloc, 'cb>>,
}

/// Options for [creating a terminal formatter](Formatter::new).
///
/// vtmux: libghostty-vt 1.0 sets each option with `ghostty_formatter_set`
/// after `ghostty_formatter_terminal_new`; [`Formatter::new`] sets every one
/// of these, so the defaults are this struct's (plain, no unwrap, no trim,
/// no extras), as the options struct of earlier versions gave.
#[derive(Debug)]
pub struct FormatterOptions<'t, 's> {
    emit: ffi::FormatterFormat::Type,
    unwrap: bool,
    trim: bool,
    selection: Option<&'s Selection<'t>>,
    extras: u32,
}

// GHOSTTY_FORMATTER_EXTRA_* (formatter.h).
const EXTRA_PALETTE: u32 = 1 << 0;
const EXTRA_MODES: u32 = 1 << 1;
const EXTRA_SCROLLING_REGION: u32 = 1 << 2;
const EXTRA_TABSTOPS: u32 = 1 << 3;
const EXTRA_PWD: u32 = 1 << 4;
const EXTRA_KEYBOARD: u32 = 1 << 5;
const EXTRA_CURSOR: u32 = 1 << 6;
const EXTRA_STYLE: u32 = 1 << 7;
const EXTRA_HYPERLINK: u32 = 1 << 8;
const EXTRA_PROTECTION: u32 = 1 << 9;
const EXTRA_KITTY_KEYBOARD: u32 = 1 << 10;
const EXTRA_CHARSETS: u32 = 1 << 11;

impl<'t, 's> FormatterOptions<'t, 's> {
    /// Create a new set of options for [creating a terminal formatter](Formatter::new).
    pub fn new() -> Self {
        Self {
            emit: ffi::FormatterFormat::PLAIN,
            unwrap: false,
            trim: false,
            selection: None,
            extras: 0,
        }
    }
    fn extra(mut self, bit: u32, value: bool) -> Self {
        if value {
            self.extras |= bit;
        } else {
            self.extras &= !bit;
        }
        self
    }
    /// Specify the output format to emit.
    pub fn with_format(mut self, value: Format) -> Self {
        self.emit = value.into();
        self
    }
    /// Specify whether to unwrap soft-wrapped lines.
    pub fn with_unwrap(mut self, value: bool) -> Self {
        self.unwrap = value;
        self
    }
    /// Specify whether to trim trailing whitespace on non-blank lines.
    pub fn with_trim(mut self, value: bool) -> Self {
        self.trim = value;
        self
    }
    /// Specify the selection to restrict output to a range.
    ///
    /// If a selection is not given, the formatter defaults to formatting
    /// the entire screen.
    pub fn with_selection(mut self, value: &'s Selection<'t>) -> Self {
        self.selection = Some(value);
        self
    }

    /// Specify whether to emit the palette using OSC 4 sequences.
    pub fn with_palette(self, value: bool) -> Self {
        self.extra(EXTRA_PALETTE, value)
    }
    /// Specify terminal modes that differ from their defaults using CSI h/l.
    pub fn with_modes(self, value: bool) -> Self {
        self.extra(EXTRA_MODES, value)
    }
    /// Specify whether to emit scrolling region state using DECSTBM and DECSLRM sequences.
    pub fn with_scrolling_region(self, value: bool) -> Self {
        self.extra(EXTRA_SCROLLING_REGION, value)
    }
    /// Specify tabstop positions by clearing all tabs and setting each one.
    pub fn with_tabstops(self, value: bool) -> Self {
        self.extra(EXTRA_TABSTOPS, value)
    }
    /// Specify the present working directory using OSC 7.
    pub fn with_pwd(self, value: bool) -> Self {
        self.extra(EXTRA_PWD, value)
    }
    /// Specify keyboard modes such as ModifyOtherKeys.
    pub fn with_keyboard(self, value: bool) -> Self {
        self.extra(EXTRA_KEYBOARD, value)
    }
    /// Specify whether to emit cursor position using CUP (CSI H).
    pub fn with_cursor(self, value: bool) -> Self {
        self.extra(EXTRA_CURSOR, value)
    }
    /// Emit current SGR style state based on the cursor's active style_id.
    pub fn with_style(self, value: bool) -> Self {
        self.extra(EXTRA_STYLE, value)
    }
    /// Emit current hyperlink state using OSC 8 sequences.
    pub fn with_hyperlink(self, value: bool) -> Self {
        self.extra(EXTRA_HYPERLINK, value)
    }
    /// Emit character protection mode using DECSCA.
    pub fn with_protection(self, value: bool) -> Self {
        self.extra(EXTRA_PROTECTION, value)
    }
    /// Emit Kitty keyboard protocol state using CSI > u and CSI = sequences.
    pub fn with_kitty_keyboard(self, value: bool) -> Self {
        self.extra(EXTRA_KITTY_KEYBOARD, value)
    }
    /// Emit character set designations and invocations.
    pub fn with_charsets(self, value: bool) -> Self {
        self.extra(EXTRA_CHARSETS, value)
    }
}

impl Default for FormatterOptions<'_, '_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'t, 'alloc: 'cb, 'cb: 't> Formatter<'t, 'alloc, 'cb> {
    /// Create a formatter for a terminal's active screen.
    pub fn new(
        terminal: &'t Terminal<'alloc, 'cb>,
        opts: FormatterOptions<'t, '_>,
    ) -> Result<Self> {
        // SAFETY: A NULL allocator is always valid
        unsafe { Self::new_inner(std::ptr::null(), terminal, opts) }
    }

    /// Create a formatter for a terminal's active screen.
    ///
    /// See the [crate-level documentation](crate#memory-management-and-lifetimes)
    /// regarding custom memory management and lifetimes.
    pub fn new_with_alloc<'ctx: 'alloc>(
        alloc: &'alloc Allocator<'ctx>,
        terminal: &'t Terminal<'alloc, 'cb>,
        opts: FormatterOptions,
    ) -> Result<Self> {
        // SAFETY: Borrow checking should forbid invalid allocators
        unsafe { Self::new_inner(alloc.to_raw(), terminal, opts) }
    }

    unsafe fn new_inner(
        alloc: *const ffi::Allocator,
        terminal: &'t Terminal<'alloc, 'cb>,
        opts: FormatterOptions,
    ) -> Result<Self> {
        let mut raw: ffi::Formatter = std::ptr::null_mut();

        let result = unsafe {
            ffi::ghostty_formatter_terminal_new(alloc, &raw mut raw, terminal.inner.as_raw())
        };
        from_result(result)?;

        let this = Self {
            inner: Object::new(raw)?,
            _terminal: PhantomData,
        };
        let set = |option, value: *const std::ffi::c_void| {
            from_result(unsafe { ffi::ghostty_formatter_set(this.inner.as_raw(), option, value) })
        };
        set(ffi::FormatterOption::EMIT, (&raw const opts.emit).cast())?;
        set(
            ffi::FormatterOption::UNWRAP,
            (&raw const opts.unwrap).cast(),
        )?;
        set(ffi::FormatterOption::TRIM, (&raw const opts.trim).cast())?;
        set(
            ffi::FormatterOption::EXTRAS,
            (&raw const opts.extras).cast(),
        )?;
        if let Some(selection) = opts.selection {
            set(
                ffi::FormatterOption::SELECTION,
                (&raw const selection.inner).cast(),
            )?;
        }
        Ok(this)
    }

    /// Run the formatter and return an allocated buffer with the output.
    ///
    /// Each call formats the current terminal state. The buffer is allocated
    /// using the provided allocator (or the default allocator if `None`).
    pub fn format_alloc<'a, 'ctx: 'a>(
        &mut self,
        alloc: Option<&'a Allocator<'ctx>>,
    ) -> Result<Bytes<'a>> {
        let alloc = if let Some(alloc) = alloc {
            alloc.to_raw()
        } else {
            std::ptr::null()
        };

        let mut bytes = std::ptr::null_mut();
        let mut len = 0usize;
        let result = unsafe {
            ffi::ghostty_formatter_format_alloc(
                self.inner.as_raw(),
                alloc,
                std::ptr::from_mut(&mut bytes),
                std::ptr::from_mut(&mut len),
            )
        };
        from_result(result)?;

        let ptr = NonNull::new(bytes).ok_or(Error::OutOfMemory)?;
        Ok(unsafe { Bytes::from_raw_parts(ptr, len, alloc) })
    }

    /// Run the formatter and produce output into the caller-provided buffer.
    ///
    /// Each call formats the current terminal state. If the buffer is too small,
    /// returns `Err(Error::OutOfSpace { required })` where `required` is the
    /// required size. The caller can then retry with a larger buffer.
    pub fn format_buf(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut len = 0usize;
        let result = unsafe {
            ffi::ghostty_formatter_format_buf(
                self.inner.as_raw(),
                std::ptr::from_mut(buf).cast(),
                buf.len(),
                std::ptr::from_mut(&mut len),
            )
        };
        from_result(result)?;
        Ok(len)
    }

    /// Query the required buffer size for the formatted output.
    ///
    /// The result can be used to create a sufficiently large buffer
    /// for [`Formatter::format_buf`].
    pub fn format_len(&mut self) -> Result<usize> {
        let mut len = 0usize;
        let result = unsafe {
            ffi::ghostty_formatter_format_buf(
                self.inner.as_raw(),
                std::ptr::null_mut(),
                0,
                std::ptr::from_mut(&mut len),
            )
        };
        // This should always fail with OutOfSpace.
        match from_result(result) {
            Err(Error::OutOfSpace { .. }) => Ok(len),
            Err(e) => Err(e),
            Ok(()) => Err(Error::InvalidValue),
        }
    }
}

impl Drop for Formatter<'_, '_, '_> {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_formatter_free(self.inner.as_raw()) }
    }
}

/// Output format.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, int_enum::IntEnum)]
pub enum Format {
    /// Plain text (no escape sequences).
    Plain = ffi::FormatterFormat::PLAIN,
    /// VT sequences preserving colors, styles, URLs, etc.
    Vt = ffi::FormatterFormat::VT,
    /// HTML with inline styles.
    Html = ffi::FormatterFormat::HTML,
}
