//! A malformed exported state is refused with a [`StateError`], never
//! imported to crash later (live upgrade spec, decision 23; the review of
//! the spike found each case below aborting or segfaulting in Ghostty
//! after an import that succeeded). And the streamed form
//! (`export_state_head` + `export_pages`, `StateImport`) is the same
//! state as the whole one.

use vtmux_vt::{
    CaptureOpts, CompressedPages, Compression, ImportCheck, LineRef, PageRows, PageState, PaneTerm,
    ScreenKind, StateError, StateImport, TermState, capture,
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
