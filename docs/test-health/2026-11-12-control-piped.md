# Piped control-mode commands (2026-11-12)

A script that pipes commands into `vtmux -C` ran 2.3x slower than tmux 3.8. vtmux now takes 0.57-0.66x tmux's time on the same input, and a command client answers in 0.35x tmux's time while the script runs.

## Setup

- Host: a shared Linux server (Linux 6.16, AMD EPYC Genoa, 96 CPUs). Load was 4.5 to 10 over the final runs, each bench under `flock /tmp/vtmux-stress.lock`.
- `tmux` is tmux 3.8 from `cargo xtask ref-tmux`. `base` is main `cd77ecc2`, which already reads each client 4 KB a turn and batches pane writes (perf_key_flood_commands, [`2026-11-12-key-flood-commands.md`](2026-11-12-key-flood-commands.md)). `vtmux` is this branch on top of it. All are release builds.
- The bench's environment had 178 variables. The size of a client's environment changes what `base` pays per command, so it matters for the `base` rows.

## Where the time went

`perf record -g` (a release build with frame pointers) of main `d7a1d4e0`'s server and `strace -c` of both processes, on 20,000 lines of `display -p '#{session_name} #{window_index} #{pane_id}'`: 0.46 s wall, 0.45 s server CPU; tmux 0.28 s. Main `cd77ecc2` costs the same (484 ms below).

| cost | share of base server CPU | fix |
|---|---|---|
| Each command's item copied the client's whole environment (360 strings) | about 50%: 80% of all mallocs, `malloc_consolidate` on every command | the item shares the client's environment (`Option<Rc<[_]>>`) |
| A writev per command, and on the client 2 recvfrom and a write per frame | 11% of the server, plus the client's 0.10 s | the output of the lines one read brings goes in as few `Stdout` frames as fit, once they have run |
| Each line taken off the input buffer moved the rest of it down | 2% | taken once per read |
| Format expansion, the command parser, the message log (`cmdq_add_message`), `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` for the format time limit | the rest | unchanged: tmux does the same work |

A client's socket was also read until it was empty, so a command client waited behind every line piped in: 1.0 s on main `d7a1d4e0`, against tmux's 19 ms. Main now reads each client 4 KB a turn. This branch makes a control client send its stdin in 4 KB reads, as libevent reads tmux's, so each turn holds whole `Stdin` frames: one turn's worth of lines, then the other clients.

The first version of this change gave each command a `Rc<[_]>` environment. `Rc::default` allocates, and a key flood makes an item per key, so a fixed 16 MB key flood cost 7% more server CPU than main (an ad-hoc probe, not kept: an attached client types 16 MB into a pane running `wc -c`; server CPU until it is idle; 19.1 s against 17.9 s). An item without a client now holds `None`. The same probe then gives 18.4-18.6 s against 17.8-18.4 s, and `key_flood` below is flat.

## Results

`RUNS=5 BASE=<main cd77ecc2> python3 scripts/bench.py control_piped` (raw output in [`2026-11-12-control-piped.json`](2026-11-12-control-piped.json)), load 5.2: 20,000 lines piped into `-C attach` at once, until the client exits. The last column is the median round trip of `display -p ok` from a command client while another control client is piped the same lines over and over.

| input | binary | wall ms | server CPU ms | client CPU ms | `display -p ok` during the flood, ms |
|---|---|---|---|---|---|
| `display -p ok` | tmux | 180 | 177 | 0 | 18.7 |
| `display -p ok` | base | 415 | 410 | 130 | 15.3 |
| `display -p ok` | vtmux | 118 | 115 | 0 | 6.5 |
| `display -p` with 3 formats | tmux | 282 | 278 | 0 | 9.2 |
| `display -p` with 3 formats | base | 484 | 479 | 160 | 4.5 |
| `display -p` with 3 formats | vtmux | 161 | 158 | 0 | 3.2 |

vtmux / tmux: 0.66x on plain lines, 0.57x with formats. A command during the flood: 0.35x tmux's time. The same bench against main `d7a1d4e0`, before the read budget, gave 1000 ms and 282 ms for the last column.

## Perf gate (before / after)

Same host and builds, `BASE=<main cd77ecc2>`, median of 3 (key_flood: 5):

| scenario | tmux | base | vtmux |
|---|---|---|---|
| key_flood: `display -p x` p50 / p90 ms while a client types keys | 23.9 / 26.7 | 13.5 / 15.5 | 13.5 / 15.3 |
| key_flood: keys/s taken | 1,215,839 | 984,837 | 977,835 |
| key_flood: `display -p x` p50 ms, no flood | 1.9 | 1.6 | 1.6 |
| throughput, plain: wall / server CPU s | 2.45 / 2.41 | 1.02 / 0.85 | 1.02 / 0.79 |
| throughput, SGR: wall / server CPU s | 3.26 / 3.10 | 1.22 / 1.10 | 1.22 / 1.13 |
| key echo p50 / p90 ms, no flood | 0.20 / 0.27 | 0.26 / 0.33 | 0.26 / 0.34 |
| key echo p50 / p90 ms, next to `yes` | 0.88 / 1.41 | 0.36 / 0.50 | 0.31 / 0.44 |
| agent panes, 5 panes / 1 client, server CPU % | 1.4 | 1.2 | 1.1 |
| agent panes, 20 panes / 1 client | 4.7 | 3.0 | 3.2 |

Every row is flat within the host's noise (load 4.5 to 10). The 20-pane agent row moves between runs: an earlier run of this branch gave 2.8 against main's 3.3.
