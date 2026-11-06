# Performance

vtmux's goal is to be fast and lean, and that is only claimed where it is measured. This page has the numbers against tmux 3.8, what each scenario measures, and how to reproduce them. Raw runs live in [`docs/test-health/`](test-health/).

**Headline: large scrollback is where vtmux is lean.** Once a pane's output pauses, vtmux compresses its scrollback; a pane that keeps streaming is compressed as it goes. One 120x40 pane holding 100,000 lines settles at 19 MB against tmux's 66 MB, and 1,000,000 lines at 103 MB against 622 MB. A session of 10 panes with 100,000 log lines each settles at 100 MB against 629 MB, and a pane streaming log lines all day at its 100,000-line limit stays at 19 MB against 66 MB (20 such panes: 217 MB against 1.25 GB). The cost: until the compression pass is done (about 15 s after 100k lines of output, about 70 s after 1M) vtmux holds up to 1.6x tmux's memory, and searching the whole history is 3.4x to 5.0x slower (*Big scrollback*, below).

## Numbers

Every row on this page comes from the runs of 2026-11-05 recorded in [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md) (commands, load and raw output; JSON next to it), except the injected-regression check of the guard at the end, and the *Agent panes* and *Hidden panes* tables, re-run after the fix of the agent-panes regression (`8a7d31f`, same host, load 6.2 to 8.4; section *Agent panes fixed* of the same record). Host: a devserver, CentOS Stream 9, Linux 6.16, AMD EPYC Genoa, 96 cpus, shared with other agents; load 2.6 to 4.9 (`uptime`) over the runs, every one under `flock /tmp/vtmux-stress.lock`. vtmux main `ada47ba` (release build), tmux 3.8 from `cargo xtask ref-tmux`; `bench.py` rows are the median of 3 runs.

The first scenarios have no attached client; `throughput_attached`, `render`, `many_clients`, `key_echo`, `agent_panes` and the hidden-pane run attach real clients (see *Attached clients* below).

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| idle, 1 session, 60 s | RSS | 4.2 MB | 9.0 MB | 2.1x |
| | CPU over 60 s | 0 ms | 0 ms | - |
| | wakeups | 0 / s | 0 / s | - |
| scale, 400 panes (10 x 10 x 4 idle `cat`) | RSS | 6.4 MB | 16.0 MB | 2.5x |
| | RSS per pane | 5.4 KB | 18.2 KB | 3.4x |
| | CPU to create | 200 ms | 380 ms | 1.9x |
| history, 150k log lines, limit 100000, 120x40 | RSS right after / +10 s / settled | 66.1 / 66.1 / 66.1 MB | 106.9 / 26.6 / 18.9 MB | 1.6x / 0.40x / 0.29x |
| history, 12k lines, 80x24 truecolour cells | RSS right after / +10 s / settled | 25.9 / 25.9 / 25.9 MB | 29.0 / 25.2 / 19.0 MB | 1.1x / 0.97x / 0.73x |
| history, 12k lines of 450 chars, 500x24 | RSS right after / +10 s / settled | 28.9 / 28.9 / 28.9 MB | 48.6 / 10.5 / 10.5 MB | 1.7x / 0.36x / 0.36x |
| startup (`new -d` + `kill-server`, 100x) | `new -d` | 12.2 ms | 8.9 ms | 0.73x |
| | `kill-server` returns | 2.0 ms | 1.4 ms | 0.70x |
| | server pid gone (from the start of `kill-server`) | 2.5 ms | 2.5 ms | 1.0x |
| throughput, 100 MB plain log, no client | wall / server CPU | 2.44 s / 2.38 s | 1.02 s / 0.89 s | 0.37x CPU |
| | peak RSS | 5.4 MB | 11.8 MB | 2.2x |
| throughput, 100 MB log, 19% SGR bytes | wall / server CPU | 3.26 s / 3.07 s | 1.43 s / 1.40 s | 0.46x CPU |
| | peak RSS | 5.4 MB | 11.5 MB | 2.1x |

The history rows come from `scripts/bench-history-memory.sh` (method in [`test-health/2026-11-01-history-memory.md`](test-health/2026-11-01-history-memory.md)): "right after" is 0.5 s after the output, "+10 s" ten seconds later, "settled" once the RSS stops moving (16-18 s after the output for vtmux).

### With the owner's config

`owner_idle` (median of 3) and `owner_startup` (20 runs). The owner's real `~/.tmux.conf` (dotfiles `870c12a`, sha256 `72a481ffac7e`), the TPM plugins and mu-crew-dotfiles `d633e15` are copied into a hermetic `HOME`; the plugin revisions are in the raw run.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| owner_idle, 1 session, 60 s | RSS | 4.6 MB | 9.7 MB | 2.1x |
| | server CPU over 60 s | 171 ms | 47 ms | 0.28x |
| | server wakeups | 3.1 / s | 1.05 / s | 0.34x |
| | poller CPU over 60 s (not the server) | 180 ms | 190 ms | 1.06x |
| owner_startup, `start-server` until `display -p ok` answers, 20x | median (min - max) | 102.4 ms (93.8 - 119.3) | 101.5 ms (86.4 - 117.4) | 0.99x |

### With a client attached

Server CPU only; the client's CPU is in the raw run (at most 0.02 s for either binary). Terminal bytes are per input byte, or per synchronized-update frame where sync is on (total terminal bytes / `ESC[?2026h` count).

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| throughput_attached, 100 MB plain log, 200x50 client, sync off | wall / server CPU | 2.85 s / 2.63 s | 1.63 s / 1.51 s | 0.57x CPU |
| | terminal bytes per input byte | 1.03 | 0.047 | 0.05x |
| throughput_attached, plain, sync on | wall / server CPU | 2.85 s / 2.66 s | 1.83 s / 1.64 s | 0.62x |
| | sync frames / bytes per frame | 25816 / 4.1 KB | 754 / 7.3 KB | |
| throughput_attached, 19% SGR, sync off | wall / server CPU | 4.08 s / 3.90 s | 1.83 s / 1.65 s | 0.42x |
| | terminal bytes per input byte | 1.04 | 0.013 | 0.01x |
| throughput_attached, 19% SGR, sync on | wall / server CPU | 4.07 s / 3.92 s | 1.83 s / 1.65 s | 0.42x |
| | sync frames / bytes per frame | 25776 / 4.2 KB | 157 / 8.8 KB | |
| | peak RSS (any row) | 7.5 MB | 16.6-16.8 MB | 2.2x |
| render, 4 panes x 3000 cursor-addressed frames (22 MB), sync off | wall / server CPU | 0.67 s / 0.65 s | 0.22 s / 0.19 s | 0.29x |
| | terminal bytes | 10.1 MB | 0.07 MB | 0.007x |
| render, sync on | wall / server CPU | 0.67 s / 0.63 s | 0.22 s / 0.20 s | 0.32x |
| | sync frames / bytes per frame | 1357 / 7.4 KB | 23 / 3.5 KB | |
| | peak RSS | 5.4 MB | 13.5 MB | 2.5x |
| many_clients, 8 clients, 20 MB flood in one pane, sync off | wall / server CPU | 1.23 s / 1.22 s | 0.41 s / 0.23 s | 0.19x |
| | terminal bytes (sum) | 29.2 MB | 0.5 MB | 0.02x |
| many_clients, sync on | wall / server CPU | 4.77 s / 4.68 s | 0.41 s / 0.29 s | 0.06x |
| | RSS attached idle / after flood (sync off) | 6.6 / 7.4 MB | 22.3 / 24.5 MB | 3.4x / 3.3x |

Key-to-echo latency (`bench.py key_echo`): a 200x50 client with sync on, the window split in two, a raw echo program in the active pane and `cat` or `yes` in the other, 200 keys 30 ms apart; each echo's arrival is stamped in the reader thread. Median of 3 runs of p50 / p90 / max:

| other pane | tmux 3.8 | vtmux | vtmux / tmux (p50) |
|---|---|---|---|
| idle | 0.21 / 0.28 / 0.39 ms | 0.33 / 0.43 / 0.70 ms | 1.6x |
| `yes` | 0.52 / 0.76 / 1.45 ms | 0.49 / 0.70 / 1.70 ms | 0.94x |

### Agent panes

`bench.py agent_panes`: each pane runs `scripts/agent_tui.py` (a synthetic pi-like agent TUI, below); 2 panes per window; one 200x50 client on window 0, a second on window 1. Server CPU in seconds per 10 s of steady state.

| panes / clients | tmux 3.8 | vtmux | vtmux / tmux | vtmux RSS (tmux) |
|---|---|---|---|---|
| 5 / 1 | 0.14 s | 0.14 s | 0.98x | 17.7 MB (6.1) |
| 5 / 2 | 0.15 s | 0.19 s | 1.28x | 20.2 MB (6.2) |
| 10 / 1 | 0.22 s | 0.21 s | 0.96x | 21.7 MB (6.6) |
| 10 / 2 | 0.23 s | 0.27 s | 1.14x | 24.0 MB (6.7) |
| 20 / 1 | 0.41 s | 0.35 s | 0.85x | 31.2 MB (8.0) |
| 20 / 2 | 0.45 s | 0.44 s | 0.97x | 33.6 MB (8.1) |

Since `perf_border_style_cache` ([`test-health/2026-11-06-border-style-cache.md`](test-health/2026-11-06-border-style-cache.md); main before it, same run: 1.16x, 1.55x, 1.02x, 1.38x, 0.91x, 1.19x): each pane's border style is expanded once and kept until a border or status redraw, as tmux does (`window_pane_get_border_style`), and a few per-frame costs went (option lookups that missed twice, Ghostty's style lookup for unstyled cells, cell-by-cell pane copies, the client list built for every frame). The styles are kept per client and expanded again when a pane switches screens or dies (medium_border_cache_stale, [`test-health/2026-11-06-border-cache-stale.md`](test-health/2026-11-06-border-cache-stale.md)): against fefa2d4 in one run, 1.01x its server CPU at 5 / 2 and 0.95x at 20 / 1, within the noise, so the rows above stand.

Both send the same number of synchronized frames (68 a second per client). Between `lows_render` and `8a7d31f` these were 3.8-5.7x tmux (the build before the fix, in the same run: 0.68 s at 5 / 1, 2.60 s at 20 / 1): each pane no client shows had its screen view dropped at the end of every turn, and each synchronized update reads that view (`?2026h` holds the screen, its end compares the rows), so every hidden agent pane was read whole about 30 times a second. A hidden pane now keeps its view until it has had no output for a second (*Hidden panes*), at about 0.2-0.5 MB more RSS per busy hidden pane (31.2 MB against 27.1 at 20 / 1).

### Hidden panes

`scripts/bench-hidden-panes.py <binary> <label>`, 4 runs per binary alternating: it attaches a 100x51 client, opens 100 more windows (each pane prints 60 lines of 85 characters, so its 100x50 screen is full) one after another so each is drawn once, goes back to window 0, then times 40 `C-b n` switches from the key to the client's last output byte.

| | tmux 3.8 | vtmux |
|---|---|---|
| server RSS, 101 windows | 9.8 MB | 18.6-19.4 MB |
| RSS per hidden pane | 48 KB | 74-80 KB |
| `C-b n` to the last byte, median | 5.1-5.2 ms | 2.5-2.6 ms |

A pane's screen view (the copied grid and Ghostty's render state, [`ScreenView`](../crates/vtmux-vt/src/snapshot.rs)) is about 0.7 MB for a full 100x50 screen. Since `lows_render` the views of panes no attached client's window shows are dropped after a turn's frames (`release_hidden_screens`; before it, 101 windows took 89.7 MB), and a pane's view is read again whole when it is next drawn. A pane with no output since a client last showed it is released at the end of the first turn it is hidden; one with output while hidden keeps its view until it has had none for a second (`HIDDEN_VIEW_IDLE`), so the synchronized updates of a busy hidden pane read it incrementally (*Agent panes*), and the server wakes to release it once the output stops. A choose-tree or display-panes preview reads a released pane again for each frame it draws.

## Big scrollback

`history-limit` set high and left there is the case vtmux is built to be lean in: idle history is compressed (`hist_compress`, Ghostty's page compression driven by the server), and a pane that never goes quiet is compressed as it streams (`hist_compress_busy`). All rows are 120x40 panes of log lines of about 100 characters (the `spec` generator).

**One pane.** `bench-history-memory.sh spec big`: 150k lines into `history-limit 100000`, 1.5M lines into `history-limit 1000000`.

| history | | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| 100k lines | RSS right after the output | 66.1 MB | 107.0 MB | 1.6x |
| | +10 s | 66.1 MB | 29.9 MB | 0.45x |
| | settled (16 s after) | 66.1 MB | 19.0 MB | 0.29x |
| 1M lines | RSS right after the output | 622.4 MB | 981.4 MB | 1.6x |
| | +10 s | 622.4 MB | 898.6 MB | 1.4x |
| | settled (72 s after) | 622.4 MB | 103.0 MB | **0.17x** |
| | time to write the 1.5M lines | 4.18 s | 2.26 s | 0.54x |

**A session.** `bench-history-memory.sh session`: 10 windows of one session, each pane writing 100k log lines at once (`history-limit 100000`; 999,620 lines of history in all). Two runs, identical within 4 MB:

| | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| server RSS right after the output | 628.5 MB | 946.9 MB | 1.5x |
| +10 s | 628.5 MB | 865.8-869.8 MB | 1.4x |
| settled (70-72 s after) | 628.5-628.6 MB | 100.4 MB | **0.16x** |
| time until all 10 panes are done | 2.84-2.91 s | 1.00-1.01 s | 0.35x |

**Using it.** `bench-history-memory.sh copymode spec big`, median of 3 servers per state. Uncompressed: copy mode entered right after the output; compressed: once settled. In the same copy mode, `search-backward` for a string that is not in the history (it scans every line) and `history-top`; times are the client command's wall time (the server answers once the command is done; a bare round trip is 5-6 ms).

| history | operation | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| 100k lines | `copy-mode` entry, uncompressed / compressed | 30 / 32 ms | 37 / 11 ms | 1.2x / 0.34x |
| | `search-backward` over all of it, uncompressed / compressed | 175 / 179 ms | 592 / 631 ms | 3.4x / 3.5x |
| | `history-top` | 7 / 8 ms | 5 / 6 ms | 0.75x |
| | RSS in copy mode after the search (compressed server) | 128.0 MB | 124.7 MB | 0.97x |
| | RSS settled after leaving copy mode | 124.2 MB | 18.9 MB | 0.15x |
| 1M lines | `copy-mode` entry, uncompressed / compressed | 237 / 220 ms | 315 / 57 ms | 1.3x / 0.26x |
| | `search-backward` over all of it, uncompressed / compressed | 1684 / 1686 ms | 7944 / 8371 ms | 4.7x / 5.0x |
| | `history-top` | 7 / 7 ms | 6 / 6 ms | 0.86x |
| | RSS in copy mode after the search (compressed server) | 1240 MB | 1160 MB | 0.94x |
| | RSS settled after leaving copy mode | 1202 MB | 102.2 MB | 0.09x |

**A pane that never stops.** `bench.py trickle`: the history filled to `history-limit 100000`, then 20 minutes of log lines at 10 or 1000 lines/s, then 2 minutes idle; RSS every 10 s, all servers at once.

| case | tmux 3.8, streaming | vtmux, streaming | vtmux, idle at the end | server CPU over 22 min (tmux / vtmux) |
|---|---|---|---|---|
| 10 lines/s | 65.7-66.1 MB | 18.9-19.6 MB | 19.3 MB | 1.8 / 1.7 s |
| 1000 lines/s | 66.1-66.5 MB | 18.7-20.1 MB | 19.0 MB | 7.5 / 5.8 s |
| 150k lines, then idle | 66.1 MB | 18.8 MB | 18.8 MB | 0.5 / 0.2 s |

The `hist_compress_busy` run ([`test-health/2026-11-05-history-compression-busy.md`](test-health/2026-11-05-history-compression-busy.md)) measured the same within 0.3 MB (18.5-19.8 MB), against 106.6-109.8 MB for the build before the starvation override, which compressed only once the output stopped; its override cost 0.9 s of server CPU over 22 minutes at 1000 lines/s, and the busy pane's p95 output latency rose at most 0.5% inside the compression window.

**Many panes that never stop.** `TRICKLE_PANES="1 5 20" TRICKLE_RATES="1000 fill" TRICKLE_SECS=600 bench.py trickle`: N panes in one server (one window each), each filled to the limit and then streaming 1000 lines/s for 10 minutes; `fill` is the same N panes idle. Streaming is every 10 s sample from 30 to 600 s ([`test-health/2026-11-06-history-compression-many-streaming.md`](test-health/2026-11-06-history-compression-many-streaming.md), with the curve).

| panes | tmux 3.8, streaming | vtmux, streaming (median) | vtmux, idle | server CPU over 12 min (tmux / vtmux) |
|---|---|---|---|---|
| 1 | 66.1-66.4 MB | 18.6-19.5 MB (19.0) | 18.9 MB | 4.5 / 3.9 s |
| 5 | 315.2-315.6 MB | 57.2-61.0 MB (59.2) | 57.4 MB | 19.8 / 18.0 s |
| 20 | 1247-1252 MB | 202.5-230.9 MB (216.7) | 202.3 MB | 76.2 / 66.9 s |

Before `low_compress_many_streaming` the rest between compression steps was shared by every pane, about 40 steps a second in all, short of the 52 pages a second 20 such panes fill: the 20 panes held 1958-1987 MB (1.6x tmux) until their output stopped. Now each starved pane gets the steps a lone one gets (up to 25 panes), at the same server CPU (66.7 s before, 66.9 s after), and the busy pane's p95 output latency with 20 panes compressing stayed within noise (-1.5% spec, -5.4% narrow inside the compression window, against a build without compression).

## Reading the numbers

- **The trade-off is speed over a few MB, as Ghostty makes it.** vtmux runs one libghostty terminal per pane and keeps a render state and row caches per pane and per client so frames are cheap; the owner accepts the memory that costs. It is visible in every RSS row with small or no history: a bare server 9.0 MB against 4.2 (2.1x), 18 KB per idle pane against 5.4 KB (3.4x, 2.5x the RSS at 400 panes), 74-80 KB per hidden full pane against 48 KB, 2.2x to 3.4x with clients attached (24.5 MB against 7.4 MB with 8 clients), and 18-34 MB against 6-8 MB with agent panes. These are losses, a few MB to about 20 MB per server, and they stay in the tables.
- **Scrollback is where the trade-off turns around.** Ghostty stores 8 bytes for every cell of every retained row where tmux stores only the used cells, so fresh history costs 1.1x to 1.7x tmux's (1.6x on 100k and 1M log lines, 1.7x on 500-column rows). vtmux compresses it 5 s after the output stops (a pass of about 10 s per 100k lines, paced between loop turns: the busy pane's p95 output latency rose at most 0.5% in the `hist_compress_busy` gate) and, for a pane that keeps streaming, in busy turns too: settled it holds 0.29x tmux's memory at 100k lines, 0.17x at 1M, and 0.16x for a session of 10 full 100k histories (100 MB against 629 MB). tmux's RSS does not shrink while the pane lives, and after copy mode it stays near the mode's peak (1.2 GB after a search of 1M lines, against vtmux's 102 MB).
- **Using big scrollback costs more on search.** Copy mode copies Ghostty's pages (patch 0005, compressed pages without decoding them) and reads rows on demand, so entry is 11 ms (100k) and 57 ms (1M) on compressed history against tmux's 32 and 220 ms, and `history-top` matches tmux. A search that reads every row is 3.4x (100k lines, 0.6 s) to 4.7-5.0x (1M lines, 8 s) slower than tmux's (1.7 s at 1M lines); the search decodes the copy's pages, so RSS in copy mode reaches about tmux's (1.16 GB against 1.24 GB at 1M) until the mode is left.
- **CPU: a win when consuming output, a loss when creating panes.** With no client, vtmux consumes 100 MB of pane output with 0.37x (plain) and 0.46x (19% SGR) of tmux's server CPU, in 2.3x to 2.4x less wall time. The idle server uses no CPU and takes no wakeups, same as tmux. Creating 400 panes costs 1.9x more CPU (380 ms against 200 ms). Startup is a wash: `new -d` is 8.9 ms against 12.2 ms, and the server pid is gone 2.5 ms after `kill-server` starts for both.
- **Throughput wall time is coarse.** The done marker is polled with `capture-pane` every 0.2 s, so each wall time is accurate to about 0.2 s (up to 20% of vtmux's 1.0 s plain run). The error adds about the same to both binaries, which pulls the ratio toward 1. Server CPU is not affected by the polling beyond the cost of the `capture-pane` calls themselves.
- **Attached, vtmux draws fewer, larger frames than tmux under a flood.** vtmux composes whole frames (D14), so its cost follows how many frames it draws, not how much output arrives. A frame waits while the client still has an unwritten one (tmux defers its redraw the same way, `server_client_check_redraw`), and while a pane still has output waiting, frames of that output go at most every 10 ms; each frame recomposes only the rows that changed. The flood costs 0.42x to 0.62x of tmux's server CPU with one client, 0.19x (sync off) and 0.06x (sync on) with 8. With sync on, where frames can be counted, vtmux sends 754 frames of about 7 KB for the plain log where tmux sends 25816 of about 4 KB; per input byte that is 0.047 against 1.03. Sync-off rows have no frame count.
- **Attached, diffed frames do show in the bytes.** In `render`, where each pane repaints full screens with cursor addressing, vtmux sends 0.07 MB against tmux's 10.1 MB at 0.3x the server CPU. Each frame on the terminal is the latest picture, so the intermediate repaints a pane made within one frame interval never reach the terminal; the final screen is the same.
- **Key echo is 0.1 ms behind tmux with no flood and level next to one.** Only flood output is paced: a frame that also shows output from a pane that was not flooding (a key's echo), a pane to redraw whole or a status redraw goes at once. Each pane keeps one Ghostty render state that re-reads only the rows Ghostty marked dirty, each client caches its composed rows, and a turn that changes no row or cursor builds no frame. What is left with no flood (0.33 ms against 0.21, 1.6x) is mostly O(screen) work per frame: building the window's drawing context and status, copying the cached rows into the frame buffer and comparing every row in the diff.
- **Agent panes: 0.85x to 1.3x tmux's CPU.** Hidden agent panes keep their screen views while they draw (*Agent panes* above; 3.8-5.7x before that fix), and border styles are expanded at border redraws, not every frame (1.6x at 5 panes / 2 clients before). What remains, from `perf record -g` at 5 panes / 2 clients ([`test-health/2026-11-06-border-style-cache.md`](test-health/2026-11-06-border-style-cache.md)): reading Ghostty's dirty rows (about a sixth of the server's samples), parsing pane output, composing the changed rows (an eighth), the frame diff and building the window's drawing inputs (a twelfth, spread over option lookups and the panes' screens). tmux's cost is mostly parsing, so it grows with panes; vtmux parses cheaper and pays per frame instead, so the second client costs it more. The synthetic load is lighter than the owner's real pi panes; no recorded real output was replayed.
- **Peak RSS in throughput** is about 6 MB above tmux's: the per-pane terminal plus the history it keeps for the 100 MB of output, capped by the default `history-limit` of 2000.
- **With the owner's config the idle server is not idle, and the cause is outside it.** The mu-crew poller wakes about once a second and runs `tmux show-options -g` (and `set-option` when a value changes) as clients of the server, so the server's 171 ms (tmux) and 47 ms (vtmux) over 60 s are the cost of answering those clients. vtmux does less server work per request here (0.28x); the RSS gap is the same 5 MB as for a bare server (2.1x). The poller's own CPU (a Python process plus the `tmux` client processes it starts, counted from `/proc/<pid>/stat` with children) is 180 ms and 190 ms: it is outside the server. Only the cheapest probe (`uptime`, a `/proc` read every 60 s) is on; the owner's real list adds `murmur_remote memory cpu uptime muzzle`, which starts `murmur status` and `muzzle status` every 5 to 10 s and is not measured here because it needs a network and the owner's state.
- **Sourcing the owner's config takes about 100 ms for both.** A bare `new -d` takes 9 to 12 ms (table above), so the rest is the config; where it goes (the config's synchronous `run-shell` calls, such as `mu-crew-bind`, each start a shell and several `tmux` clients) was not profiled. The 20 runs overlap for the two binaries (94 to 119 ms and 86 to 117 ms).

## Scenarios

`scripts/bench.py` runs each scenario against both binaries (and `BASE=<binary>`, shown as `base`, to compare against an earlier build).

| scenario | what it does | why |
|---|---|---|
| `idle` | `-f /dev/null`, one session running `cat`, 2 s to settle, then `IDLE_SECS` (60) of nothing | A multiplexer sits idle most of the time: baseline RSS, CPU and wakeups. |
| `scale` | 10 sessions x 10 windows x 4 panes (400 idle `cat` panes, 200x50 windows) | Per-pane cost, where the per-pane libghostty terminal shows up. RSS per pane is (RSS after - RSS before) / 399. |
| `history` | calls `scripts/bench-history-memory.sh` (spec, narrow, wide). Run by hand: its `big` scenario (1.5M lines, limit 1000000; with `SETTLE=900`), its `session` mode (10 panes x 100k lines), `copymode` (entry, a whole-history `search-backward`, `history-top`, RSS in copy mode and after), `steps` and `latency` | Scrollback memory and what using it costs; see the history-memory spec and *Big scrollback*. |
| `startup` | `new -d` then `kill-server`, 100 times on a fresh socket each time | How fast a script's first command and its cleanup are. |
| `owner_idle` | the owner's config in a hermetic `HOME`, one session running `cat`, 10 s to settle, then `IDLE_SECS` (60) of nothing. Reports server RSS, CPU and wakeups, and the poller's CPU separately | What the owner's server costs at rest with the config, TPM plugins and the mu-crew poller running. |
| `owner_startup` | `start-server` followed by `display -p ok` in one invocation, with the config in the hermetic `HOME`; `OWNER_STARTUP_N` (20) times | How long the config takes to source: the `display` is not run until the config has been. |
| `throughput` | a pane `cat`s 100 MB of pre-generated log text (about 100-character lines), plain and with about 20% of the bytes SGR, then prints a done marker; no client attached | Parser and grid cost for heavy output with fixed work. Wall time and server CPU are for the same work for both binaries. |
| `throughput_attached` | the `throughput` file, in a new window while a 200x50 client is attached: wall, server and client CPU, terminal bytes per input byte, sync frames, peak RSS; plain and SGR, sync on and off | What a flood costs when somebody is watching: parse plus render. |
| `render` | 4 tiled panes in a 200x50 client, each `cat`ing a pre-generated file of 3000 cursor-addressed 95x22 frames (about 5.5 MB per pane, fixed work, done marker): server and client CPU, terminal bytes | Where D14 diffed frames should show against tmux's redraw. |
| `trickle` (by name only: about 22 min) | one pane per server writes timestamped log lines at `TRICKLE_RATES` (10 and 1000 lines/s) for `TRICKLE_SECS` (1200) after filling its history to `TRICKLE_LIMIT` (100000), plus a `fill` case (150k lines, then idle); all servers at once, RSS, `#{history_size}` and server CPU every 10 s, then `TRICKLE_IDLE_SECS` (120) idle | Scrollback memory of a pane that never goes idle (agent logs, `tail -f`, builds): does it shrink like an idle one? |
| `key_echo` | one 200x50 client (sync on), the window split in two, a raw echo program in the active pane, `cat` or `yes` in the other; `KEY_ECHO_N` (200) keys 30 ms apart, each echoed as a Greek letter (bytes found nowhere else on the terminal), arrival stamped in the reader thread: p50, p90, max | What typing costs, alone and next to a flood. |
| `many_clients` | 1 session, 2 panes, 8 clients of different sizes (80x24 to 240x60), `window-size smallest` for both, a 20 MB log in one pane: server CPU and RSS, terminal bytes (sum) | Per-client fan-out cost. |
| `agent_panes` | `AGENT_PANES` (5 10 20) panes of `scripts/agent_tui.py`, 2 per window, with `AGENT_CLIENTS` (1 2) 200x50 clients answering the 2026 query, the first on window 0 and the second on a session that links window 1; 2 s to settle, then `AGENT_SECS` (10) measured: server and client CPU, terminal bytes, sync frames, wakeups, RSS | What many agent TUIs (pi, ink renderers) cost the server. `agent_tui.py` mimics pi's alt-screen renderer at real speed: every update in synchronized output rewrites only the changed rows (`CSI r;1H`, `EL`, the line, SGR reset, an empty OSC 8), a truecolour braille spinner every 80 ms, a streamed word every 20-80 ms (the whole chat area once it scrolls), a block of tool output every 2-5 s, a footer once a second. Deterministic content per pane seed. |

Owner scenarios (`owner_idle`, `owner_startup`; Linux only, skipped elsewhere with the reason): `HOME` is `<tmp>/<run>/home`, holding a copy of `~/.tmux.conf` (symlinks followed), `~/.tmux/plugins` and `~/.local/share/mu-crew-dotfiles` (without `.git`, `target` or `node_modules`); their git revisions and the config's sha256 go into the output. Two changes to the copy: `@fingers-skip-wizard 1` first (no install wizard) and `@mu_crew_probes` set to `uptime`, the cheapest probe. `<tmp>/<run>/bin/tmux` is a symlink to the binary under test and first on `PATH` for both binaries (the config, TPM and the poller all run `tmux` from `PATH`; vtmux is run as `tmux`, D20). After each run the bench waits up to 5 s for every process whose environment has the hermetic `HOME` (the poller leaves when its server is gone), kills what is left and prints how many it had to kill; processes are matched by environment, never by name, so neither the bench's shell nor the owner's real poller can match. The poller's CPU is `utime+stime+cutime+cstime` of its pid from the pidfile, which includes the `tmux` clients it ran; it is outside the server and reported next to it, not added to it.

How it measures: the server pid comes from `display -p '#{pid}'` on a private socket (`-L` and `TMUX_TMPDIR`), and is checked gone after `kill-server`; pids are never found by name. Only the server process is measured, never its pane children.

- **Linux CPU:** `/proc/<pid>/stat` utime+stime (fields 14 and 15) deltas per phase, which count all threads and exclude children. Idle and the attached scenarios use the nanosecond `schedstat` sum, because 10 ms ticks are too coarse. Never `cutime`/`cstime`, `getrusage` of children or `time(1)`: those include the reaped pane processes.
- **Linux RSS:** settled `VmRSS`, and the per-phase peak is `VmHWM` after `echo 5 > /proc/<pid>/clear_refs`.
- **Wakeups:** deltas of `voluntary_ctxt_switches` over all threads.
- **macOS:** `ps -o rss=,time=` sampled every 100 ms (peak RSS is the largest sample; wakeups are not reported). `scale` raises `ulimit -n` to 4096, or is skipped with the reason if it can't. The macOS numbers have not been collected yet.

### Attached clients

The client is the binary's own `attach` on a PTY the bench drains with a Python thread (`Client` in `scripts/bench.py`; not `script(1)`, whose flags differ between Linux and macOS). The thread counts every byte the terminal would receive and every synchronized-update begin (`ESC[?2026h`); client CPU is read from `/proc/<client pid>` like the server's. For a fair comparison both binaries get the same `TERM` (xterm-256color), the same explicit `default-terminal`, `terminal-features`, `window-size`, `status-right` and `status-interval`. Both servers enable synchronized output only from the terminal's `DECRQM 2026` reply (vtmux: `input.rs:215`, `render.rs:81`; tmux 3.8: `tty_keys_sync`), so each scenario runs twice: the reader answers the query (`sync on`) or never does (`sync off`), and the bench checks that the query arrived before it trusts a sync-on row. Measurement starts after all clients have attached and the output went quiet, and ends when the done marker shows and the terminals have been quiet for 0.3 s.

## Running

```
mise exec -- python3 scripts/bench.py                 # all scenarios; Markdown on stdout, JSON and Markdown under target/bench/
mise exec -- python3 scripts/bench.py idle scale      # chosen scenarios
IDLE_SECS=5 RUNS=1 STARTUP_N=5 THROUGHPUT_MB=10 RENDER_FRAMES=200 MANY_MB=5 AGENT_PANES=10 AGENT_SECS=5 mise exec -- python3 scripts/bench.py   # quick development run
RUNS=3 mise exec -- python3 scripts/bench.py agent_panes   # the agent-panes table (about 11 min with BASE)
BASE=/path/to/older/vtmux mise exec -- python3 scripts/bench.py
BASE=/path/to/older/vtmux mise exec -- python3 scripts/bench.py trickle   # not in the default set
```

A full run takes about 32 minutes (`agent_panes` is 9 of them). The host is shared: run it when `uptime` load is under 4, under `flock /tmp/vtmux-stress.lock`, and one bench at a time. Other env: `RUNS`, `OUT`, `NO_BUILD=1`, `VTMUX`, `RENDER_FRAMES` (3000), `MANY_MB` (20), `AGENT_PANES`, `AGENT_CLIENTS`, `AGENT_SECS`, `VTMUX_REFERENCE_TMUX`, `HISTORY_SCENARIOS` (documented at the top of the script). Put the result in `docs/test-health/<date>-perf.md` and update the table above.

## Regression guard

`python3 scripts/bench.py --smoke` (`scripts/bench_smoke.py`) is a short run (about 1 minute on an idle host, under 3 minutes anywhere) that fails when vtmux got worse. It is not part of `check.sh`. In CI it is the `perf-smoke` job in `ci.yml`: Linux only, `continue-on-error` (it never blocks a merge), also run weekly (`schedule:`, Mondays 04:17 UTC), sharing the zig/rust/ref-tmux setup with the `check` job through `.github/actions/setup`. It writes its table to `$GITHUB_STEP_SUMMARY` and uploads `smoke.json` as the `perf-smoke` artifact.

| metric | scenario | limit |
|---|---|---|
| `idle_rss_mb` | idle server, one session | baseline + max(25%, 8 MB) |
| `scale_rss_mb` | 400 idle panes | baseline + max(25%, 8 MB) |
| `history_spec_rss_mb` | `bench-history-memory.sh spec` | baseline + max(25%, 8 MB) |
| `cpu_ratio_plain`, `cpu_ratio_sgr` | throughput without a client (30 MB, median of 3): server CPU of vtmux / tmux 3.8 | baseline ratio x 1.5 |

- **RSS** is compared with `perf/baseline.json`, recorded on the CI runner (RSS depends on the allocator and libc, so numbers from a developer machine don't carry over). For one binary and one input it is nearly deterministic; the 8 MB floor keeps the small idle numbers from tripping on noise.
- **CPU** is never compared with a checked-in absolute time: runner speed varies between runs and SKUs. It is gated as the vtmux/tmux 3.8 CPU ratio measured in the same run, which cancels the machine's speed, against the ratio in the baseline. The alternative, `valgrind --tool=cachegrind` instruction counts of a foreground server (tighter, +10%), was not chosen: it needs a valgrind install, runs the server on one traced thread rather than as deployed, and doesn't exist on macOS. The ratio's coarse +50% threshold only catches a real slowdown, not drift.
- **Local runs** work on any Linux host (`BASELINE=<file>` points at another baseline; without `perf/baseline.json` the run only records). A loaded host (load >= 4) makes the ratios noisy; the script warns.

### Refreshing the baseline

After an intentional change that moves a metric (a feature that costs memory, a compression that saves it): take `smoke.json` from the `perf-smoke` artifact of a CI run on the commit, and commit its `metrics` as `perf/baseline.json` (`{"metrics": {...}}`; the whole file works too). Put the reason in the commit message ("perf baseline: idle RSS +1.2 MB, the per-pane foo table"). Never refresh to make a red run green without a reason; a regression is fixed or explained. The first baseline is whatever the first run on `main` produced.

### Staleness

The numbers above and in the README come from a full run at the commit recorded in "Numbers". When that commit is older than the last change to `perf/baseline.json` (a refresh means the metrics moved), the numbers are stale: the smoke run prints a `STALE:` line, and the next full run replaces them.

### Checked with injected regressions

Scratch builds (not committed): a 50 MB `vec![0u8; N]` per pane (for the first three panes, one byte written in every 4 KiB page so the pages are resident) and a 20-million-iteration busy loop per output chunk, run against a baseline recorded from the same host and the good build:

| metric | baseline | regressed | result |
|---|---|---|---|
| `idle_rss_mb` | 7.4 MB | 57.5 MB | fail |
| `scale_rss_mb` | 13.4 MB | 163.4 MB | fail |
| `history_spec_rss_mb` | 105.5 MB | 205.4 MB | fail |
| `cpu_ratio_plain` | 0.32 | 109 | fail |
| `cpu_ratio_sgr` | 0.89 | 84 | fail |
