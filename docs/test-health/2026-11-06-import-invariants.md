# Import invariants per cell kind, and capacity hints, 2026-11-06 (high_import_invariants_3)

The second re-review of the state import (review_lu_stack_2) found three imported states that crash
Ghostty under ReleaseSafe and capacity hints that make a tiny state allocate gigabytes. validate.rs
refused bad values one at a time; this round checks what Ghostty's own writers produce instead.

## What changed

- **Background-only cells** (`CellState` kinds 2 and 3, Ghostty's `bg_color_palette` and `bg_color_rgb`)
  must be what an erase with a background colour leaves (Screen.zig `blankCell`, style.zig
  `Style.bgCell`): narrow, unprotected, semantic content output, style index 0 and link index 0.
  Before, a style imported and a capture then hit `unreachable` in `grid_ref_style`; a link imported
  and a resize then unwrapped a null in `Page.clonePartialRowFrom`. Text cells keep every combination:
  Ghostty prints empty cells, spacer heads and tails with the cursor's style and link
  (Terminal.zig `printCell`, `writeCell(0, ..)`).
- **Cursor semantic content** is 0-2 (`Cell.SemanticContent`), and `semantic_clear_eol` is set only with
  input (Screen.zig `cursorSetSemanticContent`: OSC 133;I). With output, the first LF hit the assert in
  Terminal.zig `index`. vtmux keeps OSC 133 from Ghostty, so its own exports always have output and no
  flag.
- **Capacity hints**: `styles`, `grapheme_bytes` and `string_bytes` are at most a fresh page's
  (`std_capacity`: 128, 8192, 2048) or four times what the page's content takes (graphemes and link
  strings in Ghostty's chunks), whichever is more. Rows stay bounded by `MAX_PAGE_CELLS`, hyperlink bytes
  by their 16-bit field. A live page's capacity only grows, so after its content is erased the hint can
  pass the bound. The export clamps hints to the same bound (`PageRows::from_ffi`), so every export
  passes. A page imported from it grows again as Ghostty's own pages do.
- **Patch 0018** (`ReflowCursor.writeCell`): the fuzz turned up a crash in Ghostty that live input
  reaches without any import. Write 3000 lines of 30 linked characters on 80 columns, then resize to 17
  columns: the reflow grows the destination page's hyperlink map once, and near the 16-bit maximum the
  doubled byte count gives the same power-of-two map. `setHyperlink` then hits `unreachable` in safe
  builds; release builds drop the link (3 of 90000 in vtmux's test). The reflow now grows the map
  until the link fits; past the maximum it moves the row to a new page, which `reflowRow` already does.
- `state_fuzz` gains the `cells`, `cursor` and `capacity` kinds, `amp PAGES BYTES` (the review's
  amplification case) and `DUMP=1` (per-page cell, link, style and background counts and capacity).
  Its base now has background-only cells, with tabs across some.

## Tests

Each test below fails before its change and passes after:
`background_cells_carry_no_text_attributes` (style, link, wide, spacer head, protected and prompt on
both background kinds), `cursor_semantic_content_is_as_ghostty_sets_it`,
`capacity_hints_are_bounded_by_the_page_s_content` (including the review's one-row page),
`a_capacity_grown_for_what_was_erased_exports_within_its_bound` (graphemes double a page's capacity,
then EL erases them: the export's hint is 8192 again; without the clamp the state did not import), and
`a_reflow_keeps_the_links_of_pages_full_of_them`, plus the Zig test `Terminal: reflow keeps the links
of a page full of them (vtmux)`. Two guards pass both before and after:
`background_cells_round_trip_wherever_ghostty_makes_them` (ACS, an OSC 133 prompt, tabs, IL and ICH
over background cells) and the existing round trips, which now carry clamped hints.

The default test build links Ghostty ReleaseFast (`opt-level = 3` for the sys crate), where the
asserts above are not checked, so these tests prove the refusal. The crashes are proven by the fuzz
under ReleaseSafe.

## Fuzz (ReleaseSafe Ghostty, `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe`)

Seeds 0-1999 of every kind, plus the review's seed numbers 900218, 1700162, 1400227, 1300072 and
1800070 for each kind. The review's `rvprobe` is not in the tree, and its seeds name its own mutations,
so the cases it describes are pinned by the tests above. A driver restarts the fuzz after the seed that
crashed.

| kind | seeds | imported and used | refused | Zig crash | Rust panic |
|---|---|---|---|---|---|
| rows | 2005 | 101 | 1904 | 0 | 0 |
| state | 2005 | 478 | 1527 | 0 | 0 |
| links | 2005 | 468 | 1537 | 0 | 0 |
| styles | 2005 | 553 | 1452 | 0 | 0 |
| graphemes | 2005 | 416 | 1589 | 0 | 0 |
| cells | 2005 (+8000) | 705 (+2899) | 1300 (+5101) | 0 | 0 |
| cursor | 2005 (+8000) | 1570 (+6299) | 435 (+1701) | 0 | 0 |
| capacity | 2005 (+8000) | 326 (+1331) | 1679 (+6669) | 0 | 0 |
| blob | 2005 | 18 | 1987 | 0 | 0 |
| blob-full | 2005 | 19 | 1986 | 0 | 0 |

The (+8000) rows are seeds 2000-9999 of the three new kinds. The same fuzzer against the base tree
(0f8efb3) crashed 388 times in `cells` (null unwraps, `unreachable`, a switch on a corrupt value),
174 times in `cursor` (`unreachable`) and 63 times in `styles` (every cell of a page given one style,
background cells included). Before patch 0018, `cells` seed 128 crashed at `PageList.zig:2654`. That was
the live-input crash described above.

Memory: `state_fuzz amp 4 1073741824` (four pages each asking for 1 GiB of grapheme and string bytes)
peaked at 27.9 GB RSS in 13 s on the base tree and is refused at 6 MB on this one. The whole
`capacity` kind, seeds 0-1999 in one process, peaks at 60 MB.

## Gates

See the task report.
