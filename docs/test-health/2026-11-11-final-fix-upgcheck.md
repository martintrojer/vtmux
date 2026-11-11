# final_fix_upgcheck: the macOS end-to-end review's four findings

1. **High, data loss.** `vtmux-upgrade -b /bin/echo` (any program exiting 0) passed the dry run, and the exec replaced the server with it. The dry run now passes only when the candidate writes `vtmux-snapshot-checked <hex>` on stderr, the BLAKE3 body hash the stream's header records, for the stream it was given (`sys::upgrade::CHECKED`). A vtmux 3.8-beta2 or older writes no such line and is refused too: a live downgrade onto one is not possible, and the downgrade tests vouch for such a build through a wrapper (`acknowledging`).
2. **Medium.** A runtime `set -s vtmux-persist off` then `kill-server` left a checkpoint saying `full`, which the next start restored (a config that does not set the option goes by the checkpoint's value). Turning it off now rewrites `manifest` and `manifest.prev` with the option `off`, nothing else changed, files kept (`persist::mark_off`). Removing the manifests was rejected: the files stay until the user removes them.
3. **Low.** A bracketed paste in the same read as a key bound to `vtmux-upgrade` refused it. Paste marks come only from `\e[200~`/`\e[201~`, so they go back to the client's input as those bytes, like the keys.
4. **Low.** README: a restarted shell starts in the pane's start directory, as tmux's `respawn-pane` does (spawn.c `spawn_pane`: a respawn without `-c` keeps `wp->cwd`).

## Tests

Each fails before its fix (checked by disabling the fix) and passes after:

- `upgrade::a_program_that_exits_0_is_no_vtmux_to_upgrade_onto`: `/bin/echo`, `/usr/bin/true`, a script exiting 0 and a vtmux from before the line (the real dry run with the line filtered out) each exit 1 with the message, the server pid and pane pids unchanged, the pane still echoes; the old build through `acknowledging` and this build then take over. Before: exit 1 "server exited unexpectedly".
- `persist::persist_turned_off_at_run_time_is_not_restored_at_the_next_start`: off in a loop turn, and `off ; kill-server` in one command line, each leave a next start with only the new session and no `restored` message; the manifest stays, and a config turning the option on restores it. Before: `["a", "fresh"]`.
- `upgrade::a_paste_typed_right_after_a_bound_upgrade_is_pasted_in_the_new_image`: `F5 \e[200~pa`, then after the upgrade `xste\e[201~x F7`: the pane shows `paxste`, only the last `x` runs its binding. Before: the upgrade never happened.
- Unit: `a_dry_run_passes_only_with_the_snapshots_acknowledgement` (exit 0, `TAKES_SOCKET` alone, another stream's digest, the line run into other output or without its newline are refused), `marking_off_rewrites_only_the_option`, `a_paste_typed_after_a_bound_upgrade_goes_back_whole`, `a_streams_digest_is_its_bodys_hash`.

A probe with a real 3.8-beta2 build: refused with exit 1, server and pane pids unchanged; through the acknowledging wrapper it takes over and moves back; `/bin/echo` refused.

## Gate

GATE: f404a368 `muzzle run --class high --rule vtmux-check --run-timeout 3000 -- mise exec -- bash scripts/check.sh`
RESULT: exit 0, 292 s; fmt and clippy clean; tests 1,789 passed, 0 failed, 11 ignored (four old-binary upgrade tests, timing benches, a doc example); regress 171/174 (2 flaky rows passed, 1 xfail, 0 skipped); oracle replay 443/443; `--upgrade-every 3` 443/443 (3,453 upgrades inserted); `--upgrade-every 1` 9/9 (230 upgrades).
CONDITIONS: shared CentOS 9 devserver, 96 CPUs, suites in parallel; Rust 1.99.1, Zig 0.17.0; warm cache; Ghostty from a cached copy of the pinned commit with this tree's patches (no network). The macOS cross-check was skipped: `rustup target add aarch64-apple-darwin` fails DNS here.

GATE: f404a368 stress, 5x the selected tests (upgrade 11, persist 14, server unit 93) under 48 busy loops held under `flock /tmp/vtmux-stress.lock`
RESULT: exit 0, 5/5 passed, 6 s each.
CONDITIONS: busy loops killed by an EXIT trap.

GATE: f404a368 the same selection once with a dash-like `/bin/sh` (`unshare --user --map-root-user --mount`, a POSIX-mode bash that keeps its shell after `-c`, bound over `/usr/bin/bash`)
RESULT: exit 0, all passed.
CONDITIONS: namespace-only bind mount; not a real dash binary.
