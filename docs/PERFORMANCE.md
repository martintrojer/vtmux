# Performance

vtmux's goal is to be fast and lean, and that is only claimed where it is measured. This page has the numbers against tmux 3.8, what each scenario measures, and how to reproduce them. Raw runs live in [`docs/test-health/`](test-health/).

These scenarios have no attached client. Rendering, many clients and throughput with a client are not measured yet.

## Numbers

Run of 2026-11-01, commit `eea1a69`, Linux 7.2.11 (Fedora 44) on an AMD Ryzen 7 5700X3D (16 cpus), load 3.7, release build, median of 3 runs. Versions: tmux 3.8 (`cargo xtask ref-tmux`), vtmux 0.0.0. Raw output: [`test-health/2026-11-01-perf.md`](test-health/2026-11-01-perf.md) and `.json`.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| idle, 1 session, 60 s | RSS | 4.3 MB | 7.3 MB | 1.7x |
| | CPU over 60 s | 0 ms | 0 ms | - |
| | wakeups | 0 / s | 0 / s | - |
| scale, 400 panes (10 x 10 x 4 idle `cat`) | RSS | 6.4 MB | 20.1 MB | 3.1x |
| | RSS per pane | 5.4 KB | 33.2 KB | 6.1x |
| | CPU to create | 270 ms | 430 ms | 1.6x |
| history, 150k lines, limit 100000, 120x40 | RSS | 66.2 MB | 105.3 MB | 1.6x |
| history, 12k lines, 80x24 truecolour cells | RSS | 25.4 MB | 27.7 MB | 1.1x |
| history, 12k lines of 450 chars, 500x24 | RSS | 29.1 MB | 47.0 MB | 1.6x |
| startup (`new -d` + `kill-server`, 100x) | `new -d` | 9.5 ms | 7.4 ms | 0.8x |
| | `kill-server` | 21.9 ms | 21.4 ms | 1.0x |
| throughput, 100 MB plain log, no client | wall / server CPU | 2.64 s / 2.53 s | 1.22 s / 1.17 s | 0.46x |
| | peak RSS | 5.4 MB | 10.3 MB | 1.9x |
| throughput, 100 MB log, 19% SGR bytes | wall / server CPU | 3.25 s / 3.18 s | 2.64 s / 2.49 s | 0.81x |
| | peak RSS | 5.4 MB | 10.2 MB | 1.9x |

The history rows come from `scripts/bench-history-memory.sh` (details in [`test-health/2026-11-01-history-memory.md`](test-health/2026-11-01-history-memory.md); the output-time column is in the raw run).

## Reading the numbers

- **CPU is a win so far.** With no client, vtmux consumes the pane's output 1.2x to 2.2x faster than tmux. The idle server uses no CPU and takes no wakeups, same as tmux.
- **Memory is the cost.** vtmux runs one libghostty terminal per pane, so every pane pays for Ghostty's page allocation up front: 33 KB per idle pane against tmux's 5.4 KB, and 3 MB more for a bare server. Both are small in absolute terms, but at 400 panes the ratio is 3x. Filed as `perf_scale_rss` (the only ratio above 2x).
- **Scrollback.** Ghostty stores 8 bytes for every cell of every retained row, where tmux stores only the used cells, so wide and mostly-empty rows cost more (1.6x on plain logs and on 500-column rows). `hist_retain` already fixed the retention count (the line limit follows `history-limit`; 148 MB before it, 105 MB now). Compressing idle scrollback is `hist_compress`, not yet merged, so these rows will move.
- **Peak RSS in throughput** is about 5 MB above tmux's: the per-pane terminal plus the history it keeps for the 100 MB of output, capped by the default `history-limit` of 2000.

## Scenarios

`scripts/bench.py` runs each scenario against both binaries (and `BASE=<binary>`, shown as `base`, to compare against an earlier build).

| scenario | what it does | why |
|---|---|---|
| `idle` | `-f /dev/null`, one session running `cat`, 2 s to settle, then `IDLE_SECS` (60) of nothing | A multiplexer sits idle most of the time: baseline RSS, CPU and wakeups. |
| `scale` | 10 sessions x 10 windows x 4 panes (400 idle `cat` panes, 200x50 windows) | Per-pane cost, where the per-pane libghostty terminal shows up. RSS per pane is (RSS after - RSS before) / 399. |
| `history` | calls `scripts/bench-history-memory.sh` (spec, narrow, wide) | Scrollback memory; see the history-memory spec. |
| `startup` | `new -d` then `kill-server`, 100 times on a fresh socket each time | How fast a script's first command and its cleanup are. |
| `throughput` | a pane `cat`s 100 MB of pre-generated log text (about 100-character lines), plain and with about 20% of the bytes SGR, then prints a done marker; no client attached | Parser and grid cost for heavy output with fixed work. Wall time and server CPU are for the same work for both binaries. |

How it measures: the server pid comes from `display -p '#{pid}'` on a private socket (`-L` and `TMUX_TMPDIR`), and is checked gone after `kill-server`; pids are never found by name. Only the server process is measured, never its pane children.

- **Linux CPU:** `/proc/<pid>/stat` utime+stime (fields 14 and 15) deltas per phase, which count all threads and exclude children. Idle uses the nanosecond `schedstat` sum, because 10 ms ticks are too coarse. Never `cutime`/`cstime`, `getrusage` of children or `time(1)`: those include the reaped pane processes.
- **Linux RSS:** settled `VmRSS`, and the per-phase peak is `VmHWM` after `echo 5 > /proc/<pid>/clear_refs`.
- **Wakeups:** deltas of `voluntary_ctxt_switches` over all threads.
- **macOS:** `ps -o rss=,time=` sampled every 100 ms (peak RSS is the largest sample; wakeups are not reported). `scale` raises `ulimit -n` to 4096, or is skipped with the reason if it can't. The macOS numbers have not been collected yet.

## Running

```
mise exec -- python3 scripts/bench.py                 # all scenarios; Markdown on stdout, JSON and Markdown under target/bench/
mise exec -- python3 scripts/bench.py idle scale      # chosen scenarios
IDLE_SECS=5 RUNS=1 STARTUP_N=5 THROUGHPUT_MB=10 mise exec -- python3 scripts/bench.py   # quick development run (about 1 min)
BASE=/path/to/older/vtmux mise exec -- python3 scripts/bench.py
```

A full run takes about 20 minutes. The host is shared: run it when `uptime` load is under 4, under `flock /tmp/vtmux-stress.lock`, and one bench at a time. Other env: `RUNS`, `OUT`, `NO_BUILD=1`, `VTMUX`, `VTMUX_REFERENCE_TMUX`, `HISTORY_SCENARIOS` (documented at the top of the script). Put the result in `docs/test-health/<date>-perf.md` and update the table above.
