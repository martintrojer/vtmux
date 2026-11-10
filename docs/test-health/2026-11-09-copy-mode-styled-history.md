# Copy mode over a coloured history, 2026-11-09

Task `perf_search_styled_history`. Host: the Linux server of *Method* in `PERFORMANCE.md` (96 cpus), shared with other agents; load 3-6 during the runs below. Release builds of vtmux; tmux 3.8 from `cargo xtask ref-tmux`. `base` is main `636664b` (`/tmp/w146/vtmux-base`), `after` the branch at `5594f05` (`/tmp/w146/vtmux-after`).

Commits: `8b0656e` (libghostty-vt: a clone copies a whole page as its memory, Ghostty patch 0035), `5594f05` (copy: search reads lines of styled characters past ASCII as text).

## The cause

`perf record -g` of the base server, `copy-mode` over 1M styled lines (`scripts/bench-history-read.py --styled`'s: a green timestamp, a bold blue level, a 256-colour status, an `é`): 43% in Ghostty's `Page.clonePartialRowFrom` and 19% in `StyleSet.addWithId`, both under `ghostty_terminal_clone_screen`, most of the rest page faults. `PageList.clone` copies each page row by row (`Page.cloneFrom`), and a row with styles cell by cell, adding each cell's style to the new page's set. A plain row is one `memcpy`.

The search: `ScreenCopy::ascii_text` read a row as text only when every cell was ASCII. Each styled line ends in an `é`, so every line went through `used_row` (a `RowCell` per cell, then a `Cell`) and the cell-by-cell compare.

What changed:

- Patch 0035: a resident page the clone takes whole goes as a copy of its memory: the row headers and cells whole, the style set, the grapheme space and the hyperlink and string space only when they hold anything (the new page is zero, which is an empty set). A plain `cloneBuf` of the whole page committed memory the source never touched (RSS in copy mode over 1M lines 1961 -> 2022 MB); skipping zero memory pages kept the RSS but read that memory, a zero-page fault each, and made the first entry 20-30 ms slower. With the sets copied by what they hold: 1961 MB plain, 1971 MB styled, and no slower first entry.
- `ScreenCopy::text` reads a row as text when every cell is one character of one column or a blank, of any style (`CellBits::plain`, as capture's plain lines). The search uses the text for such a line whether ASCII or not: a plain pattern of such characters is matched in the UTF-8 text from a cell's first byte, a regular expression runs over the text (the same buffer `window_copy_search_lr_regex` builds from the cells), and a line whose cells cannot hold the pattern is passed unread.
- `plain_codepoints`, shared by capture's plain lines and `text`, takes a row of codepoints past ASCII in two branch-free passes. The first version of `text` called `CellBits::plain` from a second place, and `capture-pane -S -` over 1M styled lines went 413-446 -> 438-471 ms: `perf annotate` showed capture's per-cell loop (`plain_cells`) as the same instructions at other addresses, its back edge now 27% of the function's samples. With the shared function it is 414-435 again.

## Probe

`scripts/bench-history-read.py [--styled] <binary> <label> 100000 1000000`, three rounds of base, after and tmux in turn (`/tmp/w146/bench.sh`), 10:50-10:55 UTC, load 2.8-6.0. One 120x40 pane, `history-limit` N+1000, N log lines of about 100 characters, history uncompressed, one 120x40 client attached. Wall time of each client command in ms; the three runs, median in bold.

### Styled lines, 1M (history_size 999963)

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `copy-mode` | 283, **285**, 296 | 714, **726**, 732 | 320, **335**, 345 | 1.18x |
| `search-backward` (absent string) | 1763, **1794**, 1804 | 6054, **6067**, 6086 | 638, **646**, 649 | 0.36x |
| `search-forward` | 1762, **2065**, 2092 | 5965, **6060**, 6322 | 495, **498**, 501 | 0.24x |
| `capture-pane -p -S -` | 862, **926**, 928 | 418, **439**, 444 | 422, **430**, 434 | 0.46x |
| `capture-pane -p -e -S -` | 5399, **5490**, 5531 | 3018, **3033**, 3058 | 2973, **3076**, 3101 | 0.56x |
| `capture-pane -p -M -S -` | 826, **967**, 991 | 449, **451**, 455 | 437, **441**, 442 | 0.46x |
| alternate screen up | 3, **4**, 4 | 4131, **4153**, 4201 | 4160, **4182**, 4198 | - |

### Styled lines, 100k

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `copy-mode` | 30, **30**, 32 | 73, **74**, 74 | 32, **33**, 33 | 1.10x |
| `search-backward` | 176, **177**, 177 | 605, **611**, 615 | 65, **66**, 66 | 0.37x |
| `search-forward` | 176, **177**, 178 | 591, **591**, 600 | 51, **52**, 52 | 0.29x |
| `capture-pane -p -S -` | 89, **90**, 91 | 46, **46**, 47 | 47, **48**, 51 | 0.53x |

### Plain lines, 1M

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `copy-mode` | 225, **236**, 243 | 273, **274**, 307 | 266, **281**, 289 | 1.19x |
| `search-backward` | 1636, **1651**, 1663 | 631, **640**, 646 | 553, **554**, 589 | 0.34x |
| `search-forward` | 1648, **1650**, 1651 | 503, **505**, 512 | 394, **403**, 413 | 0.24x |
| `capture-pane -p -S -` | 853, **865**, 988 | 375, **378**, 379 | 372, **388**, 391 | 0.45x |
| `capture-pane -p -M -S -` | 829, **849**, 995 | 360, **371**, 389 | 368, **377**, 388 | 0.44x |

### Plain lines, 100k

| command | tmux 3.8 | base | after | after / tmux |
|---|---|---|---|---|
| `copy-mode` | 26, **26**, 26 | 33, **33**, 34 | 34, **34**, 34 | 1.31x |
| `search-backward` | 166, **166**, 180 | 65, **67**, 73 | 57, **57**, 58 | 0.34x |
| `search-forward` | 167, **171**, 181 | 52, **60**, 62 | 42, **42**, 43 | 0.25x |

`history-top`, `goto-line` to the middle and `history-bottom` took 2-4 ms in every column.

### Copy-mode entry alone

`/tmp/w146/probe.py` (the bench's setup without a client; `copy-mode`, then `cancel`, 4 times a server), 1M lines, three servers each, load about 3, ms:

| build | plain | styled |
|---|---|---|
| base | 290-325 | 701-727 |
| after | 270-336 | 274-303 |

RSS of the server in copy mode (`/tmp/w146/rss.py`): base 1961 MB plain and styled; after 1961 MB plain, 1971 MB styled; 947 MB before entry in all.

## Counts over 100k styled lines

`/tmp/w146/count.py`: `copy-mode`, `send -X search-backward <pattern>`, then `#{search_count}#{?search_count_partial,+,}`; the command's wall time.

| pattern | tmux 3.8 | base | after |
|---|---|---|---|
| `INFO` (every line) | `1000+` 0.204 s | `1000+` 0.203 s | 100000 0.044 s |
| `status=200 bytes` (every line) | 100000 0.194 s | `1000+` 0.202 s | 100000 0.046 s |
| `items/[0-9]+5 ` | 9999 0.146 s | `1000+` 0.202 s | 9999 0.114 s |
| `é$` | 0 0.077 s | 0 0.522 s | 0 0.071 s |
| `zzznotfound` | 0 0.183 s | 0 0.600 s | 0 0.065 s |

Base ran out tmux's 200 ms (`WINDOW_COPY_SEARCH_ALL_TIMEOUT`) where tmux counts exactly. `INFO` runs out tmux's time too; vtmux counts all of it, as it does on plain lines (`compat/DIVERGENCES.md`, D-034).

## Results unchanged

- `/tmp/w146/searchdiff.py <base> <after> <seed> 2000`: the same random styled content (colours, links, `é`, `ü`, `─`, `•`, wide, clusters, tabs, line drawing, lines that wrap) in a pane of each binary, 34 patterns (19 plain, 15 regular expressions) x up and down x emacs and vi keys, each search then `search-again`, `search-again`, `search-reverse`, `search-again`; after each step `#{search_count}`, `#{search_count_partial}`, the cursor, `#{scroll_position}` and `#{search_match}`. Seeds 1-9 (widths 23-70): 680 steps each, 0 differ.
- `/tmp/w142/diffprobe.py <base> <after> <seed> 1500` (from `perf_capture_full_history`): every `capture-pane` flag combination, three ranges, `-a` and `-M` in copy mode; seeds 1-6, 1760 captures each, 0 differ.
- Unit tests: `styled_lines_search_from_text_as_from_cells` (the text paths against reading every line as cells), `a_styled_history_is_searched_without_reading_cells`, `a_copy_row_as_text_is_its_cells`, `a_copy_of_whole_pages_reads_as_the_pane_did`, and Ghostty's new `PageList clone copies whole pages with their styles and graphemes (vtmux)`.

## Gates

The PERF gate (`bench.py agent_panes`, `key_echo`) was not run: the change touches no render, compression, sync output or pane I/O code. Ghostty's `PageList.clone` is reached only through `ghostty_terminal_clone_screen`, which only copy mode's entry calls; `CellBits::plain` and `plain_codepoints` are read by captures and copy mode, not by the compositor (Ghostty's render state, `snapshot.rs`).
