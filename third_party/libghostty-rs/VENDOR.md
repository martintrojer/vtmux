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
5. Re-check the constants in `crates/vtmux-vt/src/term.rs` (`PAGE_BYTES`,
   `PAGE_GRID_BYTES`, `CELLS_ALIGN`; `page_rows_matches_ghostty` fails if
   the page layout changed) and run the slow gate.
