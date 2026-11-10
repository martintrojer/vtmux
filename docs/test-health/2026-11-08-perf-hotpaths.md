# Performance hot paths, 2026-11-08

Task `audit_perf_hotpaths`. Host: the Linux server of *Method* (Linux 6.16, AMD EPYC Genoa, 96 cpus), shared with other agents' gates; load is given per run and was high for most of them (up to 118). Release builds of vtmux; tmux 3.8 from `cargo xtask ref-tmux`. `base` is main `23ddb70` (`/tmp/aph/vtmux-base`), `vtmux` the branch.

Commits: `b64e419` (render: the status format context only on a status redraw), `7e6ab1f` (copy: plain ASCII search in a line's text), `503f2a4` (`scripts/bench-micro.py`).

## PERF gate: agent_panes and key_echo, before and after

`NO_BUILD=1 RUNS=3 BASE=/tmp/aph/vtmux-base AGENT_PANES="5 20" AGENT_CLIENTS="1 2" python3 scripts/bench.py agent_panes key_echo`, under `flock /tmp/vtmux-stress.lock`, 17:29-17:38 UTC. Load 5.2 at the start and 32 at the end, so the `yes` rows are noisy.

2026-11-08 17:29 UTC, commit 503f2a4, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 5.2, median of 3; tmux: tmux 3.8; base: vtmux 3.8-beta1; vtmux: vtmux 3.8-beta1

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.5 | 0.0 | 15.2 | 68 | 260 | 6.1 |
| 5 panes / 1 clients | base | 1.7 | 0.2 | 14.3 | 68 | 232 | 18.8 |
| 5 panes / 1 clients | vtmux | 1.6 | 0.2 | 14.3 | 68 | 233 | 19.1 |
| 5 panes / 2 clients | tmux | 1.6 | 0.0 | 30.4 | 135 | 262 | 6.2 |
| 5 panes / 2 clients | base | 2.2 | 0.3 | 28.2 | 134 | 291 | 21.5 |
| 5 panes / 2 clients | vtmux | 2.1 | 0.3 | 28.2 | 135 | 287 | 21.6 |
| 20 panes / 1 clients | tmux | 4.5 | 0.0 | 15.2 | 68 | 860 | 8.0 |
| 20 panes / 1 clients | base | 3.9 | 0.2 | 14.3 | 68 | 713 | 32.5 |
| 20 panes / 1 clients | vtmux | 4.0 | 0.2 | 14.3 | 68 | 713 | 32.4 |
| 20 panes / 2 clients | tmux | 6.5 | 0.0 | 30.5 | 136 | 845 | 8.1 |
| 20 panes / 2 clients | base | 5.1 | 0.3 | 28.3 | 135 | 758 | 35.0 |
| 20 panes / 2 clients | vtmux | 5.0 | 0.3 | 28.3 | 135 | 757 | 35.0 |

### key_echo (200 keys 30 ms apart into a raw echo pane, one 200x50 client, sync on, the other pane idle or running `yes`; arrival stamped in the reader thread)

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.22 | 0.28 | 0.37 | 0 |
| flood none | base | 0.36 | 0.43 | 0.64 | 0 |
| flood none | vtmux | 0.34 | 0.42 | 0.65 | 0 |
| flood yes | tmux | 0.89 | 2.61 | 18.64 | 0 |
| flood yes | base | 0.72 | 3.69 | 17.27 | 0 |
| flood yes | vtmux | 0.53 | 0.75 | 4.31 | 0 |

Server CPU at 5 panes / 2 clients is 2.2 % before and 2.1 % after. At 20 panes / 1 client it is 3.9 % and 4.0 %. Idle key echo p50 is 0.36 ms before and 0.34 ms after. All within noise: neither change touches these paths at 1-2 clients.

## Instruction counts, many clients

`perf stat -e instructions:u -p <server> -- sleep 8` after 3 s of steady state, on 4 agent_tui.py panes in one window, 120x40 clients, `window-size smallest` (`clients` setup of bench-micro.py). Two runs each:

| clients | base | after |
|---|---|---|
| 32 | 22.32 G, 21.92 G | 17.02 G, 16.95 G |
| 4 | 2.33 G, 2.40 G | 2.33 G, 2.34 G |
| 5 panes / 2 clients (agent_panes layout) | 1.32 G, 1.33 G | 1.32 G, 1.31 G |

## Copy-mode search probe

One 120x40 pane with `history-limit N+1000`. awk prints N lines of about 100 characters (`2026-11-01T12:00:00Z INFO worker[%d] handled request id=%08x path=/api/v1/items/%d status=200 bytes=4096 xx`). Then `copy-mode`, `send-keys -X search-backward zzznotfound`, then `search-forward` likewise. Each command's wall time is from the client. Uncompressed: copy mode is entered right after the output. Load 6-8.

| N | binary | copy-mode | search-backward | search-forward |
|---|---|---|---|---|
| 100k | tmux 3.8 | 32 ms | 175 ms | 175 ms |
| 100k | base | 39 ms | 655 ms | 646 ms |
| 100k | after | 35 ms | 109 ms | 96 ms |
| 100k, `zz+notfound` | tmux 3.8 | 31 ms | 136 ms | 126 ms |
| 100k, `zz+notfound` | after | 37 ms | 93 ms | 82 ms |
| 1M | tmux 3.8 | 249 ms | 1673 ms | 1767 ms |
| 1M | base | 322 ms | 10533 ms | 10391 ms |
| 1M | after | 330 ms | 3078 ms | 2953 ms |

The base's 1M search passed tmux's 10 s `WINDOW_COPY_SEARCH_TIMEOUT`, so it gave up.

## Creating 400 panes

10 sessions x 10 windows x 4 `cat -` panes, 200x50, one command line per session. Server utime+stime, excluding children:

| binary | wall | server CPU |
|---|---|---|
| tmux 3.8 | 286 ms | 70 ms |
| vtmux | 493 ms | 420 ms |

In `perf record` of the server pid, 30% of the samples are in vtmux's code: `split_window` 15%, `new_session`'s environment update (fnmatch) 4%, `list-panes` 4%. The rest is the kernel: `copy_page_range` and COW faults from `forkpty`.

## Micro-benchmarks (scripts/bench-micro.py)

`RUNS=3 python3 scripts/bench-micro.py flood many_panes clients status jobs`, under the lock, 18:16-18:29 UTC, commit `503f2a4`. Load was 118 at the start and 10 at the end.

```
load 118.5, runs 3
== flood
8 panes x 20 MB                               tmux   wall_s=5.47 server_cpu_s=5.31 term_mb=35.8 sync_frames=5152 peak_rss_mb=14.3
8 panes x 20 MB                               vtmux  wall_s=1.38 server_cpu_s=1.25 term_mb=1.14 sync_frames=118 peak_rss_mb=28.2
== many_panes
50 agent panes / 1 client                     tmux   server_cpu_s_per_10s=2.14 frames_per_s=68.2 term_kb_per_s=23.8 rss_mb=11.5 wakeups_per_s=1.49e+03
50 agent panes / 1 client                     vtmux  server_cpu_s_per_10s=1.38 frames_per_s=67.5 term_kb_per_s=24.1 rss_mb=56.7 wakeups_per_s=1.48e+03
100 agent panes / 1 client                    tmux   server_cpu_s_per_10s=3.37 frames_per_s=67.7 term_kb_per_s=23.9 rss_mb=17.8 wakeups_per_s=2.3e+03
100 agent panes / 1 client                    vtmux  server_cpu_s_per_10s=2.71 frames_per_s=67.5 term_kb_per_s=24.3 rss_mb=100 wakeups_per_s=2.4e+03
== clients
4 agent panes / 16 clients                    tmux   server_cpu_s_per_10s=0.366 frames_per_s=2.16e+03 term_kb_per_s=419 rss_mb=7.08 wakeups_per_s=185
4 agent panes / 16 clients                    vtmux  server_cpu_s_per_10s=1.49 frames_per_s=2.14e+03 term_kb_per_s=439 rss_mb=32.6 wakeups_per_s=119
4 agent panes / 32 clients                    tmux   server_cpu_s_per_10s=0.563 frames_per_s=4.3e+03 term_kb_per_s=839 rss_mb=9.17 wakeups_per_s=181
4 agent panes / 32 clients                    vtmux  server_cpu_s_per_10s=2.75 frames_per_s=4.24e+03 term_kb_per_s=881 rss_mb=52.5 wakeups_per_s=107
== status
heavy status, 20 windows, 4 clients, interval 1 tmux   server_cpu_s_per_10s=0.0547 frames_per_s=3.8 term_kb_per_s=1.34 rss_mb=6.3 wakeups_per_s=1.05
heavy status, 20 windows, 4 clients, interval 1 vtmux  server_cpu_s_per_10s=0.0322 frames_per_s=3.8 term_kb_per_s=0.294 rss_mb=21.7 wakeups_per_s=1.9
== jobs
8 #() jobs, interval 1, 2 clients             tmux   server_cpu_s_per_10s=0.104 frames_per_s=8.55 term_kb_per_s=2.19 rss_mb=5.61 wakeups_per_s=4.4
8 #() jobs, interval 1, 2 clients             vtmux  server_cpu_s_per_10s=0.122 frames_per_s=7.3 term_kb_per_s=0.728 rss_mb=16.6 wakeups_per_s=5.75
```

The first RUNS=1 runs (load 8-14, `23ddb70`): flood tmux 5.44 s wall / 5.27 s CPU, vtmux 1.36 / 1.14. 50 panes: tmux 1.50, vtmux 1.15 s per 10 s. 100 panes: 3.35 and 2.46. 16 clients: 0.28 and 1.58. 32 clients: 0.43 and 2.98. Heavy status: 0.056 and 0.044. Jobs: 0.099 and 0.116.

## memgrowth, 30 minutes

`MEM_SECS=1800 MEM_SAMPLE=60 python3 scripts/bench-micro.py memgrowth`, 16:50-17:20 UTC, load 8-14, `23ddb70`. RSS MB / server CPU s since the start:

```
tmux: 0s=6.5MB/0.0s 60s=6.5MB/2.5s 120s=6.5MB/4.5s 180s=6.5MB/6.4s 240s=6.5MB/8.5s 300s=6.5MB/10.4s 360s=6.5MB/12.3s 420s=6.5MB/14.4s 480s=6.5MB/16.6s 540s=6.5MB/18.5s 600s=6.5MB/20.4s 660s=6.5MB/22.2s 720s=6.5MB/24.0s 780s=6.5MB/25.8s 840s=6.5MB/27.6s 900s=6.5MB/29.3s 960s=6.5MB/31.1s 1020s=6.5MB/33.0s 1080s=6.5MB/34.9s 1140s=6.5MB/36.9s 1200s=6.5MB/39.1s 1260s=6.5MB/41.3s 1320s=6.5MB/43.1s 1380s=6.5MB/45.0s 1440s=6.5MB/46.8s 1500s=6.5MB/48.6s 1560s=6.5MB/50.4s 1620s=6.5MB/52.3s 1680s=6.5MB/54.0s 1740s=6.5MB/55.8s 1800s=6.5MB/57.6s
vtmux: 0s=17.0MB/0.0s 60s=21.9MB/2.5s 120s=21.9MB/4.3s 180s=21.9MB/6.0s 240s=21.9MB/7.9s 300s=21.9MB/9.6s 360s=21.9MB/11.3s 420s=21.9MB/13.1s 480s=21.9MB/14.8s 540s=21.9MB/16.4s 600s=21.9MB/18.2s 660s=21.9MB/19.9s 720s=21.9MB/21.5s 780s=21.9MB/23.1s 840s=21.9MB/24.7s 900s=21.9MB/26.3s 960s=21.9MB/27.9s 1020s=21.9MB/29.6s 1080s=21.9MB/31.4s 1140s=21.9MB/33.2s 1200s=21.9MB/35.3s 1260s=21.9MB/37.6s 1320s=21.9MB/39.3s 1380s=21.9MB/40.9s 1440s=21.9MB/42.6s 1500s=21.9MB/44.2s 1560s=22.1MB/45.9s 1620s=22.1MB/47.6s 1680s=22.1MB/49.2s 1740s=22.1MB/50.8s 1800s=22.1MB/52.4s
```

## Full bench.py, RUNS=1

`NO_BUILD=1 RUNS=1 python3 scripts/bench.py`, under the lock, 18:31-18:55 UTC, commit `503f2a4`. Median of 1 at load 82 falling to 12, so it is a check that nothing moved, not new table rows. Raw JSON in `2026-11-08-perf-hotpaths.json`.

Against the tables on PERFORMANCE.md (2026-11-05, `ada47ba`): idle RSS is 10.3 MB here against 9.0 MB, scale 17.5 against 16.0 MB, and settled `spec` history 20.2 against 19.0 MB. These are the memory rows that moved. They were not bisected; neither change of this task allocates per pane or per idle server.

2026-11-08 18:31 UTC, commit 503f2a4, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 81.9, median of 1; tmux: tmux 3.8; vtmux: vtmux 3.8-beta1

### idle (1 session, 60 s, -f /dev/null)

| binary | RSS MB | CPU ms | wakeups/s |
|---|---|---|---|
| tmux | 4.3 | 0.00 | 0.00 |
| vtmux | 10.3 | 0.00 | 0.00 |

### scale (10 sessions x 10 windows x 4 panes of idle cat)

| binary | RSS MB (400 panes) | RSS KB/pane | peak RSS MB | create CPU ms | create wall s |
|---|---|---|---|---|---|
| tmux | 6.4 | 5.4 | 6.4 | 220 | 0.24 |
| vtmux | 17.5 | 18.1 | 17.5 | 410 | 0.44 |

### history (bench-history-memory.sh, fastest of RUNS)

```
scenario binary      size   limit     rss_mb hist_size retained output_s rss_out_mb settled_mb settle_s
spec    tmux      120x40  100000       66.1     99962        -     0.46       66.1       66.1       15
spec    vtmux     120x40  100000       28.7     99962   100039     0.24      108.2       20.2       16
narrow  tmux       80x24   10000       26.0      9978        -     0.74       26.0       26.0       15
narrow  vtmux      80x24   10000       26.5      9978    10244     0.35       30.3       20.1       18
wide    tmux      500x24   10000       28.9      9978        -     0.20       28.9       28.9       15
wide    vtmux     500x24   10000       11.9      9978    10025     0.08       50.0       11.9       15
```

### startup (new -d + kill-server, 100x, -f /dev/null)

| binary | new -d ms | kill-server returns ms | pid gone ms | RSS MB |
|---|---|---|---|---|
| tmux | 12.1 | 2.0 | 2.5 | 4.3 |
| vtmux | 9.4 | 1.7 | 2.8 | 10.2 |

### throughput without a client (100 MB log text, 120x40 pane, default history; sgr bytes = 19%)

| input | binary | wall s | server CPU s | MB/s | peak RSS MB |
|---|---|---|---|---|---|
| plain | tmux | 2.44 | 2.40 | 43.0 | 5.4 |
| plain | vtmux | 1.02 | 0.90 | 102.7 | 13.0 |
| sgr | tmux | 3.26 | 3.06 | 32.2 | 5.4 |
| sgr | vtmux | 1.63 | 1.55 | 64.3 | 13.0 |

### owner_idle (owner config, 1 session, 60 s)

| binary | RSS MB | server CPU ms | server wakeups/s | poller CPU ms (outside the server) |
|---|---|---|---|---|
| tmux | 4.5 | 175.51 | 3.12 | 180 |
| vtmux | 10.8 | 49.15 | 1.03 | 200 |

The poller column is the mu-crew-poller process itself plus the `tmux` clients it ran, not the server.

Processes still carrying the hermetic HOME 5 s after kill-server (the bench SIGKILLed them), summed over all runs: 0

Config snapshot: tmux.conf 870c12a sha256:72a481ffac7e; mu-crew-dotfiles d633e15; plugin tmux-fingers-rs 5759d08; plugin tmux-fzf 2839d6b; plugin tmux-session-picker 1158ce2; plugin tmux-yank acfd36e; plugin tpm 7bdb7ca; plugin vim-tmux-navigator e41c431

### owner_startup (start-server with the owner config until `display -p ok` answers, 20x)

| binary | median ms | min ms | max ms | runs |
|---|---|---|---|---|
| tmux | 120.3 | 107.0 | 173.7 | 20 |
| vtmux | 102.6 | 93.5 | 179.3 | 20 |

Processes still carrying the hermetic HOME 5 s after kill-server (the bench SIGKILLed them), summed over all runs: 0

Config snapshot: tmux.conf 870c12a sha256:72a481ffac7e; mu-crew-dotfiles d633e15; plugin tmux-fingers-rs 5759d08; plugin tmux-fzf 2839d6b; plugin tmux-session-picker 1158ce2; plugin tmux-yank acfd36e; plugin tpm 7bdb7ca; plugin vim-tmux-navigator e41c431

### throughput_attached (100 MB log, one 200x50 client, window-size latest)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | term bytes / MB in | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| plain / sync off | tmux | 2.85 | 2.66 | 0.00 | 2.66 | 1034351 | 0 | 7.4 |
| plain / sync off | vtmux | 1.63 | 1.55 | 0.02 | 1.56 | 64345 | 0 | 18.8 |
| plain / sync on | tmux | 2.85 | 2.70 | 0.00 | 2.70 | 1038287 | 25817 | 7.5 |
| plain / sync on | vtmux | 1.63 | 1.55 | 0.02 | 1.56 | 62431 | 873 | 18.5 |
| sgr / sync off | tmux | 4.08 | 3.89 | 0.00 | 3.89 | 1043253 | 0 | 7.5 |
| sgr / sync off | vtmux | 2.24 | 2.05 | 0.01 | 2.05 | 16105 | 0 | 18.7 |
| sgr / sync on | tmux | 4.07 | 3.98 | 0.00 | 3.98 | 1047183 | 25776 | 7.5 |
| sgr / sync on | vtmux | 1.83 | 1.81 | 0.01 | 1.82 | 14482 | 168 | 18.2 |

### render (4 panes in a 200x50 client, each cat'ing 3000 cursor-addressed 95x22 frames; 22 MB of input)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | terminal MB | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 0.67 | 0.64 | 0.00 | 0.64 | 10.09 | 0 | 5.4 |
| sync off | vtmux | 0.43 | 0.26 | 0.00 | 0.26 | 0.10 | 0 | 14.9 |
| sync on | tmux | 0.67 | 0.63 | 0.00 | 0.63 | 10.11 | 1360 | 5.4 |
| sync on | vtmux | 0.22 | 0.21 | 0.00 | 0.21 | 0.08 | 24 | 14.9 |

### many_clients (1 session, 8 clients 80x24, 100x30, 120x40, 132x43, 160x50, 200x50, 90x35, 240x60, window-size smallest, 20 MB flood in one of 2 panes)

| case | binary | wall s | server CPU s | clients CPU s (sum) | total CPU s | terminal MB (sum) | RSS MB (idle, attached) | RSS MB (after) | peak RSS MB |
|---|---|---|---|---|---|---|---|---|---|
| sync off | tmux | 1.43 | 1.25 | 0.00 | 1.25 | 29.1 | 6.6 | 7.4 | 7.4 |
| sync off | vtmux | 0.41 | 0.27 | 0.02 | 0.29 | 0.9 | 23.8 | 26.0 | 26.0 |
| sync on | tmux | 4.97 | 4.81 | 0.00 | 4.81 | 190.4 | 6.6 | 7.5 | 7.5 |
| sync on | vtmux | 0.41 | 0.29 | 0.01 | 0.30 | 0.8 | 23.8 | 26.0 | 26.0 |

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.8 | 0.0 | 15.2 | 68 | 262 | 6.1 |
| 5 panes / 1 clients | vtmux | 1.8 | 0.2 | 14.3 | 68 | 233 | 19.1 |
| 5 panes / 2 clients | tmux | 1.7 | 0.0 | 30.4 | 135 | 258 | 6.2 |
| 5 panes / 2 clients | vtmux | 2.3 | 0.3 | 28.3 | 135 | 287 | 21.3 |
| 10 panes / 1 clients | tmux | 2.3 | 0.0 | 15.2 | 68 | 470 | 6.6 |
| 10 panes / 1 clients | vtmux | 2.6 | 0.2 | 14.3 | 68 | 392 | 23.1 |
| 10 panes / 2 clients | tmux | 2.5 | 0.0 | 30.4 | 135 | 477 | 6.8 |
| 10 panes / 2 clients | vtmux | 3.6 | 0.4 | 28.3 | 135 | 447 | 25.3 |
| 20 panes / 1 clients | tmux | 4.6 | 0.0 | 15.2 | 68 | 857 | 8.0 |
| 20 panes / 1 clients | vtmux | 4.0 | 0.2 | 14.3 | 68 | 708 | 32.2 |
| 20 panes / 2 clients | tmux | 5.0 | 0.0 | 30.5 | 136 | 842 | 8.1 |
| 20 panes / 2 clients | vtmux | 4.9 | 0.3 | 28.2 | 135 | 766 | 35.0 |

### key_echo (200 keys 30 ms apart into a raw echo pane, one 200x50 client, sync on, the other pane idle or running `yes`; arrival stamped in the reader thread)

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.24 | 0.29 | 0.42 | 0 |
| flood none | vtmux | 0.36 | 0.43 | 0.68 | 0 |
| flood yes | tmux | 0.46 | 0.68 | 1.84 | 0 |
| flood yes | vtmux | 0.50 | 0.87 | 5.89 | 0 |

### upgrade (vtmux-upgrade onto the same binary: N panes x L lines, one window each, 1 attached 200x50 client with sync on, 2 control clients; target < 1000 ms, decision 13)

| case | binary | history lines | idle s | answered ms | redrawn ms | snapshot MB | RSS MB before | RSS MB after |
|---|---|---|---|---|---|---|---|---|
| 1 x 10k fresh | vtmux | 9952 | 0.0 | 43 | 45 | 1.3 | 29.8 | 17.2 |
| 1 x 10k idle | vtmux | 9952 | 9.0 | 35 | 36 | 1.3 | 15.7 | 17.1 |
| 1 x 100k fresh | vtmux | 99952 | 0.0 | 182 | 184 | 11.5 | 169.8 | 28.6 |
| 1 x 100k idle | vtmux | 99952 | 19.0 | 94 | 96 | 11.5 | 27.7 | 28.7 |
| 10 x 10k fresh | vtmux | 99520 | 0.0 | 136 | 138 | 11.3 | 170.4 | 31.7 |
| 10 x 10k idle | vtmux | 99520 | 12.5 | 116 | 118 | 11.3 | 30.9 | 31.6 |
| 10 x 100k fresh | vtmux | 999520 | 0.0 | 881 | 883 | 113.1 | 1567.0 | 147.2 |
| 10 x 100k idle | vtmux | 999520 | 22.3 | 711 | 713 | 113.1 | 146.2 | 147.0 |
