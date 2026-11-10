# The alternate screen over a long history, 2026-11-10

Task `perf_alt_screen_history`. Host: the Linux server of *Method* in `PERFORMANCE.md` (96 cpus, Linux 6.16), shared with other agents; load 12-38 during the runs below. Release builds of vtmux; tmux 3.8 from `cargo xtask ref-tmux`. `base` is main `a3f8b8d` (`/tmp/w181-vtmux-base`), `after` the branch (`/tmp/w181-vtmux-new`).

## The cause

`?1049h` ran `PaneTerm::freeze_history`: every row of the primary history copied out of Ghostty into a `FrozenRow` (a `RowCell` per cell), because Ghostty's C API reads only the active screen and tmux goes on showing the primary history while an alternate screen is up (screen.c `screen_alternate_on` saves only the visible rows). 4.1-4.4 s over 1M lines here, 0.43-0.47 s over 100k; tmux 3-4 ms.

Ghostty patch 0028 already leaves the primary screen as it was while the alternate one is up (no resize, no reflow, no new history: tmux's `window_pane_resize` resizes only the alternate grid). Patch 0038 adds a grid ref, a history row count and a clone for either screen, active or not. vtmux now reads the primary history in place: under the alternate screen, tmux's history row `y` is the primary's screen row `retained - hsize + y`, looked up as a `PRIMARY` row (`capture.rs`), and stepped to row by row as any other (patch 0033). What still changes the primary under the alternate screen is handled as before: `history-limit` trims `hsize` from the oldest end, ED3 and `clear-history` set it to 0 until the primary is back, and a lowered byte budget, which Ghostty applies to the primary at once, now trims `hsize` to what the primary keeps. Copy mode under the alternate screen clones the primary with patch 0035's whole-page clone, as copy mode on the primary does (tmux's `window_copy_clone_screen` copies the grid that holds that history).

A live upgrade onto the same patch stack no longer writes the copy (`AltEntry::history` empty, `has_history` false): the primary screen's pages carry the history. A snapshot from an older writer with the copy imports as before, the copy unread. Rows still carry it for the build before (round 2, below).

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

## Round 2: a downgrade under the alternate screen

Review finding: with the copy gone from every snapshot, a downgrade onto main with vim up over 300k lines showed `history_size` 0 and a 30-row capture (tmux 300778), and main then ignored a `history-limit` lowered under the alternate screen (after `?1049l` 300778 against tmux's 99999, 50 lines later 90828 against 90049). The live-upgrade spec writes state an ordinary session produces in a new form beside what the build before reads.

Fix: a stream of rows (a checkpoint, an upgrade onto another patch stack, the only way onto a build without patch 0038) carries `AltEntry::history` again, with `has_history`: the newest `hsize` rows of the primary history at the width the primary was left at, as main's `?1049h` copied them. It is built at snapshot time from the primary's exported pages, so `?1049h` and a same-stack upgrade never pay for it: whole pages are the primary's own `HistoryPage`s (cloned, the same chunk in a checkpoint), only the page the cut falls in is encoded again. The reader with patch 0038 ignores it. `Snapshot::unreadable` (the dry run's and the resume's page check) reads a page equal to one it read already only once, so the copy's pages are not inflated and hashed twice more.

Upgrade cost, `scripts/bench.py upgrade` with the new `UPGRADE_LIMIT` and `-alt` states (`?1049h` written after the history): one 200x49 pane, `history-limit` 1.1M, 1M lines of 100 columns, one attached and two control clients, fresh history, median of 3, load 18-21, release build of the branch:

| upgrade | without the alternate screen | with it up | snapshot |
|---|---|---|---|
| onto the same binary | 1645 ms | 1769 ms | 110.6 MB both |
| onto another patch stack (rows) | 13691 ms | 14173 ms | 56.6 -> 113.0 MB |

Before the check reads a page once, the rows upgrade with the alternate screen up took 17139 ms (median of 3, same host): the copy's 1M rows were inflated, hashed and decoded by the dry run and again by the resume. The stream still holds the copy's bytes twice (inline pages, the format has no reference to another page), 56 MB more at 1M lines; the build before can read nothing else.

The dry run's limit (`dry_run_limit`: 3 s and a second per 100k rows it imports) now counts the copy too, which the build before imports: without it a downgrade onto main with vim up over 1M lines was refused, `--vtmux-check-snapshot took over 12 s` (the server serves on, nothing lost). With it the same downgrade (`/tmp/rvw185/down.sh`, 200x50, release builds) goes through in 33.5 s, `history_size` 999951 and a 999951-line capture on main. Main itself refuses its own upgrade with vim up over 1M lines (`bench.py upgrade`, `fresh-alt`: the dry run took over 3 s, `fresh-alt-rows`: over 12 s), as its limit does not count its copy either.

Downgrade, in the new test and by hand (`/tmp/rvw185/probe*.sh`; tmux 3.8 and both builds without an upgrade agree on every number): 300k lines at 80x24, `?1049h`, `history-limit` 199999, upgrade onto main `a3f8b8d`, there `history-limit` 99999, `?1049l` and 50 lines, upgrade back. Before the fix the test stops at the first check on main, `history_size` 0.

| step | tmux 3.8 | after the fix, through both upgrades |
|---|---|---|
| on main under the alternate screen: `history_size`, capture `-S - -E -1` | 199999, lines 99979-299977 | the same |
| on main after the lowering | 99999, lines 199979-299977 | the same |
| after `?1049l` and 50 lines | 90050, lines 209978-27 | the same, on main and back on the branch |

Tests: `snapshot_term::an_older_reader_gets_its_copy_of_the_hidden_history` (the copy's rows are tmux's history, whole pages the primary's, none with the same stack, and unreflowed after a narrowing; fails before the fix: `has_history` false), `an_ordinary_attached_client_requires_no_feature` (rows carry the copy, no feature required), `resume::tests::the_dry_run_limit_grows_with_the_rows_it_imports` (the copy counts; fails before: 15 s, not 18 s), `vtmux-snapshot` `a_page_equal_to_one_read_is_read_once` (a page under an equal page's name with other bytes is still refused), and `upgrade::a_downgrade_under_an_editor_keeps_the_history_it_hides`, ignored without `VTMUX_PRE_0038_BINARY` (a build of main): fails before the fix (`history_size` "0 1" on main), passes 5x under 48 busy loops (flock `/tmp/vtmux-stress.lock`) and with a dash-like `/bin/sh`.
