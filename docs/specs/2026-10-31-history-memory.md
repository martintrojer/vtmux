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
6. **Threading gate:** the benchmark records the longest single compression step on the main loop and the output latency of a busy pane while another pane compresses. If a step exceeds 2 ms, or the busy pane's latency rises measurably, switch to the fallback: `PaneTerm` behind a per-pane `Mutex`, full-mode compression on a worker thread, and the event loop uses `try_lock` so a pane being compressed drains its output one turn later instead of blocking the loop.

## Builder check (2026-10-31) — facts that reshape the plan

- **No runtime budget setter at our pin.** libghostty-vt 0.2.2 sets `max_scrollback` (bytes) once in `Terminal::new`; neither the Rust crate nor the C API at that commit can change it, and resize keeps the caller's budget (`PageList.zig:1249-1254`). Rows per page shrink with width (`page.zig:1881-1898`) and styled/grapheme-heavy pages grow, so a tight budget prunes below `history-limit` after widening — visible in `#{history_size}`. Upstream Ghostty now has `GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES` (a runtime setter), named in advisory GHSA-w9q4-5jvv-mcv4.
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

## Implementation checklist

0. Upgrade libghostty-vt (setter + security fix); everything green on the new pin.
1. Benchmark script + the baseline numbers above recorded in `docs/test-health/`.
2. Retention: new budget in `crates/vtmux-vt/src/term.rs`, tests for narrow, wide and resized panes (retained rows vs `history-limit`); `history_size`/capture parity with the reference unchanged.
3. Compression: per-pane idle deadline in the server loop (`server.rs` timeouts, next to the resize deadline), `PaneTerm` methods wrapping `compression_activity`/`compress`; unit test with an injected clock; capture byte-identity test across compression.
4. Benchmark run; PAPERCUTS.md row updated to done with the numbers.
