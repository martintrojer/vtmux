# Benchmark records

Every number in [`docs/PERFORMANCE.md`](../PERFORMANCE.md) and the README comes from a dated run in this directory: the command, the host, its load and the raw output, with JSON next to the Markdown where the bench writes it. Each record also keeps the before/after of the change it measured.

This page is how the runs are made: the method, the scenarios, how to run them, and the regression guard.

## Method

### Where the numbers come from

- Speed and memory are claimed only where a recorded run measures them (D24 in the [spec](../specs/2026-10-29-vtmux.md)). Every number in PERFORMANCE.md comes from a run in this directory.
- Most rows come from the runs of 2026-11-05 in [`2026-11-05-perf-refresh.md`](2026-11-05-perf-refresh.md): commands, load and raw output, with the JSON next to it.
- That run measured vtmux main `ada47ba` (release build) against tmux 3.8 from `cargo xtask ref-tmux`. `bench.py` rows are the median of 3 runs.
- Later runs, each in its own record: hidden panes after `8a7d31f` (in the refresh record, *Agent panes fixed*), many streaming panes (2026-11-06), live upgrade (2026-11-07), the hot paths and the 1M-line search (2026-11-08), capture, styled copy mode, many clients and the release profile (2026-11-09); the terminal table's size (2026-11-10).
- The 2026-11-09 release-profile run is the latest for agent panes, many clients, key echo and throughput; rows before it were built with cargo's default release profile, which is 7-18% slower on render-bound work.
- The injected-regression check of the guard (below) is a separate scratch run.

### Host and load

- A a shared Linux server, : Linux 6.16, AMD EPYC Genoa, 96 cpus, shared with other agents.
- Load was 2.6 to 4.9 (`uptime`) over the refresh runs, every one under `flock /tmp/vtmux-stress.lock`. Later runs saw more (load 3 to 22; the release-profile run 9 to 15, under `muzzle run --class high`); each record says.
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
| `bench.py` (all default scenarios) | every `bench.py` table | about 32 min (`agent_panes` is 9 of them), and about 5 more since `upgrade` |
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

- The exact command lines of the refresh are in [`2026-11-05-perf-refresh.md`](2026-11-05-perf-refresh.md).
- Other env: `RUNS`, `OUT`, `NO_BUILD=1`, `VTMUX`, `RENDER_FRAMES` (3000), `MANY_MB` (20), `AGENT_PANES`, `AGENT_CLIENTS`, `AGENT_SECS`, `VTMUX_REFERENCE_TMUX`, `HISTORY_SCENARIOS`. They are documented at the top of the script.
- Put the result in `docs/test-health/<date>-perf.md` and update PERFORMANCE.md's tables.


## Regression guard: baseline, staleness, injected regressions

The guard itself (what it runs, its limits) is in PERFORMANCE.md, *Regression guard*.

### Refreshing the baseline

- Refresh after an intentional change that moves a metric: a feature that costs memory, a compression that saves it.
- Take `smoke.json` from the `perf-smoke` artifact of a CI run on the commit. Commit its `metrics` as `perf/baseline.json` (`{"metrics": {...}}`; the whole file works too).
- Put the reason in the commit message ("perf baseline: idle RSS +1.2 MB, the per-pane foo table").
- Never refresh to make a red run green without a reason. A regression is fixed or explained.
- The first baseline is whatever the first run on `main` produced.

### Staleness

- PERFORMANCE.md's numbers, and the README's, come from a full run at the commit it records (`commit \`<sha>\`` under *How it's measured*; `bench_smoke.py` reads the first such mention).
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

## Profiles

These come from `perf record -g` of a `release` build with frame pointers and line tables, with Ghostty built `ReleaseFast` (`LIBGHOSTTY_VT_SYS_OPTIMIZE`; a debug-info build otherwise gets Ghostty's `Debug` mode and profiles its integrity checks):

- **16 clients, before `perf_many_clients`.** Rendering was 81% of the server: building each pane's drawing inputs (`Source::window`, 18%: option lookups, styles, the pane's screen), composing rows (18%), the frame diff (15%), the client list described above (6%) and copying cells (10%), all per client.
- **32 clients, after it.** Rendering is 64%. Writing the frames to the clients' sockets is 19% of it, the window drawing 13% (mostly the first client's: `Source::window` 4%, composing 3.4%), each client's cursor 2.2% and mouse modes 4.1%. Reading the clients' acknowledgements is another 10%.
- **100 agent panes, one client.** Reading Ghostty's dirty rows at the end of each synchronized update is 33%, mostly for the 98 panes no client shows. Then parsing pane output, 20%. A hidden pane's view is read at each `?2026h` so that switching to it mid-update shows the held screen (*Hidden panes*). The same rows would be read at the next one anyway.
- **Creating 400 panes.** The server's own code is 30%. The rest is the kernel: `fork` copying page tables (`copy_page_range`) and the copy-on-write faults after it. A bigger server costs more per `fork`, and tmux forks too (`fdforkpty`). Spawning with `vfork` or `posix_spawn` would avoid it, but the child's setup (controlling terminal, signals, directory fallbacks) has to move with it.
- **A search over 100k lines, before the fix.** 78% was comparing cells (`matches_at`), and 31% of all samples went to reading rows as cells.

Raw runs: [`2026-11-08-perf-hotpaths.md`](2026-11-08-perf-hotpaths.md); *Many clients* and the 32-client profile: [`2026-11-09-perf-many-clients.md`](2026-11-09-perf-many-clients.md).

## Another host

A re-run of `bench-history-read.py` on a laptop (release build, load 11-13), 2026-11-09, before `perf_capture_full_history`: its `capture-pane` numbers are superseded by *Capturing all of a history* in [`2026-11-09-capture-full-history.md`](2026-11-09-capture-full-history.md); the search and alternate-screen numbers stand for that host.

Same script, tmux 3.8 and vtmux in turn, 3 runs, medians: `search-backward` at 100k lines 56-63 ms against tmux's 130-133 ms (0.45x), at 1M lines 464-563 ms against 1104-1155 ms (0.42-0.49x); `capture-pane -p -S -` 226 ms against 91-102 ms at 100k lines and 2083-2220 ms against 762-837 ms at 1M (2.7x slower); alternate screen on (`alt-on`) 20, 172 and 1773 ms at 2k, 100k and 1M lines against tmux's 39, 28 and 73 ms.
