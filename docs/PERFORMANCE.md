# Performance

This page compares the vtmux server with tmux 3.8: memory, CPU, bytes sent to the terminal, and latency. Both are release builds, run on the same host by the same scripts.

Raw runs live in [`docs/test-health/`](test-health/). Each section below links the run its numbers come from.

The trade-off is speed over a few MB, as Ghostty makes it. So vtmux uses more memory than tmux when idle and per pane, and this page says so wherever it shows.

## At a glance

A ratio is vtmux / tmux 3.8 for the same work in the same run; under 1 means vtmux uses less. Rows where vtmux is worse say **Worse**.

| area | tmux 3.8 | vtmux | vtmux / tmux | note |
|---|---|---|---|---|
| Big scrollback, 1M lines, settled | 622.4 MB | 103.0 MB | 0.17x | settled 72 s after the output; 1.6x until compressed |
| Big scrollback, 10 panes x 100k lines, settled | 628.5 MB | 100.4 MB | 0.16x | |
| 20 panes streaming at a 100k limit | 1247-1252 MB (1.25 GB) | 202.5-230.9 MB (median 216.7, 217 MB) | - | compressed while they stream |
| Idle server RSS | 4.2 MB | 9.0 MB | 2.1x | **Worse.** A terminal and render state per pane |
| RSS per idle pane (400 panes) | 5.4 KB | 18.2 KB | 3.4x | **Worse.** |
| Output CPU, no client, 100 MB log | 2.38 s / 3.07 s | 0.89 s / 1.40 s | 0.37x / 0.46x | plain / 19% SGR |
| Output CPU, one client attached | 2.63-3.92 s | 1.51-1.65 s | 0.42x-0.62x | |
| Rendering, redraw-heavy (`render`) | 10.1 MB to the terminal | 0.07 MB | about 140x fewer bytes | at 0.29x-0.32x the server CPU |
| 8 clients, 20 MB flood | 1.22 s / 4.68 s CPU | 0.23 s / 0.29 s | 0.19x / 0.06x | sync off / on. **Worse** RSS: 3.4x / 3.3x |
| Agent TUIs, 5-20 panes | 0.14-0.45 s per 10 s | 0.14-0.44 s | 0.85x-1.28x | **Worse** with a second client (up to 1.28x); RSS 17.7-33.6 MB against 6.1-8.1 |
| Key echo p50, other pane idle | 0.21 ms | 0.33 ms | 1.6x | **Worse.** Next to a `yes` flood: 0.94x |
| Startup, `new -d` | 12.2 ms | 8.9 ms | 0.73x | |
| Creating 400 panes, CPU | 200 ms | 380 ms | 1.9x | **Worse.** |
| Copy mode entry, 1M lines, compressed | 220 ms | 57 ms | 0.26x | |
| Search all of a 1M-line history | 1651 ms | 554 ms | 0.34x | was 3.1 s, and 10.5 s before that; coloured lines 0.36x, was 6.1 s; *Copy mode over a coloured history* |
| Copy mode entry, 1M coloured lines, uncompressed | 285 ms | 335 ms | 1.18x | **Worse**; was 726 ms (2.5x); plain lines 1.19x |
| `capture-pane -S -` of a 1M-line history | 840 ms | 385 ms | 0.46x | was 3.5 s; `-M` (copy mode) 0.42x, `-e` 0.20x; coloured lines 0.49x, `-e` 0.54x; *Capturing all of a history* |
| Alternate screen up over a 1M-line history | 3 ms | 4.0 s | - | **Worse**, was 6.9 s; 0.4 s at 100k lines. vtmux copies the history, tmux does not; *Reading all of a history* |
| 32 clients on one window of agent panes | 0.48 s per 10 s | 0.79 s | 1.6x | **Worse**, was 5.2x. Clients that show the window alike share its drawing and frame; *Micro-benchmarks* |
| 100 agent panes, one client | 3.37 s per 10 s | 2.71 s | 0.80x | RSS 100 MB against 17.8 MB, **Worse** |
| 10 agent panes for 30 minutes | 6.5 MB | 21.9-22.1 MB | - | no growth in either; **Worse** RSS |
| Hidden panes, RSS per hidden pane | 48 KB | 74-80 KB | - | **Worse.** |
| Live upgrade, 10 panes x 100k lines, request to redraw | - (kill-server) | 598 ms compressed; 751 ms not yet compressed | - | Meets decision 13's 1 s (was 1705 ms and refused). **Misses** it onto a binary of another Ghostty patch stack: 9.9-10.4 s |
| Hidden panes, `C-b n` to the last byte | 5.1-5.2 ms | 2.5-2.6 ms | - | |
| Owner's config, idle server CPU over 60 s | 171 ms | 47 ms | 0.28x | the cost of answering a status poller |
| Release binary, stripped | 1.3 MB | 9.8 MB | 7.7x | **Worse.** Ghostty is linked in, tmux links libevent and ncurses; was 10.7 MB before LTO, *Binary size and build profile* |

Where the numbers come from, and how they are measured: *Method*, at the end.

## Memory and scrollback

### Idle and per pane

`idle` is one session running `cat` for 60 s with `-f /dev/null`. `scale` is 400 idle `cat` panes (10 sessions x 10 windows x 4 panes). vtmux uses more memory in both.

The reason: vtmux runs one libghostty terminal per pane, and keeps a render state and row caches per pane and per client so frames are cheap. The owner accepts the memory that costs.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| idle, 1 session, 60 s | RSS | 4.2 MB | 9.0 MB | 2.1x |
| scale, 400 panes (10 x 10 x 4 idle `cat`) | RSS | 6.4 MB | 16.0 MB | 2.5x |
| | RSS per pane | 5.4 KB | 18.2 KB | 3.4x |

- These are losses of a few MB to about 20 MB per server. They stay in the tables.
- RSS per pane is (RSS after - RSS before) / 399.
- The same cost shows elsewhere: 2.2x to 3.4x with clients attached (24.5 MB against 7.4 MB with 8 clients), 18-34 MB against 6-8 MB with agent panes, and 74-80 KB per hidden full pane against 48 KB.

Raw runs: [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *Full `bench.py` run*.

### Big scrollback

A high `history-limit` left in place is the case vtmux is built to be lean in. Large scrollback is where the trade-off turns around: settled, vtmux holds 0.29x tmux's memory at 100k lines, 0.17x at 1M, and 0.16x for a session of 10 full 100k histories (100 MB against 629 MB).

In round numbers: one 120x40 pane holding 100,000 lines settles at 19 MB against tmux's 66 MB, and 1,000,000 lines at 103 MB against 622 MB. A pane streaming log lines all day at its 100,000-line limit stays at 19 MB against 66 MB (20 such panes: 217 MB against 1.25 GB).

Fresh history costs more than tmux's. Ghostty stores 8 bytes for every cell of every retained row, where tmux stores only the used cells. tmux's RSS does not shrink while the pane lives.

So vtmux compresses idle history (`hist_compress`, Ghostty's page compression driven by the server). A pane that never goes quiet is compressed as it streams (`hist_compress_busy`).

Unless noted, rows are 120x40 panes of log lines of about 100 characters (the `spec` generator).

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

- "Right after" is 0.5 s after the output, "+10 s" ten seconds later, "settled" once the RSS stops moving.
- Until the compression pass is done, vtmux holds up to 1.6x tmux's memory: about 15 s after 100k lines of output, about 70 s after 1M.
- Compression starts 5 s after the output stops. A pass takes about 10 s per 100k lines, paced between loop turns.

**Other row shapes.** The `history` scenario of the full `bench.py` run: `spec` as above, plus narrow truecolour rows and wide rows.

| history | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| 150k log lines, limit 100000, 120x40 | RSS right after / +10 s / settled | 66.1 / 66.1 / 66.1 MB | 106.9 / 26.6 / 18.9 MB | 1.6x / 0.40x / 0.29x |
| 12k lines, 80x24 truecolour cells | RSS right after / +10 s / settled | 25.9 / 25.9 / 25.9 MB | 29.0 / 25.2 / 19.0 MB | 1.1x / 0.97x / 0.73x |
| 12k lines of 450 chars, 500x24 | RSS right after / +10 s / settled | 28.9 / 28.9 / 28.9 MB | 48.6 / 10.5 / 10.5 MB | 1.7x / 0.36x / 0.36x |

- vtmux settled 16-18 s after the output.
- Fresh history costs 1.1x to 1.7x tmux's (1.7x on 500-column rows).
- The first row is the same scenario as the 100k row of the one-pane table, from the earlier full `bench.py` run of the same evening. The two differ at +10 s (26.6 against 29.9 MB), where compression is mid-pass.
- Method of `bench-history-memory.sh`: [`test-health/2026-11-01-history-memory.md`](test-health/2026-11-01-history-memory.md).

**A session.** `bench-history-memory.sh session`: 10 windows of one session, each pane writing 100k log lines at once (`history-limit 100000`; 999,620 lines of history in all). Two runs, identical within 4 MB:

| | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| server RSS right after the output | 628.5 MB | 946.9 MB | 1.5x |
| +10 s | 628.5 MB | 865.8-869.8 MB | 1.4x |
| settled (70-72 s after) | 628.5-628.6 MB | 100.4 MB | **0.16x** |
| time until all 10 panes are done | 2.84-2.91 s | 1.00-1.01 s | 0.35x |

**A pane that never stops.** `bench.py trickle`: the history filled to `history-limit 100000`, then 20 minutes of log lines at 10 or 1000 lines/s, then 2 minutes idle. RSS every 10 s, all servers at once.

| case | tmux 3.8, streaming | vtmux, streaming | vtmux, idle at the end | server CPU over 22 min (tmux / vtmux) |
|---|---|---|---|---|
| 10 lines/s | 65.7-66.1 MB | 18.9-19.6 MB | 19.3 MB | 1.8 / 1.7 s |
| 1000 lines/s | 66.1-66.5 MB | 18.7-20.1 MB | 19.0 MB | 7.5 / 5.8 s |
| 150k lines, then idle | 66.1 MB | 18.8 MB | 18.8 MB | 0.5 / 0.2 s |

- The `hist_compress_busy` run measured the same within 0.3 MB (18.5-19.8 MB).
- The build before that change compressed only once the output stopped: 106.6-109.8 MB.
- Its starvation override cost 0.9 s of server CPU over 22 minutes at 1000 lines/s.
- The busy pane's p95 output latency rose at most 0.5% inside the compression window.

**Many panes that never stop.** `TRICKLE_PANES="1 5 20" TRICKLE_RATES="1000 fill" TRICKLE_SECS=600 bench.py trickle`: N panes in one server (one window each), each filled to the limit, then streaming 1000 lines/s for 10 minutes.

`fill` is the same N panes idle. Streaming is every 10 s sample from 30 to 600 s.

| panes | tmux 3.8, streaming | vtmux, streaming (median) | vtmux, idle | server CPU over 12 min (tmux / vtmux) |
|---|---|---|---|---|
| 1 | 66.1-66.4 MB | 18.6-19.5 MB (19.0) | 18.9 MB | 4.5 / 3.9 s |
| 5 | 315.2-315.6 MB | 57.2-61.0 MB (59.2) | 57.4 MB | 19.8 / 18.0 s |
| 20 | 1247-1252 MB | 202.5-230.9 MB (216.7) | 202.3 MB | 76.2 / 66.9 s |

- Before `low_compress_many_streaming`, the rest between compression steps was shared by every pane: about 40 steps a second in all, short of the 52 pages a second 20 such panes fill.
- So the 20 panes held 1958-1987 MB (1.6x tmux) until their output stopped.
- Now each starved pane gets the steps a lone one gets (up to 25 panes), at the same server CPU (66.7 s before, 66.9 s after).
- The busy pane's p95 output latency with 20 panes compressing stayed within noise: -1.5% spec, -5.4% narrow inside the compression window, against a build without compression.

Raw runs:

- [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md): one pane, session, trickle.
- [`test-health/2026-11-05-history-compression-busy.md`](test-health/2026-11-05-history-compression-busy.md): `hist_compress_busy`.
- [`test-health/2026-11-06-history-compression-many-streaming.md`](test-health/2026-11-06-history-compression-many-streaming.md): many streaming panes, with the curve.

## Copy mode and search

`bench-history-memory.sh copymode spec big`, median of 3 servers per state. Entering copy mode on compressed history is faster than tmux. The table is from before `audit_perf_hotpaths` and `perf_search_1m`, which made the search about half tmux's time at 100k and 1M lines (*Plain search since `audit_perf_hotpaths`* and *Reading all of a history*, below).

Copy mode copies Ghostty's pages (patch 0005 copies compressed pages without decoding them) and reads rows on demand. That makes entry cheap; a search has to decode the pages.

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

- Uncompressed: copy mode entered right after the output. Compressed: entered once settled.
- `search-backward` looks for a string that is not in the history, so it scans every line. `history-top` runs in the same copy mode.
- Times are the client command's wall time: the server answers once the command is done. A bare round trip is 5-6 ms.
- In seconds: a full search is 0.6 s at 100k lines and 8 s at 1M lines, against tmux's 1.7 s at 1M.
- The search decodes the copy's pages, so RSS in copy mode reaches about tmux's (1.16 GB against 1.24 GB at 1M) until the mode is left.
- tmux stays near the mode's peak after leaving it: 1.2 GB after a search of 1M lines, against vtmux's 102 MB.

Raw runs: [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *Using the history*.

### Plain search since `audit_perf_hotpaths`

A probe script, not the `copymode` mode above: one 120x40 pane, N log lines of about 100 characters, `copy-mode` right after the output (uncompressed), then `send-keys -X search-backward` for a string not in the history. One server each, load 6-8.

| history | search | tmux 3.8 | vtmux before | vtmux after | after / tmux |
|---|---|---|---|---|---|
| 100k lines | plain `zzznotfound` | 175 ms | 655 ms | 109 ms | 0.62x (the author's run) |
| 100k lines | plain `zzznotfound`, the reviewer's re-run | 152-155 ms | - | 155-164 ms | about 1.0x: parity |
| 100k lines | regular expression `zz+notfound` | 136 ms | - | 93 ms | 0.68x |
| 1M lines | plain `zzznotfound` | 1673 ms | 10533 ms | 3078 ms | 1.8x |

- Read the 100k rows as parity: the author's 0.62x came from one run under a different load, the reviewer's repeat gives 155-164 ms against tmux's 152-155 ms. No claim of faster than tmux.
- Before, a plain pattern was compared cell by cell at every column of every line (`window_copy_search_lr`), reading each line's cells several times. Now a plain ASCII pattern is looked for in the text of a narrow ASCII line that does not wrap, as a regular expression already was. Other lines still go through the cells.
- Before, the 1M-line search took longer than tmux's 10 s search timeout (`WINDOW_COPY_SEARCH_TIMEOUT`), so it gave up where tmux finished in 1.7 s. A match near the top of such a history was not found.
- The 1M row still lost (1.8x) because each line was found with a walk of Ghostty's page list from the top (`ghostty_terminal_grid_ref`, `PageList.pin`). That is O(pages) per line, so a full search was quadratic: 10x the lines took about 30x the time. Fixed since, with a Ghostty patch (*Reading all of a history*).

### Reading all of a history

`scripts/bench-history-read.py`: one 120x40 pane, N log lines of about 100 characters, one 120x40 client attached, history uncompressed. Median of 3 runs, base, branch and tmux in turn, load 16-22. `before` is main before `perf_search_1m`.

| history | command | tmux 3.8 | vtmux before | vtmux after | after / tmux |
|---|---|---|---|---|---|
| 100k lines | `search-backward`, string not in the history | 176 ms | 105 ms | 90 ms | 0.51x |
| | `capture-pane -p -S -` | 88 ms | 398 ms | 362 ms | 4.1x |
| | `capture-pane -p -e -S -` | 458 ms | 1357 ms | 1385 ms | 3.0x |
| | `capture-pane -p -M -S -` (copy mode) | 87 ms | 775 ms | 755 ms | 8.7x |
| | alternate screen up (`?1049h`) | 4 ms | 438 ms | 407 ms | about 100x |
| 1M lines | `search-backward`, string not in the history | 1652 ms | 3218 ms | 825 ms | 0.50x |
| | `search-forward` | 1674 ms | 3047 ms | 714 ms | 0.43x |
| | `capture-pane -p -S -` | 888 ms | 6438 ms | 3622 ms | 4.1x |
| | `capture-pane -p -e -S -` | 4729 ms | 16166 ms | 13319 ms | 2.8x |
| | `capture-pane -p -M -S -` (copy mode) | 856 ms | 9837 ms | 7566 ms | 8.8x |
| | alternate screen up (`?1049h`) | 3 ms | 6926 ms | 4014 ms | about 1300x |
| | `history-top`, `goto-line` to the middle, with the redraw | 2-3 ms | 2-3 ms | 2-3 ms | - |

- The search is linear now: 90, 262 and 825 ms at 100k, 300k and 1M lines (tmux 176, 539, 1652). Before it was 105, 502 and 3218 ms.
- Before, every reader of all the rows looked each row up with a walk of Ghostty's page list from the oldest page, so it was quadratic: the search, `capture-pane -S -` with any flags, `-M` and `-R`, `#{history_bytes}`, the alternate screen coming up and view mode. Ghostty patch 0033 (`ghostty_grid_ref_down` / `_up`) steps from one row to the next instead.
- Jumps were never quadratic: a redraw reads one screen of rows. They take 2-3 ms at 1M lines, as tmux's.
- The captures and the alternate screen lost the walk but were still **Worse**. What remained was per row: each row was turned into cells with their text, style and link (`read_cells`) and then written out, and `-M` kept every row it read in the copy. The captures are faster than tmux's since (*Capturing all of a history*, below).
- The alternate screen is the worst. vtmux copies the whole primary history when it comes up, because Ghostty can read only the active screen and tmux goes on showing that history; tmux saves only the visible rows (`screen_alternate_on`, screen.c:700-721). A program that switches to the alternate screen (vim, less) in a pane with 1M lines of history holds the server for 4 s, with 100k lines 0.4 s; with the default `history-limit` of 2000, 10-11 ms against tmux's 3 ms (the time includes the polling client). Avoiding the copy needs a way to read the primary screen's pages while the alternate screen is up.

Raw runs: [`test-health/2026-11-08-search-1m.md`](test-health/2026-11-08-search-1m.md).

### Capturing all of a history

The same script, and `--styled`, whose lines have a coloured timestamp, a bold coloured level, a 256-colour status and an `é`. Median of 3 runs, base, branch and tmux in turn, load 12-13. `before` is main before `perf_capture_full_history`. The time includes sending the capture to the client, about 100 MB at 1M lines.

| history | command | tmux 3.8 | vtmux before | vtmux after | after / tmux |
|---|---|---|---|---|---|
| 1M plain lines | `capture-pane -p -S -` | 840 ms | 3526 ms | 385 ms | 0.46x |
| | `capture-pane -p -e -S -` | 4601 ms | 13255 ms | 931 ms | 0.20x |
| | `capture-pane -p -M -S -` (copy mode) | 883 ms | 7411 ms | 371 ms | 0.42x |
| 1M styled lines | `capture-pane -p -S -` | 896 ms | 4161 ms | 440 ms | 0.49x |
| | `capture-pane -p -e -S -` | 5594 ms | 15020 ms | 3026 ms | 0.54x |
| | `capture-pane -p -M -S -` (copy mode) | 838 ms | 8054 ms | 444 ms | 0.53x |
| 100k plain lines | `-S -` / `-e -S -` / `-M -S -` | 87 / 448 / 83 ms | 356 / 1341 / 755 ms | 44 / 99 / 42 ms | 0.51x / 0.22x / 0.51x |
| 100k styled lines | `-S -` / `-e -S -` / `-M -S -` | 90 / 544 / 83 ms | 415 / 1517 / 809 ms | 48 / 303 / 42 ms | 0.53x / 0.56x / 0.51x |

- Before, every line was built into cells with their text, style and link (`read_cells`, about 64 bytes a cell) and written out through two more buffers. `-M` built a row for every line of copy mode's copy, kept it, and converted it back.
- Now a line of plain cells (one character or none, one column, no tab, line drawing or link) is written straight from Ghostty's cell bits. Without `-e`, `-N`, `-F` and `-H`, styles and links write nothing, so a coloured line is plain too. Under `-e` or `-N` a line is read with its styles, one lookup per run of one style.
- `-M` reads a line copy mode has not read from its entry copy and keeps nothing for it.
- Other lines (wide characters, clusters, tabs, line drawing, links, or `-F`/`-H` over styles) still go through the cells, now without allocating per line. Not measured here: a history made mostly of such lines.
- The output is byte for byte what it was: a test compares the new paths with the cells path over random content under every flag, and a probe compared the branch with main over the CLI (10 seeds x 1760 captures).
- Seen on the way: over the styled history, copy mode's search was 3.4x tmux's time and entering copy mode 2.7x. Both are faster since (*Copy mode over a coloured history*, below).

Raw runs: [`test-health/2026-11-09-capture-full-history.md`](test-health/2026-11-09-capture-full-history.md).

### Copy mode over a coloured history

The same script, plain and `--styled`. Median of 3 runs, base, branch and tmux in turn, load 3-6. `before` is main before `perf_search_styled_history`. `search-*` look for a string not in the history, so they read every line and count nothing.

| history | command | tmux 3.8 | vtmux before | vtmux after | after / tmux |
|---|---|---|---|---|---|
| 1M styled lines | `copy-mode` | 285 ms | 726 ms | 335 ms | 1.18x |
| | `search-backward` | 1794 ms | 6067 ms | 646 ms | 0.36x |
| | `search-forward` | 2065 ms | 6060 ms | 498 ms | 0.24x |
| 1M plain lines | `copy-mode` | 236 ms | 274 ms | 281 ms | 1.19x |
| | `search-backward` | 1651 ms | 640 ms | 554 ms | 0.34x |
| | `search-forward` | 1650 ms | 505 ms | 403 ms | 0.24x |
| 100k styled lines | `copy-mode` / `search-backward` / `search-forward` | 30 / 177 / 177 ms | 74 / 611 / 591 ms | 33 / 66 / 52 ms | 1.10x / 0.37x / 0.29x |
| 100k plain lines | `copy-mode` / `search-backward` / `search-forward` | 26 / 166 / 171 ms | 33 / 67 / 60 ms | 34 / 57 / 42 ms | 1.31x / 0.34x / 0.25x |

- Entry: copy mode copies the pane's Ghostty pages. Ghostty copied a page row by row, and a styled row cell by cell, adding each cell's style to the new page's style set, so a coloured history took 2.5x as long to copy as a plain one. A whole page is now copied as its memory (Ghostty patch 0035), and both take what a plain one did. Entry is still **Worse** than tmux's (1.2x, the copy's page faults); on compressed history it is faster (*Copy mode and search*, above). RSS in copy mode is unchanged (1961 MB plain, 1971 MB styled, against 1961 MB before).
- Search: a line is read as text when every cell is one character of one column, of any style, colour or link, past ASCII or not. Before, only lines of ASCII were, and the `é` at the end of each styled line sent every one through its cells. A plain pattern is looked for in the line's UTF-8 text from a cell's first byte, and a regular expression runs over it as before. Lines with wide characters, clusters, tabs or line drawing, and wrapped lines, still go through their cells.
- Plain lines are faster too: the text of a row is read into buffers kept from row to row.
- Counts: with the search faster, a count over a coloured history now finishes inside tmux's 200 ms (`WINDOW_COPY_SEARCH_ALL_TIMEOUT`). Over 100k styled lines, before counted `1000+` where tmux counts exactly (`status=200 bytes`: tmux 100000 in 0.19 s, before `1000+`, after 100000 in 0.05 s; `items/[0-9]+5 `: 9999, `1000+`, 9999). A pattern on every line (`INFO`) runs out of tmux's time too (`1000+` in 0.20 s), where vtmux now counts 100000 in 0.04 s.
- The results are what they were: a unit test compares the text path with reading every line as cells over random styled content, and a probe compared the branch with main over the CLI (9 seeds x 136 searches x 5 steps: `#{search_count}`, the partial flag, the cursor, the scroll position and `#{search_match}`, emacs and vi keys); 0 differ. `capture-pane` of all of history, `-M` included, prints the same bytes as main (6 seeds x 1760 captures).

Raw runs: [`test-health/2026-11-09-copy-mode-styled-history.md`](test-health/2026-11-09-copy-mode-styled-history.md).

## CPU and throughput

`throughput` has a pane `cat` 100 MB of log text, plain or with 19% of the bytes SGR, with no client attached. vtmux consumes it with 0.37x (plain) and 0.46x (SGR) of tmux's server CPU, in 2.3x to 2.4x less wall time. Creating panes is the loss: 1.9x the CPU.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| throughput, 100 MB plain log, no client | wall / server CPU | 2.44 s / 2.38 s | 1.02 s / 0.89 s | 0.37x CPU |
| | peak RSS | 5.4 MB | 11.8 MB | 2.2x |
| throughput, 100 MB log, 19% SGR bytes | wall / server CPU | 3.26 s / 3.07 s | 1.43 s / 1.40 s | 0.46x CPU |
| | peak RSS | 5.4 MB | 11.5 MB | 2.1x |
| idle, 1 session, 60 s | CPU over 60 s | 0 ms | 0 ms | - |
| | wakeups | 0 / s | 0 / s | - |
| scale, 400 panes (10 x 10 x 4 idle `cat`) | CPU to create | 200 ms | 380 ms | 1.9x |
| startup (`new -d` + `kill-server`, 100x) | `new -d` | 12.2 ms | 8.9 ms | 0.73x |
| | `kill-server` returns | 2.0 ms | 1.4 ms | 0.70x |
| | server pid gone (from the start of `kill-server`) | 2.5 ms | 2.5 ms | 1.0x |

- The idle server uses no CPU and takes no wakeups, same as tmux.
- Startup is a wash: the server pid is gone 2.5 ms after `kill-server` starts for both.
- Throughput wall time is coarse. The done marker is polled with `capture-pane` every 0.2 s, so each wall time is accurate to about 0.2 s (up to 20% of vtmux's 1.0 s plain run).
- That error adds about the same to both binaries, which pulls the ratio toward 1. Server CPU is not affected beyond the cost of the `capture-pane` calls.
- Peak RSS in throughput is about 6 MB above tmux's: the per-pane terminal plus the history it keeps for the 100 MB of output, capped by the default `history-limit` of 2000.

- Ghostty patch 0034 (tmux's line flags, set on every print path) costs nothing measurable: against the build before it, 1.04x the server CPU on the plain log and 1.00x with SGR, and 0.96x to 1.05x in *Agent panes* ([`test-health/2026-11-08-tmux-line-flags.md`](test-health/2026-11-08-tmux-line-flags.md)).

Raw runs: [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *Full `bench.py` run*.

## Rendering and attached clients

vtmux composes whole frames (D14), so its cost follows how many frames it draws, not how much output arrives. Under a flood it draws fewer, larger frames than tmux, and its diffed frames send far fewer bytes.

The cost is memory: 2.2x to 3.4x tmux's RSS with clients attached. How clients are attached and counted: *Method*, *Attached clients*.

Server CPU only below; the client's CPU is in the raw run (at most 0.02 s for either binary). Terminal bytes are per input byte, or per synchronized-update frame where sync is on (total terminal bytes / `ESC[?2026h` count).

**One client, a 100 MB flood** (`throughput_attached`, 200x50 client):

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| plain log, sync off | wall / server CPU | 2.85 s / 2.63 s | 1.63 s / 1.51 s | 0.57x CPU |
| | terminal bytes per input byte | 1.03 | 0.047 | 0.05x |
| plain, sync on | wall / server CPU | 2.85 s / 2.66 s | 1.83 s / 1.64 s | 0.62x |
| | sync frames / bytes per frame | 25816 / 4.1 KB | 754 / 7.3 KB | |
| 19% SGR, sync off | wall / server CPU | 4.08 s / 3.90 s | 1.83 s / 1.65 s | 0.42x |
| | terminal bytes per input byte | 1.04 | 0.013 | 0.01x |
| 19% SGR, sync on | wall / server CPU | 4.07 s / 3.92 s | 1.83 s / 1.65 s | 0.42x |
| | sync frames / bytes per frame | 25776 / 4.2 KB | 157 / 8.8 KB | |
| | peak RSS (any row) | 7.5 MB | 16.6-16.8 MB | 2.2x |

**Redraw-heavy output** (`render`, 4 panes x 3000 cursor-addressed frames, 22 MB). vtmux sends 0.07 MB where tmux sends 10.1 MB, about 140x fewer bytes, at 0.3x the server CPU.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| sync off | wall / server CPU | 0.67 s / 0.65 s | 0.22 s / 0.19 s | 0.29x |
| | terminal bytes | 10.1 MB | 0.07 MB | 0.007x (about 140x fewer) |
| sync on | wall / server CPU | 0.67 s / 0.63 s | 0.22 s / 0.20 s | 0.32x |
| | sync frames / bytes per frame | 1357 / 7.4 KB | 23 / 3.5 KB | |
| | peak RSS | 5.4 MB | 13.5 MB | 2.5x |

**Eight clients** (`many_clients`, a 20 MB flood in one pane):

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| sync off | wall / server CPU | 1.23 s / 1.22 s | 0.41 s / 0.23 s | 0.19x |
| | terminal bytes (sum) | 29.2 MB | 0.5 MB | 0.02x |
| sync on | wall / server CPU | 4.77 s / 4.68 s | 0.41 s / 0.29 s | 0.06x |
| | RSS attached idle / after flood (sync off) | 6.6 / 7.4 MB | 22.3 / 24.5 MB | 3.4x / 3.3x |

- A frame waits while the client still has an unwritten one. tmux defers its redraw the same way (`server_client_check_redraw`).
- While a pane still has output waiting, frames of that output go at most every 10 ms. Each frame recomposes only the rows that changed.
- With sync on, where frames can be counted, vtmux sends 754 frames of about 7 KB for the plain log where tmux sends 25816 of about 4 KB. Per input byte that is 0.047 against 1.03.
- Sync-off rows have no frame count.
- In `render` each pane repaints full screens with cursor addressing. Each frame on the terminal is the latest picture, so repaints a pane made within one frame interval never reach the terminal. The final screen is the same.

Raw runs: [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *Full `bench.py` run*.

## Latency

`bench.py key_echo`: a 200x50 client with sync on, the window split in two, a raw echo program in the active pane and `cat` or `yes` in the other. 200 keys go 30 ms apart; each echo's arrival is stamped in the reader thread.

Key echo is 0.1 ms behind tmux with no flood, and level with it next to one. Median of 3 runs of p50 / p90 / max:

| other pane | tmux 3.8 | vtmux | vtmux / tmux (p50) |
|---|---|---|---|
| idle | 0.21 / 0.28 / 0.39 ms | 0.33 / 0.43 / 0.70 ms | 1.6x |
| `yes` | 0.52 / 0.76 / 1.45 ms | 0.49 / 0.70 / 1.70 ms | 0.94x |

- Only flood output is paced. A frame that also shows a key's echo from a pane that was not flooding, a pane to redraw whole, or a status redraw goes at once.
- Each pane keeps one Ghostty render state that re-reads only the rows Ghostty marked dirty. Each client caches its composed rows. A turn that changes no row or cursor builds no frame.
- What is left with no flood (0.33 ms against 0.21, 1.6x) is mostly O(screen) work per frame: building the window's drawing context and status, copying the cached rows into the frame buffer, and comparing every row in the diff.

Raw runs: [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *key_echo*.

## Agent panes

`bench.py agent_panes`: each pane runs `scripts/agent_tui.py`, a synthetic pi-like agent TUI (*Scenarios*, under *Method*). There are 2 panes per window, one 200x50 client on window 0 and a second on window 1. Server CPU is in seconds per 10 s of steady state.

vtmux uses 0.85x to 1.3x tmux's CPU here, and 18-34 MB of RSS against tmux's 6-8 MB.

| panes / clients | tmux 3.8 | vtmux | vtmux / tmux | vtmux RSS (tmux) |
|---|---|---|---|---|
| 5 / 1 | 0.14 s | 0.14 s | 0.98x | 17.7 MB (6.1) |
| 5 / 2 | 0.15 s | 0.19 s | 1.28x | 20.2 MB (6.2) |
| 10 / 1 | 0.22 s | 0.21 s | 0.96x | 21.7 MB (6.6) |
| 10 / 2 | 0.23 s | 0.27 s | 1.14x | 24.0 MB (6.7) |
| 20 / 1 | 0.41 s | 0.35 s | 0.85x | 31.2 MB (8.0) |
| 20 / 2 | 0.45 s | 0.44 s | 0.97x | 33.6 MB (8.1) |

- Both send the same number of synchronized frames: 68 a second per client.
- tmux's cost is mostly parsing, so it grows with panes. vtmux parses cheaper and pays per frame instead, so the second client costs it more.
- The synthetic load is lighter than the owner's real pi panes. No recorded real output was replayed.
- Where vtmux's time goes, from `perf record -g` at 5 panes / 2 clients: reading Ghostty's dirty rows (about a sixth of the server's samples), parsing pane output, composing the changed rows (an eighth).
- Then the frame diff and building the window's drawing inputs (a twelfth, spread over option lookups and the panes' screens).

How it got here:

- The rows are from `perf_border_style_cache`. Main before it, in the same run: 1.16x, 1.55x, 1.02x, 1.38x, 0.91x, 1.19x (1.6x at 5 panes / 2 clients).
- That change expands each pane's border style once and keeps it until a border or status redraw, as tmux does (`window_pane_get_border_style`).
- It also removed a few per-frame costs: option lookups that missed twice, Ghostty's style lookup for unstyled cells, cell-by-cell pane copies, and the client list built for every frame.
- The styles are kept per client and expanded again when a pane switches screens or dies (medium_border_cache_stale). Against fefa2d4 in one run: 1.01x its server CPU at 5 / 2 and 0.95x at 20 / 1, within the noise, so the rows stand.
- Between `lows_render` and `8a7d31f` these were 3.8-5.7x tmux. The build before the fix, in the same run: 0.68 s at 5 / 1, 2.60 s at 20 / 1.
- The cause: each pane no client shows had its screen view dropped at the end of every turn. Each synchronized update reads that view (`?2026h` holds the screen, its end compares the rows), so every hidden agent pane was read whole about 30 times a second.
- Now a hidden pane keeps its view until it has had no output for a second (*Hidden panes*). That costs about 0.2-0.5 MB more RSS per busy hidden pane (31.2 MB against 27.1 at 20 / 1).

Raw runs:

- [`test-health/2026-11-06-border-style-cache.md`](test-health/2026-11-06-border-style-cache.md): the table.
- [`test-health/2026-11-06-border-cache-stale.md`](test-health/2026-11-06-border-cache-stale.md): the per-client styles.
- [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *Agent panes fixed*: the `lows_render` regression and its fix.

## Hidden panes

`scripts/bench-hidden-panes.py <binary> <label>`, 4 runs per binary, alternating. It attaches a 100x51 client and opens 100 more windows one after another, so each is drawn once.

Each pane prints 60 lines of 85 characters, so its 100x50 screen is full. Then it goes back to window 0 and times 40 `C-b n` switches, from the key to the client's last output byte.

A hidden pane costs vtmux more memory than tmux. Switching to it is faster.

| | tmux 3.8 | vtmux |
|---|---|---|
| server RSS, 101 windows | 9.8 MB | 18.6-19.4 MB |
| RSS per hidden pane | 48 KB | 74-80 KB |
| `C-b n` to the last byte, median | 5.1-5.2 ms | 2.5-2.6 ms |

- A pane's screen view (the copied grid and Ghostty's render state, [`ScreenView`](../crates/vtmux-vt/src/snapshot.rs)) is about 0.7 MB for a full 100x50 screen.
- Since `lows_render`, the views of panes no attached client's window shows are dropped after a turn's frames (`release_hidden_screens`). Before it, 101 windows took 89.7 MB.
- A pane's view is read again whole when it is next drawn.
- A pane with no output since a client last showed it is released at the end of the first turn it is hidden.
- A pane with output while hidden keeps its view until it has had none for a second (`HIDDEN_VIEW_IDLE`). Its synchronized updates then read it incrementally (*Agent panes*), and the server wakes to release it once the output stops.
- A choose-tree or display-panes preview reads a released pane again for each frame it draws.

Raw runs: [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *Agent panes fixed* (re-run after `8a7d31f`).

## Live upgrade

`bench.py upgrade` (median of 3, release build): `vtmux-upgrade -b` onto the same binary, with N panes (one window each, 200x49, `history-limit 100000`) that printed L lines of 100 columns (every tenth coloured), one attached 200x50 client (sync on) and two control clients attached. tmux has no live upgrade: its answer is `kill-server`. *answered* is until the command returns, which it does once the new image serves and every attached client has re-attached; *redrawn* until the attached client has had its terminal restarted and then its status line. `fresh` upgrades as soon as the history is full, `idle` once the compressor has finished with it (RSS settled, *idle s* after the output). `-rows` upgrades onto a wrapper that reports another Ghostty patch stack (`UPGRADE_STATES`), so the history goes as rows, as onto a build with another pin.

Before is `1b0dba1`, after is `lu_upgrade_perf`:

| case | history lines | answered before | after | redrawn after | snapshot before | after | RSS before the upgrade | RSS after it, before | after |
|---|---|---|---|---|---|---|---|---|---|
| 1 x 10k, fresh | 9952 | 123 ms | 39 ms | 41 ms | 0.9 MB | 1.3 MB | 29.5 MB | 32.8 MB | 16.6 MB |
| 1 x 10k, idle | 9952 | 42 ms | 31 ms | 33 ms | 1.3 MB | 1.3 MB | 15.5 MB | 19.8 MB | 16.7 MB |
| 1 x 100k, fresh | 99952 | 1036 ms | 161 ms | 163 ms | 6.0 MB | 11.5 MB | 169.4 MB | 182.5 MB | 28.3 MB |
| 1 x 100k, idle | 99952 | 186 ms | 81 ms | 82 ms | 11.5 MB | 11.5 MB | 27.3 MB | 51.0 MB | 28.4 MB |
| 10 x 10k, fresh | 99520 | 1040 ms | 117 ms | 119 ms | 6.1 MB | 11.3 MB | 170.0 MB | 183.6 MB | 31.2 MB |
| 10 x 10k, idle | 99520 | 206 ms | 100 ms | 101 ms | 11.3 MB | 11.3 MB | 30.6 MB | 54.2 MB | 31.2 MB |
| 10 x 100k, fresh | 999520 | refused | 749 ms | 751 ms | - | 113.1 MB | 1566.7 MB | - | 146.5 MB |
| 10 x 100k, idle | 999520 | 1703 ms | 596 ms | 598 ms | 113.1 MB | 113.1 MB | 145.6 MB | 364.3 MB | 146.4 MB |
| 1 x 100k, fresh-rows | 99952 | | 1019 ms | 1021 ms | | 5.8 MB | 169.4 MB | | 170.3 MB |
| 10 x 10k, fresh-rows | 99520 | | 1030 ms | 1032 ms | | 5.9 MB | 169.9 MB | | 170.8 MB |
| 10 x 100k, fresh-rows | 999520 | | 9869 ms | 9871 ms | | 56.7 MB | 1566.8 MB | | 1567.7 MB |
| 10 x 100k, idle-rows | 999520 | | 10422 ms | 10424 ms | | 56.7 MB | 145.6 MB | | 1567.7 MB |

- Decision 13's target is 10 panes x 100k lines in under 1 s. It is met now: 598 ms with the history compressed, 751 ms with none of it compressed yet. Before, the compressed case took 1.7 s and the uncompressed one was refused every time (the dry run passed its 3 s limit).
- What changed (`lu_upgrade_perf`): the blob checksum of vendored patch 0017 is Wyhash instead of Zig 0.17's `XxHash3`, which hashed about 250 MB/s and was most of the 1.7 s (each 113 MB stream was hashed three times: export, dry run, new image); the old image asks the new binary its patch stack (`--vtmux-patch-stack`) and, when it is its own, compresses every pane's history first, on up to 16 threads (0.17 s at 10 x 100k, 0.85 s on one), so all history goes in Ghostty's form and nothing is decoded or imported as rows; the new image drops the stream and its decoded records before it serves and gives the freed heap back (`malloc_trim`).
- Where the time goes at 10 x 100k idle (one run of a build stamping each step to a file, 547 ms in all): snapshot 31 ms, encoding 89 ms (mostly BLAKE3 over the 113 MB body), memory file and the dry run's start 25 ms, the dry run 194 ms (reading 31, decoding with the hash check 79, rebuilding 76), the exec 20 ms, the new image 184 ms (reading 32, decoding 82, rebuilding 64), then the re-attach. A fresh history adds the compression, 0.17 s.
- RSS after the upgrade is what it was before (146 MB at 10 x 100k, was 364 MB), and after a fresh upgrade far less than before it (1567 MB to 147 MB): the history arrives compressed, as the compressor would have left it.
- The snapshot of uncompressed history is now the larger one (Ghostty's compressed pages, 11.3-11.5 MB per 100k lines, against 5.8-6.1 MB of vtmux rows).
- Onto a binary of another patch stack the history goes as rows, decoded and imported twice (dry run, new image): about 1 s per 100k lines, 9.9-10.4 s at 10 x 100k, and the new image holds it resident (1.57 GB) until its compressor has been through it (decision 23). It is no longer refused for its size: the dry run may take 3 s plus a second per 100k rows it imports (13 s here). Decision 13's second is out of reach on that path without importing in parallel; it is the upgrade after a Ghostty pin bump, not the daily one.
- Since `lows_batch_j` the server serves on while the dry run runs and takes the snapshot again once it has passed (live-upgrade spec, step 4), so the loop is held only for the two snapshots instead of through the dry run. That costs a second snapshot and encoding: at 10 x 100k fresh 715 ms before, 855 ms after (`d1c808b` against it, median of 3 each, load 15-17); onto another patch stack 10.1 s before, 12.4 s after (the rows exported twice), of which the loop was held about 10 s before and two exports now. `crates/vtmux/tests/upgrade.rs` `typing_echoes_while_the_dry_run_runs` pins that a key echoes during a dry run held at its start.
- The snapshot size is taken by a second upgrade right after the timed one, through a wrapper that counts the dry run's snapshot descriptor.
- The host was shared (load 5-8, other workers' builds), so the times are noisy.
- The same with streaming panes, in a debug build: `an_upgrade_under_load_loses_and_repeats_nothing` (`crates/vtmux/tests/upgrade.rs`) prints its time to redraw.

Raw runs: [`test-health/2026-11-07-live-upgrade-bench.md`](test-health/2026-11-07-live-upgrade-bench.md) (before), [`test-health/2026-11-07-live-upgrade-perf.md`](test-health/2026-11-07-live-upgrade-perf.md) (after).

## Micro-benchmarks

`scripts/bench-micro.py` covers what `bench.py` doesn't. It uses `bench.py`'s servers, attached clients and `/proc` probes. Each row is the median of 3 runs, taken on 2026-11-08 under `flock /tmp/vtmux-stress.lock` at `503f2a4`, except the `clients` rows: 2026-11-09 at `417a3c4` (*Many clients*). The host was badly loaded (load 118 at the start, 10 at the end; 19 to 10 for `clients`), so read the CPU rows as rough. Server CPU is seconds per 10 s of steady state.

| scenario | tmux 3.8 | vtmux | vtmux / tmux | note |
|---|---|---|---|---|
| `flood`: 8 tiled panes each `cat` 20 MB at once, one 200x50 client, sync on | 5.47 s wall, 5.31 s CPU, 35.8 MB to the terminal | 1.38 s, 1.25 s, 1.14 MB | 0.24x CPU | 5152 against 118 frames. Peak RSS 14.3 against 28.2 MB, **Worse** |
| `many_panes`: 50 `agent_tui.py` panes, 2 per window, one client | 2.14 s | 1.38 s | 0.64x | RSS 11.5 against 56.7 MB, **Worse** |
| `many_panes`: 100 panes | 3.37 s | 2.71 s | 0.80x | RSS 17.8 against 100 MB, **Worse** |
| `clients`: 16 clients (120x40, `window-size smallest`) on one window of 4 agent panes | 0.31 s | 0.51 s | 1.65x | **Worse**, was 4.4x. RSS 7.1 against 25.8 MB |
| `clients`: 32 clients | 0.48 s | 0.79 s | 1.6x | **Worse**, was 5.2x. RSS 9.2 against 41.3 MB |
| `status`: `status-interval 1`, window and session loops, conditionals, strftime and `#{E:}` in the status, 20 windows, 4 clients | 0.055 s | 0.032 s | 0.59x | |
| `jobs`: 8 different `#()` jobs in `status-right`, `status-interval 1`, 2 clients | 0.104 s | 0.122 s | 1.17x | **Worse**, about a job's worth of CPU |

**Memory over 30 minutes** (`memgrowth`). 10 agent panes and one 200x50 client in each server, both servers at once, RSS every minute for 1800 s (`load 8-14`):

| | tmux 3.8 | vtmux |
|---|---|---|
| RSS at 60 s | 6.5 MB | 21.9 MB |
| RSS at 1800 s | 6.5 MB | 22.1 MB |
| server CPU over the 30 minutes | 57.6 s | 52.4 s |

Neither server grows. vtmux added 0.2 MB in 30 minutes, at 1560 s.

**Many clients.** tmux writes each pane update to every client's terminal as it comes. vtmux composes a whole frame per client (D14): its window, status line, the diff against that client's last frame, and the encoding. Before `perf_many_clients`, 16 clients showing the same window paid that 16 times, 4.4x tmux's CPU (5.2x at 32).

Now clients that show a window alike share the work within a render turn (`render/share.rs`):

- Clients of the same session, size, view and terminal (caps and reported colours) take the first one's drawing of the window and its composed rows, when their own drawing would be the same: their border styles (kept per client as tmux keeps them) are the same, and the drawing read neither the client it was drawn for (`#{client_prefix}` in a pane status line or border style) nor its own `#()` job or animation. The time and each pane's foreground process (`#{pane_current_command}`, `#{pane_current_path}`) are read once per render turn (`live.rs`), so a drawing that shows them is every client's.
- A client whose terminal shows what another's showed, and whose new screen is that client's, takes that client's frame bytes. Plain frames (nothing over the window but the status line) are compared by row tags, status rows included, before they are drawn, and the screen is shared, not copied.
- Every client still gets the bytes it would get alone. A server test runs one script with sharing on and off and compares every client's bytes, and debug builds draw each shared drawing and frame again the client's own way and check.

`bench-micro.py clients` (`CLIENTS_N="1 2 5 10 16 32"`, 120x40 clients, sync on), median of 3, load 19 to 10, server CPU in seconds per 10 s:

| clients | tmux 3.8 | vtmux before | vtmux after | before / tmux | after / tmux |
|---|---|---|---|---|---|
| 1 | 0.17 | 0.28 | 0.28 | 1.7x | 1.7x |
| 2 | 0.16 | 0.32 | 0.28 | 1.9x | 1.7x |
| 5 | 0.20 | 0.59 | 0.34 | 3.0x | 1.7x |
| 10 | 0.25 | 0.90 | 0.40 | 3.6x | 1.6x |
| 16 | 0.31 | 1.34 | 0.51 | 4.4x | 1.65x |
| 32 | 0.48 | 2.50 | 0.79 | 5.2x | 1.6x |

- Before is main `0107471`, after `417a3c4`, in the same run. Terminal bytes and frames are the same for both.
- The ratio no longer grows with the clients: vtmux is about 1.6x-1.7x tmux from 1 client to 32. Each client after the first costs vtmux about 0.016 s per 10 s here, tmux 0.010.
- What each further client still costs: writing its frame to its socket and reading its acknowledgement (D14's backpressure; tmux has none), its status line and its terminal's mouse modes.
- Clients of different sizes, terminals or sessions (a linked window) draw alone, as before: the *Agent panes* table's second client is on another window and did not move (the PERF GATE run in the raw record).
- RSS fell with it: 41.3 MB at 32 clients, was 52.4. Clients that share a drawing share its composed rows.
- Earlier, `audit_perf_hotpaths` made a client's frame build its format context only for a status redraw (it lists every attached client, so a tick of n clients was O(n^2)): 23% fewer server instructions at 32 clients (`perf stat -e instructions:u`, 8 s, two runs each: 22.3 and 21.9 G before, 17.0 and 16.9 G after).

**Profiles.** These come from `perf record -g` of a `release` build with frame pointers and line tables, with Ghostty built `ReleaseFast` (`LIBGHOSTTY_VT_SYS_OPTIMIZE`; a debug-info build otherwise gets Ghostty's `Debug` mode and profiles its integrity checks):

- **16 clients, before `perf_many_clients`.** Rendering was 81% of the server: building each pane's drawing inputs (`Source::window`, 18%: option lookups, styles, the pane's screen), composing rows (18%), the frame diff (15%), the client list described above (6%) and copying cells (10%), all per client.
- **32 clients, after it.** Rendering is 64%. Writing the frames to the clients' sockets is 19% of it, the window drawing 13% (mostly the first client's: `Source::window` 4%, composing 3.4%), each client's cursor 2.2% and mouse modes 4.1%. Reading the clients' acknowledgements is another 10%.
- **100 agent panes, one client.** Reading Ghostty's dirty rows at the end of each synchronized update is 33%, mostly for the 98 panes no client shows. Then parsing pane output, 20%. A hidden pane's view is read at each `?2026h` so that switching to it mid-update shows the held screen (*Hidden panes*). The same rows would be read at the next one anyway.
- **Creating 400 panes.** The server's own code is 30%. The rest is the kernel: `fork` copying page tables (`copy_page_range`) and the copy-on-write faults after it. A bigger server costs more per `fork`, and tmux forks too (`fdforkpty`). Spawning with `vfork` or `posix_spawn` would avoid it, but the child's setup (controlling terminal, signals, directory fallbacks) has to move with it.
- **A search over 100k lines, before the fix.** 78% was comparing cells (`matches_at`), and 31% of all samples went to reading rows as cells.

Raw runs: [`test-health/2026-11-08-perf-hotpaths.md`](test-health/2026-11-08-perf-hotpaths.md); *Many clients* and the 32-client profile: [`test-health/2026-11-09-perf-many-clients.md`](test-health/2026-11-09-perf-many-clients.md).

## With the owner's config

`owner_idle` (median of 3) and `owner_startup` (20 runs). The owner's real `~/.tmux.conf` (dotfiles `870c12a`, sha256 `72a481ffac7e`), the TPM plugins and mu-crew-dotfiles `d633e15` are copied into a hermetic `HOME`. The plugin revisions are in the raw run.

With this config the idle server is not idle, and the cause is outside it. vtmux does less server work per request (0.28x); the memory gap is the same as for a bare server.

| scenario | metric | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|---|
| owner_idle, 1 session, 60 s | RSS | 4.6 MB | 9.7 MB | 2.1x |
| | server CPU over 60 s | 171 ms | 47 ms | 0.28x |
| | server wakeups | 3.1 / s | 1.05 / s | 0.34x |
| | poller CPU over 60 s (not the server) | 180 ms | 190 ms | 1.06x |
| owner_startup, `start-server` until `display -p ok` answers, 20x | median (min - max) | 102.4 ms (93.8 - 119.3) | 101.5 ms (86.4 - 117.4) | 0.99x |

- The mu-crew poller wakes about once a second and runs `tmux show-options -g` (and `set-option` when a value changes) as clients of the server. The server's 171 ms (tmux) and 47 ms (vtmux) over 60 s are the cost of answering those clients.
- The RSS gap is the same 5 MB as for a bare server (2.1x).
- The poller's own CPU (a Python process plus the `tmux` client processes it starts, counted from `/proc/<pid>/stat` with children) is 180 ms and 190 ms. It is outside the server.
- Only the cheapest probe is on: `uptime`, a `/proc` read every 60 s.
- The owner's real list adds `murmur_remote memory cpu uptime muzzle`, which starts `murmur status` and `muzzle status` every 5 to 10 s. That is not measured here: it needs a network and the owner's state.
- Sourcing the config takes about 100 ms for both. A bare `new -d` takes 9 to 12 ms (*CPU and throughput*), so the rest is the config.
- Where that time goes was not profiled. The config's synchronous `run-shell` calls, such as `mu-crew-bind`, each start a shell and several `tmux` clients.
- The 20 runs overlap for the two binaries (94 to 119 ms and 86 to 117 ms).

Raw runs: [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md), *owner_idle* and *owner_startup*.

## Binary size and build profile

The release profile (`Cargo.toml`) is whole-program LTO in one codegen unit, stripped: `lto = "fat"`, `codegen-units = 1`, `strip = true`. Against cargo's default, same commit:

| | default | vtmux's profile | |
|---|---|---|---|
| `target/release/vtmux` | 13.1 MB | 9.8 MB | -26% (now stripped by cargo) |
| stripped, as shipped | 10.7 MB | 9.8 MB | -9% |
| gzipped, in the tarball | 4.41 MB | 4.28 MB | -3% |
| server CPU, agent panes (5-20 panes, 1-2 clients) | | | -13% to -14% |
| server CPU, 50 and 100 agent panes | | | -17%, -18% |
| server CPU, 16 and 32 clients, heavy status line | | | -6% to -7% |
| key echo p50, other pane idle / next to `yes` | 0.43 / 0.54 ms | 0.38 / 0.45 ms | -12% / -17% |
| output CPU, no client, 100 MB plain / SGR | 1.03 / 1.64 s | 1.05 / 1.71 s | flat: Ghostty's parser, which LTO doesn't touch |
| live upgrade, 10 x 100k, fresh / idle | 950 / 774 ms | 943 / 804 ms | flat (noise) |
| RSS | | | about 1 MB lower |
| build after touching `vtmux-core` | 108 s | 118 s | +10 s |

- tmux 3.8 is 1.3 MB stripped, plus the libevent and ncurses it links. vtmux carries Ghostty statically (1.35 MB of its text) and 6.5 MB of Rust text.
- `panic = "abort"` would save 0.67 MB more, but a live upgrade catches a panic in its rebuild to switch back (`server/resume.rs` `guarded`), and the restore catches a panicking decode. Abort would take every pane down with it, so panics unwind.
- No `opt-level = "s"`/`"z"` (they would also build Ghostty `ReleaseSmall`) and no debug info (cargo's `DEBUG=true` would build Ghostty in Zig `Debug`). Ghostty stays `ReleaseFast`: its text is byte-identical under both profiles.
- Rows elsewhere on this page dated before 2026-11-09 were measured with the default profile, so the render-bound ones are now a little pessimistic.

Raw runs, every candidate's size and build time: [`test-health/2026-11-09-release-profile.md`](test-health/2026-11-09-release-profile.md).

## Method

### Where the numbers come from

- Speed and memory are claimed only where a recorded run measures them (D24 in the [spec](specs/2026-10-29-vtmux.md)). Every number on this page comes from a run under [`docs/test-health/`](test-health/).
- Most rows come from the runs of 2026-11-05 in [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md): commands, load and raw output, with the JSON next to it.
- That run measured vtmux main `ada47ba` (release build) against tmux 3.8 from `cargo xtask ref-tmux`. `bench.py` rows are the median of 3 runs.
- Exceptions: the *Hidden panes* table was re-run after the fix of the agent-panes regression (`8a7d31f`, same host, load 6.2 to 8.4; *Agent panes fixed* in the same record).
- The *Agent panes* table comes from the later `perf_border_style_cache` run; *Many panes that never stop* and the `hist_compress_busy` figures from their own records (linked in each section).
- The injected-regression check of the guard (*Regression guard*) is a separate scratch run.

### Host and load

- A devserver: CentOS Stream 9, Linux 6.16, AMD EPYC Genoa, 96 cpus, shared with other agents.
- Load was 2.6 to 4.9 (`uptime`) over the refresh runs, every one under `flock /tmp/vtmux-stress.lock`.
- The host is shared, so run when `uptime` load is under 4, under that lock, and one bench at a time.
- macOS numbers have not been collected yet.

### How the server is measured

- The server pid comes from `display -p '#{pid}'` on a private socket (`-L` and `TMUX_TMPDIR`), and is checked gone after `kill-server`. Pids are never found by name.
- Only the server process is measured, never its pane children.
- **Linux CPU:** `/proc/<pid>/stat` utime+stime (fields 14 and 15) deltas per phase, which count all threads and exclude children.
- Idle and the attached scenarios use the nanosecond `schedstat` sum, because 10 ms ticks are too coarse.
- Never `cutime`/`cstime`, `getrusage` of children or `time(1)`: those include the reaped pane processes.
- **Linux RSS:** settled `VmRSS`. The per-phase peak is `VmHWM` after `echo 5 > /proc/<pid>/clear_refs`.
- **Wakeups:** deltas of `voluntary_ctxt_switches` over all threads.
- **macOS:** `ps -o rss=,time=` sampled every 100 ms (peak RSS is the largest sample; wakeups are not reported). `scale` raises `ulimit -n` to 4096, or is skipped with the reason if it can't.

### Attached clients

- The client is the binary's own `attach` on a PTY the bench drains with a Python thread (`Client` in `scripts/bench.py`). Not `script(1)`: its flags differ between Linux and macOS.
- The thread counts every byte the terminal would receive and every synchronized-update begin (`ESC[?2026h`). Client CPU is read from `/proc/<client pid>` like the server's.
- Both binaries get the same `TERM` (xterm-256color), the same explicit `default-terminal`, `terminal-features`, `window-size`, `status-right` and `status-interval`.
- Both servers enable synchronized output only from the terminal's `DECRQM 2026` reply (vtmux: `input.rs:215`, `render.rs:81`; tmux 3.8: `tty_keys_sync`).
- So each scenario runs twice: the reader answers the query (`sync on`) or never does (`sync off`). The bench checks that the query arrived before it trusts a sync-on row.
- Measurement starts after all clients have attached and the output went quiet. It ends when the done marker shows and the terminals have been quiet for 0.3 s.

### Owner scenarios

- `owner_idle` and `owner_startup` are Linux only, skipped elsewhere with the reason.
- `HOME` is `<tmp>/<run>/home`. It holds a copy of `~/.tmux.conf` (symlinks followed), `~/.tmux/plugins` and `~/.local/share/mu-crew-dotfiles` (without `.git`, `target` or `node_modules`). Their git revisions and the config's sha256 go into the output.
- Two changes to the copy: `@fingers-skip-wizard 1` first (no install wizard), and `@mu_crew_probes` set to `uptime`, the cheapest probe.
- `<tmp>/<run>/bin/tmux` is a symlink to the binary under test, first on `PATH` for both binaries. The config, TPM and the poller all run `tmux` from `PATH`; vtmux is run as `tmux` (D20).
- After each run the bench waits up to 5 s for every process whose environment has the hermetic `HOME` (the poller leaves when its server is gone), kills what is left, and prints how many it had to kill.
- Processes are matched by environment, never by name, so neither the bench's shell nor the owner's real poller can match.
- The poller's CPU is `utime+stime+cutime+cstime` of its pid from the pidfile, which includes the `tmux` clients it ran. It is reported next to the server's, not added to it.

### Scenarios

`scripts/bench.py` runs each scenario against both binaries, and against `BASE=<binary>` (shown as `base`) to compare with an earlier build.

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
| `upgrade` | vtmux only: `UPGRADE_PANES` (1 10) panes x `UPGRADE_LINES` (10000 100000) lines of 100 columns, one window each, `history-limit 100000`, one 200x50 client (sync on) and two control clients; `vtmux-upgrade -b` onto the same binary as soon as the history is full and again on a new server once the compressor is done (at most `UPGRADE_IDLE_LIMIT`, 180 s); `UPGRADE_STATES` adds `fresh-rows` and `idle-rows`, the same onto a wrapper reporting another Ghostty patch stack: time to the answer and to the client's redraw, snapshot size, RSS before and after | What decision 13 of the live-upgrade spec measures: how long a dogfood upgrade takes. |
| `agent_panes` | `AGENT_PANES` (5 10 20) panes of `scripts/agent_tui.py`, 2 per window, with `AGENT_CLIENTS` (1 2) 200x50 clients answering the 2026 query, the first on window 0 and the second on a session that links window 1; 2 s to settle, then `AGENT_SECS` (10) measured: server and client CPU, terminal bytes, sync frames, wakeups, RSS | What many agent TUIs (pi, ink renderers) cost the server. |

`agent_tui.py` mimics pi's alt-screen renderer at real speed, with deterministic content per pane seed:

- Every update is in synchronized output and rewrites only the changed rows (`CSI r;1H`, `EL`, the line, SGR reset, an empty OSC 8).
- A truecolour braille spinner every 80 ms.
- A streamed word every 20-80 ms (the whole chat area once it scrolls).
- A block of tool output every 2-5 s, and a footer once a second.

## Running the benchmarks

```
mise exec -- python3 scripts/bench.py                 # all scenarios; Markdown on stdout, JSON and Markdown under target/bench/
mise exec -- python3 scripts/bench.py idle scale      # chosen scenarios
IDLE_SECS=5 RUNS=1 STARTUP_N=5 THROUGHPUT_MB=10 RENDER_FRAMES=200 MANY_MB=5 AGENT_PANES=10 AGENT_SECS=5 mise exec -- python3 scripts/bench.py   # quick development run
RUNS=3 mise exec -- python3 scripts/bench.py agent_panes   # the agent-panes table (about 11 min with BASE)
BASE=/path/to/older/vtmux mise exec -- python3 scripts/bench.py
BASE=/path/to/older/vtmux mise exec -- python3 scripts/bench.py trickle   # not in the default set
```

Times for the `bench-history-memory.sh` and `bench-hidden-panes.py` rows are the step's start and end (UTC) in the refresh record.

| command | what it measures | typical time |
|---|---|---|
| `bench.py` (all default scenarios) | every `bench.py` table on this page | about 32 min (`agent_panes` is 9 of them), and about 5 more since `upgrade` |
| `bench-micro.py flood many_panes clients status jobs` | *Micro-benchmarks* | about 12 min with `RUNS=3`; `memgrowth` 30 min more |
| `bench.py agent_panes` | *Agent panes* | about 11 min with `BASE` |
| `bench.py upgrade` | *Live upgrade* | about 5 min; 15 with `UPGRADE_STATES="fresh idle fresh-rows idle-rows"` |
| `bench.py trickle` | *A pane that never stops*; with `TRICKLE_PANES`, *Many panes that never stop* | about 22 min |
| `bench.py --smoke` | the regression guard | about 1 minute on an idle host, under 3 minutes anywhere |
| `bench-history-memory.sh spec big` | *Big scrollback*, one pane | 22:29-22:31 in the refresh record |
| `bench-history-memory.sh session` | *Big scrollback*, a session | 22:31-22:35 for two runs |
| `bench-history-memory.sh copymode spec big` | *Copy mode and search* | 22:35-22:45 |
| `bench-hidden-panes.py <binary> <label>` | *Hidden panes* | 22:45-22:47 for 4 x tmux and vtmux |
| `bench-history-read.py [--styled] <binary> <label> 100000 1000000` | *Reading all of a history*, *Capturing all of a history* | about 1.5 min for vtmux, 30 s for tmux |

- The exact command lines of the refresh are in [`test-health/2026-11-05-perf-refresh.md`](test-health/2026-11-05-perf-refresh.md).
- Other env: `RUNS`, `OUT`, `NO_BUILD=1`, `VTMUX`, `RENDER_FRAMES` (3000), `MANY_MB` (20), `AGENT_PANES`, `AGENT_CLIENTS`, `AGENT_SECS`, `VTMUX_REFERENCE_TMUX`, `HISTORY_SCENARIOS`. They are documented at the top of the script.
- Put the result in `docs/test-health/<date>-perf.md` and update the tables above.

## Regression guard

`python3 scripts/bench.py --smoke` (`scripts/bench_smoke.py`) is a short run that fails when vtmux got worse. It takes about 1 minute on an idle host, under 3 minutes anywhere.

- It is not part of `check.sh`.
- In CI it is the `perf-smoke` job in `ci.yml`: Linux only, and `continue-on-error`, so it never blocks a merge.
- It also runs weekly (`schedule:`, Mondays 04:17 UTC). It shares the zig/rust/ref-tmux setup with the `check` job through `.github/actions/setup`.
- It writes its table to `$GITHUB_STEP_SUMMARY` and uploads `smoke.json` as the `perf-smoke` artifact.

| metric | scenario | limit |
|---|---|---|
| `idle_rss_mb` | idle server, one session | baseline + max(25%, 8 MB) |
| `scale_rss_mb` | 400 idle panes | baseline + max(25%, 8 MB) |
| `history_spec_rss_mb` | `bench-history-memory.sh spec` | baseline + max(25%, 8 MB) |
| `cpu_ratio_plain`, `cpu_ratio_sgr` | throughput without a client (30 MB, median of 3): server CPU of vtmux / tmux 3.8 | baseline ratio x 1.5 |

- **RSS** is compared with `perf/baseline.json`, recorded on the CI runner. RSS depends on the allocator and libc, so numbers from a developer machine don't carry over.
- For one binary and one input RSS is nearly deterministic. The 8 MB floor keeps the small idle numbers from tripping on noise.
- **CPU** is never compared with a checked-in absolute time: runner speed varies between runs and SKUs.
- It is gated as the vtmux/tmux 3.8 CPU ratio measured in the same run, which cancels the machine's speed, against the ratio in the baseline. The coarse +50% threshold only catches a real slowdown, not drift.
- Not chosen: `valgrind --tool=cachegrind` instruction counts of a foreground server (tighter, +10%). It needs a valgrind install, runs the server on one traced thread rather than as deployed, and doesn't exist on macOS.
- **Local runs** work on any Linux host. `BASELINE=<file>` points at another baseline; without `perf/baseline.json` the run only records.
- A loaded host (load >= 4) makes the ratios noisy; the script warns.

### Refreshing the baseline

- Refresh after an intentional change that moves a metric: a feature that costs memory, a compression that saves it.
- Take `smoke.json` from the `perf-smoke` artifact of a CI run on the commit. Commit its `metrics` as `perf/baseline.json` (`{"metrics": {...}}`; the whole file works too).
- Put the reason in the commit message ("perf baseline: idle RSS +1.2 MB, the per-pane foo table").
- Never refresh to make a red run green without a reason. A regression is fixed or explained.
- The first baseline is whatever the first run on `main` produced.

### Staleness

- The numbers above and in the README come from a full run at the commit recorded in *Method*.
- When that commit is older than the last change to `perf/baseline.json` (a refresh means the metrics moved), the numbers are stale.
- The smoke run then prints a `STALE:` line, and the next full run replaces them.

### Checked with injected regressions

Scratch builds, not committed, run against a baseline recorded from the same host and the good build:

- A 50 MB `vec![0u8; N]` per pane. For the first three panes one byte is written in every 4 KiB page, so the pages are resident.
- A 20-million-iteration busy loop per output chunk.

| metric | baseline | regressed | result |
|---|---|---|---|
| `idle_rss_mb` | 7.4 MB | 57.5 MB | fail |
| `scale_rss_mb` | 13.4 MB | 163.4 MB | fail |
| `history_spec_rss_mb` | 105.5 MB | 205.4 MB | fail |
| `cpu_ratio_plain` | 0.32 | 109 | fail |
| `cpu_ratio_sgr` | 0.89 | 84 | fail |

### Merger re-run of `bench-history-read.py` (release build, this laptop, load 11-13)

Same script, tmux 3.8 and vtmux in turn, 3 runs, medians: `search-backward` at 100k lines 56-63 ms against tmux's 130-133 ms (0.45x), at 1M lines 464-563 ms against 1104-1155 ms (0.42-0.49x); `capture-pane -p -S -` 226 ms against 91-102 ms at 100k lines and 2083-2220 ms against 762-837 ms at 1M (2.7x slower); alternate screen on (`alt-on`) 20, 172 and 1773 ms at 2k, 100k and 1M lines against tmux's 39, 28 and 73 ms. These numbers replace the 4x and 4 s in the README's earlier text, which another load produced.
