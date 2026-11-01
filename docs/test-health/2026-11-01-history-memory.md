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
