# History compression round 2 — measured gates

Base: `1459bf40`; round-1 compression rebased as `d80bab2`.
Host: Fedora x86_64, pinned Rust/Zig, warm cache. Measurements below use
`flock /tmp/vtmux-stress.lock` and `timeout 120` for each experiment.

## Changes tested

- No starvation override; a compression-only loop turn requires 50 ms quiet
  after any full loop turn. Input and timer work always wins.
- Completed passes wait for one standard page of new scrollback. Explicit
  history reads or resize rearm regardless of scroll progress.
- Standard pages retain their LZ4 call. Large styled pages encode an immutable
  snapshot in 128 KiB blocks; snapshot/publish capped at 4 MiB. Larger pages
  remain resident. Generation/activity/metadata/bytes validated before publish.
- Failed/no-op work advances the traversal. Grid pages are never split.

## Step budget

`target/release/examples/compress_steps COLS ROWS LIMIT FILE`, review corpus
(seed 2), starting host load below 4. Two runs except CJK one; third repetitions
stopped when host load rose above 4. Values: steps, max step milliseconds.
Every completed run reported byte-identical `capture -S - -E - -e`.

| scenario | size / limit | steps | maximum ms, runs 1 / 2 |
|---|---|---:|---|
| spec | 120x40 / 100000 | 291 | 0.266 / 0.288 |
| narrow fg | 80x24 / 10000 | 344 | 1.319 / 1.297 |
| fg+bg | 80x24 / 10000 | 344 | 1.375 / 1.153 |
| OSC 8 fg+bg | 80x24 / 10000 | 315 | 1.548 / 1.396 |
| halfblock | 120x40 / 10000 | 505 | 1.298 / 1.252 |
| fg+bg wide | 200x50 / 10000 | 686 | 1.619 / 1.391 |
| wide plain | 500x24 / 10000 | 121 | 0.120 / 0.118 |
| random base64 | 120x40 / 100000 | 292 | 0.729 / 0.590 |
| CJK | 60x24 / 10000 | 16 | 0.810 / not run |

Measured step budget passes (<5 ms). Three repetitions subsequently completed
at load 1.3–1.8; their maxima were spec 0.208, narrow 1.306, fg+bg80 1.241,
link80 1.354, halfblock 1.214, fg+bg200 1.131, wide 0.099, base64 0.636 and
CJK 0.551 ms. All captures were identical. Worst across all runs: 1.619 ms.
Spec in-process RSS: 146.3 -> 58.3–58.4 MB (includes captured strings).

## Latency — initial failure and final measurements

Exact reviewer command:
`python3 scripts/bench-compress-latency.py BIN spec 2 16`, three complete
alternating pairs against a no-compression main build, host load 1.2–2.2.
No compression windows: continuous markers prevent 50 ms whole-loop quiet.
`compare` returns windows 0, NaN. This demonstrates deferral, not concurrent
compression latency, and must not be graded as a pass.

Concurrent probe: same command with `GAP_MS=60`, five pairs (order reversed
on alternate pairs after the first two), load 1.3–1.5. Compression windows
existed in all five runs, mean 11.07 s:

| | samples | p50 ms | p95 ms |
|---|---:|---:|---:|
| compressing | 897 | 1.553 | 1.815 |
| same build before compression | 318 | 1.564 | 1.827 |
| no compression | 898 | 1.267 | 1.643 |

**Initial +10.5% p95: FAIL** against <10% gate. Small sample set and unequal
first two orders are limitations, not grounds to call this result a pass.

### Final artifact after eliminating a discarded grid clone

`draw_window` built a snapshot with `cells: vec![...]` and `..screen.clone()`.
The latter cloned `screen.cells` only to discard that vector immediately. The
initializer now copies the four remaining metadata fields explicitly. A fresh
read-only refuter confirmed field-by-field equivalence; no speedup is claimed
from noisy latency samples.

Six alternating pairs per scenario, `GAP_MS=60`, same command and 16 s duration,
flock, host load 1.2–2.1. All twelve candidate runs had compression windows:

| scenario | compressing p50 / p95 ms | no-compression p50 / p95 ms | p95 rise |
|---|---|---|---|
| spec | 1.406 / 1.625 | 1.271 / 1.543 | +5.4% |
| narrow | 0.715 / 0.837 | 0.798 / 0.922 | -9.2% |

**Concurrent probes pass the <10% p95 threshold.** Exact GAP2 remains deferred
(no compression window), not graded as a concurrent pass. These measurements
support the threshold, not a general speedup claim. No thread fallback built.

Rejected experiments (reverted): quiet=1 ms let compression overlap the exact
reviewer workload, but p95 rose 28.9% in one pair; persistent RenderState reuse
still gave +22.5% in three pairs. Neither ships. Earlier process-wide malloc
threshold experiments are diagnostic only and do not ship.

## Trickle wakeups

Detached spec pane, 25 s after fill, one line every 6 s for 36 s:
`flock /tmp/vtmux-stress.lock timeout 120 python3 /tmp/hc2/trickle-final.py BIN 6`.
Load 1.3–1.5. New schedule 0.167 context switches/s, RSS 17.4 -> 17.5 MB;
no-compression baseline 0.167/s, RSS 105 MB. This reproduces the reviewer's
6 s probe without restarting a pass. The 10 s / 40 s variant subsequently
measured 0.100/s on both builds at load 1.1–1.4; candidate RSS 17.1 MB steady,
baseline 105.3–105.4 MB.

## Verification

Real red-green tests: `busy_turns_never_force_compression` failed with a page
compressed in a busy turn, then passed after removing starvation; the trickle
test failed with a new deadline after one line, then passed with batching.
`cargo test -p vtmux-server compress::tests`: 7/7 pass.
`cargo test -p vtmux-vt -p vtmux-server`: 253 passed, 18 ignored; exit 0.
Workspace clippy `-D warnings`: exit 0 before the final comment/quiet-turn edits.

Vendored prototype checks: `zig build test-lib-vt -Doptimize=ReleaseSafe` with
filters `chunked Page` and `PageList incremental compression`: exit 0. Two
upstream tests of oversized replacements now drain within 100 steps rather
than expecting immediate compression. Full upstream suite not run.
Fresh read-only refuter found no concrete safety/liveness defect; it ran no
executable checks. Earlier delegate timeouts had no verdict and are not passes.

Two full `mise exec -- bash scripts/check.sh` attempts timed out at 600 s during
oracle replay. fmt, clippy, workspace tests and regress completed. Regress:
61/174 pass, 105 xfail, 6 skip, 2 flaky (menus and terminal-feature-utf8).
Those timed-out attempts remain failures/incomplete. An extended full run
(`muzzle run --rule vtmux-check --run-timeout 1200 -- bash -c
'mise exec -- bash scripts/check.sh'`) subsequently exited 0 in 691 s:
fmt, clippy, cross-platform check, workspace tests, regress 61/174 pass,
105 xfail, 6 skip, 2 flaky, oracle replay 130 scenarios / 0 failures.
Host load rose during gates; warm cache. Over the 10-minute budget, recorded
rather than silently treating a timeout as success. Runtime upkeep is needed.
CI checkpoint run 41406557488 passed Linux and macOS; final follow-up CI is
recorded in the task close evidence.

## Discarded structural split prototype

Splitting styled grid pages was abandoned. `compact()` returned null for a
47-row hyperlink page; incorrectly counting that no-op as progress restarted
verification indefinitely. The orchestrator killed a 2.4 h experiment and
removed its 500 MB log. After making no-ops advance, it terminated (5,370 steps,
1.594 ms max, identical capture), but RSS rose 114.9 -> 128.2 MB. None of that
prototype is in the patch. All subsequent runs have time and output bounds.

## Remaining

The measured single-threaded step and concurrent p95 gates pass after the
follow-up allocation removal. Continuous busy output defers reclamation;
that is intentional, not a compression-work measurement. No thread fallback
is required by the measured gates. Full upstream Zig suite and arbitrary
pages larger than 4 MiB remain untested beyond the documented skip policy.
Follow-up branch CI and final rebase status are in task close evidence.

## Round 3: the busy-pane baseline cost (hist_compress_r3)

The round 2 close graded only windows where compression ran. Review
(`high_hist_compress_r2_busy_pane_latency`) showed that the exact GAP2 command,
`python3 scripts/bench-compress-latency.py BIN spec 2 16`, was +16% p50/p95
over the parent across whole runs, although nothing was compressed (RSS flat
at 204 MB).

**Bisect**: alternating release builds, same command, flock
`/tmp/vtmux-stress.lock`. Server CPU per sample came from
`/proc/PID/task/*/schedstat`, faults from `/proc/PID/stat`.

| build | p50 / p95 ms (pooled) | server µs/sample | minflt/sample |
|---|---|---|---|
| parent | 0.93-1.05 / 1.17-1.36 | ~1830 | ~200 |
| commit 1 (loop, observe) | 1.19 / 1.49 | | |
| HEAD without the server hooks | 1.13 / 1.42 | ~2120 | |
| HEAD without patch 0004 | 1.02 / 1.21 | ~1830 | |
| parent + patch 0004 alone | 1.19-1.22 / 1.50-1.55 | ~2400 | |
| parent + `compression_job` field only | 0.96 / 1.20 | ~1880 | |
| parent + `compression.Page` +2 fields | 1.21 / 1.54 | ~2410 | |
| parent + `compression.Page` +8/16/24 B pad | 1.16-1.17 / 1.46-1.47 | ~2300 | 474 |

None of `PaneCompress::observe`, the timeout scan or `compress_turn` costs
anything measurable. The two fields patch 0004 added to `compression.Page`
did: the PageList `Node` union grew from 488 to 512 bytes. Padding alone
reproduces the cost. The user-space flat profile (`perf -e cpu-clock:u`) is
unchanged. The extra time is in the kernel, as page faults on the per-frame
~260 KB `Vec<Cell>` buffers (snapshot, compose window, client screen), which
are just over glibc's dynamic mmap threshold. The larger node layout changes
the heap so that those buffers are mapped and unmapped each frame instead of
reused. Evidence: with `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=…:trim_threshold=…`,
both builds drop to 0 faults per sample and the padded build is as fast as the
parent.

**Fix** (patch 0004 only): `compression.Page` keeps its upstream fields, and a
comptime assert pins its size. The block format follows from the page size
(`Page.chunked`, over a standard page). `encoded` owns the exact allocation.
Two latent bugs were fixed on the way:
- `cloneBuf` (`pagePreservingState`: search, snapshots) decoded a chunked page
  as one LZ4 block.
- `Job.finish` gave up whenever `remap` could not shrink in place. That failed
  the two `PageList incremental compression restarts after … replacement`
  tests under the testing allocator, and the round 2 note claimed they passed.
  Fixed by copying the exact encoding in that case; full mode and prepend
  compress oversized pages as one job.

The vtmux side is unchanged. Allocator tuning stays out of scope (spec).

## GATE RECORD (round 3)

- GATE: 473e52f (on b5ceb8a, which contains 965d4130), `mise exec -- bash scripts/check.sh`, full.
  RESULT: exit 0, 1107 s (over the 10-minute budget: upkeep, not a stop).
  fmt, clippy, cross-check and workspace tests ok; regress 62/174 pass,
  103 xfail, 6 skip, 3 flaky (clock-mode passed, screen-redraw-menus and
  terminal-feature-utf8 failed); oracle replay 167 scenarios, 0 failed.
  CONDITIONS: Fedora host, pinned toolchain, warm cache, host load 8-12.
- GATE: patch 0004 at 473e52f, `zig build test-lib-vt -Doptimize=ReleaseSafe
  -Dtest-filter=Page` (ghostty 3bd54e025 + patches 0001-0004). RESULT: exit 0,
  955/957 (2 skipped); `-Dtest-filter=PageList` 45/45 steps. Full upstream
  suite not run.
- GATE: busy-pane latency, exact GAP2 command, 4 alternating rounds per
  build: parent b5ceb8a, fix 473e52f, before b0354d7. Whole runs, samples
  t ≥ 0.5 s. RESULT, pooled p50/p95 ms: parent 1.180/1.423 (n 18928),
  fix 1.158/1.411 (n 19125), before 1.399/1.706 (n 17876). Fix against parent:
  p50 −1.9%, p95 −0.8%. Before against parent: +18.6% / +19.9%. Per round,
  fix p95 1.362/1.451/1.374/1.325 against parent 1.411/1.433/1.421/1.416.
  minflt/sample: fix 188-250, parent ~198, before ~360. No compression ran in
  any run (RSS flat). CONDITIONS: **host load 9.3-12, not below 4** (the
  spec's condition). Three orphaned CPU-bound test processes owned by other
  workers held the load up for more than 3 hours of waiting; I did not kill
  them. Every build ran under the same load, alternating, so the comparison
  holds, but this is a deviation from the stated condition and not a clean pass.
- GATE: compression still runs, GAP_MS=60 spec 2 16, 2 alternating pairs fix/parent.
  RESULT: RSS 204 to 144 MB on both fix runs, flat on the parent. p95 while
  compressing against no compression: −1.1% (2 windows, mean 11.1 s).
  CONDITIONS: load 8.6-9.2 (same deviation).
- GATE: step budget, `compress_steps` ×3. RESULT: spec max 0.531 ms, narrow
  truecolour max 1.548 ms (≤ 5 ms); capture identical on every run.
