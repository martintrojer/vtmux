# Performance refresh, 2026-11-05 (perf_refresh_full_bench_py_rerun_after)

Every row of `docs/PERFORMANCE.md`'s tables comes from the runs below. Raw JSON (the full `bench.py` run and
`trickle`): [`2026-11-05-perf-refresh.json`](2026-11-05-perf-refresh.json).

- **Code:** vtmux main `ada47ba` (all the perf work of 3-5 November: `copy_mode_entry_slow`, `key_echo_latency`,
  `perf_scale_rss`, `hist_compress` and `hist_compress_busy`, `perf_agent_panes`, `lows_render`, control
  backpressure) plus `9ae86d2`, which only adds bench modes (`bench.py key_echo`; `bench-history-memory.sh` scenario
  `big`, mode `session`, and search / history-top timing in `copymode`). One release build, copied to
  `/tmp/pr178/vtmux`, measured with `NO_BUILD=1 VTMUX=/tmp/pr178/vtmux`. Main later moved to `2b9d8a9` (a test and
  a DIVERGENCES line only; no server code), and `9ae86d2` was rebased onto it as `8b43983`.
- **Reference:** tmux 3.8 from `cargo xtask ref-tmux` (`~/.cache/vtmux/tmux-3.8/bin/tmux`).
- **Host:** devserver, CentOS Stream 9, Linux 6.16.1, AMD EPYC-Genoa, 96 cpus, 235 GB RAM, shared with other
  agents. Every step ran under `flock /tmp/vtmux-stress.lock`, one after another, from `/tmp/pr178/drive.sh`.
  Within a step the binaries alternate the way the harness runs them (`bench.py`: all tmux runs of a scenario, then
  all vtmux runs; `bench-history-memory.sh`: tmux then vtmux per scenario; hidden panes: tmux, vtmux, four times).
- **Load** (`uptime` 1-minute average at the start and end of each step):

| step | UTC | load at start / end |
|---|---|---|
| full `bench.py` (all default scenarios, RUNS=3) | 21:57-22:29 | 3.0 / 4.9 |
| `bench-history-memory.sh spec big` | 22:29-22:31 | 4.9 / 3.6 |
| `bench-history-memory.sh session`, twice | 22:31-22:35 | 3.6 / 4.7 / 2.6 |
| `bench-history-memory.sh copymode spec big` | 22:35-22:45 | 2.6 / 3.2 |
| `bench-hidden-panes.py`, 4 x tmux and vtmux alternating | 22:45-22:47 | 2.8-3.6 |
| `bench.py trickle` | 22:47-23:09 | 3.3 / 3.1 |
| agent-panes probe (below) | 23:12-23:13 | 4.3 / 2.6 |

Commands (each prefixed by `flock /tmp/vtmux-stress.lock mise exec --`, with `NO_BUILD=1 VTMUX=/tmp/pr178/vtmux`):

```
RUNS=3 OUT=/tmp/pr178/logs/full python3 scripts/bench.py
RUNS=1 IDLE=10 SETTLE=900 DEADLINE=1200 bash scripts/bench-history-memory.sh spec big
IDLE=10 SETTLE=900 bash scripts/bench-history-memory.sh session          # twice
COPIES=3 SETTLE=900 DEADLINE=1200 bash scripts/bench-history-memory.sh copymode spec big
python3 scripts/bench-hidden-panes.py <binary> rvh<pid><t|v><n>          # 4 x tmux, vtmux
OUT=/tmp/pr178/logs/trickle python3 scripts/bench.py trickle
```

## Full `bench.py` run

The run's own output, unchanged:

2026-11-05 21:57 UTC, commit 9ae86d2, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 3.0, median of 3; tmux: tmux 3.8; vtmux: vtmux 0.0.0 (tmux 3.8)

### idle (1 session, 60 s, -f /dev/null)

| binary | RSS MB | CPU ms | wakeups/s |
|---|---|---|---|
| tmux | 4.2 | 0.00 | 0.00 |
| vtmux | 9.0 | 0.00 | 0.00 |

### scale (10 sessions x 10 windows x 4 panes of idle cat)

| binary | RSS MB (400 panes) | RSS KB/pane | peak RSS MB | create CPU ms | create wall s |
|---|---|---|---|---|---|
| tmux | 6.4 | 5.4 | 6.4 | 200 | 0.23 |
| vtmux | 16.0 | 18.2 | 16.0 | 380 | 0.41 |

### history (bench-history-memory.sh, fastest of RUNS)

```
scenario binary      size   limit     rss_mb hist_size retained output_s rss_out_mb settled_mb settle_s
spec    tmux      120x40  100000       66.1     99962        -     0.46       66.1       66.1       15
spec    vtmux     120x40  100000       26.6     99962   100039     0.24      106.9       18.9       16
narrow  tmux       80x24   10000       25.9      9978        -     0.69       25.9       25.9       15
narrow  vtmux      80x24   10000       25.2      9978    10244     0.29       29.0       19.0       18
wide    tmux      500x24   10000       28.9      9978        -     0.19       28.9       28.9       15
wide    vtmux     500x24   10000       10.5      9978    10025     0.07       48.6       10.5       15
```

### startup (new -d + kill-server, 100x, -f /dev/null)

| binary | new -d ms | kill-server returns ms | pid gone ms | RSS MB |
|---|---|---|---|---|
| tmux | 12.2 | 2.0 | 2.5 | 4.3 |
| vtmux | 8.9 | 1.4 | 2.5 | 8.9 |

### throughput without a client (100 MB log text, 120x40 pane, default history; sgr bytes = 19%)

| input | binary | wall s | server CPU s | MB/s | peak RSS MB |
|---|---|---|---|---|---|
| plain | tmux | 2.44 | 2.38 | 42.9 | 5.4 |
| plain | vtmux | 1.02 | 0.89 | 102.9 | 11.8 |
| sgr | tmux | 3.26 | 3.07 | 32.2 | 5.4 |
| sgr | vtmux | 1.43 | 1.40 | 73.5 | 11.5 |

### owner_idle (owner config, 1 session, 60 s)

| binary | RSS MB | server CPU ms | server wakeups/s | poller CPU ms (outside the server) |
|---|---|---|---|---|
| tmux | 4.6 | 171.18 | 3.12 | 180 |
| vtmux | 9.7 | 47.48 | 1.05 | 190 |

The poller column is the mu-crew-poller process itself plus the `tmux` clients it ran, not the server.

Processes still carrying the hermetic HOME 5 s after kill-server (the bench SIGKILLed them), summed over all runs: 0

Config snapshot: tmux.conf 870c12a sha256:72a481ffac7e; mu-crew-dotfiles d633e15; plugin tmux-fingers-rs 5759d08; plugin tmux-fzf 2839d6b; plugin tmux-session-picker 1158ce2; plugin tmux-yank acfd36e; plugin tpm 7bdb7ca; plugin vim-tmux-navigator e41c431

### owner_startup (start-server with the owner config until `display -p ok` answers, 20x)

| binary | median ms | min ms | max ms | runs |
|---|---|---|---|---|
| tmux | 102.4 | 93.8 | 119.3 | 20 |
| vtmux | 101.5 | 86.4 | 117.4 | 20 |

Processes still carrying the hermetic HOME 5 s after kill-server (the bench SIGKILLed them), summed over all runs: 0

Config snapshot: tmux.conf 870c12a sha256:72a481ffac7e; mu-crew-dotfiles d633e15; plugin tmux-fingers-rs 5759d08; plugin tmux-fzf 2839d6b; plugin tmux-session-picker 1158ce2; plugin tmux-yank acfd36e; plugin tpm 7bdb7ca; plugin vim-tmux-navigator e41c431

### throughput_attached (100 MB log, one 200x50 client, window-size latest)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | term bytes / MB in | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| plain / sync off | tmux | 2.85 | 2.63 | 0.00 | 2.63 | 1034350 | 0 | 7.5 |
| plain / sync off | vtmux | 1.63 | 1.51 | 0.01 | 1.52 | 46741 | 0 | 16.8 |
| plain / sync on | tmux | 2.85 | 2.66 | 0.00 | 2.66 | 1038289 | 25816 | 7.5 |
| plain / sync on | vtmux | 1.83 | 1.64 | 0.02 | 1.66 | 53904 | 754 | 16.8 |
| sgr / sync off | tmux | 4.08 | 3.90 | 0.00 | 3.90 | 1043247 | 0 | 7.5 |
| sgr / sync off | vtmux | 1.83 | 1.65 | 0.00 | 1.66 | 13315 | 0 | 16.6 |
| sgr / sync on | tmux | 4.07 | 3.92 | 0.00 | 3.92 | 1047197 | 25776 | 7.5 |
| sgr / sync on | vtmux | 1.83 | 1.65 | 0.00 | 1.65 | 13485 | 157 | 16.7 |

### render (4 panes in a 200x50 client, each cat'ing 3000 cursor-addressed 95x22 frames; 22 MB of input)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | terminal MB | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 0.67 | 0.65 | 0.00 | 0.65 | 10.09 | 0 | 5.4 |
| sync off | vtmux | 0.22 | 0.19 | 0.00 | 0.19 | 0.07 | 0 | 13.4 |
| sync on | tmux | 0.67 | 0.63 | 0.00 | 0.63 | 10.11 | 1357 | 5.4 |
| sync on | vtmux | 0.22 | 0.20 | 0.00 | 0.20 | 0.08 | 23 | 13.5 |

### many_clients (1 session, 8 clients 80x24, 100x30, 120x40, 132x43, 160x50, 200x50, 90x35, 240x60, window-size smallest, 20 MB flood in one of 2 panes)

| case | binary | wall s | server CPU s | clients CPU s (sum) | total CPU s | terminal MB (sum) | RSS MB (idle, attached) | RSS MB (after) | peak RSS MB |
|---|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 1.23 | 1.22 | 0.00 | 1.22 | 29.2 | 6.6 | 7.4 | 7.4 |
| sync off | vtmux | 0.41 | 0.23 | 0.01 | 0.24 | 0.5 | 22.3 | 24.5 | 24.5 |
| sync on | tmux | 4.77 | 4.68 | 0.00 | 4.68 | 187.0 | 6.6 | 7.5 | 7.5 |
| sync on | vtmux | 0.41 | 0.29 | 0.01 | 0.30 | 0.8 | 22.5 | 24.6 | 24.6 |

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 263 | 6.1 |
| 5 panes / 1 clients | vtmux | 7.2 | 0.2 | 14.3 | 67 | 224 | 16.0 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 262 | 6.2 |
| 5 panes / 2 clients | vtmux | 5.7 | 0.3 | 28.3 | 135 | 284 | 19.2 |
| 10 panes / 1 clients | tmux | 2.3 | 0.0 | 15.2 | 68 | 479 | 6.6 |
| 10 panes / 1 clients | vtmux | 12.3 | 0.2 | 14.3 | 68 | 364 | 19.3 |
| 10 panes / 2 clients | tmux | 2.4 | 0.0 | 30.4 | 135 | 474 | 6.7 |
| 10 panes / 2 clients | vtmux | 10.5 | 0.3 | 28.2 | 134 | 429 | 21.8 |
| 20 panes / 1 clients | tmux | 4.5 | 0.0 | 15.2 | 68 | 861 | 8.0 |
| 20 panes / 1 clients | vtmux | 25.7 | 0.2 | 14.3 | 68 | 578 | 27.2 |
| 20 panes / 2 clients | tmux | 4.4 | 0.0 | 30.5 | 135 | 856 | 8.1 |
| 20 panes / 2 clients | vtmux | 24.5 | 0.3 | 28.2 | 134 | 638 | 29.1 |

### key_echo (200 keys 30 ms apart into a raw echo pane, one 200x50 client, sync on, the other pane idle or running `yes`; arrival stamped in the reader thread)

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.21 | 0.28 | 0.39 | 0 |
| flood none | vtmux | 0.33 | 0.43 | 0.70 | 0 |
| flood yes | tmux | 0.52 | 0.76 | 1.45 | 0 |
| flood yes | vtmux | 0.49 | 0.70 | 1.70 | 0 |

## Big scrollback, one pane: `bench-history-memory.sh spec big`

`rss_out` is 0.5 s after the output ends, `rss_mb` after `IDLE` (10 s) more, `settled` once the RSS stops moving,
`settle_s` seconds after the output ended (the settle loop polls for at least 5 s, so tmux shows 15).

```
scenario binary      size   limit     rss_mb hist_size retained output_s rss_out_mb settled_mb settle_s
spec    tmux      120x40  100000       66.1     99962        -     0.46       66.1       66.1       15
spec    vtmux     120x40  100000       29.9     99962   100039     0.18      107.0       19.0       16
big     tmux      120x40 1000000      622.4    999962        -     4.18      622.4      622.4       15
big     vtmux     120x40 1000000      898.6    999962  1000345     2.26      981.4      103.0       72
```

## A session of 10 panes x 100k lines: `bench-history-memory.sh session`

Ten windows of one 120x40 session, `history-limit 100000`, every pane `cat`ing the same 100k log lines at once.
`hist_sum` is the panes' `#{history_size}` summed. Two runs:

```
scenario    binary  panes   limit hist_sum output_s rss_out_mb   rss_mb settled_mb per_pane_mb settle_s
session100k tmux       10  100000   999620     2.91      628.5    628.5      628.5       62.9       15
session100k vtmux      10  100000   999620     1.00      946.9    869.8      100.4       10.0       70
session100k tmux       10  100000   999620     2.84      628.6    628.6      628.6       62.9       15
session100k vtmux      10  100000   999620     1.01      946.9    865.8      100.4       10.0       72
```

## Using the history: `bench-history-memory.sh copymode spec big`

Three servers per binary and state (medians in the table, every run below it). `uncomp`: entry right after the
output (vtmux has compressed nothing yet); `comp`: entry once the RSS settled. Each entry is followed in the same copy
mode by `send -X search-backward no-such-text-in-the-history` (no match: the search scans every line, wrapping) and
`send -X history-top`; all times are the wall time of the `tmux` client command (the server answers once the
command is done). `rss_cm`: the compressed server in copy mode after the search and the jump; `rss_end`: the same
server settled after `send -X cancel`. `noise_ms`: a `display -p ok` round trip.

```
scenario binary    limit  noise_ms uncomp_ms   comp_ms  ratio  rss_u_mb  rss_c_mb  search_u     top_u  search_c     top_c rss_cm_mb rss_end_mb
spec    tmux     100000         6        30        32   1.07      66.1      66.1       175         7       179         8     128.0     124.2
  run spec tmux: entry 29 ms uncompressed, 31 ms compressed (RSS 66 -> 66 MB); search 175 / 176 ms, top 7 / 8 ms; RSS in copy mode 128.0 MB, after cancel 124.2 MB
  run spec tmux: entry 31 ms uncompressed, 32 ms compressed (RSS 66 -> 66 MB); search 176 / 179 ms, top 7 / 7 ms; RSS in copy mode 128.0 MB, after cancel 124.2 MB
  run spec tmux: entry 30 ms uncompressed, 33 ms compressed (RSS 66 -> 66 MB); search 173 / 220 ms, top 7 / 8 ms; RSS in copy mode 128.0 MB, after cancel 124.2 MB
spec    vtmux    100000         5        37        11   0.30     106.9      18.8       592         5       631         6     124.7      18.9
  run spec vtmux: entry 37 ms uncompressed, 11 ms compressed (RSS 107 -> 19 MB); search 592 / 631 ms, top 5 / 6 ms; RSS in copy mode 124.7 MB, after cancel 18.9 MB
  run spec vtmux: entry 33 ms uncompressed, 11 ms compressed (RSS 107 -> 19 MB); search 616 / 631 ms, top 5 / 6 ms; RSS in copy mode 124.7 MB, after cancel 18.9 MB
  run spec vtmux: entry 39 ms uncompressed, 11 ms compressed (RSS 107 -> 19 MB); search 589 / 626 ms, top 5 / 6 ms; RSS in copy mode 125.0 MB, after cancel 19.1 MB
big     tmux    1000000         6       237       220   0.93     622.4     622.4      1684         7      1686         7    1240.4    1202.3
  run big tmux: entry 246 ms uncompressed, 220 ms compressed (RSS 622 -> 622 MB); search 1680 / 1682 ms, top 7 / 7 ms; RSS in copy mode 1240.4 MB, after cancel 1202.3 MB
  run big tmux: entry 217 ms uncompressed, 212 ms compressed (RSS 622 -> 622 MB); search 1684 / 1686 ms, top 7 / 7 ms; RSS in copy mode 1240.4 MB, after cancel 1202.3 MB
  run big tmux: entry 237 ms uncompressed, 249 ms compressed (RSS 622 -> 622 MB); search 1731 / 1715 ms, top 7 / 7 ms; RSS in copy mode 1240.4 MB, after cancel 1202.3 MB
big     vtmux   1000000         5       315        57   0.18     978.4     100.8      7944         6      8371         6    1160.3     102.2
  run big vtmux: entry 315 ms uncompressed, 57 ms compressed (RSS 981 -> 101 MB); search 7963 / 8371 ms, top 7 / 7 ms; RSS in copy mode 1160.3 MB, after cancel 102.2 MB
  run big vtmux: entry 325 ms uncompressed, 51 ms compressed (RSS 978 -> 101 MB); search 7944 / 8282 ms, top 6 / 6 ms; RSS in copy mode 1160.8 MB, after cancel 102.7 MB
  run big vtmux: entry 282 ms uncompressed, 58 ms compressed (RSS 978 -> 100 MB); search 7928 / 8419 ms, top 6 / 6 ms; RSS in copy mode 1159.6 MB, after cancel 101.4 MB
```

## Hidden panes: `bench-hidden-panes.py`

```
tmux panes=101 rss_before=5052 rss_after=9840 per_hidden_kb=47.9 switch_ms median=5.14 min=5.01 max=7.01
vtmux panes=101 rss_before=11460 rss_after=18948 per_hidden_kb=74.9 switch_ms median=2.47 min=2.38 max=3.69
tmux panes=101 rss_before=5056 rss_after=9828 per_hidden_kb=47.7 switch_ms median=5.18 min=5.05 max=6.45
vtmux panes=101 rss_before=11204 rss_after=18784 per_hidden_kb=75.8 switch_ms median=2.52 min=2.39 max=3.53
tmux panes=101 rss_before=5052 rss_after=9820 per_hidden_kb=47.7 switch_ms median=5.14 min=5.00 max=5.51
vtmux panes=101 rss_before=11336 rss_after=18900 per_hidden_kb=75.6 switch_ms median=2.48 min=2.28 max=3.39
tmux panes=101 rss_before=5056 rss_after=9820 per_hidden_kb=47.6 switch_ms median=5.15 min=5.08 max=6.08
vtmux panes=101 rss_before=11332 rss_after=19504 per_hidden_kb=81.7 switch_ms median=2.52 min=2.34 max=2.76
```

## A pane that never stops: `bench.py trickle`

120x40, `history-limit 100000`; each rate case first writes 150k lines (history at the limit), then 1200 s of log
lines at 10 or 1000 lines/s, then 120 s idle; `fill` writes the 150k lines and goes idle. All six servers at once.
Summary of every 10 s sample from 30 s to 1200 s (streaming), from the JSON:

| case | tmux 3.8 streaming | vtmux streaming | vtmux idle at the end | server CPU over 1320 s (tmux / vtmux) |
|---|---|---|---|---|
| 10 lines/s | 65.7-66.1 MB (median 65.8) | 18.9-19.6 MB (median 19.3) | 19.3 MB | 1.8 / 1.7 s |
| 1000 lines/s | 66.1-66.5 MB (median 66.4) | 18.7-20.1 MB (median 19.1) | 19.0 MB | 7.5 / 5.8 s |
| fill, then idle | 66.1 MB | 18.8 MB | 18.8 MB | 0.5 / 0.2 s |

The `hist_compress_busy` run on this host ([`2026-11-05-history-compression-busy.md`](2026-11-05-history-compression-busy.md),
load 5.5-9.6) had 18.7-19.4 and 18.5-19.8 MB for vtmux: the same within 0.3 MB.


### trickle (120x40, history-limit 100000, 150k lines then 1200 s of output then 120 s idle, RSS every 10 s)

| t s | tmux 10 RSS MB (hsize) | tmux 1000 RSS MB (hsize) | tmux fill RSS MB (hsize) | vtmux 10 RSS MB (hsize) | vtmux 1000 RSS MB (hsize) | vtmux fill RSS MB (hsize) |
|---|---|---|---|---|---|---|
| 0 | 23.7 (31219) | 20.2 (25467) | 12.9 (13722) | 39.7 (32343) | 36.3 (28853) | 20.8 (12169) |
| 30 | 65.7 (90258) | 66.1 (99601) | 66.1 (99961) | 19.0 (90260) | 18.9 (99841) | 18.8 (99961) |
| 60 | 65.7 (90558) | 66.2 (99601) | 66.1 (99961) | 19.2 (90560) | 19.0 (99841) | 18.8 (99961) |
| 90 | 65.7 (90858) | 66.4 (99601) | 66.1 (99961) | 19.6 (90860) | 18.9 (99841) | 18.8 (99961) |
| 120 | 65.7 (91158) | 66.4 (99601) | 66.1 (99961) | 19.6 (91160) | 19.0 (99841) | 18.8 (99961) |
| 150 | 65.7 (91458) | 66.4 (99601) | 66.1 (99961) | 19.3 (91460) | 18.9 (99841) | 18.8 (99961) |
| 180 | 65.7 (91758) | 66.4 (99601) | 66.1 (99961) | 19.3 (91760) | 19.4 (99841) | 18.8 (99961) |
| 210 | 65.7 (92058) | 66.4 (99601) | 66.1 (99961) | 19.6 (92060) | 18.9 (99841) | 18.8 (99961) |
| 240 | 65.8 (92358) | 66.4 (99601) | 66.1 (99961) | 19.6 (92360) | 19.0 (99841) | 18.8 (99961) |
| 270 | 65.8 (92658) | 66.4 (99601) | 66.1 (99961) | 19.6 (92660) | 18.9 (99841) | 18.8 (99961) |
| 300 | 65.8 (92958) | 66.4 (99601) | 66.1 (99961) | 19.3 (92960) | 19.0 (99841) | 18.8 (99961) |
| 330 | 65.8 (93258) | 66.4 (99601) | 66.1 (99961) | 19.3 (93260) | 19.0 (99841) | 18.8 (99961) |
| 360 | 65.8 (93558) | 66.4 (99601) | 66.1 (99961) | 19.6 (93560) | 19.4 (99841) | 18.8 (99961) |
| 390 | 65.8 (93858) | 66.4 (99601) | 66.1 (99961) | 19.3 (93860) | 19.0 (99841) | 18.8 (99961) |
| 420 | 65.8 (94158) | 66.4 (99601) | 66.1 (99961) | 18.9 (94160) | 19.4 (99841) | 18.8 (99961) |
| 450 | 65.8 (94458) | 66.4 (99601) | 66.1 (99961) | 18.9 (94460) | 19.0 (99841) | 18.8 (99961) |
| 480 | 65.8 (94758) | 66.4 (99601) | 66.1 (99961) | 19.3 (94760) | 19.3 (99841) | 18.8 (99961) |
| 510 | 65.9 (95058) | 66.4 (99601) | 66.1 (99961) | 19.3 (95060) | 19.0 (99841) | 18.8 (99961) |
| 540 | 65.9 (95358) | 66.4 (99601) | 66.1 (99961) | 19.3 (95360) | 19.3 (99841) | 18.8 (99961) |
| 570 | 65.9 (95658) | 66.4 (99601) | 66.1 (99961) | 18.9 (95660) | 19.0 (99841) | 18.8 (99961) |
| 600 | 65.9 (95958) | 66.4 (99601) | 66.1 (99961) | 19.3 (95960) | 19.3 (99841) | 18.8 (99961) |
| 630 | 65.9 (96258) | 66.4 (99601) | 66.1 (99961) | 19.3 (96260) | 19.0 (99841) | 18.8 (99961) |
| 660 | 65.9 (96558) | 66.4 (99601) | 66.1 (99961) | 19.3 (96560) | 19.4 (99841) | 18.8 (99961) |
| 690 | 65.9 (96858) | 66.4 (99601) | 66.1 (99961) | 18.9 (96860) | 19.0 (99841) | 18.8 (99961) |
| 720 | 65.9 (97158) | 66.4 (99601) | 66.1 (99961) | 18.9 (97160) | 19.4 (99841) | 18.8 (99961) |
| 750 | 65.9 (97458) | 66.4 (99601) | 66.1 (99961) | 19.3 (97460) | 19.1 (99841) | 18.8 (99961) |
| 780 | 66.0 (97758) | 66.4 (99601) | 66.1 (99961) | 19.3 (97760) | 19.2 (99841) | 18.8 (99961) |
| 810 | 66.0 (98058) | 66.4 (99601) | 66.1 (99961) | 19.3 (98060) | 19.7 (99841) | 18.8 (99961) |
| 840 | 66.0 (98358) | 66.4 (99601) | 66.1 (99961) | 19.0 (98360) | 18.9 (99841) | 18.8 (99961) |
| 870 | 66.0 (98658) | 66.4 (99601) | 66.1 (99961) | 19.3 (98660) | 19.0 (99841) | 18.8 (99961) |
| 900 | 66.0 (98958) | 66.4 (99601) | 66.1 (99961) | 19.3 (98960) | 18.9 (99841) | 18.8 (99961) |
| 930 | 66.0 (99258) | 66.4 (99601) | 66.1 (99961) | 19.3 (99260) | 19.0 (99841) | 18.8 (99961) |
| 960 | 66.0 (99558) | 66.4 (99601) | 66.1 (99961) | 19.0 (99560) | 18.9 (99841) | 18.8 (99961) |
| 990 | 66.1 (99858) | 66.4 (99601) | 66.1 (99961) | 19.0 (99860) | 19.1 (99841) | 18.8 (99961) |
| 1020 | 65.7 (90158) | 66.4 (99601) | 66.1 (99961) | 19.3 (90160) | 19.7 (99841) | 18.8 (99961) |
| 1050 | 65.8 (90458) | 66.4 (99601) | 66.1 (99961) | 19.3 (90460) | 19.1 (99841) | 18.8 (99961) |
| 1080 | 65.8 (90758) | 66.4 (99601) | 66.1 (99961) | 19.3 (90760) | 19.4 (99841) | 18.8 (99961) |
| 1110 | 65.8 (91058) | 66.4 (99601) | 66.1 (99961) | 19.0 (91060) | 19.5 (99841) | 18.8 (99961) |
| 1140 | 65.8 (91358) | 66.5 (99601) | 66.1 (99961) | 19.3 (91360) | 19.4 (99841) | 18.8 (99961) |
| 1170 | 65.8 (91658) | 66.5 (99601) | 66.1 (99961) | 19.3 (91660) | 19.5 (99841) | 18.8 (99961) |
| 1200 | 65.8 (91958) | 66.5 (99601) | 66.1 (99961) | 19.3 (91960) | 19.4 (99841) | 18.8 (99961) |
| 1230 | 65.8 (91961) | 66.5 (99961) | 66.1 (99961) | 19.3 (91961) | 19.0 (99961) | 18.8 (99961) |
| 1260 | 65.8 (91961) | 66.5 (99961) | 66.1 (99961) | 19.3 (91961) | 19.0 (99961) | 18.8 (99961) |
| 1290 | 65.8 (91961) | 66.5 (99961) | 66.1 (99961) | 19.3 (91961) | 19.0 (99961) | 18.8 (99961) |
| 1320 | 65.8 (91961) | 66.5 (99961) | 66.1 (99961) | 19.3 (91961) | 19.0 (99961) | 18.8 (99961) |
| server CPU s | 1.8 | 7.5 | 0.5 | 1.7 | 5.8 | 0.2 |

## Regression: agent panes since `lows_render`

`agent_panes` above has vtmux at 5.1-5.7x tmux's server CPU, where the `perf_agent_panes` run on this host
([`2026-11-05-agent-panes.md`](2026-11-05-agent-panes.md)) had 0.9-1.6x. That run measured the change on `3682ca7`,
which does not contain `63ee517` (`lows_render`: drop the screen view of panes no client shows); main has both.

Probe (`/tmp/pr178/agentprobe.py`: the `agent_panes` setup, one 200x50 client on window 0, 10 s of server CPU after
3 s; `vtmux-nohide` is the same tree with the `self.release_hidden_screens()` call in `render_and_exit` commented out,
a scratch build, not committed):

| panes / clients | vtmux (main) | vtmux-nohide |
|---|---|---|
| 20 / 1 (two runs) | 25.8%, 25.8% | 4.0%, 3.9% |
| 5 / 1 | 7.5% | 1.6% |

`perf record -F 2000 -p <server>` on main at 20 panes / 1 client: 28% in `ScreenView::update`, 14% in
`CellIteration::style`, 9% in `Vec<Cell>::extend_with`, and most of the rest Ghostty's cell reads: a whole-screen
read. Each hidden pane's view is dropped at the end of every turn, and the next synchronized update in that pane
(`?2026h`, ~30 a second per pane in `agent_tui.py`) builds it again from scratch, so every hidden agent pane is read
whole about 30 times a second. Fewer hidden panes, lower cost: with 2 clients (window 1 shown too) vtmux is cheaper
than with 1 at every pane count. Fixed in `8a7d31f`, below.

## Agent panes fixed (`8a7d31f`)

`8a7d31f` keeps a hidden pane's screen view until it has had no output for a second (`HIDDEN_VIEW_IDLE`); a pane
with no output since a client last showed it is still released at the end of the first turn it is hidden. The
*Agent panes* and *Hidden panes* tables of `PERFORMANCE.md` come from these runs, which replace the ones above.

- **Binaries:** `vtmux` = `8a7d31f` (release, `/tmp/hidrel/vtmux-new`), `base` = `71a14bd` (release, the same tree
  without the change, `/tmp/hidrel/vtmux-base`), tmux 3.8 from `cargo xtask ref-tmux`. Same host.
- **Load:** `agent_panes` 23:24-23:35 UTC, 7.9 / 6.2; hidden panes 23:39-23:42 UTC, 6.7 / 8.4 (above the bench's
  4: the host was busy all evening). Both under `flock /tmp/vtmux-stress.lock`.

```
RUNS=3 NO_BUILD=1 VTMUX=/tmp/hidrel/vtmux-new BASE=/tmp/hidrel/vtmux-base python3 scripts/bench.py agent_panes
python3 scripts/bench-hidden-panes.py <binary> rvh<pid><binary><n>     # 4 x tmux, base, vtmux alternating
```

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 264 | 6.1 |
| 5 panes / 1 clients | base | 6.8 | 0.2 | 14.2 | 67 | 225 | 15.9 |
| 5 panes / 1 clients | vtmux | 1.6 | 0.2 | 14.3 | 68 | 232 | 17.7 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 263 | 6.2 |
| 5 panes / 2 clients | base | 5.4 | 0.3 | 28.2 | 134 | 285 | 19.2 |
| 5 panes / 2 clients | vtmux | 2.4 | 0.3 | 28.3 | 134 | 283 | 20.3 |
| 10 panes / 1 clients | tmux | 2.3 | 0.0 | 15.2 | 68 | 477 | 6.6 |
| 10 panes / 1 clients | base | 12.2 | 0.2 | 14.3 | 67 | 357 | 17.4 |
| 10 panes / 1 clients | vtmux | 2.4 | 0.2 | 14.3 | 68 | 395 | 21.7 |
| 10 panes / 2 clients | tmux | 2.6 | 0.0 | 30.4 | 135 | 471 | 6.7 |
| 10 panes / 2 clients | base | 11.3 | 0.3 | 28.2 | 134 | 404 | 23.1 |
| 10 panes / 2 clients | vtmux | 3.5 | 0.3 | 28.2 | 134 | 442 | 24.1 |
| 20 panes / 1 clients | tmux | 4.4 | 0.0 | 15.2 | 68 | 853 | 8.0 |
| 20 panes / 1 clients | base | 25.9 | 0.2 | 14.3 | 68 | 559 | 27.1 |
| 20 panes / 1 clients | vtmux | 4.1 | 0.1 | 14.3 | 68 | 717 | 31.2 |
| 20 panes / 2 clients | tmux | 4.4 | 0.0 | 30.5 | 136 | 860 | 8.1 |
| 20 panes / 2 clients | base | 24.7 | 0.3 | 28.2 | 134 | 623 | 27.2 |
| 20 panes / 2 clients | vtmux | 5.4 | 0.3 | 28.3 | 135 | 753 | 33.7 |

Server CPU against tmux's, unrounded from the JSON: base 4.95x / 3.64x / 5.31x / 4.33x / 5.96x / 5.58x, vtmux
1.20x / 1.63x / 1.04x / 1.35x / 0.93x / 1.22x (5/1, 5/2, 10/1, 10/2, 20/1, 20/2). 5 panes / 2 clients is the
`perf_agent_panes` figure again (2.4% against 1.5%); with 2 clients 4 of the 5 panes are shown, so the change does
little there. RSS rises 1.0-6.5 MB over base (busy hidden panes keep their views).

```
tmux panes=101 rss_before=5056 rss_after=9820 per_hidden_kb=47.6 switch_ms median=5.18 min=5.03 max=5.33
vtmux-base panes=101 rss_before=11404 rss_after=19364 per_hidden_kb=79.6 switch_ms median=2.51 min=2.42 max=2.84
vtmux-new panes=101 rss_before=11220 rss_after=18600 per_hidden_kb=73.8 switch_ms median=2.57 min=2.47 max=2.73
tmux panes=101 rss_before=5004 rss_after=9772 per_hidden_kb=47.7 switch_ms median=5.13 min=4.98 max=5.23
vtmux-base panes=101 rss_before=11216 rss_after=18828 per_hidden_kb=76.1 switch_ms median=2.50 min=2.40 max=2.70
vtmux-new panes=101 rss_before=11400 rss_after=18796 per_hidden_kb=74.0 switch_ms median=2.55 min=2.41 max=3.08
tmux panes=101 rss_before=5052 rss_after=9808 per_hidden_kb=47.6 switch_ms median=5.08 min=4.96 max=8.69
vtmux-base panes=101 rss_before=11344 rss_after=19012 per_hidden_kb=76.7 switch_ms median=2.53 min=2.37 max=3.08
vtmux-new panes=101 rss_before=11348 rss_after=18856 per_hidden_kb=75.1 switch_ms median=2.47 min=2.39 max=2.65
tmux panes=101 rss_before=5040 rss_after=9808 per_hidden_kb=47.7 switch_ms median=5.10 min=4.95 max=6.00
vtmux-base panes=101 rss_before=11340 rss_after=18840 per_hidden_kb=75.0 switch_ms median=2.50 min=2.40 max=3.81
vtmux-new panes=101 rss_before=11344 rss_after=19356 per_hidden_kb=80.1 switch_ms median=2.50 min=2.43 max=2.82
```

Hidden-pane RSS with 101 windows: vtmux 18.6-19.4 MB against base 18.8-19.4 (every hidden pane there is idle, so
released at once as before); `C-b n` medians 2.47-2.57 ms against 2.50-2.53.

## Other changes against the earlier numbers

Different host for most of the old rows (Fedora laptop, AMD Ryzen 7 5700X3D, glibc 2.43; this one has glibc 2.34),
so only ratios compare:

- idle RSS 9.0 MB against tmux 4.2 (2.1x; was 7.3 against 4.3, 1.7x, on the laptop).
- scale: 18.2 KB per idle pane (was 33.2 before `perf_scale_rss`), 2.5x the 400-pane RSS (was 3.1x).
- throughput without a client: 0.37x (plain) and 0.46x (SGR) of tmux's server CPU (were 0.46x and 0.78x).
- attached plain flood with sync on: 754 frames of 7.3 KB (were 133 of 40 KB), still 0.62x tmux's CPU.
- many_clients RSS: 22.3 / 24.5 MB attached idle / after the flood (3.4x / 3.3x; were 14.0 / 17.7, the per-client
  row caches of `key_echo_latency`).
- key echo with no flood: 0.33 ms p50 against 0.21 (1.6x; `lat2.py` had 0.50-0.54 against 0.14-0.18 on the
  laptop); next to `yes`: 0.49 against 0.52 ms.
- search-backward over the whole history: 3.4-3.5x (100k lines) and 4.7-5.0x (1M lines) tmux's time (uncompressed - compressed); first measured here.
