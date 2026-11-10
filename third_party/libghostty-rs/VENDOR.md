# Vendored libghostty-rs

`libghostty-vt` and `libghostty-vt-sys` are the crates.io 0.2.2 sources of
[libghostty-rs](https://github.com/uzaaft/libghostty-rs) (git `c9876b29`),
MIT OR Apache-2.0 (`LICENSE`, from the same commit). `vtmux-vt` depends on
them by path. The workspace excludes `third_party/`, so vtmux's lints, `fmt`
and doctests do not run on this code.

## Published as vtmux's own crates

The upstream names are taken on crates.io (by upstream, at another Ghostty
and without these patches), so the fork publishes as `vtmux-ghostty-vt-sys`
and `vtmux-ghostty-vt`, at vtmux's version (`docs/RELEASING.md`). The
libraries keep upstream's names (`[lib] name`), and `vtmux-vt` renames the
dependency back (`package = "vtmux-ghostty-vt"`), so no code says the new
names. Licence: MIT, one of upstream's two, for the wrapper and for vtmux's
changes here; each crate ships libghostty-rs's `LICENSE`, and the sys crate
also `LICENSE-ghostty` (Ghostty's MIT licence, from the pinned commit), as
its build script downloads, patches and links in Ghostty. Folding the two
into `vtmux-vt` would have put them under the workspace lints and lost the
per-file diff against upstream this file keeps.

## Where the Ghostty source comes from

The sys crate's build script downloads GitHub's archive of
`GHOSTTY_COMMIT` with curl, checks it with `zig fetch` against
`GHOSTTY_HASH` (Zig's package hash: of the files, not of the compressed
bytes, so `git archive` of the commit gives the same), extracts it into
`OUT_DIR/ghostty-src` and applies `patches/*.patch` (name order, `git
apply` in a fresh `git init`, so a target dir inside another repository
does not take the paths). The packaged crate carries the patches (230 KiB
compressed), not Ghostty (150 MB unpacked, 40 MB as a tarball: over the
10 MB crate limit, and a hand-picked subset of it would need re-picking at
every bump). `GHOSTTY_SOURCE_TARBALL=<absolute path>` uses a tarball
already downloaded (an offline build: `git archive --format=tar.gz
<commit>` of a clone works too, with or without `--prefix`),
`GHOSTTY_SOURCE_URL` a mirror. Zig's own dependencies
(uucode, zigimg and others) come from `deps.files.ghostty.org` or Zig's
global cache, as before (`GHOSTTY_ZIG_SYSTEM_DIR` for a sandbox). The
script stops with what to install when `zig` is missing or not
`ZIG_VERSION` (0.17), or curl, tar or git is missing. docs.rs builds with
`DOCS_RS` set, which skips the native build: the checked-in bindings are
enough for rustdoc.

## Why vendored

The published 0.2.2 pins ghostty `a887df42c`, which predates the fix for
GHSA-w9q4-5jvv-mcv4 (ghostty `e257bb45`, PR #14857) and the runtime
`GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES` setter. No newer libghostty-vt is
published, and the sys crate only takes a `GHOSTTY_SOURCE_DIR` override, so
the pin is bumped here.

## Local changes

Recorded per commit in git history; the list is kept current:

- `patches/0041-unicode-options-thread-safe.patch`: the width overrides
  are one atomic pointer to an immutable table (ranges and the printer's
  narrow bound), never freed: setting the same ranges again reuses their
  table, so the memory kept is one table per distinct set. The clustering
  flags are one atomic byte. No lock anywhere; `printSliceFast`,
  `printSliceFill` and `print` read the table and flags once per call,
  except the narrow bound in `printSliceFill`'s scalar loop, read per
  character: holding it in a register cost that loop 5% on UTF-8 text.
  Printing speed matches the build without this patch. This covers
  printing as well as both width queries, so the Rust setters (including
  vtmux-vt's) remain safe. The server sets them from its single-threaded
  `set_option` (`codepoint-widths`, tmux `utf8.c`
  `utf8_update_width_cache`; clustering, `screen-write.c`
  `screen_write_combine`). A subprocess test runs two writers and two
  readers, including terminal printing; it segfaulted before the patch and
  segfaults with a setter that frees the old table.

- `libghostty-vt/src/alloc.rs` (`allocator_api`): `From<&A>` borrows the
  original allocator for the adapter lifetime; `From<A>` kept a pointer
  to its destroyed parameter. Tests check the callback context, allocate,
  free, grow and shrink, and reject owned or too-short-lived allocators
  at compile time. vtmux does not enable this optional feature.

- `libghostty-vt/src/osc.rs`: window titles read the `const char **`
  output specified by `osc.h`, then borrow and validate the parser-owned
  C string. The generic getter previously fabricated a Rust `&str` with
  a zero length. Tests cover ASCII, empty and UTF-8 titles.

- `patches/0040-grid-ref-bounds.patch`: `CGridRef.toPin` checks both
  coordinates against the page before any native reader dereferences a
  cell. `GridRef::with_x` remains safe; out-of-range readers return
  `InvalidValue` even in ReleaseFast. Both vtmux capture call sites take
  columns from `row_cells`, already within the page. The Rust test checks
  the last valid column, the first invalid one and `u16::MAX`.

- `libghostty-vt/src/key.rs`, `mouse.rs`: growable encoders reserve the
  required output length relative to the vector length, not its remaining
  capacity. Tests cover empty and prefixed buffers one byte short.

- `libghostty-vt/Cargo.toml`: `path` on the `libghostty-vt-sys` dependency.
- Both `Cargo.toml`s: published as `vtmux-ghostty-vt-sys` and
  `vtmux-ghostty-vt` at vtmux's version, licence MIT, vtmux's repository,
  an `include` list (above); the redundant dev-dependency on the sys crate
  is gone.
- `libghostty-vt-sys/build.rs`: the Ghostty source is GitHub's archive of
  the commit, checked with `zig fetch` (above), where upstream cloned it
  with git; the Zig version is checked first.
- `libghostty-vt-sys/build.rs`: `GHOSTTY_COMMIT` = `3bd54e025cc3` (ghostty
  main, 2026-10-31), which contains `e257bb45`.
- `libghostty-vt-sys/src/bindings.rs`: regenerated from that commit's
  headers with `gen-bindings`, then `cargo fmt`.
- `libghostty-vt/src`: API drift from `a887df42c`, ported in the shape
  libghostty-rs master uses:
  - `ghostty_terminal_new(alloc, &t, cols, rows)`: `GhosttyTerminalOptions`
    is gone. `TerminalOptions` stays; `max_scrollback` (always bytes) is
    applied through the new `Terminal::set_scrollback_max_bytes`.
  - `ghostty_terminal_mode_get/set` are gone: `Terminal::mode`/`set_mode`
    go through `TERMINAL_DATA_MODE`/`TERMINAL_OPT_MODE`.
  - `ghostty_render_state_colors_get` is gone: `RENDER_STATE_DATA_COLORS`.
  - The clipboard-write callback returns `void` and answers through the
    request's `reply` function; the wrapper's trampoline calls it.
  - New `Terminal::scrollback_max_bytes`/`set_scrollback_max_bytes`.
- `libghostty-vt/src/terminal.rs`: `Terminal::scrollback_max_lines` and
  `set_scrollback_max_lines` (`GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES`,
  `GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_LINES`, already in the bindings);
  vtmux keeps history by line count with it.
- `libghostty-vt/src/terminal.rs`: `Terminal::memory_usage`
  (`GHOSTTY_TERMINAL_DATA_MEMORY_USAGE`, a sized struct, already in the
  bindings) re-exported as `terminal::MemoryUsage`; vtmux's compression
  tests read compressed pages and resident bytes from it.
- `libghostty-vt/src/terminal.rs`: `Terminal::on_desktop_notification`
  (`GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION`, `GhosttyTerminalDesktopNotification`,
  already in the bindings): the title and body of an OSC 9 (not ConEmu)
  or OSC 777 notification, as bytes. vtmux sends them on to Ghostty
  clients (spec D30).
- `libghostty-vt-sys/build.rs`: applies `libghostty-vt-sys/patches/*.patch`
  (name order, `git apply`) to the fetched ghostty; the source stamp
  records the commit and the patch contents. Not applied to a
  `GHOSTTY_SOURCE_DIR` tree.
- `libghostty-vt-sys/build.rs`: `rerun-if-changed=build.rs`, not
  upstream's `crates/libghostty-vt-sys/build.rs` (a path of its repo
  layout): cargo counts a missing file as changed, so every cargo
  command reran the script and rebuilt `libghostty-vt-sys` and every
  crate above it, `target/debug/vtmux` included.
- `patches/0001-keep-one-free-page-resident.patch` (`PageList.zig`
  `destroyNodeExt`, `keepWarm`): a freed pool page is zeroed in place
  instead of decommitted while no other free page is resident. With a
  line limit and no byte limit, Ghostty erases the oldest page each time
  output fills one (`Limits.enforce`), `madvise(MADV_DONTNEED)` on Linux;
  the next page comes from the same pool item and faults all 98 of its
  4 KiB pages back in. vtmux's 1M-line flood went from 0.26 s to 0.91 s
  (506k minor faults) without it, 0.27 s with it
  (`docs/test-health/2026-11-01-history-memory.md`). Upstream candidate.
  ghostty's `zig build test-lib-vt` with it: 6977/7053 pass, 74 skip; the
  one failure (`kitty.graphics_image` "temporary file outside directory
  prefix is rejected", in both test binaries) fails the same way on
  unpatched 3bd54e025 on this host.

- `patches/0002-tab-cell-mark.patch` (`page.zig` `Cell.tab`,
  `Terminal.zig` `horizontalTab`/`markTab`/`writeCell`,
  `c/cell.zig`, `include/ghostty/vt/screen.h`, the snapshot codec's
  layout probe): a horizontal tab that moves over blank cells all alike
  (tmux's HT rule, input.c `input_c0_dispatch`) marks the cells in 8
  spare bits of `Cell` (width of the tab on the first cell, 255 on the
  others), `GHOSTTY_CELL_DATA_TAB` reads it, and writing over any cell of
  a tab clears the whole mark (`screen_write_overwrite`); the print
  fast path skips a cell that carries a mark. Other erase and shift
  operations drop the mark with the cell, as they do for tmux's TAB cell
  when the cell is cleared. The bits are not in the snapshot wire
  format, so a tab does not survive a snapshot round trip. `Cell.tab` and
  `Cell::tab()` are reflected in `bindings.rs` and `screen.rs`.
  Upstream candidate if Ghostty ever wants tab cells (`vtmux-vt`
  `capture.rs` is the one consumer). Not run: ghostty's own
  `zig build test-lib-vt` (`zig build` of the library with the patch
  passes and vtmux's goldens exercise it).

  Checked by diffing the old and new bindings: no constant was renumbered
  or removed, and struct layouts only grew at the end (`ClipboardWrite`,
  `TerminalScrollViewportValue`).

- `patches/0003-resize-without-faulting-blank-memory.patch`
  (`PageList.zig` `trimTrailingBlankRows`, `keepWarm`): idle panes stay
  small after a resize. A height shrink trims trailing blank rows with
  `resetRow`, which memsets cells that are already zero and faults them
  in (200x50 to 200x25: +40 KB); an all-zero row without managed memory
  now only gets its header reset. `keepWarm` (patch 0001) now keeps a
  freed page resident only when every row of it was in use (the line
  limit's pruned page). The page a width reflow replaces used only a few
  rows, and zeroing it faulted the whole 400 KB pool item in.
  `scripts/bench.py scale` (400 idle panes): 33.2 to 15.3 KB/pane; a
  width change of an 80x24 pane: 400 KB to 8.5 KB. The 1M-line flood
  keeps its 0.72 s. ghostty's `zig build test-lib-vt
  -Dtest-filter=PageList` passes with it.

- `patches/0004-work-bounded-compression.patch` (`PageList.zig`,
  `compress/Page.zig`): oversized resident pages (`compression.Page.chunked`)
  are encoded from an owned snapshot in 128 KiB LZ4 blocks per incremental
  call. Snapshot work is capped at 4 MiB; bigger pages remain resident.
  Publication validates node generation, activity, metadata and source bytes;
  every failed or no-op attempt advances. Grid page layout and pruning are
  unchanged. Full mode and prepend compress an oversized page as one chunked
  job (`compressPage` sends it to `compressPageChunked`); smaller pages keep
  the original block encoding. Encoded storage uses the page allocator and
  shrinks in place with `remap`, falling back to dupe and free when the
  allocator can't remap. The chunked format decodes through restore and
  `cloneBuf` (chunk by chunk).
  Constraint: `compression.Page` must stay at its upstream size (a comptime
  assert): the chunked bookkeeping lives outside the struct. Growing it
  (the round-2 version added two fields, so a PageList node went from 488 to
  512 bytes) made every frame's cell copy fault in more pages and slowed a
  busy pane by 16-20% p50/p95 with no compression running (task
  hist_compress_r3).
  Tests: `zig build test-lib-vt -Dtest-filter="chunked Page" -Doptimize=ReleaseSafe`
  and `-Dtest-filter="PageList incremental compression"`; the full upstream
  suite is not yet run.

- `patches/0005-clone-screen.patch` (`c/terminal.zig`, `c/grid_ref.zig`,
  `lib_vt.zig`, `c/main.zig`, `include/ghostty/vt/{terminal,grid_ref}.h`,
  `PageList.zig` `clone`/`cloneCompressedNode`): copy-mode entry takes a
  frozen copy of the pane's screen instead of reading every row through
  per-cell `grid_ref` calls (4.3 s on 100k lines; each call walked the
  page list from the top).
  - `ghostty_terminal_clone_screen`: `Screen.clone` of the active screen
    (history included) into a new terminal of the same size. Only the
    grid, cursor and selection are copied.
  - `ghostty_grid_ref_row_cells`: all cells of a grid ref's row in one
    call, so a row read walks the page list once.
  - `PageList.clone` copies a whole compressed page compressed: the copy
    gets the encoding (duplicated) over fresh backing memory of the same
    kind (pool item or heap) and decodes on first access, and the source
    is not restored. Upstream restored every compressed page to clone it,
    which cost about 60 ms on 100k lines and left the source pane's
    history uncompressed. The upstream test "PageList compression restores
    through page access" now expects the clone to stay compressed; a new
    test, "PageList clone keeps compressed history compressed", checks
    every row reads back the same.
  Wrapper: `Terminal::clone_screen`, `GridRef::row_cells`,
  `GridRef::with_x` (`libghostty-vt/src`), bindings added by hand to
  `bindings.rs`. Clone of 100k history lines at 120x40 (release, in
  process, `crates/vtmux-server/examples/copy_entry.rs`): 46 ms resident,
  5.6 ms compressed. Tests: `zig build test-lib-vt -Dtest-filter=PageList
  -Doptimize=ReleaseSafe` (697 pass), `-Dtest-filter=Screen` (553 pass),
  `-Dtest-filter=compress` (191 pass, 6 skip).

- `patches/0006-hyperlink-id.patch` (`c/grid_ref.zig`, `c/main.zig`,
  `lib_vt.zig`, `include/ghostty/vt/grid_ref.h`): `ghostty_grid_ref_hyperlink_id`
  reads the OSC 8 id of a cell's hyperlink (explicit bytes, or Ghostty's
  implicit counter for a link opened without `id=`), wrapped as
  `GridRef::hyperlink_id` / `screen::HyperlinkId` in `screen.rs` and
  `bindings.rs`. `capture-pane` needs it to tell links apart as tmux does
  (`hyperlinks_put`: same URI and id are one link): `-e` writes `id=` in
  its OSC 8, `-H` lists each link once, `-R` prints `linkid=`. Read-only,
  no layout change. Not run: ghostty's own `zig build test-lib-vt`
  (`zig build` of the library with the patch passes and vtmux's
  `cli.rs` `capture_pane_*` tests exercise it).

- `patches/0007-width-overrides.patch` (`unicode/main.zig`,
  `terminal/Terminal.zig` `print`/`printSliceFast`/`printSliceFill`,
  `c/unicode.zig`, `c/main.zig`, `lib_vt.zig`,
  `include/ghostty/vt/unicode.h`): tmux's `codepoint-widths` for the pane
  grid. `ghostty_unicode_set_width_overrides` sets process-wide ranges
  (sorted, not overlapping, widths 0 to 2; binary-searched, a no-op check
  while empty) that the printer and `codepointWidth` take over the table,
  as tmux's one width cache does. The printer's "0x00-0xFF is narrow"
  shortcuts (`print`, the batched narrow run and its SIMD scan) stop at
  0x7F while an override reaches below 0x100. Cells already printed keep
  their width. Wrapped as `unicode::set_width_overrides` in
  `libghostty-vt/src/unicode.rs`, the binding added by hand to
  `bindings.rs`. Tests: new `Terminal: width overrides (vtmux)` and
  `set_width_overrides` (`zig build test-lib-vt -Dtest-filter=width`);
  the full `zig build test-lib-vt` gives 3247 pass, 37 skip and the same
  two failures as without the patch (`tabClear single`, and the kitty
  temp-file test noted under 0001) on this host.

- `patches/0008-cluster-keeps-zero-width.patch` (`Terminal.zig` `print`):
  vtmux runs every pane with grapheme clustering (mode 2027), which is
  how tmux's `screen_write_combine` groups regional-indicator pairs, emoji
  modifiers and ZWJ sequences. Upstream drops a zero-width character that
  starts a new grapheme in that mode (U+200B, U+2028, U+2029); tmux and
  Ghostty without the mode attach it to the previous cell, so this patch
  removes the early return and it is attached in both modes. Test:
  `Terminal: zero-width character after grapheme cluster is kept (vtmux)`
  (in patch 0009's file, which adds it). Upstream candidate only as an
  option: dropping is a deliberate choice there.

- `patches/0009-c1-is-zero-width.patch` (`stream.zig` ground-state UTF-8
  paths, `Terminal.zig` `print`, `printSliceFast`, `printSliceFill`): a C1
  control decoded from UTF-8 (U+0080-U+009F) is printed as a zero-width
  character that joins the previous cell, as tmux's `utf8_width` makes it
  (wcwidth -1, a C1 control, 0). Upstream ignores it. `print` gives it
  width 0, and the batched narrow run (its SIMD scan now stops at 0x7F, the
  scalar loop takes 0xA0-0xFF) leaves it to `print`. The upstream test
  `stream: ground state UTF-8-decoded C1 controls are ignored` is now
  `... are printed (vtmux)`. Tests: `zig build test-lib-vt
  -Dtest-filter=vtmux` (45/45 steps), `-Dtest-filter=stream` (597 pass),
  `-Dtest-filter="Terminal: "`: 1011/1013, the same two `tabClear single`
  failures as the unpatched tree (patch 0002 marks the tab cells and
  dirties the row). Throughput: a 21.9 MB ASCII, 19.5 MB Latin-1 and 15 MB
  CJK flood through one `PaneTerm` (80x24, release) take 0.10/0.13/0.13 s,
  against 0.16/0.16/0.15 s on main before this and the scanner's ASCII run.
- `libghostty-vt/src/screen.rs`: `Cell::bits`, the cell's `u64`. `vtmux-vt`
  (`capture.rs` `CellBits`) decodes `page.zig`'s packed `Cell` from it
  instead of seven `ghostty_cell_get` calls per cell (copy mode reading a
  27-cell row: 2.2 to 1.65 µs, release, with the row kept).
  `cell_bits_match_the_getters` checks the decoding against the getters, so
  a bump that moves a field fails there.
- `libghostty-vt/src/screen.rs`: `GridRef::row_cells` writes the row's
  cells straight into the caller's `Vec` (its spare capacity, at least 256
  cells), where it allocated and zeroed a 256-cell buffer per call and
  copied from it; `Cell` is `#[repr(transparent)]` over `ffi::Cell` for
  that. A capture of a 1M-line history reads a row per line: the
  allocation, memset and copy were about 10% of `capture-pane -S -`
  (`docs/test-health/2026-11-09-performance-detail.md`, *Reading all of a history*).

- `patches/0010-palette-256-form.patch` (`style.zig` `Style.Flags`,
  `bgCell`, `Screen.zig` `setAttribute`, `page.zig` `Cell.content`,
  `c/style.zig`, `c/cell.zig`, `include/ghostty/vt/{style,screen}.h`):
  tmux keeps `COLOUR_FLAG_256` on a colour set by `38;5;N`/`48;5;N`
  (input.c `input_csi_dispatch_sgr_256_do`), so `capture-pane -e` writes
  `38;5;1` back apart from `31`, and an 8-colour terminal gets `38;5;9` as
  plain red where `91` is bold (tty.c `tty_check_fg`). Ghostty kept one
  palette index. Two spare `Style.Flags` bits (`fg_palette_256`,
  `bg_palette_256`) record the form; any other foreground or background
  change clears its bit. A cell erased with such a background carries the
  bit in its `color_palette` content (`palette_256`). C: two `bool`s at
  the end of the sized `GhosttyStyle` (offsets 68 and 69, the size stays
  72) and `GHOSTTY_CELL_DATA_COLOR_PALETTE_256`; Rust: `Style::fg_palette_256`
  / `bg_palette_256` and `Cell::bg_color_palette_256` (`style.rs`,
  `screen.rs`, `bindings.rs` by hand). The VT formatter still writes every
  palette colour as `38;5;N`, so its two round-trip tests ("Screen vt
  with style", "Terminal vt restores gaps and cells that only have a
  background") compare styles without the bits; the snapshot codec drops
  them. Tests: new `Terminal: 256-colour SGR keeps its form (vtmux)`;
  the full `zig build test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none`
  gives 3251 pass, 37 skip and the known `tabClear single` failure (see
  0009). Upstream candidate only for a consumer that needs the SGR form.

- `patches/0011-tmux-wrap-bookkeeping.patch` (`Terminal.zig`
  `insertLines`, `deleteLines`, `scrollDown`, `scrollUp`, `index`,
  `eraseLine`, `eraseDisplay`, `switchScreenMode`; `Screen.zig`
  `unwrapRow`; `stream_terminal.zig` CSI M): the soft-wrap flag follows
  tmux's `GRID_LINE_WRAPPED` bookkeeping (grid.c `grid_clear_lines`,
  `grid_move_lines`) instead of Ghostty's, so `capture-pane -J`, copy
  mode, reflow and the compositor see the lines tmux sees as wrapped.
  A full-width clear (EL 2, EL 0 from column 0, EL 1 from the last
  column, ED 0's lines below, ED 1's lines above, ED 2) unwraps the row
  above it, the last scrollback row for the top row; EL 0 from further
  right keeps the row's own flag, and with a pending wrap changes nothing
  (`screen_write_clearendofline`). IL/DL/SD/RI move rows with their flags
  (upstream unset them) and unwrap as `grid_move_lines` does: IL the row
  above the cursor, the last row of the inserted count before the move
  and the row its `grid_clear` touches (also when tmux's `ny - ny2`
  underflows); SD and RI the top row and the row above; DL (CSI M only,
  `deleteLinesTmux`; Ghostty's SU path through `deleteLines` keeps tmux's
  history-scroll behaviour) the row above and the last moved row. A
  region scrolled up without scrollback (the alternate screen) unwraps
  the row above it (`grid_view_scroll_region_up`), and entering the
  alternate screen unwraps the primary's last scrollback row
  (`screen_alternate_on`). After a move, the continuation flag of each
  row whose neighbour above changed follows that neighbour's wrap flag
  (`tmuxFixContinuations`, `tmuxFixContinuationAt`): IL/SD/RI over the
  region, DL and SU (`deleteLines`) the first moved row and the row
  below the region, LF/IND at a region's bottom on the primary screen
  (`cursorScrollRegionUp`) the region's top row and the row below it,
  and a region from the top scrolled into history (`cursorScrollAbove`)
  the row below it. Without that, a stale continuation made Ghostty's
  reflow drop a blank line (task medium_dl_wrap_flag_resize: DL moving
  an erased continuation under an unwrapped line). The blank row a
  scroll adds keeps its unset flag: patch 0012 reads that as tmux's
  empty line. The alternate screen does not reflow, so its LF skips
  this. Four upstream tests that assert the xterm behaviour now assert
  tmux's ("insertLines resets wrap", "deleteLines resets wrap",
  "deleteLines wide character spacer head", "eraseLine resets pending
  wrap"), and "deleteLines count over region blanks row metadata" runs
  CSI M (`deleteLinesTmux`); new tests `Terminal: tmux wrap bookkeeping
  (vtmux)` and `Terminal: tmux wrap continuations follow moved rows
  (vtmux)`. Not an upstream candidate (it is tmux's model, not a
  terminal's).

- `patches/0012-reflow-blank-continuation.patch` (`PageList.zig`
  `ReflowCursor.reflowRow`, new field `prev_wrap`): a blank row under a
  wrapped line reflows as tmux's `grid_reflow_join` does. A blank
  continuation (a continuation row erased with ECH or EL 1, where tmux
  keeps the erased cells in `cellused`) joins the wrapped line as one
  blank cell and ends it, so the next line stays a line of its own;
  upstream dropped the row and the line break with it, so a wider pane
  joined the line after it. A blank row that is not a continuation
  under a wrapped line (the empty line a region scroll adds,
  `grid_empty_line`, `cellused` 0) stops the join: it stays a blank
  line and the line above keeps its wrap flag. Tests: `Terminal: reflow
  keeps the line break after a blank continuation (vtmux)`, `Terminal:
  reflow keeps an empty line under a wrapped line (vtmux)`; oracle
  `wrap_flag_resize`. With 0011 and 0012 the full `zig build
  test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none` gives 3254 pass,
  37 skip and the known `tabClear single` failure, plus the kitty
  temporary-file test of 0001 when the tree is under `/tmp`. Upstream
  candidate for the first half (a blank continuation keeping its line
  break); the empty-line half is tmux's model.

- `patches/0013-tmux-cells-used.patch` (`page.zig` `Row.tmux_used`,
  `tmuxRaiseUsed`, `tmuxCellsUsed`; `Terminal.zig` `writeCell`,
  `printSliceFill`, `markTab`, `insertBlanks`, `deleteChars`,
  `eraseChars`, `eraseLine`; `PageList.zig` `ReflowCursor.reflowRow`):
  tmux's `cellused` per row (grid.c `struct grid_line`), in 16 of the
  Row's 23 spare bits. Printing raises it (`grid_set_cell`,
  `grid_set_cells`; a tab over blanks counts its width, a spacer head
  does not), ICH and DCH raise it to the cells they moved to
  (`grid_move_cells`: the width, and the width less the count), a
  partial erase leaves it and a full-line clear (EL 2, EL 0 from column
  0, EL 1 from the last column, ED's whole lines, ECH over a whole line
  from column 0, which also unwraps the line above) empties it with the
  row. Readers take the larger of it and the row's text extent, so a row
  from elsewhere (Ghostty's snapshot codec drops the field) measures by
  its text. Reflow measures an unwrapped row by it as `grid_reflow` does:
  a row that fits the new width keeps its background cells up to the
  width (`grid_reflow_move`), one that splits or joins the wrapped line
  above takes only its used cells (`grid_reflow_split`,
  `grid_reflow_join`), so a line erased with a background no longer
  takes extra rows and written cells erased with the default background
  still do; a row under a wrapped line with none used is tmux's empty
  line (replacing patch 0012's continuation-flag test), and an erased
  continuation joins with all its erased cells (0012 joined one); a
  cursor past the used cells goes to their end (`grid_wrap_position`).
  The new rows get the used count of the cells copied into them. The
  upstream test "Screen: resize less cols reflows cursor after empty
  cells" now expects tmux's cursor (3,0); new tests `Terminal: reflow
  measures a line by the cells it used (vtmux)`, `Terminal: cells used
  follow tmux's cellused (vtmux)`; oracle `reflow_cells_used`. Not an
  upstream candidate (tmux's model).

- `patches/0014-tmux-pending-wrap.patch` (`Terminal.zig` `backspace`,
  `cursorLeft`, `horizontalTabBack`, `restoreCursor`, `index`,
  `printWrap`, `reverseIndex`, `setCursorRow`, `insertLinesTmux`,
  `deleteLinesTmux`, `insertBlanks`, `deleteChars`, `eraseChars`,
  `eraseLine`, `eraseDisplay`, `printRepeat`; `stream_terminal.zig`
  CSI L): Ghostty's pending wrap is tmux's `cx` past the last column,
  and each sequence treats it as tmux does. BS and CUB move from there
  (BS to the last column; CUB n to width - n); BS at the first column
  of a continuation goes to the end of the line above
  (`screen_write_backspace`). ICH, DCH and ECH do nothing and EL, ED 1
  and ED 2 clear without dropping the wrap (screen-write.c: no cells
  past the column, and no clear moves the cursor). LF/IND, RI and VPA
  move the row and keep it (`screen_write_linefeed`,
  `screen_write_reverseindex`, `screen_write_cursormove` with px -1);
  IL and DL leave the cursor where it was, column included, with or
  without a pending wrap (`screen_write_insertline`,
  `screen_write_deleteline`; Ghostty moved it to the left margin). CBT
  starts from the last column (`INPUT_CSI_CBT`). DECRC and leaving
  ?1049 never restore a pending wrap (`input_restore_state`,
  `screen_alternate_off`: `screen_write_cursormove` puts a column past
  the last on it). REP repeats at most to the end of the line
  (`INPUT_CSI_REP`). vtmux's scroll-on-clear ED (`term.rs`
  `clear_into_history`) prints a space in the last column before its
  ED 2 to keep a pending wrap its CUP dropped. Upstream tests that
  assert xterm's behaviour now assert tmux's (cursorLeft, deleteChars,
  eraseChars, eraseLine left, insertBlanks outside the margins,
  linefeed, printRepeat, CBT, saveCursor, the resize of a saved
  pending cursor; the tests that saved a pending wrap to restore it set
  it after the restore); the formatter test "Terminal vt cursor
  preserves pending wrap over a blank cell" is skipped: the VT
  formatter rebuilds that state with DECSC, ECH and DECRC, which no
  longer gives it, and vtmux does not use the formatter. The oracle's
  observer is a `PaneTerm`, so it reads the clients' output with these
  semantics too, where a real terminal has xterm's; no screen showed it:
  all 265 earlier traces, recorded with the old observer, replay
  unchanged. New test `Terminal: tmux cursor past the last column
  (vtmux)`; oracle `pending_wrap_edges`. Not an upstream candidate.

  With 0013 and 0014 the full `zig build test-lib-vt -Demit-lib-vt=true
  -Dapp-runtime=none` gives 3256 pass, 38 skip (the formatter test
  above is the new one) and the two known failures, `tabClear single`
  and the kitty temporary-file test of 0001 (whose leak is the one
  reported).

- `patches/0015-reflow-tab-as-tmux.patch` (`PageList.zig`
  `ReflowCursor.reflowRow`, new field `tab_extra`, `bulkCopyable`,
  `BulkRunMask`, `moveLastRowToNewPage`, `cursorScroll`): a reflow
  measures a tab as tmux's does. tmux keeps a tab as one TAB cell as wide
  as the tab plus a padding cell per further column, and `grid_reflow`,
  `grid_reflow_split` and `grid_reflow_join` add up `gc.data.width`, where
  a padding cell reads back as width 1 (`grid_get_cell1`): a tab of width
  w costs 2w - 1 columns. The reflow cursor keeps that excess for the row
  it is filling (`tab_extra`) and starts the next row before a cell that
  does not fit by it, so a tab that does not fit moves whole and one that
  fits by its cells but not by tmux's measure splits after its first
  columns, the rest of its cells going to the next row as tmux's padding
  does (`ab<tab>c` on 20 columns narrowed to 12: `ab<tab>` and `c` in
  column 1). Upstream cut a tab cell by cell at the row's end. A line that
  fits by its cells but not by tmux's measure takes only its used cells,
  as a split does. A wrapped row with a used count (patch 0013) shorter
  than the width, which only such a wrap leaves, is measured by it when it
  joins back. Tab cells leave the bulk copy, which no longer runs past
  `tab_extra`. Wide characters keep upstream's reflow (tmux also counts
  their padding cell). Test: `Terminal: reflow measures a tab as tmux does
  (vtmux)`; `zig build test-lib-vt -Dtest-filter=PageList` (348 pass).
  Not an upstream candidate (tmux's model).

- `patches/0016-tmux-decawm.patch` (`Terminal.zig` `print`, `resize`):
  DECAWM as tmux's `screen_write_cell` has it. A print that reaches the
  last column sets the pending wrap only under autowrap (with it reset,
  tmux's `cx` stays on the column, `not_wrap`), so a pending wrap is
  always tmux's `cx` past the last column (patch 0014); a character
  printed with the wrap pending and autowrap reset since does not fit and
  is dropped ("Sanity check cursor position"), where upstream replaced
  the last cell as xterm does. The main screen reflows on a resize
  whatever DECAWM says (window.c `window_pane_resize`); upstream reflowed
  only under wraparound. vtmux's `tmux_cursor` reports the column count
  whenever the wrap is pending, and the scroll-on-clear ED sets autowrap
  around the space that keeps one (closes D-030). Upstream tests: "resize
  with wraparound off" now expects the reflow, "resize less cols without
  reflow cutting wide char tail" resizes the alternate screen. New test
  `Terminal: DECAWM as tmux (vtmux)`. Not an upstream candidate.

- `patches/0017-vtmux-state-export.patch` (new `src/terminal/c/vtmux_state.zig`
  and `include/ghostty/vt/vtmux_state.h`; exports in `c/main.zig`,
  `lib_vt.zig`, `vt.h`): export and import of a terminal's whole state for
  live upgrade (vtmux `docs/specs/2026-11-06-live-upgrade.md`, decisions 5,
  11, 23, 24). Functions only: no struct, page or row layout changes.
  - `ghostty_vtmux_page_count` / `ghostty_vtmux_page_export`: every page of
    a screen (history and active area, oldest first) as rows of cells in the
    header's own records (`GhosttyVtmuxRow`: wrap, continuation, semantic
    prompt, `tmux_used`; `GhosttyVtmuxCell`: codepoint or background,
    width, protected, the palette-256 bit, semantic content, the tab mark,
    style and link indexes), with the page's style table (`GhosttyStyle`,
    palette-256 flags included), hyperlinks (URI, explicit or implicit id),
    grapheme codepoints and capacity. Filled through the page's accessors,
    so the records survive a pin bump. A compressed page is decoded into
    scratch memory (`compression.Page.cloneBuf`) and stays compressed, or,
    with `blob_if_compressed`, comes out as a blob: a small header, the
    native `Page` value and the LZ4 bytes, readable only by the same build
    (decision 23; vtmux checks `ffi::PATCH_STACK`, the import checks the
    magic, `@sizeOf(Page)` and a Wyhash checksum over the `Page` value
    and the LZ4 bytes before it reads either (Wyhash, not Zig 0.17's
    `XxHash3`, which hashed about 250 MB/s against Wyhash's 10+ GB/s and
    so was most of a 10 x 100k-line upgrade: `lu_upgrade_perf`), then that the `Page` value
    is the one `Page.initBuf` gives for its capacity but for the fields
    use changes (size, dirty, allocator search hints, set counters, each
    bounded), and that its width is the terminal's).
  - `ghostty_vtmux_state_export`: the state outside the grids
    (`GhosttyVtmuxState`): every mode by number with its current, saved
    and default value, tab stops, scroll region, status display, REP's
    previous character, cursor defaults, mouse/keyboard flags, palette
    overrides and OSC 10/11/12 colours; per screen the cursor (position,
    pending wrap, visual style, pen, protected, open hyperlink, implicit
    hyperlink counter), saved cursor, charsets, protected mode, Kitty
    keyboard stack and semantic-click state. Title and pwd use the
    existing options.
  - `ghostty_vtmux_importer_new(terminal, check, out)` / `_page` /
    `_finish` / `_free`: build
    each screen's pages with `PageList.Builder` (rows into fresh pages via
    `styles.add`, `insertHyperlink`, `setHyperlink`, `appendGrapheme`;
    blobs as compressed nodes over fresh backing memory, as
    `cloneCompressedNode` does), then swap them into a fresh terminal's
    screens (refused while it has tracked grid refs) and rebuild the
    cursor as `snapshot/screen.zig` `decode` does. A page's capacity is the
    exporter's when given (it held that content); when the content does
    not fit it (another build's capacities) the half-built page is taken
    off the builder again and refilled at what `Page.exactRowCapacity`
    would count. Every count from outside is checked arithmetic: a page of
    another width than the terminal's, a capacity whose layout passes
    `size.max_page_size`, a grapheme on a cell without a codepoint, a
    cursor off the screen, a scroll region out of order or a previous
    character that is not a scalar value is `GHOSTTY_INVALID_VALUE`, the
    importer or terminal as it was. With `check` (vtmux's dry run before
    an upgrade's exec), every blob is decoded into scratch memory and
    every page passes `Page.verifyIntegrity` as it is added; without, a
    blob is decoded on its first read. vtmux-vt validates the whole state
    before any of this (`term/state/validate.rs`); these checks are the
    second line. The importer leaves to that validation the bounds
    Ghostty's parser and pages keep for live input, past which Ghostty
    crashes or writes out of bounds instead of failing: a hyperlink's
    URI and explicit id are not empty and fit one OSC 8 (`;URI` or
    `id=ID;URI` in 2047 bytes, `osc.zig` `Parser.MAX_BUF` less the NUL
    the parser adds; a longer string is a `string_alloc` span over 64
    chunks, and `bitmap_allocator.zig` `findFreeChunks` then indexes a
    bitmap past the end when the free span reaches it, and a URI and id
    each within 64 chunks but more together pass the reflow's
    `hyperlinkStringsFit` one at a time and then fail their `dupe`,
    `unreachable` in safe builds: upstream bugs live OSC 8 never
    reaches), at
    most 64 codepoints after a cell's first (`page.zig`
    `grapheme_max_len`), at most 53247 styles a page (a 16-bit style
    capacity's `RefCountedSet`), no style or link twice in a page's table
    and at most 65519 cells of room a page (each reference is counted in a
    16-bit `CellCountInt`), no open link on the inactive screen's
    cursor (`Terminal.switchScreen` asserts it), background-only cells
    (`bg_color_palette`, `bg_color_rgb`) as `Style.bgCell` makes them:
    narrow, unprotected, output, with no style and no link (a style
    reached `unreachable` in a capture's `grid_ref_style`, a link a null
    unwrap in `Page.clonePartialRowFrom` on a resize), a cursor whose
    `semantic_content_clear_eol` is set only with input content (a
    linefeed asserts it, `Terminal.index`), and capacity hints within a
    fresh page's or four times what the page holds (grapheme and string
    bytes, styles: the page is allocated, and zeroed in safe builds, at
    the hint, so a one-cell page asking for 1 GiB of each took 3.4 GB;
    the export writes hints clamped to the same bound).
    Blobs are Ghostty's own page memory and are trusted for where they
    come from, not checked as rows are: only an upgrade's memory file,
    the same user's, carries them (never a checkpoint on disk), and only a
    build of the same `PATCH_STACK` reads them. The checksum finds a
    damaged blob, not a forged one, and `check` is no defence against a
    forged one either: `Page.verifyIntegrity` reads the page through its
    own offsets and asserts, so a forged page with its checksum computed
    again can crash it ("unreachable", an index past a slice). For a
    blob whose checksum holds, `check` finds LZ4 bytes that do not decode
    and a page the writer got wrong.
  - `ghostty_vtmux_set_active_screen`: switch screens with no mode side
    effects (vtmux pins OSC 133 marks on the primary before switching).
  Ghostty's own snapshot codec (`src/terminal/snapshot`) is not used: its
  format is still breaking (v1 "work-in-progress"), it drops vtmux's patch
  bits (`tmux_used`, the palette-256 flags, tab marks), and it cannot carry
  compressed pages compressed. Wrapper: `libghostty-vt/src/vtmux.rs`
  (`Terminal::vtmux_page_export`, `vtmux_state_export`, `vtmux_importer`,
  `Importer`, `vtmux_set_title`, `vtmux_set_pwd`, `vtmux_set_active_screen`;
  `Importer` does not borrow the terminal, so a streamed import keeps both
  side by side: each call takes the terminal and refuses another one),
  `Vtmux*` structs and functions added by hand to `bindings.rs` with size
  asserts that `vtmux_state.zig` asserts too. `build.rs` `emit_patch_stack`
  sets `LIBGHOSTTY_VT_PATCH_STACK` (FNV-1a of the commit, every patch,
  `zig version`, the Zig optimize mode and the target: `Page` is not an
  `extern` struct, so another compiler may order its fields differently
  at the same size), read as `ffi::PATCH_STACK`.
  vtmux-vt's `PaneTerm::export_state` / `import_state` (`term/state.rs`)
  are the one consumer. Tests: `vtmux state: rows round trip into a
  fresh terminal`, `vtmux state: malformed pages and states are refused`
  and `vtmux state: blobs are checked before they are trusted` (a resealed
  blob with a moved offset, another width, or LZ4 bytes cut short, which
  only a `check` import decodes and refuses)
  (`zig build test-lib-vt -Dtest-filter="vtmux state"`);
  the full `zig build test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none`
  (Debug) over 0001-0017 gives 3258 pass, 38 skip and two known failures
  per test binary: `tabClear single`, and the kitty `temporary file
  outside directory prefix is rejected` of patch 0001's note (it fails
  whenever the checkout is under `/tmp`, as this run's was). Written as
  0015 before main's 0015 and 0016; renumbered on the rebase, no hunk
  changed;
  vtmux's `crates/vtmux-vt/tests/state_roundtrip.rs`. Not an upstream
  candidate as is (it carries vtmux's patch bits).

- `patches/0018-reflow-hyperlink-map-full.patch` (`PageList.zig`
  `ReflowCursor.writeCell`): the reflow grows a destination page's
  hyperlink map until a cell's link fits, not once. Near
  `hyperlink_bytes`' 16-bit maximum a doubling (capped at the maximum)
  can leave the map's power-of-two capacity as it was, and the
  `setHyperlink` after it failed: `unreachable` in safe builds, the
  link dropped in others. Past the maximum the growth is `OutOfSpace`,
  on which `reflowRow` already moves the row to a new page. Live input
  reaches it: 3000 lines of 30 linked characters on 80 columns, then 17
  columns (the narrower rows pack more linked cells into a page than the
  source pages held), found by vtmux's import fuzz. Test: `Terminal:
  reflow keeps the links of a page full of them (vtmux)` (fails before:
  `unreachable` at the `setHyperlink`); vtmux's
  `a_reflow_keeps_the_links_of_pages_full_of_them` (ReleaseFast: 3 of
  90000 links dropped before). The full `zig build test-lib-vt
  -Demit-lib-vt=true -Dapp-runtime=none` (Debug) over 0001-0018: 7009 of
  7089 pass, 76 skip, and the two known failures in each test binary
  (`tabClear single`, and the kitty temporary-file test of patch 0001's
  note, the checkout under `/tmp`). Upstream candidate.

- `patches/0019-reflow-retry-rewrites-the-whole-cell.patch` (`PageList.zig`
  `ReflowCursor.reflowRow`): `writeCell` stores a cell's graphemes, then
  its hyperlink, then its style. When the hyperlink (or style) is
  OutOfSpace, `reflowRow` moves the row to a new page and writes the cell
  again, but the moved row carried the graphemes already stored and the
  retry stored them a second time: `setGraphemes`' `putNoClobber`
  assertion in safe builds, a duplicate grapheme-map entry and a leaked
  slice in fast ones (nothing a release build shows). The retry now clears
  the half-written cell on the new page first. With 0018 the reflow
  reaches this retry far more often: a page whose map is full now moves
  its row instead of dropping the link. Narrowing gathers the short lines
  of several source pages into one destination page, so live output dense
  with OSC 8 links fills its hyperlink map or set. Found by worker-25's
  import fuzz (2026-11-06) on live output: an 80x24 pane, 4000 lines of
  per-cell `id=` links and graphemes, resized to 17 columns. Test:
  `PageList resize reflow retries a cell with graphemes and a hyperlink on
  a new page (vtmux)` (aborts in Debug and ReleaseSafe without the patch;
  under 1 s in Debug); vtmux-vt
  `reflow_keeps_every_link_past_a_full_hyperlink_map` (a dropped link in
  the default build without 0018, an abort with 0018 alone under
  `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe`). Not fixed: the hyperlink and
  style set adds in `writeCell` still grow once and assume room, which a
  page whose capacity is not one of the doubling steps could defeat (no
  vtmux path makes such a page that the fuzz found). Upstream: not fixed
  as of ghostty `e95bed633` (2026-11-04, the newest commit a local clone
  has; judged from the commit log only, as GitHub was unreachable), which
  sizes the hyperlink map apart from the set (`d20d208c5`) but keeps the
  retry. Upstream candidate, together with 0018; to be reported.
  worker-25's reproduction itself (80x24, 4000 lines of 20 grapheme
  cells each its own `id=` link, 1000 ids, resized to 17 columns) is
  vtmux-vt `reflow_to_17_columns_keeps_per_cell_links_and_graphemes`: an
  abort at `setGraphemes`' `putNoClobber` under ReleaseSafe without 0018
  and 0019, every cell and link kept with them. The full `zig build
  test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none` (Debug) over
  0001-0019: 7013 of 7091 pass, 76 skip, and `tabClear single` fails in
  each test binary (the kitty temporary-file test of patch 0001's note
  passes with the checkout outside `/tmp`).

- `patches/0020-reflow-wrapped-row-by-cells-used.patch` (`PageList.zig`
  `ReflowCursor.reflowRow`): a wrapped row reflows by its used cells
  (patch 0013's `tmux_used`, at least its text) whatever they are, as
  `grid_reflow_join` takes `cellused`. Patch 0015 measured a wrapped row
  by them only when the count was not 0, and kept every cell of one with
  none (for rows from Ghostty's snapshot codec, which vtmux does not use:
  patch 0017 carries the count). But a row wraps with none used when
  nothing was written to it: a wide character that does not fit an
  empty row (Ghostty's spacer head is the cell tmux leaves unwritten), and
  the empty last line a dead pane's linefeed marks wrapped (vtmux
  `remain.rs` `mark_wrapped` makes the mark that way). Such a row joined
  its full width of blanks to the line after it, so after a resize the
  dead pane's line started 20 columns in (100 to 80 columns); and an
  empty wrapped row was taken for a line break (`cols_len == 0` deferred
  a new row), where tmux's join takes nothing from it and goes on, so the
  pane kept one line more than tmux (`history_size` 8, tmux 7). Test:
  `Terminal: reflow joins a wrapped row by the cells it used (vtmux)`
  (two spaces before `DEAD` without the patch); vtmux oracle
  `remain_on_exit_reflow`. The full `zig build test-lib-vt
  -Demit-lib-vt=true -Dapp-runtime=none` (Debug) over 0001-0020: 3262 of
  3301 and 3753 of 3792 pass, 38 skip each, and the known `tabClear
  single` fails in each test binary. Not an upstream candidate (tmux's
  model).

- `patches/0021-rep-repeats-a-zero-width-character.patch` (`Terminal.zig`
  `print`): REP repeats the last character printed, a zero-width one too.
  tmux keeps every character it prints as `ictx->last` (input.c
  `input_print`, `input_top_bit_set`), and `INPUT_CSI_REP` writes it again:
  `e`, U+0301, `CSI 2 b`, `|` is one cell with three accents and `|` in
  column 1. Upstream set `previous_char` only for a character that takes a
  cell, so it repeated `e`. `print` now sets it before anything else. The
  Ghostty snapshot fixture `kitty-virtual-v1.hex` records the previous
  character, now the last combining mark (`snapshot/testdata`, regenerated
  from its test's candidate; vtmux does not use that codec). Test:
  `Terminal: printRepeat repeats a zero-width character (vtmux)` (fails
  without the change). Not an upstream candidate as is (xterm repeats the
  last graphic character).

- `patches/0022-deccolm-never-resizes.patch` (`stream_terminal.zig`): DECCOLM
  (`?3h`/`?3l`) never resizes the grid. With DEC mode 40 set first, upstream
  resized it to 132 or 80 columns behind the pane's back and cleared it
  without history; tmux has no 132-column mode and ignores mode 40
  (input.c `input_csi_dispatch_sm_private`, `_rm_private` home the cursor
  and clear the screen, which vtmux's `PaneTerm::deccolm` does, into history
  under `scroll-on-clear`). Test: `DECCOLM never resizes, with DEC mode 40
  or without (vtmux)` (fails without the change). Not an upstream
  candidate.

- `patches/0023-reflow-cursor-at-line-end.patch` (`Screen.zig` `resize`): a
  cursor at or past the end of the cells its line used goes to the end of the
  reflowed line, as tmux's does (`grid_wrap_position` gives it `UINT_MAX`,
  `grid_unwrap_position` the `cellused` of the line's last row): when that
  row is full the cursor is past its last column, a pending wrap, where
  upstream started a row of its own with it (18 characters on 20 columns
  narrowed to 9: tmux and now vtmux `9,0` with `history_size` 1, vtmux
  before `0,2` with 0). Before the reflow the cursor goes on the line's last
  used cell with a wrap pending; the reflow moves it with that cell and
  `resize` already drops the pending wrap, moving the cursor one on, unless
  the cell ends a row. Main screen only (it reflows). vtmux's `PaneTerm::resize`
  no longer skips its blank-row fix-up for such a cursor and moves it only by
  VPA, which keeps a pending wrap (patch 0014). Test: `Terminal: reflow leaves
  a cursor at the line's end past it (vtmux)` (fails without the change).
  Not an upstream candidate.

- `patches/0024-reflow-wide-character-as-tmux.patch` (`PageList.zig`
  `ReflowCursor.reflowRow`, `tmuxWidePadding`, `tmuxReflowWidth`,
  `bulkRunLength`, `bulkCopyable`, `cursorNewPage`, new field
  `src_in_tab`): a reflow measures a wide character as tmux's does: its cell
  of width 2 and one padding cell, which reads back as width 1
  (`grid_get_cell1`), 3 columns in all (`grid_reflow`, `grid_reflow_split`,
  `grid_reflow_join`), so `一abcdefghijklmnop` on 20 columns narrowed to 9
  is `一abcdef` / `ghijklmno` / `p`. The wide cell adds 1 to `tab_extra`
  (patch 0015). tmux can split the two: a padding cell that does not fit
  goes to the next row, which then starts with it (drawn blank,
  `tty_draw_line`; skipped by `capture-pane`, `grid_string_cells`), even
  when the line ends there. Ghostty keeps the spacer tail with its wide
  cell, so `tmuxWidePadding` starts that row with a tab continuation cell
  that no tab precedes (the reflow tracks `src_in_tab` to tell them apart):
  vtmux-vt reads it as padding already (skipped by a capture, blank on
  screen). A reflow drops such a cell and makes it again where the padding
  falls; pins on it go to the cell before it, pins on a spacer tail whose
  padding moves go with the padding (a cursor after `abcdefg一` narrowed to
  9 goes to 1,1 as in tmux). Wide pairs leave the bulk copy (it copied
  pairs as two columns), and so do tab cells erased with a background
  (`bulkCopyable` let a tab over erased cells through, so patch 0015
  measured it as one column: `一`, EL in green, two tabs, 40 -> 57 columns).
  The cells past the used ones of a line that fits (the background it was
  erased with) are copied up to the width without being measured, as
  `grid_reflow_move` keeps them. Seven upstream wide-character reflow tests
  ("less cols to wrap a wide char", the five "wide char bulk run" tests,
  "to wrap a multi-codepoint grapheme with a spacer head") now expect
  tmux's rows. New test `Terminal: reflow measures a wide character as tmux
  does (vtmux)` (fails without the change). Cost: 20k lines of CJK at
  120x40, five resizes (release, in a server): 0.05 s before, 0.11 s after
  (tmux 0.23 s). Not an upstream candidate (tmux's model).

  With 0021-0024 the full `zig build test-lib-vt -Demit-lib-vt=true
  -Dapp-runtime=none` (Debug, 40 min on a loaded host, run on a draft of
  0024 without its last three changes: the background tab, the cells past
  the used ones, the pins) gives 7017 of 7099 pass, 76 skip, and two
  failures in each test binary: the known `tabClear single`, and the kitty
  temporary-file test of patch 0001's note (the checkout was under `/tmp`);
  "golden snapshot with a virtual placement" failed on that run, before the
  fixture above was regenerated, and passes with it (`-Dtest-filter="golden
  snapshot"`, 89/89). On the final patches, `-Doptimize=ReleaseSafe` with
  `-Dtest-filter=PageList` 699/699, `reflow` 303/303, `vtmux` 130/130.

- `patches/0025-tmux-hscrolled.patch` (`PageList.zig` new field
  `tmux_hscrolled` and `TmuxHscrolled`, `resizeWithoutReflow`,
  `resizeCols`, `ReflowCursor.reflowRow`; `c/vtmux_state.zig`
  `set_hscrolled`, `take_hscrolled`; `c/main.zig`, `lib_vt.zig`,
  `include/ghostty/vt/vtmux_state.h`): tmux's `hscrolled` (grid.c `struct
  grid`) through a resize of the primary screen. tmux pulls back into a
  taller screen only the history lines it counts as scrolled, at most
  `hscrolled` of them, wherever the cursor is, and adds the rest blank at
  the bottom; a shorter screen adds the lines it pushes into history
  (screen.c `screen_resize_y`). Upstream pulled every history row it had
  when the cursor was on the last row and none otherwise (vtmux moved the
  cursor there to get tmux's pull, `grow_pulling_history`, now gone).
  `grid_reflow` then moves `hscrolled` as if it were the index of a line
  from the top: `grid_reflow_split` adds a split line's extra lines when
  the line's index is not past it, `grid_reflow_join` takes away each
  line it consumes before it or stops at the line they join. The reflow
  cursor reproduces that: a source row that starts a line (a line's first
  row, a row after a full one, the empty line under a wrapped one, or the
  rest of a row a join took part of) is tmux's split or move, with as many
  lines as destination rows its cells start, and a row joined whole is a
  consumed line. `origin`, the rows above tmux's oldest history line that
  Ghostty still keeps (it prunes whole pages), are left out of both
  indexes. So a narrowing under a short first line keeps it, a long first
  line runs it on through the split lines after it, and rows a clear or a
  narrowing put in history stay there when the screen grows: oracle
  `reflow_wide_padding_history` (scenes `pshort`, `plong`, `clear`,
  `emoji`, `deccolm`). vtmux sets it before each primary resize
  (`ghostty_vtmux_set_hscrolled`) and reads it after Ghostty's resizes,
  before its own fix-ups (`ghostty_vtmux_take_hscrolled`); vtmux-vt keeps
  the count between resizes (`PaneTerm::hscrolled`: scrolls add,
  `grid_view_clear_history` and `grid_clear_history` reset, at most
  `hsize`), and the live-upgrade state and snapshot carry it
  (`HistoryState` tag 11). Wrapper: `Terminal::vtmux_set_hscrolled`,
  `vtmux_take_hscrolled` (`vtmux.rs`, `bindings.rs` by hand). Test:
  `Terminal: tmux hscrolled through resizes (vtmux)`. With it,
  `-Doptimize=ReleaseSafe` `-Dtest-filter=reflow` 152/152 (153 in the C
  test binary), `PageList` 349/349, `resize` 227/227, `Screen` 276/276,
  `vtmux` 65/65; Debug `-Dtest-filter="Terminal: "` 521/522 with the known
  `tabClear single` failure. Not an upstream candidate (tmux's model).

- `patches/0026-region-scroll-into-history.patch` (`Terminal.zig` `index`,
  `scrollUp`, new `tmuxRegionScrollsIntoHistory`,
  `tmuxScrollRegionUpIntoHistory`, `tmuxRotationPin`, `tmuxMoveRow`): a
  full-width scroll region that does not start at the top feeds history on
  the primary screen, as tmux's `grid_view_scroll_region_up` (grid-view.c)
  does for any region of a grid with history with
  `grid_scroll_history_region` (grid.c): the region's top row becomes the
  newest history row with its wrap flag, the rows above the region stay, the
  rest of the region moves up and its bottom row is cleared with the cursor's
  background. LF/IND at the region's bottom and SU (once a line, at most the
  region's height, `screen_write_scrollup`) take it. Upstream made history
  only from a region at the top and moved a lower one in place, so the line
  was lost (vtmux D-032). It is the top-region scroll (`cursorScrollAbove`:
  row 0 into history, the rows down to the region's bottom up a row), then a
  rotation of the newest history row and the rows above the region down by
  one: `rotateOnceR` when they are in one page, else row by row upwards with
  the blank cursor row holding the moving row (a swap of `Row`s in one page,
  `clonePartialRowGrowCapacity` across pages, which replaces only the
  destination's page, so the pins above it stay valid). Ghostty's
  history limits are lifted for the step and enforced after it: a page the
  scroll pruned (a byte budget, or `history-limit 0`'s zero bytes) would
  otherwise take the old row 0 with it. Continuation flags (patch 0011)
  follow on the history row, active row 0 (under the region's old top row,
  as tmux's history line stays wrapped above it), the region's new top row
  and the row below the region. The alternate screen, left/right margins
  and a screen with Kitty image placements keep upstream's in-place path
  (vtmux stores no images). The upstream test "eraseDisplay complete
  ignores stale prompt on recycled row" now expects the region's prompt row
  in history and checks the clear adds none. New test `Terminal: a region
  below the top scrolls into history as tmux's does (vtmux)` (the D-032
  evidence, SU over the region's height, the wrap flag, the background, and
  a styled rotation across a page boundary at every offset); vtmux goldens
  `cap_region_scroll_history`, `fmt_region_scroll_history_size`, regress
  `input-raw-scroll.sh`, oracle `region_scroll_history`. With it,
  `-Doptimize=ReleaseSafe` with the filters `PageList`, `Screen`, `vtmux`,
  `reflow`, `scroll`, `index`, `stream`, `eraseDisplay`, `prompt`,
  `insertLines`, `deleteLines`: 1132/1132 and 1160/1160; Debug
  `-Dtest-filter="Terminal: "` 522/523 and 523/524, the known `tabClear
  single` failure; Debug `-Dtest-filter=kitty`: the known kitty
  temporary-file failure only (the tree under `/tmp`). Cost (release,
  in process, 200k lines through an 80x24 `PaneTerm`, history 2000):
  region from the top 0.025 s either way; a region from row 1 0.026 s
  before (no history) and 0.036 s after; from row 20 0.026 s and 0.042 s.
  Not an upstream candidate (tmux's model).

- `patches/0027-tmux-clusters.patch` (`unicode/main.zig` `tmux_clusters`,
  `tmux_vs16_wide`, `setTmuxClusters`, `tmuxJamo`, `tmuxShouldCombine`,
  `tmuxMayJoin`, `tmuxTextPresentation`; `Terminal.zig` `tmuxCombine`, `print`, `printSliceFast`,
  `printSliceFill`; `c/unicode.zig`, `c/main.zig`, `lib_vt.zig`,
  `include/ghostty/vt/unicode.h`): with a process-wide switch
  (`ghostty_unicode_set_tmux_clusters(enabled, vs16_wide)`, off by default,
  like patch 0007's overrides), mode 2027 joins characters into a cell as
  tmux's `screen_write_combine` (screen-write.c) does instead of by UAX #29.
  U+200D, U+FE0F and zero-width characters join the cell before, or are
  dropped where there is none; U+FE0F makes a one-column cell two columns
  while `vs16_wide` (tmux's `variation-selector-always-wide`), after any
  character, not only an emoji variation base (upstream dropped it there),
  and U+FE0E changes no width (upstream narrowed an emoji); any character
  joins a cell that ends with a ZWJ (0xA0-0xFF too: the slice fast path
  leaves a Latin-1 character after a cell with graphemes to `print`); two
  lone regional indicators, and an emoji skin-tone modifier with one of
  tmux's 72 bases in either order, join into two columns
  (`utf8_should_combine`, utf8-combined.c); a character joining after a
  ZWJ makes one of the nine text-presentation emoji tmux's table makes two
  columns (U+261D, U+26F9, U+270C, U+270D, U+1F3CB, U+1F3CC, U+1F574,
  U+1F575, U+1F590), one column alone in vtmux's grid (D-045), two
  columns, as tmux's cell is and Ghostty draws the sequence; Hangul jamo join as
  `hanguljamo_check_state` says (a medial or final jamo that cannot join is
  dropped); U+3164 is dropped wherever it comes; a cell stops joining at 32
  bytes of UTF-8 (`UTF8_SIZE`). Everything else, a spacing mark or a prepend
  character included, takes a cell of its own. A cell on the last column
  that U+FE0F would widen stays one column with the wrap pending: tmux puts
  its padding past the edge, where Ghostty has no cell, and the next
  character wraps in both. The slice fast path batches a character only
  when `tmuxMayJoin` says tmux could not join it to the one before. vtmux
  turns it on in the server (`vtmux_vt::set_tmux_clusters`), with tmux's
  own width table (`utf8_default_width_cache`, less those nine) as
  overrides under `codepoint-widths` (D-031); the oracle's observer stays on Ghostty's
  rules. Wrapper: `unicode::set_tmux_clusters` (`libghostty-vt/src/unicode.rs`,
  `bindings.rs` by hand). Tests: `Terminal: tmux clusters (vtmux)` (each case
  through `print` and `printSlice`) and `tmux clustering rules (vtmux)`;
  `zig build test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none` (Debug)
  `-Dtest-filter=vtmux` 67/67 and 71/71, `print` 109/109 and 110/110,
  `grapheme` 89/89 and 97/97 (the two test binaries); the switch is off
  in Ghostty's own tests, so their behaviour is upstream's. Not an upstream
  candidate (tmux's model).

- `patches/0028-tmux-alternate-screen.patch` (`Terminal.zig` new fields
  `tmux_alternate`, `tmux_alt_saved` and `TmuxAltSaved`,
  `tmuxSwitchScreenMode`, `tmuxRestoreAltCursor`, `resize`,
  `switchScreen`, `switchScreenMode`, `fullReset`; `c/vtmux_state.zig`
  `set_tmux_alternate`, `alternate_saved`, `set_alternate_saved`,
  `primary_size`, `importer_primary_size`, the importer's per-screen size;
  `c/main.zig`, `lib_vt.zig`, `include/ghostty/vt/vtmux_state.h`): with
  `tmux_alternate` set, modes 47, 1047 and 1049 switch screens as tmux's
  `screen_alternate_on` and `screen_alternate_off` do (screen.c; input.c
  `input_csi_dispatch_sm_private`, `_rm_private`), and the primary screen
  is not resized while the alternate one is up (window.c
  `window_pane_resize` resizes only the alternate grid). Every mode clears
  the alternate screen as it comes up (upstream's 47 kept what it had) and
  keeps the cursor; only 1049 saves the cursor and pen, in a terminal-wide
  slot apart from DECSC's (`tmux_alt_saved`, tmux's `saved_cx`, `saved_cy`,
  `saved_cell`), restored by `?1049l` even off the alternate screen and
  from an earlier one, never with the charsets or origin mode; `?1049h` on
  an alternate screen already up saves nothing. DECSC's slot is tmux's
  input context's, so both screens share it (copied at each switch), and
  `resize` leaves it as it is (input.c `input_save_state`; DECRC clamps
  it, `input_restore_state`): upstream moved it with its row as a tracked
  pin, to the top-left once the row left the screen, and the pin kept the
  blank rows under it from being trimmed, so a shorter alternate screen
  lost the rows its cursor was on.
  Upstream used DECSC's per-screen slot for 1049 and resized and reflowed
  the primary at every resize, with its own cursor. When the alternate
  screen ends at another size, the terminal takes the primary's size with
  the cursor tmux resizes it with (the saved one, or the alternate
  screen's back at that size: `min(y, rows - 1)`, a pending wrap as
  tmux's `cx` past the last column) and drops the alternate screen; the
  caller resizes back (vtmux: `PaneTerm::alternate_ended`, through its
  usual primary resize, so patch 0025 moves `hscrolled` and the rows under
  the cursor go first). `switchScreen` and `fullReset` resize a primary
  left at another size before showing it. The state export and import
  carry the primary's size (`primary_size`, `importer_primary_size`: its
  pages and cursor at that size, only with an alternate screen) and the
  1049 slot (`alternate_saved`, `set_alternate_saved`) outside
  `GhosttyVtmuxState`, so patch 0017's layout is unchanged. Wrapper:
  `Terminal::vtmux_set_tmux_alternate`, `vtmux_alternate_saved`,
  `vtmux_set_alternate_saved`, `vtmux_primary_size`,
  `Importer::primary_size` (`vtmux.rs`; `VtmuxAltSaved` and the
  functions in `bindings.rs` by hand). Tests: `Terminal: tmux alternate
  screen leaves the primary at its size (vtmux)`, `Terminal: tmux
  alternate screen cursor slots (vtmux)`, `Terminal: tmux saved cursor is
  not moved by a resize (vtmux)`. With it, `-Doptimize=ReleaseSafe`
  `-Dtest-filter=vtmux` 68/68 (72 in the C test binary), `resize`,
  `PageList`, `reflow` and `Screen` pass; Debug `-Dtest-filter="Terminal: "`
  1049/1051 with the known `tabClear single` failure in each test binary.
  Oracle `alt_screen_exit`, `alt_screen_cleared_narrowed`,
  `alt_screen_cursor_slots`, `alt_screen_history_clears`,
  `alt_screen_saved_cursor_resize`. Not an upstream
  candidate (tmux's model).

- `patches/0029-tmux-grid-corners.patch` (`PageList.zig`
  `ReflowCursor.reflowRow`, `tmuxEmptyRows`, new fields `tmux_empties`,
  `tmux_joined`, `tmux_padding_made`, `scrollClear`; `Terminal.zig`
  `eraseLine`, `tmuxLineUnwritten`, `index`, `scrollUp`, `scrollDown`,
  `tmuxUnwrapAboveRegionWithoutHistory`, `tmuxFixRegionContinuations`,
  `insertLinesTmux`, `insertLinesRowsTmux`, `tmuxOutsideRegion`,
  `deleteLinesTmux`; `stream.zig` `csiDispatch`): corners of tmux's grid
  bookkeeping (D-040 closed, D-006 narrowed).
  - Reflow of a cell wider than the pane (a tab, D-040). tmux's
    `grid_reflow_split` moves to the next line before every cell for
    which `width + gc.data.width > sx`, also with nothing on the line
    yet, so the first cell of a split (`at`) that is wider than the pane
    gets an empty row before it, and a line of that one cell (`at` 0)
    two: the line emptied and the one skipped. `grid_reflow_join` skips
    empty wrapped lines (no cells used) looking for the next cell; when
    that cell does not fit, even with the width at 0, and the join has
    consumed no line yet, it removes nothing and each empty line stays a
    row; once it has, the skipped lines go with it. A line tmux fills
    exactly (`width == sx`, tab_extra counted) joins nothing, so the row
    after it starts a line of its own (a wrap pending is set for it; a
    wide character's padding cell that tmux has at the start of that row
    is not counted), as does a row whose first cell is the padding a full
    row's wide character left on a row of its own (`tmuxWidePadding`).
    reflowRow follows which cells start a line tmux splits as its own (a
    row starting a line, or the rest of a row a join stopped in) and adds
    the empty rows (`tmuxEmptyRows`). Pins marking where an emptied line
    starts (vtmux's oldest history row, OSC 133 marks) stay on it (tmux
    keeps the original line, `first`); the cursor goes with the cell
    (`grid_unwrap_position` skips lines with no cells used). A pin on an
    empty wrapped row no longer makes it a blank cell (Ghostty raised the
    row's length to write it; joined, the line gained a blank).
  - EL 2 (and EL 0 from column 0, EL 1 from the last column) does nothing
    to a line never written to (`cellsize` 0: no cell used, every cell
    blank, no tab or background) in the default background
    (`screen_write_clearline`), so it unwraps nothing; such a wrapped
    line comes from the reflow above.
  - `scrollClear` (`CSI 22 J`, which vtmux sends only for scroll-on-clear)
    scrolls the rows up to the last with cells used (`tmuxCellsUsed`,
    erased ones included), as `grid_view_clear_history` takes
    `cellused`; Ghostty took the last row with a non-empty cell, so a
    clear after an erased row scrolled one row less, and a row only
    erased with a background one more.
  - Without history (`no_scrollback`: the alternate screen, and a pane
    made with history-limit 0, which vtmux gives no scrollback bytes) a
    region scrolled up goes through `grid_move_lines`
    (`grid_view_scroll_region_up` without `GRID_HISTORY`): a region of two
    rows unwraps the line moved to its top (`py >= dy + ny`), so a line a
    print wrapped onto the bottom row of a two-row pane loses its flag as
    it scrolls; a region of one row (a one-row pane) moves nothing, so LF
    and SU leave the row as it is. SD and RI at the top margin
    (`grid_view_scroll_region_down`) move nothing in a one-row pane,
    history or not. The continuation fix-up after a region scroll
    (`tmuxFixRegionContinuations`, patch 0011) now skips the alternate
    screen only, not every screen without scrollback: the primary screen
    of a pane without history reflows.
  - IL and DL with the cursor above or below the scroll region work on
    the whole screen from the cursor's row (`screen_write_insertline`,
    `screen_write_deleteline`: `grid_view_insert_lines`,
    `grid_view_delete_lines`), where Ghostty (as xterm) did nothing. DL is
    the region's DL with the region the whole screen; IL is
    `grid_move_lines` alone (no `grid_clear`, so not its unwrap), and a
    count reaching the bottom moves no line and changes nothing. The
    cursor stays, as in the region (patch 0011).
  - A zero count is 1 for IL, DL, DCH, SU, SD and CBT (`stream.zig`
    `csiDispatch`: `@max(1, params[0])`, as ICH had): tmux reads every
    count with `input_get(ictx, 0, 1, 1)` (input.c), where Ghostty passed
    the 0 on and `insertLinesTmux`, `deleteLinesTmux`, `deleteChars`,
    `scrollUp`, `scrollDown` and CBT's loop do nothing for it. A sweep of
    every count final (`@ A B C D E F G H I L M P S T X Z a b d e` and
    backquote, counts 0, 1, 2, 65535) against tmux 3.8 found only these
    six; the others already took 0 as 1. Not changed: extra parameters
    (`CSI 3;1 L`: tmux uses the first, Ghostty drops the sequence) and a
    colon in a later parameter.

  Tests: new `stream: a zero count is one for IL, DL, DCH, SU, SD and CBT
  (vtmux)` (fails without the change), `Terminal: reflow leaves empty rows before a tab wider than
  the pane (vtmux)` (fails without the PageList change: no empty rows)
  and `Terminal: tmux grid bookkeeping corners (vtmux)`; oracle
  `reflow_wide_tab`, `wrap_flag_corners`, `reflow_tmux_quirks` (scene
  `tabwide` no longer annotated), `csi_zero_count` (every count final with
  0, IL and DL in, above and below a region, ED 2 after a background). A random reflow probe against tmux 3.8
  (780 seeds of letters, tabs, wide characters, erases and CRLF on
  20x6, 2 to 7 resizes of 2 to 20 columns, some of the height) matches
  but for the 14 seeds of D-041. `-Doptimize=ReleaseSafe` with test filters
  reflow, PageList, Screen, resize, scroll, insertLines, deleteLines,
  eraseLine, eraseDisplay, index and vtmux: 1761/1761 over the two test
  binaries; Debug `-Dtest-filter="Terminal: "` 1047/1049 with the known
  `tabClear single` failure in each. Not an upstream candidate (tmux's
  model).

- `patches/0030-import-saved-cursor-as-saved.patch` (`c/vtmux_state.zig`
  `installScreen`, patch 0017's importer): the saved cursor (DECSC, SCP)
  is imported as exported, off the screen too. A screen shrunk below or
  narrowed past a saved cursor keeps it where it was, as tmux keeps
  `input_save_state`'s position unclamped and Ghostty's own resize
  leaves it (`Terminal.restoreCursor` clamps it when it is used); the
  importer clamped it to the screen, so after a live upgrade a screen
  grown back restored the cursor at the old bottom row instead.
  Found by `state_roundtrip`'s fuzz (seed 11) once `CSI 25;264 s` saved
  the cursor as tmux's SCP does, before a resize to two rows. Test: Rust
  `state_roundtrip::saved_cursor_off_a_shrunk_screen` (fails without the
  change: the re-exported state differs, y 1 for 4); no Zig test.
  Changes the patch stack, so an upgrade across it moves history as
  rows. Not an upstream candidate (the importer is vtmux's).

- `patches/0031-decaln-as-tmux.patch` (`Terminal.zig` `decaln`): DECALN
  fills the screen with default cells holding E, puts the cursor top
  left and resets the region, and leaves the pen and origin mode alone,
  as tmux's `screen_write_alignmenttest` (screen-write.c:
  `grid_default_cell`). Ghostty wrote the Es in the pen's colours, then
  kept only those colours in the pen (bold, blink and the rest were lost
  for what followed) and turned origin mode off. No D4 arbiter verdict:
  xterm's DECALN fills with its current video attributes, so neither
  Ghostty's nor tmux's, and esctest checks only the Es, the margins and
  the cursor; so tmux's. Test: new `Terminal: decaln writes default cells and keeps the
  pen and origin mode (vtmux)` (Ghostty's `decaln preserves color`
  still passes: the row SD inserts takes the pen's background); Rust
  `term::tests::decaln_writes_default_cells`; oracle `decaln_as_tmux`.
  `zig build test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none
  -Dtest-filter=decaln -Dtest-filter=DECALN` (Debug) passes. Found by
  `scripts/emu_fuzz.py`. Not an upstream candidate (tmux's model).

- `patches/0033-grid-ref-step.patch` (`c/grid_ref.zig`, `c/main.zig`,
  `lib_vt.zig`, `include/ghostty/vt/grid_ref.h`): `ghostty_grid_ref_down`
  and `ghostty_grid_ref_up` move a grid ref `n` rows (`Pin.down`,
  `Pin.up`: along the page list from the ref's own page), INVALID_VALUE
  past either end. `ghostty_terminal_grid_ref` resolves a screen point by
  walking the page list from the oldest page, so vtmux reading every row
  of a long history (a copy-mode search, `capture-pane -S -`,
  `#{history_bytes}`, the alternate screen copying the primary history)
  took time quadratic in its length: about 3 s for a search of 1M lines.
  Stepping from the row read before makes each row constant time.
  Read-only, no layout change. Wrapper: `GridRef::down`, `GridRef::up`
  (`libghostty-vt/src/screen.rs`), bindings added by hand to
  `bindings.rs`; vtmux uses them in `capture.rs` `row_ref_near`. Tests:
  new `grid_ref_down and grid_ref_up step as a lookup resolves (vtmux)`
  (3000 lines over several pages: every row stepped to one at a time
  down and up, and in strides, is the row a lookup gives; past either end
  is refused) and `grid_ref_down and grid_ref_up null node`; `zig build
  test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none
  -Dtest-filter=grid_ref` (Debug) passes. Upstream candidate: a C API
  user reading many rows has no other way to avoid the walk.

- `patches/0037-wide-wrap-keeps-last-cell.patch` (`Terminal.zig` `print`,
  `printSliceFast`): a wide character that does not fit the rest of the
  row wraps and leaves the row's last cell as it was, its text, style
  and link included, as tmux's `screen_write_cell` (screen-write.c: the
  wrap comes before any cell is written); only an empty cell becomes the
  spacer head, which is tmux's unwritten cell. Ghostty wrote a blank
  spacer head over the cell in the pen's style and link, so `printf
  '0123456789\033[10G\344\270\200'` on 10 columns lost the 9. The
  batched path keeps the cell too (its mask check already means the
  cell has the pen's style and no link or tab). Inside a right margin
  the cell is still blanked (vtmux never sets one: tmux has no DECSLRM).
  Two cells still become the default spacer head: in insert mode tmux
  makes room before it wraps (`grid_view_insert_cells` at the last
  column clears the cell to the default one, whatever the pen), so the
  cell is cleared, text, style and link; and a blank with only a
  background, which is mostly the fill of a line a wrap scrolled in
  (Ghostty fills it with the pen's background, tmux's wrap linefeed
  with the default, bg 8), so `capture-pane -e` ends the background
  before it as tmux does and the next line opens it again. A wide
  character's tail in the last column stays in insert mode too (tmux
  keeps the character and clears its padding cell, which only `-J`
  shows, as a space).
  esctest has no case for a wide character that does not fit, so no D4
  arbiter verdict: tmux's. The upstream test `print wide char at right
  edge with hyperlink` now expects the spacer head without the link.
  New test `Terminal: a wide char that wraps leaves the last cell as it
  was (vtmux)` (both paths, insert mode, a background blank); Rust
  `capture::tests::wide_wrap_keeps_last_cell`; oracle
  `wide_wrap_keeps_last_cell`, `wide_wrap_insert_mode`,
  `wide_wrap_scrolled_fill`. `zig build test-lib-vt
  -Demit-lib-vt=true -Dapp-runtime=none` (Debug): 7047/7125 pass, 76
  skip, the known `tabClear single` failure. Found by
  `scripts/emu_fuzz.py --gen round2` (`widemargin`). Not an upstream
  candidate (tmux's model).

- `patches/0034-tmux-line-flags.patch` (`page.zig` `Row.tmux_extended`,
  `Row.tmux_hyperlink`, `tmuxMarkExtended`, `tmuxEmptied`,
  `clonePartialRowFrom`, `moveCells`; `Terminal.zig` `printCell`,
  `writeCell`, `tmuxPenExtended`, `tmuxWrittenBlank`, `printSliceFill`,
  `markTab`, the grapheme attaches in `print`, `eraseChars`, `eraseLine`;
  `Screen.zig` `clearRows`, `clearCells`; `PageList.zig`
  `ReflowCursor.reflowRow`, `ReflowCursor.writeCell`, `tmuxCellExtended`;
  `c/row.zig` and `screen.h` `GHOSTTY_ROW_DATA_TMUX_LINE_FLAGS`;
  `c/vtmux_state.zig` and `vtmux_state.h` row flags `0x10`, `0x20`):
  tmux's `GRID_LINE_EXTENDED` and `GRID_LINE_HYPERLINK` per row (grid.c
  `grid_extended_cell`), in two of the Row's seven spare bits (five
  left), and the other half of a wide character a print overwrites
  (D-027 narrowed; regress `input-unicode.sh` and `input-raw-unicode.sh`
  pass).
  - tmux sets the line flags when a cell needs its extended form
    (`grid_need_extended_cell`: a wide or non-ASCII character, more than
    one codepoint, an RGB colour, an underline colour, an attribute past
    the first eight, a tab, a link; HYPERLINK for a link) and keeps them,
    and the cell's extended entry, when the cell is overwritten or
    erased, until the line is emptied (`grid_empty_line`). The bits are
    set by the writes that make such a cell: `printCell` for a character
    past ASCII before its charset mapping (line drawing maps ASCII and is
    not one), `writeCell` for a wide character or an extended pen
    (`tmuxPenExtended`; not on a spacer head, the cell tmux leaves
    unwritten), the `printSlice` fast path (wide runs, an extended pen,
    a run with a character past ASCII: the vectorised scan takes ASCII
    only, so an ASCII run costs no extra pass), grapheme attaches, a tab
    (`markTab`), an erase to an RGB background (`clearCells`, and
    `clearRows` after it resets the row: tmux clears to an RGB background
    with extended cells; `Screen.resetBlankRow` for the scroll paths that
    reset a row after clearing it, and `cursorScrollAbove`, whose new
    line is filled with the pen's blank cell by hand). They are reset where `tmux_used` is (patch
    0013's full-row clears, now `tmuxEmptied`) and with the row
    (`Row.reset`, `clearRows`). A reflow keeps them on the first row of a
    line it moves, splits or joins onto (`grid_reflow_move`,
    `grid_reflow_split`'s first line, `grid_reflow_join`'s target), and
    sets them on a row it writes an extended cell to (`grid_set_cell`;
    `tmuxCellExtended`, which takes a cell with vtmux's line-drawing mark,
    Ghostty's protected bit, as ASCII). Cells moved between rows
    (`moveCells`, a partial `clonePartialRowFrom`: left and right
    margins, which tmux has not got) bring their row's bits. So a row
    with an extended cell should always have the bit; vtmux's snapshot
    stores the bits a row's cells don't imply, and those a row lacks
    though its cells imply them (a path that misses the bit: `CSI S` in
    an RGB background with no history), beside the rows under their own
    tags (`vtmux_snapshot::KeptRows`, `Row::lacks`), which a build before
    them skips.
    Readers OR them with what the cells say now (vtmux `capture.rs`
    `LineKept`).
  - A narrow character over half of a wide one: tmux writes the other
    half as a default cell (`screen_write_overwrite`,
    `screen_write_clear_cell`), a space that counts as written, where
    Ghostty erased it to an empty cell (`capture-pane -R` showed it
    `CLEARED`). `tmuxWrittenBlank` makes the erased half a space when the
    erase left a default cell (one the cursor's background coloured stays
    a background cell; tmux keeps the old cell's background, which
    Ghostty does not).
  - `GHOSTTY_ROW_DATA_TMUX_LINE_FLAGS` (bit 0 EXTENDED, bit 1 HYPERLINK)
    reads them; the state export and import carry them as row flags
    `0x10` and `0x20` (`GHOSTTY_VTMUX_ROW_TMUX_EXTENDED`,
    `GHOSTTY_VTMUX_ROW_TMUX_HYPERLINK`); a page in Ghostty's own form
    carries the Row as it is. Wrapper: `screen.rs` `Row::tmux_line_flags`,
    `RowData::TMUX_LINE_FLAGS` added to `bindings.rs` by hand.

  Tests: new `Terminal: tmux line flags (vtmux)` (both print paths, a wide
  character overwritten from either half, a partial and a full erase, a
  non-ASCII fast-path run, a strikethrough pen, a link, ED) and
  `Terminal: tmux line flags follow a reflow (vtmux)`, and `vtmux state:
  rows round trip into a fresh terminal` checks the bits; vtmux's
  `capture.rs` `line_flags_outlive_the_cells_that_set_them`,
  `state_roundtrip.rs` `kept_line_flags_round_trip`, `snapshot_term.rs`
  `kept_line_flags_cross_a_snapshot` (and its fuzzer, which found the
  reflow and spacer-head cases), oracle `capture_line_flags`. New
  `Terminal: a row scrolled in under an RGB pen is extended (vtmux)`
  (fails without `resetBlankRow` and without the `cursorScrollAbove`
  mark: a new line under an RGB pen had no bit, which the snapshot, giving
  a row back the bits its cells say, then restored; the fuzzer found it
  once main's generator took `?47h` and `CSI s`), vtmux's
  `snapshot_term.rs` `rows_scrolled_in_under_an_rgb_background_round_trip`
  (fails without them) and `capture.rs`
  `a_line_scrolled_in_under_an_rgb_background_is_extended` (the reference's
  flags; it passes without them, as readers OR the cells).
  `Terminal: tmux line flags follow a reflow` is the one new test not
  run without the change (its join case is the reflow fix the fuzzer
  asked for). The upstream tests `Terminal: print over wide char at 0,0`,
  `Terminal: print over wide spacer tail` and `Terminal: overwrite
  multicodepoint grapheme clears grapheme data` now expect the written
  space (` `, `"X "`). `zig build test-lib-vt -Demit-lib-vt=true
  -Dapp-runtime=none` (Debug, 0001-0034): 7039/7125, 76 skipped, 10
  failed over the two test binaries: the known `tabClear single` and
  kitty `temporary file outside directory prefix is rejected` (patch
  0001's note) and those three upstream tests, before their expectations
  were changed; after, `-Dtest-filter="Terminal: "` 1061/1063 with the
  known `tabClear single` in each, and `-Dtest-filter=vtmux` 156/156.
  Not an upstream candidate (tmux's model).

- `patches/0035-clone-whole-page-memory.patch` (`PageList.zig` `clone`,
  new `cloneWholePage`): a resident page `PageList.clone` copies whole
  (every row of it) goes as a copy of its memory, as `Page.cloneBuf`
  does: the new page has the source's capacity, so its layout, and a
  page keeps everything as offsets into its memory. Upstream cloned it
  row by row (`Page.cloneFrom`), and a row with managed memory cell by
  cell, adding each styled cell's style to the new page's set
  (`clonePartialRowFrom`, `StyleSet.addWithId`): copy-mode entry
  (`ghostty_terminal_clone_screen`, patch 0005) over 1M styled lines took
  701-727 ms against 290-325 ms for plain ones (tmux 276-282). Now both
  270-303 ms. The new page comes zeroed but for its row headers
  (`createPageExt`, `initBuf`), and zero memory is an empty set or
  allocator (`initAssumeZeroed`): the row headers and cells are copied
  whole, the style set, the grapheme allocator and map, and the string
  allocator with the hyperlink set and map each only when it holds
  anything (else the copy keeps the fresh page's, empty). Memory a page
  never touched is not committed, and reading it to copy it faults in a
  zero page: a plain `cloneBuf` made copy mode's RSS over 1M lines 61 MB
  bigger (2022 against 1961 MB), and one that skipped zero memory pages
  made the first entry 20-30 ms slower. Now 1961 MB plain, 1971 MB
  styled (the style set's whole region is copied). A partial page (the
  clone's first or last, cut by `top`/`bot`) and a compressed page
  (patch 0005) go as before. Test: new `PageList clone copies whole pages
  with their styles and graphemes (vtmux)` (several pages of styled rows
  and graphemes: every cell, style and cluster reads back the same, each
  page is byte for byte its source's, which the row-by-row clone fails,
  and changing the source changes nothing in the copy); `zig build
  test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none -Dtest-filter="PageList
  clone"` (Debug) passes, and the whole `test-lib-vt` gives what it gives
  without the patch (7047/7127 pass, 76 skip; the same two fail,
  `Terminal: tabClear single` and `kitty.graphics_image` "temporary file
  outside directory prefix is rejected", and the same one leak, in both
  test binaries). Rust: `capture::tests::a_copy_of_whole_pages_reads_as_the_pane_did`
  (links included). Upstream candidate: any full-page clone gains.

- `patches/0038-read-the-primary-under-the-alternate-screen.patch`
  (`c/terminal.zig` new `vtmux_screen_grid_ref`,
  `vtmux_screen_scrollback_rows`, `vtmux_clone_screen_of`, `clone_screen`
  through the new `cloneScreenOf`; `c/main.zig`, `lib_vt.zig`,
  `include/ghostty/vt/terminal.h`): a grid ref to a point of either
  screen, the history rows of either screen, and `clone_screen` of either
  screen into a terminal of that screen's size, active or not (key 0
  primary, 1 alternate; NO_VALUE for an alternate screen not yet made).
  tmux goes on showing the primary history while an alternate screen is
  up (screen.c `screen_alternate_on` saves only the visible rows), and
  Ghostty reads only the active screen, so vtmux copied every history row
  out as `?1049h` came up: 4.1 s over 1M lines on a shared server, 1.8 s
  on a laptop (tmux 3 ms). With patch 0028 the primary screen is left
  as it was meanwhile, so vtmux now reads its history in place: a grid
  ref is a pin like any other, which the `grid_ref` calls read and step
  (patch 0033) whatever screen it is on. `?1049h` over 1M lines: 3 ms.
  Copy mode under the alternate screen clones the primary (patch 0035's
  whole-page clone) as it clones the active screen. Read-only, no layout
  change. Wrapper: `Terminal::vtmux_screen_grid_ref`,
  `vtmux_screen_scrollback_rows`, `vtmux_clone_screen_of`
  (`libghostty-vt/src/terminal.rs`), bindings added by hand to
  `bindings.rs`; vtmux uses them in `capture.rs` (`PRIMARY` rows) and
  `term.rs`. Test: new `vtmux_screen_grid_ref reads the primary history
  under the alternate screen (vtmux)` (2000 lines; after `?1049h` and a
  resize the primary keeps its rows and size, every row looked up or
  stepped to reads as written, the clone is the primary at its own size
  without the alternate screen's text, bad keys are refused); `zig build
  test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none
  -Dtest-filter=vtmux_screen_grid_ref -Dtest-filter=clone` (Debug) 167/167;
  the whole `test-lib-vt` 7056/7136 pass, 76 skip, with the two known
  failures (`Terminal: tabClear single`, `kitty.graphics_image`
  "temporary file outside directory prefix is rejected") and the one
  leak, in both test binaries. Not an upstream candidate (tmux's model).

- `patches/0039-reflow-bulk-copy-line-flags.patch` (`PageList.zig`
  `ReflowCursor.copyRun`, new `tmuxRunExtended`): a reflow's bulk copy
  of a run of narrow cells sharing a style sets `Row.tmux_extended` on the
  row it lands on when a cell of the run is one tmux holds extended, as
  `ReflowCursor.writeCell` does (patch 0034, `tmuxCellExtended`): tmux
  writes every cell its reflow moves with `grid_set_cell` (grid.c
  `grid_reflow_split`, `grid_reflow_join`). The run shares one style and
  has no wide, tab, link or grapheme cell, so the first cell's style and
  each cell's character and RGB background decide. Patch 0034 missed this
  path: a row a reflow filled with overlined text, or a character past
  ASCII amid plain ones, lacked the flag, and lost it for good once its
  cells were overwritten (tmux keeps it); a live upgrade, which gave the
  row the flags its cells said, did not. The fuzzer of vtmux's
  `snapshot_term.rs` found it (seed 141). Test: new `Terminal: tmux line
  flags follow a bulk-copied reflow (vtmux)` (an extended style, a
  character past ASCII, an RGB background cell, each on rows with no pin;
  fails without the change); `zig build test-lib-vt -Demit-lib-vt=true
  -Dapp-runtime=none` with `-Dtest-filter=vtmux` 233/233 and
  `-Dtest-filter=PageList` 701/701 (Debug). Oracle
  `reflow_line_flags_bulk`. Not an upstream candidate (tmux's model).
- `patches/0040-kitty-graphics-vtmux.patch` (`c/vtmux_state.zig`
  `kitty_storage`, `set_kitty_limit`; `c/kitty_graphics.zig`
  `vtmux_placement_active_pos`; exported as `ghostty_vtmux_*`, declared in
  `vtmux_state.h`, bindings by hand in `bindings.rs`, wrapped in
  `libghostty-vt/src/vtmux.rs` and `kitty/graphics.rs`): what D33 (kitty
  graphics) needs. Each screen's image storage, the bytes it holds and its
  limit, and setting one screen's limit alone (`ghostty_terminal_set`
  sets every screen's), so vtmux holds the images of all its panes to one
  cap and evicts through Ghostty's own `ImageStorage.setLimit`; and a
  direct placement's position relative to the active area's top, however
  far into history, where `viewport_pos` gives only placements on screen
  (vtmux draws history itself, in copy mode). Rust tests:
  `vtmux-vt` `graphics::tests` (storage, limits, placements) and
  `vtmux-server` `pane_graphics::tests` (the shared limit evicting across
  panes). Not an upstream candidate as is (vtmux's model), though a
  per-screen limit may be.
- `libghostty-vt/src/kitty/graphics.rs`:
  `set_kitty_image_from_temp_file_allowed(bool)` passed a `bool` where
  `GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE` reads a
  `GhosttyString` (the directory); replaced by `set_kitty_image_temp_dir(
  Option<&str>)`, which passes the string or NULL. Upstream candidate.

## Bumping ghostty

1. Pick a ghostty commit and set `GHOSTTY_COMMIT` in
   `libghostty-vt-sys/build.rs`, and `GHOSTTY_HASH` to what `zig fetch
   https://github.com/ghostty-org/ghostty/archive/<commit>.tar.gz` prints.
   Copy the commit's `LICENSE` to `libghostty-vt-sys/LICENSE-ghostty` if
   it changed.
2. Match Zig to the commit's `build.zig.zon` `minimum_zig_version` in
   `mise.toml`, `.github/workflows/ci.yml`, `ZIG_VERSION` in
   `libghostty-vt-sys/build.rs` and the README's install line.
3. Regenerate the bindings from the new headers, in
   `third_party/libghostty-rs/libghostty-vt-sys`:
   `GHOSTTY_INCLUDE_DIR=<ghostty-install>/include cargo run --features
   bindgen-tool --bin gen-bindings`, then `cargo fmt` there. It needs
   libclang (`LIBCLANG_PATH`; the `libclang` PyPI wheel works without root)
   and, on Fedora, `BINDGEN_EXTRA_CLANG_ARGS=-I/usr/lib/gcc/x86_64-redhat-linux/<N>/include`
   for `stddef.h`. Diff the old and new bindings for removed or changed
   functions, callback types and struct fields (a changed callback type
   still compiles), and fix the wrapper.
4. Re-apply or drop each `libghostty-vt-sys/patches/*.patch` (the build
   fails if one no longer applies).
5. Patch 0017 reads and writes Terminal, Screen and page fields by name:
   a renamed or new field fails its build, and a new mode or terminal
   flag that should survive a live upgrade needs adding to
   `vtmux_state.zig` (`crates/vtmux-vt/tests/state_roundtrip.rs` compares
   whole exported states, so a field it drops shows there only once a
   test sets it). The blob format changes with any `Page` change; that is
   what `PATCH_STACK` is for.
6. `vtmux_vt::compress_all_of` (`crates/vtmux-vt/src/term.rs`) runs
   `ghostty_terminal_compress(FULL)` on distinct terminals from several
   threads at once. Check that it still is safe: no `threadlocal` under
   `src/terminal`, no process-wide mutable state on the compression path
   (`PageList.compressPage`, `compressPageChunked`), its memory from the
   pools and the page allocator (`PageList.zig` `pageAllocator`), and no
   terminal callback run from it.
7. Re-check the constants in `crates/vtmux-vt/src/term.rs` (`PAGE_BYTES`,
   `PAGE_GRID_BYTES`, `CELLS_ALIGN`; `page_rows_matches_ghostty` fails if
   the page layout changed) and run the slow gate.
