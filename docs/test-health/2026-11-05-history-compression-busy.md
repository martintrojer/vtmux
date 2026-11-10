# History compression of busy panes 2026-11-05 (hist_compress_busy)

Spec: `docs/specs/2026-10-31-history-memory.md`, *Starvation override*. Change: `compress.rs` `STARVE`
(a pane whose history keeps changing, or keeps waiting for a pass, for 10 s steps in busy turns too).
Host: a shared Linux server, Linux 6.16.1, AMD EPYC-Genoa, 96 cpus, shared with other agents
(load 4.7-13.6 during the latency runs, 5.5-9.6 during the trickle run; nobody else held
`/tmp/vtmux-stress.lock`). Release builds: `before` is main `3682ca7`, `after` is this change on the same
base (both from this worktree's target dir, same compiler). Reference tmux 3.8 from `cargo xtask ref-tmux`.

## Streaming RSS: `bench.py trickle`

`BASE=before VTMUX=after NO_BUILD=1 python3 scripts/bench.py trickle` (defaults: 120x40, `history-limit 100000`,
each rate case first writes the 150k lines of `fill` so the history starts at the limit, then 1200 s of
log lines at 10 or 1000 lines/s, then 120 s idle; `fill` writes the 150k lines and goes idle; RSS every 10 s;
all nine servers at once). `base` is before, `vtmux` after. Every 10 s sample, 30-1200 s (streaming):

| case | tmux 3.8 | before | after | after, idle at the end | server CPU over 1320 s (tmux / before / after) |
|---|---|---|---|---|---|
| 10 lines/s | 65.7-66.1 MB | 106.6-107.4 MB | **18.7-19.4 MB** (median 19.1) | 19.1 MB | 1.9 / 1.8 / 1.8 s |
| 1000 lines/s | 66.2-66.7 MB | 108.5-109.8 MB | **18.5-19.8 MB** (median 19.0) | 18.8 MB | 8.6 / 6.6 / 7.5 s |
| fill, then idle | 66.0 MB | 18.5 MB | 18.7 MB | 18.7 MB | 0.5 / 0.3 / 0.3 s |

Before the change a streaming pane kept its whole 100k-line history uncompressed for the 20 minutes and
shrank only after the output stopped (18.7 MB at 1230 s). After it, the streaming panes are at 18.5-19.8 MB
from the 20 s sample on: the idle-compressed level (18.5-18.7 MB) plus at most about 1 MB, the pages not yet
compressed (the viewport and the page holding the active area are never compressed). The override costs
0.9 s of server CPU over 22 minutes at 1000 lines/s (0.07% of a core); none at 10 lines/s.

Raw table (vtmux = after, base = before):

| t s | tmux 10 RSS MB (hsize) | tmux 1000 RSS MB (hsize) | tmux fill RSS MB (hsize) | base 10 RSS MB (hsize) | base 1000 RSS MB (hsize) | base fill RSS MB (hsize) | vtmux 10 RSS MB (hsize) | vtmux 1000 RSS MB (hsize) | vtmux fill RSS MB (hsize) |
|---|---|---|---|---|---|---|---|---|---|
| 0 | 28.6 (39202) | 29.2 (40112) | 26.2 (35355) | 64.5 (59043) | 46.4 (39564) | 49.0 (42431) | 36.6 (29105) | 29.6 (21799) | 21.8 (13296) |
| 30 | 65.7 (90258) | 66.2 (99651) | 66.0 (99961) | 106.6 (90260) | 108.5 (99781) | 18.5 (99961) | 18.8 (90259) | 19.1 (99761) | 18.7 (99961) |
| 60 | 65.7 (90558) | 66.3 (99651) | 66.0 (99961) | 106.7 (90560) | 109.8 (99791) | 18.5 (99961) | 19.0 (90560) | 19.1 (99761) | 18.7 (99961) |
| 90 | 65.7 (90858) | 66.5 (99651) | 66.0 (99961) | 106.7 (90860) | 109.8 (99781) | 18.5 (99961) | 19.4 (90859) | 19.1 (99761) | 18.7 (99961) |
| 120 | 65.7 (91158) | 66.6 (99651) | 66.0 (99961) | 106.7 (91160) | 109.8 (99781) | 18.5 (99961) | 19.4 (91159) | 19.4 (99761) | 18.7 (99961) |
| 150 | 65.7 (91458) | 66.6 (99651) | 66.0 (99961) | 106.7 (91460) | 109.8 (99781) | 18.5 (99961) | 19.1 (91459) | 19.3 (99761) | 18.7 (99961) |
| 180 | 65.8 (91758) | 66.6 (99651) | 66.0 (99961) | 106.7 (91760) | 109.8 (99781) | 18.5 (99961) | 19.1 (91759) | 19.1 (99761) | 18.7 (99961) |
| 210 | 65.8 (92058) | 66.6 (99651) | 66.0 (99961) | 106.8 (92060) | 109.8 (99781) | 18.5 (99961) | 19.4 (92059) | 19.1 (99761) | 18.7 (99961) |
| 240 | 65.8 (92358) | 66.6 (99651) | 66.0 (99961) | 106.8 (92360) | 109.8 (99791) | 18.5 (99961) | 19.4 (92360) | 18.8 (99761) | 18.7 (99961) |
| 270 | 65.8 (92658) | 66.6 (99651) | 66.0 (99961) | 106.8 (92660) | 109.8 (99781) | 18.5 (99961) | 19.4 (92659) | 19.3 (99761) | 18.7 (99961) |
| 300 | 65.8 (92958) | 66.7 (99651) | 66.0 (99961) | 106.8 (92960) | 109.8 (99791) | 18.5 (99961) | 19.1 (92960) | 18.8 (99761) | 18.7 (99961) |
| 330 | 65.8 (93258) | 66.7 (99651) | 66.0 (99961) | 106.9 (93260) | 109.8 (99781) | 18.5 (99961) | 19.1 (93259) | 18.6 (99761) | 18.7 (99961) |
| 360 | 65.8 (93558) | 66.7 (99651) | 66.0 (99961) | 106.9 (93560) | 109.8 (99781) | 18.5 (99961) | 19.4 (93559) | 18.8 (99761) | 18.7 (99961) |
| 390 | 65.8 (93858) | 66.7 (99651) | 66.0 (99961) | 106.9 (93860) | 109.8 (99781) | 18.5 (99961) | 19.4 (93860) | 19.4 (99761) | 18.7 (99961) |
| 420 | 65.8 (94158) | 66.7 (99651) | 66.0 (99961) | 106.9 (94160) | 109.8 (99781) | 18.5 (99961) | 19.1 (94160) | 18.9 (99761) | 18.7 (99961) |
| 450 | 65.9 (94458) | 66.7 (99651) | 66.0 (99961) | 106.9 (94460) | 109.8 (99781) | 18.5 (99961) | 18.7 (94459) | 19.1 (99761) | 18.7 (99961) |
| 480 | 65.9 (94758) | 66.7 (99651) | 66.0 (99961) | 107.0 (94760) | 109.8 (99781) | 18.5 (99961) | 19.1 (94759) | 18.7 (99761) | 18.7 (99961) |
| 510 | 65.9 (95058) | 66.7 (99651) | 66.0 (99961) | 107.0 (95060) | 109.8 (99781) | 18.5 (99961) | 19.1 (95059) | 19.1 (99761) | 18.7 (99961) |
| 540 | 65.9 (95358) | 66.7 (99651) | 66.0 (99961) | 107.0 (95360) | 109.8 (99791) | 18.5 (99961) | 19.1 (95360) | 18.8 (99761) | 18.7 (99961) |
| 570 | 65.9 (95658) | 66.7 (99651) | 66.0 (99961) | 107.0 (95660) | 109.8 (99791) | 18.5 (99961) | 18.7 (95660) | 18.7 (99761) | 18.7 (99961) |
| 600 | 65.9 (95958) | 66.7 (99651) | 66.0 (99961) | 107.0 (95960) | 109.8 (99781) | 18.5 (99961) | 19.1 (95959) | 18.7 (99761) | 18.7 (99961) |
| 630 | 65.9 (96258) | 66.7 (99651) | 66.0 (99961) | 107.1 (96260) | 109.8 (99781) | 18.5 (99961) | 19.1 (96260) | 19.1 (99761) | 18.7 (99961) |
| 660 | 65.9 (96558) | 66.7 (99651) | 66.0 (99961) | 107.1 (96560) | 109.8 (99791) | 18.5 (99961) | 19.1 (96560) | 19.2 (99761) | 18.7 (99961) |
| 690 | 65.9 (96858) | 66.7 (99651) | 66.0 (99961) | 107.1 (96860) | 109.8 (99781) | 18.5 (99961) | 18.7 (96859) | 18.8 (99761) | 18.7 (99961) |
| 720 | 66.0 (97158) | 66.7 (99651) | 66.0 (99961) | 107.1 (97160) | 109.8 (99781) | 18.5 (99961) | 18.7 (97159) | 18.9 (99761) | 18.7 (99961) |
| 750 | 66.0 (97458) | 66.7 (99651) | 66.0 (99961) | 107.1 (97460) | 109.8 (99781) | 18.5 (99961) | 19.1 (97460) | 18.7 (99761) | 18.7 (99961) |
| 780 | 66.0 (97758) | 66.7 (99651) | 66.0 (99961) | 107.2 (97760) | 109.8 (99781) | 18.5 (99961) | 19.1 (97759) | 19.3 (99761) | 18.7 (99961) |
| 810 | 66.0 (98058) | 66.7 (99651) | 66.0 (99961) | 107.2 (98060) | 109.8 (99791) | 18.5 (99961) | 19.1 (98060) | 18.7 (99761) | 18.7 (99961) |
| 840 | 66.0 (98358) | 66.7 (99651) | 66.0 (99961) | 107.2 (98360) | 109.8 (99781) | 18.5 (99961) | 18.7 (98359) | 19.5 (99761) | 18.7 (99961) |
| 870 | 66.0 (98658) | 66.7 (99651) | 66.0 (99961) | 107.2 (98660) | 109.8 (99781) | 18.5 (99961) | 19.1 (98660) | 19.1 (99761) | 18.7 (99961) |
| 900 | 66.0 (98958) | 66.7 (99651) | 66.0 (99961) | 107.2 (98960) | 109.8 (99781) | 18.5 (99961) | 19.1 (98959) | 19.0 (99761) | 18.7 (99961) |
| 930 | 66.0 (99258) | 66.7 (99651) | 66.0 (99961) | 107.3 (99260) | 109.8 (99781) | 18.5 (99961) | 19.1 (99260) | 18.8 (99761) | 18.7 (99961) |
| 960 | 66.1 (99558) | 66.7 (99651) | 66.0 (99961) | 107.3 (99560) | 109.8 (99791) | 18.5 (99961) | 18.7 (99560) | 19.8 (99761) | 18.7 (99961) |
| 990 | 66.1 (99858) | 66.7 (99651) | 66.0 (99961) | 107.3 (99860) | 109.8 (99781) | 18.5 (99961) | 18.7 (99859) | 18.8 (99761) | 18.7 (99961) |
| 1020 | 65.7 (90158) | 66.7 (99651) | 66.0 (99961) | 107.3 (90160) | 109.8 (99781) | 18.5 (99961) | 19.1 (90159) | 19.0 (99761) | 18.7 (99961) |
| 1050 | 65.7 (90458) | 66.7 (99651) | 66.0 (99961) | 107.3 (90460) | 109.8 (99781) | 18.5 (99961) | 19.1 (90459) | 19.1 (99761) | 18.7 (99961) |
| 1080 | 65.8 (90758) | 66.7 (99651) | 66.0 (99961) | 107.4 (90760) | 109.8 (99781) | 18.5 (99961) | 18.7 (90759) | 19.1 (99761) | 18.7 (99961) |
| 1110 | 65.8 (91058) | 66.7 (99651) | 66.0 (99961) | 107.4 (91060) | 109.8 (99781) | 18.5 (99961) | 18.7 (91059) | 19.5 (99761) | 18.7 (99961) |
| 1140 | 65.8 (91358) | 66.7 (99651) | 66.0 (99961) | 107.4 (91360) | 109.8 (99781) | 18.5 (99961) | 19.1 (91360) | 19.5 (99761) | 18.7 (99961) |
| 1170 | 65.8 (91658) | 66.7 (99651) | 66.0 (99961) | 107.4 (91660) | 109.8 (99781) | 18.5 (99961) | 19.1 (91659) | 18.8 (99761) | 18.7 (99961) |
| 1200 | 65.8 (91958) | 66.7 (99651) | 66.0 (99961) | 107.4 (91960) | 109.8 (99781) | 18.5 (99961) | 19.1 (91959) | 19.0 (99761) | 18.7 (99961) |
| 1230 | 65.8 (91961) | 66.7 (99961) | 66.0 (99961) | 18.7 (91961) | 18.7 (99961) | 18.5 (99961) | 19.1 (91961) | 18.8 (99961) | 18.7 (99961) |
| 1260 | 65.8 (91961) | 66.7 (99961) | 66.0 (99961) | 18.7 (91961) | 18.7 (99961) | 18.5 (99961) | 19.1 (91961) | 18.8 (99961) | 18.7 (99961) |
| 1290 | 65.8 (91961) | 66.7 (99961) | 66.0 (99961) | 18.7 (91961) | 18.7 (99961) | 18.5 (99961) | 19.1 (91961) | 18.8 (99961) | 18.7 (99961) |
| 1320 | 65.8 (91961) | 66.7 (99961) | 66.0 (99961) | 18.7 (91961) | 18.7 (99961) | 18.5 (99961) | 19.1 (91961) | 18.8 (99961) | 18.7 (99961) |
| server CPU s | 1.9 | 8.6 | 0.5 | 1.8 | 6.6 | 0.3 | 1.8 | 7.5 | 0.3 |

## Latency gate (criterion 6)

`/tmp/hcb/lat.sh` (the `latency` mode of `scripts/bench-history-memory.sh`, run by hand with `after` against
`before` instead of `NOCOMPRESS`: `before` is the same code without the override, so at GAP_MS 2 it never
compresses), 6 alternating pairs (order swapped every other pair), 2 history panes, 30 s per run, under
`flock /tmp/vtmux-stress.lock`. With GAP_MS 2 the loop is never quiet: `after` compresses the history panes once
they are starved (from about 8 s after the fill, windows of 12.0 s spec and 14.6 s narrow found in all 6 runs),
in busy turns; `before` compresses nothing. "Window" compares `after`'s samples inside its compression window
with `before`'s samples in the same seconds.

| scenario | GAP_MS | after p50 / p95 ms (whole run) | before p50 / p95 ms | whole-run rise p50 / p95 | inside the window: p95 rise |
|---|---|---|---|---|---|
| spec | 2 | 0.256 / 0.377 | 0.255 / 0.383 | +0.1% / **-1.6%** | **+0.5%** (0.385 vs 0.383, n 30903) |
| spec | 60 | 0.569 / 0.704 | 0.578 / 0.725 | -1.4% / -3.0% | -3.1% |
| narrow | 2 | 0.208 / 0.281 | 0.208 / 0.295 | +0.4% / **-4.6%** | **-1.6%** (0.279 vs 0.284, n 38462) |
| narrow | 60 | 0.504 / 0.627 | 0.511 / 0.632 | -1.3% / -0.8% | -0.4% |

Per-run p95 (ms), spec GAP_MS 2: after 0.365 0.369 0.465 0.351 0.329 0.343, before 0.327 0.369 0.371 0.396
0.448 0.326. Within `after`'s own runs, spec p95 inside the window is 0.385 ms against 0.345 ms before it
(the first 8 s); narrow 0.279 against 0.278. The pairwise figure is the gate's; the within-run one compares
different seconds of the run (the first seconds follow the fill) and is given for completeness.

**The gate holds**: p95 rise below 10% in every row, at most +0.5%.

## Step budget

`bash scripts/bench-history-memory.sh steps spec narrow wide` (load 4.8): the step is the same function in busy
and quiet turns.

| scenario | steps | max step ms | total ms | compressed pages | capture identical |
|---|---|---|---|---|---|
| spec | 291 | 0.244 | 42.7 | 258 | true |
| narrow | 344 | 0.795 | 61.3 | 17 | true |
| wide | 121 | 0.087 | 5.8 | 107 | true |

Worst step 0.8 ms against the 5 ms bound.

## Not measured

- macOS (the footprint column of `trickle` is written but has not been run there).
- A trickle without the prefill (`TRICKLE_PREFILL=0`): at 10 lines/s the history stays far below the limit in
  20 minutes. The predecessor's partial laptop run of that shape (t=410 s, before the change) had vtmux at
  12.0 MB (10 lines/s) and 108.5 MB (1000 lines/s) against tmux's 7.2 and 69.7 MB.
