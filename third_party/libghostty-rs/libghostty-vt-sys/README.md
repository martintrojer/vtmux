# vtmux-ghostty-vt-sys

Raw FFI bindings for libghostty-vt: [vtmux](https://github.com/martintrojer/vtmux)'s
fork of [libghostty-rs](https://github.com/uzaaft/libghostty-rs)'s
`libghostty-vt-sys`, on a pinned [Ghostty](https://github.com/ghostty-org/ghostty)
commit with vtmux's patches (`VENDOR.md` in vtmux's repository lists every
one). The library is still `libghostty_vt_sys`. Built for vtmux: no API is
promised to anyone else.

Building it needs [Zig](https://ziglang.org/download/) 0.17 on `PATH`
(`mise use -g zig@0.17`), and curl, tar and git: the build script downloads
GitHub's archive of the pinned commit, checks its Zig package hash, and
applies the patches shipped in this crate. Offline, set
`GHOSTTY_SOURCE_TARBALL` to the absolute path of a copy of that archive (or
of `git archive --format=tar.gz` of the commit); `GHOSTTY_SOURCE_URL` names
a mirror.

MIT (`LICENSE`, libghostty-rs's; upstream is MIT OR Apache-2.0). Ghostty is
MIT too (`LICENSE-ghostty`).

From upstream's README:

- Fetches and builds `libghostty-vt.a` from ghostty sources via Zig by default.
- Exposes checked-in generated bindings in `src/bindings.rs`.
- Static linking is the baseline rather than a Cargo feature. Enable the
  additive `link-dynamic` feature to link the shared library instead.
- Set `GHOSTTY_SOURCE_DIR` to force the build to use a local Ghostty checkout.
- Set `GHOSTTY_ZIG_SYSTEM_DIR` to force Zig package resolution through a
  pre-fetched `zig build --system` directory. This is intended for Nix and other
  sandboxed package managers that cannot fetch during build scripts.
- Vendored builds target Zig's portable `baseline` CPU by default. Set
  `LIBGHOSTTY_VT_SYS_CPU` to `native`, a named CPU model such as `x86_64_v3`, or
  another Zig CPU expression to optimize for known deployment hardware.
- Set `LIBGHOSTTY_VT_SYS_OPTIMIZE` to `Debug`, `ReleaseSafe`, `ReleaseFast`, or
  `ReleaseSmall` to override the Zig optimize mode used by vendored builds.
- If the `pkg-config` feature is enabled, the build will use an installed
  `libghostty-vt` found through `pkg-config` only when `GHOSTTY_SOURCE_DIR` is
  unset. With the default static link mode, it probes Ghostty's
  `libghostty-vt-static` pkg-config module instead.
- libghostty-vt is pre-1.0, so these bindings do not guarantee compatibility
  with arbitrary installed C API revisions.
