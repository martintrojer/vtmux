# Perf run, attached clients, 2026-11-02 (perf_attached)

Command: `flock /tmp/vtmux-stress.lock mise exec -- python3 scripts/bench.py throughput_attached render many_clients` (defaults: RUNS=3, 100 MB log, 3000 frames per pane, 20 MB flood), release build of commit 96dc4ad (rebased on main 34b5a53).
Host: Fedora 44, Linux 7.2.11 x86_64, AMD Ryzen 7 5700X3D (16 cpus). Load 4.0 at start (the waiter started it at the first sample under 4); other workers' gates pushed it to 7-9 during the run, so absolute times are noisy. The ratios below are far larger than that noise. Reference: tmux 3.8 from `cargo xtask ref-tmux`.
Triage filed: `perf_attached_throughput`, `perf_attached_many_clients`, `perf_attached_render_cpu`.

### throughput_attached (100 MB log, one 200x50 client, window-size latest)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | term bytes / MB in | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| plain / sync off | tmux | 2.84 | 2.66 | 0.00 | 2.66 | 1034350 | 0 | 7.5 |
| plain / sync off | vtmux | 16.15 | 15.99 | 0.38 | 16.37 | 2153433 | 0 | 16.2 |
| plain / sync on | tmux | 2.85 | 2.77 | 0.00 | 2.77 | 1038288 | 25817 | 7.5 |
| plain / sync on | vtmux | 18.88 | 18.57 | 0.52 | 19.10 | 2609945 | 6276 | 16.1 |
| sgr / sync off | tmux | 4.07 | 3.91 | 0.00 | 3.91 | 1043264 | 0 | 7.5 |
| sgr / sync off | vtmux | 38.56 | 38.20 | 1.07 | 39.27 | 5717945 | 0 | 15.9 |
| sgr / sync on | tmux | 6.34 | 5.92 | 0.00 | 5.92 | 1047239 | 25886 | 7.6 |
| sgr / sync on | vtmux | 42.05 | 41.49 | 1.21 | 42.70 | 5725536 | 12368 | 15.7 |

### render (4 panes in a 200x50 client, each cat'ing 3000 cursor-addressed 95x22 frames; 22 MB of input)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | terminal MB | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 0.89 | 0.84 | 0.00 | 0.84 | 10.09 | 0 | 5.4 |
| sync off | vtmux | 2.05 | 1.98 | 0.02 | 2.00 | 4.92 | 0 | 11.9 |
| sync on | tmux | 0.88 | 0.79 | 0.00 | 0.79 | 10.11 | 1358 | 5.4 |
| sync on | vtmux | 2.03 | 1.85 | 0.02 | 1.87 | 4.93 | 679 | 11.6 |

### many_clients (1 session, 8 clients 80x24, 100x30, 120x40, 132x43, 160x50, 200x50, 90x35, 240x60, window-size smallest, 20 MB flood in one of 2 panes)

| case | binary | wall s | server CPU s | clients CPU s (sum) | total CPU s | terminal MB (sum) | RSS MB (idle, attached) | RSS MB (after) | peak RSS MB |
|---|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 1.43 | 1.36 | 0.00 | 1.36 | 29.1 | 6.5 | 7.3 | 7.3 |
| sync off | vtmux | 15.09 | 15.04 | 0.38 | 15.42 | 61.9 | 14.9 | 16.5 | 20.5 |
| sync on | tmux | 5.59 | 5.53 | 0.00 | 5.53 | 192.6 | 6.4 | 7.4 | 7.4 |
| sync on | vtmux | 15.55 | 15.46 | 0.40 | 15.86 | 64.1 | 15.3 | 18.0 | 21.0 |
