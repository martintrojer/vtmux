# Performance

vtmux against tmux 3.8: release builds, same host, same scripts. A ratio is vtmux / tmux, so under 1 means vtmux uses less. Where vtmux loses, the table says **Worse**.

The trade is Ghostty's: speed over a few MB. vtmux holds more memory when idle and per pane, far less once scrollback grows, and spends less CPU on output. Every number comes from a dated run, with its command, host, load and raw output, in [`docs/test-health/`](test-health/), which also has the method.

## At a glance

| | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| 1M lines of scrollback, settled | 622 MB | 103 MB | 0.17x |
| 10 panes x 100k lines, settled | 629 MB | 100 MB | 0.16x |
| 20 panes streaming at a 100k limit | 1.25 GB | 217 MB | 0.17x |
| Idle server | 4.2 MB | 9.0 MB | 2.1x **Worse** |
| Each idle pane | 5.4 KB | 18 KB | 3.4x **Worse** |
| Pane output, no client | | | 0.37-0.54x CPU |
| Pane output, one client attached | | | 0.42-0.62x CPU |
| 8 clients, a 20 MB flood | | | 0.06-0.19x CPU |
| Redraw-heavy output, bytes to the terminal | 10.1 MB | 0.07 MB | about 140x fewer, at 0.3x CPU |
| Agent TUIs, 5-20 panes | | | 0.85-1.25x CPU **Worse** at 2 clients |
| 100 agent panes | | | 0.70x CPU |
| 32 clients on one window | | | 1.6x CPU **Worse** |
| Idle server with the owner's config | 171 ms CPU a minute | 47 ms | 0.28x |
| Key echo, p50 | 0.26 ms | 0.38 ms | 1.5x **Worse** |
| `new -d` | 12.2 ms | 8.9 ms | 0.73x |
| Creating 400 panes | 200 ms CPU | 380 ms | 1.9x **Worse** |
| Search a 1M-line history | 1.65 s | 0.55 s | 0.34x |
| `capture-pane -S -` of 1M lines | 840 ms | 385 ms | 0.46x |
| Alternate screen up over 1M lines of history | 3-73 ms | 1.8-4.0 s | **Worse** |
| Live upgrade, 10 panes x 100k lines | kill-server | 0.6-0.9 s | |
| Release binary, stripped | 1.3 MB | 10.0 MB | 7.7x **Worse** |

## Memory

Each pane is a libghostty terminal with its own render state, so a small server costs a few MB more. Scrollback is where that turns around: vtmux compresses history once it goes quiet, or in small steps while a pane keeps streaming, and tmux never gives memory back.

| | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| Idle server | 4.2 MB | 9.0 MB | 2.1x |
| 400 idle panes | 6.4 MB | 16.0 MB | 2.5x |
| A hidden full pane (101 windows) | 48 KB | 74-80 KB | 1.6x |
| 100k lines, right after the output | 66 MB | 107 MB | 1.6x |
| 100k lines, settled (16 s later) | 66 MB | 19 MB | 0.29x |
| 1M lines, right after the output | 622 MB | 981 MB | 1.6x |
| 1M lines, settled (72 s later) | 622 MB | 103 MB | 0.17x |
| After a copy-mode search of 1M lines, mode left | 1.2 GB | 102 MB | 0.09x |

Fresh history costs up to 1.6x tmux's until the pass finishes: about 15 s after 100k lines, 70 s after 1M. Panes that never stop streaming shrink too (1000 lines/s each, at `history-limit 100000`):

| streaming panes | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| 1 | 66 MB | 19 MB | 0.29x |
| 5 | 315 MB | 59 MB | 0.19x |
| 20 | 1.25 GB | 217 MB | 0.17x |

vtmux also spent less server CPU on those runs (66.9 s against 76.2 s over 12 minutes at 20 panes). Ten agent panes over 30 minutes: neither server grows (vtmux 21.9 to 22.1 MB, tmux 6.5 MB).

## CPU and rendering

vtmux parses output faster than tmux and draws whole frames: one per client, only the cells that changed, inside synchronized output. Under a flood it sends a fraction of tmux's bytes. Latest run (release profile, load 9-15), server CPU:

| 100 MB of log text | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| no client, plain / 19% SGR | 2.47 / 3.15 s | 1.05 / 1.71 s | 0.43x / 0.54x |
| one client, plain, sync off / on | 2.77 / 2.73 s | 1.41 / 1.57 s | 0.51x / 0.58x |
| one client, SGR, sync off / on | 4.04 / 4.04 s | 1.95 / 1.93 s | 0.48x |
| bytes to the terminal per byte of output, plain | 1.03 | 0.06 | 0.05x |

| other floods | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| `render`: 4 panes of cursor-addressed redraws, bytes sent | 10.1 MB | 0.07 MB | 0.007x |
| `render`, server CPU | 0.65 s | 0.19 s | 0.29x |
| 8 panes each `cat` 20 MB at once, server CPU | 5.34 s | 1.05 s | 0.20x |
| 8 clients, one 20 MB flood, sync off / on | 1.22 / 4.68 s | 0.23 / 0.29 s | 0.19x / 0.06x |

`render` and the 8 clients are from the 2026-11-05 run, which also put the no-client rows at 0.37x and 0.46x at a lower load. The cost is memory with clients attached: 2.2x to 3.4x tmux's RSS (24.5 MB against 7.4 MB with 8 clients).

## Agent panes and many clients

Agent TUIs (`scripts/agent_tui.py`, a synthetic pi-like renderer: a spinner, streamed words, tool output, all in synchronized updates) cost about what tmux spends. Server CPU, release profile:

| | tmux 3.8 | vtmux | vtmux / tmux | RSS (tmux) |
|---|---|---|---|---|
| 5 panes, 1 client | 1.9% | 1.9% | 1.00x | 18.4 MB (6.1) |
| 5 panes, 2 clients | 2.0% | 2.5% | 1.25x | 20.9 MB (6.2) |
| 20 panes, 1 client | 5.3% | 4.5% | 0.85x | 31.8 MB (8.0) |
| 20 panes, 2 clients | 5.4% | 5.5% | 1.02x | 34.5 MB (8.1) |
| 50 panes, 1 client | 15.3% | 11.1% | 0.73x | 56 MB (11.5) |
| 100 panes, 1 client | 32.1% | 22.5% | 0.70x | 100 MB (17.8) |
| 16 clients on one window of 4 panes | 3.0% | 5.3% | 1.7x | 25.5 MB (7.1) |
| 32 clients on one window of 4 panes | 5.2% | 8.3% | 1.6x | 42.8 MB (9.2) |

tmux's cost is parsing, so it grows with panes; vtmux parses cheaper and pays per frame, so it wins on many panes and loses on many clients. Clients that show a window alike share its drawing and frame bytes, which took 32 clients from 5.2x tmux to 1.6x. Switching to a hidden full window (`C-b n` to its last byte) takes 2.5 ms against tmux's 5.1 ms.

## Latency

| | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| key echo p50 / p90, other pane idle | 0.26 / 0.30 ms | 0.38 / 0.43 ms | 1.5x |
| key echo p50 / p90, next to a `yes` flood | 0.55 / 0.97 ms | 0.45 / 0.71 ms | 0.82x |
| `new -d` | 12.2 ms | 8.9 ms | 0.73x |
| start with the owner's config until it answers | 102 ms | 101 ms | 0.99x |

vtmux is 0.1 ms behind with no flood: each frame does work proportional to the screen (the window's drawing context, the row diff). Next to a flood it is ahead, because flood output is paced and a key's echo is not.

## Big histories

Reading a 1M-line history is faster than tmux; bringing up the alternate screen over one is not. Release build, one 120x40 pane of log lines:

| 1M lines | tmux 3.8 | vtmux | vtmux / tmux |
|---|---|---|---|
| copy-mode search, plain / coloured lines | 1.65 / 1.79 s | 0.55 / 0.65 s | 0.34x / 0.36x |
| `capture-pane -p -S -` | 840 ms | 385 ms | 0.46x |
| `capture-pane -p -e -S -` | 4.6 s | 0.93 s | 0.20x |
| enter copy mode, history compressed | 220 ms | 57 ms | 0.26x |
| enter copy mode, fresh history | 236 ms | 281 ms | 1.19x **Worse** |
| alternate screen up (vim, less) | 3 ms | 4.0 s | **Worse** |

The alternate screen copies the whole primary history, because Ghostty reads only the active screen and tmux keeps showing that history. On another host (a laptop) it took 1.8 s at 1M lines and 172 ms at 100k; at the default `history-limit` of 2000 it is 10-20 ms.

## Live upgrade

`vtmux-upgrade -b` onto the same binary, one attached and two control clients, from the request to the client's redraw. tmux has no live upgrade: its answer is `kill-server`.

| panes x lines of history | fresh | compressed | snapshot |
|---|---|---|---|
| 1 x 10k | 41 ms | 33 ms | 1.3 MB |
| 1 x 100k | 163 ms | 82 ms | 11.5 MB |
| 10 x 100k | 751-945 ms | 598-806 ms | 113 MB |
| 10 x 100k, onto a build of another Ghostty pin | 9.9 s | 10.4 s | 57 MB |

10 x 100k meets the one-second target. Onto a build with another Ghostty pin, history travels as rows and is decoded twice, about a second per 100k lines; that is the upgrade after a pin bump, not the daily one. RSS after the upgrade is what the compressor would have left (146 MB at 10 x 100k, from 1.57 GB fresh).

## Binary and build profile

The release profile is fat LTO in one codegen unit, stripped. Against cargo's default:

| | default | vtmux's profile |
|---|---|---|
| stripped binary (tmux 3.8: 1.3 MB) | 10.7 MB | 9.8 MB |
| gzipped, in the release tarball | 4.41 MB | 4.28 MB |
| server CPU, agent panes and many clients | | 6-18% less |
| key echo p50 | 0.43 ms | 0.38 ms |
| output parsing (Ghostty's, untouched by LTO) | | flat |
| build after touching `vtmux-core` | 108 s | 118 s |

`cargo install vtmux` builds the "default" column: a published crate does not carry the workspace's profile. The environment line in [RELEASING.md](RELEASING.md#what-cargo-install-vtmux-gets) builds this profile.

Ghostty is linked in statically (1.35 MB of text; tmux links libevent and ncurses). `panic = "abort"` would save 0.67 MB, but a live upgrade catches a panic to switch back to the old binary, so panics unwind.

## How it's measured

- Host: a shared Linux server (Linux 6.16, AMD EPYC Genoa, 96 CPUs). macOS numbers are not collected yet.
- Only the server process: CPU from `/proc/<pid>/stat` or `schedstat`, RSS from `VmRSS`, never the pane children. Medians of 3 or 5 runs.
- Clients are the binary's own `attach` on a PTY the bench drains, counting every byte and synchronized frame.
- Most memory, startup and owner-config rows: commit `ada47ba`, 2026-11-05, cargo's default release profile. Agent panes, many clients, key echo and output CPU: the 2026-11-09 release-profile run. Others: their own dated runs.
- The host is shared and the load varied (3 to 22), so read CPU rows as rough; each run compares binaries side by side.

## Running the benchmarks

```sh
mise exec -- python3 scripts/bench.py                 # every default scenario, about 37 min
mise exec -- python3 scripts/bench.py idle scale      # chosen scenarios
BASE=/path/to/older/vtmux mise exec -- python3 scripts/bench.py agent_panes   # against an earlier build
mise exec -- python3 scripts/bench-micro.py flood many_panes clients status jobs
mise exec -- python3 scripts/bench-history-read.py [--styled] <binary> <label> 100000 1000000
```

Every scenario, environment variable and typical time is in the method under `docs/test-health/`. Put a new run there as `<date>-<name>.md` and update this page.

## Regression guard

`python3 scripts/bench.py --smoke` fails when vtmux got worse: about a minute on an idle host. In CI it is the `perf-smoke` job (Linux, on every push and weekly, `continue-on-error`, so it never blocks a merge).

| metric | limit |
|---|---|
| RSS: idle server, 400 idle panes, `bench-history-memory.sh spec` | `perf/baseline.json` + max(25%, 8 MB) |
| server CPU vtmux / tmux 3.8, 30 MB of output, plain and SGR | the baseline's ratio x 1.5 |

RSS is compared with a baseline recorded on the CI runner; CPU as a ratio measured in the same run, which cancels the machine's speed. Refresh the baseline only for an intentional change, with the reason in the commit message.
