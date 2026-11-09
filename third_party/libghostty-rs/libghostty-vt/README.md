# vtmux-ghostty-vt

Safe Rust API over libghostty-vt: [vtmux](https://github.com/martintrojer/vtmux)'s
fork of [libghostty-rs](https://github.com/uzaaft/libghostty-rs)'s
`libghostty-vt`, over `vtmux-ghostty-vt-sys` (a pinned
[Ghostty](https://github.com/ghostty-org/ghostty) with vtmux's patches, built
with Zig 0.17), with the calls vtmux added. The library is still
`libghostty_vt`. Built for vtmux: no API is promised to anyone else. MIT
(`LICENSE`, libghostty-rs's; upstream is MIT OR Apache-2.0).

Handle types (`Terminal`, `RenderState`, `KeyEncoder`, etc.) are `!Send + !Sync` by design. Callers should drive all operations from a single thread.
