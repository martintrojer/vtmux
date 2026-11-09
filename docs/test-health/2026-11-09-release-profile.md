# Release profile: fat LTO, one codegen unit, stripped (`release_profile`)

Task `release_profile`: `Cargo.toml` had no `[profile.release]`, so the shipped binary was cargo's default (thin-local LTO, 16 codegen units, debuginfo stripped, symbols kept). The owner's 2026-11-09 numbers: 10.1 MB (8.3 stripped) against tmux's 1.2 MB. This record picks a profile and measures it against the default.

Host: the shared devserver (CentOS Stream 9, Linux 6.16, AMD EPYC Genoa, 96 cpus), load 9-15 over the runs from other workers; every run went through `muzzle run --class high` (not `flock /tmp/vtmux-stress.lock`). `base` = main `3e57394` with cargo's default release profile, `vtmux` = the same commit with `lto = "fat"`, `codegen-units = 1` (strip only changes the symbol table, so the benched binary is that one unstripped). tmux 3.8 from `cargo xtask ref-tmux`. Each candidate was built with `CARGO_PROFILE_RELEASE_*` overrides of the default. Read CPU as rough; base and vtmux in one run are what to compare.

## Size

Bytes of `target/release/vtmux`, then after `strip` (what `scripts/release-tarball.sh` ships):

| profile | as built | stripped |
|---|---|---|
| default (`base`) | 13,133,872 | 10,738,064 |
| `lto = "thin"` | 13,124,320 | 10,719,032 |
| `codegen-units = 1` | 11,352,856 | 9,923,304 |
| thin + `codegen-units = 1` | 11,498,248 | 10,051,240 |
| `lto = "fat"` | 11,131,856 | 9,808,872 |
| **fat + `codegen-units = 1`** (picked) | 10,942,768 | 9,757,784 |
| fat + cgu 1 + `panic = "abort"` (rejected) | 10,071,648 | 9,089,768 |
| tmux 3.8 (links libevent and ncurses) | 1,422,104 | 1,269,608 |

- With `strip = true` cargo writes the stripped binary itself: `target/release/vtmux` is 9,758,024 bytes, 26% under the default's 13.1 MB. The shipped (stripped) binary is 9.1% smaller; the tarball's gzip of it 3.0% (4,409,576 to 4,279,125 bytes; the whole tarball is 4,282,991).
- Where the bytes are (base, `nm -S`): 6.5 MB of Rust text, 1.35 MB of Ghostty's Zig text, 0.67 MB `.eh_frame` and 0.24 MB `.gcc_except_table` (unwinding), 0.7 MB `.rodata`. LTO shrinks only the Rust part; Ghostty's text is byte-identical in base and the picked build (1,349,151 bytes), and `libghostty-vt.a` is the same size in every candidate's build dir: still ReleaseFast.
- `panic = "abort"` would save another 0.67 MB, but the server relies on unwinding: a live upgrade's resume turns a panic in the rebuild into the switch back (`server/resume.rs` `guarded`, `catch_unwind`), and the persistence restore and the newer-binary check catch a panicking decode (`server/restore.rs`). With abort a bad snapshot would kill the server and every pane's PTY with it. Kept unwind.
- `opt-level` stays 3 (cargo's default for release); `s`/`z` were not tried (and would build Ghostty `ReleaseSmall`, build.rs `zig_optimize_mode`). `debug` stays off: cargo sets `DEBUG=true` for a profile with debug info, and the build script then builds Ghostty in Zig `Debug`.

## Build time

`touch crates/vtmux-core/src/lib.rs; cargo build --release --locked -p vtmux` (dependencies built, Ghostty built): default 108 s, picked 118 s. A clean build, Ghostty's Zig build included: default 219 s, picked 214 s (one run each, load about 10). CI's release job (2-4 vCPU runners, 60 min timeout) was not measured: the single-threaded LTO step weighs more there, a minute or two at most by these numbers.

A profile change gives `libghostty-vt-sys` a new build directory, so Ghostty is cloned and built again; this host can't reach GitHub, so each candidate's `out/` was seeded with a copy of the first build's.

## Speed

`RUNS=5 NO_BUILD=1 BASE=<default> VTMUX=<fat+cgu1> AGENT_PANES="5 20" AGENT_CLIENTS="1 2" python3 scripts/bench.py agent_panes key_echo`, 17:49 UTC, load 9.9 at the start:

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.9 | 0.0 | 15.2 | 68 | 261 | 6.1 |
| 5 panes / 1 clients | base | 2.2 | 0.2 | 14.3 | 68 | 230 | 19.3 |
| 5 panes / 1 clients | vtmux | 1.9 | 0.2 | 14.3 | 68 | 231 | 18.4 |
| 5 panes / 2 clients | tmux | 2.0 | 0.0 | 30.4 | 135 | 260 | 6.2 |
| 5 panes / 2 clients | base | 2.9 | 0.4 | 28.2 | 134 | 275 | 21.8 |
| 5 panes / 2 clients | vtmux | 2.5 | 0.3 | 28.2 | 134 | 279 | 20.9 |
| 20 panes / 1 clients | tmux | 5.3 | 0.0 | 15.2 | 68 | 853 | 8.0 |
| 20 panes / 1 clients | base | 5.2 | 0.2 | 14.3 | 68 | 700 | 32.8 |
| 20 panes / 1 clients | vtmux | 4.5 | 0.2 | 14.3 | 68 | 709 | 31.8 |
| 20 panes / 2 clients | tmux | 5.4 | 0.0 | 30.5 | 136 | 846 | 8.1 |
| 20 panes / 2 clients | base | 6.4 | 0.4 | 28.3 | 135 | 747 | 35.3 |
| 20 panes / 2 clients | vtmux | 5.5 | 0.4 | 28.3 | 135 | 753 | 34.5 |

### key_echo (200 keys 30 ms apart into a raw echo pane, one 200x50 client, sync on, the other pane idle or running `yes`; arrival stamped in the reader thread)

| case | binary | p50 ms | p90 ms | max ms | lost |
|---|---|---|---|---|---|
| flood none | tmux | 0.26 | 0.30 | 0.34 | 0 |
| flood none | base | 0.43 | 0.48 | 0.67 | 0 |
| flood none | vtmux | 0.38 | 0.43 | 0.56 | 0 |
| flood yes | tmux | 0.55 | 0.97 | 2.44 | 0 |
| flood yes | base | 0.54 | 0.79 | 5.98 | 0 |
| flood yes | vtmux | 0.45 | 0.71 | 4.56 | 0 |

`RUNS=5 NO_BUILD=1 BASE=<default> VTMUX=<fat+cgu1> python3 scripts/bench.py throughput throughput_attached`, 18:05 UTC, load 14.6:

### throughput without a client (100 MB log text, 120x40 pane, default history; sgr bytes = 19%)

| input | binary | wall s | server CPU s | MB/s | peak RSS MB |
|---|---|---|---|---|---|
| plain | tmux | 2.65 | 2.47 | 39.6 | 5.3 |
| plain | base | 1.22 | 1.03 | 85.8 | 13.4 |
| plain | vtmux | 1.22 | 1.05 | 85.9 | 12.1 |
| sgr | tmux | 3.26 | 3.15 | 32.2 | 5.4 |
| sgr | base | 1.83 | 1.64 | 57.3 | 13.0 |
| sgr | vtmux | 1.83 | 1.71 | 57.3 | 11.5 |

### throughput_attached (100 MB log, one 200x50 client, window-size latest)

| case | binary | wall s | server CPU s | client CPU s | total CPU s | term bytes / MB in | sync frames | peak RSS MB |
|---|---|---|---|---|---|---|---|---|
| plain / sync off | tmux | 2.85 | 2.77 | 0.00 | 2.77 | 1034346 | 0 | 7.4 |
| plain / sync off | base | 1.63 | 1.50 | 0.01 | 1.51 | 52824 | 0 | 18.8 |
| plain / sync off | vtmux | 1.63 | 1.41 | 0.02 | 1.43 | 56362 | 0 | 17.9 |
| plain / sync on | tmux | 2.85 | 2.73 | 0.00 | 2.73 | 1038289 | 25816 | 7.5 |
| plain / sync on | base | 2.03 | 1.87 | 0.02 | 1.89 | 68366 | 954 | 18.8 |
| plain / sync on | vtmux | 1.83 | 1.57 | 0.02 | 1.58 | 65699 | 918 | 17.8 |
| sgr / sync off | tmux | 4.07 | 4.04 | 0.00 | 4.04 | 1043247 | 0 | 7.5 |
| sgr / sync off | base | 2.03 | 1.91 | 0.01 | 1.91 | 15137 | 0 | 18.6 |
| sgr / sync off | vtmux | 2.03 | 1.95 | 0.01 | 1.96 | 15926 | 0 | 17.7 |
| sgr / sync on | tmux | 4.28 | 4.04 | 0.00 | 4.04 | 1047184 | 25776 | 7.5 |
| sgr / sync on | base | 2.03 | 1.96 | 0.01 | 1.96 | 15603 | 182 | 18.7 |
| sgr / sync on | vtmux | 2.03 | 1.93 | 0.01 | 1.94 | 15691 | 182 | 17.7 |

`RUNS=5 NO_BUILD=1 VTMUX=<binary> UPGRADE_PANES=10 UPGRADE_LINES=100000 python3 scripts/bench.py upgrade`, once per binary (the scenario runs only `VTMUX`), 18:11 UTC (load 9.0) for the picked profile, 18:14 for the default:

**fat + cgu 1**

| case | binary | history lines | idle s | answered ms | redrawn ms | snapshot MB | RSS MB before | RSS MB after |
|---|---|---|---|---|---|---|---|---|
| 10 x 100k fresh | vtmux | 999520 | 0.0 | 943 | 945 | 113.1 | 1566.3 | 146.4 |
| 10 x 100k idle | vtmux | 999520 | 23.3 | 804 | 806 | 113.1 | 145.0 | 146.3 |

**default**

| case | binary | history lines | idle s | answered ms | redrawn ms | snapshot MB | RSS MB before | RSS MB after |
|---|---|---|---|---|---|---|---|---|
| 10 x 100k fresh | vtmux | 999520 | 0.0 | 950 | 952 | 113.1 | 1567.3 | 147.4 |
| 10 x 100k idle | vtmux | 999520 | 23.5 | 774 | 776 | 113.1 | 146.3 | 147.3 |

`RUNS=5 BASE=<default> VTMUX=<fat+cgu1> python3 scripts/bench-micro.py flood many_panes clients status`, 18:17-18:37 UTC:

```
== flood
8 panes x 20 MB                               tmux   wall_s=5.46 server_cpu_s=5.34 term_mb=35.8 sync_frames=5157 peak_rss_mb=14.3
8 panes x 20 MB                               base   wall_s=1.14 server_cpu_s=1.06 term_mb=0.943 sync_frames=100 peak_rss_mb=28.4
8 panes x 20 MB                               vtmux  wall_s=1.13 server_cpu_s=1.05 term_mb=0.948 sync_frames=102 peak_rss_mb=27.6
== many_panes
50 agent panes / 1 client                     tmux   server_cpu_s_per_10s=1.53 frames_per_s=68.2 term_kb_per_s=23.8 rss_mb=11.5 wakeups_per_s=1.61e+03
50 agent panes / 1 client                     base   server_cpu_s_per_10s=1.34 frames_per_s=67.6 term_kb_per_s=24.1 rss_mb=57.3 wakeups_per_s=1.5e+03
50 agent panes / 1 client                     vtmux  server_cpu_s_per_10s=1.11 frames_per_s=67.6 term_kb_per_s=24.1 rss_mb=56.3 wakeups_per_s=1.55e+03
100 agent panes / 1 client                    tmux   server_cpu_s_per_10s=3.21 frames_per_s=68.1 term_kb_per_s=24 rss_mb=17.8 wakeups_per_s=2.35e+03
100 agent panes / 1 client                    base   server_cpu_s_per_10s=2.74 frames_per_s=67.5 term_kb_per_s=24.3 rss_mb=101 wakeups_per_s=2.45e+03
100 agent panes / 1 client                    vtmux  server_cpu_s_per_10s=2.25 frames_per_s=67.6 term_kb_per_s=24.3 rss_mb=99.7 wakeups_per_s=2.6e+03
== clients
4 agent panes / 16 clients                    tmux   server_cpu_s_per_10s=0.302 frames_per_s=2.15e+03 term_kb_per_s=419 rss_mb=7.09 wakeups_per_s=186
4 agent panes / 16 clients                    base   server_cpu_s_per_10s=0.561 frames_per_s=2.15e+03 term_kb_per_s=440 rss_mb=26 wakeups_per_s=130
4 agent panes / 16 clients                    vtmux  server_cpu_s_per_10s=0.527 frames_per_s=2.15e+03 term_kb_per_s=440 rss_mb=25.5 wakeups_per_s=129
4 agent panes / 32 clients                    tmux   server_cpu_s_per_10s=0.518 frames_per_s=4.3e+03 term_kb_per_s=839 rss_mb=9.21 wakeups_per_s=185
4 agent panes / 32 clients                    base   server_cpu_s_per_10s=0.891 frames_per_s=4.31e+03 term_kb_per_s=885 rss_mb=42.1 wakeups_per_s=127
4 agent panes / 32 clients                    vtmux  server_cpu_s_per_10s=0.831 frames_per_s=4.29e+03 term_kb_per_s=881 rss_mb=42.8 wakeups_per_s=128
== status
heavy status, 20 windows, 4 clients, interval 1 tmux   server_cpu_s_per_10s=0.0543 frames_per_s=3.8 term_kb_per_s=1.34 rss_mb=6.34 wakeups_per_s=1
heavy status, 20 windows, 4 clients, interval 1 base   server_cpu_s_per_10s=0.0556 frames_per_s=3.8 term_kb_per_s=0.294 rss_mb=21.2 wakeups_per_s=2.65
heavy status, 20 windows, 4 clients, interval 1 vtmux  server_cpu_s_per_10s=0.0517 frames_per_s=3.8 term_kb_per_s=0.294 rss_mb=20.1 wakeups_per_s=2.35
```

Summary, server CPU of the picked profile against the default:

- Render- and frame-bound work is 7-18% cheaper: agent panes -13 to -14% (5/1, 5/2, 20/1, 20/2), 50 and 100 agent panes -17% and -18%, 16 and 32 clients -6% and -7%, the heavy status line -7%, an attached plain flood with sync on -16%.
- Key echo p50 0.43 to 0.38 ms with the other pane idle, 0.54 to 0.45 ms next to `yes`.
- Parse-bound work is flat: `throughput` without a client +2% (plain) and +4% (SGR) at the same wall time, `flood` -1%. That is Ghostty's parser, which LTO doesn't touch.
- Live upgrade, 10 x 100k: 943 against 950 ms fresh, 804 against 774 ms idle. Noise at 5 runs.
- RSS about 1 MB lower everywhere (less text mapped).

## Release paths checked

- `scripts/release-tarball.sh x86_64-unknown-linux-gnu v3.8-beta1`: exit 0, the binary in the tarball runs as `vtmux` and `tmux`; its own `strip` is now a no-op on Linux. On macOS cargo's strip runs Apple's `strip`, as cargo's default `strip = "debuginfo"` already did; the script still re-signs after its own strip. Not run on macOS here.
- `vtmux-upgrade` from the default build onto the picked one and back: exit 0 both ways, the pane's screen kept.
- `python3 scripts/bench.py --smoke` (ci.yml's perf-smoke, which builds `--release`): exit 0, `cpu_ratio_plain` 0.34 and `cpu_ratio_sgr` 0.54 against the CI baseline's 0.45 and 0.68 (another host), RSS within limits.

Raw JSON of the bench.py runs: [`2026-11-09-release-profile.json`](2026-11-09-release-profile.json).
