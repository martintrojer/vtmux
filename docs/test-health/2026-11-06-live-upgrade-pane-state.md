# Live upgrade: pane terminal state through the snapshot 2026-11-06 (lu_pane_state)

Spec: `docs/specs/2026-11-06-live-upgrade.md`, decisions 5, 11 and 23. Host: devserver, CentOS Stream 9,
96 cpus, shared with other agents. Built on `8a63595` (main `ee4957d` + snapshot crate + spike) over a stopgap
per-page API; rebased onto main `3ae218e` on the spike's streaming API (*Rebased onto main* below).

## What was built

- `vtmux_server::snapshot_term`: `term_to_snapshot(&PaneTerm, Form) -> vtmux_snapshot::TermState` and
  `term_from_snapshot(&TermState, ghostty_patches) -> PaneTerm`, plus `Pane::export_term` / `import_term` and
  `Popup::export_term` / `import_term` for the model's hooks. `Form::Rows` writes every page in the vtmux row
  format (checkpoints, any reader); `Form::SamePin` lets pages Ghostty holds compressed go as
  `HistoryPage::ghostty` (tag 6) behind `Meta.requires` `ghostty-pages-<PATCH_STACK>`
  (`ghostty_pages_requirement()`, read with `reader_features()`).
- Export is per page (`PaneTerm::export_state_head` + `export_pages`): each Ghostty page is converted and LZ4-encoded into a `HistoryPage` as it comes, holding
  at most a screenful and a page in the rows form. Import assembles one Ghostty page at a time from the history
  pages and visible rows (`pages_of`) into a `StateImport`, following the screen's recorded page layout (`Screen.pages`) when it adds up.
- After an import from rows the pane's history is resident: `PaneCompress::restored` makes it due at once
  (decision 23), in the compressor's ordinary background steps.
- The snapshot crate carries what the spike's `TermState` has and v1's records lacked (new tags only; the golden
  `v1.snap` is unchanged and still pinned): mode saved/default values, the kitty flag ring and index, cursor
  shape, protected mode, OSC 133 click and marks, implicit hyperlink ids and URIs/ids as bytes, history times
  (run-length), byte budget, the alternate screen's entry row and frozen copies, the sync hold's cursor, tmux's
  own charset, `extended-keys`, and Ghostty-only state (`Emulator`).

## Round trips

`crates/vtmux-server/tests/snapshot_term.rs`: pane, `term_to_snapshot`, a whole `Snapshot` stream (upgrade:
`encode` / `decode_for`; checkpoint: `encode_manifest` + chunks / `decode_with`), `check()`, `term_from_snapshot`.
Compared right after import: every capture of the spike's harness (`-e -S -`, `-J -N`, `-F -T`, `-H`, `-C -e -I`
with times, `-a`), cursor, modes, marks, pending input, view, the history times, and the re-exported state
with page tables flattened. Then the same further input to both: replies and all of it again.

| corpus | result |
|---|---|
| spike fuzzer, 60 seeds (gate), alternating Rows / SamePin | pass; `VTMUX_STATE_FUZZ_SEEDS=4000` pass (78 s) |
| 214 oracle pane programs x 4 cuts (checkpoint path) + compressed history through both forms | pass |
| long history (6000 lines, 90x30) in both forms, page layout and capacities reproduced | pass |
| alternate screen + sync hold + non-UTF-8 OSC 8 URI and id; foreign Ghostty pages refused; bad layout ignored; wrong-width or short screens refused | pass |

Mutations (each reverted), failing tests out of 6: tab stops dropped 4, history times dropped 4, `38;5;N`
form dropped 1, parser continuation dropped 2, REP character dropped 4, implicit link ids dropped 1, frozen
marks dropped 2, ACS bit dropped 3, page layout ignored 1 (`long_history_in_both_forms`, page shapes).

Stress: test binary and `compress::` unit tests 5x under 48 busy loops (`flock /tmp/vtmux-stress.lock`, load
23-54): 5/5 pass, 27.6-28.4 s each. No test spawns a shell: the dash-like `/bin/sh` run does not apply.

## Time and memory, 10 panes x 100k lines

`cargo build --release -p vtmux-server --example snapshot_bench`, then `flock /tmp/vtmux-stress.lock
snapshot_bench 10 100000 <form>` (120x40 panes, the spike's log-like content). One form per process. Write:
`term_to_snapshot` for every pane plus `encode`; read: `decode_for` plus `term_from_snapshot` per pane, with the
source panes dropped (the new image). Peaks from `VmHWM`, reset between phases.

| form | history | write | stream | write peak over panes | read | read peak | imported compressed | identical |
|---|---|---|---|---|---|---|---|---|
| SamePin (upgrade) | compressed | 56 ms | 48.7 MB | +91 MB | 45 ms | 175 MB | 2580 / 2580 | yes |
| Rows | compressed | 1563 ms | 22.3 MB | +57 MB | 1107 ms | 1016 MB | 0 | yes |
| Rows | resident | 1437 ms | 22.3 MB | +57 MB | 1215 ms | 1016 MB | 0 | yes |

For comparison, the spike's whole-state path on the same host (`state_bench 10 100000`, one process): rows
export 742 ms + import 481 ms, +624 MB to hold the rows `TermState` (719 MB), process peak 2679 MB. Per-page
encoding takes the write peak from +624 MB to +57 MB. The read peak of the rows form is the panes themselves,
resident (930 MB, like a never-compressed pane) until the compressor catches up. The vtmux row format plus LZ4
is 2.2x smaller than Ghostty's compressed pages for this content (22.3 MB against 48.7 MB): the same-pin path
saves time, not bytes. The rows path costs about twice the spike's raw export and import (row conversion,
run-length encoding, LZ4 and BLAKE3); it is the checkpoint and cross-pin path, and outside decision 13's 1 s budget.

## Not carried

- Events drained each loop turn (replies, bells, title changes, passthrough, renames): drain before export
  (the `replies`, `bells`, `dcs`, `rename` fields stay empty). `HistoryState.reads` is not carried: the
  compressor is re-armed on import instead.
- Page tables are rebuilt from the rows (dead style and link entries dropped, first-use order), so the
  re-exported state equals its source with page tables resolved, not entry for entry. Captures and further
  output are identical.
- A frozen (alternate-screen) cell with a link whose URI or id did not read comes back with an empty URI and
  implicit id 0; Ghostty always reads both for a linked cell.
- Whether a history page was compressed travels only for pages in Ghostty's form; rows come back resident and
  are recompressed.

## Rebased onto main (lu_stack_rebase)

The spike, its import validation and streaming (455b790), this conversion and the snapshot crate's terminal
records were rebased onto main `3ae218e`: the Ghostty patch is now 0017, after main's 0015 (REP, DECCOLM, tab
reflow) and 0016 (DECAWM). The conversion uses the streaming API (`PaneTerm::export_state_head` +
`export_pages`, `StateImport`) in place of the stopgap per-page callback it was written against; snapshot-side
failures are `TermError::Snapshot`, vtmux-vt's refusals `TermError::State`. `Server::to_snapshot` now fills
`Pane.term` and `Popup.term` (Upgrade: `SamePin`; CheckpointFull: `Rows`; CheckpointLayout: none;
`to_snapshot_with` takes the form), sets `meta.ghostty_patches` and lists `ghostty_pages_requirement()` in
`meta.requires` when a page went in Ghostty's form; `restore_term` / `restore_popup_term` import them.

The 4000-seed round trip found two pieces of state main added after the spike, both fixed with tests: tmux's
`INPUT_LAST` (REP after a print, patch 0015's scanner flag; `PaneState.last_print`, snapshot `TermState`
tag 36 `input_last`), and an alternate screen narrowed without a reflow keeping cells-used counts past its
width (tmux's `cellused` outlives `grid->sx`), which validation refused.

Ghostty (EMULATOR RULE): all 17 patches apply in order to the pinned commit; the full `zig build test-lib-vt
-Demit-lib-vt=true -Dapp-runtime=none` (Debug) gives 3258 pass, 38 skip, and per test binary the two known
failures (`tabClear single`, kitty's temporary-file test under `/tmp`), as on main plus the spike's tests.

Same host, load 4-9 outside the stress runs:

| check | result |
|---|---|
| `state_roundtrip` / `snapshot_term` fuzz, `VTMUX_STATE_FUZZ_SEEDS=4000` (2000 per form in `snapshot_term`) | pass (64 s / 76 s); seeds 126 and 466 failed before the fix |
| `state_fuzz` (ReleaseSafe Zig) rows, state, blob, blob-full, seeds 0-1199 each | 0 Zig crashes, 0 Rust panics; imported 44 / 291 / 13 / 12, the rest refused |
| state, snapshot crate, `snapshot_term` and `server::snapshot` + `compress::` tests, 5x under 48 busy loops (`flock /tmp/vtmux-stress.lock`, load 37-54) | 5/5 pass, 56-57 s each |

Streamed bench, 10 panes x 100k lines, 120x40, release:

| bench | path | time | memory |
|---|---|---|---|
| `STATE_BENCH_STREAMED=1 state_bench` | Blob | 346 ms | +56 MB, peak 1956 MB |
| | Rows (compressed / resident) | 1267 / 1118 ms | +929 / +930 MB |
| `snapshot_bench samepin` | write 219 ms (convert 180 + encode 40), read 210 ms (decode 24 + import 185) | 429 ms | stream 48.7 MB, write peak +92 MB, read peak 177 MB, 2580/2580 pages compressed |
| `snapshot_bench rows` | write 1641 ms, read 1618 ms | 3259 ms | stream 22.3 MB, +57 MB / 999 MB |
| `snapshot_bench resident` | write 1428 ms, read 1284 ms | 2712 ms | stream 22.3 MB, +57 MB / 999 MB |

`SamePin` takes 429 ms against 101 ms on the stopgap. The difference is the blob checks from 455b790 (an
XxHash3 checksum and the `initBuf` layout check on every blob): 455b790's own note has whole-state blobs at
362 ms against the spike's 37 ms. That is still inside decision 13's 1 s budget. Every import captures as its
source does.

PERF GATE (compress.rs gains `PaneCompress::restored`, which only a restore calls): `bench.py agent_panes
key_echo`, AGENT_PANES "5 20", AGENT_CLIENTS "1 2", release, base main `3ae218e` against the tip, load 9.
Server CPU: 5/1 1.5 -> 1.5 %, 5/2 2.1 -> 2.3 %, 20/1 3.5 -> 3.7 %, 20/2 4.6 -> 4.6 %. RSS is unchanged.
key_echo p50: 0.32 -> 0.30 ms (idle), 0.44 -> 0.45 ms (flood). All within noise.

## Integrated onto main a19b4b4 (lu_emulator_integration)

One branch on main `a19b4b4` (highest patch there 0016): 0017 (state export), one copy of 0018 (reflow
hyperlink map), 0019 (reflow retry) on top, the import validation, the model plug-in, and `CheckpointFull`
writing every pane's terminal in the row form (`612e22b`). Measured on the tip `612e22b`, same devserver
(96 cores, shared, load 6-10):

| check | result |
|---|---|
| `state_roundtrip` / `snapshot_term` fuzz, `VTMUX_STATE_FUZZ_SEEDS=4000` (2000 per form in `snapshot_term`), Debug and release + ReleaseSafe Ghostty | pass, 65 / 80 s (Debug), 32 / 42 s (ReleaseSafe) |
| `state_fuzz` (release, ReleaseSafe Ghostty), seeds 0-999 of each of the ten kinds | 0 Ghostty crashes, 0 Rust panics; imported rows 46, state 234, links 233, styles 272, graphemes 199, cells 342, cursor 781, capacity 154, blob 11, blob-full 11 |
| reflow repro (80x24, 4000 lines of per-cell OSC 8 links and graphemes, to 17 columns) and the other `reflow` tests, ReleaseSafe | pass |
| `zig build test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none` (Debug) over 0001-0019 | 7013 / 7091 pass, 76 skip, `tabClear single` the only failure in each test binary |

The Debug Zig suite takes about 36 min on this host, almost all of it in 0018's `Terminal: reflow keeps the
links of a page full of them (vtmux)`: Debug runs `Page.verifyIntegrity` on every `cursorSetHyperlink`, so its
90000 linked prints are quadratic in the page. `-Doptimize=ReleaseSafe` does not build the suite (an upstream
test asserts `slow_runtime_safety` at comptime, `PageList.zig`).

PERF GATE (Ghostty's reflow changed): `bench.py agent_panes key_echo`, AGENT_PANES "5 20", AGENT_CLIENTS "1 2",
release, base main `a19b4b4` against the tip, load 8-10. Server CPU: 5/1 1.4 -> 1.4 %, 5/2 2.0 -> 2.0 %,
20/1 3.5 -> 3.5 %, 20/2 4.6 -> 4.5 %; RSS +0.1-0.2 MB. key_echo p50: 0.31 -> 0.30 ms (idle), 0.45 -> 0.45 ms
(flood). All within noise.
