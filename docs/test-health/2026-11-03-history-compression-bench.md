# History compression benchmark 2026-11-03 (hist_compress_bench)

Spec: `docs/specs/2026-10-31-history-memory.md`, criteria 1, 3 and 6. Base: main `5de1824`
(`hist_compress` merged). Host: Fedora 44, Linux 7.2.11 x86_64, 16 cores, warm cache, release builds,
reference tmux 3.8 from `cargo xtask ref-tmux`. Every run was started under
`flock /tmp/vtmux-stress.lock` once the 1-minute load was below 4; the load at each run's
start is given below, and the latency pairs list their range (a pair can drift above 4 while it runs).
Everything is driven by `scripts/bench-history-memory.sh` (new modes: `copymode`, `steps`, `latency`;
the default mode gained the `rss_out_mb`, `settled_mb` and `settle_s` columns).

## Criterion 1: RSS after idle compression, against tmux 3.8

`IDLE=10 bash scripts/bench-history-memory.sh spec narrow wide`, load 3.97 at start (4.73 at the end),
2 runs on the same builds (first run: load 3.93, spec 42.2 MB at 10 s, narrow 25.2, wide 9.3, same settled
numbers). Columns: `rss_mb` is the RSS 10 s after the output stopped (the criterion's measurement),
`rss_out_mb` 0.5 s after it (nothing compressed yet), `settled_mb` once the RSS stopped moving (at most 512 KB
between polls for 5 s), `settle_s` the seconds from the end of the output to that point.

| scenario | binary | size | limit | RSS at 10 s idle MB | hist_size | retained | RSS after output MB | settled MB | settle s |
|---|---|---|---|---|---|---|---|---|---|
| spec (150k log lines) | tmux 3.8 | 120x40 | 100000 | 66.0 | 99962 | - | 66.0 | 66.0 | 15 |
| | vtmux | | | **36.6** (42.2) | 99962 | 100039 | 105.6 | **17.6** | 17 |
| narrow (12k lines, truecolour) | tmux 3.8 | 80x24 | 10000 | 25.4 | 9978 | - | 25.4 | 25.4 | 15 |
| | vtmux | | | **24.8** (25.2) | 9978 | 10244 | 27.8 | **17.6** | 20 |
| wide (12k lines of 450 chars) | tmux 3.8 | 500x24 | 10000 | 29.0 | 9978 | - | 29.0 | 29.0 | 15 |
| | vtmux | | | **9.2** (9.3) | 9978 | 10025 | 47.3 | **9.2** | 15 |

**Criterion 1 holds.** At the spec scenario vtmux is at 36.6-42.2 MB 10 s after the output stops against
tmux's 66.0 MB (0.55-0.64x); settled it is 17.6 MB (0.27x). The 10 s number is not the floor: compression
starts 5 s after the output stops (`compress::IDLE`) and one pass over 100k lines takes 8-12 s of wall time
because the loop rests 100x each step's time (`REST_FACTOR`), so at 10 s the pass is mid-way and the figure
moves between runs (36.6 and 42.2 MB). Narrow panes: 24.8 MB at 10 s (tmux 25.4) and 17.6 settled. Wide
panes: 9.2 MB against 29.0. vtmux's RSS right after the output (105.6 MB at the spec scenario) is still
1.6x tmux's, as `docs/PERFORMANCE.md` says; compression reclaims it later. Peak RSS is not reduced.

## Criterion 3: `copy-mode` entry time, compressed against uncompressed history

`bash scripts/bench-history-memory.sh copymode spec narrow`, load 0.34 at start (0.58 at the end), 3 runs;
the table shows medians. One server per state, since a history read restores the pages for good: the
"uncompressed" entry comes right after the output (before the 5 s idle delay, RSS 105 MB = nothing compressed),
the "compressed" entry after the RSS stopped moving (17.5 MB). `noise_ms` is the fastest of five `display -p ok`
round trips (process start and a command round trip). Time is wall time of the `copy-mode -t b` command itself.

| scenario | binary | limit | noise ms | uncompressed ms | compressed ms | ratio | RSS before entry MB (uncompressed / compressed) |
|---|---|---|---|---|---|---|---|
| spec (100k history) | tmux 3.8 | 100000 | 4 | 42 | 43 | 1.02 | 66.1 / 66.1 |
| | vtmux | | 3 | 4269 | 4378 | **1.03** | 105.4 / 17.5 |
| narrow (10k history) | tmux 3.8 | 10000 | 4 | 16 | 16 | 1.00 | 25.4 / 25.4 |
| | vtmux | | 3 | 125 | 159 | **1.27** | 27.7 / 17.4 |

Runs (ms, uncompressed / compressed): spec vtmux 4366/4324, 4185/4378, 4269/4466; narrow vtmux 126/156,
123/159, 125/159.

**Criterion 3 holds**: 1.03x (spec) and 1.27x (narrow) against the 2x bar. `capture-pane -S - -E -`
byte-identity across compression is checked by `compress_steps` (below, `identical` is true on every
scenario) and, end to end through the server, by `hist_compress_r3` review (spec and narrow, `cmp` identical).

Not a criterion, but visible in this table: **vtmux's `copy-mode` entry on 100k lines of history takes 4.3 s,
about 100x tmux's 43 ms**, compressed or not; the cost is `Grid::clone_of` reading every row at entry (the
spec's builder check: "copy mode copies the whole history at entry"; a lazy grid is out of scope there). 10k
lines cost 125 ms against 16 ms. A first draft of this mode that entered copy mode three times per server
(cancel between) got wrong compressed numbers, because an entry restores the pages; hence one entry per server.

## Criterion 6: threading gate

### Step budget (at most 5 ms per step)

`bash scripts/bench-history-memory.sh steps spec narrow wide` (`compress_steps` example, one `PaneTerm`, steps as
the server takes them), load 3.05 at the end of a run that started at 3.82:

| scenario | steps | max step ms | total ms | compressed pages | in-process RSS before -> after MB | capture identical |
|---|---|---|---|---|---|---|
| spec | 291 | 0.219 | 48.7 | 258 | 146.4 -> 58.4 | true |
| narrow | 344 | 1.337 | 66.3 | 17 | 64.7 -> 54.6 | true |
| wide | 121 | 0.097 | 8.8 | 107 | 58.1 -> 20.1 | true |

Passes (worst 1.34 ms, bound 5 ms). The in-process RSS includes the captured strings of the identity check.

### Busy-pane latency (p95 rise below 10%)

`NOCOMPRESS=<build> PAIRS=5 bash scripts/bench-history-memory.sh latency spec narrow`: for each scenario and
GAP_MS, alternating runs (`python3 scripts/bench-compress-latency.py BIN SCENARIO 2 16`, 16 s each) of the
branch's `vtmux` and a build without compression, in swapped order on every other pair. The build without
compression is `compress_turn` and `PaneCompress::observe` returning at once (`scripts/bench-nocompress.patch`,
applied to the same commit; same compiler, same code layout otherwise). GAP_MS 2 is the gate command of
`hist_compress_r3`: the pane is never quiet for 50 ms, so nothing is compressed and the result is the cost of the
compression-capable build alone. GAP_MS 60 leaves quiet gaps, so compression runs meanwhile (the `compress`
subcommand finds the window from the RSS timeline). Load at start 0.49, 0.42 at the end; the load at the start of each run was 0.3-2.7.
"Whole run" is every sample from 0.5 s on, pooled over the runs of a side.

| scenario | GAP_MS | pairs | with: p50 / p95 ms | without: p50 / p95 ms | whole-run rise p50 / p95 | inside the compression window: p95 rise |
|---|---|---|---|---|---|---|
| spec | 2 | 5 | 0.967 / 1.204 | 0.976 / 1.221 | -0.9% / **-1.4%** | none (no window) |
| spec | 60 | 5 | 1.229 / 1.482 | 1.206 / 1.385 | +1.9% / **+7.0%** | **+6.7%** (1.475 vs 1.383 ms, n 894, window 11.0 s) |
| narrow | 2 | 5 | 0.522 / 0.683 | 0.485 / 0.621 | +7.6% / +10.0% | none |
| narrow | 2, rerun | 8 | 0.550 / 0.693 | 0.549 / 0.694 | +0.2% / **-0.1%** | none |
| narrow | 60 | 5 | 0.752 / 0.892 | 0.748 / 0.865 | +0.5% / **+3.1%** | **+2.3%** (0.886 vs 0.866 ms, n 708, window 8.6 s) |

Reruns of the GAP_MS 2 whole-run comparison (same command, alternating, swapped order every other round), because the
first narrow figure was at the bound and the first spec/narrow runs of the day disagreed:

| scenario | pairs | load at start / range | with p50 / p95 ms | without p50 / p95 ms | whole-run rise p50 / p95 |
|---|---|---|---|---|---|
| narrow | 8 | 0.24 / 0.24-3.61 | 0.550 / 0.693 | 0.549 / 0.694 | +0.2% / -0.1% |
| spec | 8 | 1.98 / 1.39-3.11 | 0.956 / 1.148 | 0.998 / 1.256 | -4.2% / -8.6% |
| spec, three builds, 6 rounds (with / no-compression v1 / v2) | 6 | 3.81 / 2.0-3.8 | 0.979 / 1.162 | v1 0.896 / 1.095, v2 0.971 / 1.128 | v1 +9.3% / +6.2%, v2 +0.9% / +3.0% |

"No-compression" v1 and v2 are two builds of the same patch variant (v1 only stubs `compress_turn`, v2 also stubs
`observe`, the shipped `scripts/bench-nocompress.patch`). They differ by 8% in p50 from each other's layout alone, so
a build-to-build difference below about 10% is not distinguishable from code layout on this host.

**Verdict.** Criterion 6 (p95 rise below 10%) holds on the larger samples and fails on some small ones:

- Held, whole run, GAP_MS 2: spec -1.4% (5 pairs) and -8.6% (8 pairs); narrow -0.1% (8 pairs).
- Held, GAP_MS 60 (compression runs): spec +7.0% whole run, +6.7% inside the window; narrow +3.1% and +2.3%.
- Failed on small samples: narrow GAP_MS 2, 5 pairs, +10.0% p95 (+7.6% p50), rerun with 8 pairs -0.1%. An
  earlier 4-pair run (load 1.2-4.0) gave spec GAP_MS 2 +16.4%, GAP_MS 60 +18.2% whole run and +50.2% inside the
  window; narrow GAP_MS 60 +4.8% and +26.1% inside. Those 4-pair runs compared against the first stub build (v1,
  which only stubs `compress_turn`), whose layout alone differs from v2 by 8% in p50 (table above); the
  5-pair reruns against the shipped patch gave +6.7% and +2.3%. The 3-build rotation against v1 also read +6.2%
  (p95) and against v2 +3.0%.
- Per-run p95 of the same build and scenario scatters by 20-40% (0.86-1.29 ms), so 4 pairs cannot resolve a 10%
  bound; the script's `PAIRS` default is 4 and a gate run should use 5 or more. I cannot rule out a real
  few-percent cost of the compression-capable build; the measurements put it between -9% and +7%.

No worker thread is needed on these numbers.

## Reading

- Criterion 1 holds, with margin once the pass is done (0.27x tmux at the spec scenario) and at the 10 s mark
  (0.55-0.64x).
- Criterion 3 holds (1.03x and 1.27x). The surprising number is absolute: entering copy mode on 100k lines of
  history costs vtmux 4.3 s against tmux's 43 ms, compressed or not. That is the whole-history copy at entry
  (`Grid::clone_of`), outside this task; it is reported to the orchestrator as a finding, not fixed here.
- Criterion 6 holds for steps (worst 1.34 ms against 5 ms) and, on the larger samples, for latency.
- Peak RSS is not reduced: RSS right after the output is 105.6 MB at the spec scenario, 1.6x tmux. Only idle
  memory drops. A flood that never goes quiet for 50 ms (the `GAP_MS=2` workload) never compresses.
- `perf/baseline.json` (`history_spec_rss_mb` 105.7) was not touched: it is refreshed from a CI runner's
  `perf-smoke` artifact (docs/PERFORMANCE.md, *Refreshing the baseline*), not from a developer machine.
