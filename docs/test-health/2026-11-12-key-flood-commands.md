# Commands under a key flood, perf_key_flood_commands, 2026-11-12

The prove_resurrect record saw a debug server, flooded with key input by an attached client, answer command clients in 4-6 s, against 25 ms for tmux 3.8. Release builds show the same: 1.8 s. Commands now answer in 13 ms, under tmux's 24 ms, and the server takes 0.99 M keys/s (tmux 1.25 M), from 0.32 M.

Release builds on main 073e7108: vtmux = this branch, base = 073e7108, tmux = 3.8 (`cargo xtask ref-tmux`). Linux 6.16, AMD EPYC Genoa (96 CPUs), shared with other workers (load 13-38; 75 for key echo). Raw JSON: [`2026-11-12-key-flood-commands.json`](2026-11-12-key-flood-commands.json).

## The bench

New scenario `key_flood` in `scripts/bench.py`: one 200x50 client (sync on) on a pane running `cat`, and a thread typing printable keys into the client's terminal as fast as the client reads them. Meanwhile `display -p x` runs 50 times in a row, each timed from spawn to exit. It runs once with no flood too. Command: `OUT=… RUNS=3 NO_BUILD=1 python3 scripts/bench.py key_flood`.

| case | binary | p50 ms | p90 ms | max ms | keys/s typed | server CPU % |
|---|---|---|---|---|---|---|
| flood none | tmux | 2.3 | 2.8 | 3.1 | | 10 |
| flood none | vtmux | 2.0 | 2.3 | 2.7 | | 10 |
| flood keys | tmux | 23.9 | 26.5 | 27.9 | 1.25 M | 99 |
| flood keys | base (one run, 20 commands, load 16) | 1807 | 1947 | 2205 | 0.32 M | 99 |
| flood keys | vtmux | 13.4 | 16.0 | 20.9 | 0.99 M | 99 |

Load 38 for tmux and vtmux; an earlier run at load 12 had 24.4 against 14.4 ms. Keys/s is what the client's terminal took while the commands ran (the first runs counted the second before too and overstated it; this one does not).

A debug build, the same flood, 10 commands: 90-200 ms each (`/tmp` probe, not in the tree), against the 4-6 s of the earlier record.

## Why commands starved

`strace -c` of the base server for 3 s of flood showed one `epoll_wait` and 116k `write`s: one loop turn longer than 3 s, and one write to the pane per key.

1. **One turn took the whole flood.** `client_readable` read the client's socket until `WouldBlock`, then handled every key it got. The client keeps refilling the socket (hundreds of KB on Linux), so a turn handled 200-350 KB of keys. A command client needs two or three turns (accept, its Hello, the answer), so it waited for several of them. tmux reads a client's terminal once per libevent callback (tty.c `tty_read_callback`, `evbuffer_read` of at most 4 KB) and runs every other handler between reads.
2. **A write per key.** `write_pane` wrote each key to the pane's PTY at once (`PaneIo::write`), about 3 µs of kernel PTY work per key: 300k keys/s at most. tmux queues keys with `bufferevent_write` (window.c `window_pane_key`, input-keys.c:418) and writes them when the loop gets to it.

## The fix

- A client that has said Hello is read at most `CLIENT_READ_BUDGET` (4 KB plus a frame header, one `Input` frame) per turn. One with more waiting goes in `client_backlog`, is read again next turn after that turn's events, and makes the poll timeout zero. mio is edge-triggered and would not report the socket again, as `backlog` already handles for panes. The Hello itself is read whole: it carries the environment, about 30 KB here, and a budget made a command client wait seven turns for it (40 ms). A re-attach after a live upgrade still reads the old connection whole before it goes.
- `write_pane` and synchronized panes queue keys (`PaneIo::queue`) and `flush_pane_writes` writes them after each client's input and before the loop waits: one `write` per 4 KB of keys. A direct write meanwhile (`paste-buffer`, a query reply) writes what is queued first, so the order holds.

After both, a profile of the flooded server is all key handling (options lookups, the client's command queue, `Instant::now`, malloc), about 1 µs a key, against tmux's 0.8 µs. A turn is 4-5 ms, and a command spans two or three. That per-key cost (string-keyed option lookups for `prefix`, `prefix2`, `prefix-timeout` and the key table, a command-queue batch per key, `Instant::now`) is where a paste could still get faster; commands no longer wait on it.

## Perf gate, before and after (base vs vtmux, side by side)

| | base | vtmux |
|---|---|---|
| agent_panes 10 panes, 1 / 2 clients, server CPU % (RUNS 2) | 2.3 / 3.3 | 2.6 / 3.2 |
| agent_panes 20 panes, 1 / 2 clients, server CPU % (RUNS 2) | 4.9 / 6.0 | 4.2 / 6.2 |
| throughput 100 MB, plain / SGR, server CPU s (RUNS 3) | 1.13 / 1.70 | 0.97 / 1.66 |
| key echo p50, idle / next to `yes`, ms (RUNS 2, load 75) | 0.33 / 0.40 | 0.32 / 0.40 |

All are within noise. tmux in the same runs: agent_panes 3.1 / 3.2 and 5.4 / 6.3 %, throughput 2.48 / 3.09 s, key echo 0.24 / 0.49 ms.

## Tests

- `attach::a_key_flood_leaves_commands_answered_between_its_reads` (end to end, vtmux only): an attached client floods keys into `cat >file` while a control client sends `display -p` 11 times. It asserts that the median of the bytes reaching the pane during one round trip is under 64 KB. Against the base release binary: 168-358 KB, median 176 KB, fails. After: 1.5-8 KB, median 4 KB. It is vtmux only because tmux handles keys several times faster, so its keys per round trip say nothing of its latency.
- `server::tests::a_flooding_client_is_read_a_budget_a_turn`: a socket holding more than two budgets takes more than `sent / budget` reads. Each read that stops short leaves the client in the backlog with a zero poll timeout.
- Stress: both, 5x under 48 busy loops (`flock /tmp/vtmux-stress.lock`), 5/5 each. The end-to-end test once more with a dash-like `/bin/sh` (a POSIX-mode bash that forks its last command, bind-mounted over `/usr/bin/bash` in a user and mount namespace): passed.
