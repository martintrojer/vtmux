# Performance

vtmux's goal is to be fast and lean, and that is only claimed where it is measured. This page has the numbers against tmux 3.8, what each scenario measures, and how to reproduce them. Raw runs live in [`docs/test-health/`](test-health/).

The first five scenarios have no attached client; `throughput_attached`, `render` and `many_clients` attach real clients (see *Attached clients* below).

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

Run of 2026-11-02 on the perf_owner branch (147e54b, before rebase onto main; same host as above, vtmux release build of that commit), load 3.9 at start, median of 3 runs for `owner_idle` and 20 runs for `owner_startup`. The owner's real `~/.tmux.conf` (`tmux.conf` at dotfiles `6430744` with local edits, sha256 `c1ffe189d326`), the TPM plugins and mu-crew-dotfiles `d633e15` are copied into a hermetic `HOME`; the plugin revisions are in the raw run. Raw output: [`test-health/2026-11-02-perf-owner.md`](test-health/2026-11-02-perf-owner.md) and `.json`.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| owner_idle, 1 session, 60 s | RSS | 4.6 MB | 8.0 MB | 1.7x |
| | server CPU over 60 s | 187 ms | 48 ms | 0.26x |
| | server wakeups | 4.1 / s | 1.0 / s | 0.26x |
| | poller CPU over 60 s (not the server) | 180 ms | 220 ms | 1.2x |
| owner_startup, `start-server` until `display -p ok` answers, 20x | median (min - max) | 62.9 ms (58.9 - 74.1) | 61.8 ms (58.3 - 80.5) | 0.98x |

Attached-client run of 2026-11-02, commit `4a1bcf4` (render_flood_cpu round 2), same host, load 2.0 at start and 1.1 at the end, median of 3. Raw output: [`test-health/2026-11-02-render-flood-r2.md`](test-health/2026-11-02-render-flood-r2.md) and `.json`, with the key-echo latency run. The last column is the run before `render_flood_cpu` (commit `96dc4ad`, [`test-health/2026-11-02-perf-attached.md`](test-health/2026-11-02-perf-attached.md)); it was taken at load 7-9 and before `status-right` and `status-interval` were pinned, so only its ratios are comparable, not its absolute times. Server CPU only; the client's CPU is in the raw run (at most 0.02 s for either binary). Terminal bytes are per input byte, or per synchronized-update frame where sync is on (total terminal bytes / `ESC[?2026h` count).

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux | before `render_flood_cpu` |
|---|---|---|---|---|---|
| throughput_attached, 100 MB plain log, 200x50 client, sync off | wall / server CPU | 2.84 s / 2.72 s | 1.44 s / 1.44 s | 0.53x CPU | 6.0x |
| | terminal bytes per input byte | 1.03 | 0.05 | 0.05x | 2.15 |
| throughput_attached, plain, sync on | wall / server CPU | 2.84 s / 2.76 s | 1.45 s / 1.42 s | 0.51x | 6.7x |
| | sync frames / bytes per frame | 25815 / 3.9 KB | 133 / 40 KB | | |
| throughput_attached, 19% SGR, sync off | wall / server CPU | 4.06 s / 3.85 s | 3.69 s / 3.48 s | 0.90x | 9.8x |
| | terminal bytes per input byte | 1.04 | 0.13 | 0.12x | 5.72 |
| throughput_attached, 19% SGR, sync on | wall / server CPU | 4.06 s / 3.83 s | 3.68 s / 3.51 s | 0.92x | 7.0x |
| | sync frames / bytes per frame | 25776 / 4.0 KB | 271 / 45 KB | | |
| render, 4 panes x 3000 cursor-addressed frames (22 MB), sync off | wall / server CPU | 0.66 s / 0.63 s | 0.45 s / 0.40 s | 0.63x | 2.4x |
| | terminal bytes | 10.1 MB | 0.49 MB | 0.05x | 4.9 MB |
| render, sync on | wall / server CPU | 0.66 s / 0.64 s | 0.46 s / 0.41 s | 0.64x | 2.3x |
| | sync frames / bytes per frame | 1357 / 7.3 KB | 34 / 15 KB | | |
| | peak RSS | 5.4 MB | 11.4 MB | 2.1x | 11.9 MB |
| many_clients, 8 clients, 20 MB flood in one pane, sync off | wall / server CPU | 1.43 s / 1.28 s | 0.87 s / 0.76 s | 0.59x | 11x |
| | terminal bytes (sum) | 29.1 MB | 2.2 MB | 0.08x | 61.9 MB |
| many_clients, sync on | wall / server CPU | 5.17 s / 5.10 s | 0.87 s / 0.83 s | 0.16x | 2.8x |
| | RSS attached idle / after flood (sync off) | 6.5 / 7.3 MB | 14.0 / 17.7 MB | 2.2x / 2.4x | 14.9 / 16.5 MB |

Key-to-echo latency with a client attached (the reviewer's `lat.py`: 200x50 client, window split in two, `yes` in one pane, a raw echo program in the other, 60 keys 30 ms apart, two runs each; same load):

| | tmux 3.8 p50 / p90 / max | vtmux p50 / p90 / max |
|---|---|---|
| a flood in the other pane | 0.8 / 0.8 / 1.0 ms | 2.8-3.1 / 5.9-7.0 / 7.9-8.2 ms |
| no flood | 0.3 / 0.6 / 1.0 ms | 4.7-5.0 / 5.1-5.9 / 5.5-6.4 ms |

The history rows come from `scripts/bench-history-memory.sh` (details in [`test-health/2026-11-01-history-memory.md`](test-health/2026-11-01-history-memory.md); the output-time column is in the raw run).

## Reading the numbers

- **CPU: a win when consuming output, a loss when creating panes.** With no client, vtmux consumes 100 MB of pane output with 0.46x (plain) and 0.78x (19% SGR) of tmux's server CPU, and in 1.2x to 2.2x less wall time. The idle server uses no CPU and takes no wakeups, same as tmux. Creating 400 panes costs 1.6x more CPU (430 ms against 270 ms). Startup is a wash: `new -d` is 7.0 ms against 9.1 ms, and the server pid is gone 2.4 ms after `kill-server` starts for both.
- **Throughput wall time is coarse.** The done marker is polled with `capture-pane` every 0.2 s, so each wall time is accurate to about 0.2 s (up to 17% of vtmux's 1.2 s plain run). The error adds about the same to both binaries, which pulls the ratio toward 1, so the speed-up is if anything understated. Server CPU is not affected by the polling beyond the cost of the `capture-pane` calls themselves.
- **Memory is the cost.** vtmux runs one libghostty terminal per pane, so every pane pays for Ghostty's page allocation up front: 33 KB per idle pane against tmux's 5.4 KB, and 3 MB more for a bare server. Both are small in absolute terms, but at 400 panes the ratio is 3x. Filed as `perf_scale_rss`.
- **Scrollback.** Ghostty stores 8 bytes for every cell of every retained row, where tmux stores only the used cells, so wide and mostly-empty rows cost more (1.6x on plain logs and on 500-column rows). `hist_retain` already fixed the retention count (the line limit follows `history-limit`; 148 MB before it, 105 MB now). The rows above are the RSS right after the output. `hist_compress` compresses idle scrollback (5 s after the output stops, then a pass of 8-12 s for 100k lines); `bench-history-memory.sh` run of 2026-11-03 ([`test-health/2026-11-03-history-compression-bench.md`](test-health/2026-11-03-history-compression-bench.md)) measured 36.6 MB (spec), 24.8 MB (narrow) and 9.2 MB (wide) ten seconds after the output against tmux's 66.0, 25.4 and 29.0 MB, and 17.6, 17.6 and 9.2 MB once settled. `copy-mode` entry on 100k lines took 4.3 s then (every row read at entry); since `copy_mode_entry_slow` it copies Ghostty's pages (patch 0005) and reads rows on demand: 59 ms uncompressed and 13 ms compressed (compressed pages are copied without decoding) against tmux's 42 and 44 ms (`bench-history-memory.sh copymode spec`, 2026-11-03).
- **Attached, vtmux draws fewer, larger frames than tmux under a flood.** vtmux composes whole frames (D14), so its cost follows how many frames it draws, not how much output arrives. Before `render_flood_cpu` it drew one full frame per loop turn, about one per 18 KB of pane output, and a 100 MB flood cost 6x to 10x tmux's server CPU; per-phase timers showed 94% of that in building and diffing whole frames. Now a frame waits while the client still has an unwritten one (tmux defers its redraw the same way, `server_client_check_redraw`), and while a pane still has output waiting, frames of that output go at most every 10 ms. The flood costs 0.5x (plain) to 0.9x (SGR) of tmux's server CPU. With sync on, where frames can be counted, vtmux sends 133 frames of about 40 KB for the plain log where tmux sends 25815 updates of about 4 KB; per input byte that is 0.05 against 1.03. With 8 clients it is 0.6x (sync off) and 0.16x (sync on). What remains in the SGR row is mostly Ghostty parsing the input (the no-client throughput row is 2.49 s). Sync-off rows have no frame count.
- **Attached, diffed frames do show in the bytes.** In `render`, where each pane repaints full screens with cursor addressing, vtmux sends 0.49 MB against tmux's 10.1 MB at 0.6x the server CPU. Each frame on the terminal is the latest picture, so the intermediate repaints a pane made within one frame interval never reach the terminal; the final screen is the same.
- **Key echo is slower than tmux's, flood or not.** Only flood output is paced: a frame that also shows output from a pane that was not flooding (a key's echo), a pane to redraw whole or a status redraw goes at once. Next to a flood the echo takes 3 ms (p50) and at most 8 ms against tmux's 0.8 ms; with the pacing applied to every frame it was 8-10 ms p50 and 16.5 ms max. Without any flood vtmux takes 5 ms against 0.3 ms at 200x50: that is composing and diffing a whole frame for one changed cell, which is the cost of D14, not of pacing, and is not addressed here.
- **Peak RSS in throughput** is about 5 MB above tmux's: the per-pane terminal plus the history it keeps for the 100 MB of output, capped by the default `history-limit` of 2000.

- **With the owner's config the idle server is not idle, and the cause is outside it.** The mu-crew poller wakes about once a second and runs `tmux show-options -g` (and `set-option` when a value changes) as clients of the server, so the server's 187 ms (tmux) and 48 ms (vtmux) over 60 s are the cost of answering those clients: about 3 ms against 0.8 ms per wake-up round. vtmux does less server work per request here (0.26x); the RSS gap is the same 3 MB as for a bare server (1.7x). The poller's own CPU (a Python process plus the `tmux` client processes it starts, counted from `/proc/<pid>/stat` with children) is 180 ms and 220 ms: it is outside the server. The 40 ms difference between the two was not investigated (3 runs, shared host). Only the cheapest probe (`uptime`, a `/proc` read every 60 s) is on; the owner's real list adds `murmur_remote memory cpu uptime muzzle`, which starts `murmur status` and `muzzle status` every 5 to 10 s and is not measured here because it needs a network and the owner's state.
- **Sourcing the owner's config takes 62 ms for both.** A bare `new -d` takes 7 to 9 ms (table above), so the rest is the config; where it goes (the config's synchronous `run-shell` calls, such as `mu-crew-bind`, each start a shell and several `tmux` clients) was not profiled. The 20 runs overlap for the two binaries (58 to 74 ms and 58 to 80 ms).

## Scenarios

`scripts/bench.py` runs each scenario against both binaries (and `BASE=<binary>`, shown as `base`, to compare against an earlier build).

| scenario | what it does | why |
|---|---|---|
| `idle` | `-f /dev/null`, one session running `cat`, 2 s to settle, then `IDLE_SECS` (60) of nothing | A multiplexer sits idle most of the time: baseline RSS, CPU and wakeups. |
| `scale` | 10 sessions x 10 windows x 4 panes (400 idle `cat` panes, 200x50 windows) | Per-pane cost, where the per-pane libghostty terminal shows up. RSS per pane is (RSS after - RSS before) / 399. |
| `history` | calls `scripts/bench-history-memory.sh` (spec, narrow, wide; its `copymode`, `steps` and `latency` modes are run by hand) | Scrollback memory; see the history-memory spec. |
| `startup` | `new -d` then `kill-server`, 100 times on a fresh socket each time | How fast a script's first command and its cleanup are. |
| `owner_idle` | the owner's config in a hermetic `HOME`, one session running `cat`, 10 s to settle, then `IDLE_SECS` (60) of nothing. Reports server RSS, CPU and wakeups, and the poller's CPU separately | What the owner's server costs at rest with the config, TPM plugins and the mu-crew poller running. |
| `owner_startup` | `start-server` followed by `display -p ok` in one invocation, with the config in the hermetic `HOME`; `OWNER_STARTUP_N` (20) times | How long the config takes to source: the `display` is not run until the config has been. |
| `throughput` | a pane `cat`s 100 MB of pre-generated log text (about 100-character lines), plain and with about 20% of the bytes SGR, then prints a done marker; no client attached | Parser and grid cost for heavy output with fixed work. Wall time and server CPU are for the same work for both binaries. |
| `throughput_attached` | the `throughput` file, in a new window while a 200x50 client is attached: wall, server and client CPU, terminal bytes per input byte, sync frames, peak RSS; plain and SGR, sync on and off | What a flood costs when somebody is watching: parse plus render. |
| `render` | 4 tiled panes in a 200x50 client, each `cat`ing a pre-generated file of 3000 cursor-addressed 95x22 frames (about 5.5 MB per pane, fixed work, done marker): server and client CPU, terminal bytes | Where D14 diffed frames should show against tmux's redraw. |
| `many_clients` | 1 session, 2 panes, 8 clients of different sizes (80x24 to 240x60), `window-size smallest` for both, a 20 MB log in one pane: server CPU and RSS, terminal bytes (sum) | Per-client fan-out cost. |

Owner scenarios (`owner_idle`, `owner_startup`; Linux only, skipped elsewhere with the reason): `HOME` is `<tmp>/<run>/home`, holding a copy of `~/.tmux.conf` (symlinks followed), `~/.tmux/plugins` and `~/.local/share/mu-crew-dotfiles` (without `.git`, `target` or `node_modules`); their git revisions and the config's sha256 go into the output. Two changes to the copy: `@fingers-skip-wizard 1` first (no install wizard) and `@mu_crew_probes` set to `uptime`, the cheapest probe. `<tmp>/<run>/bin/tmux` is a symlink to the binary under test and first on `PATH` for both binaries (the config, TPM and the poller all run `tmux` from `PATH`; vtmux is run as `tmux`, D20). After each run the bench waits up to 5 s for every process whose environment has the hermetic `HOME` (the poller leaves when its server is gone), kills what is left and prints how many it had to kill; processes are matched by environment, never by name, so neither the bench's shell nor the owner's real poller can match. The poller's CPU is `utime+stime+cutime+cstime` of its pid from the pidfile, which includes the `tmux` clients it ran; it is outside the server and reported next to it, not added to it.

How it measures: the server pid comes from `display -p '#{pid}'` on a private socket (`-L` and `TMUX_TMPDIR`), and is checked gone after `kill-server`; pids are never found by name. Only the server process is measured, never its pane children.

- **Linux CPU:** `/proc/<pid>/stat` utime+stime (fields 14 and 15) deltas per phase, which count all threads and exclude children. Idle and the attached scenarios use the nanosecond `schedstat` sum, because 10 ms ticks are too coarse. Never `cutime`/`cstime`, `getrusage` of children or `time(1)`: those include the reaped pane processes.
- **Linux RSS:** settled `VmRSS`, and the per-phase peak is `VmHWM` after `echo 5 > /proc/<pid>/clear_refs`.
- **Wakeups:** deltas of `voluntary_ctxt_switches` over all threads.
- **macOS:** `ps -o rss=,time=` sampled every 100 ms (peak RSS is the largest sample; wakeups are not reported). `scale` raises `ulimit -n` to 4096, or is skipped with the reason if it can't. The macOS numbers have not been collected yet.

### Attached clients

The client is the binary's own `attach` on a PTY the bench drains with a Python thread (`Client` in `scripts/bench.py`; not `script(1)`, whose flags differ between Linux and macOS). The thread counts every byte the terminal would receive and every synchronized-update begin (`ESC[?2026h`); client CPU is read from `/proc/<client pid>` like the server's. For a fair comparison both binaries get the same `TERM` (xterm-256color), the same explicit `default-terminal`, `terminal-features`, `window-size`, `status-right` and `status-interval`. Both servers enable synchronized output only from the terminal's `DECRQM 2026` reply (vtmux: `input.rs:215`, `render.rs:81`; tmux 3.8: `tty_keys_sync`), so each scenario runs twice: the reader answers the query (`sync on`) or never does (`sync off`), and the bench checks that the query arrived before it trusts a sync-on row. Measurement starts after all clients have attached and the output went quiet, and ends when the done marker shows and the terminals have been quiet for 0.3 s.

## Running

```
mise exec -- python3 scripts/bench.py                 # all scenarios; Markdown on stdout, JSON and Markdown under target/bench/
mise exec -- python3 scripts/bench.py idle scale      # chosen scenarios
IDLE_SECS=5 RUNS=1 STARTUP_N=5 THROUGHPUT_MB=10 RENDER_FRAMES=200 MANY_MB=5 mise exec -- python3 scripts/bench.py   # quick development run (about 1 min)
BASE=/path/to/older/vtmux mise exec -- python3 scripts/bench.py
```

A full run takes about 20 minutes. The host is shared: run it when `uptime` load is under 4, under `flock /tmp/vtmux-stress.lock`, and one bench at a time. Other env: `RUNS`, `OUT`, `NO_BUILD=1`, `VTMUX`, `RENDER_FRAMES` (3000), `MANY_MB` (20), `VTMUX_REFERENCE_TMUX`, `HISTORY_SCENARIOS` (documented at the top of the script). Put the result in `docs/test-health/<date>-perf.md` and update the table above.

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
