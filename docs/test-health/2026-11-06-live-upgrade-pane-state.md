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
