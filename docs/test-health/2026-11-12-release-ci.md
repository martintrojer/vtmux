# Release CI before 3.8-rc1 (2026-11-12)

Fixes from the M5 final review panel: the beta3 upgrade tests run, the musl tarballs build before a tag, and CI has one Rust pin.

## Host

A shared EL9 server (96 CPUs, load 6-8 from other workers), Rust 1.99.1, Zig 0.17.0, libghostty-vt 1.0.1 built from the signed release tarballs (`GHOSTTY_SOURCE_TARBALL`, `GHOSTTY_DEPS_TARBALL`). GitHub and static.rust-lang.org are unreachable here: no GitHub Actions run, no download from the beta3 release, and no musl `rust-std`.

## Beta3 both ways

- `a_beta3_server_upgrades_onto_this_build_and_back` and `a_downgrade_to_beta3_leaves_the_terminal_cooked_after_the_detach` are no longer `#[ignore]`d. Without `VTMUX_BETA3_BINARY` they call `common::skip`: `SKIP: <test>: VTMUX_BETA3_BINARY unset (scripts/fetch-beta3.sh)` goes to stderr past libtest's capture, and onto the list `check.sh` prints and counts at its end (`check: N test(s) skipped (not passed):`). `-V` must print `vtmux 3.8-beta3`. The same helper now reports the resurrect plugins missing and the root `RLIMIT_NPROC` case in `cli.rs`.
- After the first upgrade, the both-ways test respawns the pane beta3 restored (`respawn-pane`, then keys reach `cat`: the word is shown twice, echo and output). This was untested in any gate.
- Against the 3.8-beta3 release binary (x86_64 gnu) on this tree, after the libghostty-vt 1.0.1 rebase: both pass in 0.55 s. 5x under 48 busy loops (flock `/tmp/vtmux-stress.lock`): 5/5, 0.60-0.66 s. Once with `sh` as zsh's sh emulation first on `PATH` (no dash on the host): 2/2.
- `scripts/fetch-beta3.sh` downloads the runner's tarball (x86_64/aarch64 gnu, aarch64-apple-darwin: beta3's release matrix), checks it against the release's `SHA256SUMS`, and prints the binary's path. CI's test jobs (Linux, macOS) put it in `VTMUX_BETA3_BINARY`. Tried against a local HTTP mirror (`VTMUX_BETA3_URL`) of a tarball repacked from the release binary: it downloads, verifies and prints the path; a second run downloads nothing; a corrupted tarball fails `sha256sum -c` (exit 1).

## Musl before a tag

ci.yml's `musl` jobs run `release-tarball.sh` for `x86_64-unknown-linux-musl` (ubuntu-22.04) and `aarch64-unknown-linux-musl` (ubuntu-22.04-arm), as release.yml does. That covers the link, the static check and `-V` under both names, and `check` waits on them. They could not run here (no musl `rust-std`). For cost, the same release build for gnu, pinned to 4 CPUs as a runner has: about 350 s cold (Ghostty ReleaseFast included) and 111 s with only the workspace crates rebuilt, which is what rust-cache leaves. The jobs run beside the five stage jobs, on runners of their own. A run gets longer only if a musl job outlasts the slowest of those.

## One Rust pin

The setup action reads `channel` from `rust-toolchain.toml` (a `sed` step) and gives that toolchain to `dtolnay/rust-toolchain`. In GitHub's Linux lint job (`GITHUB_ACTIONS=true`), a missing type-check target now fails `check.sh`. Locally it is still a skip. Tried here: `GITHUB_ACTIONS=true check.sh --only lint` exits 1 with `check: no aarch64-apple-darwin target installed for 1.99.1-…`.

`cargo test -p xtask ci`: three new tests (`ci_rust_is_the_toolchain_file`, `ci_builds_every_musl_release_target`, `ci_test_jobs_have_the_beta3_binary`). All three fail on main's ci.yml and action.yml and pass on this branch. `actionlint` 1.7.12 on the workflows: exit 0. `shellcheck` on check.sh and fetch-beta3.sh: clean.

## The first CI run (to check by hand)

- Both `test` jobs: the step *vtmux 3.8-beta3 for the upgrade tests* prints `…: OK` from the checksum. The test log shows no `SKIP: a_beta3_…` line, and `check.sh` prints no skip list. On macOS the beta3 tests also pass with the darwin release binary. That pairing has never run.
- Both `musl` jobs go green. The aarch64 one is the first aarch64 musl link since the 1.0.1 switch. Note their times against the lint job's.
- The `lint,ghostty-zig` job runs both type-checks (`cargo check … --target aarch64-apple-darwin`, `… x86_64-unknown-linux-musl`) and prints no `its type-check skipped`.
- The setup action's *Rust version* step outputs `1.99.1`.

## Gate

The 300 s command cap here is shorter than one `check.sh` run, so the gate ran as CI runs it, a stage or two per call:

- `lint,ghostty-zig`: pass, 209 s. Zig: 4341/4367 passed, 26 skipped (Zig's own). Darwin and musl type-checks skipped (targets not installed here).
- `test`: the stage's one call ran past the cap (297 s, killed), so its suites ran as separate calls. `cargo test --workspace --exclude vtmux` 1,257 passed, 2 ignored. `-p vtmux` attach 169, cli 164, control 34, upgrade 56 (4 ignored), upgrade_command 11, handshake 11, resurrect 2, bins 5, all passed. Wrapper `--features allocator_api` 40 passed, 3 ignored. `VTMUX_BETA3_BINARY` was set and 0 tests skipped.
- `regress,replay-upgrade-1`: pass, 81 s. regress 171/174 (xfail 1, skip 0, flaky 2 passed), upgrade-every 1 9/9 (230 upgrades).
- `replay`: 447/447, 98 s. `replay-upgrade-3`: 447/447 (3,503 upgrades), 136 s.
