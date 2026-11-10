# Capturing all of a long history, 2026-11-09

Task `perf_capture_full_history`. Host: the Linux server of *Method* in `PERFORMANCE.md` (96 cpus), shared with other agents; load 12-13 during the runs below. Release builds of vtmux; tmux 3.8 from `cargo xtask ref-tmux`. `base` is main `457a169` (`/tmp/w142/vtmux-base`), `after` the branch at `6ca43ff` (`/tmp/w142/vtmux-after`).

Commits: `1dcbed4` (libghostty-vt: `row_cells` writes into the caller's buffer), `0e0e299` (lines of plain text from Ghostty's cells), `de3eb28` (styled text without `-e`, and the general path's buffers), `6ca43ff` (`-e` and `-N` with styles).

## The cause

`perf record -g` of the base server during `capture-pane -p -S -` over 1M plain lines: 63% in `read_cells`, 10% in `capture_from` (with `emit_row` inlined), most of the rest `malloc`, `free` and `memcpy`. Every line was built into a `Vec<RowCell>` (text, style, link: about 64 bytes a cell, 120 cells a line) and then written out through two more buffers. `-M` (copy mode): `read_cells`, then `RowCell::to_cell` into a `Row` the copy kept for every line, then back into `RowCell`s for the capture; 30% `capture_from`, 23% `read_cells`, 10% `to_cell`, and page faults for the kept rows.

What changed:

- A line whose cells are all plain (one codepoint or none, one column, no tab, line drawing or link) is read as one codepoint per cell straight from Ghostty's cell bits and written by `emit_plain`. Which cells are plain depends on the flags: without `-e`, `-N`, `-F` and `-H` a cell's style and link change nothing written, so a coloured line is plain too; under `-F` only ASCII is (a character of more than one byte flags the line `X`).
- Under `-e` or `-N` (without `-F` or `-H`), such a line is read with its styles, one lookup per run of a style id, and `emit_styled` writes each run's SGR once.
- `-M` reads a line copy mode has not read from its entry copy and keeps no row for it.
- Any other line (wide characters, clusters, tabs, line drawing, links, or `-F`/`-H` over styles) goes the general way, which now keeps its buffers from line to line, writes into the capture's buffer, skips `sgr_code` for a cell of the last cell's style and looks a run of one style up once.
- `GridRef::row_cells` no longer allocates a 256-cell buffer per row.

Output is unchanged. Evidence: `plain_lines_capture_as_cells_do` (random content, every combination of `-e -J -C -N -T` with each of `-L -I -F -H`, two ranges, resident, compressed and alternate-screen history, `-a`, `-M`: plain paths against the general one), and a CLI probe that feeds the same random content to a base and an after server and compares every flag combination, three ranges, `-a` and `-M`: 10 seeds x 1760 captures, 0 differ (the `-I` times normalised, as the two servers' lines can scroll in a second apart).

## Probe

`scripts/bench-history-read.py [--styled] <binary> <label> 100000 1000000`, three rounds of base, after and tmux in turn, plain at 06:56-06:59 UTC and `--styled` at 06:59-07:04. One 120x40 pane, `history-limit` N+1000, N log lines of about 100 characters, history uncompressed, one 120x40 client attached. `--styled` lines have a green timestamp, a bold blue level, a 256-colour status and an `é`. Wall time of each client command in ms (it includes sending about 100 MB to the client at 1M lines); the three runs, median in bold.

### Plain lines, 1M (history_size 999963)

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `capture-pane -p -S -` | 835, **840**, 873 | 3523, **3526**, 3560 | 375, **385**, 391 | 0.46x |
| `capture-pane -p -e -S -` | 4540, **4601**, 4654 | 13173, **13255**, 13353 | 922, **931**, 936 | 0.20x |
| `capture-pane -p -M -S -` (copy mode) | 831, **883**, 983 | 7386, **7411**, 7466 | 369, **371**, 438 | 0.42x |

### Plain lines, 100k (history_size 99963)

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `capture-pane -p -S -` | 84, **87**, 89 | 345, **356**, 367 | 43, **44**, 46 | 0.51x |
| `capture-pane -p -e -S -` | 445, **448**, 457 | 1339, **1341**, 1455 | 96, **99**, 109 | 0.22x |
| `capture-pane -p -M -S -` | 83, **83**, 83 | 750, **755**, 762 | 42, **42**, 44 | 0.51x |

### Styled lines, 1M

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `capture-pane -p -S -` | 869, **896**, 1087 | 4103, **4161**, 4232 | 431, **440**, 451 | 0.49x |
| `capture-pane -p -e -S -` | 5443, **5594**, 5775 | 14901, **15020**, 15206 | 2979, **3026**, 3090 | 0.54x |
| `capture-pane -p -M -S -` | 829, **838**, 1067 | 8018, **8054**, 8118 | 442, **444**, 473 | 0.53x |

### Styled lines, 100k

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `capture-pane -p -S -` | 89, **90**, 96 | 410, **415**, 450 | 46, **48**, 48 | 0.53x |
| `capture-pane -p -e -S -` | 543, **544**, 561 | 1494, **1517**, 1570 | 302, **303**, 304 | 0.56x |
| `capture-pane -p -M -S -` | 82, **83**, 85 | 804, **809**, 819 | 41, **42**, 43 | 0.51x |

The other columns of the same runs did not change beyond noise (the change touches no other path): plain 1M `search-backward` base 681-775, after 644-667, tmux 1651-1953; `copy-mode` 290-300, 295-347, 235-242; `alt-on` 3979-4073, 3999-4125, 3-4.

## Seen on the way, not done here

With styled lines, copy mode is well behind tmux and this task does not touch it: at 1M lines `search-backward` takes 5966-6089 ms against tmux's 1746-1765 (3.4x) and `copy-mode` 749-774 ms against 276-282 (2.7x). The search reads a line as text only when every cell is a narrow ASCII character of the default style (`ScreenCopy::ascii_text`); any styled cell sends the line to `used_row`, which builds its cells. The plain-text reading here (any style taken as plain where it changes nothing) would apply to the search the same way.

## Steps of the work, 1M plain lines

Single probe runs (`/tmp/w142/probe.py`, the bench's setup without a client, three captures each), load 3-5:

| build | `-S -` | `-M` |
|---|---|---|
| base | 3847-3894 | 7335 (8063 the first, 2496 again: the copy kept the rows) |
| plain lines from Ghostty's cells | 451-468 | 386-424 (5800 before `entry_row` was wired) |
| + `row_cells` into the caller's buffer | 380-398 | 302-356 |
| + the pane's sizes once per capture | 270-337 | 302-336 |

## Gates

The PERF gate (`bench.py agent_panes`, `key_echo`) was not run: the change touches no render, compression, sync output or pane I/O code. `row_cells` and `read_cells` are read by captures, copy mode (the rows it draws and searches; a row there costs no more than before), formats that read a row, the alternate screen's history copy and the resize trim of blank rows; the compositor reads panes through Ghostty's render state (`snapshot.rs`), not these.
