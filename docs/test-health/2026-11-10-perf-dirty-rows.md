# Fewer renders, cheaper frames: key echo and agent panes (`perf_dirty_rows`)

Task `perf_dirty_rows`: a key's echo was 0.38 ms against tmux's 0.26, and agent TUIs cost 1.25-1.7x tmux's server CPU with two or more clients. Four commits on main `a3f8b8d`:

- `2bbcdbb` compose: a one-row change composes one row (the row below was composed again too);
- `e729423` vtmux-vt: a pane's screen is read once per write, shared by every client and view of the turn;
- `9979c25` render: the frame diff looks only between the first and last changed cell of a row;
- `acbd379` server: acknowledgement-only turns render nothing; style options parsed and pane-input options read once per options change; cheaper option maps; fewer syscalls per key in the server and the client.

Host: the shared Linux server (Linux 6.16, AMD EPYC Genoa, 96 cpus), load 5-32 over the runs from other workers; every bench run held `flock /tmp/vtmux-stress.lock`. `base` = main `a3f8b8d`, `vtmux` = `acbd379`, both release builds; tmux 3.8 from `cargo xtask ref-tmux`. Read the CPU numbers as rough; base and vtmux of one run are what to compare.

## Where the time went

`perf record -g -F 20000` of the base server (release build with frame pointers and line tables, Ghostty `ReleaseFast`) while a probe typed 1500 keys 5 ms apart into the `key_echo` layout (a raw echo pane next to an idle `cat`, one 200x50 client):

- A key took three loop turns: the key, the echo, the client's `FrameAck`. Every turn rendered every client, so two of the three renders drew nothing new, each one building the window's whole drawing first.
- Rendering was 50% of the server's samples: building the drawing (`Source::window`) 14%, of which re-parsing `window-style` and the border styles 6%; composing 12% (the echoed row and the row below it, whose signature held the row above's stamp); the frame 12%, half of it comparing the changed rows twice.
- Option lookups were 7% at 5 agent panes / 2 clients: `pane_input` read seven options per pane read, and the store hashed pane and window ids with SipHash.
- Syscalls per 100 keys (`strace -c` of the server): 300 `epoll_wait`, 400 `epoll_ctl`, 400 `recvfrom`, 200 `read`, 100 `write`, 100 `writev`. The `epoll_ctl`s re-armed sockets and ptys after every write and read with the interest they already had. The client: 400 `ioctl`s (switching its socket to blocking and back around every message) and a third wakeup per key for stdout.

## After

Server CPU per key from the same probe, 300 keys 10 ms apart, three runs each, alternating, load 25-37: base 116-148 µs (median 128), `acbd379` 98-112 µs (median 105). Syscalls per 100 keys: the server 1500 to 1100 (no `epoll_ctl`; one `read` and one `FIONREAD` instead of two reads), the client 1400 to 900 (no `ioctl`, two wakeups instead of three). Two of the three turns of a key now render.

### `bench.py key_echo agent_panes`

`flock /tmp/vtmux-stress.lock env RUNS=3 AGENT_PANES="5 20" AGENT_CLIENTS="1 2" NO_BUILD=1 BASE=<a3f8b8d> VTMUX=<acbd379> python3 scripts/bench.py key_echo agent_panes`, 2026-11-10 11:22 UTC, load 9.7 at the start, 19.7 at the end. JSON: [`2026-11-10-perf-dirty-rows.json`](2026-11-10-perf-dirty-rows.json).

| key echo, other pane | tmux 3.8 | base | vtmux | vtmux / tmux |
|---|---|---|---|---|
| idle, p50 / p90 | 0.24 / 0.29 ms | 0.34 / 0.40 ms | 0.30 / 0.37 ms | 1.25x |
| `yes`, p50 / p90 | 0.56 / 0.83 ms | 0.43 / 0.63 ms | 0.39 / 0.74 ms | 0.70x |

| agent panes / clients | tmux 3.8 | base | vtmux | base / tmux | vtmux / tmux |
|---|---|---|---|---|---|
| 5 / 1 | 1.7% | 1.6% | 1.4% | 0.94x | 0.82x |
| 5 / 2 | 2.2% | 2.7% | 1.8% | 1.23x | 0.82x |
| 20 / 1 | 4.9% | 4.6% | 3.7% | 0.94x | 0.76x |
| 20 / 2 | 5.0% | 4.9% | 4.6% | 0.98x | 0.92x |

Frames, terminal bytes and RSS are the same for base and vtmux in every row (vtmux 21.1 MB at 5 / 2, base 20.7).

The idle echo moves with the host's load more than with the build. Every `key_echo` run of `acbd379`, p50 with no flood:

| run (UTC, load, runs) | tmux 3.8 | base | vtmux |
|---|---|---|---|
| 10:18, 32, 3 | 0.26 | 0.37 | 0.33 |
| 10:33, 9.1, 5 | 0.25 | 0.35 | 0.32 |
| 10:43, 8.1, 5 | 0.22 | | 0.29 |
| 11:17, 5.1, 5 | 0.19 | | 0.25 |
| 11:22, 9.7, 3 (the table above) | 0.24 | 0.34 | 0.30 |

The 10:43 and 11:17 runs compared `acbd379` with the experiment below instead of base. Median of the five: 0.30 ms, against base's 0.35 in the three runs that had it.

What is left is mostly not the server's: tmux's server reads the terminal and writes to it itself, while vtmux's attached client relays every key and frame (about 25 µs of client CPU per key, two more process wakeups on the path). The server's own work per key is now one render of one row.

### `bench-micro.py clients`

`flock /tmp/vtmux-stress.lock env RUNS=3 CLIENTS_N="1 2 5" BASE=<a3f8b8d> VTMUX=<acbd379> python3 scripts/bench-micro.py clients`, 11:31 UTC, load 19.5 at the start, 10.5 at the end. N 120x40 clients (sync on) on one session of 4 `agent_tui.py` panes; server CPU in seconds per 10 s, median of 3.

| clients | tmux 3.8 | base | vtmux | base / tmux | vtmux / tmux |
|---|---|---|---|---|---|
| 1 | 0.189 | 0.262 | 0.161 | 1.39x | 0.85x |
| 2 | 0.135 | 0.195 | 0.154 | 1.44x | 1.14x |
| 5 | 0.144 | 0.267 | 0.200 | 1.85x | 1.39x |

tmux's 1-client number is above its 2-client one: the load was high. An earlier run at load 15.7 (11:09 UTC, the build with the experiment below, which changes nothing here) gave 1.08x, 1.06x and 1.24x against base's 1.35x, 1.36x and 1.75x. Server wakeups went up at 5 clients (base 139/s, vtmux 268/s): acknowledgements no longer wait out a render, so they no longer pile into one turn.

## Tried and dropped

- **A key that goes to the pane's program renders nothing.** tmux redraws nothing for such a key, so the key's turn was made quiet too (one render per key instead of two). Its CPU went down, but its echo got slower: in the 10:43 and 11:17 runs above, `acbd379` 0.29 and 0.25 ms against 0.31 and 0.31 with the change (and 0.31 / 0.33 against base in a third run). The server went back to sleep right after writing the key and woke up later for the echo than when it was still busy. Not committed.
- **Skipping the backlog turn after every pane read.** A pane read with nothing more waiting (`FIONREAD` 0) left the backlog, so a key's echo needed no turn of its own to read `WouldBlock`. A `yes` flood also drains the pty for a moment now and then, and each time it left the backlog its frames lost their pacing: 3-4x the client CPU and an echo next to it at 0.55-0.61 ms instead of 0.42. Kept only for reads under 1 KB by a pane that was not flooding.
- **The first acknowledgement-only turn** looked only at the turn's own messages. Regress `prompt-keys.sh` and oracle `window_view_follows_cursor` failed: a redraw asked for after a render waited for the next event. The committed version also counts pane output, commands and dropped clients since the last render, and any redraw asked for.

## Gate

`GHOSTTY_SOURCE_TARBALL=<pinned tarball> muzzle run --class high --rule vtmux-check -- mise exec -- bash scripts/check.sh` at `acbd379`: exit 0 in 238 s (suites at once, 96 cpus): cargo test 1557 passed, 0 failed, 6 ignored; regress 171/174 pass, 1 xfail, 2 marked flaky that passed (`clock-mode.sh`, `screen-redraw-menus.sh`); oracle replay 415/415; with an upgrade every third step 415/415; every step 9/9. The macOS type-check was skipped: `aarch64-apple-darwin` is not installed on this host.

Stress, five rounds under 48 busy loops (`flock /tmp/vtmux-stress.lock`, load 65-70): `vtmux-vt` lib 161/161, `vtmux` attach 145/145, oracle `window_view_follows_cursor` and regress `prompt-keys.sh` passed every round; `vtmux-server` lib failed once each in three rounds, on `timers_count_from_when_the_snapshot_was_taken` (twice) and `a_client_that_comes_and_goes_first_leaves_the_starter_a_server`. Both fail on base too: eight full lib runs each under the same loops, base 4 of 8, `acbd379` 1 of 8; alone, 0 of 10 for both. Once with dash 0.5.11 bind-mounted over `/usr/bin/sh` in a user and mount namespace: `vtmux-server` lib 375/375, attach 145/145, cli 154/155 (`current_command_is_read_as_bytes`: the namespace replaces `bash` too, as in [`2026-11-08-test-suite-speed.md`](2026-11-08-test-suite-speed.md)), control 30/30, persist 27/27, upgrade 38/38, `window_view_follows_cursor` passed.
