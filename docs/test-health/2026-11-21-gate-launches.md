# Processes the slow gate starts (2026-11-21)

How many processes each stage of `scripts/check.sh` creates on a macOS host where starting one is slow, before and after the harnesses stopped making the extra ones.

## Conditions

- Base `3b55e666` (3.8-rc2); after: the same with the change of this record.
- macOS laptop, 16 cores, with several endpoint-security agents: `true(1)` takes about 14 ms to start, with spikes to 1 s. Shared with other agents' builds and tests the whole time (the host's admission queue was full for most of both runs), so wall times are rough.
- Each stage on its own, `mise exec -- bash scripts/check.sh --only <stage>`, one after another, with `target/debug/vtmux` and `xtask` built first.
- Counted from the host's endpoint agent, which logs every fork (new pid, parent) and exec: a script followed the stage's process tree through that log (pids dropped on exit, so a reused pid is not counted). The script is not in the tree.

## The cause

`Command::new("ps")` is `posix_spawnp`, and macOS's tries the name in every `PATH` entry with a `posix_spawn` of its own, each a process that fails to exec. Under `mise exec` `PATH` has 69 entries and `/usr/bin` is the 54th, so a `ps`, `hostname`, `kill` or `lsof` by name made 54 processes. Ten spawns of `true` by name: 540 processes; by path: 10. Node's `child_process` does the same (530 for ten); Python's `subprocess` does not (10).

The harnesses spawned helpers by name in loops: the replay's teardown ran `ps` three times and `hostname` once per scenario, regress's PTY census ran `sysctl` every 100 ms, the tests' teardown polled `ps`. A site `git` wrapper on `PATH` started a few hundred telemetry processes per call, and the Ghostty build applied its 43 patches with a `git apply` each.

## Results

| stage | wall before | wall after | processes before | processes after | execs before | execs after |
|---|---|---|---|---|---|---|
| lint | 18 s | 5 s | 355 | 47 | 95 | 58 |
| test | 529 s | 349 s | 102,753 | 33,004 | 30,788 | 31,688 |
| regress | 438 s | 509 s | 56,871 | 34,399 | 31,057 | 29,478 |
| replay | 282 s | 243 s | 115,226 | 15,222 | 16,291 | 15,821 |
| replay-upgrade-3 | 455 s | 392 s | 122,485 | 22,500 | 29,343 | 28,878 |
| replay-upgrade-1 | 30 s | 27 s | 2,906 | 788 | 1,228 | 1,223 |
| ghostty-zig | 224 s | 254 s | 1,869 | 1,781 | 163 | 238 |
| **all** | **1976 s** | **1778 s** | **402,465** | **107,741** | **108,965** | **107,384** |

Processes created fell 73%; the programs run (execs) stayed the same, as they should. Wall time fell 10% over the gate, but regress and ghostty-zig were slower after, on code this change hardly touches: the host's load moved more than the change did. Each process spared saved something like 1 ms here.

What is left is the work itself: vtmux and tmux clients from the tests (about 20,000), the regress scripts' own `vtmux`, `sleep`, `grep` and `sed` (tmux's suite, not edited), and the panes' programs.

Both runs failed the same way on this loaded host: `initial_window_name_is_the_command` in `test`, five or six regress scripts (`alerts`, `format-render-contexts`, `input-replies`, `sync-output-atomic`, `wait-for-client-lost` in both), and `popup_resize`/`popup_positions`/`shell_bindings_background` in the replays. Those three replay scenarios fail on the base too, run alone; `sync-output-wrapped` and `scrollbars_mouse`, which failed only after, pass run alone.

## The change

- xtask (`progs.rs`) and the tests (`common::program`) look each helper up on `PATH` once and spawn it by path; regress's scripts still start as `sh` under their own `PATH=/usr/bin:/bin`, one process already.
- Signals go through rustix (`kill`, `kill -- -pgid`, a `kill -0` poll); the host name is `uname`'s node name; regress reads `kern.tty.ptmx_max` once.
- `git` is resolved once, on macOS with `xcrun -f git` (the developer tools' own, past a wrapper on `PATH`); the Ghostty build applies its patch stack with one `git apply`, one at a time only when that fails.
