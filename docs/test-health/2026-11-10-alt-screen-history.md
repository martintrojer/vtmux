# The alternate screen over a long history, 2026-11-10

Task `perf_alt_screen_history`. Host: the Linux server of *Method* in `PERFORMANCE.md` (96 cpus, Linux 6.16), shared with other agents; load 12-38 during the runs below. Release builds of vtmux; tmux 3.8 from `cargo xtask ref-tmux`. `base` is main `a3f8b8d` (`/tmp/w181-vtmux-base`), `after` the branch (`/tmp/w181-vtmux-new`).

## The cause

`?1049h` ran `PaneTerm::freeze_history`: every row of the primary history copied out of Ghostty into a `FrozenRow` (a `RowCell` per cell), because Ghostty's C API reads only the active screen and tmux goes on showing the primary history while an alternate screen is up (screen.c `screen_alternate_on` saves only the visible rows). 4.1-4.4 s over 1M lines here, 0.43-0.47 s over 100k; tmux 3-4 ms.

Ghostty patch 0028 already leaves the primary screen as it was while the alternate one is up (no resize, no reflow, no new history: tmux's `window_pane_resize` resizes only the alternate grid). Patch 0038 adds a grid ref, a history row count and a clone for either screen, active or not. vtmux now reads the primary history in place: under the alternate screen, tmux's history row `y` is the primary's screen row `retained - hsize + y`, looked up as a `PRIMARY` row (`capture.rs`), and stepped to row by row as any other (patch 0033). What still changes the primary under the alternate screen is handled as before: `history-limit` trims `hsize` from the oldest end, ED3 and `clear-history` set it to 0 until the primary is back, and a lowered byte budget, which Ghostty applies to the primary at once, now trims `hsize` to what the primary keeps. Copy mode under the alternate screen clones the primary with patch 0035's whole-page clone, as copy mode on the primary does (tmux's `window_copy_clone_screen` copies the grid that holds that history).

A live upgrade no longer writes the copy (`AltEntry::history` empty, `has_history` false): the primary screen's pages carry the history. A snapshot from an older writer with the copy imports as before, the copy unread.

## Probe

`scripts/bench-history-read.py [--styled] <binary> <label> 100000 1000000`, three rounds of base and after in turn (`/tmp/w181-bench.sh`, 00:50-00:52 local), then tmux once. One 120x40 pane, `history-limit` N+1000, N log lines of about 100 characters, history uncompressed, one 120x40 client attached. Wall time in ms, the three runs, median in bold. "alternate screen up" is `?1049h` written by the pane until `#{alternate_on}` is 1.

| alternate screen up | tmux 3.8 | base | after |
|---|---|---|---|
| 1M plain lines | 4 | 4119, **4212**, 4309 | 3, **3**, 3 |
| 1M styled lines | 3 | 4237, **4313**, 4354 | 3, **3**, 3 |
| 100k plain lines | 3 | 429, **433**, 457 | 3, **3**, 3 |
| 100k styled lines | not run | 434, **439**, 469 | 3, **3**, 3 |

The other rows of the probe (captures, copy mode, searches, jumps) did not move beyond noise: 1M plain `capture-pane -p -S -` 376-405 against 375-391, `copy-mode` 255-270 against 251-289, `search-backward` 558-601 against 574-602.

Copy mode and a search once the alternate screen is up (`/tmp/w181-p/probe2.py`: the probe's pane, then `copy-mode` and `send -X search-backward <absent>`), 1M plain lines, three runs:

| | tmux 3.8 | base | after |
|---|---|---|---|
| `copy-mode` under the alternate screen | 247 | 23, **24**, 25 | 265, **288**, 294 |
| `search-backward` under it | 1741 | 4773, **4846**, 5109 | 560, **573**, 613 |

Entering copy mode there now costs what it costs on the primary screen (the clone), where the base had paid for it when the screen came up; a search reads the history as text instead of building cells for every row.

In process (an ad-hoc example: `PaneTerm` alone, 100k lines at 120 columns, release): `?1049h` 167 µs.

## Tests

- Rust: `capture::tests::alternate_screen_reads_the_primary_history_in_place` (at most a screen's rows read as `?1049h` comes up over 20k lines; the history reads the same from the pane, its copy, a capture and by history id), `term::tests::history_budget_lowered_under_the_alternate_screen`, `term::tests::osc133_marks_follow_their_lines` (ED3's marks), `capture::tests::a_copy_row_as_text_is_its_cells` (the history under the alternate screen reads as text), `snapshot_term::an_old_writers_copy_of_the_hidden_history_is_not_read`, `an_ordinary_attached_client_requires_no_feature` (an editor over history).
- Oracle: new `alt_screen_copy_mode_history` (3000 lines under the alternate screen: captures before and after a narrowing, copy mode's top and a search, a trim, the way back), replayed with `--upgrade-every 3`; the existing alternate-screen scenarios (`alt_*`, `history_limit_live_alt`, `capture_flags`, `decawm_resize_alt`) pass.
- Zig: `vtmux_screen_grid_ref reads the primary history under the alternate screen (vtmux)`.
