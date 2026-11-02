# Perf run 2026-11-02 (perf_owner)

Command: `flock /tmp/vtmux-stress.lock mise exec -- python3 scripts/bench.py owner_idle owner_startup` (RUNS=3, IDLE_SECS=60, OWNER_STARTUP_N=20), release build, started when load fell below 4 (3.85).
Host: Fedora 44, Linux 7.2.11 x86_64, AMD Ryzen 7 5700X3D (16 cpus). Commit 7d48b29 (main e96bcea plus the scenarios).
Load: 3.9 at start of the run, 4.97 (1 min average) at the end; other agents' gates share the host, so treat the CPU rows as +-20%.
Poller check after the run: `ps -eo pid,etime,args | grep mu-crew-poller` shows one process, pid 1383738, elapsed 1 d 19 h, `HOME=/home/martintrojer` in its environment: the owner's own poller, not the bench's. No process carries a `HOME=/tmp/vtmux-bench-*` environment and no `/tmp/vtmux-bench-*` directory is left; the bench's own count of leftover processes is 0 in both scenarios (below).
The poller CPU column is the poller process plus the `tmux` clients it ran: outside the server.

2026-11-02 09:57 UTC, commit 7d48b29, Linux-7.2.11-200.fc44.x86_64-x86_64-with-glibc2.43, AMD Ryzen 7 5700X3D 8-Core Processor (16 cpus), load 3.9, median of 3; tmux: tmux 3.8; vtmux: vtmux 0.0.0 (tmux 3.8)

### owner_idle (owner config, 1 session, 60 s)

| binary | RSS MB | server CPU ms | server wakeups/s | poller CPU ms (outside the server) |
|---|---|---|---|---|
| tmux | 4.6 | 187.29 | 4.12 | 180 |
| vtmux | 8.0 | 48.06 | 1.05 | 220 |

The poller column is the mu-crew-poller process itself plus the `tmux` clients it ran, not the server.

Processes still carrying the hermetic HOME 5 s after kill-server (the bench SIGKILLed them), summed over all runs: 0

Config snapshot: tmux.conf 6430744+dirty sha256:c1ffe189d326; mu-crew-dotfiles d633e15; plugin tmux-fingers-rs 5759d08; plugin tmux-fzf 05af76d; plugin tmux-session-picker 1158ce2; plugin tmux-yank acfd36e; plugin tpm 7bdb7ca; plugin vim-tmux-navigator e41c431

### owner_startup (start-server with the owner config until `display -p ok` answers, 20x)

| binary | median ms | min ms | max ms | runs |
|---|---|---|---|---|
| tmux | 62.9 | 58.9 | 74.1 | 20 |
| vtmux | 61.8 | 58.3 | 80.5 | 20 |

Processes still carrying the hermetic HOME 5 s after kill-server (the bench SIGKILLed them), summed over all runs: 0

Config snapshot: tmux.conf 6430744+dirty sha256:c1ffe189d326; mu-crew-dotfiles d633e15; plugin tmux-fingers-rs 5759d08; plugin tmux-fzf 05af76d; plugin tmux-session-picker 1158ce2; plugin tmux-yank acfd36e; plugin tpm 7bdb7ca; plugin vim-tmux-navigator e41c431
