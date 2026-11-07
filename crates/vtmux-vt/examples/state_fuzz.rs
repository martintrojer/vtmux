//! Mutation fuzz of `PaneTerm::import_state` (live upgrade spec, decision
//! 23: a malformed state is an error, never a crash).
//!
//! Usage: `state_fuzz rows|state|links|styles|graphemes|cells|cursor|capacity|blob|blob-full
//! FIRST COUNT` (`blob-full` imports with `ImportCheck::Full`, which
//! decodes every blob; `links`, `styles` and `graphemes` aim at the tables
//! Ghostty bounds: URI and id lengths in pages and at the cursor, style
//! counts and repeats, codepoints per cell, cells per page; `cells` at
//! each cell kind's attributes, `cursor` at the cursors' semantic content
//! and flags, `capacity` at capacity hints), or `state_fuzz amp PAGES
//! BYTES`: PAGES pages asking for BYTES of grapheme and string room each. Each seed takes an
//! export of a pane with styled, linked, wide and grapheme history
//! (compressed, so `blob` has blobs; every other seed with an alternate
//! screen up), changes one to three fields at random, imports it, and
//! when the import succeeds uses the pane: captures, input, resizes,
//! compression and a second export. Prints `seed N` before each seed (and
//! why an import was refused), and a `done` line with how many imported, how many were refused and how
//! many panicked in Rust. A crash in Ghostty ends the process: the
//! last `seed` line names the seed. Build with
//! `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe` so Ghostty checks its own
//! invariants instead of running on past them.

#![expect(
    clippy::many_single_char_names,
    reason = "a fuzzer's scratch: the rng, an index, a case"
)]

use std::io::Write as _;

use vtmux_vt::{
    CaptureOpts, Color, CompressedPages, Compression, ImportCheck, LineRef, LinkId, LinkState,
    PageRows, PageState, PaneTerm, SavedCursor, Style, StyleState, TermState, capture,
};

/// xorshift64: the same seed gives the same mutations on every host.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            usize::try_from(self.next() % n as u64).unwrap_or(0)
        }
    }

    /// A value that hits edges: zero, all ones, small, 16-bit, anything.
    fn wild(&mut self) -> u32 {
        #[expect(clippy::cast_possible_truncation, reason = "random bits")]
        match self.below(5) {
            0 => 0,
            1 => u32::MAX,
            2 => self.below(70) as u32,
            3 => (self.next() as u32) & 0xffff,
            _ => self.next() as u32,
        }
    }

    #[expect(clippy::cast_possible_truncation, reason = "random bits")]
    fn wild16(&mut self) -> u16 {
        self.wild() as u16
    }

    #[expect(clippy::cast_possible_truncation, reason = "random bits")]
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

fn base(blob: bool, alt: bool) -> TermState {
    let mut a = PaneTerm::new(80, 24, 3000);
    for i in 0..4000 {
        a.feed(
            format!(
                "\x1b[3{}m{i} \x1b]8;;http://l{}\x07lk\x1b]8;;\x07 e\u{301}\u{1f468}\u{200d}\u{1f469}{}\u{4e16}\r\n",
                i % 8,
                i % 5,
                "w".repeat(i % 37)
            )
            .as_bytes(),
        );
    }
    // Background-only cells (an erase with a background colour), palette
    // and RGB, with tabs across some.
    for i in 0..30 {
        a.feed(
            format!(
                "\x1b[4{}m\x1b[K\t{i}\r\n\x1b[48;2;{i};2;3m\x1b[K\r\n\x1b[m",
                i % 8
            )
            .as_bytes(),
        );
    }
    a.feed(b"\x1b[3;5r\x1b7\x1b]133;A\x07$ \x1b]8;id=z;http://open\x07ab");
    if alt {
        a.feed(b"\x1b[?1049h\x1b[2;2Halt\x1b[?2026hx");
    }
    while a.compress_step() != Compression::Complete {}
    a.export_state_with(if blob {
        CompressedPages::Blob
    } else {
        CompressedPages::Rows
    })
}

fn mutate_blob(st: &mut TermState, r: &mut Rng) {
    let blobs: Vec<usize> = st
        .primary
        .pages
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p, PageState::Blob(_)))
        .map(|(i, _)| i)
        .collect();
    assert!(!blobs.is_empty(), "the base has no blobs");
    let i = blobs[r.below(blobs.len())];
    let PageState::Blob(b) = &mut st.primary.pages[i] else {
        return;
    };
    let len = b.bytes.len();
    let c = r.below(6);
    print!(" blobcase{c}");
    match c {
        0 => {
            let k = r.below(len);
            b.bytes.truncate(k);
        }
        // The header, the `Page` value, anywhere (the LZ4 bytes).
        1 => {
            let k = r.below(64.min(len));
            b.bytes[k] ^= 1 << r.below(8);
        }
        2 => {
            let k = r.below(1200.min(len));
            b.bytes[k] = r.byte();
        }
        3 => {
            let k = r.below(len);
            b.bytes[k] = r.byte();
        }
        4 => {
            let from = 1200.min(len - 1);
            let k = from + r.below(len - from);
            b.bytes[k] ^= 1 << r.below(8);
        }
        _ => {
            let k = r.below(1200.min(len)) & !3;
            if k + 4 <= len {
                b.bytes[k..k + 4].copy_from_slice(&r.wild().to_le_bytes());
            }
        }
    }
}

fn mutate_rows(st: &mut TermState, r: &mut Rng) {
    let i = r.below(st.primary.pages.len());
    let c = r.below(15);
    print!(" rowcase{c}");
    if c == 14 {
        st.primary.pages.remove(i);
        return;
    }
    let PageState::Rows(p) = &mut st.primary.pages[i] else {
        return;
    };
    match c {
        0 => {
            let k = r.below(p.rows.len());
            p.rows[k].cell_start = r.wild();
        }
        1 => {
            let k = r.below(p.rows.len());
            p.rows[k].cell_count = r.wild16();
        }
        2 => {
            let k = r.below(p.rows.len());
            p.rows[k].used = r.wild16();
            p.rows[k].flags = r.byte();
        }
        3 if !p.cells.is_empty() => {
            let k = r.below(p.cells.len());
            p.cells[k].content = r.wild();
            p.cells[k].flags = r.wild16();
        }
        4 if !p.cells.is_empty() => {
            let k = r.below(p.cells.len());
            p.cells[k].style = r.wild16();
        }
        5 if !p.cells.is_empty() => {
            let k = r.below(p.cells.len());
            p.cells[k].link = r.wild16();
        }
        6 if !p.graphemes.is_empty() => {
            let k = r.below(p.graphemes.len());
            p.graphemes[k] = r.wild();
        }
        7 => match r.below(5) {
            0 => p.capacity.rows = r.wild(),
            1 => p.capacity.styles = r.wild(),
            2 => p.capacity.hyperlink_bytes = r.wild(),
            3 => p.capacity.grapheme_bytes = r.wild(),
            _ => p.capacity.string_bytes = r.wild(),
        },
        8 => p.cols = r.wild16(),
        9 => {
            let k = r.below(p.rows.len() + 1);
            p.rows.truncate(k);
        }
        10 => {
            let k = r.below(p.styles.len() + 1);
            p.styles.truncate(k);
        }
        11 => {
            let k = r.below(p.links.len() + 1);
            p.links.truncate(k);
        }
        12 => {
            let k = r.below(p.cells.len() + 1);
            p.cells.truncate(k);
        }
        13 if !p.cells.is_empty() => {
            // Only the width: a wide character without its spacer.
            let k = r.below(p.cells.len());
            p.cells[k].flags = (p.cells[k].flags & !0x0c) | (u16::from(r.byte() & 3) << 2);
        }
        _ => {}
    }
}

fn mutate_state(st: &mut TermState, r: &mut Rng) {
    let c = r.below(24);
    print!(" statecase{c}");
    match c {
        0 => {
            st.ghostty.scroll_region = (r.wild16(), r.wild16(), r.wild16(), r.wild16());
        }
        1 => {
            st.primary.cursor.x = r.wild16();
            st.primary.cursor.y = r.wild16();
        }
        2 => {
            st.primary.saved_cursor = Some(SavedCursor {
                x: r.wild16(),
                y: r.wild16(),
                ..SavedCursor::default()
            });
        }
        3 => {
            st.primary.kitty_index = r.byte();
            st.primary.kitty_flags[r.below(8)] = r.byte();
        }
        4 => st.ghostty.previous_char = Some(r.wild()),
        5 => st.ghostty.tab_stops.push(r.wild16()),
        6 if !st.ghostty.modes.is_empty() => {
            let k = r.below(st.ghostty.modes.len());
            st.ghostty.modes[k].current ^= true;
            st.ghostty.modes[k].number = r.wild16();
        }
        7 => st.pane.hsize = r.wild() as usize,
        8 => {
            let k = r.below(st.pane.times.len() + 1);
            st.pane.times.truncate(k);
        }
        9 if !st.pane.marks.is_empty() => {
            let k = r.below(st.pane.marks.len());
            st.pane.marks[k].0 = r.wild();
        }
        10 => st.pane.alt_entry = Some((r.wild(), r.wild() as usize)),
        11 => {
            st.pane.continuation = (0..r.below(20)).map(|_| r.byte()).collect();
        }
        12 => st.cols = r.wild16(),
        13 => st.rows = r.wild16(),
        14 => st.alternate_active ^= true,
        15 => {
            st.pane.scrolled = u64::from(r.wild()) << r.below(40);
            st.pane.generation = u64::from(r.wild()) << r.below(40);
            st.pane.times_base = u64::from(r.wild()) << r.below(40);
        }
        16 => {
            st.primary.charset.g[r.below(4)] = r.byte();
            st.primary.charset.single_shift = Some(r.byte());
        }
        17 => st.pane.history_limit = (r.wild() as usize) << r.below(40),
        18 => st.pane.input_buffer_size = (r.wild() as usize) << r.below(40),
        19 => {
            if let Some((held, _)) = st.pane.sync.as_mut() {
                held.cols = r.wild16();
                held.cursor = Some((r.wild16(), r.wild16()));
            } else if let Some(row) = st.pane.frozen.as_mut().and_then(|rows| rows.first_mut()) {
                row.len = r.wild() as usize;
            }
        }
        20 => st.alternate = None,
        21 => {
            if let Some(link) = st.primary.cursor.link.as_mut() {
                link.uri.clear();
            }
        }
        22 => st.primary.protected_mode = r.byte(),
        _ => st.pane.alt_collected = r.wild() as usize,
    }
}

/// A length that hits the bounds Ghostty keeps: around 2048 (a link's
/// string), 64 (codepoints after a cell's first), 53247 (styles a page
/// holds), 65535 (a 16-bit count), or anything.
fn edge_len(r: &mut Rng) -> usize {
    const EDGES: [usize; 8] = [0, 1, 64, 2048, 53_247, 65_519, 65_535, 1 << 20];
    let base = EDGES[r.below(EDGES.len())];
    match r.below(4) {
        0 => base,
        1 => base + r.below(3),
        2 => base.saturating_sub(r.below(3)),
        _ => r.below(5000),
    }
}

/// A random page of rows (any screen).
fn rows_page<'a>(st: &'a mut TermState, r: &mut Rng) -> Option<&'a mut PageRows> {
    let screen = match st.alternate.as_mut() {
        Some(alt) if r.below(4) == 0 => alt,
        _ => &mut st.primary,
    };
    let n = screen.pages.len();
    match screen.pages.get_mut(r.below(n)) {
        Some(PageState::Rows(p)) => Some(p),
        _ => None,
    }
}

fn some_link(r: &mut Rng) -> LinkState {
    let len = edge_len(r).min(70_000);
    LinkState {
        uri: vec![b'a' + (r.byte() % 26); len],
        id: if r.below(2) == 0 {
            LinkId::Implicit(r.wild())
        } else {
            LinkId::Explicit(vec![b'i'; edge_len(r).min(70_000)])
        },
    }
}

fn mutate_links(st: &mut TermState, r: &mut Rng) {
    let c = r.below(7);
    print!(" linkcase{c}");
    if c < 2 {
        let link = (r.below(8) != 0).then(|| some_link(r));
        let screen = match st.alternate.as_mut() {
            Some(alt) if c == 1 => alt,
            _ => &mut st.primary,
        };
        screen.cursor.link = link;
        return;
    }
    let Some(p) = rows_page(st, r) else { return };
    let k = 1 + r.below(p.links.len().saturating_sub(1).max(1));
    match c {
        // One link's URI or id.
        2 if k < p.links.len() => {
            if let Some(l) = p.links[k].as_mut() {
                l.uri = vec![b'u'; edge_len(r).min(70_000)];
            }
        }
        3 if k < p.links.len() => {
            if let Some(l) = p.links[k].as_mut() {
                l.id = LinkId::Explicit(vec![b'i'; edge_len(r).min(70_000)]);
            }
        }
        // A link repeated, or many new ones, and cells pointed at them.
        4 if k < p.links.len() => {
            let again = p.links[k].clone();
            for _ in 0..=r.below(3) {
                p.links.push(again.clone());
            }
        }
        5 => {
            for i in 0..edge_len(r).min(5000) {
                p.links.push(Some(LinkState {
                    uri: format!("http://f/{i}").into_bytes(),
                    id: LinkId::Implicit(r.wild()),
                }));
            }
        }
        _ => {}
    }
    let n = p.links.len();
    if n > 1 && r.below(2) == 0 {
        for c in &mut p.cells {
            c.link = u16::try_from(1 + r.below(n - 1)).unwrap_or(0);
        }
    }
}

fn mutate_styles(st: &mut TermState, r: &mut Rng) {
    let c = r.below(6);
    print!(" stylecase{c}");
    let Some(p) = rows_page(st, r) else { return };
    match c {
        // Distinct styles up to and past what a page holds.
        0 => {
            let n = edge_len(r).min(70_000);
            p.styles.extend((0..n).map(|i| {
                let [_, r, g, b] = u32::try_from(i).unwrap_or(0).to_be_bytes();
                StyleState {
                    style: Style {
                        bg: Color::Rgb(r, g, b),
                        ..Style::default()
                    },
                    ..StyleState::default()
                }
            }));
        }
        // One style repeated.
        1 => {
            let again = p.styles[r.below(p.styles.len())];
            let n = edge_len(r).min(70_000);
            p.styles.extend(std::iter::repeat_n(again, n));
        }
        // Every cell one style, in a page with room for more cells.
        2 | 3 => {
            p.capacity.rows = if c == 2 {
                u32::try_from(edge_len(r) / usize::from(p.cols.max(1))).unwrap_or(0)
            } else {
                r.wild() & 0xffff
            };
            let s = u16::try_from(r.below(p.styles.len())).unwrap_or(0);
            for c in &mut p.cells {
                c.style = s;
            }
        }
        // The pens.
        4 => {
            st.primary.cursor.pen.fg_256 ^= true;
            st.primary.cursor.pen.style.bg = Color::Rgb(r.byte(), r.byte(), r.byte());
            if let Some(saved) = st.primary.saved_cursor.as_mut() {
                saved.pen.style.underline_color = Color::Palette256(r.byte());
            }
        }
        _ => p.capacity.styles = r.wild(),
    }
}

fn mutate_graphemes(st: &mut TermState, r: &mut Rng) {
    let c = r.below(4);
    print!(" graphemecase{c}");
    let Some(p) = rows_page(st, r) else { return };
    match c {
        // One cell's codepoints after its first.
        0 if p.graphemes.len() > 2 => {
            let n = edge_len(r).min(70_000);
            let old = p.graphemes[1] as usize;
            let rest = p.graphemes.split_off((2 + old).min(p.graphemes.len()));
            p.graphemes.truncate(1);
            p.graphemes.push(u32::try_from(n).unwrap_or(u32::MAX));
            p.graphemes.extend(std::iter::repeat_n(0x301, n));
            p.graphemes.extend(rest);
        }
        // Many cells with many codepoints.
        1 => {
            let n = 1 + edge_len(r).min(100);
            let mut table = Vec::new();
            for (i, cell) in p.cells.iter_mut().enumerate() {
                if cell.flags & 3 <= 1 && cell.content != 0 && r.below(3) == 0 {
                    cell.flags = (cell.flags & !3) | 1;
                    table.push(u32::try_from(i).unwrap_or(0));
                    table.push(u32::try_from(n).unwrap_or(0));
                    table.extend(std::iter::repeat_n(0x1f3fb, n));
                } else if cell.flags & 3 == 1 {
                    cell.flags &= !3;
                }
            }
            p.graphemes = table;
        }
        2 => p.capacity.grapheme_bytes = r.wild(),
        _ => p.capacity.string_bytes = r.wild(),
    }
}

/// One cell of a page with cells: its page and index.
fn some_cell<'a>(st: &'a mut TermState, r: &mut Rng) -> Option<(&'a mut PageRows, usize)> {
    let p = rows_page(st, r)?;
    if p.cells.is_empty() {
        return None;
    }
    let k = r.below(p.cells.len());
    Some((p, k))
}

/// Cell kinds with the attributes of another kind: a background-only
/// cell with a style, a link, a width, protection or semantic content,
/// and text cells with any of their tables' entries.
fn mutate_cells(st: &mut TermState, r: &mut Rng) {
    let c = r.below(6);
    print!(" cellcase{c}");
    let Some((p, k)) = some_cell(st, r) else {
        return;
    };
    let (styles, links) = (p.styles.len(), p.links.len());
    let cell = &mut p.cells[k];
    match c {
        // Background only, with a style or a link from the page's tables.
        0 | 1 => {
            cell.flags = (cell.flags & !3) | (2 + u16::from(r.byte() & 1));
            cell.content = r.wild() & 0xff_ffff;
            if c == 0 || r.below(2) == 0 {
                cell.style = u16::try_from(r.below(styles)).unwrap_or(0);
            }
            if c == 1 || r.below(2) == 0 {
                cell.link = u16::try_from(r.below(links)).unwrap_or(0);
            }
        }
        // Any width, protection, semantic content or palette form.
        2 => cell.flags ^= 1 << (2 + r.below(6)),
        // A text cell's table entries, or none.
        3 => {
            cell.style = u16::try_from(r.below(styles)).unwrap_or(0);
            cell.link = u16::try_from(r.below(links)).unwrap_or(0);
        }
        // An empty text cell with a style and a link.
        4 => {
            cell.flags &= !3;
            cell.content = 0;
            cell.style = u16::try_from(r.below(styles)).unwrap_or(0);
            cell.link = u16::try_from(r.below(links)).unwrap_or(0);
        }
        // Every cell of the page so.
        _ => {
            let kind = r.byte() & 3;
            let style = u16::try_from(r.below(styles)).unwrap_or(0);
            let link = u16::try_from(r.below(links)).unwrap_or(0);
            for cell in &mut p.cells {
                if kind >= 2 && cell.flags & 3 >= 2 {
                    cell.style = style;
                    cell.link = link;
                } else if kind < 2 && cell.flags & 3 < 2 {
                    cell.link = link;
                }
            }
        }
    }
}

/// The cursors' semantic content, styles, links and flags.
fn mutate_cursor(st: &mut TermState, r: &mut Rng) {
    let c = r.below(5);
    print!(" cursorcase{c}");
    let screen = match st.alternate.as_mut() {
        Some(alt) if r.below(2) == 0 => alt,
        _ => &mut st.primary,
    };
    let cur = &mut screen.cursor;
    match c {
        0 => {
            cur.semantic_content = r.byte() % 5;
            cur.semantic_clear_eol = r.below(2) == 0;
        }
        1 => {
            cur.visual = r.byte();
            cur.protected ^= true;
            cur.pending_wrap ^= true;
        }
        2 => screen.semantic_click = (r.byte() % 4, r.byte() % 8),
        3 => {
            screen.hyperlink_counter = r.wild();
            if let Some(LinkState {
                id: LinkId::Implicit(n),
                ..
            }) = cur.link.as_mut()
            {
                *n = r.wild();
            }
        }
        _ => {
            if let Some(saved) = screen.saved_cursor.as_mut() {
                saved.pending_wrap ^= true;
                saved.origin ^= true;
                saved.protected ^= true;
            }
        }
    }
}

/// A capacity hint that amplifies: 1 GiB, edges, and pages cut to one
/// row (the review's `amp`).
fn mutate_capacity(st: &mut TermState, r: &mut Rng) {
    let c = r.below(4);
    print!(" capcase{c}");
    let Some(p) = rows_page(st, r) else { return };
    if c == 0 {
        p.rows.truncate(1);
        let n = p.rows[0].cell_start as usize + usize::from(p.rows[0].cell_count);
        p.cells.truncate(n.min(p.cells.len()));
        p.graphemes.clear();
        for cell in &mut p.cells {
            cell.flags &= !3;
        }
    }
    let v = match r.below(3) {
        0 => 1 << (10 + r.below(21)),
        1 => r.wild(),
        _ => r.wild() & 0x3ff_ffff,
    };
    match r.below(5) {
        0 => p.capacity.rows = v & 0xffff,
        1 => p.capacity.styles = v,
        2 => p.capacity.hyperlink_bytes = v,
        3 => p.capacity.grapheme_bytes = v,
        _ => p.capacity.string_bytes = v,
    }
}

/// The review's `amp`: a state of `pages` one-row pages, each asking for
/// `bytes` of grapheme and string room.
fn amp(pages: usize, bytes: u32) {
    let mut a = PaneTerm::new(80, 24, 3000);
    for i in 0..(pages * 600) {
        a.feed(format!("{i}\r\n").as_bytes());
    }
    let mut st = a.export_state();
    let mut cut = 0;
    for page in &mut st.primary.pages {
        if let PageState::Rows(p) = page {
            if cut == pages {
                break;
            }
            p.capacity.grapheme_bytes = bytes;
            p.capacity.string_bytes = bytes;
            cut += 1;
        }
    }
    match PaneTerm::import_state(&st) {
        Ok(mut b) => {
            exercise(&mut b);
            println!("amp {pages} x {bytes}: imported");
        }
        Err(e) => println!("amp {pages} x {bytes}: refused: {e}"),
    }
}

/// `DUMP=1`: each page of rows, what Ghostty sizes it by.
fn dump(st: &TermState) {
    let screens = std::iter::once(("primary", &st.primary))
        .chain(st.alternate.iter().map(|a| ("alternate", a)));
    for (name, screen) in screens {
        for (i, page) in screen.pages.iter().enumerate() {
            if let PageState::Rows(p) = page {
                let linked = p.cells.iter().filter(|c| c.link != 0).count();
                let styled = p.cells.iter().filter(|c| c.style != 0).count();
                let background = p.cells.iter().filter(|c| c.flags & 3 >= 2).count();
                println!(
                    "  {name} page {i}: {} rows, {} cells ({linked} linked, {styled} styled, {background} background), {} styles, {} links, {:?}",
                    p.rows.len(),
                    p.cells.len(),
                    p.styles.len(),
                    p.links.len(),
                    p.capacity
                );
            }
        }
    }
}

fn exercise(b: &mut PaneTerm) {
    let all = CaptureOpts {
        start: LineRef::Start,
        end: LineRef::End,
        escapes: true,
        ..CaptureOpts::default()
    };
    let flags = CaptureOpts {
        flags: true,
        used_only: true,
        escapes: false,
        ..all
    };
    let look = |b: &PaneTerm| {
        let _ = capture(b, all);
        let _ = capture(b, flags);
        let _ = b.screen().snapshot();
        let _ = (b.cursor(), b.line_marks(), b.osc133_lines());
    };
    look(b);
    // The cursor's pen and link onto the page, both screens switched.
    b.feed(b"\x1b[?2026lstyled\x1b[38;2;1;2;3mx\x1b]8;;http://n\x07L\x1b[?1049hA\x1b[?1049lB\x1b8\x1b[?1049h\x1b[?1049l\x1b]8;;\x07\x1b[m");
    let again = b.export_state();
    PaneTerm::import_state(&again).expect("its own export imports");
    b.feed(b"\x1b[?2026lhello\r\n\x1b[Lx\x1b[3;1Hmore\x1b[S\x1b[T\x1b8z\x1b[2Jcleared");
    for i in 0..50 {
        b.feed(format!("after {i} \u{4e16}e\u{301}\r\n").as_bytes());
    }
    look(b);
    b.resize(17, 4);
    look(b);
    b.resize(50, 9);
    while b.compress_step() != Compression::Complete {}
    look(b);
    b.set_history_limit(100);
    b.feed(b"\x1b[?1049l\x1b[3J");
    look(b);
    let _ = b.export_state_with(CompressedPages::Blob);
    let _ = b.export_state();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let kind = args.get(1).map_or("rows", String::as_str);
    if kind == "amp" {
        let pages = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(4);
        let bytes = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(1 << 30);
        amp(pages, bytes);
        return;
    }
    let first: u64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(0);
    let count: u64 = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(100);
    std::panic::set_hook(Box::new(|info| eprintln!("panic: {info}")));
    let blob = kind.starts_with("blob");
    let check = if kind == "blob-full" {
        ImportCheck::Full
    } else {
        ImportCheck::Lazy
    };
    let bases = [base(blob, false), base(blob, true)];
    let (mut imported, mut refused, mut panicked) = (0, 0, 0);
    let mut out = std::io::stdout();
    for seed in first..first + count {
        print!("seed {seed}");
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut st = bases[usize::from(seed % 2 == 1)].clone();
        for _ in 0..=r.below(3) {
            match kind {
                _ if blob => mutate_blob(&mut st, &mut r),
                "rows" => mutate_rows(&mut st, &mut r),
                "links" => mutate_links(&mut st, &mut r),
                "styles" => mutate_styles(&mut st, &mut r),
                "graphemes" => mutate_graphemes(&mut st, &mut r),
                "cells" => mutate_cells(&mut st, &mut r),
                "cursor" => mutate_cursor(&mut st, &mut r),
                "capacity" => mutate_capacity(&mut st, &mut r),
                _ => mutate_state(&mut st, &mut r),
            }
            if st.primary.pages.is_empty() {
                break;
            }
        }
        println!();
        if std::env::var_os("DUMP").is_some() {
            dump(&st);
        }
        let _ = out.flush();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match PaneTerm::import_state_with(&st, check) {
                Ok(mut b) => {
                    exercise(&mut b);
                    true
                }
                Err(e) => {
                    println!("  refused: {e}");
                    false
                }
            }
        }));
        match res {
            Ok(true) => imported += 1,
            Ok(false) => refused += 1,
            Err(_) => {
                panicked += 1;
                println!("RUSTPANIC seed {seed}");
            }
        }
    }
    println!("done imported {imported} refused {refused} rustpanic {panicked}");
}
