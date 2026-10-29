# AGENTS.md

vtmux is a drop-in tmux 3.8 replacement in Rust on libghostty-vt. The design and every decision (D1–D22) are in [`docs/specs/2026-10-29-vtmux.md`](docs/specs/2026-10-29-vtmux.md). Read it before changing behaviour; a change that contradicts a decision needs the spec updated first.

## Gates

- **Fast** (while iterating): `mise exec -- cargo test -p <crate>` for the crate you touched.
- **Slow** (before closing work, and once per merged batch): `mise exec -- bash scripts/check.sh` — `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` (pedantic is on via `[workspace.lints]`), `cargo test --workspace`, then `cargo xtask regress` and `cargo xtask oracle replay` once those exist.
- **Targeted slow**: `mise exec -- cargo xtask regress --only <test>` and `mise exec -- cargo xtask oracle replay --only <scenario>`.
- **Budget**: slow gate 10 min. Over budget triggers test upkeep, not a stop.
- A skip is not a pass: say so in the gate record.

Record each gate run in the task note (`GATE: commit command scope`, `RESULT: exit duration counts skips`, `CONDITIONS: host toolchain cache`).

## Toolchain

- Rust is pinned in `rust-toolchain.toml`; Zig is pinned in `mise.toml` (libghostty-vt 0.2.2 needs exactly 0.15.2). Run cargo through `mise exec --` so the right Zig is on PATH. The first build of `libghostty-vt-sys` compiles ghostty from source (~30 s warm machine, longer cold).
- The reference tmux 3.8 lives at `.cache/tmux-3.8/bin/tmux` (built by `cargo xtask ref-tmux`), or `$VTMUX_REFERENCE_TMUX`. Never use the `tmux` on PATH as the reference: it is whatever mise installed.

## Rules

- `unsafe` only in `crates/vtmux-vt` and `crates/vtmux-server/src/sys/`. Every other crate root has `#![forbid(unsafe_code)]` (D10).
- Behaviour comes from tmux 3.8, read as a spec (D11). Cite the tmux source file and function when you match a non-obvious behaviour.
- Never edit `compat/tmux-3.8/regress/`. Divergences go in `compat/patches/` and `compat/DIVERGENCES.md` (spec, *Divergence process*).
- Every behaviour fix lands with an oracle scenario or a regress flip that proves it.
