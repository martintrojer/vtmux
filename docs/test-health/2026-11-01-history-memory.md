# History memory 2026-11-01 (hist_retain: baseline and after)

Spec: `docs/specs/2026-10-31-history-memory.md`, checklist items 1-2.
Command: `mise exec -- bash scripts/bench-history-memory.sh` (IDLE=10), release builds.
Host: Fedora 44, Linux 7.2.11 x86_64, 16 cores; reference tmux 3.8 from `cargo xtask ref-tmux`.
Baseline: main a3b729c (byte budget sized as if every pane were at least 256 columns wide).
After: hist_retain on a3b729c (Ghostty line limit `history-limit` + one page at the pane's width, byte limit off).

`retained` is the history rows Ghostty keeps (`scrollback_rows`, the `history_retention` example fed the
same bytes); `hist_size` is `#{history_size}` from the running server.

| scenario | binary | size | limit | RSS MB | hist_size | retained | output s |
|---|---|---|---|---|---|---|---|
| spec (150k log lines) | tmux 3.8 | 120x40 | 100000 | 66.1 | 99962 | - | 0.45 |
| | vtmux baseline | | | 148.0 | 99962 | 149962 | 0.36 |
| | vtmux after | | | 101.5 | 99962 | 100039 | 0.35 |
| narrow (12k lines, 70 truecolour cells each) | tmux 3.8 | 80x24 | 10000 | 25.5 | 9978 | - | 0.78 |
| | vtmux baseline | | | 18.5 | **5620** | 5620 | 0.34 |
| | vtmux after | | | 27.1 | 9978 | 10244 | 0.34 |
| wide (12k lines of 450 chars) | tmux 3.8 | 500x24 | 10000 | 28.9 | 9978 | - | 0.18 |
| | vtmux baseline | | | 53.2 | 9978 | 11978 | 0.13 |
| | vtmux after | | | 45.9 | 9978 | 10025 | 0.13 |

Findings:
- Criterion 2 holds: retained rows are within one page of `history-limit` (page = 578 rows at 80 columns,
  387 at 120, 93 at 500), for plain, styled and wide output. Unit tests pin it across resizes
  (`retained_rows_track_history_limit*` in `crates/vtmux-vt/src/term.rs`).
- The baseline's byte budget broke parity on styled output: 5620 rows where tmux has 9978 (the refuter's
  40-style probe gave 9664). Ghostty's line limit doesn't depend on styling; `history_limit_retention`
  (`crates/vtmux/tests/cli.rs`) compares `history_size` and `capture-pane -S -` with the reference.
- Criterion 1 (RSS <= tmux) still fails: 101.5 MB vs 66.1 MB on the spec scenario. Ghostty stores 8 bytes per
  cell for every column of every row; tmux stores only the used cells. That is compression's job
  (task `hist_compress`).
- The styled pane now costs vtmux slightly more than the baseline (27.1 vs 18.5 MB) because it keeps the
  rows tmux keeps instead of pruning below them.

## Re-run after review round 2 (hist_retain fixes)

Fixes: resize under the alternate screen keeps the primary history (finding
`high_resize_under_the_alternate_screen`); vendored ghostty patch
`0001-keep-one-free-page-resident` stops the line limit decommitting and
re-faulting a page every time output fills one (finding
`medium_1m_line_plain_flood_1_9x_slower`).
Command: `BASE=<a3b729c build> mise exec -- bash scripts/bench-history-memory.sh`, then
`BASE=... RUNS=3 IDLE=2 ... flood flood100k` (fastest of 3). Same host. `base` is a3b729c.

| scenario | binary | size | limit | RSS MB | hist_size | retained | output s |
|---|---|---|---|---|---|---|---|
| spec | tmux 3.8 | 120x40 | 100000 | 66.1 | 99962 | - | 0.51 |
| | vtmux base | | | 148.0 | 99962 | - | 0.30 |
| | vtmux | | | 104.8 | 99962 | 100039 | 0.30 |
| narrow | tmux 3.8 | 80x24 | 10000 | 25.6 | 9978 | - | 0.78 |
| | vtmux base | | | 18.5 | **5620** | - | 0.28 |
| | vtmux | | | 27.5 | 9978 | 10244 | 0.34 |
| wide | tmux 3.8 | 500x24 | 10000 | 29.0 | 9978 | - | 0.18 |
| | vtmux base | | | 53.5 | 9978 | - | 0.13 |
| | vtmux | | | 46.9 | 9978 | 10025 | 0.13 |
| flood (1M lines) | tmux 3.8 | 120x40 | 2000 | 5.6 | 1962 | - | 1.80 |
| | vtmux base | | | 12.8 | 1962 | - | 0.77 |
| | vtmux | | | 10.2 | 1962 | 2276 | 0.77 |
| flood100k (1M lines) | tmux 3.8 | 120x40 | 100000 | 66.1 | 99962 | - | 1.92 |
| | vtmux base | | | 230.0 | 99962 | - | 1.03 |
| | vtmux | | | 108.0 | 99962 | 100187 | 0.87 |

Before the patch (86ec851) the flood took 1.48 s at limit 2000 and 1.62 s at 100000
(reviewer-60). In-process (`PaneTerm::feed` of the same bytes, 120x40):

| limit | a3b729c | 86ec851 | now |
|---|---|---|---|
| 10 | 1.51 s / 34k faults | 2.10 s / 506k | 1.31 s / 34k |
| 100 | 0.46 s / 34k | 1.17 s / 506k | 0.46 s / 34k |
| 2000 | 0.26 s / 36k | 0.94 s / 506k | 0.27 s / 35k |
| 100000 | 0.44 s / 143k | 0.94 s / 507k | 0.38 s / 83k |

Alternate screen + resize (reviewer-60's matrix: limit 2000, 6 kinds x 80/200/500 cols, resize
to cols/2 x20 then cols+10 x30 on the alternate screen, then `?1049l`): `history_size` after
`?1049l` now equals tmux's for plain, truecolour and combining (e.g. 1974; 86ec851 had 1112-1253),
and `capture-pane -S -` is byte-identical to tmux for those. CJK-wide, ZWJ and longwrap match
tmux's count within 1 (or, for wide/ZWJ, differ as base did: Ghostty reflow/cluster divergences
P2/P3). Pinned by `resize_under_the_alternate_screen_keeps_history` (vt) and
`history_resize_under_the_alternate_screen` (cli, against tmux 3.8).
