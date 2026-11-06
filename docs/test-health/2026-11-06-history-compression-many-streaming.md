# History compression of many streaming panes 2026-11-06 (low_compress_many_streaming)

Spec: `docs/specs/2026-10-31-history-memory.md`, *Shared rest*. Change: `compress.rs` `rest`/`MAX_SHARE` (the
rest after a compression step is `REST_FACTOR` times the step divided by the number of starved panes, up to 25,
at least `STEP_GAP`; it was `REST_FACTOR` times the step whatever the number of panes).
Host: devserver, CentOS Stream 9, Linux 6.16.1, AMD EPYC-Genoa, 96 cpus, shared with other agents (load
1.8-4.9 during these runs; every run held `/tmp/vtmux-stress.lock`). Release builds from this worktree's target
dir, same compiler: `before` is main `6b9812f`, `after` is this change on it, `nocompress` is `before` with
`scripts/bench-nocompress.patch`. Reference tmux 3.8 from `cargo xtask ref-tmux`.

## Why the old rest fell behind

The rest was global: after a step of `t` the loop waited `100 t` before any pane's next step, so compression had
about 1% of the loop whatever the number of panes, about 40 compressing steps a second for spec pages (0.15-0.25
ms each). A 120x40 pane at 1000 lines/s fills about 2.6 pages a second; 20 such panes need 52. Short of that the
loss is not proportional, it is total: Ghostty's traversal starts at the oldest page and each pane's cursor got
about 2 steps a second while its history moved 2.6 pages a second towards pruning, so the cursor never left the
oldest pages, and those are the ones pruned next. The 20 panes held their whole history uncompressed (1958-1987
MB) for the 10 minutes, and shrank only once the output stopped. With 5 panes (13 pages a second) the old rest
kept up.

## Streaming RSS: `bench.py trickle`, N panes

`BASE=/tmp/lcms/before VTMUX=/tmp/lcms/after NO_BUILD=1 TRICKLE_PANES="1 5 20" TRICKLE_RATES="1000 fill"
TRICKLE_SECS=600 TRICKLE_IDLE_SECS=120 python3 scripts/bench.py trickle` (120x40, `history-limit 100000`; each
case is one server with N windows, every pane first writing the 150k lines of `fill`, so its history starts at the
limit, then 600 s of log lines at 1000 lines/s, then 120 s idle; `fill`: N panes write the 150k lines and go idle,
the idle-compressed level; RSS every 10 s; all 18 servers at once). Streaming is every sample from 30 to 600 s.

| case | tmux 3.8, streaming | before, streaming | after, streaming (median) | after, idle (`fill`) | server CPU over 720 s (tmux / before / after) |
|---|---|---|---|---|---|
| 1 pane | 66.1-66.4 MB | 18.5-19.8 MB | 18.6-19.5 MB (19.0) | 18.9 MB | 4.5 / 3.6 / 3.9 s |
| 5 panes | 315.2-315.6 MB | 57.6-251.2 MB (59.2 from 60 s) | 57.2-61.0 MB (59.2) | 57.4 MB | 19.8 / 17.0 / 18.0 s |
| 20 panes | 1247-1252 MB | **1958-1987 MB** | **202.5-230.9 MB (216.7)** | 202.3 MB | 76.2 / 66.7 / 66.9 s |

After the change 20 streaming panes stay within 29 MB (median 14 MB, 0.7 MB a pane) of their idle-compressed
202 MB, the same margin a lone pane has (the pages not yet compressed, and the page holding the active area,
which is never compressed). Before it they held 9.7x that, 1.6x tmux's. One pane is unchanged (one starved
pane: the same rest as before). The server CPU is the same with and without the change (66.9 against 66.7 s at
20 panes): compressing 52 pages a second costs about 13 ms a second. The idle 20-pane case settles sooner too
(202 MB by 30 s, against 150 s before): those panes are starved while the fill holds the loop busy, so their passes
share the rest.

The curve (RSS MB; the JSON has hsize and CPU per sample):

| t s | tmux x1 stream | before x1 stream | after x1 stream | tmux x5 stream | before x5 stream | after x5 stream | tmux x20 stream | before x20 stream | after x20 stream | after x1 idle | after x5 idle | after x20 idle |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 65.9 | 106.7 | 51.5 | 89.3 | 108.6 | 87.5 | 77.1 | 80.3 | 80.2 | 81.8 | 102.2 | 68.5 |
| 30 | 66.1 | 19.1 | 19.1 | 315.6 | 251.2 | 58.0 | 1247.2 | 1978.1 | 210.2 | 18.9 | 57.4 | 202.3 |
| 60 | 66.2 | 19.8 | 19.0 | 315.6 | 60.9 | 58.8 | 1248.7 | 1986.9 | 204.1 | 18.9 | 57.4 | 202.3 |
| 90 | 66.3 | 18.7 | 19.2 | 315.6 | 58.7 | 59.2 | 1250.2 | 1975.4 | 211.1 | 18.9 | 57.4 | 202.3 |
| 120 | 66.4 | 18.5 | 19.3 | 315.6 | 60.2 | 59.2 | 1251.4 | 1958.6 | 224.5 | 18.9 | 57.4 | 202.3 |
| 150 | 66.4 | 19.1 | 18.8 | 315.6 | 59.1 | 60.2 | 1251.5 | 1962.8 | 230.9 | 18.9 | 57.4 | 202.3 |
| 180 | 66.4 | 19.5 | 19.0 | 315.6 | 58.4 | 57.9 | 1251.6 | 1959.7 | 223.9 | 18.9 | 57.4 | 202.3 |
| 210 | 66.4 | 18.8 | 19.2 | 315.6 | 59.2 | 60.6 | 1251.6 | 1963.1 | 226.4 | 18.9 | 57.4 | 202.3 |
| 240 | 66.4 | 19.5 | 19.5 | 315.6 | 57.9 | 58.0 | 1251.6 | 1959.9 | 224.0 | 18.9 | 57.4 | 202.3 |
| 270 | 66.4 | 18.8 | 18.8 | 315.6 | 59.6 | 60.5 | 1251.6 | 1964.0 | 224.0 | 18.9 | 57.4 | 202.3 |
| 300 | 66.4 | 19.3 | 19.4 | 315.3 | 59.8 | 60.2 | 1251.7 | 1961.1 | 217.0 | 18.9 | 57.4 | 202.3 |
| 330 | 66.4 | 18.8 | 18.8 | 315.3 | 58.3 | 57.3 | 1251.7 | 1964.0 | 221.1 | 18.9 | 57.4 | 202.3 |
| 360 | 66.4 | 19.1 | 18.7 | 315.3 | 59.8 | 58.0 | 1251.7 | 1961.3 | 217.0 | 18.9 | 57.4 | 202.3 |
| 390 | 66.4 | 19.1 | 18.8 | 315.3 | 60.0 | 60.8 | 1251.7 | 1964.2 | 216.6 | 18.9 | 57.4 | 202.3 |
| 420 | 66.4 | 19.1 | 19.2 | 315.3 | 57.8 | 57.8 | 1251.7 | 1960.6 | 210.6 | 18.9 | 57.4 | 202.3 |
| 450 | 66.4 | 18.8 | 19.2 | 315.3 | 60.8 | 59.2 | 1251.7 | 1964.1 | 216.1 | 18.9 | 57.4 | 202.3 |
| 480 | 66.4 | 19.1 | 19.4 | 315.3 | 58.9 | 58.7 | 1251.7 | 1962.1 | 205.5 | 18.9 | 57.4 | 202.3 |
| 510 | 66.4 | 18.8 | 18.9 | 315.2 | 60.5 | 60.8 | 1251.7 | 1963.5 | 210.1 | 18.9 | 57.4 | 202.3 |
| 540 | 66.4 | 18.6 | 19.2 | 315.2 | 57.8 | 59.5 | 1251.7 | 1961.0 | 205.5 | 18.9 | 57.4 | 202.3 |
| 570 | 66.4 | 18.8 | 19.2 | 315.2 | 60.5 | 59.2 | 1251.7 | 1961.6 | 202.5 | 18.9 | 57.4 | 202.3 |
| 600 | 66.4 | 19.1 | 19.5 | 315.2 | 59.6 | 58.3 | 1251.7 | 1960.5 | 209.8 | 18.9 | 57.4 | 202.3 |
| 630 | 66.4 | 18.7 | 18.8 | 315.6 | 57.3 | 57.2 | 1258.2 | 1595.9 | 202.5 | 18.9 | 57.4 | 202.3 |
| 660 | 66.4 | 18.7 | 18.8 | 315.6 | 57.3 | 57.2 | 1258.2 | 1164.4 | 202.5 | 18.9 | 57.4 | 202.3 |
| 690 | 66.4 | 18.7 | 18.8 | 315.6 | 57.3 | 57.2 | 1258.2 | 740.5 | 202.5 | 18.9 | 57.4 | 202.3 |
| 720 | 66.4 | 18.7 | 18.8 | 315.6 | 57.3 | 57.2 | 1258.2 | 330.7 | 202.5 | 18.9 | 57.4 | 202.3 |
| server CPU s | 4.5 | 3.6 | 3.9 | 19.8 | 17.0 | 18.0 | 76.2 | 66.7 | 66.9 | 0.4 | 1.8 | 6.6 |

## Latency gate (criterion 6)

`VTMUX=/tmp/lcms/after NOCOMPRESS=/tmp/lcms/nocompress PAIRS=4 bash scripts/bench-history-memory.sh latency spec
narrow` (2 history panes, 30 s per run, alternating order): two starved panes now share the rest, so a step comes
twice as often as before.

| scenario | GAP_MS | whole-run p50 / p95 rise | p95 inside the compression window vs nocompress |
|---|---|---|---|
| spec | 2 | +1.5% / +0.3% | -9.8% (0.154 vs 0.170 ms, n 10762) |
| spec | 60 | -1.3% / -2.5% | -2.8% |
| narrow | 2 | +1.1% / +0.4% | +0.0% (0.144 vs 0.144 ms, n 13694) |
| narrow | 60 | -2.8% / -2.8% | -3.0% |

The case the change is for, 20 history panes (`bench-compress-latency.py BINARY SCENARIO 20 40`, GAP_MS 2, three
rounds of `after`, `before` and `nocompress` in rotated order; the loop is never quiet, so all 20 compress once
starved, sharing the rest 20 ways after the change):

| scenario | binary | whole-run p50 / p95 rise vs nocompress | window (mean) | p95 inside the window vs nocompress | max inside |
|---|---|---|---|---|---|
| spec | after | +2.5% / +2.0% | 11.3 s | **-1.5%** (0.162 vs 0.165 ms, n 15539) | 0.70 ms |
| spec | before | +1.2% / +8.4% | 32.7 s | +4.7% (0.169 vs 0.161 ms, n 44904) | 0.79 ms |
| narrow | after | +0.7% / -0.8% | 18.2 s | **-5.4%** (0.151 vs 0.160 ms, n 25109) | 0.83 ms |
| narrow | before | +0.5% / -4.0% | 26.2 s | -1.7% (0.149 vs 0.152 ms, n 36172) | 0.98 ms |

**The gate holds**: no p95 rise reaches 10% in any row (at most +2.0% whole-run after the change). Sharing the
rest compresses 20 panes in a third (spec) to two thirds (narrow) of the time, at no p95 cost the runs can tell
from the noise.

## Step budget

`bash scripts/bench-history-memory.sh steps spec narrow wide`: the step itself is unchanged.

| scenario | steps | max step ms | total ms | compressed pages | capture identical |
|---|---|---|---|---|---|
| spec | 291 | 0.227 | 42.3 | 258 | true |
| narrow | 344 | 0.846 | 67.1 | 17 | true |
| wide | 121 | 0.077 | 5.6 | 107 | true |

Worst step 0.85 ms against the 5 ms bound. With `MAX_SHARE` 25 a rest is at least 4 steps, so however many panes
stream, compression takes at most a fifth of the loop, and a busy pane's output still meets at most one step in
progress.

## Not measured

- macOS.
- More than 20 streaming panes, or 20 at 10 lines/s (that rate needs 50x fewer steps).
- Streaming narrow (truecolour) panes, whose steps are up to 3-4x the spec ones.
