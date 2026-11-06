//! vtmux: export and import a terminal's whole state (patch 0017,
//! `vtmux_state.h`). Pages come out as rows of cells, or a compressed
//! page as a blob only the same build reads back; an [`Importer`] puts
//! them, with the [`State`], into a fresh terminal.

use crate::{
    error::{Error, Result, from_result},
    ffi,
    terminal::Terminal,
};

pub use ffi::{
    VtmuxCapacity as Capacity, VtmuxCell as Cell, VtmuxCharset as Charset, VtmuxLink as Link,
    VtmuxMode as Mode, VtmuxRow as Row, VtmuxScreenState as ScreenState, VtmuxState as RawState,
};

/// The primary screen's key.
pub const PRIMARY: u8 = 0;
/// The alternate screen's key.
pub const ALTERNATE: u8 = 1;

/// One exported page: rows of cells (with their style, link and grapheme
/// tables), or a blob. Reused across exports to keep its buffers.
#[derive(Clone, Default)]
pub struct Page {
    /// The rows, each a run of `cells`.
    pub rows: Vec<Row>,
    /// The rows' cells; a row's trailing default cells are not stored.
    pub cells: Vec<Cell>,
    /// Per grapheme cell: its index, the count of codepoints after the
    /// first, and those codepoints.
    pub graphemes: Vec<u32>,
    /// Styles the cells index; entry 0 is unused.
    pub styles: Vec<ffi::Style>,
    /// Hyperlinks the cells index; entry 0 is unused.
    pub links: Vec<Link>,
    /// The links' strings, or the blob.
    pub bytes: Vec<u8>,
    /// The page's width.
    pub cols: u16,
    /// `bytes` is a compressed page's blob and nothing else is set.
    pub blob: bool,
    /// The page's allocation sizes, as hints for an import.
    pub capacity: Capacity,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("rows", &self.rows.len())
            .field("cols", &self.cols)
            .field("blob", &self.blob)
            .finish_non_exhaustive()
    }
}

impl Page {
    fn raw(&mut self) -> ffi::VtmuxPage {
        ffi::VtmuxPage {
            size: std::mem::size_of::<ffi::VtmuxPage>(),
            rows: self.rows.as_mut_ptr(),
            rows_cap: self.rows.capacity(),
            rows_len: self.rows.len(),
            cells: self.cells.as_mut_ptr(),
            cells_cap: self.cells.capacity(),
            cells_len: self.cells.len(),
            graphemes: self.graphemes.as_mut_ptr(),
            graphemes_cap: self.graphemes.capacity(),
            graphemes_len: self.graphemes.len(),
            styles: self.styles.as_mut_ptr(),
            styles_cap: self.styles.capacity(),
            styles_len: self.styles.len(),
            links: self.links.as_mut_ptr(),
            links_cap: self.links.capacity(),
            links_len: self.links.len(),
            bytes: self.bytes.as_mut_ptr(),
            bytes_cap: self.bytes.capacity(),
            bytes_len: self.bytes.len(),
            cols: self.cols,
            blob: self.blob,
            capacity: self.capacity,
        }
    }
}

/// The terminal's state outside its grids, with the bytes its cursors'
/// hyperlinks point into.
#[derive(Clone)]
pub struct State {
    /// The C struct; its `bytes` pointer is not used.
    pub raw: Box<RawState>,
    /// The cursors' hyperlink strings.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").finish_non_exhaustive()
    }
}

impl Default for State {
    fn default() -> Self {
        // SAFETY: every field of the C struct is an integer, bool, array
        // of those, a style (whose all-zero tags are `none`) or a null
        // pointer: all-zero is a valid value.
        let mut raw: Box<RawState> = Box::new(unsafe { std::mem::zeroed() });
        raw.size = std::mem::size_of::<RawState>();
        Self {
            raw,
            bytes: Vec::new(),
        }
    }
}

impl<'alloc, 'cb> Terminal<'alloc, 'cb> {
    /// Pages of a screen, history and active area together; `None` when
    /// the screen does not exist.
    pub fn vtmux_page_count(&self, screen: u8) -> Result<Option<usize>> {
        let mut n = 0usize;
        let r = unsafe { ffi::ghostty_vtmux_page_count(self.inner.as_raw(), screen, &raw mut n) };
        if r == ffi::Result::NO_VALUE {
            return Ok(None);
        }
        from_result(r)?;
        Ok(Some(n))
    }

    /// Export page `index` (0 the oldest) of a screen into `page`. A
    /// compressed page comes out as a blob when `blob_if_compressed`.
    pub fn vtmux_page_export(
        &self,
        screen: u8,
        index: usize,
        blob_if_compressed: bool,
        page: &mut Page,
    ) -> Result<()> {
        page.rows.clear();
        page.cells.clear();
        page.graphemes.clear();
        page.styles.clear();
        page.links.clear();
        page.bytes.clear();
        loop {
            let mut raw = page.raw();
            let r = unsafe {
                ffi::ghostty_vtmux_page_export(
                    self.inner.as_raw(),
                    screen,
                    index,
                    blob_if_compressed,
                    &raw mut raw,
                )
            };
            if r == ffi::Result::OUT_OF_SPACE {
                reserve(&mut page.rows, raw.rows_len);
                reserve(&mut page.cells, raw.cells_len);
                reserve(&mut page.graphemes, raw.graphemes_len);
                reserve(&mut page.styles, raw.styles_len);
                reserve(&mut page.links, raw.links_len);
                reserve(&mut page.bytes, raw.bytes_len);
                continue;
            }
            from_result(r)?;
            // SAFETY: the export wrote that many elements of each buffer,
            // each within its capacity (it returns OUT_OF_SPACE otherwise).
            unsafe {
                page.rows.set_len(raw.rows_len);
                page.cells.set_len(raw.cells_len);
                page.graphemes.set_len(raw.graphemes_len);
                page.styles.set_len(raw.styles_len);
                page.links.set_len(raw.links_len);
                page.bytes.set_len(raw.bytes_len);
            }
            page.cols = raw.cols;
            page.blob = raw.blob;
            page.capacity = raw.capacity;
            return Ok(());
        }
    }

    /// Export the terminal's state outside its grids.
    pub fn vtmux_state_export(&self) -> Result<State> {
        let mut state = State::default();
        loop {
            state.raw.bytes = state.bytes.as_mut_ptr();
            state.raw.bytes_cap = state.bytes.capacity();
            let r = unsafe {
                ffi::ghostty_vtmux_state_export(self.inner.as_raw(), &raw mut *state.raw)
            };
            if r == ffi::Result::OUT_OF_SPACE {
                reserve(&mut state.bytes, state.raw.bytes_len);
                continue;
            }
            from_result(r)?;
            // SAFETY: as in `vtmux_page_export`.
            unsafe { state.bytes.set_len(state.raw.bytes_len) };
            state.raw.bytes = std::ptr::null_mut();
            state.raw.bytes_cap = 0;
            return Ok(state);
        }
    }

    /// Start importing into this terminal, which must be fresh (no
    /// tracked grid references) and of the state's size.
    pub fn vtmux_importer(&mut self) -> Result<Importer<'_, 'alloc, 'cb>> {
        let mut raw: ffi::VtmuxImporter = std::ptr::null_mut();
        from_result(unsafe { ffi::ghostty_vtmux_importer_new(self.inner.as_raw(), &raw mut raw) })?;
        if raw.is_null() {
            return Err(Error::InvalidValue);
        }
        Ok(Importer { raw, _term: self })
    }

    /// Set the title as OSC 2 would store it, without the title-changed
    /// effect.
    pub fn vtmux_set_title(&mut self, title: &str) -> Result<()> {
        self.set(ffi::TerminalOption::TITLE, &ffi::String::from(title))
    }

    /// Set the working directory as OSC 7 would store it, without an
    /// effect.
    pub fn vtmux_set_pwd(&mut self, pwd: &str) -> Result<()> {
        self.set(ffi::TerminalOption::PWD, &ffi::String::from(pwd))
    }

    /// Make a screen the active one without a mode switch's side effects.
    pub fn vtmux_set_active_screen(&mut self, screen: u8) -> Result<()> {
        from_result(unsafe { ffi::ghostty_vtmux_set_active_screen(self.inner.as_raw(), screen) })
    }
}

fn reserve<T>(v: &mut Vec<T>, len: usize) {
    v.reserve(len.saturating_sub(v.len()));
}

/// An import in progress; it holds the terminal until it is finished.
#[allow(missing_debug_implementations, reason = "a handle")]
pub struct Importer<'t, 'alloc, 'cb> {
    raw: ffi::VtmuxImporter,
    _term: &'t mut Terminal<'alloc, 'cb>,
}

impl Importer<'_, '_, '_> {
    /// Add the next page (oldest first) of a screen. The page is copied.
    pub fn page(&mut self, screen: u8, page: &PageRef<'_>) -> Result<()> {
        let raw = page.raw();
        from_result(unsafe { ffi::ghostty_vtmux_importer_page(self.raw, screen, &raw const raw) })
    }

    /// Swap the pages in and apply `state`; the terminal is unchanged on
    /// failure.
    pub fn finish(self, state: &State) -> Result<()> {
        let mut raw = *state.raw;
        raw.size = std::mem::size_of::<RawState>();
        raw.bytes = state.bytes.as_ptr().cast_mut();
        raw.bytes_len = state.bytes.len();
        raw.bytes_cap = state.bytes.len();
        let importer = self.raw;
        std::mem::forget(self);
        from_result(unsafe { ffi::ghostty_vtmux_importer_finish(importer, &raw const raw) })
    }
}

impl Drop for Importer<'_, '_, '_> {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_vtmux_importer_free(self.raw) };
    }
}

/// A page to import, borrowed: rows of cells with their tables, or a blob
/// (see [`Page`]).
#[derive(Clone, Copy, Default)]
#[allow(missing_docs, reason = "the fields of `Page`")]
pub struct PageRef<'a> {
    pub rows: &'a [Row],
    pub cells: &'a [Cell],
    pub graphemes: &'a [u32],
    pub styles: &'a [ffi::Style],
    pub links: &'a [Link],
    pub bytes: &'a [u8],
    pub cols: u16,
    pub blob: bool,
    pub capacity: Capacity,
}

impl std::fmt::Debug for PageRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageRef")
            .field("rows", &self.rows.len())
            .finish_non_exhaustive()
    }
}

impl PageRef<'_> {
    /// The import only reads through these pointers.
    fn raw(&self) -> ffi::VtmuxPage {
        ffi::VtmuxPage {
            size: std::mem::size_of::<ffi::VtmuxPage>(),
            rows: self.rows.as_ptr().cast_mut(),
            rows_cap: self.rows.len(),
            rows_len: self.rows.len(),
            cells: self.cells.as_ptr().cast_mut(),
            cells_cap: self.cells.len(),
            cells_len: self.cells.len(),
            graphemes: self.graphemes.as_ptr().cast_mut(),
            graphemes_cap: self.graphemes.len(),
            graphemes_len: self.graphemes.len(),
            styles: self.styles.as_ptr().cast_mut(),
            styles_cap: self.styles.len(),
            styles_len: self.styles.len(),
            links: self.links.as_ptr().cast_mut(),
            links_cap: self.links.len(),
            links_len: self.links.len(),
            bytes: self.bytes.as_ptr().cast_mut(),
            bytes_cap: self.bytes.len(),
            bytes_len: self.bytes.len(),
            cols: self.cols,
            blob: self.blob,
            capacity: self.capacity,
        }
    }
}

impl Page {
    /// Borrow this page for an import.
    #[must_use]
    pub fn as_ref(&self) -> PageRef<'_> {
        PageRef {
            rows: &self.rows,
            cells: &self.cells,
            graphemes: &self.graphemes,
            styles: &self.styles,
            links: &self.links,
            bytes: &self.bytes,
            cols: self.cols,
            blob: self.blob,
            capacity: self.capacity,
        }
    }
}
