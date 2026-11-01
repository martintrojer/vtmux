# AGENTS.md

vtmux is a drop-in tmux 3.8 replacement in Rust on libghostty-vt. The design and every decision (D1–D22) are in [`docs/specs/2026-10-29-vtmux.md`](docs/specs/2026-10-29-vtmux.md). Read it before changing behaviour; [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains how the crates and processes fit; a change that contradicts a decision needs the spec updated first.

## Gates

- **Fast** (while iterating): `mise exec -- cargo test -p <crate>` for the crate you touched.
- **Slow** (before closing work, and once per merged batch): `mise exec -- bash scripts/check.sh` — `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` (pedantic is on via `[workspace.lints]`), `cargo test --workspace`, then `cargo xtask regress` and `cargo xtask oracle replay` once those exist.
- **Targeted slow**: `mise exec -- cargo xtask regress --only <test>` and `mise exec -- cargo xtask oracle replay --only <scenario>`.
- **Budget**: slow gate 10 min. Over budget triggers test upkeep, not a stop.
- A skip is not a pass: say so in the gate record.

Record each gate run in the task note (`GATE: commit command scope`, `RESULT: exit duration counts skips`, `CONDITIONS: host toolchain cache`).

## Toolchain

- Rust is pinned in `rust-toolchain.toml`; Zig is pinned in `mise.toml` (libghostty-vt 0.2.2 needs exactly 0.15.2). Zig 0.15.2 cannot link against the macOS 26 SDK, so CI's macOS job runs on `macos-14`; a local macOS 26 build needs an older SDK. Run cargo through `mise exec --` so the right Zig is on PATH. The first build of `libghostty-vt-sys` compiles ghostty from source (~30 s warm machine, longer cold).
- `scripts/check.sh` also type-checks the other OS (`cargo check --target aarch64-apple-darwin` on Linux) when that target is installed. Install it (`rustup target add aarch64-apple-darwin`): macOS-only build breaks otherwise reach CI first.
- CI: Linux runs the full `scripts/check.sh`. macOS must build, pass clippy and the unit tests; its tmux-comparison suites run but are informational until task `macos_attach_parity` closes (macOS-specific FIFO, locale and process-name gaps).
- The reference tmux 3.8 lives at `${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux` (built by `cargo xtask ref-tmux`), or `$VTMUX_REFERENCE_TMUX`. It is shared by every worktree. Never use the `tmux` on PATH as the reference: it is whatever mise installed. Building it needs libevent and ncurses headers, bison (configure requires yacc), pkg-config and a C compiler; on Fedora Atomic run the build inside the `vtmux-dev` toolbox (`toolbox run -c vtmux-dev mise exec -- ...`); the resulting binary runs on the host.
- A fresh worktree needs `mise trust` once before `mise exec` works.

## Rules

- `unsafe` only in `crates/vtmux-vt` and `crates/vtmux-server/src/sys/`. Every other crate root has `#![forbid(unsafe_code)]` (D10).
- Behaviour comes from tmux 3.8, read as a spec (D11). Cite the tmux source file and function when you match a non-obvious behaviour.
- Compliance is at the edge only; the core is idiomatic Rust (spec, *Rust at the core, tmux at the edge*). Don't copy tmux's structures, flag ints or names into the core. When tmux ordering or ID behaviour is visible at the edge, reproduce it explicitly and pin it with an oracle scenario.
- Never edit `compat/tmux-3.8/regress/`. Divergences go in `compat/patches/` and `compat/DIVERGENCES.md` (spec, *Divergence process*).
- Every behaviour fix lands with an oracle scenario or a regress flip that proves it.
