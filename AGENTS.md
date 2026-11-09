# AGENTS.md

vtmux is a drop-in tmux 3.8 replacement in Rust on libghostty-vt. The design and every decision (D1–D22) are in [`docs/specs/2026-10-29-vtmux.md`](docs/specs/2026-10-29-vtmux.md). Read it before changing behaviour; [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains how the crates and processes fit; a change that contradicts a decision needs the spec updated first.

## Gates

- **Fast** (while iterating): `mise exec -- cargo test -p <crate>` for the crate you touched.
- **Slow** (before closing work, and once per merged batch): `mise exec -- bash scripts/check.sh` — `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` (pedantic is on via `[workspace.lints]`), `cargo test --workspace`, then `cargo xtask regress` and `cargo xtask oracle replay` once those exist. The test suites and harness runs go at once, each log printed whole as it ends, only when `nproc` (the CPUs this process may use, so a pinned gate counts its pins) is 32 or more, or with `VTMUX_CHECK_PARALLEL=1`; otherwise (a 16-core laptop, CI's runners) they run one after another, and `VTMUX_CHECK_PARALLEL=0` forces that on a big host.
- **Targeted slow**: `mise exec -- cargo xtask regress --only <test>` and `mise exec -- cargo xtask oracle replay --only <scenario>`.
- **Budget**: slow gate 10 min. Over budget triggers test upkeep, not a stop: `scripts/test-times.sh` gives a test binary's per-test times (last pass: `docs/test-health/2026-11-08-test-suite-speed.md`).
- A skip is not a pass: say so in the gate record.

Record each gate run in the task note (`GATE: commit command scope`, `RESULT: exit duration counts skips`, `CONDITIONS: host toolchain cache`).

## Toolchain

- Rust is pinned in `rust-toolchain.toml`; Zig is pinned in `mise.toml` (0.17.0: the ghostty commit's `build.zig.zon` requires that major.minor). Zig 0.17 targets macOS 15+, so CI's macOS job runs on `macos-15`. Run cargo through `mise exec --` so the right Zig is on PATH. The first build of `libghostty-vt-sys` clones ghostty and compiles it from source (~1 min warm machine, several cold).
- `scripts/check.sh` also type-checks the other OS (`cargo check --target aarch64-apple-darwin` on Linux) when that target is installed. Install it (`rustup target add aarch64-apple-darwin`): macOS-only build breaks otherwise reach CI first.
- CI: Linux and macOS both run the full `scripts/check.sh` as a hard gate. The macOS reference calls Darwin's libc, so a few byte-level rows compare on Linux only (`GLIBC_REFERENCE` in `cli.rs`, D-019).
- libghostty-vt is vendored in `third_party/libghostty-rs` (path dependency, outside the workspace, so the workspace lints and doctests skip it). `VENDOR.md` there records the pinned ghostty commit, every local change, and how to bump the pin.
- The reference tmux 3.8 lives at `${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux` (built by `cargo xtask ref-tmux`), or `$VTMUX_REFERENCE_TMUX`. It is shared by every worktree. Never use the `tmux` on PATH as the reference: it is whatever mise installed. Building it needs libevent and ncurses headers, bison (configure requires yacc), pkg-config and a C compiler; on macOS an ncurses 6 (`brew install ncurses`, or an ncurses 6 build's `lib/pkgconfig` on `PKG_CONFIG_PATH`), because macOS's own 5.4 reads a 2015 terminfo database and `ref-tmux` rejects a reference linked to it; on Fedora Atomic run the build inside the `vtmux-dev` toolbox (`toolbox run -c vtmux-dev mise exec -- ...`); the resulting binary runs on the host.
- A fresh worktree needs `mise trust` once before `mise exec` works.

## Rules

- `unsafe` only in `crates/vtmux-vt` and `crates/vtmux-server/src/sys/`. Every other crate root has `#![forbid(unsafe_code)]` (D10).
- Behaviour comes from tmux 3.8, read as a spec (D11). Cite the tmux source file and function when you match a non-obvious behaviour.
- Compliance is at the edge only; the core is idiomatic Rust (spec, *Rust at the core, tmux at the edge*). Don't copy tmux's structures, flag ints or names into the core. When tmux ordering or ID behaviour is visible at the edge, reproduce it explicitly and pin it with an oracle scenario.
- Never edit `compat/tmux-3.8/regress/`. Divergences go in `compat/patches/` and `compat/DIVERGENCES.md` (spec, *Divergence process*).
- Every behaviour fix lands with an oracle scenario or a regress flip that proves it.
- Tests and scenarios must not depend on the host: pin `status-right` (it shows the host name, and a long one hides the window list on a narrow client) and pane titles before waiting on the status line, and don't assume Linux paths, `sh`, `cat` or glibc (CI gates macOS too; see `oracle/README.md`, *Host names on screen* and *Races under load*).
- A server leaves its socket file behind when it exits. Every test harness owns its socket dir: a private `TMUX_TMPDIR` under `/tmp` (short: `sun_path` is 104 bytes on macOS) passed to every client, or `-S` in a private dir, never the real `tmux-<uid>/` or `vtmux-<uid>/`. `scripts/check.sh` fails when a run adds a test-labelled socket there (`scripts/socket-guard.sh`, `cargo xtask sockets`); `cargo xtask reap` removes dead ones (connect refused), never `default` or a non-test label.
- Ad-hoc probe scripts (reviewers', debugging) must clean up: `trap '… kill-server' EXIT` for every `-L` label they start, and prefer the harness (`cargo xtask regress --only`, `oracle replay --only`), which reaps its own servers. Name probe labels `rv*` (or `testX$$`, `cliN`, `attN`). `cargo xtask reap [--min-age <min>] [--dry-run]` kills leftovers: test-labelled `tmux`/`vtmux` servers over 1 h old whose parent is init, plus their pane programs, never its own ancestors, then removes dead test-labelled socket files from the real socket dirs; `check.sh` runs it first.
