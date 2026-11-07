//! A malformed exported state is refused with a [`StateError`], never
//! imported to crash later (live upgrade spec, decision 23; the review of
//! the spike found each case below aborting or segfaulting in Ghostty
//! after an import that succeeded). And the streamed form
//! (`export_state_head` + `export_pages`, `StateImport`) is the same
//! state as the whole one.

use vtmux_vt::{
    CaptureOpts, Color, CompressedPages, Compression, ImportCheck, LineRef, LinkId, LinkState,
    PageRows, PageState, PaneTerm, ScreenKind, StateError, StateImport, Style, StyleState,
    TermState, capture,
};

/// A pane with styles, links, wide characters and graphemes in history,
/// compressed as the server would, a scroll region and an OSC 133 mark.
fn source(alternate: bool) -> PaneTerm {
    let mut t = PaneTerm::new(40, 8, 3000);
    for i in 0..3000 {
        t.feed(
            format!(
                "\x1b[3{}m{i} \x1b]8;;http://l{}\x07lk\x1b]8;;\x07 e\u{301}\u{4e16}{}\r\n",
                i % 8,
                i % 5,
                "w".repeat(i % 23)
            )
            .as_bytes(),
        );
    }
    t.feed(b"\x1b[2;5r\x1b]133;A\x07$ ");
    if alternate {
        t.feed(b"\x1b[?1049h\x1b[2;2Halt");
    }
    while t.compress_step() != Compression::Complete {}
    t
}

/// What a client and a script see, after the pane is used: the crashes
/// the review found came on a print, a resize or a page's first read.
fn use_pane(t: &mut PaneTerm) -> Vec<u8> {
    let all = CaptureOpts {
        start: LineRef::Start,
        end: LineRef::End,
        escapes: true,
        ..CaptureOpts::default()
    };
    t.feed("more \u{4e16}\r\n".as_bytes());
    t.resize(17, 4);
    t.resize(50, 9);
    capture(t, all)
}

/// The first page of rows with a grapheme, a wide character and a link.
fn rich_page(state: &mut TermState) -> &mut PageRows {
    state
        .primary
        .pages
        .iter_mut()
        .find_map(|p| match p {
            PageState::Rows(r) if !r.graphemes.is_empty() && r.links.len() > 1 => Some(r),
            _ => None,
        })
        .expect("a page with graphemes and links")
}

/// Import `state` after `change`: it must be refused. Should it import
/// anyway, the pane is used, which is where the unchecked import crashed.
fn refused(name: &str, change: impl FnOnce(&mut TermState)) -> StateError {
    let mut state = source(false).export_state();
    change(&mut state);
    match PaneTerm::import_state(&state) {
        Ok(mut t) => {
            use_pane(&mut t);
            panic!("{name}: imported");
        }
        Err(e) => e,
    }
}

#[test]
fn malformed_rows_are_refused() {
    // A wild count of codepoints after a grapheme's first: Ghostty cast it
    // unchecked (`capacityFor`) or overflowed summing it.
    refused("wild grapheme count", |s| {
        rich_page(s).graphemes[1] = u32::MAX;
    });
    refused("grapheme count past the table", |s| {
        rich_page(s).graphemes[1] = 0x00ff_ffff;
    });
    // A page of another width: accepted, then `unreachable` in
    // `PageList.beginUpdate` on the next print or resize.
    refused("wild page width", |s| {
        rich_page(s).cols = 0xfff0;
    });
    refused("page narrower than the screen", |s| {
        rich_page(s).cols = 39;
    });
    // Capacity hints over Ghostty's page limit: `unreachable` in
    // `createPageExt`.
    refused("wild capacity rows", |s| {
        rich_page(s).capacity.rows = u32::MAX;
    });
    refused("capacity over the page limit", |s| {
        let p = rich_page(s);
        p.capacity.rows = 0xffff;
        p.capacity.grapheme_bytes = u32::MAX;
        p.capacity.string_bytes = u32::MAX;
    });
    refused("wide character without its spacer", |s| {
        let p = rich_page(s);
        let wide = p
            .cells
            .iter()
            .position(|c| (c.flags >> 2) & 3 == 1)
            .expect("a wide cell");
        p.cells[wide + 1].flags &= !0x0c;
    });
    refused("style index past the table", |s| {
        rich_page(s).cells[0].style = u16::MAX;
    });
    refused("link index past the table", |s| {
        rich_page(s).cells[0].link = u16::MAX;
    });
    // Past any width a pane can have had (a narrowed screen keeps its
    // rows' used counts, so up to that is fine).
    refused("cells used past any width", |s| {
        rich_page(s).rows[0].used = u16::MAX;
    });
    refused("row cells past the page's", |s| {
        rich_page(s).rows[0].cell_start = u32::MAX;
    });
    refused("a cell short", |s| {
        rich_page(s).cells.pop();
    });
    refused("codepoint past Unicode", |s| {
        rich_page(s).cells[0].content = 0x0011_0000;
    });
}

#[test]
fn malformed_states_are_refused() {
    // The state's width disagrees with its pages: `unreachable` in
    // `beginUpdate`.
    refused("state wider than its pages", |s| s.cols += 1);
    refused("wild height", |s| s.rows = u16::MAX);
    // A region top below its bottom: `unreachable` in `Screen.print`.
    refused("scroll region out of order", |s| {
        s.ghostty.scroll_region = (5, 2, 0, 39);
    });
    refused("scroll region off the screen", |s| {
        s.ghostty.scroll_region.1 = 8;
    });
    refused("cursor off the screen", |s| s.primary.cursor.x = 40);
    refused("previous character a surrogate", |s| {
        s.ghostty.previous_char = Some(0xd800);
    });
    refused("unknown charset", |s| s.primary.charset.g[2] = 9);
    refused("kitty flag stack index", |s| s.primary.kitty_index = 8);
    refused("history over the rows retained", |s| s.pane.hsize = 100_000);
    refused("alternate screen active but absent", |s| {
        s.alternate_active = true;
    });
    refused("mark off the screen", |s| {
        s.pane.marks.push((u32::MAX, vtmux_vt::Osc133::default()));
    });
    refused("wild history limit", |s| s.pane.history_limit = usize::MAX);
}

#[test]
fn a_link_open_on_the_inactive_screen_is_refused() {
    // Ghostty ends a screen's link when it leaves it and asserts there is
    // none when it comes back: imported, `?1049l` reached `unreachable`
    // in `Terminal.switchScreenMode`.
    let link = || LinkState {
        uri: b"http://inactive".to_vec(),
        id: LinkId::Implicit(1),
    };
    let mut on_alt = source(true);
    let mut on_primary = source(true);
    on_primary.feed(b"\x1b[?1049l");
    let switch = b"\x1b[?1049h\x1b[?1049l\x1b[?1049h\x1b[?1049l";
    for (t, alternate_active) in [(&mut on_alt, true), (&mut on_primary, false)] {
        let mut s = t.export_state();
        assert_eq!(s.alternate_active, alternate_active);
        let screen = |s: &mut TermState, active: bool| {
            if active == s.alternate_active {
                s.alternate.as_mut().unwrap().cursor.link = Some(link());
            } else {
                s.primary.cursor.link = Some(link());
            }
        };
        // The active screen's own link is fine.
        screen(&mut s, true);
        let mut b = PaneTerm::import_state(&s).expect("a link on the active screen");
        b.feed(switch);
        screen(&mut s, false);
        match PaneTerm::import_state(&s) {
            Ok(mut b) => {
                b.feed(switch);
                panic!("a link on the inactive screen imported");
            }
            Err(e) => assert!(matches!(e, StateError::Invalid(_)), "{e}"),
        }
    }
}

#[test]
fn corrupt_blobs_are_refused() {
    let mut a = source(false);
    let state = a.export_state_with(CompressedPages::Blob);
    let blobs: Vec<usize> = state
        .primary
        .pages
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p, PageState::Blob(_)))
        .map(|(i, _)| i)
        .collect();
    if blobs.is_empty() {
        // Ghostty reclaims compressed pages' memory on Linux and macOS only.
        let reclaims = cfg!(any(target_os = "linux", target_os = "macos"));
        assert!(a.compressed_pages() == 0 && !reclaims, "nothing compressed");
        return;
    }
    let good = PaneTerm::import_state(&state).expect("the blobs as exported");
    drop(good);
    let corrupt = |at: fn(usize) -> usize| {
        let mut s = state.clone();
        let PageState::Blob(b) = &mut s.primary.pages[blobs[0]] else {
            unreachable!()
        };
        let k = at(b.bytes.len());
        b.bytes[k] ^= 0x5a;
        s
    };
    // A byte of the page's metadata, and of its LZ4 data: before the
    // checksum both imported, and the LZ4 one only failed on the page's
    // first read (`PageList.restore`, "failed to restore compressed
    // terminal page"), after an upgrade's exec.
    for (name, at) in [
        ("metadata", (|_| 100) as fn(usize) -> usize),
        ("compressed data", |len| len - 5),
        ("header", |_| 9),
    ] {
        let s = corrupt(at);
        for check in [ImportCheck::Lazy, ImportCheck::Full] {
            match PaneTerm::import_state_with(&s, check) {
                Ok(mut t) => {
                    use_pane(&mut t);
                    panic!("corrupt blob {name} imported ({check:?})");
                }
                Err(e) => assert!(matches!(e, StateError::Ghostty(_)), "{name}: {e}"),
            }
        }
    }
    let mut s = state.clone();
    if let PageState::Blob(b) = &mut s.primary.pages[blobs[0]] {
        b.bytes.truncate(b.bytes.len() / 2);
    }
    assert!(PaneTerm::import_state(&s).is_err(), "truncated blob");
    // The source is untouched by all this.
    use_pane(&mut a);
}

/// The export, with what two exports of one terminal differ in (the
/// time left on a sync hold) zeroed.
fn exported(t: &PaneTerm, form: CompressedPages) -> TermState {
    let mut s = t.export_state_with(form);
    if let Some((_, left)) = s.pane.sync.as_mut() {
        *left = std::time::Duration::ZERO;
    }
    s
}

/// Stream `a` into a new pane: the head, then each page as it is
/// exported, one at a time.
fn streamed(a: &PaneTerm, form: CompressedPages, check: ImportCheck) -> PaneTerm {
    let head = a.export_state_head();
    assert!(head.primary.pages.is_empty(), "the head has no pages");
    let mut import = StateImport::new(&head, check).expect("head");
    a.export_pages(form, |screen, page| import.page(screen, &page))
        .expect("pages");
    import.finish().expect("finish")
}

#[test]
fn streamed_state_is_the_whole_state() {
    for alternate in [false, true] {
        let a = source(alternate);
        for form in [CompressedPages::Rows, CompressedPages::Blob] {
            // The head and the pages are the whole export, in its order.
            let mut whole = a.export_state_head();
            a.export_pages(form, |screen, page| {
                match (screen, whole.alternate.as_mut()) {
                    (ScreenKind::Alternate, Some(alt)) => alt.pages.push(page),
                    (ScreenKind::Primary, _) => whole.primary.pages.push(page),
                    (ScreenKind::Alternate, None) => return Err("alternate page, no screen"),
                }
                Ok(())
            })
            .expect("pages");
            assert_eq!(
                whole,
                a.export_state_with(form),
                "alternate {alternate} {form:?}"
            );
            for check in [ImportCheck::Lazy, ImportCheck::Full] {
                let b = streamed(&a, form, check);
                assert_eq!(
                    exported(&b, form),
                    exported(&a, form),
                    "alternate {alternate} {form:?} {check:?}"
                );
            }
        }
    }
}

#[test]
fn a_refused_page_leaves_the_import_as_it_was() {
    let a = source(false);
    let state = a.export_state();
    let mut import = StateImport::new(&state, ImportCheck::Lazy).expect("head");
    let mut bad = state.primary.pages[0].clone();
    if let PageState::Rows(r) = &mut bad {
        r.cols = 7;
    }
    assert!(import.page(ScreenKind::Primary, &bad).is_err());
    assert_eq!(
        import.page(ScreenKind::Alternate, &state.primary.pages[0]),
        Err(StateError::Invalid("page of an absent alternate screen"))
    );
    for page in &state.primary.pages {
        import.page(ScreenKind::Primary, page).expect("page");
    }
    let b = import.finish().expect("finish");
    assert_eq!(
        exported(&b, CompressedPages::Rows),
        exported(&a, CompressedPages::Rows)
    );
}

#[test]
fn undersized_capacity_hints_fill_the_page_again() {
    // Another Ghostty build's capacities may not hold the content: that
    // page is filled again sized from its content, the others keep
    // theirs.
    let mut a = source(false);
    let mut state = a.export_state();
    let p = rich_page(&mut state);
    p.capacity.styles = 1;
    p.capacity.string_bytes = 1;
    let mut b = PaneTerm::import_state(&state).expect("import");
    let all = CaptureOpts {
        start: LineRef::Start,
        end: LineRef::End,
        escapes: true,
        hyperlinks: true,
        ..CaptureOpts::default()
    };
    assert_eq!(capture(&b, all), capture(&a, all));
    assert_eq!(use_pane(&mut b), use_pane(&mut a));
}

/// Replace the page's first grapheme with `n` codepoints after its first.
fn set_first_grapheme(p: &mut PageRows, n: usize) {
    let old = p.graphemes[1] as usize;
    let rest = p.graphemes.split_off(2 + old);
    p.graphemes.truncate(1);
    p.graphemes.push(u32::try_from(n).unwrap());
    p.graphemes.extend(std::iter::repeat_n(0x301, n));
    p.graphemes.extend(rest);
}

/// `n` styles no page of `source` has (RGB foregrounds).
fn new_styles(n: usize) -> impl Iterator<Item = StyleState> {
    (0..n).map(|i| {
        let [_, r, g, b] = u32::try_from(i).unwrap().to_be_bytes();
        StyleState {
            style: Style {
                fg: Color::Rgb(r, g, b),
                bold: true,
                ..Style::default()
            },
            ..StyleState::default()
        }
    })
}

/// A resource Ghostty bounds: `at` puts it at the bound, which imports
/// and is a terminal that works (output, resizes, both screens); `past`
/// one past it, which is refused before Ghostty sees it. Past each of
/// these Ghostty did not fail an import: it crashed, wrote past a bitmap
/// or overflowed a reference count (the import-validation re-review).
fn bound(
    base: &TermState,
    name: &str,
    at: impl FnOnce(&mut TermState),
    past: impl FnOnce(&mut TermState),
) -> PaneTerm {
    let mut s = base.clone();
    past(&mut s);
    match PaneTerm::import_state(&s) {
        Ok(mut t) => {
            use_pane(&mut t);
            panic!("{name}: past the bound imported");
        }
        Err(e) => assert!(matches!(e, StateError::Invalid(_)), "{name} past: {e}"),
    }
    let mut s = base.clone();
    at(&mut s);
    let mut t = PaneTerm::import_state(&s).unwrap_or_else(|e| panic!("{name}: at the bound: {e}"));
    let back = t.export_state();
    use_pane(&mut t);
    t.feed(b"\x1b[?1049hx\x1b[?1049ly\x1b[?1049h\x1b[?1049l");
    for check in [ImportCheck::Lazy, ImportCheck::Full] {
        PaneTerm::import_state_with(&s, check)
            .unwrap_or_else(|e| panic!("{name}: at the bound ({check:?}): {e}"));
    }
    PaneTerm::import_state(&back).unwrap_or_else(|e| panic!("{name}: exported again: {e}"))
}

fn links_of(t: &PaneTerm) -> Vec<LinkState> {
    let s = t.export_state();
    s.primary
        .pages
        .iter()
        .filter_map(|p| match p {
            PageState::Rows(r) => Some(r.links.iter().flatten().cloned().collect::<Vec<_>>()),
            PageState::Blob(_) => None,
        })
        .flatten()
        .chain(s.primary.cursor.link)
        .collect()
}

/// An OSC 8 is at most 2047 bytes after its `8;` (`osc.zig`
/// `Parser.MAX_BUF` with a NUL): `;URI`, or `id=ID;URI`.
const URI: usize = 2046;
const ID: usize = 2047 - "id=;http://id".len();
/// A 16-bit style capacity's set holds 53247 styles but the default
/// (`ref_counted_set.zig` `Layout.init`: 13/16 of 65536 buckets, id 0
/// reserved).
const STYLES: usize = 53_247;

#[test]
fn hyperlinks_at_and_past_their_bounds() {
    let base = source(false).export_state();
    let uri = |n: usize| {
        move |s: &mut TermState| rich_page(s).links[1].as_mut().unwrap().uri = vec![b'h'; n]
    };
    let t = bound(&base, "page link URI", uri(URI), uri(URI + 1));
    assert!(
        links_of(&t).iter().any(|l| l.uri.len() == URI),
        "the long URI is kept"
    );
    bound(&base, "empty page link URI", uri(1), uri(0));
    // With an id, the two share the sequence.
    let id = |n: usize| {
        move |s: &mut TermState| {
            let l = rich_page(s).links[1].as_mut().unwrap();
            l.uri = b"http://id".to_vec();
            l.id = LinkId::Explicit(vec![b'i'; n]);
        }
    };
    let t = bound(&base, "page link id", id(ID), id(ID + 1));
    assert!(
        links_of(&t)
            .iter()
            .any(|l| l.id == LinkId::Explicit(vec![b'i'; ID])),
        "the long id is kept"
    );
    bound(&base, "empty page link id", id(1), id(0));
    let cursor = |n: usize, id: LinkId| {
        move |s: &mut TermState| {
            s.primary.cursor.link = Some(LinkState {
                uri: vec![b'c'; n],
                id,
            });
        }
    };
    let t = bound(
        &base,
        "cursor link URI",
        cursor(URI, LinkId::Implicit(3)),
        cursor(URI + 1, LinkId::Implicit(3)),
    );
    assert!(links_of(&t).iter().any(|l| l.uri.len() == URI));
    bound(
        &base,
        "cursor link id",
        cursor(9, LinkId::Explicit(vec![b'i'; ID])),
        cursor(9, LinkId::Explicit(vec![b'i'; ID + 1])),
    );
    bound(
        &base,
        "empty cursor link id",
        cursor(9, LinkId::Explicit(vec![b'i'])),
        cursor(9, LinkId::Explicit(Vec::new())),
    );
    bound(
        &base,
        "link repeated",
        |s| {
            let p = rich_page(s);
            let mut other = p.links[1].clone().unwrap();
            other.uri.push(b'/');
            p.links.push(Some(other));
        },
        |s| {
            let p = rich_page(s);
            let again = p.links[1].clone();
            p.links.push(again);
        },
    );
}

#[test]
fn page_tables_at_and_past_their_bounds() {
    let base = source(false).export_state();
    // page.zig `grapheme_max_len`: codepoints after a cell's first.
    let t = bound(
        &base,
        "grapheme",
        |s| set_first_grapheme(rich_page(s), 64),
        |s| set_first_grapheme(rich_page(s), 65),
    );
    assert_eq!(rich_page(&mut t.export_state()).graphemes[1], 64);
    let styles = |n: usize| {
        move |s: &mut TermState| {
            let p = rich_page(s);
            let more = n + 1 - p.styles.len();
            p.styles.extend(new_styles(more));
        }
    };
    bound(&base, "styles", styles(STYLES), styles(STYLES + 1));
    // An export writes each style and link once: a table that repeats one
    // counts it again in its 16-bit reference count (65534 copies
    // overflowed it in `RefCountedSet.addContext`). Here the table is as
    // large as it may be, all copies of one style.
    let repeated = |copy: bool| {
        move |s: &mut TermState| {
            let p = rich_page(s);
            let more = STYLES + 1 - p.styles.len();
            if copy {
                let again = p.styles[1];
                p.styles.extend(std::iter::repeat_n(again, more));
            } else {
                p.styles.extend(new_styles(more));
            }
        }
    };
    bound(&base, "style repeated", repeated(false), repeated(true));
    // A style or link is counted once per cell in a 16-bit count: a page
    // has room for 65519 cells (40 x 1637) and Ghostty's few references of
    // its own. Every cell takes one style to fill it.
    let room = |rows: u32| {
        move |s: &mut TermState| {
            let p = rich_page(s);
            p.capacity.rows = rows;
            for c in &mut p.cells {
                c.style = 1;
            }
        }
    };
    bound(&base, "page cells", room(1637), room(1638));
}

#[test]
fn hyperlinks_past_a_page_s_capacity_are_an_error() {
    // Ghostty sizes a page's hyperlink set from 16-bit bytes; more links
    // than it holds are refused, not a crash.
    let base = source(false).export_state();
    let mut s = base.clone();
    let p = rich_page(&mut s);
    for i in 0..5000 {
        p.links.push(Some(LinkState {
            uri: format!("http://many/{i}").into_bytes(),
            id: LinkId::Implicit(1),
        }));
    }
    let links = p.links.len();
    for (i, c) in p.cells.iter_mut().enumerate() {
        c.link = u16::try_from(1 + i % (links - 1)).unwrap();
    }
    match PaneTerm::import_state(&s) {
        Ok(mut t) => {
            use_pane(&mut t);
            panic!("more links than a page holds imported");
        }
        Err(e) => assert!(matches!(e, StateError::Ghostty(_)), "{e}"),
    }
    // As many as an export can have: Ghostty's own page full of them.
    let mut t = PaneTerm::new(40, 8, 3000);
    for i in 0..3000 {
        t.feed(format!("\x1b]8;;http://x/{i}\x07a\x1b]8;;\x07\r\n").as_bytes());
    }
    let mut b = PaneTerm::import_state(&t.export_state()).expect("a page full of links");
    assert_eq!(use_pane(&mut b), use_pane(&mut t));
}

#[test]
fn every_width_s_pages_import() {
    // A page's room for cells (`MAX_PAGE_CELLS`) is above what Ghostty's
    // pages have at any width a pane can have.
    for cols in [1, 2, 3, 7, 40, 80, 214, 215, 216, 500, 1000, 4096, 10_000] {
        let mut t = PaneTerm::new(cols, 5, 100_000);
        let line = format!("\x1b[1;3{}m{}\r\n", cols % 8, "x".repeat(usize::from(cols)));
        for _ in 0..(400_000 / usize::from(cols)).clamp(10, 3000) {
            t.feed(line.as_bytes());
        }
        let s = t.export_state();
        PaneTerm::import_state(&s).unwrap_or_else(|e| panic!("{cols} columns: {e}"));
    }
}

#[test]
fn the_osc8_bound_is_the_parser_s() {
    // The longest link a program can open is the longest an import takes
    // (`MAX_OSC8_BYTES`), with and without an id, and one byte more opens
    // none.
    let opened = |id: &str, uri: usize| {
        let mut t = PaneTerm::new(40, 8, 100);
        let mut bytes = format!("\x1b]8;{id};").into_bytes();
        bytes.extend(std::iter::repeat_n(b'h', uri));
        bytes.extend_from_slice(b"\x07x");
        t.feed(&bytes);
        let links = links_of(&t);
        assert!(links.len() <= 2, "{links:?}");
        let found = links.iter().any(|l| l.uri.len() == uri);
        if found {
            PaneTerm::import_state(&t.export_state()).expect("the parser's longest link");
        }
        found
    };
    assert!(opened("", 2046) && !opened("", 2047));
    let id = format!("id={}", "i".repeat(100));
    let uri = 2047 - id.len() - 1;
    assert!(opened(&id, uri) && !opened(&id, uri + 1));
}
