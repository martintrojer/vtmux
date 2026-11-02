# Perf run, attached clients, render_flood_cpu round 2, 2026-11-02

Command: `OUT=target/bench/r2-attached NO_BUILD=1 RUNS=3 flock -w 1800 /tmp/vtmux-stress.lock mise exec -- python3 scripts/bench.py throughput_attached render many_clients` (defaults: 100 MB log, 3000 frames per pane, 20 MB flood), release build of commit 4a1bcf4 (clean tree, on main eca18c6), which includes the status-right / status-interval pin (7d9976e) and the sync-frame counter fix (f636e4d). Load 2.0 at start, 1.1 at the end (`uptime` before and after). Replaces the earlier records' absolute numbers: [`2026-11-02-perf-attached.md`](2026-11-02-perf-attached.md) (96dc4ad, before the pin, load 7-9) and [`2026-11-02-render-flood.md`](2026-11-02-render-flood.md) (e1b2ae7, load 2.2-3.7, before the counter fix).

What changed since round 1: a quiet pane's frame is not paced by a flood elsewhere, passthrough is written while a frame is deferred, and a command client's exit follows the turn's frames. None of these is in the flood path, so the rows match round 1 within noise.

Key-to-echo latency (`/tmp/rv_rfc/lat.py`: 200x50 client on a draining PTY, window split -h, `yes` in the left pane, a raw echo program in the right, 60 single-key probes 30 ms apart; load 1.0-1.1, two runs each, interleaved):

| flood | binary | p50 ms | p90 ms | max ms |
|---|---|---|---|---|
| `yes` in the other pane | tmux 3.8 | 0.8 / 0.8 | 0.8 / 0.8 | 1.0 / 1.0 |
| | vtmux round 1 (7a293f6) | 10.4 / 8.0 | 13.9 / 15.7 | 16.4 / 16.5 |
| | vtmux 4a1bcf4 | 2.8 / 3.1 | 7.0 / 5.9 | 7.9 / 8.2 |
| none | tmux 3.8 | 0.3 / 0.3 | 0.6 / 0.6 | 1.0 / 0.9 |
| | vtmux round 1 | 5.4 / 5.4 | 5.9 / 5.8 | 6.4 / 6.2 |
| | vtmux 4a1bcf4 | 5.0 / 4.7 | 5.9 / 5.1 | 6.4 / 5.5 |

The no-flood row is the same for both vtmux builds: 5 ms per echo at 200x50 is the cost of composing and diffing one whole frame (a split timing put 2.5 ms of it between the pane's write and the terminal at 100x30), not pacing. It is unchanged by this work.

2026-11-02 21:31 UTC, commit 4a1bcf4, Linux-7.2.11-200.fc44.x86_64-x86_64-with-glibc2.43, AMD Ryzen 7 5700X3D 8-Core Processor (16 cpus), load 2.0, median of 3; tmux: tmux 3.8; vtmux: vtmux 0.0.0 (tmux 3.8)

### throughput_attached (100 MB log, one 200x50 client, window-size latest)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | term bytes / MB in | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| plain / sync off | tmux | 2.84 | 2.72 | 0.00 | 2.72 | 1034350 | 0 | 7.5 |
| plain / sync off | vtmux | 1.44 | 1.44 | 0.01 | 1.45 | 53591 | 0 | 15.6 |
| plain / sync on | tmux | 2.84 | 2.76 | 0.00 | 2.76 | 1038289 | 25815 | 7.5 |
| plain / sync on | vtmux | 1.45 | 1.42 | 0.01 | 1.43 | 55086 | 133 | 15.7 |
| sgr / sync off | tmux | 4.06 | 3.85 | 0.00 | 3.85 | 1043260 | 0 | 7.5 |
| sgr / sync off | vtmux | 3.69 | 3.48 | 0.02 | 3.51 | 127552 | 0 | 15.2 |
| sgr / sync on | tmux | 4.06 | 3.83 | 0.00 | 3.83 | 1047184 | 25776 | 7.5 |
| sgr / sync on | vtmux | 3.68 | 3.51 | 0.02 | 3.53 | 124852 | 271 | 15.5 |

### render (4 panes in a 200x50 client, each cat'ing 3000 cursor-addressed 95x22 frames; 22 MB of input)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | terminal MB | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 0.66 | 0.63 | 0.00 | 0.63 | 10.09 | 0 | 5.4 |
| sync off | vtmux | 0.45 | 0.40 | 0.00 | 0.40 | 0.49 | 0 | 11.4 |
| sync on | tmux | 0.66 | 0.64 | 0.00 | 0.64 | 10.11 | 1357 | 5.4 |
| sync on | vtmux | 0.46 | 0.41 | 0.00 | 0.41 | 0.51 | 34 | 11.4 |

### many_clients (1 session, 8 clients 80x24, 100x30, 120x40, 132x43, 160x50, 200x50, 90x35, 240x60, window-size smallest, 20 MB flood in one of 2 panes)

| case | binary | wall s | server CPU s | clients CPU s (sum) | total CPU s | terminal MB (sum) | RSS MB (idle, attached) | RSS MB (after) | peak RSS MB |
|---|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 1.43 | 1.28 | 0.00 | 1.28 | 29.1 | 6.5 | 7.3 | 7.3 |
| sync off | vtmux | 0.87 | 0.76 | 0.01 | 0.77 | 2.2 | 14.0 | 17.7 | 19.8 |
| sync on | tmux | 5.17 | 5.10 | 0.00 | 5.10 | 189.3 | 6.3 | 7.3 | 7.3 |
| sync on | vtmux | 0.87 | 0.83 | 0.01 | 0.85 | 2.5 | 14.4 | 17.4 | 19.8 |

