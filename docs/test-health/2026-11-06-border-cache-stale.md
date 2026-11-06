# Border styles per client, 2026-11-06

medium_border_cache_stale: the border style cache (perf_border_style_cache) kept per client, and expanded again when a pane switches screens or dies under `remain-on-exit`. `base` is fefa2d4 (the cache, one entry per pane), `vtmux` the fix on top of it. Run: `RUNS=5 NO_BUILD=1 AGENT_PANES="5 20" AGENT_CLIENTS="1 2" BASE=<fefa2d4 build> VTMUX=<fix build> python3 scripts/bench.py agent_panes`, under `flock /tmp/vtmux-stress.lock`, raw JSON in `2026-11-06-border-cache-stale.json`. Server CPU % is CPU seconds per 100 s.

2026-11-06 10:09 UTC, commit fefa2d4+dirty, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 5.0, median of 5; tmux: tmux 3.8; base: vtmux 0.0.0 (tmux 3.8); vtmux: vtmux 0.0.0 (tmux 3.8)

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 261 | 6.1 |
| 5 panes / 1 clients | base | 1.4 | 0.2 | 14.3 | 68 | 233 | 17.7 |
| 5 panes / 1 clients | vtmux | 1.5 | 0.2 | 14.3 | 68 | 233 | 17.8 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 263 | 6.2 |
| 5 panes / 2 clients | base | 2.1 | 0.3 | 28.2 | 134 | 291 | 20.2 |
| 5 panes / 2 clients | vtmux | 2.1 | 0.3 | 28.2 | 135 | 291 | 20.2 |
| 20 panes / 1 clients | tmux | 4.9 | 0.0 | 15.2 | 68 | 845 | 8.0 |
| 20 panes / 1 clients | base | 3.8 | 0.2 | 14.3 | 68 | 712 | 31.2 |
| 20 panes / 1 clients | vtmux | 3.6 | 0.2 | 14.3 | 68 | 712 | 31.2 |
| 20 panes / 2 clients | tmux | 4.6 | 0.0 | 30.5 | 135 | 854 | 8.1 |
| 20 panes / 2 clients | base | 4.6 | 0.3 | 28.3 | 135 | 766 | 33.6 |
| 20 panes / 2 clients | vtmux | 4.8 | 0.3 | 28.3 | 135 | 765 | 33.8 |

vtmux / tmux, server CPU (unrounded): 5 / 1 1.06x, 5 / 2 1.38x, 20 / 1 0.73x, 20 / 2 1.03x (base: 1.02x, 1.37x, 0.77x, 1.00x). vtmux / base: 1.04x, 1.01x, 0.95x, 1.03x: no change past the run-to-run noise (tmux alone moves about 0.1x between runs, *2026-11-06-border-style-cache.md*). The second client expands each pane's style once per border redraw of its own, not once per frame, so the per-client cache costs nothing measurable; the *Agent panes* table in PERFORMANCE.md keeps the fefa2d4 run.
