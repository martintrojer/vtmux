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
4. Re-check the constants in `crates/vtmux-vt/src/term.rs` (`PAGE_BYTES` and
   friends) and run the slow gate.
