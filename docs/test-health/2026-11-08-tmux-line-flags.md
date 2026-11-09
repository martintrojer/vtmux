# Ghostty patch 0034 (tmux line flags), 2026-11-08

regress_xfail_d027: patch 0034 keeps tmux's GRID_LINE_EXTENDED and GRID_LINE_HYPERLINK per row, set on the print paths (`printCell`, `writeCell`, the `printSlice` fast path, grapheme attaches, tabs, RGB erases) and in the reflow, so every print does a little more. `base` is main at 0107471 (release build), `vtmux` the change on top of it. Run: `RUNS=5 NO_BUILD=1 AGENT_PANES="5 20" AGENT_CLIENTS="1 2" BASE=<0107471 build> python3 scripts/bench.py agent_panes key_echo throughput`, under `flock /tmp/vtmux-stress.lock`, on a shared 96-core host at load 17 (other workers' jobs), raw JSON in `2026-11-08-tmux-line-flags.json`. Server CPU % is CPU seconds per 100 s.

2026-11-09 00:35 UTC, commit 550d0d9+dirty, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 17.4, median of 5; tmux: tmux 3.8; base: vtmux 3.8-beta1; vtmux: vtmux 3.8-beta1

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.5 | 0.0 | 15.2 | 68 | 262 | 6.1 |
| 5 panes / 1 clients | base | 1.6 | 0.2 | 14.3 | 68 | 231 | 19.1 |
| 5 panes / 1 clients | vtmux | 1.6 | 0.2 | 14.3 | 68 | 233 | 19.3 |
| 5 panes / 2 clients | tmux | 1.6 | 0.0 | 30.4 | 135 | 262 | 6.2 |
| 5 panes / 2 clients | base | 2.2 | 0.3 | 28.2 | 134 | 290 | 21.6 |
| 5 panes / 2 clients | vtmux | 2.3 | 0.4 | 28.2 | 134 | 287 | 21.7 |
| 20 panes / 1 clients | tmux | 5.0 | 0.0 | 15.2 | 68 | 858 | 8.0 |
| 20 panes / 1 clients | base | 3.8 | 0.2 | 14.3 | 68 | 714 | 32.5 |
| 20 panes / 1 clients | vtmux | 3.9 | 0.2 | 14.3 | 68 | 713 | 32.5 |
| 20 panes / 2 clients | tmux | 4.7 | 0.0 | 30.5 | 135 | 862 | 8.1 |
| 20 panes / 2 clients | base | 4.9 | 0.3 | 28.3 | 135 | 761 | 34.8 |
| 20 panes / 2 clients | vtmux | 5.0 | 0.3 | 28.3 | 135 | 764 | 34.9 |

### key_echo (200 keys 30 ms apart into a raw echo pane, one 200x50 client, sync on, the other pane idle or running `yes`; arrival stamped in the reader thread)

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.22 | 0.29 | 0.40 | 0 |
| flood none | base | 0.33 | 0.43 | 0.71 | 0 |
| flood none | vtmux | 0.32 | 0.41 | 0.64 | 0 |
| flood yes | tmux | 0.52 | 0.71 | 2.54 | 0 |
| flood yes | base | 0.47 | 0.62 | 3.60 | 0 |
| flood yes | vtmux | 0.45 | 0.64 | 5.47 | 0 |

### throughput without a client (100 MB log text, 120x40 pane, default history; sgr bytes = 19%)

| input | binary | wall s | server CPU s | MB/s | peak RSS MB |
|---|---|---|---|---|---|
| plain | tmux | 2.45 | 2.39 | 42.9 | 5.4 |
| plain | base | 1.22 | 0.99 | 85.7 | 13.1 |
| plain | vtmux | 1.22 | 1.03 | 85.6 | 13.3 |
| sgr | tmux | 3.26 | 3.11 | 32.2 | 5.4 |
| sgr | base | 1.63 | 1.52 | 64.3 | 12.8 |
| sgr | vtmux | 1.63 | 1.52 | 64.3 | 12.5 |

vtmux / base, server CPU (unrounded): agent_panes 5 / 1 0.96x, 5 / 2 1.05x, 20 / 1 1.03x, 20 / 2 1.03x; throughput 1.04x plain, 1.00x SGR. key_echo p50 0.32 / 0.33 ms idle, 0.45 / 0.47 ms beside `yes`. An earlier run of the same (RUNS=3, load 19) gave agent_panes 1.00x, 0.96x, 1.08x, 1.02x and throughput 0.72x plain (the base run was the slow one), 1.01x SGR. Both runs move either way by up to 8%, as tmux's own rows do between runs: no change past the noise.
