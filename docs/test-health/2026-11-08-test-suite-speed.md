# Slow gate speed, 2026-11-08

`scripts/check.sh` took 1102 s on a build host, against a 10 min budget. It now takes 220 s there (499 s with the suites one after another, as CI's runners run them), with the same tests, scenarios and regress scripts, all passing.

## Method

- Host: a build host (96 CPUs, shared with other workers), warm `target/`, the gate under muzzle as AGENTS.md runs it (`muzzle run --class high --rule vtmux-check`: 3 slots, so `RUST_TEST_THREADS=3`).
- Gate phases: before, `bash -x` with a timestamp in `PS4`; after, `check.sh`'s own per-suite seconds. CPU from muzzle's `runs.jsonl` (`cpu_s` of the whole gate).
- Per-test wall time: stable `cargo test` has no per-test times (`--report-time` is nightly), and nextest is not a dependency, so `scripts/test-times.sh` (added here) runs every test of a binary alone (`--exact`, one thread), 8 at once, and sums them. The sum is what a test binary costs on N threads, about sum / N, unless one test is longer than that.
- Profiles: `perf record` of the slowest CPU-bound test; for the oracle, a temporary counter of settles that waited out their 3 s reaction limit (not committed).

## The gate

| phase | before | after (parallel) | after (`VTMUX_CHECK_PARALLEL=0`) |
|---|---:|---:|---:|
| reap, fmt, clippy, builds | 35 s | 9 s | 9 s |
| `cargo test --workspace` | 436 s | 211 s | 210 s |
| `cargo xtask regress` | 98 s | 81 s | 80 s |
| `oracle replay` | 198 s | 72 s | 72 s |
| `oracle replay --upgrade-every 3` | 308 s | 125 s | 112 s |
| `oracle replay --upgrade-every 1` (9 scenarios) | 25 s | 22 s | 21 s |
| **total** | **1102 s** | **220 s** | **499 s** |
| CPU (all processes), average cores | 1104 s, 1.0 | 852 s, 3.9 | 812 s, 1.6 |
| memory peak | 5.0 GB | 2.0 GB | 1.1 GB |

In parallel mode the five suites start together and the gate takes as long as the longest (`cargo test`). The CPU it needs went down (no rebuild on every cargo call, no pretty-printing of whole terminal states), but it is spent over a fifth of the time: 3.9 cores on average against the rule's 3 slots, so muzzle's `vtmux-check` weight may want retuning once `muzzle report` has a few runs.

`cargo test --workspace` by binary (gate, 3 threads):

| binary | before | after |
|---|---:|---:|
| vtmux `attach` | 234.8 s | 102.6 s |
| vtmux `cli` | 58.8 s | 31.2 s |
| vtmux-server `snapshot_term` | 38.3 s | 16.9 s |
| vtmux `control` | 31.0 s | 15.9 s |
| vtmux-vt `state_roundtrip` | 21.0 s | 7.3 s |
| vtmux `upgrade` | 13.3 s | 11.9 s |
| vtmux-vt unit | 10.6 s | 7.6 s |

Per-test sums (each test alone, 8 at once):

| binary | before | after | longest test before -> after |
|---|---:|---:|---|
| `attach` | 655 s | 300 s | `typed_meta_bspace_follows_the_backspace_option` 93 s -> 12 s; longest now `slow_terminal_detaches_promptly` 31 s (unchanged) |
| `cli` | 172 s | 91 s | `format_jobs_chained` 34 s -> 17 s |
| `control` | 61 s | 44 s | `stalled_client_does_not_keep_the_server` 24 s -> 12 s |
| `snapshot_term` | 72 s | 36 s | `long_history_in_both_forms` 38 s -> 17 s |
| `state_roundtrip` | 37 s | 12 s | `oracle_pane_programs_round_trip` 20 s -> 7 s |

## What changed, by commit

1. **libghostty-vt-sys's build script reran on every cargo call.** It printed `rerun-if-changed=crates/libghostty-vt-sys/build.rs`, a path of upstream's repo layout that does not exist in the vendored package, and cargo counts a missing file as changed: every `cargo` command rebuilt libghostty-vt-sys and the five crates above it and rewrote `target/debug/vtmux`. A few seconds per call, about ten calls a gate, and a binary swapped under any suite running meanwhile.
2. **The terminal-state round trips compared pretty-printed text.** `exported_flat` formatted a pane's whole re-exported state with `{:#?}` on every round trip, pass or fail; `perf` put `PadAdapter::write_str` at 20-32% of `snapshot_term`'s cycles and most of the rest in string pushes and debug UB checks. They compare the values now and format only to show a difference, and vtmux-vt builds at opt-level 1 in the dev profile.
3. **The oracle pool was 6 whatever the host, in name order.** 1138 s of scenarios on 6 workers; the slow ones (`status_msg_freeze` 45 s, `menu_mouse_stayopen` 35 s, the other `status_msg_*`) sort late and ran on alone at the end. One scenario per 6 CPUs past 6 (at most 16), and each run's times (`target/oracle-durations/`) start the slowest first next time: 198 s -> 110 s (bigger pool) -> 72 s (ordered).
4. **Races the load exposed** (pre-existing; see *Flakes*).
5. **A test's independent worlds run side by side** (`crates/vtmux/tests/par`): the two binaries of 88 attach tests, cli's and control's `compare`, and the cases of the slowest loops. Flood tests stay one at a time (each keeps several CPUs busy and some time their exits), as do runs that share a file outside their worlds.
6. **`check.sh` runs the suites at once on a host with `nproc` 32 or more** (first 16; together they peaked at 14.5 of 16 CPUs, so a 16-core laptop runs them serially), after making and checking fresh every build they make.

## Flakes

Every changed test binary ran 5 times under CI's runner size: `taskset -c 0-3` with 4 busy loops on the same 4 CPUs (`flock /tmp/vtmux-stress.lock`), so 4 test threads and the inner parallel runs on top.

- First pass (parallel runs, races unfixed): attach 4/5, cli 1/5, control 4/5. The failures were all one of two races. A server exits a moment after `kill-server` or its last session, and the next command reaches it on the way out: `server exited unexpectedly` (`format_time_unparseable_tz_name`, `client_window_target_without_a_client`, `wait_exit_lingers_until_an_empty_line`, `config_causes_show_in_view_mode_once_attached`). Or an attached client's reader thread is still draining the terminal when a test reads its text right after the exit (`terminals_tmux_knows_attach`, `terminals_tmux_refuses_leave_no_session`, `clear_on_attach_off_scrolls_instead_of_the_alternate_screen`).
- The unchanged cli tests under the same load: `format_time_unparseable_tz_name` failed 2 runs in 5. The races predate this work; the parallel runs only made them likelier.
- After the fixes (wait for the server's pid, or its socket, to go; `exited` waits for the terminal to close): attach 5/5 (81 s a run), cli 5/5 (32 s; 55 s before the parallel runs), control 5/5 (16 s), snapshot_term 5/5 (32 s), state_roundtrip and xtask's unit tests 5/5.
- Once with a dash `/bin/sh` (Fedora EPEL's dash 0.5.11 bind-mounted over `/usr/bin/sh` in a user and mount namespace, which also replaces `bash` there): attach 126/126, control 30/30, cli 152/153. The one is `current_command_is_read_as_bytes`, which runs `bash -c 'exec -a …'` and gets dash, whose `exec` has no `-a`: the namespace's doing, not the test's.
- The gate's five suites ran together three times with no suite failing (the first run's exit 2 came from the script's own cleanup trap, fixed before it was committed); the last full gate, on the final commit, took 214 s (`cargo test` 210 s, regress 80 s, replay 72 s, `--upgrade-every 3` 112 s with its times from the run before, `--upgrade-every 1` 21 s).

## Not done, and where the time is now

- **The oracle's reaction limit.** After an input, a `settle` waits for the output it provoked, up to 3 s (`REACTION_LIMIT`), and an input that provokes nothing (a mouse release over nothing, a key a menu ignores) waits all of it. In one replay 149 settles did: 447 s of the 1138 s of scenarios (`menu_mouse_stayopen` 10 of them, `border_style_redraws` 9, `progress_bar` and `popup_mouse` 7). Cutting it needs a way to know no reaction is coming, for the reference as much as for vtmux; a shorter limit would turn a slow host's late reaction into a mismatch.
- **Flood tests.** `slow_terminal_detaches_promptly` (31 s), `stalled_terminal_keeps_server_memory_bounded` (30 s), `detach_after_stall_is_not_behind_a_backlog` (20 s) and `stalled_terminal_bounds_passthrough` (12 s) hold a stall or a slow terminal for seconds by design and keep several CPUs busy each; they are a third of `attach`'s remaining 300 s.
- **Regress** is bounded by `alerts.sh` (80 s, its own sleeps; vendored, never edited).
- **Shared setup.** Starting a server costs 17 ms for tmux 3.8 and for debug vtmux (`new -d` plus `kill-server`), and the gate builds nothing in release, so there was no expensive setup to share.
