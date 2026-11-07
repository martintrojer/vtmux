# Live upgrade bench after `lu_upgrade_perf`, 2026-11-07

Release builds (the after one uncommitted, so `1b0dba1+dirty` below) on the shared devserver (AMD EPYC-Genoa, 96 cpus, load 5-8 from other workers). Before:
`1b0dba1`; after: `lu_upgrade_perf` (Wyhash blob checksum in patch 0017, the patch-stack question,
every pane's history compressed on threads before a same-stack upgrade, the dry-run limit scaled by
the rows it imports, the new image dropping the stream and trimming the heap). What each column means:
[PERFORMANCE.md](../PERFORMANCE.md), *Live upgrade*.

## Before

`NO_BUILD=1 RUNS=3 VTMUX=<1b0dba1 release> python3 scripts/bench.py upgrade`

2026-11-07 19:11 UTC, commit 1b0dba1+dirty, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 5.4, median of 3; tmux: tmux 3.8; vtmux: vtmux 3.8-beta1

### upgrade (vtmux-upgrade onto the same binary: N panes x L lines, one window each, 1 attached 200x50 client with sync on, 2 control clients; target < 1000 ms, decision 13)

| case | binary | history lines | idle s | answered ms | redrawn ms | snapshot MB | RSS MB before | RSS MB after |
|---|---|---|---|---|---|---|---|---|
| 1 x 10k fresh | vtmux | 9952 | 0.0 | 123 | 125 | 0.9 | 29.6 | 32.8 |
| 1 x 10k idle | vtmux | 9952 | 9.0 | 42 | 43 | 1.3 | 15.6 | 19.8 |
| 1 x 100k fresh | vtmux | 99952 | 0.0 | 1036 | 1038 | 6.0 | 169.3 | 182.5 |
| 1 x 100k idle | vtmux | 99952 | 18.0 | 186 | 188 | 11.5 | 27.4 | 51.0 |
| 10 x 10k fresh | vtmux | 99520 | 0.0 | 1040 | 1042 | 6.1 | 169.9 | 183.6 |
| 10 x 10k idle | vtmux | 99520 | 12.5 | 206 | 208 | 11.3 | 30.7 | 54.2 |
| 10 x 100k fresh | vtmux | 999520 | 0.0 | - | - | - | 1566.7 | - |
| 10 x 100k idle | vtmux | 999520 | 22.0 | 1703 | 1705 | 113.1 | 145.5 | 364.3 |

10 x 100k fresh: 3 of 3 runs refused: <vtmux> --vtmux-check-snapshot took over 3 s

## After

`NO_BUILD=1 RUNS=3 UPGRADE_STATES="fresh idle fresh-rows idle-rows" VTMUX=<lu_upgrade_perf release> python3 scripts/bench.py upgrade`

2026-11-07 19:16 UTC, commit 1b0dba1+dirty, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 5.8, median of 3; tmux: tmux 3.8; vtmux: vtmux 3.8-beta1

### upgrade (vtmux-upgrade onto the same binary: N panes x L lines, one window each, 1 attached 200x50 client with sync on, 2 control clients; target < 1000 ms, decision 13)

| case | binary | history lines | idle s | answered ms | redrawn ms | snapshot MB | RSS MB before | RSS MB after |
|---|---|---|---|---|---|---|---|---|
| 1 x 10k fresh | vtmux | 9952 | 0.0 | 39 | 41 | 1.3 | 29.5 | 16.6 |
| 1 x 10k idle | vtmux | 9952 | 9.0 | 31 | 33 | 1.3 | 15.5 | 16.7 |
| 1 x 10k fresh-rows | vtmux | 9952 | 0.0 | 130 | 132 | 0.8 | 29.5 | 30.8 |
| 1 x 10k idle-rows | vtmux | 9952 | 9.0 | 134 | 136 | 0.8 | 15.5 | 30.8 |
| 1 x 100k fresh | vtmux | 99952 | 0.0 | 161 | 163 | 11.5 | 169.4 | 28.3 |
| 1 x 100k idle | vtmux | 99952 | 18.0 | 81 | 82 | 11.5 | 27.3 | 28.4 |
| 1 x 100k fresh-rows | vtmux | 99952 | 0.0 | 1019 | 1021 | 5.8 | 169.4 | 170.3 |
| 1 x 100k idle-rows | vtmux | 99952 | 18.0 | 1106 | 1108 | 5.8 | 27.3 | 170.3 |
| 10 x 10k fresh | vtmux | 99520 | 0.0 | 117 | 119 | 11.3 | 170.0 | 31.2 |
| 10 x 10k idle | vtmux | 99520 | 12.5 | 100 | 101 | 11.3 | 30.5 | 31.2 |
| 10 x 10k fresh-rows | vtmux | 99520 | 0.0 | 1030 | 1032 | 5.9 | 169.9 | 170.8 |
| 10 x 10k idle-rows | vtmux | 99520 | 12.5 | 1063 | 1065 | 5.9 | 30.6 | 170.7 |
| 10 x 100k fresh | vtmux | 999520 | 0.0 | 749 | 751 | 113.1 | 1566.7 | 146.5 |
| 10 x 100k idle | vtmux | 999520 | 22.5 | 596 | 598 | 113.1 | 145.6 | 146.4 |
| 10 x 100k fresh-rows | vtmux | 999520 | 0.0 | 9869 | 9871 | 56.7 | 1566.8 | 1567.7 |
| 10 x 100k idle-rows | vtmux | 999520 | 22.0 | 10422 | 10424 | 56.7 | 145.6 | 1567.7 |

## Perf gate: agent_panes and key_echo

`NO_BUILD=1 AGENT_PANES="5 20" AGENT_CLIENTS="1 2" VTMUX=<binary> python3 scripts/bench.py agent_panes key_echo`,
before then after. Nothing on these paths changed (the new compression runs only inside an upgrade; the
checksum only in blob export and import); the differences follow the reference tmux's, which did not change
either: host noise.

### Before (1b0dba1)

2026-11-07 19:26 UTC, commit 1b0dba1+dirty, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 5.0, median of 3; tmux: tmux 3.8; vtmux: vtmux 3.8-beta1

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 263 | 6.1 |
| 5 panes / 1 clients | vtmux | 1.5 | 0.2 | 14.3 | 68 | 233 | 18.6 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 260 | 6.2 |
| 5 panes / 2 clients | vtmux | 2.0 | 0.3 | 28.2 | 134 | 293 | 21.1 |
| 20 panes / 1 clients | tmux | 4.4 | 0.0 | 15.2 | 68 | 856 | 8.0 |
| 20 panes / 1 clients | vtmux | 3.8 | 0.2 | 14.3 | 68 | 712 | 32.0 |
| 20 panes / 2 clients | tmux | 4.7 | 0.0 | 30.5 | 136 | 855 | 8.1 |
| 20 panes / 2 clients | vtmux | 4.7 | 0.3 | 28.3 | 135 | 768 | 34.7 |

### key_echo (200 keys 30 ms apart into a raw echo pane, one 200x50 client, sync on, the other pane idle or running `yes`; arrival stamped in the reader thread)

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.23 | 0.28 | 0.35 | 0 |
| flood none | vtmux | 0.36 | 0.43 | 0.59 | 0 |
| flood yes | tmux | 0.51 | 0.80 | 2.21 | 0 |
| flood yes | vtmux | 0.45 | 0.65 | 3.80 | 0 |

### After (lu_upgrade_perf)

2026-11-07 19:33 UTC, commit 1b0dba1+dirty, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 7.3, median of 3; tmux: tmux 3.8; vtmux: vtmux 3.8-beta1

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 261 | 6.0 |
| 5 panes / 1 clients | vtmux | 1.6 | 0.2 | 14.3 | 68 | 233 | 18.6 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 263 | 6.2 |
| 5 panes / 2 clients | vtmux | 2.1 | 0.3 | 28.3 | 134 | 288 | 21.1 |
| 20 panes / 1 clients | tmux | 5.1 | 0.0 | 15.2 | 68 | 849 | 8.0 |
| 20 panes / 1 clients | vtmux | 4.7 | 0.2 | 14.3 | 68 | 699 | 32.0 |
| 20 panes / 2 clients | tmux | 6.0 | 0.0 | 30.5 | 135 | 840 | 8.1 |
| 20 panes / 2 clients | vtmux | 6.3 | 0.4 | 28.2 | 135 | 745 | 34.5 |

### key_echo (200 keys 30 ms apart into a raw echo pane, one 200x50 client, sync on, the other pane idle or running `yes`; arrival stamped in the reader thread)

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.24 | 0.30 | 0.40 | 0 |
| flood none | vtmux | 0.37 | 0.43 | 0.60 | 0 |
| flood yes | tmux | 0.45 | 0.85 | 2.83 | 0 |
| flood yes | vtmux | 0.50 | 0.86 | 6.94 | 0 |
