# Vendored libghostty-rs

`libghostty-vt` and `libghostty-vt-sys` are the crates.io 0.2.2 sources of
[libghostty-rs](https://github.com/uzaaft/libghostty-rs) (git `c9876b29`),
MIT OR Apache-2.0 (`LICENSE`, from the same commit). `vtmux-vt` depends on
them by path. The workspace excludes `third_party/`, so vtmux's lints, `fmt`
and doctests do not run on this code.

## Why vendored

The published 0.2.2 pins ghostty `a887df42c`, which predates the fix for
GHSA-w9q4-5jvv-mcv4 (ghostty `e257bb45`, PR #14857) and the runtime
`GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES` setter. No newer libghostty-vt is
published, and the sys crate only takes a `GHOSTTY_SOURCE_DIR` override, so
the pin is bumped here.

## Local changes

Recorded per commit in git history; the list is kept current:

- `libghostty-vt/Cargo.toml`: `path` on the `libghostty-vt-sys` dependency.
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
- `libghostty-vt-sys/build.rs`: applies `libghostty-vt-sys/patches/*.patch`
  (name order, `git apply`) to the fetched ghostty; the source stamp
  records the commit and the patch contents. Not applied to a
  `GHOSTTY_SOURCE_DIR` tree.
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
    magic, `@sizeOf(Page)` and an XxHash3 checksum over the `Page` value
    and the LZ4 bytes before it reads either, then that the `Page` value
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
    16-bit `CellCountInt`), and no open link on the inactive screen's
    cursor (`Terminal.switchScreen` asserts it).
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

## Bumping ghostty

1. Pick a ghostty commit and set `GHOSTTY_COMMIT` in
   `libghostty-vt-sys/build.rs`.
2. Match Zig to the commit's `build.zig.zon` `minimum_zig_version` in
   `mise.toml` and `.github/workflows/ci.yml`.
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
6. Re-check the constants in `crates/vtmux-vt/src/term.rs` (`PAGE_BYTES`,
   `PAGE_GRID_BYTES`, `CELLS_ALIGN`; `page_rows_matches_ghostty` fails if
   the page layout changed) and run the slow gate.
