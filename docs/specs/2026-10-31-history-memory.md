# Scrollback memory: compress idle history, stop over-retaining

Status: approved 2026-10-31 (owner). Build after M1 (spec, *Paper-cut fixes*: candidates are taken one at a time, after M1).

## Problem

Large scrollback in tmux costs a lot of RAM, and people set `history-limit` low to avoid it. That is a long-standing paper cut:

- tmux #3352 (2022): 10M lines × 80 plain cells is about 6.7 GB (nicm). nicm wants a byte-based limit shared across panes some day; nobody has built it.
- tmux #3194 (2022): `history-limit 50000000`, the server grows past 5 GB RSS, input lags.
- tmux #4859 (2026): "Configurable limit for memory usage for a pane's history"; 2-3 panes reach 23.8 GB RSS (a leak, fixed in #4862, but the request stands).
- tmux PR #4013 (2024): a regression made scrollback take about 4× more memory; 2M lines went from 715 MB to 3.2 GB RSS.
- tmux #4777 / #4962 (2025-26): copy-mode crash in long sessions with big histories; users (the owner included) keep `history-limit` low partly for this.

vtmux has a second problem on top: today it uses more memory than tmux. Measured at `a1bf083` (release build), 120×40 pane, `history-limit 100000`, 150k lines of about 100 characters, idle afterwards:

| | RSS |
|---|---|
| tmux 3.8 | 66 MB |
| vtmux | 152 MB |

## Approach

Two changes, in this order:

1. **Stop over-retaining.** Ghostty can cap scrollback only in bytes (`max_scrollback`). `history_budget` (`crates/vtmux-vt/src/term.rs`) sizes that budget as if every pane were at least 256 columns wide, so narrow panes keep far more rows than `history-limit`, and `history_size` hides the excess. Fix the budget so retained rows track `history-limit` closely, and so a later resize or `history-limit` change still behaves like tmux.
2. **Compress idle history.** libghostty-vt has built-in, caller-driven LZ4 page compression: watch `Terminal::compression_activity()`; once it hasn't changed for an idle period, call `Terminal::compress(CompressionMode::Incremental)` until it stops returning `Pending`. Reading history (capture-pane, copy mode, search) restores pages transparently. libghostty creates no thread or timer: vtmux drives it from its event loop with a per-pane idle deadline, like the resize timer.

No option (owner, 2026-10-31). Compression is invisible to scripts: every byte any command returns is unchanged. Under the paper-cut rules this puts it with the performance fixes, which need no flag. If measurements show a real cost (CPU, latency), add `vtmux-history-compress` then.

## Scope

In:
- `history_budget` replaced by a budget that keeps retained rows close to `history-limit` at any pane width, with the over-retention bound written down.
- Per-pane idle compression in the server loop: deadline after the last compression-relevant activity, incremental steps bounded per loop turn so one pane can't stall the others.
- A pinned benchmark (below) and a `docs/PAPERCUTS.md` row.

Out:
- A byte-based or shared-across-panes history limit (nicm's long-term idea). It would be a new capability, not a defect fix.
- Changing what `history-limit` means. It stays a line count, as tmux defines it.
- Compressing the visible screen or the alternate screen (Ghostty compresses scrollback pages only).
- Allocator tuning (jemalloc, `malloc_trim`). Measure it, but separately.

## Key decisions

| Decision | Why | Whose |
|---|---|---|
| No option; always on | No script can see it; the paper-cut policy exempts performance fixes from the opt-in rule; a default-off flag helps nobody who doesn't know about it | owner |
| Fix over-retention as part of this | Otherwise compression mostly recovers memory vtmux wastes itself, and "less RAM than tmux" can't be shown | owner |
| Build after M1; record now | The spec's paper-cut rule; M1 workers are in the same files (`vtmux-vt`, the server loop) | owner |
| Idle delay 5 s, at most one incremental step per pane per loop turn, steps round-robin across panes | Keeps an interactive pane's output path free of compression work, and keeps the loop responsive with many panes | orchestrator's call; tune from the benchmark |
| Activity token checked when a pane's output is read and on resize/history changes, not polled | The token is cheap, but polling every pane every tick is wasted work | orchestrator's call |
| Single-threaded first; a worker thread only if the gate below fails | Ghostty's compress "is not thread-safe with other operations on the same terminal" (terminal.h, `ghostty_terminal_compress`), and `Terminal` is not `Send`. A thread would need a per-pane lock on every grid reader (render, capture, copy mode, formats), a structural change to a core that is single-threaded by design. Idle-only, one bounded step per turn (about one 400 KB page, LZ4) should cost well under a millisecond | owner agreed (2026-10-31) |

## Success criteria

All measured by a pinned benchmark script in the repo (`scripts/bench-history-memory.sh`) against the reference tmux 3.8:

1. **RSS:** vtmux ≤ tmux at `history-limit 100000`, 120×40, 150k lines of typical log text, 10 s idle after output stops.
2. **Narrow panes don't over-retain:** with `history-limit 10000` in an 80-column pane, retained rows (Ghostty `scrollback_rows`) stay within 10% plus one page of `history-limit`.
3. **No visible delay:** `copy-mode` entry on fully compressed 100k-line history takes no more than 2× entry on the same history uncompressed (one pane, release build). `capture-pane -S - -E -` output is byte-identical before and after compression.
4. **Idle CPU:** with nothing to compress, the server does no compression work (no wakeups for it).
5. **Compliance:** check.sh green; no oracle or regress change except new scenarios pinning `history-limit` applied to existing panes (a compliance fix).
6. **Threading gate (BRIEF v2, hist_compress_r2):** every measured main-loop compression step must be at most 5 ms, and busy-pane PTY-to-client-frame latency must rise less than 10% at p95 against a build without compression. Measure alternating release builds under `/tmp/vtmux-stress.lock` at host load below 4. Rework single-threaded scheduling and work bounds first; if those cannot meet the gate, switch to the fallback: `PaneTerm` behind a per-pane `Mutex`, full-mode compression on a worker thread, and the event loop uses `try_lock` so a pane being compressed drains its output one turn later instead of blocking the loop.

## Builder check (2026-10-31) — facts that reshape the plan

- **No runtime budget setter at our pin** (since fixed: task `ghostty_upgrade` vendored the wrapper at ghostty 3bd54e025, see `third_party/libghostty-rs/VENDOR.md`, and added `PaneTerm::set_history_budget`, which applies to the primary screen and defers its `hsize` clamp while the alternate screen is up). libghostty-vt 0.2.2 sets `max_scrollback` (bytes) once in `Terminal::new`; neither the Rust crate nor the C API at that commit can change it, and resize keeps the caller's budget (`PageList.zig:1249-1254`). Rows per page shrink with width (`page.zig:1881-1898`) and styled/grapheme-heavy pages grow, so a tight budget prunes below `history-limit` after widening — visible in `#{history_size}`. Upstream Ghostty now has `GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES` (a runtime setter), named in advisory GHSA-w9q4-5jvv-mcv4.
- **That advisory also affects us.** Memory corruption when a clear prunes the cursor's page, in libghostty-vt's default (small-limit) configuration; fixed upstream in e257bb45 (PR #14857). Our pin predates it. Upgrading libghostty-vt is a prerequisite, and a security fix in its own right.
- **tmux applies a new `history-limit` to existing panes** (`options.c:1464-1466` → `session_update_history`, `session.c:825-847`). vtmux reads it only at pane creation (`server.rs:2198-2200`): an existing compliance gap. It needs the runtime setter too.
- **Wide panes need no separate formula** (pages stay pool-sized; only rows per page shrink, `page.zig:1889-1890`).
- **Reading history decompresses silently and permanently** (`Node.page()` restores without `markActivity`, `PageList.zig:90-95`). After capture-pane or copy mode the activity token doesn't change, so vtmux must re-arm its idle deadline after its own history reads (and after `freeze_history`).
- **Compression doesn't change pruning** (compressed pages count at full size, `PageList.zig:3527-3529`), so the two halves don't interact.
- **Compression is supported only on 64-bit Linux and Darwin** (`mem.zig:32-60`); elsewhere `compress` returns `Unsupported`.
- **Copy mode copies the whole history at entry** (`grid.rs:74-90`), which decompresses everything. Criterion 3 times the `copy-mode` command itself on fully compressed history; a lazy copy-mode grid is out of scope here.

## Resolved decisions (orchestrator, from the builder check)

| Decision | Why |
|---|---|
| Step 0: upgrade libghostty-vt to a commit with `SCROLLBACK_MAX_BYTES` and the GHSA-w9q4-5jvv-mcv4 fix, as its own task before anything else | A security fix we need anyway; it removes the "budget fixed at creation" constraint |
| Retention: set the byte budget from the current width and `history-limit`, and update it with the setter on resize and on `history-limit` change | With a setter, "close to `history-limit`" and "behaves like tmux on resize" both hold |
| Runtime `history-limit` on existing panes: in scope, matching `session_update_history` | Same setter; closes an existing compliance gap; its oracle change is a fix, recorded as such |
| Re-arm the compression deadline after vtmux's own history reads | Otherwise read history stays decompressed forever |
| Criterion 3 measures `copy-mode` entry on compressed 100k-line history; the bar is "no worse than 2× entry on uncompressed history" rather than 50 ms | Entry copies all history today; an absolute 50 ms would require a lazy copy-mode grid, which is a separate piece of work |

## Retention as built (hist_retain, 2026-11-01)

The byte-budget resolution above was refuted (scratch `delegate_refute-hist-retain`): a byte budget holds far fewer rows of styled output, so `#{history_size}` fell below tmux's (5620 rows for tmux's 9978 at 80 columns, 70 truecolour cells per line). Ghostty 3bd54e025 has a line limit (`OPT_SCROLLBACK_MAX_LINES`), now exposed by the vendored wrapper. `PaneTerm` sets it to `max(history-limit, hsize)` plus one page of rows at the current width, re-sets it after every resize, and turns the byte limit off. Ghostty prunes whole pages, so it keeps every row tmux has and at most one page more, whatever the styling. `history-limit 0` keeps the byte limit at 0, since the line limit can't go below a page. Applying `history-limit` changes to existing panes moved to task `hist_limit_live`. Numbers: `docs/test-health/2026-11-01-history-memory.md`.

## Compression round 2 schedule and work bounds

The round-1 gate record was refuted: spec busy-pane p95 rose 18–22%, styled
pages took 5–7 ms, and a single new line restarted a traversal of compressed
history. The new schedule requires 50 ms of whole-loop quiet, with no starvation
override; input always wins. A completed pane pass is rearmed only after a
standard page of new scrollback, or a history read/resize. Continuous activity
may therefore defer memory reclamation indefinitely (superseded by the
starvation override below).

Grid pages are not split. Standard pages still use a single LZ4 step; larger
pages encode an owned immutable snapshot in 128 KiB blocks. Snapshot allocation,
comparison and publication are capped at 4 MiB; larger pages stay resident.
Publication requires unchanged generation, activity, metadata and bytes. Failed,
skipped or no-op work advances the traversal; it never counts as progress.
Encoded allocations use Ghostty's page allocator, not the general malloc heap,
so retaining compressed history does not perturb transient render allocations.
No process-wide allocator tuning or worker thread is introduced. Measurements
and any unmet criteria are recorded in `docs/test-health/` and task notes.

## Starvation override (hist_compress_busy, 2026-11-05)

The owner's case is a pane that streams all day (agent logs, `tail -f`, builds):
under the round-2 schedule its history never shrinks. Measured: a pane at the
100k limit writing 10 or 1000 lines/s held 107-110 MB for 20 minutes, against
18.5 MB once idle and tmux's 66 MB.

A pane is *starved* once its history has kept changing (gaps under `IDLE`, 5 s)
or kept wanting a pass (one a busy loop holds off) for `STARVE` (10 s). A new
run starts only from rest: no pass pending and no change for `IDLE`. A starved
pane's steps also run at the end of busy turns, after the turn's frames are
written, one step per turn under the same `REST_FACTOR` rest (shared by the
starved panes since low_compress_many_streaming, below), and in quiet
turns without waiting out `IDLE`. Ghostty's traversal takes the oldest pages
first, farthest from the active area; it never touches the viewport or the
page holding the active area's top row. The pane stays starved until its pass
completes and its history is quiet for `IDLE`, so each new page of a streaming
pane is compressed soon after it scrolls off.

| Decision | Why |
|---|---|
| Starvation counts history changes, not the pane's own deadline | Output postpones the deadline every time; the start of the run does not move |
| Stays starved across completed passes while the run goes on | Resetting at each pass would let a streaming pane gather another `STARVE` of pages (about 10 MB at 1000 lines/s) every cycle |
| `STARVE` 10 s, longer than `IDLE` | A shell someone types in, with pauses of 5 s, never meets a step in a busy turn; the step is the same bounded work as in quiet turns |
| Steps after rendering, one per turn, same rest | Keeps the work bound and the gate below unchanged; a step delays at most the next turn, by at most one step |

Gate (criterion 6, run 2026-11-05, `docs/test-health/2026-11-05-history-compression-busy.md`):
busy-pane p95 against a build without the override, GAP_MS 2 (the loop is never
quiet; with the override the history panes compress meanwhile): spec +0.5%,
narrow -1.6% inside the compression window. Steps are unchanged (worst 0.8 ms).
Streaming RSS: 18.5-19.8 MB at 10 and 1000 lines/s for 20 minutes, against
106.6-109.8 MB before and tmux's 65.7-66.7 MB.

## Shared rest (low_compress_many_streaming, 2026-11-06)

The rest above was global: about 1% of the loop for compression however many
panes stream, about 40 spec-page steps a second. Twenty 120x40 panes at 1000
lines/s fill 52 pages a second, and falling short is a cliff, not a drift:
each pane's traversal starts at its oldest page, moves slower than its history
moves towards pruning, and compresses only pages about to be pruned. Measured:
20 such panes held 1958-1987 MB for 10 minutes, against 202 MB idle.

The rest after a step is now `REST_FACTOR` times the step divided by the number
of panes starved at the time, up to `MAX_SHARE` (25), and at least `STEP_GAP`
(`compress.rs` `rest`). Each starved pane gets the steps a lone starved pane
gets; with none or one starved, nothing changes.

| Decision | Why |
|---|---|
| Divide the rest, keep one step per turn | A busy pane's output still meets at most one step in progress; more steps per turn would delay a turn by several |
| Count starved panes only | Panes merely due (idle for `IDLE`) compress in quiet turns, where the loop has nothing else to do; starvation is what says a pane's output outruns that |
| `MAX_SHARE` 25 | A rest stays at least four steps, so compression takes at most a fifth of the loop however many panes stream |

Gate (run 2026-11-06, `docs/test-health/2026-11-06-history-compression-many-streaming.md`):
20 streaming panes at 202.5-230.9 MB (median 216.7) against 202.3 MB idle and
tmux's 1247-1252 MB; one pane unchanged (18.6-19.5 MB). Busy-pane p95 against
`nocompress`, GAP_MS 2, inside the compression window: 2 history panes spec
-9.8%, narrow +0.0%; 20 history panes spec -1.5%, narrow -5.4%. Server CPU
unchanged (66.9 against 66.7 s at 20 panes over 12 minutes).

## Implementation checklist

0. Upgrade libghostty-vt (setter + security fix); everything green on the new pin.
1. Benchmark script + the baseline numbers above recorded in `docs/test-health/`.
2. Retention: new budget in `crates/vtmux-vt/src/term.rs`, tests for narrow, wide and resized panes (retained rows vs `history-limit`); `history_size`/capture parity with the reference unchanged.
3. Compression: per-pane idle deadline in the server loop (`server.rs` timeouts, next to the resize deadline), `PaneTerm` methods wrapping `compression_activity`/`compress`; unit test with an injected clock; capture byte-identity test across compression.
4. Benchmark run; PAPERCUTS.md row updated to done with the numbers.
