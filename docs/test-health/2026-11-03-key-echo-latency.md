# Key echo latency and attached flood, key_echo_latency, 2026-11-03

Branch `key_echo_latency` (dirty-row pane views, cached scene and composed rows, frame buffer reuse, theme cache, plain-frame skip) on main 9b6d58c, measured at commit 1814030 (rebased later onto 40d8bb1, whose new commit touches attach and new-session, not rendering). Release builds: vtmux = this branch (commit 1814030), base = 5de1824 (main before the work, `/tmp/kel/vtmux-base`), tmux = 3.8 (`cargo xtask ref-tmux`). Linux 7.2.11 (Fedora 44), AMD Ryzen 7 5700X3D (16 cpus).

## Key-to-echo latency

Two harnesses, both: 200x50 client on a draining PTY, `-f /dev/null`, window split -h, the right pane a raw echo program (each byte: `\r ESC[K` and the byte), the left pane `yes` or `cat`, `status-right` pinned, `status-interval 0`, `escape-time 0`, keys 30 ms apart. Run under `flock /tmp/vtmux-stress.lock`, tmux and vtmux interleaved, two runs each, load 1.3-1.8.

- `lat.py`: the reviewer's harness from render_flood_cpu, 60 keys. The reader polls every 0.2 ms, so a sample is the arrival rounded up to that grid.
- `lat2.py`: the same with the arrival time stamped in the reader thread when the echo's bytes are read, 200 keys.

| harness | flood | tmux 3.8 p50 / p90 / max ms | vtmux p50 / p90 / max ms | vtmux / tmux (p50) | base p50 / p90 / max ms |
|---|---|---|---|---|---|
| lat.py | none | 0.3 / 0.3-0.6 / 0.6 | 0.6 / 0.8 / 0.9-1.1 | 2.0x | 4.9 / 5.4 / 5.6 |
| lat.py | `yes` | 0.8 / 0.8 / 0.8 | 1.0 / 1.0-1.3 / 1.3-1.9 | 1.25x | 2.8-3.1 / 5.9-7.0 / 7.9-8.2 (r2 record) |
| lat2.py | none | 0.14-0.18 / 0.27-0.30 / 0.4-0.5 | 0.50-0.54 / 0.62-0.63 / 0.9-1.1 | 2.8-3.9x | 4.63 / 5.19 / 6.6 |
| lat2.py | `yes` | 0.55-0.56 / 0.65-0.66 / 0.8-1.0 | 0.87-0.89 / 1.03 / 1.6-2.0 | 1.6x | 2.78 / 7.17 / 7.9 |

The brief's bar (echo p50 within 2x tmux with no flood) is met with `lat.py` (0.6 vs 0.3 ms, at the bar; the parked run on the same branch had 0.5 vs 0.3) and not with `lat2.py` (0.50-0.54 vs 0.14-0.18 ms, 2.8-3.9x). Against base the echo is about 9x faster with no flood and 3x faster next to a flood.

Where the remaining ~0.24 ms of a 200x50 echo frame goes (temporary `Instant` timers, from the parked profile; not in the tree): `Source::window` 43 us (status-line scans, pane contexts and styles), `compose_cached` 17 us (one row), `ComposeCache::copy_to` (10000-cell clone into the window buffer) 47 us, blanking that buffer 9 us, swapping it into the screen 21 us, status 14 us, `Compositor::frame` row compare and encode 52 us. Most of it is O(screen) copies and compares per frame, not compose. Follow-ups: keep a persistent screen buffer and copy only the rows `compose_cached` composed again (drops copy_to, blank and swap), and pass that row list to `frame()` so it compares only those rows and the status rows.

## Attached flood (bench.py)

Command: `OUT=target/bench/kel-attached NO_BUILD=1 RUNS=3 BASE=/tmp/kel/vtmux-base flock -w 1800 /tmp/vtmux-stress.lock mise exec -- python3 scripts/bench.py throughput_attached render many_clients`. Load 3.0 at start, 1.5 at the end. Raw JSON: [`2026-11-03-key-echo-latency.json`](2026-11-03-key-echo-latency.json).

Server CPU is lower than base in every row and stays under tmux: 0.44-0.47x plain, 0.79x SGR, 0.54x render, 0.22x and 0.06x with 8 clients (render_flood_cpu r2: 0.51-0.92x, 0.63x, 0.59x and 0.16x). With 8 clients it is 0.29 s against base's 0.74 s, because a client's frame now composes only changed rows. The cost is memory: one persistent Ghostty RenderState per pane and per-client row caches and frame buffers. Peak RSS goes up 2 MB with one client (15.6 to 17.4 MB) and 10-12 MB with 8 clients (19.9 to 29.6 MB peak; 13.8 to 25.6 MB attached and idle).

`bench.py --smoke` (load 1.3): every metric ok (idle RSS 7.79 MB, scale 14.06 MB, history 105.6 MB, CPU ratio plain 0.44x, SGR 0.85x against limit 1.02x).

2026-11-03 13:47 UTC, commit 1814030, Linux-7.2.11-200.fc44.x86_64-x86_64-with-glibc2.43, AMD Ryzen 7 5700X3D 8-Core Processor (16 cpus), load 3.0, median of 3; tmux: tmux 3.8; base: vtmux 0.0.0 (tmux 3.8); vtmux: vtmux 0.0.0 (tmux 3.8)

### throughput_attached (100 MB log, one 200x50 client, window-size latest)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | term bytes / MB in | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| plain / sync off | tmux | 2.64 | 2.58 | 0.00 | 2.58 | 1034350 | 0 | 7.5 |
| plain / sync off | base | 1.65 | 1.45 | 0.01 | 1.46 | 53233 | 0 | 15.6 |
| plain / sync off | vtmux | 1.23 | 1.14 | 0.01 | 1.15 | 48586 | 0 | 17.4 |
| plain / sync on | tmux | 2.64 | 2.50 | 0.00 | 2.50 | 1038291 | 25816 | 7.5 |
| plain / sync on | base | 1.44 | 1.41 | 0.01 | 1.42 | 56361 | 136 | 15.6 |
| plain / sync on | vtmux | 1.23 | 1.18 | 0.01 | 1.19 | 54408 | 132 | 17.5 |
| sgr / sync off | tmux | 3.86 | 3.80 | 0.00 | 3.80 | 1043250 | 0 | 7.6 |
| sgr / sync off | base | 3.48 | 3.43 | 0.02 | 3.46 | 122918 | 0 | 14.9 |
| sgr / sync off | vtmux | 3.06 | 3.00 | 0.02 | 3.02 | 122580 | 0 | 17.4 |
| sgr / sync on | tmux | 3.86 | 3.74 | 0.00 | 3.74 | 1047191 | 25776 | 7.5 |
| sgr / sync on | base | 3.68 | 3.50 | 0.02 | 3.52 | 125648 | 272 | 15.3 |
| sgr / sync on | vtmux | 3.06 | 2.97 | 0.02 | 2.99 | 121100 | 263 | 17.4 |

### render (4 panes in a 200x50 client, each cat'ing 3000 cursor-addressed 95x22 frames; 22 MB of input)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | terminal MB | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 0.66 | 0.63 | 0.00 | 0.63 | 10.09 | 0 | 5.4 |
| sync off | base | 0.45 | 0.39 | 0.00 | 0.39 | 0.52 | 0 | 11.2 |
| sync off | vtmux | 0.43 | 0.34 | 0.00 | 0.34 | 0.50 | 0 | 13.2 |
| sync on | tmux | 0.66 | 0.63 | 0.00 | 0.63 | 10.11 | 1358 | 5.4 |
| sync on | base | 0.45 | 0.38 | 0.00 | 0.38 | 0.49 | 32 | 11.1 |
| sync on | vtmux | 0.43 | 0.34 | 0.00 | 0.34 | 0.50 | 33 | 13.3 |

### many_clients (1 session, 8 clients 80x24, 100x30, 120x40, 132x43, 160x50, 200x50, 90x35, 240x60, window-size smallest, 20 MB flood in one of 2 panes)

| case | binary | wall s | server CPU s | clients CPU s (sum) | total CPU s | terminal MB (sum) | RSS MB (idle, attached) | RSS MB (after) | peak RSS MB |
|---|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 1.43 | 1.29 | 0.00 | 1.29 | 29.1 | 6.4 | 7.2 | 7.2 |
| sync off | base | 0.86 | 0.74 | 0.01 | 0.75 | 2.2 | 13.8 | 17.9 | 19.9 |
| sync off | vtmux | 0.41 | 0.29 | 0.01 | 0.29 | 1.2 | 25.6 | 29.6 | 29.6 |
| sync on | tmux | 5.37 | 5.14 | 0.00 | 5.14 | 190.0 | 6.5 | 7.4 | 7.4 |
| sync on | base | 0.86 | 0.74 | 0.01 | 0.75 | 2.2 | 13.9 | 18.8 | 19.9 |
| sync on | vtmux | 0.41 | 0.33 | 0.01 | 0.33 | 1.4 | 25.8 | 29.7 | 29.7 |

