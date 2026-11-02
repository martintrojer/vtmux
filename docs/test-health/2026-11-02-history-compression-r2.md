# History compression round 2 — gate unresolved

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

Measured step budget passes (<5 ms); the repetitions are incomplete.
Spec in-process RSS: 146.3 -> 58.3–58.4 MB (includes captured strings).

## Latency — NOT PASS

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

**+10.5% p95: FAIL** against <10% gate. Small sample set and unequal first two
orders are limitations, not grounds to call the result a pass.

Rejected experiments (reverted): quiet=1 ms let compression overlap the exact
reviewer workload, but p95 rose 28.9% in one pair; persistent RenderState reuse
still gave +22.5% in three pairs. Neither ships. Earlier process-wide malloc
threshold experiments are diagnostic only and do not ship.

## Trickle wakeups

Detached spec pane, 25 s after fill, one line every 6 s for 36 s:
`flock /tmp/vtmux-stress.lock timeout 120 python3 /tmp/hc2/trickle-final.py BIN 6`.
Load 1.3–1.5. New schedule 0.167 context switches/s, RSS 17.4 -> 17.5 MB;
no-compression baseline 0.167/s, RSS 105 MB. This reproduces the reviewer's
6 s probe without restarting a pass. The 10 s variant is not yet measured.

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
**No full-gate PASS, no CI result yet.** Oracle gate runtime needs upkeep or a
recorded extended run; a timeout is not a pass.

## Discarded structural split prototype

Splitting styled grid pages was abandoned. `compact()` returned null for a
47-row hyperlink page; incorrectly counting that no-op as progress restarted
verification indefinitely. The orchestrator killed a 2.4 h experiment and
removed its 500 MB log. After making no-ops advance, it terminated (5,370 steps,
1.594 ms max, identical capture), but RSS rose 114.9 -> 128.2 MB. None of that
prototype is in the patch. All subsequent runs have time and output bounds.

## Remaining

Latency gate remains unresolved. Findings remain open; no thread-fallback
claim, no completion claim. The owner requested fallback only after the
single-threaded gate cannot be met; the numbers above are evidence for that
next decision, not evidence to close round 2.
