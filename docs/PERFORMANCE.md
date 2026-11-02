# Performance

vtmux's goal is to be fast and lean, and that is only claimed where it is measured. This page has the numbers against tmux 3.8, what each scenario measures, and how to reproduce them. Raw runs live in [`docs/test-health/`](test-health/).

These scenarios have no attached client. Rendering, many clients and throughput with a client are not measured yet.

## Numbers

Run of 2026-11-01, commit `eea1a69`, Linux 7.2.11 (Fedora 44) on an AMD Ryzen 7 5700X3D (16 cpus), load 3.7, release build, median of 3 runs. Versions: tmux 3.8 (`cargo xtask ref-tmux`), vtmux 0.0.0. The startup rows were re-run on 2026-11-02 (load 3.6) after the first run's `kill-server` timing was found to measure its 20 ms poll interval; the pid is now polled every 0.5 ms. Raw output: [`test-health/2026-11-01-perf.md`](test-health/2026-11-01-perf.md) and `.json`.

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
| startup (`new -d` + `kill-server`, 100x) | `new -d` | 9.1 ms | 7.0 ms | 0.8x |
| | `kill-server` returns | 1.9 ms | 1.3 ms | 0.7x |
| | server pid gone (from the start of `kill-server`) | 2.4 ms | 2.4 ms | 1.0x |
| throughput, 100 MB plain log, no client | wall / server CPU | 2.64 s / 2.53 s | 1.22 s / 1.17 s | 0.46x |
| | peak RSS | 5.4 MB | 10.3 MB | 1.9x |
| throughput, 100 MB log, 19% SGR bytes | wall / server CPU | 3.25 s / 3.18 s | 2.64 s / 2.49 s | 0.81x |
| | peak RSS | 5.4 MB | 10.2 MB | 1.9x |

### With the owner's config

Run of 2026-11-02, commit `7d48b29` (same host and binaries as above), load 3.9 at start, median of 3 runs for `owner_idle` and 20 runs for `owner_startup`. The owner's real `~/.tmux.conf` (`tmux.conf` at dotfiles `6430744` with local edits, sha256 `c1ffe189d326`), the TPM plugins and mu-crew-dotfiles `d633e15` are copied into a hermetic `HOME`; the plugin revisions are in the raw run. Raw output: [`test-health/2026-11-02-perf-owner.md`](test-health/2026-11-02-perf-owner.md) and `.json`.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| owner_idle, 1 session, 60 s | RSS | 4.6 MB | 8.0 MB | 1.7x |
| | server CPU over 60 s | 187 ms | 48 ms | 0.26x |
| | server wakeups | 4.1 / s | 1.0 / s | 0.26x |
| | poller CPU over 60 s (not the server) | 180 ms | 220 ms | 1.2x |
| owner_startup, `start-server` until `display -p ok` answers, 20x | median (min - max) | 62.9 ms (58.9 - 74.1) | 61.8 ms (58.3 - 80.5) | 0.98x |

The history rows come from `scripts/bench-history-memory.sh` (details in [`test-health/2026-11-01-history-memory.md`](test-health/2026-11-01-history-memory.md); the output-time column is in the raw run).

## Reading the numbers

- **CPU: a win when consuming output, a loss when creating panes.** With no client, vtmux consumes 100 MB of pane output with 0.46x (plain) and 0.78x (19% SGR) of tmux's server CPU, and in 1.2x to 2.2x less wall time. The idle server uses no CPU and takes no wakeups, same as tmux. Creating 400 panes costs 1.6x more CPU (430 ms against 270 ms). Startup is a wash: `new -d` is 7.0 ms against 9.1 ms, and the server pid is gone 2.4 ms after `kill-server` starts for both.
- **Throughput wall time is coarse.** The done marker is polled with `capture-pane` every 0.2 s, so each wall time is accurate to about 0.2 s (up to 17% of vtmux's 1.2 s plain run). The error adds about the same to both binaries, which pulls the ratio toward 1, so the speed-up is if anything understated. Server CPU is not affected by the polling beyond the cost of the `capture-pane` calls themselves.
- **Memory is the cost.** vtmux runs one libghostty terminal per pane, so every pane pays for Ghostty's page allocation up front: 33 KB per idle pane against tmux's 5.4 KB, and 3 MB more for a bare server. Both are small in absolute terms, but at 400 panes the ratio is 3x. Filed as `perf_scale_rss` (the only ratio above 2x).
- **Scrollback.** Ghostty stores 8 bytes for every cell of every retained row, where tmux stores only the used cells, so wide and mostly-empty rows cost more (1.6x on plain logs and on 500-column rows). `hist_retain` already fixed the retention count (the line limit follows `history-limit`; 148 MB before it, 105 MB now). Compressing idle scrollback is `hist_compress`, not yet merged, so these rows will move.
- **Peak RSS in throughput** is about 5 MB above tmux's: the per-pane terminal plus the history it keeps for the 100 MB of output, capped by the default `history-limit` of 2000.

- **With the owner's config the idle server is not idle, and the cause is outside it.** The mu-crew poller wakes about once a second and runs `tmux show-options -g` (and `set-option` when a value changes) as clients of the server, so the server's 187 ms (tmux) and 48 ms (vtmux) over 60 s are the cost of answering those clients: about 3 ms against 0.8 ms per wake-up round. vtmux does less server work per request here (0.26x); the RSS gap is the same 3 MB as for a bare server (1.7x). The poller's own CPU (a Python process plus the `tmux` client processes it starts, counted from `/proc/<pid>/stat` with children) is 180 ms and 220 ms: it is outside the server. The 40 ms difference between the two was not investigated (3 runs, shared host). Only the cheapest probe (`uptime`, a `/proc` read every 60 s) is on; the owner's real list adds `murmur_remote memory cpu uptime muzzle`, which starts `murmur status` and `muzzle status` every 5 to 10 s and is not measured here because it needs a network and the owner's state.
- **Sourcing the owner's config takes 62 ms for both.** A bare `new -d` takes 7 to 9 ms (table above), so the rest is the config; where it goes (the config's synchronous `run-shell` calls, such as `mu-crew-bind`, each start a shell and several `tmux` clients) was not profiled. The 20 runs overlap for the two binaries (58 to 74 ms and 58 to 80 ms).

## Scenarios

`scripts/bench.py` runs each scenario against both binaries (and `BASE=<binary>`, shown as `base`, to compare against an earlier build).

| scenario | what it does | why |
|---|---|---|
| `idle` | `-f /dev/null`, one session running `cat`, 2 s to settle, then `IDLE_SECS` (60) of nothing | A multiplexer sits idle most of the time: baseline RSS, CPU and wakeups. |
| `scale` | 10 sessions x 10 windows x 4 panes (400 idle `cat` panes, 200x50 windows) | Per-pane cost, where the per-pane libghostty terminal shows up. RSS per pane is (RSS after - RSS before) / 399. |
| `history` | calls `scripts/bench-history-memory.sh` (spec, narrow, wide) | Scrollback memory; see the history-memory spec. |
| `startup` | `new -d` then `kill-server`, 100 times on a fresh socket each time | How fast a script's first command and its cleanup are. |
| `owner_idle` | the owner's config in a hermetic `HOME`, one session running `cat`, 10 s to settle, then `IDLE_SECS` (60) of nothing. Reports server RSS, CPU and wakeups, and the poller's CPU separately | What the owner's server costs at rest with the config, TPM plugins and the mu-crew poller running. |
| `owner_startup` | `start-server` followed by `display -p ok` in one invocation, with the config in the hermetic `HOME`; `OWNER_STARTUP_N` (20) times | How long the config takes to source: the `display` is not run until the config has been. |
| `throughput` | a pane `cat`s 100 MB of pre-generated log text (about 100-character lines), plain and with about 20% of the bytes SGR, then prints a done marker; no client attached | Parser and grid cost for heavy output with fixed work. Wall time and server CPU are for the same work for both binaries. |

Owner scenarios (`owner_idle`, `owner_startup`; Linux only, skipped elsewhere with the reason): `HOME` is `<tmp>/<run>/home`, holding a copy of `~/.tmux.conf` (symlinks followed), `~/.tmux/plugins` and `~/.local/share/mu-crew-dotfiles` (without `.git`, `target` or `node_modules`); their git revisions and the config's sha256 go into the output. Two changes to the copy: `@fingers-skip-wizard 1` first (no install wizard) and `@mu_crew_probes` set to `uptime`, the cheapest probe. `<tmp>/<run>/bin/tmux` is a symlink to the binary under test and first on `PATH` for both binaries (the config, TPM and the poller all run `tmux` from `PATH`; vtmux is run as `tmux`, D20). After each run the bench waits up to 5 s for every process whose environment has the hermetic `HOME` (the poller leaves when its server is gone), kills what is left and prints how many it had to kill; processes are matched by environment, never by name, so neither the bench's shell nor the owner's real poller can match. The poller's CPU is `utime+stime+cutime+cstime` of its pid from the pidfile, which includes the `tmux` clients it ran; it is outside the server and reported next to it, not added to it.

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

## Regression guard

`python3 scripts/bench.py --smoke` (`scripts/bench_smoke.py`) is a short run (about 1 minute on an idle host, under 3 minutes anywhere) that fails when vtmux got worse. It is not part of `check.sh`. In CI it is the `perf-smoke` job in `ci.yml`: Linux only, `continue-on-error` (it never blocks a merge), also run weekly (`schedule:`, Mondays 04:17 UTC), sharing the zig/rust/ref-tmux setup with the `check` job through `.github/actions/setup`. It writes its table to `$GITHUB_STEP_SUMMARY` and uploads `smoke.json` as the `perf-smoke` artifact.

| metric | scenario | limit |
|---|---|---|
| `idle_rss_mb` | idle server, one session | baseline + max(25%, 8 MB) |
| `scale_rss_mb` | 400 idle panes | baseline + max(25%, 8 MB) |
| `history_spec_rss_mb` | `bench-history-memory.sh spec` | baseline + max(25%, 8 MB) |
| `cpu_ratio_plain`, `cpu_ratio_sgr` | throughput without a client (30 MB, median of 3): server CPU of vtmux / tmux 3.8 | baseline ratio x 1.5 |

- **RSS** is compared with `perf/baseline.json`, recorded on the CI runner (RSS depends on the allocator and libc, so numbers from a developer machine don't carry over). For one binary and one input it is nearly deterministic; the 8 MB floor keeps the small idle numbers from tripping on noise.
- **CPU** is never compared with a checked-in absolute time: runner speed varies between runs and SKUs. It is gated as the vtmux/tmux 3.8 CPU ratio measured in the same run, which cancels the machine's speed, against the ratio in the baseline. The alternative, `valgrind --tool=cachegrind` instruction counts of a foreground server (tighter, +10%), was not chosen: it needs a valgrind install, runs the server on one traced thread rather than as deployed, and doesn't exist on macOS. The ratio's coarse +50% threshold only catches a real slowdown, not drift.
- **Local runs** work on any Linux host (`BASELINE=<file>` points at another baseline; without `perf/baseline.json` the run only records). A loaded host (load >= 4) makes the ratios noisy; the script warns.

### Refreshing the baseline

After an intentional change that moves a metric (a feature that costs memory, a compression that saves it): take `smoke.json` from the `perf-smoke` artifact of a CI run on the commit, and commit its `metrics` as `perf/baseline.json` (`{"metrics": {...}}`; the whole file works too). Put the reason in the commit message ("perf baseline: idle RSS +1.2 MB, the per-pane foo table"). Never refresh to make a red run green without a reason; a regression is fixed or explained. The first baseline is whatever the first run on `main` produced.

### Staleness

The numbers above and in the README come from a full run at the commit recorded in "Numbers". When that commit is older than the last change to `perf/baseline.json` (a refresh means the metrics moved), the numbers are stale: the smoke run prints a `STALE:` line, and the next full run replaces them.

### Checked with injected regressions

Scratch builds (not committed): a 50 MB `vec![0u8; N]` per pane (for the first three panes, one byte written in every 4 KiB page so the pages are resident) and a 20-million-iteration busy loop per output chunk, run against a baseline recorded from the same host and the good build:

| metric | baseline | regressed | result |
|---|---|---|---|
| `idle_rss_mb` | 7.4 MB | 57.5 MB | fail |
| `scale_rss_mb` | 13.4 MB | 163.4 MB | fail |
| `history_spec_rss_mb` | 105.5 MB | 205.4 MB | fail |
| `cpu_ratio_plain` | 0.32 | 109 | fail |
| `cpu_ratio_sgr` | 0.89 | 84 | fail |
