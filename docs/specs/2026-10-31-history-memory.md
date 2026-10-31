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
3. **No visible delay:** copy mode `history-top` on fully compressed history returns in under 50 ms (one pane, release build). `capture-pane -S - -E -` output is byte-identical before and after compression.
4. **Idle CPU:** with nothing to compress, the server does no compression work (no wakeups for it).
5. **Compliance unchanged:** check.sh green; no oracle or regress change.
6. **Threading gate:** the benchmark records the longest single compression step on the main loop and the output latency of a busy pane while another pane compresses. If a step exceeds 2 ms, or the busy pane's latency rises measurably, switch to the fallback: `PaneTerm` behind a per-pane `Mutex`, full-mode compression on a worker thread, and the event loop uses `try_lock` so a pane being compressed drains its output one turn later instead of blocking the loop.

## Open questions

- Does the byte budget have to change when `history-limit` changes at runtime? tmux applies a new `history-limit` to new panes only (`window_pane_create` reads it once). Check against the reference and match it. Builder's call to verify.
- Ghostty's page size is fixed (`std_capacity` 215×215 cells). Very wide panes (500+ columns) may need a different budget formula. The benchmark should include one wide pane.

## Implementation checklist

1. Benchmark script + the baseline numbers above recorded in `docs/test-health/`.
2. Retention: new budget in `crates/vtmux-vt/src/term.rs`, tests for narrow, wide and resized panes (retained rows vs `history-limit`); `history_size`/capture parity with the reference unchanged.
3. Compression: per-pane idle deadline in the server loop (`server.rs` timeouts, next to the resize deadline), `PaneTerm` methods wrapping `compression_activity`/`compress`; unit test with an injected clock; capture byte-identity test across compression.
4. Benchmark run; PAPERCUTS.md row updated to done with the numbers.
