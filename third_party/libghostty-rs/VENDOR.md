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

## Bumping ghostty

1. Pick a ghostty commit and set `GHOSTTY_COMMIT` in
   `libghostty-vt-sys/build.rs`.
2. Match Zig to the commit's `build.zig.zon` `minimum_zig_version` in
   `mise.toml` and `.github/workflows/ci.yml`.
3. Diff `include/ghostty/vt/` between the old and new commit and fix
   `libghostty-vt-sys/src/bindings.rs` and the wrapper; or regenerate with
   `cargo run -p libghostty-vt-sys --features bindgen-tool --bin gen-bindings`
   (needs libclang; see `tools/gen_bindings.rs`).
4. Re-check the constants in `crates/vtmux-vt/src/term.rs` (`PAGE_BYTES` and
   friends) and run the slow gate.
