# Perf run, attached clients, after render_flood_cpu, 2026-11-02

Command: `OUT=... NO_BUILD=1 RUNS=3 flock /tmp/vtmux-stress.lock mise exec -- python3 scripts/bench.py throughput_attached render many_clients` (defaults: 100 MB log, 3000 frames per pane, 20 MB flood), release build of commit e1b2ae7 (on main cbfcee0). Load 3.2 at start, 2.2-3.7 during. Before: [`2026-11-02-perf-attached.md`](2026-11-02-perf-attached.md) (commit 96dc4ad, same host, load 7-9 during).
What changed: a frame is deferred while the client has an unwritten one (tmux's `server_client_check_redraw`), and frames are at most one per 10 ms while a pane still has output waiting. The profile behind it is in the render_flood_cpu task note.

2026-11-02 12:15 UTC, commit e1b2ae7, Linux-7.2.11-200.fc44.x86_64-x86_64-with-glibc2.43, AMD Ryzen 7 5700X3D 8-Core Processor (16 cpus), load 3.2, median of 3; tmux: tmux 3.8; vtmux: vtmux 0.0.0 (tmux 3.8)

### throughput_attached (100 MB log, one 200x50 client, window-size latest)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | term bytes / MB in | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| plain / sync off | tmux | 2.84 | 2.70 | 0.00 | 2.70 | 1034348 | 0 | 7.5 |
| plain / sync off | vtmux | 1.44 | 1.39 | 0.01 | 1.39 | 49774 | 0 | 16.0 |
| plain / sync on | tmux | 2.84 | 2.73 | 0.00 | 2.73 | 1038286 | 25815 | 7.5 |
| plain / sync on | vtmux | 1.43 | 1.42 | 0.01 | 1.43 | 47465 | 115 | 16.1 |
| sgr / sync off | tmux | 4.06 | 3.86 | 0.00 | 3.86 | 1043257 | 0 | 7.6 |
| sgr / sync off | vtmux | 3.67 | 3.47 | 0.02 | 3.50 | 120611 | 0 | 15.6 |
| sgr / sync on | tmux | 3.86 | 3.82 | 0.00 | 3.82 | 1047188 | 25776 | 7.5 |
| sgr / sync on | vtmux | 3.68 | 3.50 | 0.02 | 3.52 | 122122 | 265 | 15.8 |

### render (4 panes in a 200x50 client, each cat'ing 3000 cursor-addressed 95x22 frames; 22 MB of input)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | terminal MB | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 0.66 | 0.65 | 0.00 | 0.65 | 10.09 | 0 | 5.5 |
| sync off | vtmux | 0.45 | 0.40 | 0.00 | 0.40 | 0.49 | 0 | 11.8 |
| sync on | tmux | 0.87 | 0.66 | 0.00 | 0.66 | 10.11 | 1357 | 5.4 |
| sync on | vtmux | 0.45 | 0.40 | 0.00 | 0.40 | 0.48 | 31 | 11.8 |

### many_clients (1 session, 8 clients 80x24, 100x30, 120x40, 132x43, 160x50, 200x50, 90x35, 240x60, window-size smallest, 20 MB flood in one of 2 panes)

| case | binary | wall s | server CPU s | clients CPU s (sum) | total CPU s | terminal MB (sum) | RSS MB (idle, attached) | RSS MB (after) | peak RSS MB |
|---|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 1.43 | 1.33 | 0.00 | 1.33 | 29.1 | 6.4 | 7.3 | 7.3 |
| sync off | vtmux | 0.86 | 0.81 | 0.01 | 0.83 | 2.2 | 15.1 | 18.1 | 20.2 |
| sync on | tmux | 5.17 | 5.12 | 0.00 | 5.12 | 189.0 | 6.4 | 7.4 | 7.4 |
| sync on | vtmux | 0.86 | 0.86 | 0.01 | 0.88 | 2.4 | 15.3 | 16.2 | 20.3 |
