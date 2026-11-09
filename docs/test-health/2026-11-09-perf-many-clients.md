# Many clients on one window: shared drawings and frames (`perf_many_clients`)

Task `perf_many_clients`: clients that show one window alike share its drawing and frame (`crates/vtmux-server/src/render/share.rs`). Three commits on main `0107471`:

- `9f5991d` the drawing (`Source::window`, the blend, the composed rows) and the frame bytes are shared within a render turn;
- `8cddcba` a plain frame's status rows are tagged by their cells, so they are kept and compared by tag;
- `417a3c4` a client takes another's plain frame without drawing it, the screen it leaves shared.

Every number here is from release builds on the shared devserver (CentOS Stream 9, Linux 6.16, AMD EPYC Genoa, 96 cpus), `base` = main `0107471`, `vtmux` = `417a3c4` unless a row says otherwise, tmux 3.8 from `cargo xtask ref-tmux`. The host was shared with other workers: load 9-27 over the runs. Read the CPU numbers as rough; the ratios between binaries of one run are what counts.

## `bench-micro.py clients`, before and after

`timeout 1800 flock /tmp/vtmux-stress.lock env RUNS=3 CLIENTS_N="1 2 5 10 16 32" BASE=<main 0107471> NO_BUILD=1 VTMUX=<417a3c4> python3 scripts/bench-micro.py clients`, 00:11-00:22 UTC, load 18.7 at the start, 9.5 at the end. N 120x40 clients (sync on) on one session of 4 `agent_tui.py` panes in one window, `window-size smallest`; median of 3; server CPU in seconds per 10 s.

```
load 18.7, runs 3
== clients
4 agent panes / 1 clients                     tmux   server_cpu_s_per_10s=0.168 frames_per_s=134 term_kb_per_s=26.3 rss_mb=5.05 wakeups_per_s=183
4 agent panes / 1 clients                     base   server_cpu_s_per_10s=0.284 frames_per_s=134 term_kb_per_s=27.6 rss_mb=13.8 wakeups_per_s=259
4 agent panes / 1 clients                     vtmux  server_cpu_s_per_10s=0.278 frames_per_s=134 term_kb_per_s=27.6 rss_mb=13.5 wakeups_per_s=259
4 agent panes / 2 clients                     tmux   server_cpu_s_per_10s=0.164 frames_per_s=269 term_kb_per_s=52.6 rss_mb=5.1 wakeups_per_s=190
4 agent panes / 2 clients                     base   server_cpu_s_per_10s=0.319 frames_per_s=269 term_kb_per_s=55.1 rss_mb=14.8 wakeups_per_s=189
4 agent panes / 2 clients                     vtmux  server_cpu_s_per_10s=0.278 frames_per_s=269 term_kb_per_s=55.2 rss_mb=14.6 wakeups_per_s=235
4 agent panes / 5 clients                     tmux   server_cpu_s_per_10s=0.198 frames_per_s=674 term_kb_per_s=132 rss_mb=5.55 wakeups_per_s=187
4 agent panes / 5 clients                     base   server_cpu_s_per_10s=0.589 frames_per_s=671 term_kb_per_s=137 rss_mb=18.8 wakeups_per_s=129
4 agent panes / 5 clients                     vtmux  server_cpu_s_per_10s=0.338 frames_per_s=673 term_kb_per_s=137 rss_mb=16.4 wakeups_per_s=137
4 agent panes / 10 clients                    tmux   server_cpu_s_per_10s=0.25 frames_per_s=1.35e+03 term_kb_per_s=262 rss_mb=6.28 wakeups_per_s=187
4 agent panes / 10 clients                    base   server_cpu_s_per_10s=0.898 frames_per_s=1.34e+03 term_kb_per_s=274 rss_mb=24.7 wakeups_per_s=120
4 agent panes / 10 clients                    vtmux  server_cpu_s_per_10s=0.404 frames_per_s=1.34e+03 term_kb_per_s=275 rss_mb=19.8 wakeups_per_s=131
4 agent panes / 16 clients                    tmux   server_cpu_s_per_10s=0.308 frames_per_s=2.15e+03 term_kb_per_s=419 rss_mb=7.05 wakeups_per_s=186
4 agent panes / 16 clients                    base   server_cpu_s_per_10s=1.34 frames_per_s=2.14e+03 term_kb_per_s=439 rss_mb=32.4 wakeups_per_s=119
4 agent panes / 16 clients                    vtmux  server_cpu_s_per_10s=0.508 frames_per_s=2.15e+03 term_kb_per_s=440 rss_mb=25.8 wakeups_per_s=130
4 agent panes / 32 clients                    tmux   server_cpu_s_per_10s=0.484 frames_per_s=4.3e+03 term_kb_per_s=839 rss_mb=9.16 wakeups_per_s=181
4 agent panes / 32 clients                    base   server_cpu_s_per_10s=2.5 frames_per_s=4.22e+03 term_kb_per_s=874 rss_mb=52.4 wakeups_per_s=108
4 agent panes / 32 clients                    vtmux  server_cpu_s_per_10s=0.789 frames_per_s=4.29e+03 term_kb_per_s=881 rss_mb=41.3 wakeups_per_s=128
```

| clients | tmux | base | vtmux | base / tmux | vtmux / tmux | vtmux / base |
|---|---|---|---|---|---|---|
| 1 | 0.17 | 0.28 | 0.28 | 1.7x | 1.7x | 0.98x |
| 2 | 0.16 | 0.32 | 0.28 | 1.9x | 1.7x | 0.87x |
| 5 | 0.20 | 0.59 | 0.34 | 3.0x | 1.7x | 0.57x |
| 10 | 0.25 | 0.90 | 0.40 | 3.6x | 1.6x | 0.45x |
| 16 | 0.31 | 1.34 | 0.51 | 4.4x | 1.65x | 0.38x |
| 32 | 0.48 | 2.50 | 0.79 | 5.2x | 1.6x | 0.32x |

Terminal bytes and frames per second are the same for base and vtmux in every row: the change is in how a frame is made, not in what is sent. RSS fell too (52.4 to 41.3 MB at 32 clients): clients that share a drawing share its composed rows.

The same run at `9f5991d` (the first commit alone), 2026-11-08 23:22-23:34 UTC, load 13.9 to 16.5: 1 client 0.231, 2 0.283, 5 0.39, 10 0.525, 16 0.663, 32 1.17 s per 10 s (base 0.221, 0.315, 0.573, 0.918, 1.41, 2.64; tmux 0.15, 0.157, 0.195, 0.24, 0.339, 0.532).

Single runs of `/tmp` probe scripts between the commits (10 and 32 clients, 8 s, one server at a time): `9f5991d` 0.52-0.54 s at 10; `8cddcba` 0.44-0.45 at 10, 0.97 at 32; `417a3c4` 0.42 at 10, 0.85 at 32.

## Where the time goes at 32 clients

`perf record -g` of the server for 8 s, a `release` build with frame pointers and line tables, Ghostty `ReleaseFast`.

- main `0107471`, 10 clients: `render_clients` 78% of the samples; per client the window drawing (`Source::window` 18%), composing its rows (20%), copying them into its buffer, the frame diff (14%).
- `9f5991d`, 32 clients: copying the composed rows into each client's buffer (`copy_rows` and `Cell::clone_from`, 14%), comparing each client's status row with the first client's twice (8.5%), writing to the sockets (12%).
- `417a3c4`, 32 clients: `render_clients` 64%, of which writing the frames to the clients' sockets 19%, the window drawing 13% (mostly the first client's: `Source::window` 4%, composing 3.4%; the cursor 2.2% per client), taking a frame without drawing it 5.5%, drawing a frame whole 5.2%, the terminal's mouse modes 4.1% per client. Reading the clients' frame acknowledgements is 10%, pane output 8.5%.

## PERF GATE: `bench.py agent_panes` and `key_echo`

`timeout 2400 flock /tmp/vtmux-stress.lock env RUNS=3 AGENT_PANES="5 20" AGENT_CLIENTS="1 2" BASE=<main 0107471> NO_BUILD=1 VTMUX=<417a3c4> python3 scripts/bench.py agent_panes key_echo`, 00:22-00:32 UTC. JSON in `2026-11-09-perf-many-clients.json`. The second client is on another window here, so nothing is shared: these rows check that nothing got slower.

2026-11-09 00:22 UTC, commit 417a3c4, load 9.6, median of 3.

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.9 | 0.0 | 15.2 | 68 | 261 | 6.1 |
| 5 panes / 1 clients | base | 2.2 | 0.2 | 14.3 | 68 | 231 | 19.2 |
| 5 panes / 1 clients | vtmux | 2.2 | 0.2 | 14.3 | 68 | 231 | 19.0 |
| 5 panes / 2 clients | tmux | 2.0 | 0.0 | 30.4 | 135 | 261 | 6.2 |
| 5 panes / 2 clients | base | 2.8 | 0.4 | 28.2 | 135 | 285 | 21.5 |
| 5 panes / 2 clients | vtmux | 2.9 | 0.4 | 28.2 | 134 | 281 | 21.5 |
| 20 panes / 1 clients | tmux | 5.5 | 0.0 | 15.2 | 68 | 846 | 8.0 |
| 20 panes / 1 clients | base | 5.1 | 0.2 | 14.3 | 68 | 704 | 32.5 |
| 20 panes / 1 clients | vtmux | 5.2 | 0.2 | 14.3 | 68 | 706 | 32.4 |
| 20 panes / 2 clients | tmux | 5.5 | 0.0 | 30.5 | 135 | 848 | 8.1 |
| 20 panes / 2 clients | base | 5.9 | 0.4 | 28.3 | 134 | 751 | 34.8 |
| 20 panes / 2 clients | vtmux | 5.9 | 0.4 | 28.3 | 135 | 751 | 35.1 |

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.28 | 0.32 | 0.44 | 0 |
| flood none | base | 0.43 | 0.48 | 0.63 | 0 |
| flood none | vtmux | 0.42 | 0.47 | 0.64 | 0 |
| flood yes | tmux | 0.50 | 0.83 | 1.95 | 0 |
| flood yes | base | 0.52 | 0.72 | 4.67 | 0 |
| flood yes | vtmux | 0.53 | 0.70 | 3.22 | 0 |

The same gate at `8cddcba`, 23:44-23:54 UTC, load 13.8 to 15.9: agent panes 2.5/2.7 (base/vtmux) at 5 / 1, 3.2/3.0 at 5 / 2, 5.2/5.3 at 20 / 1, 6.3/6.1 at 20 / 2; key echo p50 0.43/0.43 ms idle, 0.55/0.55 ms next to `yes`. Within the noise in both runs.
