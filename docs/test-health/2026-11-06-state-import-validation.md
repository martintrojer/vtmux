# Terminal state import validation and streaming, 2026-11-06 (high_state_import_validation)

The review of the live-upgrade spike (review_lu_spike) found that a malformed state imported, then
aborted, segfaulted or broke Ghostty's invariants later: rows pages with wild grapheme counts, page
widths or capacity hints, states whose width disagreed with their pages or whose scroll region was out
of order, and corrupt compressed-page blobs, which only failed on the page's first read (after an
upgrade's exec). This change makes every such import an error and adds the streamed form.

## What changed

- `crates/vtmux-vt/src/term/state/validate.rs`: the whole `TermState` is checked in Rust before Ghostty
  sees it. Sizes (at most tmux's `WINDOW_MAXIMUM`), cursors on the screen, scroll regions in order and
  on the screen, scalar previous characters, tab stops, charsets, Kitty flags and protected mode in
  range. Every rows page must have the terminal's width, contiguous row runs and in-range cell, style,
  link and grapheme indexes, valid codepoints, wide characters with their spacers (`page.zig`
  `verifyIntegrity`'s rules), and capacity hints that fit Ghostty's 16-bit fields. The pane model is
  checked too: history sizes and ids bounded, `times` within `scrolled`, frozen rows and the held sync
  screen consistent. After the pages are in, `hsize` must fit the rows retained (the frozen copy under
  an alternate screen), and every OSC 133 mark and the alternate-entry pin must land on a row.
- Patch 0017 (0015 before the rebase onto main), the second line of checks: checked arithmetic in `capacityFor`, refusing a page of
  another width or a layout over `size.max_page_size`, and graphemes on cells without a codepoint.
  Blobs carry an XxHash3 checksum over the `Page` value and the LZ4 bytes, and the `Page` value must
  be its capacity's `initBuf` layout. `importer_finish` refuses a bad cursor, region or previous
  character before it changes anything. `ghostty_vtmux_importer_new` takes `check`: decode every blob
  and run `verifyIntegrity` on every page now (the dry run). When another build's capacity hint is too
  small, only that page is refilled at the counted capacity (the half-built page is taken off the
  builder), which replaces the whole-import retry.
- `PATCH_STACK` also hashes `zig version`.
- Streaming: `PaneTerm::export_state_head`, `PaneTerm::export_pages(form, |ScreenKind, PageState| ..)`
  and `StateImport::new(&head, ImportCheck) / .page(screen, &page) / .finish()`. `export_state[_with]`
  and `import_state[_with]` wrap them.

## Fuzz (reviewer's probe approach, ReleaseSafe Zig)

`crates/vtmux-vt/examples/state_fuzz.rs` is the review's `rvprobe fuzz` mutation set, with more state
fields, built with `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe`. It mutates one to three fields of an
80x24 export with 4000 lines of styled, linked, wide and grapheme history (compressed; every other
seed with an alternate screen and a sync hold up), imports it and, when the import succeeds, uses the
pane: captures, input, two resizes, compression, a history-limit cut, a second export. A driver
restarts it after a crash.

| kind | seeds | imported and used | refused | Zig crash | Rust panic |
|---|---|---|---|---|---|
| rows | 0-1199 | 44 | 1156 | 0 | 0 |
| state | 0-1199 | 291 | 909 | 0 | 0 |
| blob (`ImportCheck::Lazy`) | 0-1199 | 12 | 1188 | 0 | 0 |
| blob-full (`ImportCheck::Full`) | 0-1199 | 12 | 1188 | 0 | 0 |

Before the fix, the review saw 8 rows and 2 state crashes in 300 seeds each, and 73 blob crashes in
300. The first state run of this fuzzer also found two Rust panics, both fixed here: `hsize` far over
the frozen copy under an alternate screen (a capture loop over billions of rows), and `times` longer
than `scrolled` (a subtraction overflow in `account_scroll`). The 12 blobs that import are mutations
that left the bytes as they were.

The probe's 56 round-trip scenarios and both phase-2 chains (`rvprobe` without arguments, against
this tree, ReleaseSafe) still pass: 58 ok, 0 fail.

## Streamed export and import, 10 panes x 100k lines (120x40, release)

`state_bench 10 100000` (whole `TermState`) against `STATE_BENCH_STREAMED=1 state_bench 10 100000`
(each pane from `export_pages` straight into a `StateImport`):

| path | pages as | time | RSS added | peak RSS of the run |
|---|---|---|---|---|
| whole | Blob | 362 ms | +50 MB export, +61 MB import | 2679 MB |
| whole | Rows | 1628 ms | +624 MB export, +938 MB import | (same run) |
| streamed | Blob | 350 ms | +56 MB | 1956 MB |
| streamed | Rows | 1272 ms | +929 MB | (same run) |

The streamed path never holds a `TermState` (no +624 MB). The +929 MB is the imported panes' own
history, resident after a rows import until the compressor takes it back (decision 23). At 1 x 100k the
peak is 211 MB streamed against 277 MB whole. Every import still captures as its source does.

## Gates

Slow gate (`scripts/check.sh`): exit 0 in 24 min (over the 10-minute budget; the gate as a whole,
not these tests, which take 20 s): cargo tests pass, regress 158/174 pass with 12 xfail, 1 skip and
3 flaky, oracle replay 272/272. The macOS type-check was skipped (`aarch64-apple-darwin` is not
installed on this host): a skip, not a pass. Also run: `zig build test-lib-vt -Demit-lib-vt=true
-Dapp-runtime=none`, the full suite in Debug: 3256 pass, 38 skip, and two known failures per binary
(`tabClear single`, and kitty `temporary file outside directory prefix is rejected`, which fails with
the checkout under `/tmp`). The new tests replayed 5 times under `nproc/2` busy loops: all pass. They
spawn no shell, so the dash-like `/bin/sh` run does not apply.
