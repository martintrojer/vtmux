# CI in parallel jobs (2026-11-12)

CI ran `scripts/check.sh` whole, one stage after another: about 30 min on `ubuntu-latest` and 47 min on `macos-15` (3 vCPU). It now runs five jobs per OS at once, each `check.sh --only <stages>`, so a run takes as long as its longest job. Estimate: about 13 min on Linux and 21 min on macOS. These are estimates from this host's stage times, not measured on GitHub yet.

## Stages

`check.sh --list` gives six stages. CI's matrix runs each once (`cargo test -p xtask ci` checks this):

| CI job | `check.sh --only` | Runs |
|---|---|---|
| lint | `lint` | `cargo fmt --check` (both), clippy, the other OS's `cargo check` (Linux job only: macOS target installed) |
| test | `test` | `cargo test --workspace`, the Ghostty wrapper's tests |
| regress | `regress,replay-upgrade-1` | `cargo xtask regress`, `replay --upgrade-every 1 --only …` |
| replay | `replay` | `cargo xtask oracle replay` |
| replay-upgrade-3 | `replay-upgrade-3` | `replay --upgrade-every 3` |

A `check` job (and `check-macos`) passes only when every stage job passed, so there is still one result to wait on.

## The stages add up to the whole gate

With a fake `cargo` on PATH that logs its arguments (`VTMUX_CHECK_PARALLEL=0`):

- `check.sh` logs the same 20 cargo commands, in the same order, as `check.sh` at 034e438a.
- The six `--only <stage>` runs together log the same set of commands as the full run. Each also runs the reap and both socket-guard listings.
- `--only lint,test,regress,replay,replay-upgrade-3,replay-upgrade-1` logs exactly what the full run logs.
- With `VTMUX_CHECK_HARNESS=0`, the run no longer builds `xtask` and `vtmux` just for the harness it skips.

## Stage times

This host, 4 CPUs (`taskset -c 0-3`, so suites run one after another, as on a runner), builds already done, load 9 to 24:

| Stage | s |
|---|---|
| test | 259 |
| regress | 274 |
| replay | 315 |
| replay-upgrade-3 | 494 |
| replay-upgrade-1 | 30 |
| sum | 1372 (22.9 min) |

So about 7 of CI's 30 Linux minutes went to setup, two cold Ghostty builds (the wrapper's tests use a second build-script hash), the workspace build and clippy. Cold, Ghostty's `zig build` took 72 s here on 96 CPUs (160 s user time). With its output restored it took under a second.

## Estimate

Each job pays setup (about 1 min) and builds its own binaries (about 3 min on 4 vCPU; rust-cache keeps the dependencies and the new Ghostty cache keeps Zig's output). The longest job is replay-upgrade-3: 494 s plus about 4 min, so about 13 min on Linux. macOS was 47/30 = 1.6 times slower, which gives about 21 min. Runner minutes go up, because each of the five jobs builds on its own: roughly 23 + 5 × 4 = 43 min on Linux.

## Caches

- **rust-cache:** one cache per job group (`lint`, `test`, `replay` shared by the three harness jobs, `perf`), so the jobs don't overwrite each other's caches.
- **Ghostty:** `target/**/build/vtmux-ghostty-vt-sys-*/out`, about 130 MB with zstd per build. It is keyed on the OS, arch, Zig version, `build.rs` (the pin), the patches, `Cargo.lock`, `Cargo.toml` and `rust-toolchain.toml`, plus the job group. rust-cache deletes this directory (it belongs to a package under the workspace root), which is why every run used to rebuild Ghostty.

## Gate

GATE: 07fbc6eb `muzzle run --class high --rule vtmux-check --run-timeout 3000 -- mise exec -- bash scripts/check.sh`, full
RESULT: exit 0, 260 s; 1805 tests passed, 11 ignored (old-binary upgrade tests, timing benches, as on main); regress 171/174 pass, 1 xfail, 2 known flaky (passed); replay 443/443; `--upgrade-every 3` 443/443; `--upgrade-every 1` 9/9. Skipped: the macOS type-check (no `aarch64-apple-darwin` target here: `rustup target add` cannot reach static.rust-lang.org). Zig cross-building Ghostty for aarch64-macos ran by hand and passed (74 s).
CONDITIONS: shared CentOS 9 devserver, nproc 96, suites parallel; Rust 1.99.1, Zig 0.17.0; warm cache; Ghostty source copied from the main checkout's build at the pinned commit with this tree's patches.

Each stage also passed on its own (`--only lint`, `test`, `regress`, `replay`, `replay-upgrade-3`, `replay-upgrade-1`, `regress,replay-upgrade-1`), both unpinned and pinned to 4 CPUs. Exception: one pinned `test` run failed in `attach::history_file_is_saved_at_exit_and_loaded_after_the_config`, on the reference tmux side (`show-prompt-history` printed nothing), at load 14. That test is unchanged here, and it passed when rerun. It looks like a race with the old server's exit and needs a follow-up.
