# Divergences from tmux 3.8

Where vtmux behaves differently from tmux 3.8, and why; everything not listed matches tmux, quirks included. Each entry has a number (D-NNN) that is never reused, and every entry carries its evidence. A fixed entry stays here, marked closed.

| Category | Meaning |
|---|---|
| Ghostty grid | vtmux's grid is Ghostty's, and here Ghostty's terminal behaviour was kept over tmux's. |
| tmux bug | tmux loses data, hangs or is unsafe; vtmux does not copy it. Entries say *Do not copy tmux bug*. |
| Deferred | Not done yet. vtmux does not claim to be compatible there. |
| Spec decision | A design decision chose this for drop-in use (plugins, version checks). |
| Extension | A terminal protocol tmux drops, behind a `vtmux-*` option that is off by default (decision D28). |

Status: **open** means it differs and will stay that way; **deferred** means not done yet; **extension** means opt-in; **closed** means fixed, and vtmux now does what tmux does.

## Summary

| ID | What differs for you | Category | Status |
|---|---|---|---|
| [D-001](#d-001) | `#{history_bytes}` is an estimate. It is off after line edits and resizes | Deferred | deferred |
| [D-002](#d-002) | Pathological regexes stop at a limit instead of running for seconds | Deferred | deferred |
| [D-003](#d-003) | `^` inside a repeated regex group follows POSIX, not a glibc bug | tmux bug | open |
| [D-004](#d-004) | Time formats use the C locale; odd `TZ` values and over-long output differ | Deferred, tmux bug | deferred |
| [D-005](#d-005) | A format loop nested in one of the same kind no longer skips or repeats items | tmux bug | open |
| [D-006](#d-006) | A pane with `history-limit 0` keeps nothing a narrowing pushes off the top | Deferred | deferred |
| [D-007](#d-007) | Terminals come from a built-in table of 2,663 names (ncurses 6.6's database), not terminfo | Deferred | deferred |
| [D-008](#d-008) | `respawn-window -k` on a zoomed window works; tmux crashes | tmux bug | open |
| [D-009](#d-009) | A tab or `\001` in a config error shown at attach is dropped | Deferred | deferred |
| [D-010](#d-010) | Copy mode redraws its whole screen, so no stale selection is left on screen | Deferred | deferred |
| [D-012](#d-012) | A new window or pane is named after its command, never `tmux` by chance | tmux bug | open |
| [D-013](#d-013) | Byte sequences that glibc decodes but that are not UTF-8 count as separate bytes | Deferred | deferred |
| [D-015](#d-015) | The cell after a pane prompt's input is always styled (tmux: depends on width) | tmux bug | open |
| [D-016](#d-016) | With `status off` and `status-position top`, the prompt's cursor is on the prompt | tmux bug | open |
| [D-017](#d-017) | `show-messages -T` lists features, not every terminfo capability | Deferred | deferred |
| [D-019](#d-019) | Bytes that are not UTF-8 follow glibc's rules on macOS too | Deferred | deferred |
| [D-020](#d-020) | Keys sent with `send-keys -K` reach the pane; tmux loses most of them | tmux bug | open |
| [D-022](#d-022) | `tmux` called from a pane, job or plugin reaches this server | Spec decision | open |
| [D-023](#d-023) | A dead pane's last line may stay apart from the remain-on-exit text | Ghostty grid | open |
| [D-024](#d-024) | A nested `source -` inside `source-file -` fails at once; tmux hangs | tmux bug | open |
| [D-026](#d-026) | `vtmux -V` prints vtmux's version; `#{version}` stays `3.8` | Spec decision | open |
| [D-027](#d-027) | `capture-pane -R`, `-N` and `-e` estimate line allocation; a few lines differ | Ghostty grid | open |
| [D-029](#d-029) | A control client stalled after its exit is dropped 10 s sooner | Deferred | deferred |
| [D-031](#d-031) | A few rare characters take Ghostty's width, not the host's `wcwidth` | Deferred | deferred |
| [D-033](#d-033) | Attach also asks the terminal for palette colours 0-15 (for pane blend) | Spec decision | open |
| [D-036](#d-036) | Widening a pane does not bring back cells cut when it was narrowed | Ghostty grid | open |
| [D-037](#d-037) | A command right after `kill-server` starts a new server instead of failing | tmux bug | open |
| [D-038](#d-038) | On the alternate screen, cells cut by narrowing are blank until the program redraws | Ghostty grid | open |
| [D-039](#d-039) | A blank cell left of a vertical border is written as a space | Ghostty grid | open |
| [D-041](#d-041) | A resize keeps a line's soft wrap where tmux loses it | tmux bug | open |
| [D-042](#d-042) | A character tmux hides behind a wide one after a resize stays visible | tmux bug | open |
| [D-043](#d-043) | `capture-pane -e` keeps a link that runs on from the line above | tmux bug | open |
| [D-044](#d-044) | Emoji joined in the last column don't scroll the screen or cover borders | tmux bug | open |
| [D-045](#d-045) | Nine emoji such as ☝ and ✌ are one column, as programs and terminals count them | tmux bug | open |
| [D-046](#d-046) | The history counters are empty; `display -v` shows only the result | Deferred | deferred |
| [D-047](#d-047) | Text partly under a floating pane keeps its visible end | tmux bug | open |
| [D-048](#d-048) | A control client closing stdin with output queued can't crash the server | tmux bug | open |
| [D-049](#d-049) | Copying a line wrapped before a wide character has no extra space | Deferred | deferred |
| [D-050](#d-050) | A narrowed popup reflows its lines; tmux cuts them | Deferred | deferred |
| [D-051](#d-051) | `destroy-unattached` checks every session; tmux skips some | tmux bug | open |
| [D-052](#d-052) | The format time limit counts CPU time, so a busy host doesn't truncate formats | tmux bug | open |
| [D-053](#d-053) | Spaces in a no-autowrap popup get another (invisible) foreground colour | Deferred | deferred |
| [D-054](#d-054) | A pane title over 2047 bytes is ignored | Deferred | deferred |
| [D-055](#d-055) | Passthrough is written after the frame of the same turn | Deferred | deferred |
| [D-056](#d-056) | On a full redraw each cell keeps its own underline colour | Deferred | deferred |
| [D-057](#d-057) | `suspend-client` leaves control clients alone; tmux cuts their output for good | tmux bug | open |
| [D-058](#d-058) | Curly and other styled underlines fall back to a plain underline; tmux drops them | tmux bug | open |
| [D-059](#d-059) | Two pane-palette corners: theme colours in mode styles, open popups | Deferred | deferred |
| [D-060](#d-060) | A client that is not UTF-8 still gets UTF-8 frames | Deferred | deferred |
| [D-061](#d-061) | A link restored by DECRC shows as a new link in `capture-pane -e` | Deferred | deferred |
| [D-062](#d-062) | DECSTBM under origin mode puts the cursor at the region's top | Deferred | deferred |
| [D-063](#d-063) | A zero-width character after an empty cell is dropped | Deferred | deferred |
| [D-064](#d-064) | With autowrap off, a combining mark joins the character it follows | tmux bug | open |
| [D-065](#d-065) | Kitty keyboard protocol in panes (`vtmux-kitty-keys`) | Extension | extension |
| [D-066](#d-066) | Desktop notifications reach Ghostty clients (`vtmux-notifications`) | Extension | extension |
| [D-067](#d-067) | In-band resize reports, mode 2048 (`vtmux-inband-resize`) | Extension | extension |
| [D-068](#d-068) | Link ids (`tmuxN`) are numbered when first drawn, not when parsed | Deferred | deferred |
| [D-069](#d-069) | A dead pane keeps its exit status when its old pid comes back for another process | tmux bug | open |
| [D-070](#d-070) | A failed `-t` with no session part and no client keeps the session in formats | Deferred | deferred |
| [D-071](#d-071) | Keys typed at a background `attach` stay with the foreground job | Spec decision | open |
| [D-072](#d-072) | Kitty graphics drawn as unicode placeholders for Ghostty clients (`vtmux-graphics`) | Extension | extension |
| [D-073](#d-073) | `-D` (run the server in the foreground) is refused | Deferred | deferred |
| [D-074](#d-074) | `#{c/f:theme...}` and `#{c/b:theme...}` give the slot's terminal colour, whatever the client's theme | Deferred | deferred |

Closed (vtmux now matches tmux):

| ID | Was | Status |
|---|---|---|
| [D-011](#d-011) | U+0085 and TAB cells | closed |
| [D-014](#d-014) | Window pixel sizes and pane PTY pixels | closed |
| [D-018](#d-018) | `clear-history -H` kept links | closed |
| [D-021](#d-021) | Typed M-BSpace under a keypad or BTab `backspace` | closed |
| [D-025](#d-025) | RIS erased history | closed |
| [D-028](#d-028) | The editor ran in a popup | closed |
| [D-030](#d-030) | DECAWM pending wrap | closed |
| [D-032](#d-032) | A scroll region below the top lost history | closed |
| [D-034](#d-034) | Search counts over long histories | closed |
| [D-035](#d-035) | Control output backpressure | closed |
| [D-040](#d-040) | A tab wider than the pane on reflow | closed |

## Entries

<a id="d-001"></a>
### D-001: `#{history_bytes}` and `#{history_all_bytes}` are a model of tmux's grid memory

- **Category:** Deferred: the edited-line allocation and the allocation across a width change. Nothing known depends on either.
- **Test:**
  - Oracle `history_bytes` (empty, scrolled, extended-cell and cursor-addressed panes) matches the reference.
  - So does the default-template `list-panes` step of `panes_split_resize` (`[history 0/2000, 120 bytes]`); cli.rs compares `list-panes` with `-F` elsewhere.
  - Oracle `history_bytes_resize` pins the width-change rows below: the rows that agree against the reference, the four that differ as `divergence` annotations.
- **tmux:**
  - `format_cb_history_bytes` (format.c:996-1020) sums tmux's own grid memory, per line: `cellsize * sizeof(struct grid_cell_entry)` (5 bytes), plus `extdsize * sizeof(struct grid_extd_entry)` (23), plus `sizeof(struct grid_line)` (40).
  - A line's `cellsize` is what `grid_expand_line` (grid.c:564-589) allocated over its life: a quarter, a half or the whole width, for the furthest cell a write reached.
- **vtmux:**
  - The grid is Ghostty's (D4), which has no per-line allocation, so `vtmux_vt::history_bytes` rebuilds the sum from the rows read back. It counts 40 bytes a line, and `cellsize` by the capture's allocation model (`tmux_cellsize`: one `grid_expand_line` per run of same-style ASCII text and one per cell tmux writes alone, D-027).
  - It counts one extended entry for each cell `grid_need_extended_cell` (grid.c:117-138) would keep one for: a wide or non-ASCII character, an RGB or theme colour, an attribute past the first eight, an underline colour, a link, a tab's first cell.
  - It is exact for lines written once at the pane's current width: plain, wide, accented, RGB, tab and cursor-addressed text, empty rows, scrolled history, `clear-history`.
  - `#{history_all_bytes}` (format.c:1022-1047: lines, their bytes, cell entries, their bytes, extended entries, their bytes) gives the same model's counts (oracle `format_history_all_bytes`).
- **Where it differs:**
  - A line edited after it was written (erase to end of line, delete or insert characters, a shortened line). tmux keeps the allocation the longer text made; Ghostty keeps no such mark, so vtmux counts what is left.
  - Any line after a width change (`resize-window`, a split, a client of another size, a window that follows it). `grid_reflow` (grid.c:1528-1600) moves a line that fits with its `celldata` and `cellsize` as they were (`grid_reflow_move`). It keeps the first part of a split line at `at` cells (`grid_reflow_split`) and the first line of a join at its old size; only lines it fills afresh grow by `grid_expand_line` for the new width.
    - A line first written at 80 columns holds 20 cells in tmux (a quarter) whatever the width becomes. vtmux counts it at the new width: 7 cells at 30 columns, 30 at 120. The counts agree only when both land on the same step.
- **Why it stays:** a model needs a per-line high-water mark of the allocated width that follows the line through Ghostty's reflow. That is a new `Row` field in the patched Ghostty, its state export and the snapshot format, plus the split and join rules above. Nothing known depends on `#{history_bytes}` being exact after a resize, so it is not built.
- **Evidence:**
  - Oracle `history_bytes` (traces from the reference).
  - `\033[K`, `\033[P` and `\033[@` after text on 40x6: 340/440/440 bytes on the reference, 290/290/290 on vtmux.
  - Oracle `history_bytes_resize`, 80x24, 200 lines in the history (177 in it, 24 on the screen), tmux / vtmux:
    - `1`..`200` one per line: 80 -> 30 columns 28040 / 15040; then 120 columns 28040 / 38040; back to 80 28040 / 28040; `split-window -h` (40 columns) 28040 / 18040.
    - 53-character lines: 80 -> 30 76040 / 76040; 120 68040 / 68040; back to 80 68040 / 88040.

<a id="d-002"></a>
### D-002: ERE resource limits (`m/r`, `s///`)

- **Category:** Deferred.
- **Test:** `crates/vtmux-core/src/regex/tests.rs` `regex_resource_limits_are_errors_not_panics`; `crates/vtmux-core/src/format/matching.rs` `format_matching_limit_is_regexec_failure`.
- **tmux:**
  - glibc `regcomp`/`regexec` (format.c:format_match, regsub.c:regsub) have no step cap. Backreference searches backtrack without bound: `(a*)\1b` against 3000 `a` takes 47 s.
  - Compiling fails with `REG_ESPACE` only when memory runs out.
- **vtmux:**
  - Patterns without backreferences have no work cap; they run in O(text × compiled nodes) (`regex/nfa.rs`).
  - These limits return `Error::Limit`:
    - 64 nested groups or chained quantifiers;
    - 128 groups plus quantifiers;
    - about one million compiled nodes after `{m,n}` expansion (`(a{1000}){1000}` is refused; glibc compiles it);
    - for backreference patterns, one million capture-weighted backtracking steps per start position. There is no cross-start ceiling, so an ordinary backreference on long text still finds its match.
  - Backreference search is quadratic in text length: `(\w+) \1` on 1.9k chars takes about 1 s in a release build, where glibc takes about 10 ms.
  - The format layer handles a limit as tmux handles a failed `regexec`: `m/r` gives `0`, and `s///` copies the rest of the text unchanged, keeping earlier replacements.
  - A large `{m,n}` is slow but correct: `a{32767}` against 32767 `a` takes about 4 s (glibc: 0.07 s).
- **Evidence:**
  - reviewer-8's `high_regex_work_budget_silently_turns` repros (`[0-9]+%` on 710 digits, `[a-z]+:`, `\w+@`, `[^ ]+ x` on 1000 chars, `(a|b)*c`, `.*z|q`) now match glibc 2.43 and the reference tmux 3.8 (`regex_long_texts_match_like_glibc`).
  - 100k-character texts take < 1 s per search (`regex_without_backreferences_is_linear`).
  - `(a*)\1b` against 3000 `a` gives `Limit` after about 1 ms, where glibc returns no match after 47 s.

<a id="d-003"></a>
### D-003: glibc's `^` inside a repeated group

- **Category:** Do not copy tmux bug: a glibc DFA bug whose result changes with the number of captures requested. Copying it means modelling glibc's DFA and its pruning.
- **Test:**
  - None fails: the rows below are not in `src/regex/golden.in`. Task `regex_corners` probed them against glibc 2.34 and 2.43 (python ctypes `regcomp`/`regexec`, as `tests/regex_cases.sh`) and reference tmux 3.8.
  - The other corners this entry used to list now match and are pinned:
    - `golden.in`: an escaped lowercase letter under `REG_ICASE` (`\a`) never matches; the last iteration of an empty-matching repeat is reported (`(a*){2}` on `aa`: `\1` = 2:2).
    - `fnmatch_golden.in`: fnmatch `[a-[:digit:]]` fails the bracket.
    - `format/matching.rs` `empty_matches_step_one_byte_as_regsub_does`: the `s///` empty-match step of one byte.
- **tmux:**
  - glibc's DFA ignores a `^` that is not at the start in a later iteration of a repeated group. Its answer depends on how many captures the caller asks for: regexec.c `re_search_internal` runs `prune_impossible_nodes` only with `nmatch > 1`, and when that fails no shorter match is tried.
  - `(^a|b)+` on `ba`:
    - 0..2 with `nmatch` 1 (copy-mode search);
    - a match with `nmatch` 0 (`m/r`);
    - no match with `nmatch` 10 (`s///`: `#{s/(^a|b)+/X/:abab}` is `abab`).
  - `(^a)+` on `aa` and `(a|^b)+` on `ab` behave the same. A backreference to such a group differs too: `(a*){2}\1` on `aa` is no match.
- **vtmux:** POSIX: `^` matches only at the start of the text, in every iteration. `(^a|b)+` on `ba` is 0..1 whatever the caller asks for, `#{s/(^a|b)+/X/:abab}` is `XX`, and `(a*){2}\1` on `aa` matches 0..2.
- **Evidence:**
  - Task regex_corners probes (glibc 2.34 on CentOS 9, the same answers as 2.43 in review_fmt_regex).
  - Reference tmux 3.8 `display -p '#{m/r:(^a|b)+,abab} #{s/(^a|b)+/X/:#{l:abab}}'` gives `1 abab`; vtmux `1 XX`.

<a id="d-004"></a>
### D-004: time formatting: C locale, long strftime output, TZ values glibc half-parses

- **Category:** Deferred (1, and the offset-then-garbage cases of 4); Do not copy tmux bug (3, a print of uninitialised memory).
- **Test:** `crates/vtmux-core/tests/time_cases.sh` (rows citing D-004); cli `format_time_out_of_range`, `format_time_unparseable_tz_name`; hand probes from review_fmt_time (reviewer-14).
- **tmux:** calls `setlocale(LC_TIME, "")` and glibc `strftime`/`localtime_r` (format.c:4694-4700, 6728-6741).
  1. A non-C `LANG` changes `%c %x %r %p %a %b`. `LANG=en_GB.UTF-8` gives `Fri 30 Oct 2026 14:32:28 GMT` for `%c`; `LANG=de_DE.UTF-8` gives `Do Okt` for `%a %b`.
  2. (Closed, see below.)
  3. `t/f` output longer than 511 bytes makes `strftime` fail, and tmux prints its half-written stack buffer: `#{t/f/%H<600 y>:@n}` gives `16yyyy...`, as far as the buffer got.
  4. A `TZ` that is neither a zoneinfo file nor a POSIX rule keeps its leading letters as the abbreviation (`__tzset_parse_tz`, time/tzset.c).
     - `TZ=Invalid/Zone` gives `%Z` = `Invalid` with offset `+0000`; `TZ=AB/x` an empty `%Z`; `TZ=<+03>/x` gives `+03`.
     - When a valid name and offset come first and garbage after, glibc keeps part of the rule: `EST5/x` is empty with `+0000`, `EST5EDT/x` is `EDT -0400`, `EST25` is `EST -2400`, `ABC-3abc,x` is `abc +0400`.
- **vtmux:**
  1. C locale always (`Fri Oct 30 14:32:28 2026`, `Thu Oct`). `#{t:}` (ctime) is C in both.
  2. (Closed.)
  3. Output over 511 bytes is empty.
  4. The cases with no offset after the name are as glibc's (`SystemWallClock::local_time`, `unparsed_tz_name`). A name followed by an offset that jiff rejects (the `EST5/x` family) is UTC, with `%Z` `UTC`.
- **Closed:** years past 9999 format normally: `@big=999999999999` gives `33658 Fri` (`Tm::in_zone`; cli `format_time_out_of_range`, golden rows at 99999999999).
- **Why it stays:**
  - (1) needs the C library's locale data. The core crates forbid `unsafe` (D10), so the answer would come through a hook from `sys/` into libc `setlocale`/`strftime`.
  - Which locales exist differs per host (this one has `en_GB` and `en_US`, no `de_DE`). A test either skips (a skip is not a pass) or needs a locale CI may lack, and on macOS the libc is Darwin's, not glibc's.
  - The `EST5/x` family needs a model of what glibc's POSIX parser keeps of a half-valid rule (names, offsets and default DST rules).
  - (3) prints uninitialised stack bytes after the part strftime wrote, so there is nothing stable to copy.
- **Evidence:** reference tmux 3.8 and glibc 2.34: probes of 24 `TZ` values and `LANG=en_GB.utf8`/`en_US.utf8` in the task `div_smalls_b` (cli test `format_time_unparseable_tz_name` compares eleven of them on Linux); reviewer-14's probes on b6b2721.

<a id="d-005"></a>
### D-005: a loop nested in a loop of the same kind does not disturb the outer order

- **Category:** Do not copy tmux bug: the outer loop skips and repeats items. Copying it means sharing a mutable list across nested expansions.
- **Test:** `crates/vtmux-core/tests/loop_cases.sh`, the row citing D-005 (`#{S:#{S/n:#{session_name}}-#{session_name} }`).
- **tmux:**
  - `sort_get_sessions`, `sort_get_winlinks_session`, `sort_get_panes_window` and `sort_get_clients` (sort.c:461-600) return one static array per kind, which every call refills and sorts in place.
  - A nested loop of the same kind re-sorts the array the outer loop is walking, and the outer loop reads the inner order from then on.
  - With sessions zeta, alpha, mike (IDs 0, 1, 2), the outer loop visits zeta, then reads index 1 and 2 of the name-sorted array: `alphamikezeta-zeta alphamikezeta-mike alphamikezeta-zeta`. mike is visited once, zeta twice, alpha never.
- **vtmux:** each loop sorts its own list, so the outer loop visits each session once in its own order: `alphamikezeta-zeta alphamikezeta-alpha alphamikezeta-mike`.
- **Evidence:** reference tmux 3.8 on the `loop_cases.sh` topology (the reference value is in the row's comment).

<a id="d-006"></a>
### D-006: a pane without history keeps none of the lines a narrowing pushes off its top

- **Category:** Deferred: a pane without history would need Ghostty scrollback that only a reflow fills and nothing else does.
- **Test:**
  - None fails.
  - The wrap-flag bookkeeping is pinned by oracle `wrap_flag_capture_join` (29 scenes: EL, ED, IL, DL, SD, SU, RI, regions, the history row, the alternate screen), `wrap_flag_redraw_copy` (the redraw of a blank run and copy mode's `select-line`), `wrap_flag_resize` (15 scenes of moves then a wider pane) and `reflow_cells_used` (patches 0011-0013).
  - Its corners are pinned by `wrap_flag_corners` (patch 0029).
- **tmux:** `grid_reflow` (grid.c) sets `hsize` to the lines a narrowing adds above the screen, whether or not the grid has `GRID_HISTORY`. So a pane made with `history-limit` 0 has history after it narrows (nothing collects it: `grid_collect_history` runs only for `GRID_HISTORY`), and widening joins those lines back onto the screen.
- **vtmux:** a pane without history has no Ghostty scrollback (bytes 0, the no-scrollback paths that tmux's missing `GRID_HISTORY` also takes). The rows a narrowing pushes up are dropped: `#{history_size}` stays 0, and widening brings nothing back.
- **Corners that match since patch 0029:**
  - EL 2 on a line never written to (`cellsize` 0) in the default background unwraps nothing (`screen_write_clearline` returns early).
  - Scroll-on-clear scrolls the lines up to the last with cells used, erased ones included (`grid_view_clear_history`).
  - IL and DL with the cursor outside the scroll region work on the whole screen (`grid_view_insert_lines`, `grid_view_delete_lines`); an IL count reaching the bottom moves nothing.
  - Without history, a region of two rows scrolled up unwraps the line moved to its top, and one of one row (a one-row pane) does not scroll. SD and RI do not scroll a one-row pane at all (`grid_move_lines` of `rlower - rupper` lines).
- **Related entries:** a line scrolled out of a region that does not start at the top (D-032); the trailing erased cells `capture-pane -J` keeps on a wrapped line (D-027, the `cellsize` model).
- **Evidence:**
  - Reference tmux 3.8, `set -g history-limit 0`, `printf 'aaaaaaaaaa\r\nbbbbbbbbbb\r\ncc'` on 10x3, `resize-window -x 5`:
    - tmux: `#{history_size}` 2 and `capture-pane -p -S -` `aaaaa` / `aaaaa` / `bbbbb` / `bbbbb` / `cc`; then `-x 10` brings all three lines back.
    - vtmux: 0 and `bbbbb` / `bbbbb` / `cc`; then `bbbbbbbbbb` / `cc` / blank.
  - The corners: task div_wrap_tab_reflow probe, 38 scenes against tmux 3.8 (EL, ED 2 under scroll-on-clear, LF, SU, SD, RI and regions on one- and two-row panes with and without history and on the alternate screen, IL and DL outside the region). 23 differed before patch 0029. With it two do: a region scroll into history (D-032) and the trailing erased cells of a `-J` capture (D-027).

<a id="d-007"></a>
### D-007: terminals come from a built-in table, and frames use fixed sequences

- **Category:** Deferred: terminals outside the table (the 8-bit ones, and any entry a user's own terminfo adds); frames written with fixed sequences.
- **Test:**
  - `crates/vtmux-core/src/term_features/golden.rs`: 2,908 reference rows from `tests/term_features_cases.py`: one for every added terminal and every hardcopy or generic one tmux refuses (372 rows are refusals), and `COLORTERM`, feature and override rows for the older ones and for `wezterm`, `contour`, `foot`, `ansi`, `Eterm` and `rio`.
  - `term_features` `table_tests` and `code_tests`.
  - `crates/vtmux/tests/attach.rs`: `terminal_features_apply_at_attach`, `terminals_tmux_knows_attach`, `terminals_from_the_ncurses_database_attach`, `a_hardcopy_terminal_is_refused`, `terminals_tmux_refuses_leave_no_session`, `terminfo_keys_follow_the_terminal`, `overrides_remove_the_alternate_screen`, `colours_below_256_are_downgraded_like_tmux` (`linux`, `vt100`), `cursor_style_and_colour_reach_the_terminal`, `application_escape_mode_follows_the_appesc_feature`, `the_console_cursor_is_hidden_with_civis`.
  - `render`: `attributes_follow_the_terminal`, `underline_styles_colours_and_overline_follow_the_terminal`.
  - Oracle `term_console_vt`, `term_ncurses_terminals`, `attach_underline_styles`, `attach_attributes_by_terminal`.
- **tmux:**
  - The client reads the terminal's terminfo entry (`tty_term_read_list`, tty-term.c). For any command run from a terminal, it refuses a name the database lacks (`missing or unsuitable terminal: NAME`, client.c:325-331).
  - `tty_term_create` applies `terminal-features` (tty-features.c), `COLORTERM`, then `terminal-overrides` (`tty_term_apply`) to any of ~250 capability names. It refuses a terminal without `clear` or `cup`.
  - Every sequence it writes is a terminfo string.
  - Later device-attribute replies add features too (`tty_default_features`: `XTVERSION`, DA2).
- **vtmux: the table.** vtmux reads no terminfo (D8). Its terminals are a table generated from terminfo by `tests/terminfo_table.py` (`terminfo.rs`).
  - 2,663 names: every entry and alias of ncurses 6.6's database (`wezterm`, `contour`, `foot`, `foot-direct`, `rio`, `ghostty`, `ansi`, `Eterm`, `vscode`, `hterm`, `mlterm`, `terminology`, the `xterm-*` and `screen*` families, `xterm-direct*`, and the old VT, Wyse and DEC ones), plus `xterm-ghostty`, `xterm-kitty` and urxvt's `rxvt-unicode*` from the first generation. Most are terminals nobody runs tmux in.
  - An `xterm*` name not in it takes `xterm`'s entry, with the colours its name suggests.
  - Each entry keeps the `tty_term_codes` names it has, which flags are false, `colors`, and the strings vtmux writes or decodes.
  - Left out: 163 entries with a byte above 0x7f in a string the table keeps (the 8-bit C1 terminals: `vt220-8bit`, `ansi.sys`, `ibmpc`, `qnx`, `p8`...), because the table holds UTF-8 text. They are refused as `missing or unsuitable terminal`, where tmux takes them. 75 hardcopy ones (`hc`: `tty33`, `diablo*`, `lpr`...) are refused as `can't use hardcopy terminal: NAME`, as in tmux's client, and 2 generic ones (`gn`: `unknown`, `ibm327x`) as `missing or unsuitable terminal`. 295 more attach nowhere in tmux either: no `clear` or `cup`, `open terminal failed`.
  - The first generation (CentOS 9: ncurses 6.2 with ncurses-term, kitty's terminfo, and Ghostty's from the pinned ghostty commit's `src/terminfo/ghostty.zig`) is the 68 entries in `tests/terminfo_carried.json`; they take precedence over ncurses 6.6's, whose entries differ for some (`iterm2`, `kitty`, `vte-256color` have `Smulx`; ncurses' `ghostty` lacks `Tc`, `Sync` and `Setulc`). Their cursor (`civis` too), underline and standout strings were added on a host without kitty's and Ghostty's entries or the newer ncurses ones: values from `ghostty.zig` (`ghostty`, `xterm-ghostty`) or from the ncurses entry each is built on (`alacritty-direct`, `kitty-direct` and `xterm-kitty` from `kitty`; `konsole-direct`, `linux-16color`, `st-direct`, `vte-direct`).
  - `foot` and `foot-direct` were stand-ins (`xterm-256color`'s and `xterm-direct`'s) until ncurses 6.6's entries; foot's XTVERSION reply adds its features at runtime, as in tmux.
  - The goldens for the 2,595 added names ran the reference tmux 3.8 (ncurses 6.2's library) on the 6.6 database; `oracle/terminfo` has five of its entries and `tty33` as fixtures, so no host's terminfo matters to the tests.
- **vtmux: refusing a terminal.**
  - A name outside the table is refused when its terminal is opened, with tmux's `missing or unsuitable terminal: NAME`. tmux refuses it in the client for every command; vtmux only for `attach` and `new`.
  - A name the host's terminfo has but the table lacks is refused where tmux attaches it: an 8-bit terminal, or an entry newer than ncurses 6.6 or of the user's own (`~/.terminfo`).
  - A name the table has but the host's terminfo lacks attaches where tmux refuses it. Example: `TERM=xterm-ghostty` or `TERM=wezterm` over SSH to a Linux host without that terminal's terminfo. tmux says `missing or unsuitable terminal: xterm-ghostty`, exit 1; vtmux attaches with Ghostty's entry (audit_owner_setup_linux).
  - A terminal left without `clear` or `cup` after the overrides (`dumb`, an empty `TERM`, `cup@`) is refused with `open terminal failed: terminal does not support clear`, as in tmux, before `new-session` makes the session.
  - Feature parsing, `terminal-features` matching, `COLORTERM`, the `-T`/`-2` flags and `TERM_VT100LIKE` (`XT`, or a `clear` that starts with CSI) follow tmux.
- **vtmux: what follows the entry and the overrides.**
  - The start, stop and lock sequences: `smcup`, `rmcup`, `clear`, `smkx`, `rmkx`, `cnorm`, `E3`, `indn`/`ind`. The mouse, bracketed-paste and focus modes are written only with `kmous`, `Enbp`/`Dsbp` and `Dsfcs`. So `smcup@:rmcup@` keeps the terminal on its normal screen.
  - `bce` and `am`.
  - Colours: `colors` (also as an override) sets the palette and `#{client_colours}`. A terminal without `setaf`/`setab` gets no colours and a background as reverse video (`tty_attributes`).
  - The key tree: `kf1`-`kf12`, `kich1`, `kdch1`, `khome`, `kend`, `knp`, `kpp`, `kcbt` (the console's `ESC [[A` is F1).
    - `kf13`-`kf63` and the modified `kIC`, `kHOM`, `kUP3`... are not in it. So `linux`'s `ESC [25~` (its `kf13`, tmux's S-F1) is S-F3 by the rxvt rows, and `rxvt`'s `ESC [2$` (`kIC`, tmux's S-IC) is M-[ and text.
  - The title (`tsl`, `fsl`), `Ms`, `Eneks` and `Dseks`.
- **vtmux: fixed sequences.** Frames otherwise use fixed xterm sequences, whatever the entry has.
  - `cup`, `el`, `ech` (`vt100` has no `ech`), `csr`, `sgr0`.
  - The attributes' SGR codes, each written only when the terminal has its capability, as `tty_attributes` does: `bold`, `dim`, `sitm` else `smso` (and `smso` under a `screen` or `screen-*` `default-terminal`), `smul`, `blink`, `rev` else `smso`, `invis`, `smxx`, `Smol`, `Smulx`, `Setulc` and `Setulc1`.
  - These go in xterm's form or the form tmux's own feature gives (`\E[4:Nm`, `\E[58:2:R:G:Bm`, `\E[58:5:Nm`, `\E[53m`), not the entry's template. Ghostty's `Setulc` is `\E[58:2::R:G:Bm`. `smso`'s SGR parameter comes from its string (screen's is 3).
  - The startup queries also go to a terminal an override made not VT100-like (tmux sends none).
  - The `margins` feature's `Enmg`/`Dsmg` are not written: vtmux never uses DECSLRM.
- **vtmux: strings written from their values.**
  - The cursor (`cnorm`, `Ss`, `Se`, `cvvis`, `Cs`, `Cr`), bracketed paste (`Enbp`, `Dsbp`), focus (`Enfcs`, `Dsfcs`) and application escape (`Enesc`, `Dsesc`) strings are written from their values (the entry's, a feature's or an override's) through a `tparm` model.
  - The table keeps the values of `civis`, `cvvis`, `smso`, `Ss`, `Se`, `Cs`, `Cr`, `Smulx`, `Setulc` and `Smol`.
    - A frame hides the cursor with `civis`, and ends a hidden one with `cnorm` when that is more than DECTCEM, as on `linux`: `ESC[?25l ESC[?1c`. A terminal without `civis` gets none.
  - A name the table has without a value (`mintty`, `iterm2`, `ms-terminal`: not on the generating host) takes the value tmux's feature gives.
- **vtmux: presence only.**
  - For `Tc`, `RGB`, `setrgbf`, `setrgbb`, `Sync`, `Smulx` and `Setulc`, frames go by whether the capability is there. `Sync=x` or `Sync=QQ%p1%dQQ` still gives `ESC[?2026h`, where tmux writes the template (nothing at all for a value that does not expand to a sequence).
  - `setrgbf` is always `38;2;r;g;b`, where Ghostty's terminfo makes tmux write `38:2:r:g:b`.
  - `Ms` is validated with a model of `tty_term_validate`'s check, fitted to 40 probed templates.
  - Every other name is tracked by presence: features add and `ignorefkeys` removes theirs, overrides set and remove them. So `#{I/c:name}` and `#{I/f:name}` answer as tmux does for the host the table was generated on.
- **vtmux: startup queries.**
  - The queries are tmux's: DA, DA2, XTVERSION, DECRQM 2026, the OSC 10/11 colours, and `?996n` with `?2031h`.
  - Their replies add features as tmux's do (`tty_keys_sync`, `tty_keys_device_attributes`, `tty_keys_device_attributes2`, `tty_keys_extended_device_attributes`). XTVERSION sets `#{client_termtype}`.
  - Then `Eneks` and `Enfcs` are written as `tty_update_features` writes them. The client is redrawn only if a capability vtmux draws with changed (tmux always redraws; the screen is the same).
  - vtmux does not repeat the colour queries on a resize (`tty_repeat_requests` after `TTY_REQUEST_LIMIT`), only after a theme report.
  - A reply that arrives after its query was answered or timed out is swallowed, where tmux reads it as keys.
  - With the `hyperlinks` feature (`Hls`), frames write each cell's link as tmux does (`tty_hyperlink`): `\E]8;id=tmuxN;URI\E\\` when the link changes, nothing to a terminal without `Hls`. The numbering of `tmuxN` differs: [D-068](#d-068).
- **Evidence:**
  - Reference tmux 3.8 `list-clients -F '#{client_termfeatures}|#{client_colours}|#{Ic:...}'` on a PTY client, for every row of `golden/cases.tsv`.
  - `Ms` probes in `term_features_cases.py` rows `xterm*:Ms=...`.
  - Task div_terminals_overrides: the reference attaches `linux`, `alacritty`, `xterm-kitty`, `kitty`, `screen.xterm-256color`, `screen.linux`, `tmux`, `tmux-direct`, `rxvt`, `rxvt-unicode-256color`, `vt100`, `vt220`, `st-256color` and `putty-256color` with the features and colours vtmux now gives (vtmux used to refuse them all). It refuses `dumb`, an empty `TERM` and `bogus` with the texts above (vtmux used to leave the `new-session` session behind).

<a id="d-008"></a>
### D-008: respawn-window -k on a zoomed window

- **Category:** Do not copy tmux bug.
- **Test:** `crates/vtmux/tests/cli.rs` `windows_respawn_zoomed` (vtmux only: the reference dies).
- **tmux:** `spawn_window` (spawn.c:106-128) with `SPAWN_RESPAWN` frees the layout (`layout_free`) and the other panes, but leaves `WINDOW_ZOOMED` set with the saved layout pointing at freed cells. The next command that reads the window's layout (`display -p '#{window_layout}'`, `splitw`) kills the server (`server exited unexpectedly`).
- **vtmux:** the window is unzoomed first, then respawned: one pane, full size, `window_zoomed_flag` 0.
- **Evidence:** reference tmux 3.8, isolated: `new -d -s a cat; new -d -s k cat; splitw -d -t a cat; resizep -Z -t a:0.1; respawnw -k -t a; display -p -t a '#{window_layout}'` gives `server exited unexpectedly`, exit 1, and every later command says `no server running`.

<a id="d-009"></a>
### D-009: a tab or `\001` in a config cause is not drawn as tmux draws it

- **Category:** Deferred: the cell and the charset toggle need the backing terminal to hold a tab cell and an ACS attribute.
- **Test:** cli (attach) `config_causes_show_in_view_mode_once_attached`; oracle `config_causes_view`. Neither holds a tab or `\001`.
- **tmux:**
  - Once a session is attached (`new-session`, `attach-session`, cfg.c:239-280), the startup config's causes go into view mode of its active pane, written unparsed (`window_copy_add(wp, 0, ...)` through `screen_write_vnputs`, screen-write.c:607-661).
  - A control character is dropped, so an ESC shows as text. Two exceptions: a tab is written as a cell holding the tab byte, and `\001` toggles the ACS charset for the cells after it.
  - `set -g "a<TAB>b<^A>c" 1` draws `a`, a tab, `b`, then `c` as `␌`.
- **vtmux:**
  - `cfg_show_causes` does the same (`Server::cfg_show_causes`): from `cfg_done`, and at the end of `new-session` and `attach-session` once `cfg_finished`.
  - The oldest client, if a control client, gets `%config-error`. With no attached session the causes stay queued. The text is `unparsed`, as `screen_write_vnputs` writes it.
  - Only a tab and `\001` differ: both are dropped (`abc` where tmux draws a tab and `␌`). Everything else matches, including the escape shown as `(0red`.
- **Evidence:** reference tmux 3.8 on a PTY client, a config with `set -g nosuchopt 1` and `set -g "\033(0red" 1`: both screens equal. With `a<TAB>b<^A>c`, tmux's output has `a\tb\u240c`, vtmux's `abc`.

<a id="d-010"></a>
### D-010: copy mode draws its whole screen after every command (narrowed)

- **Category:** Deferred.
- **Test:**
  - None fails. Regress `copy-mode-*.sh` all pass (`copy-mode-commands.sh`, `copy-mode-no-client-scroll.sh` and `copy-mode-redraw.sh` were the xfails). Oracle `copy_*` pins copy mode.
  - Three steps annotated `D-010` pin vtmux's side of the cases below: `copy_line_numbers_cut` steps 30 and 37, `copy_line_numbers_reselect` step 43.
- **tmux:** after a command, tmux writes to the terminal only what that command redraws.
  - A move (`WINDOW_COPY_CMD_MOVE`, window-copy.c:3967-3976) redraws the indicator and the lines the cursor crossed (`window_copy_redraw_selection`), cell by cell (`screen_write_start_pane`). Blanks in the mark, match or selection style go out as spaces.
  - A full pane redraw draws from the mode's screen with `tty_draw_line`, which erases a run of such blanks unless they are selected. Full redraws are `PANE_REDRAW`: an option set, `refresh-client`, and every redraw while line numbers are on (`window_copy_redraw_lines`).
- **Three cases differ:**
  - *Stale selection.* Each cell of the mode's screen keeps the selected flag it had when last written (`GRID_FLAG_SELECTED`, set by `screen_write_cell`, screen-write.c:2765), and `tty_draw_line` reads that flag.
    - A row the command did not rewrite keeps the old selection on the terminal, also through a later `refresh-client` or option set, until something rewrites the row.
    - So tmux shows the old selection on other rows when a word or line selection widens over lines the cursor did not cross, or when a move follows `line-numbers-off` or `line-numbers-on` (the selection is set again with the new gutter, but only the cursor's rows are rewritten). See `copy_line_numbers_reselect` step 43, `copy_line_numbers_cut` step 37.
    - A probe with `refresh-client` and `set copy-mode-current-line-style` after such a move kept the stale row in tmux.
  - *Mark after an option set.* The mark's line keeps its style only up to the first blank past the mark (a probe of `set-mark`, `cursor-up` and `set copy-mode-current-line-style`).
  - *Blanks written as spaces.* Without line numbers, a redraw goes cell by cell and writes only the cells that changed in the mode's screen, so the blanks `line-numbers-off` leaves where the gutter was go out as spaces.
    - vtmux's compositor erases a run of 10 or more blanks, as `tty_draw_line` does. The two look the same, but a capture sees the written cells.
    - `copy_line_numbers_cut` step 30: tmux's reset after the cursor's styled row lands on the row below, vtmux's on the status line.
- **vtmux:** draws the whole mode screen after every command, with the selection the formats report on every row.
  - Without line numbers, styled blanks are always written, as tmux's cell-by-cell path writes them. With line numbers on, only selected blanks are, as with tmux's `tty_draw_line`.
  - The rest of window-copy.c's drawing matches tmux, and all 99 commands are done.
  - Line numbers are covered: the gutter in `copy-mode-line-number-style` and `copy-mode-current-line-number-style` for all five `copy-mode-line-numbers` values; the text, the indicator and the `$` of a cursor past the text, drawn after the gutter.
  - A wide character or TAB cut at the text's edge is a default cell, with no current-line, mark or match style (`window_copy_write_one`, window-copy.c:5242).
  - The selection is kept in screen columns. Its `clipx` and columns come from the gutter as it was when the selection was last set (`window_copy_set_selection`; `window_copy_set_line_numbers1` only redraws).
    - So a selection set with numbers off covers the gutter cells it reaches once they come on, and the gutter is selected cell by cell (`screen_write_cell`), until the next command sets it again (`copy_line_numbers_reselect`, `copy_line_numbers_cut` step 33).
  - Mouse columns are taken less the gutter. `copy-mode-current-line-style` and `copy-mode -M` (the mouse drag entry, which turns line numbers off) are done too.
- **Ghostty-grid limits left:**
  - A line's time (`top_line_time`) is lost when the pane's width changes before copy mode is entered: Ghostty's reflow does not say which row went where, so the indicator shows no time for that line.
  - The same reflow keeps an OSC 133 mark on the row its pin follows.
- **Why it stays:**
  - vtmux composes each client's frame from the model and diffs it against what that client last got.
  - Matching the cases above means keeping tmux's per-cell selected flags for the mode's screen and knowing which rows each command rewrites (`window_copy_redraw_selection`, `window_copy_redraw_lines`, the rectangle's own redraw in `window_copy_set_selection`, the scrolls' line inserts and deletes).
  - It also means knowing, per client and per pane, whether each blank reached the terminal as a written space or an erase: tracking every full-redraw trigger tmux has (`server_redraw_*`, `PANE_REDRAW`, overlays, resizes).
  - The stale selection is one tmux's own state no longer has: what tmux copies is the selection vtmux draws.
- **Evidence:**
  - `cargo xtask oracle replay --only 'copy_*'`. It covers `copy_line_numbers` and `copy_line_numbers_marks` (the gutter, the five modes, `$`, the selection beside the gutter, a mouse drag, `copy-mode -M` and `copy-mode-current-line-style`), `copy_line_numbers_cut` (cut cells on the cursor's row, the gutter selected after `line-numbers-on`) and `copy_line_numbers_reselect` (a vi rectangle and an emacs selection across toggles).
  - `cargo xtask regress --only 'copy-mode-*'`; cli.rs `copy_mode_formats`, `copy_selection_formats`; attach.rs `owner_copy_mode_vi_bindings`, `owner_prompt_jumps`; oracle `attach_mouse_drag`.

<a id="d-011"></a>
### D-011: closed (U+0085 and TAB cells match tmux)

- **Was:**
  - A C1 control written as UTF-8 (U+0085) took a cell of its own in Ghostty, where tmux combines it into the cell before (`utf8_width`: wcwidth -1, C1, width 0). So next-word over `xa\302\205b` stopped one column later.
  - HT left no TAB cell.
- **Now:**
  - Vendored patch `0009-c1-is-zero-width.patch` makes a decoded C1 control a zero-width character that joins the previous cell (Ghostty dropped it). `0002-tab-cell-mark.patch` gives tmux's TAB cell.
  - Regress `copy-mode-unicode-whitespace.sh`, `utf8-test.sh` and `copy-mode-test-emacs.sh` pass; cli `program_output_reaches_the_grid_as_in_tmux` (case `c1`) pins it.
  - A resize reflows a tab as tmux's `grid_reflow` does (the TAB cell's width plus one per padding cell): patch `0015-reflow-tab-as-tmux.patch`, goldens `cap_tab_resize_*`, oracle `rep_deccolm_tab_reflow`.

<a id="d-012"></a>
### D-012: a new pane's first automatic-rename check waits one name interval

- **Category:** Do not copy tmux bug: the name depends on a scheduler race.
- **Test:** cli.rs `initial_window_name_is_the_command` and `split_and_respawned_pane_name_is_the_command` (vtmux only: the reference's outcome is not repeatable, so it cannot be compared).
- **tmux:**
  - `spawn_window` sets the name from `default_window_name` (the command's first word), and `check_window_name` (names.c) runs at the end of the same loop turn, with no `name_time` yet.
  - `#{pane_current_command}` then reads the foreground process of the new PTY. That is usually still the forked server (`tmux`), sometimes the shell before it execs, or the command itself.
  - So `new -d -s a cat; neww -d cat; neww -d cat` gives `tmux`, `cat` and `sh` in different runs. Out of 20 runs: 10 `tmux/tmux/tmux`, 6 `tmux/tmux/cat`, 2 `cat/cat/tmux`, 2 `tmux/cat/cat`.
  - `window-renamed` fires for the ones that lose, and a window that prints nothing keeps the wrong name.
  - The same goes for `split-window` and `respawn-pane` in a window whose `name_time` is older than `NAME_INTERVAL`: the new pane is active and changed, so the check runs at once. `splitw sh -c 'exec sleep 101'` and `respawnp -k sh -c 'exec sleep 102'` named their windows `tmux` in 9 of 10 runs (once `cat`).
- **vtmux:**
  - A pane's name clock starts at its spawn (new window, split or respawn), so the first check comes one `NAME_INTERVAL` (500 ms) later, when the command has exec'd. Before this, a split or respawn was checked at once: `sh` on Linux, the server binary (`vtmux-<sha>`) on macOS.
  - The window keeps the name from `default_window_name` (`cat`) or settles on the command's `argv[0]` (`sleep` for `sh -c 'exec sleep 5'`).
  - No spurious `window-renamed` or `%window-renamed` fires at start.
- **Evidence:** 30 runs of `new -d -s a cat; neww -d cat; neww -d; neww -d sleep 4; neww -d 'exec sleep 5'` with SHELL=/bin/sh give `cat/cat/sh/sleep/sleep` every time; before the change `sh/cat/...` mixes. Reference runs of the same script vary as above.

<a id="d-013"></a>
### D-013: byte sequences glibc reads as one character but Unicode does not (past U+10FFFF, surrogates)

- **Category:** Deferred. Only byte sequences that are not valid UTF-8 but that glibc decodes anyway. Every valid UTF-8 sequence (U+10FF80..U+10FFFF and U+FFFD included) matches the reference, and so does every byte glibc rejects.
- **Test:** none; the rows are left out of cli.rs `non_utf8_bytes_through_options_and_formats`.
- **tmux:**
  - Option values, formats and arguments are C strings.
  - For `.`, `?` and `[...]` (`regexec`, `fnmatch`) and for `#{w:}` and `#{=N:}`, tmux 3.8 on glibc 2.43 sees each of these as one character: `\364\220\200\200` (`\U00110000`), the five-byte `\370\210\200\200\200` and the six-byte `\375\277\277\277\277\277` (`\U7fffffff`).
  - `#{s/./x/:@o}` gives `x`; `#{m:?,#{@o}}` and `#{m/r:^.$,#{@o}}` give 1; `#{w:@o}` of the four-byte form gives 1.
- **vtmux:**
  - The core keeps text as `String`, with each byte that is not UTF-8 as one escaped byte (`rawstr`). These sequences are separate bytes: `.`, `?` and `[...]` do not match them, `w` is 0, and `=` skips them.
  - `n`, `p`, `q`, `b`, `d`, `==`, `show`, `display -p` and `source-file -v` match tmux for them, as they do for every byte string.
- **Also** (review_bytes_args_2, reviewer-54, `/tmp/r54/n.sh`):
  - Names. tmux's `utf8_isvalid` decodes through `mbtowc`, so a window, session or buffer name containing `\364\220\200\200` (F4 90..F4 BF leads, U+110000..U+13FFFF) is accepted. vtmux rejects it as `invalid window/buffer/session name`.
  - Surrogates. glibc decodes `\355\240\200` (U+D800, ED A0..ED BF leads) as one character for `regexec`/`fnmatch`. So `#{s/./Z/:@p}` on `a<ED A0 80>b` gives `ZZZ` in tmux and `Z<ED A0 80>Z` in vtmux; `m/r` and captures differ the same way.
- **Evidence:** `set -g @o "$(printf '\370\210\200\200\200')"; display -p '#{s/./x/:@o}'`: tmux `x`, vtmux `\370\210\200\200\200`. `/tmp/bytes_args/p7.sh` (bytes_args worker) lists every differing row.

<a id="d-014"></a>
### D-014: closed (window pixel sizes follow the clients' cell sizes)

- **Was:** `#{window_cell_width}` and `#{window_cell_height}` were empty, panes got `ws_xpixel = ws_ypixel = 0`, and XTWINOPS replies always used 16x32.
- **Now:**
  - A window's cell size is that of the clients that size it, taken only when it is resized: the first client counted whose cell is wider and taller than any before (`clients_calculate_size`, resize.c:212-215), the creating client's for a new window (`default_window_size`, resize.c:300-301), the old window's for `break-pane` (cmd-break-pane.c:159), and 16x32 (`DEFAULT_XPIXEL`, `DEFAULT_YPIXEL`, window.c:575-578) under a manual size or with no client giving one.
  - A pane's PTY gets it times its size (spawn.c:451-452, `window_pane_send_resize`), and XTWINOPS `CSI 14/15/16 t` and mode 2048 (D-067) answer with it (input.c:2163-2180). A live upgrade carries it.
  - The client side, unchanged: a terminal that gives no pixel size is sent `ESC[18t ESC[14t` by `tty_resize` (tty.c:144-150), and `TTY_WINSIZEQUERY` holds the partial-key delay until its `ESC[4;h;wt` reply (tty-keys.c:1002-1008).
- **Test:** oracle `window_cell_pixels` and `client_cell_pixels`; attach.rs `window_cell_pixels_size_the_pane_ptys` (PTY pixels against the reference) and `cell_pixel_size_from_the_terminal_or_its_reply`; cli `display_message_all` compares both formats; vtmux-vt `size_reports_use_the_cell_size_given`; vtmux-server `a_windows_cell_pixels_are_kept`, `the_pty_carries_its_size_in_pixels`.

<a id="d-015"></a>
### D-015: the cell after a pane prompt's input keeps message-style at some widths

- **Category:** Do not copy tmux bug: an allocation-rounding artefact, one cell's background at four widths.
- **Test:** none; reviewer-57's raw-byte probe (review_pane_prompt).
- **tmux:**
  - The pane prompt row is drawn with `tty_draw_line` up to the line's `cellsize` (screen-redraw.c:1671, tty-draw.c:161-166). `cellsize` comes from `grid_expand_line`'s rounding to sx/4 or sx/2 (grid.c).
  - So whether the cell after the input is drawn in message-style depends on the pane width. At widths 48-51 tmux sends `(goto line)\e[39m\e[49m\e[K`; at 20, 30, 35, 46, 47, 52, 55, 99 and 100 it sends a styled space.
- **vtmux:** always draws the styled space after the input.
- **Evidence:** reviewer-57's raw PTY bytes from the reference, `command-prompt -b -P -p '(goto line)'`, at the widths above.

<a id="d-016"></a>
### D-016: the pane prompt's cursor with status off and status-position top

- **Category:** Do not copy tmux bug: the cursor is away from the text being edited.
- **Test:** none; reviewer-57's oracle live scenario (review_pane_prompt).
- **tmux:** the prompt row follows `status-position` (screen-redraw.c:1616-1617, 1641), but the cursor is placed using `status_at_line(c) == 0` (server-client.c:2141-2144), which is -1 with status off (status.c:90-91, 106). With `status off` and `status-position top`, the prompt is drawn on row 0 and the cursor is put on the last row.
- **vtmux:** the cursor is placed on the prompt's row.
- **Evidence:** 40x8, `set -g status off; set -g status-position top`, copy-mode, `g`: tmux puts the cursor at 13,7 with the prompt on row 0; vtmux at 13,0.

<a id="d-017"></a>
### D-017: `show-messages -T` lists features, not terminfo capabilities

- **Category:** Deferred.
- **Test:** `crates/vtmux/tests/attach.rs` `show_messages_names_keys_and_terminals` (the `Terminal N: ...` header line only).
- **tmux:** `cmd_show_messages_terminals` (cmd-show-messages.c:47-70) prints `Terminal N: <TERM> for <client>, flags=0x..:`, then `tty_term_describe` for every capability the terminfo entry has or lacks (237 lines for `TERM=screen`).
- **vtmux:**
  - vtmux has no terminfo model (D8, D-007). It prints the same header line (flags: 0x1 256 colours, 0x2 no `am`, 0x10 RGB, 0x20 VT100-like) and one `   features: <list>` line with what vtmux knows.
  - A client with no open terminal (a control or command client) is skipped, as in tmux.
- **Why it stays:**
  - The lines are the capability *values* of the host's terminfo entry after `tty_term_create` has applied `terminal-features`, `COLORTERM` and `terminal-overrides` (the header's `flags=` already is).
  - vtmux keeps only the presence of a name (D-007: the tables from `tests/terminfo_names.sh`; `xterm-256color` shares `xterm`'s, though its `colors` differs). Printing the values means reading terminfo for real (a compiled-entry parser, the strings escaped by `strnvis`) and modelling what features and overrides add to each string.
  - Without the values a line cannot be right, so a script that greps `show-messages -T` for `(flag) true` of `RGB` or `Tc` finds nothing in vtmux's output either way.
- **Evidence:** reference tmux 3.8 on a PTY client with `TERM=screen`, `xterm-256color`, and `xterm-256color` with `COLORTERM=truecolor`: flags `0x20`, `0x20`, `0x31`. `show-messages -T | wc -l` is 237 for `screen`.

<a id="d-018"></a>
### D-018: closed (`clear-history -H` empties the link table)

- **Was:** `-H` was accepted and changed nothing, so `#{mouse_hyperlink}`, `#{copy_cursor_hyperlink}` and `capture-pane -H`, `-e` and `-R` still gave a link written before it.
- **Now:**
  - `-H` runs `PaneTerm::reset_hyperlinks` after the history is cleared (cmd-capture-pane.c:424-431, `screen_reset_hyperlinks`, screen.c:145-151). The screen in use (the alternate one while it is up, as `wp->screen`) forgets every link its cells carry and the one open at its cursor, and text written on is not linked.
  - libghostty-vt has no call to drop links (it keeps them with the page that holds the cells). So a screen that has links is rebuilt from its exported state, the path a live upgrade takes, minus the links; a screen with none is untouched.
  - Plain `clear-history` leaves them.
  - Oracle `clear_history_hyperlinks` fails without it (links gone from `capture-pane -H`/`-e` and `#{copy_cursor_hyperlink}`, a link written afterwards read as ever); unit test `reset_hyperlinks_forgets_the_links_of_the_screen_in_use`.
- **Residual:** tmux keeps the links of the screen not in use (the primary's, while the alternate is up), and so does vtmux.

<a id="d-019"></a>
### D-019: bytes that are not UTF-8 follow glibc on macOS too

- **Category:** Deferred: a per-platform libc model in the core. Only text that is not valid UTF-8, or code points past U+10FFFF, is affected.
- **Test:**
  - cli.rs `non_utf8_bytes_in_command_strings`, `non_utf8_bytes_through_options_and_formats`, `plane16_characters_are_ordinary_text`: the rows gated on `GLIBC_REFERENCE` are compared on Linux only.
  - Also `current_command_is_read_as_bytes`, whose trailing-byte rule is glibc's `isalnum` and `ispunct`. It is Linux only for another reason too: macOS reads `p_comm`, not argv[0].
- **tmux:** calls the host libc. On macOS (Darwin libc, tmux 3.8 built with utf8proc, as configure requires there):
  1. `regexec` returns `REG_ILLSEQ` for any text holding a byte that is not UTF-8. So `#{s/b/Z/:@o}` on `a\377b\303\251c` copies the text unchanged, and `#{s/x*/-/:@p}`, whose empty-match step lands inside a character (regsub.c `regsub` steps one byte), stops there.
  2. `wctomb` fails past U+10FFFF, so `\U00110000` in a config file is `invalid \U argument`.
  3. `isalnum(0xe9)` is true in a UTF-8 locale, so `display -\351` says `unknown flag` instead of `invalid flag`.
- **vtmux:** the core implements glibc's answers on every system (D-013 lists where glibc itself differs).
  1. The rest of the text is matched around the byte.
  2. `\U00110000` gives the old four-byte form.
  3. A non-ASCII flag byte is `invalid flag`, and `parse_window_name` strips every trailing byte past 0x7f.
- **Evidence:** C probes on macOS 15 (`setlocale(LC_CTYPE, "en_US.UTF-8")` as tmux.c does): `regexec("b", "a\377b")` = 17 (`REG_ILLSEQ`), `regcomp("a\377")` = 17; `wctomb(0x110000)` = -1; `isalnum(0xe9)` true. The reference tmux 3.8 on the same host gives the outputs above.

<a id="d-020"></a>
### D-020: `send-keys -K` delivers keys to the pane

- **Category:** Do not copy tmux bug: keys are lost.
- **Test:** `crates/vtmux/tests/attach.rs` `send_keys_k_delivers_keys_to_the_pane` (vtmux only: the reference drops the keys); oracle `extended_keys_sendk` (last three captures annotated). The bytes themselves are pinned against tmux by `keys.rs` `send_keys_in_each_key_mode_and_format`.
- **tmux:**
  - `cmd_send_keys_inject_key` (cmd-send-keys.c:79) hands the key to the client as `key|KEYC_SENT`. Nothing strips the flag when the key reaches a pane (`server_client_key_callback` -> `window_pane_key` -> `input_key`).
  - So `input_key` misses the trivial case (input-keys.c:628-646) and the tree lookup (655-662). Named keys then hit the `KEYC_IS_SPECIAL` ignore (input-keys.c:681-685), and in modes 1 and 2 `input_key_extended`'s modifier switch hits `default` and returns -1.
  - Most keys sent with `-K` to a pane are lost; in mode 1 even `a` and `Tab`.
- **vtmux:** the key is handled like a typed one. The pane gets the same bytes as from `send-keys` without `-K` (and as from the attached client's own keystroke). Bindings and prompts see the key as in tmux.
- **Evidence:** reference tmux 3.8, attached client under a PTY, `send-keys -K -c <client> KEY` for `C-F3 BTab Escape F3 C-Up a Tab`, each followed by `send-keys -H 7c`, the pane copying its input to a file:
  - standard mode `||\e|||a|\t` (keys 1, 2, 4, 5 lost);
  - mode 1 nothing at all;
  - mode 2 only `\e[27;2;9~` (BTab) arrives.
  - vtmux gives every key (review_extkeys, `skk2.py`).

<a id="d-021"></a>
### D-021: closed (a typed Meta-BSpace with `backspace` set to a keypad key or BTab)

- **Was:** with `backspace` set to a keypad key or BTab, a typed modified M-BSpace (CSI-u `\e[127;3u`, xterm `\e[27;3;127~`) made vtmux write the Escape-prefixed key (`\e1`, `\e[Z`).
- **Now:** as in tmux, nothing is sent:
  - The key keeps `KEYC_IMPLIED_META` through input-keys.c:612. The table has no Meta rows for keypad keys or BTab, and a key with the flag is not looked up again without Meta (655-656), so tmux ignores it as a function key (681-685).
  - BTab keeps the flag when it loses its modifiers (617-625), except in mode 2, where it is S-Tab.
  - keys.rs `typed_meta_bspace_under_a_keypad_or_btab_backspace` (KP1, KPEnter, KP*, BTab in each mode and format); attach.rs `typed_meta_bspace_follows_the_backspace_option` against tmux 3.8 (all 16 keypad keys and BTab in a one-off run; KP1 and BTab committed). Fixed in commit 6a753c4.

<a id="d-022"></a>
### D-022: children's `PATH` starts with the `tmux` shim (spec D5)

- **Category:** Spec decision (D5, drop-in: plugins call `tmux` and must not depend on the user's `PATH`).
- **Test:** `crates/vtmux/tests/attach.rs` `tmux_shim_reaches_this_server_from_children` and `a_child_without_path_gets_the_default_path_after_the_shim` (vtmux only); `vtmux-server` `shim::tests`; regress `new-session-environment.sh` (`PATH=1` became `<shim>:1`).
- **Patch:** `compat/patches/0001-new-session-environment-path-shim.patch`. The script prints a pane's `PATH`; the patch drops a leading shim entry, a no-op under tmux.
- **tmux:** panes get the session/global `PATH` (or the command client's, spawn.c:418-424), else `_PATH_DEFPATH` (`/usr/bin:/bin` on Linux and macOS). Jobs (`run-shell`, `if-shell`, `#()`, hooks) and popups get `environ_for_session` unchanged, so without a `PATH` there they get none and the shell picks its own (bash: `/usr/local/bin:/usr/bin`; job.c:153,189). A script's `tmux` is whatever that `PATH` finds.
- **vtmux:**
  - At start the server makes `.<socket name>.bin/` (0700) beside its socket, holding `tmux` -> its own executable (symlinks resolved).
  - It puts that directory first on `PATH` for every pane, respawned pane, popup and job, the config's `run-shell`/`if-shell` included. It adds it only once, when `PATH` already starts with it.
  - Without a `PATH`, a pane gets `<shim>:/usr/bin:/bin` (tmux's `_PATH_DEFPATH` after the shim), and so do jobs and popups, where tmux gives them none: the shim needs a `PATH` to go first on.
  - The directory is removed when the server exits, and a stale one is reused.
  - Invoked as `tmux`, the binary finds this server through `$TMUX`. So `tmux set -g @x 1` from a script reaches it even when the server was started as `vtmux -L foo` and the user's `PATH` has another `tmux` first.
  - `show-environment PATH` still shows the stored value.
  - A pane's shell may still reorder `PATH` (a login shell's /etc/profile with macOS path_helper, or an rc file that prepends its own directories). So in an interactive pane the shim can end up after another `tmux`; jobs and non-login commands keep it first.
- **Evidence:** the owner's `.tmux.conf` under `vtmux -L foo`.
  - Without the shim, mu-crew's poller and `run-shell` scripts ran the `tmux` 3.7c on `PATH`, which set `@mu_crew_*` on the wrong server, so the status line showed only the host pill.
  - With the shim, after 12 s `@mu_crew_cpu_text`, `@mu_crew_uptime_value` and `@mu_crew_host_color` are set, and `#{T:status-right}` has the CPU, RAM, host and uptime pills.

<a id="d-023"></a>
### D-023: a dead pane's line after a last cell vtmux cannot write back

- **Category:** Ghostty grid: a row carries no settable wrap flag (D4).
- **Test:** none pins the cells it cannot write back (`remain_on_exit` and `remain_on_exit_screen` keep away from them); reproduction below. Oracle `remain_on_exit_reflow` pins the lines it marks.
- **tmux:**
  - `server_destroy_pane` (server-fn.c:406-408) scrolls the dead pane with `screen_write_linefeed(ctx, 1, 8)`, which marks the old last line wrapped whatever it holds.
  - So `capture-pane -J` joins it to the `remain-on-exit-format` line, and a wider pane reflows the two into one.
  - 20x4, `printf 'a\nb\nc\nlast'; exit 2` with the format `DEAD #{pane_dead_status}`: `capture-pane -pJ` ends `lastDEAD 2`. After `resize-window -x 30` the last row is `lastDEAD 2`, with the cursor at 4,3 (tmux's grid holds only the cells written).
- **vtmux:**
  - Ghostty marks a row wrapped only when a print wraps, so vtmux prints at the line's last column to get the mark (`remain.rs` `mark_wrapped`). It does one of:
    - rewrite the cell that was there;
    - clear a blank one the line used, with ICH;
    - past the cells the line used, print a wide character that does not fit, which leaves the cell unwritten and the line's used cells (patch 0013's `cellused`) as they were.
  - `capture-pane -J` and the reflow (patch 0020 joins a wrapped row by its used cells) then match tmux: `lastDEAD 2`, wider and narrower (oracle `remain_on_exit_reflow`).
  - The exception is a last cell it cannot write back as it was: a wide character's half, a hyperlink, a line-drawing character, a TAB, a blank with attributes beyond its background. That line is not marked, so `-J` prints it on a line of its own and a reflow keeps the two lines apart.
- **Evidence:** the reproduction above against tmux 3.8 and vtmux.

<a id="d-024"></a>
### D-024: a second stdin read from inside `source-file -` fails at once

- **Category:** Do not copy tmux bug: tmux hangs.
- **Test:** `crates/vtmux/tests/cli.rs` `source_file_stdin` (the cases tmux finishes: `source - -`, a file then `-`, `-` alone).
- **tmux:**
  - A `source -` or `load-buffer -` among the commands read from stdin by `source-file -` (`printf 'display -p x\nsource -\n' | tmux source -`) waits for ever when the client's stdout is a pipe. The inner `file_read` of `-` needs the client's stdin, which the outer read already has, and the client never answers.
  - With stdout redirected to a file it ends with `Input/output error: -`. When the inner `source -` is the first command it fails at once with `Bad file descriptor: -`.
  - `tmux source - </dev/null` (stdin empty) returns 0 at once.
- **vtmux:** the inner read fails at once with `Bad file descriptor: -` (exit 1), after printing the output of the commands before it, however stdout is connected. `source - </dev/null` returns 0 at once, as in tmux.
- **Evidence:** reference tmux 3.8 under `timeout -k 5 6`, six runs each:
  - `printf 'display -p x\nsource -\n' | tmux source -` and the `load-buffer -` form time out every time with stdout a pipe; `>file` gives `x` and `Input/output error: -`, exit 1.
  - vtmux: 0.01 s, `x` then `Bad file descriptor: -`, exit 1, every time.

<a id="d-025"></a>
### D-025: closed (a program's RIS is tmux's `screen_write_reset`)

- **Was:** `ESC c` went to Ghostty, whose full reset erased the history and left the alternate screen.
- **Now:** the scanner keeps the `c` from Ghostty and runs the reset `send-keys -R` uses (`PaneTerm::reset_screen`): history kept, the used rows scrolled into it under `scroll-on-clear`, the alternate screen and the title kept (input.c:1407-1412, screen-write.c:423-437). Goldens `cap_ris`, `fmt_ris`, `cap_ris_alt`, `fmt_ris_alt`; cli `program_output_reaches_the_grid_as_in_tmux` (cases `ris`, `risalt`).

<a id="d-026"></a>
### D-026: `vtmux -V` is not `#{version}` (spec D21, D25)

- **Category:** Spec decision (D21, amended by D25).
- **Test:** regress `format-variables.sh`, `test_var version "$VER"` with `VER` from `$TMUX -V | sed 's/^tmux //'`.
- **Patch:** `compat/patches/0002-format-variables-version.patch`. The script takes the tmux version out of a `vtmux` `-V` line too; a no-op under tmux.
- **tmux:** `-V` prints `tmux 3.8` and `#{version}` is `3.8`, so stripping `tmux ` from one gives the other.
- **vtmux:**
  - Invoked as `vtmux`, `-V` prints vtmux's own version: `vtmux 0.0.0 (tmux 3.8)` today, `vtmux 3.8-beta1` under D25. `#{version}` stays the tmux version `3.8`, so plugins' version gates see tmux.
  - Invoked as `tmux`, `-V` prints `tmux 3.8`, as tmux does.
  - The regress harness runs the binary as `vtmux`.
- **Evidence:** `vtmux -V` gives `vtmux 0.0.0 (tmux 3.8)`; `vtmux display -p '#{version}'` gives `3.8`.

<a id="d-027"></a>
### D-027: `capture-pane -R`, `-N` and `-e` model tmux's line allocation (`cellsize`)

- **Category:** Ghostty grid: tmux's allocation does not exist in Ghostty's grid.
- **Test:** oracle `capture_grid_dump`, `capture_hyperlinks`, `capture_line_flags` and `capture_grid_cells_alone` pin the scenes where the model is exact. Regress `input-unicode.sh` (wide/widepad `-F`) and `input-raw-unicode.sh` (the overwrite scenes) pass since Ghostty patch 0034; no regress assertion is left on this entry.
- **tmux:** `-R` (`cmd_capture_pane_grid`, cmd-capture-pane.c:132-178) dumps tmux's own grid: each line's flags, `cellused`/`cellsize` and OSC 133 data; each cell's flags, attributes and colours.
  - Line flags `GRID_LINE_EXTENDED` and `GRID_LINE_HYPERLINK` stay set once a cell needed the extended form, until the line is emptied (grid.c `grid_extended_cell`, `grid_empty_line`). So does the cell's extended entry: such a cell erased is `NONE`, not `CLEARED` (`grid_clear_cell`), as is any cell erased to an RGB background.
  - `cellsize` is how far the line has been allocated (`grid_expand_line`, once per flush of collected ASCII and once per cell written alone). It also bounds `-N` and `-e`.
  - A palette colour keeps the form it was set with: `ESC[31m` is `red[1]`, `ESC[38;5;1m` is `colour1[1000001]` with `FG256`.
- **vtmux:**
  - The line flags are kept per row since patch 0034: set by the writes that make an extended cell, kept when it is overwritten, reset when the row is emptied, carried through a reflow, the state export and the snapshot.
  - A character that overwrites half of a wide one leaves the other half a written space, as tmux does. `ESC[38;5;1m` matches since patch 0010 (oracle `capture_sgr_256_low16`).
  - `cellsize`, and the `CLEARED` flag of an erased cell, are still derived from the cells as they are now (`capture.rs` `tmux_cellsize`, `capture/raw.rs`):
    - `cellsize` from the runs of collectable written cells with one style, charset and link, and each cell tmux writes alone (a wide or non-ASCII character, a tab, line drawing) as one write after its padding;
    - a cell erased to an RGB background is not `CLEARED`.
- **What shows differently:**
  - a line erased and rewritten shorter (tmux keeps the allocation);
  - a line whose run tmux flushed in two pieces: plain text with an escape sequence in it that changes nothing (`zz ESC[m out`: tmux allocates up to the second piece's end, 5/5 on a 20-column line; vtmux sees one run, 5/10), and likewise a run split by the end of a read;
  - a line whose characters past ASCII were overwritten;
  - the rows a reflow makes; also the line flags after a run of resizes: 1-4 lines in about 550 show `-` where tmux shows `X` (narrow characters only, seeds 1, 2, 3, 5, 7 and 8 of a resize-storm probe, on main and with patch 0038 alike; 2026-11-10 review of `panel_g2_persist`);
  - a once-extended cell erased (`CLEARED` in vtmux);
  - `cellused` after an erase (vtmux counts the cells left);
  - under `-N`, the trailing blanks follow the same model.
- **Evidence:** reference tmux 3.8 against vtmux, history-limit 0.
  - 10x3 `printf '\343\201\202B\rX'`: both `flags=EXTENDED[2] 3/5`, cell 0,1 `flags=NONE[0]` (before patch 0034 vtmux gave `flags=NONE[0]` and `flags=CLEARED[40]`).
  - A probe of 20 such scenes (overwritten wide characters, links, RGB and strikethrough text, tabs, erases, a reflow): 13 differed before the patch and 3 after (two `cellsize`, one `CLEARED`). The two reflow scenes were compared with history-limit 100, as a pane without history drops the rows a narrowing pushes up (D-006).
  - 2 differ once the cells written alone and RGB erases were modelled: 10x3 `printf '\303\251a\rxy'` tmux `2/2`, vtmux `2/5` (the `é` is gone).
  - Of 27 scenes of cells written alone (accented, combining, wide, tab, line drawing, mixed, on 8, 10, 12 and 20 columns), 7 differed before that model and none after, nor do the RGB erases.
  - 20x3 `printf 'zz\033[mout'`: tmux `capture-pane -pN` gives `zzout`, vtmux `zzout` and five spaces.
  - 10x3 `printf 'a\303\251b\r\033[1C\033[K'`: tmux `3/5` and cell 0,1 `NONE`; vtmux `1/2` and `CLEARED`.

<a id="d-028"></a>
### D-028: closed (the editor runs in a modal floating pane)

- **Was:** customize-mode's and choose-buffer's `e` ran `editor file` in a popup over the client whose key pressed it, not in a modal floating pane as `spawn_editor` does (spawn.c:703-788). So `list-panes` did not show it, and other clients saw only `WAITING FOR EDITOR`.
- **Now:** `crates/vtmux-server/src/editor.rs` runs it as tmux does: a floating pane nine tenths of the window, centred, after the whole layout, modal and above a zoomed pane, `remain-on-exit` off. The file is read back when the pane's program exits 0 (`spawn_editor_finish`), and a pane killed first changes nothing. Regress `modal-pane.sh` passes; oracle `editor_modal_pane`.

<a id="d-029"></a>
### D-029: a client whose socket takes nothing for 10 s after its exit is dropped then

- **Category:** Deferred: the exit on a channel of its own.
- **Test:** regress `control-client-exit-stalled.sh`; control `stalled_client_does_not_keep_the_server`, `slow_reader_detached_in_a_flood_exits_cleanly`.
- **tmux:** `server_client_check_exit` and `server_client_exit_timer` (server-client.c:2361-2440):
  - A control client told to exit first loses the pane output not yet written (`control_discard`).
  - If what is left has not drained after 10 s, everything queued is dropped (`control_discard_all`) and `MSG_EXIT` sent alone. A client that has still not closed 10 s later is dropped.
  - A control client's output goes down its own stdout descriptor and `MSG_EXIT` down the server socket, so a stalled terminal never holds up the exit message.
  - The tmux client itself can still hang there. When its full stdout has fewer than 6 bytes free, its `%exit` `fflush` fails with `EAGAIN` (stdout is still non-blocking); `client_main` then makes stdout blocking, and the flush at `exit()` blocks for good. So its server goes only at the second 10 s step, 20 s after `kill-server`.
- **vtmux:**
  - The same steps, but the output and the exit message share the one socket, and a message the socket has taken part of must be finished first.
  - So at the first 10 s step, a client whose socket has taken nothing for 10 s is dropped at once instead of being sent the exit alone. One still reading gets its exit after the part-written message.
- **Why it stays:**
  - Closing it needs the exit message on a channel of its own: a second socket per control client that the client polls beside the first. That has to be carried through a live upgrade (D27: the snapshot lists the client sockets it hands on), and it changes the handshake.
  - It buys only what a client does after a stall longer than 10 s: tmux's client gets `MSG_SHUTDOWN` and prints `%exit` when its stdout frees, vtmux's finds the socket closed.
  - The server's own exit is bounded at the first 10 s step in vtmux; in tmux at the first, or (when its client hangs in the `fflush`) the second.
- **Evidence:**
  - `control-client-exit-stalled.sh` passes on both, 12.1 s each (`cargo xtask regress --tmux reference --only control-client-exit-stalled.sh`).
  - A reader 64 KB per 10 ms behind a `yes` flood, then detached: both exit 0 with `%exit` within 0.5 s (reviewer repro rv_det2.py).

<a id="d-030"></a>
### D-030: closed (DECAWM as tmux)

- **Was:**
  - Ghostty set its pending-wrap flag on a print reaching the last column under DECAWM reset too, and a print with the flag set and DECAWM reset replaced the last cell (xterm).
  - So a wrap pending when `?7l` arrived reported column 9 of 10 (tmux 10) and took the next characters; one filled under `?7l` and then `?7h` reported 10 (tmux 9) and wrapped.
  - Ghostty also did not reflow the main screen while DECAWM was reset.
- **Now:** vendored patch `0016-tmux-decawm.patch` sets the flag only under autowrap, drops a character printed with the flag set and autowrap reset (`screen_write_cell`, "Sanity check cursor position"), and reflows the main screen whatever DECAWM says (`window_pane_resize`). `term.rs` `tmux_cursor` reports the column count whenever the flag is set. Goldens `cap_decawm_*`, `fmt_decawm_*`; oracle `decawm_resize_alt`.

<a id="d-031"></a>
### D-031: single characters the host's `wcwidth` and Ghostty's table give other widths

- **Category:** Deferred. Following the host's `wcwidth` would put one glibc version's table over Ghostty's for every code point. `vtmux-core` `width.rs` has the reference host's table, which the status line uses.
- **Test:** none fails; reproductions below.
- **Fixed earlier** (clustering and tmux's own table):
  - *Was:* Ghostty's clustering and widths, not tmux's.
    - A lone regional indicator took two columns (tmux one, `utf8_default_width_cache`, utf8.c:53-), as did the third of three. U+261D and the rest of tmux's table took Ghostty's width.
    - U+FE0F after a character that is not an emoji was dropped; tmux joins it and makes the cell two columns (`screen_write_combine` with `variation-selector-always-wide`). U+FE0E narrowed an emoji (tmux: no width change).
    - A character after a ZWJ that does not continue an emoji sequence took a cell of its own (tmux joins anything to a cell ending in a ZWJ). A skin-tone base after its modifier stayed apart (`utf8_should_combine` joins either order). U+3164 was kept (tmux drops it).
    - 20x3 pane: `printf 'x\360\237\207\254y'` gave `#{cursor_x}` 4 (tmux 3), `printf 'x\357\270\217y'` 2 (tmux 3).
  - *Now:* vendored patch `0027-tmux-clusters.patch` makes mode 2027 join characters by tmux's rules (`screen_write_combine`, `utf8_should_combine`, `hanguljamo_check_state`), switched on in the server with `variation-selector-always-wide` (`Server::update_width_cache`).
    - tmux's own width table goes to the grid as overrides under `codepoint-widths` (`vtmux_core::width::grid_width_ranges`), less the nine text-presentation emoji it makes two columns (D-045).
    - The compositor moves the cursor after a cell that a joined character made two columns wide, as tmux's `TTY_CTX_CELL_INVALIDATE` does (`tty_cmd_cell`).
    - Oracle `tmux_char_widths`; cli `program_output_reaches_the_grid_as_in_tmux` case `widths`; 400 random sequences of 30 such characters compared with the reference by `cursor_x` and `capture-pane`, all equal.
- **tmux:** a character's width is `wcwidth` in the server's locale, after tmux's table (`utf8_width`). On the reference host (glibc 2.43, `en_US.UTF-8`), U+2028 is one column, U+3248-U+324F two, U+2E3A and U+2E3B one.
- **vtmux:**
  - Ghostty's table, after tmux's table and `codepoint-widths`: U+2028 is zero width (joins the cell before), U+3248 one column, U+2E3A two.
  - On the reference host the two disagree on 45 ranges, mostly unassigned code points (planes 2 and 3, U+E01F0-U+E0FFF). glibc's own table changes with its version and locale (the cli harness's locale gives U+2028 zero width).
  - A cell that U+FE0F or a second regional indicator joins in a pane's last column: D-044.
  - On a terminal that counts a character's width otherwise than tmux's grid, tmux's idea of the terminal's cursor drifts (`tty_cursor` skips a move to where it thinks it is). vtmux moves the cursor whenever it skips cells and at the end of each frame, so such screens can differ (oracle `tmux_char_widths` takes its screen where they agree).
- **Evidence:** 20x3 pane, reference tmux 3.8 under `en_US.UTF-8`: `printf 'c\342\200\250d'` gives `#{cursor_x}` 3, vtmux 2; `printf 'c\343\211\210d'` 4, vtmux 3; `printf 'c\342\270\272d'` 3, vtmux 4.

<a id="d-032"></a>
### D-032: closed (a scroll region below the top feeds history as tmux's does)

- **Was:** Ghostty created scrollback only for a region whose top margin is row 0 (Terminal.zig `index`, `scrollUp`), so a line scrolled out of a region starting lower was lost. 6x4 pane, `printf 'A\nB\nC\033[2;3r\033[2;1HX\033D\033DY'`: tmux `#{history_size}` 1 and capture `-S -` starts `X`; vtmux 0.
- **Now:**
  - Vendored patch `0026-region-scroll-into-history.patch` moves the region's top row into history for LF/IND at the region's bottom and for SU (at most the region's height), on the primary screen. That is what `grid_view_scroll_region_up` (grid-view.c:107-117) does with `grid_scroll_history_region` (grid.c:529-560): the rows above the region stay, the history row keeps its wrap flag, the region's bottom row is cleared with the current background.
  - The alternate screen still moves lines in place (no `GRID_HISTORY`). A pane created with `history-limit 0` scrolls its region as one with history, as before (D-006).
  - Regress `input-raw-scroll.sh` (scene `index`) passes; goldens `cap_region_scroll_history`, `fmt_region_scroll_history_size`; oracle `region_scroll_history`.

<a id="d-033"></a>
### D-033: attach also asks the terminal for palette entries 0-15 (OSC 4)

- **Category:** Spec decision (D26: the pane blend mixes palette cells from the terminal's own palette).
- **Test:** attach `colour_queries_at_attach_and_on_a_theme_report` (the OSC 4 queries are the one byte-level difference it allows); oracle `terminal_colour_replies` (the replies change nothing on screen).
- **tmux:** `tty_send_requests` (tty.c:406-425) ends with `ESC]10;?ST ESC]11;?ST`; `tty_repeat_requests` (tty.c:428-451) sends the same two after a theme report. Palette replies are parsed (`tty_keys_palette`) only to answer a pane's own OSC 4 query.
- **vtmux:**
  - The same two, then `ESC]4;N;?ST` for N 0-15, both at attach and after a theme report.
  - The replies are kept with the client's other reported colours (`term_queries::Reported`). While one of these queries is out, they answer no pane's OSC 4 request: tmux sends none, so vtmux must not answer or drop one.
  - Nothing changes on screen unless `vtmux-pane-active-blend` or `vtmux-pane-inactive-blend` is set.
- **Evidence:** the attach test above compares both binaries' startup bytes for OSC 10/11 and asserts the OSC 4 queries only for vtmux.

<a id="d-034"></a>
### D-034: closed (a search matching every line of a long history counts it all)

- **Was:** a match on each of 100000 lines ran out the 200 ms of `window_copy_search_marks` (window-copy.c:4876-5003, `WINDOW_COPY_SEARCH_ALL_TIMEOUT`) and counted `1000+`, where tmux counts exactly (0.11 s). A line cost about 2.6 µs to read and stringify (`ScreenCopy::used_row`: a page-list walk, a `RowCell` for each cell, then a `Cell` for each).
- **Now (plain ASCII lines):**
  - A line out of view, narrow ASCII and not wrapped is counted from its text alone. `ScreenCopy::ascii_text` reads the row's cells in one call and turns the bits into a string (no cell structures).
  - `Grid::count_ascii` runs the regex (or the plain compare) over it. A cell is a byte, so a match's cell is its offset and its width its length (`window_copy_cstrtocellpos` can find nothing else), and the next search starts at its end with `REG_NOTBOL` (`window_copy_search_lr_regex`).
  - Any other line (wide, accented, tab, line-drawing, wrapped, in view) is read as cells as before.
  - Unit test `counting_from_text_agrees_with_counting_cells` holds both counts equal over plain, coloured, accented, wide, tab and wrapped lines and 17 patterns, and checks that the text path counted the ASCII lines.
  - Evidence: release build, 80x24, `history-limit 200000`, `seq 1 100000 | sed 's/$/ some filler text here/'`, `copy-mode`, `send -X search-backward RE`, `#{search_count}#{?search_count_partial,+,}`. Reference / vtmux (count, seconds):

    | Pattern | tmux | vtmux |
    |---|---|---|
    | `filler` | 100000, 0.119 | 100000, 0.051 |
    | `some f` | 100000, 0.115 | 100000, 0.052 |
    | `^5` | 11111, 0.067 | 11111, 0.145 |
    | `^5.*0.some` | 1111, 0.079 | 1111, 0.156 |
    | `fill.r` | 100000, 0.174 | 100000, 0.186 |
    | `f[a-z]+r` | 100000, 0.192 | 100000, 0.192 |

  - Before: vtmux `1000+` for `filler` and `some f`; 0.26 s for `^5`. A regular expression on every line now takes about what tmux takes (the regex engine, not the row read, is most of it); the 200 ms is wall clock in both.
  - What is left of the row read is mostly Ghostty's walk from the oldest page to the row (`PageList.pin`, 17% of the time before this change). Decoding a page's rows in one call would need a Ghostty patch.
- **Later (task `low_history_bytes_width_search_nonascii`): patterns past ASCII.**
  - A plain search for text with a character past ASCII (`été`, `é`, `中`) over a long ASCII history still read every line as cells. The count came out empty after the 200 ms (`-1`; tmux `1`/`4`/`1` in 0.18 s on a history of 100000 lines with three such lines), and the jump to the match took as long again.
  - Now a pattern no ASCII line can hold (a tab, a padding cell or a character past ASCII among its cells: `Pattern::never_in_ascii`) counts none on every narrow, unwrapped ASCII line from its text, and the jump passes those lines unread.
  - A regular expression passes a line unread when the line's whole text has no match (`Regex::span_with`). Not the first line going down, which may be searched from the middle: `\<` can match there what the whole line does not.
  - The regex engine skips to the next byte a match can start with (`Nfa::first`, a table over the pattern's first units; a character past ASCII always counts as one), and an anchored pattern looks at the first byte only. A line without a candidate costs a scan, not a simulation: `été` over an ASCII line 2.0 µs -> 0.04 µs, `f[a-z]+r` 0.52 -> 0.33 µs. The count asks for the span only, not the captures.
  - Unit tests: `counting_from_text_agrees_with_counting_cells` (now with the jump position, 31 patterns, forward and back), `a_pattern_past_ascii_skips_ascii_lines_unread`, regex `skipping_to_a_first_byte_changes_no_span` and `span_with_is_capture_zero`. Oracle `copy_search_nonascii_history` compares counts, matches and cursors with the reference.
  - The oracle found a second difference, fixed with it. `window_copy_is_lowercase` (window-copy.c:4509-4518) compares a signed `char` with `tolower((u_char)c)`, so a search string with a character past ASCII is case-sensitive: `é+` counts 6, not the 8 that matching `É` too gives; `É+` counts 2. vtmux matched ignoring case (`Pattern::cis`, now `is_lowercase`).
  - The reference is built for x86-64 Linux, where `char` is signed (as on macOS). On a target where it is unsigned, tmux would ignore case there.
  - Evidence: same host, load about 70 on 96 cores, release build, 80x24, `history-limit 200000`; 100000 lines of `N some filler text here`; a second history (`s2`) with three lines `un été chaud`, `é and é`, `中文` in the middle. Count and seconds:

    | Pattern | tmux | before | after |
    |---|---|---|---|
    | `été` (s2) | 1, 0.18 | empty, 0.33 | 1, 0.074 |
    | `é` (s2) | 4, 0.19 | empty, 0.33 | 4, 0.074 |
    | `中` (s2) | 1, 0.18 | empty, 0.33 | 1, 0.074 |
    | `é`, no match | 0, 0.12 | 0, 0.24 | 0, 0.06 |
    | `[0-9]+` | 100000, 0.19 | `1000+`, 0.21 | 100000, 0.074 |
    | `f[a-z]+r` | 100000, 0.17 | 100000, 0.21 | 100000, 0.081 |
    | `fill.r` | 100000, 0.18 | 0.20 | 0.072 |
    | `^5` | 11111, 0.063 | 0.125 | 0.073 |
    | `filler` | 100000, 0.125 | 0.049 | 0.054 |

  - `filler` takes the plain ASCII path, untouched. The before runs on this loaded host sit at the 200 ms limit, which is why `[0-9]+` and `f[a-z]+r` fell to `1000+` there.
- **Later still (task `perf_search_styled_history`): styled lines.**
  - A coloured history whose lines have a character past ASCII (a log line ending in `é`) was read as cells line by line. A count over 100000 such lines ran out the 200 ms where tmux finishes: `1000+` for `status=200 bytes` (tmux 100000 in 0.19 s) and for `items/[0-9]+5 ` (tmux 9999 in 0.15 s).
  - Now a line is counted from its text when every cell is one character of one column, of any style, past ASCII or not (`ScreenCopy::text`, `CopyMode::count_text`, `Pattern::never_in_text`; the text is UTF-8 and a match is looked for from a cell's first byte). 100000 in 0.05 s and 9999 in 0.11 s.
  - A pattern that runs out tmux's time too (`INFO` on every line: tmux `1000+` in 0.20 s) counts exactly in vtmux (100000 in 0.04 s), as for the plain lines above.
  - Unit tests `styled_lines_search_from_text_as_from_cells` (random styled content, 30 patterns, up and down, again and reversed: counts, cursor and `#{search_match}` equal to reading every line as cells) and `a_styled_history_is_searched_without_reading_cells`. A CLI probe against the previous build, 9 seeds x 680 search steps: 0 differ (`docs/test-health/2026-11-09-copy-mode-styled-history.md`).

<a id="d-035"></a>
### D-035: closed (control output backpressure is per line and round-robin)

- **Was:** the reply cap applied to each flush of a command's lines (`%begin`, output and `%end` together). So the stream before `%exit too far behind` stopped at a flush boundary (after an `%end`; tmux after a `%begin`), and pane output went out in the order it was read.
- **Now:**
  - `control_check_reply_buffer` (control.c:429-451) runs for each `control_write`: each line a command prints, each guard, each notification (`control.rs` `Pending`, `control_flush_lines`). It counts what the connection holds plus what the flush has handed it, so the stream stops after the last line that fits.
  - Pane output goes as `control_write_callback` and `control_write_pending` send it (control.c:889-958; `control_queue.rs`):
    - every pending pane takes a turn of the same share (a third of the room left, over the panes pending when the round starts, at least `CONTROL_WRITE_MINIMUM`), in the order they became pending;
    - a turn is cut across the pane's blocks;
    - lines wait for the blocks queued before them and go as soon as those are sent, in the middle of a turn if need be;
    - a pane whose blocks were discarded stays pending until its turn finds nothing;
    - a new callback starts again at the first pending pane.
  - cli `reply_buffer_cap_lets_a_stalled_client_go` now compares where the stream stops (the line before `%exit`: `%begin`, as tmux; before, `%end`). Unit tests in `control_queue.rs`: `panes_take_turns`, `a_share_runs_across_blocks_and_stops_for_a_line`, `discarding_a_pane_keeps_the_lines_and_other_panes`.
- **Residual:**
  - The buffer measured is the server's queue for the socket (with the socket and the relaying client in front of the reader), not the client's own stdout descriptor. So the byte at which a stalled client is let go differs by what sits in those (the stream is under 70 MB on both).
  - How panes interleave under a backlog depends on buffer timing in tmux too, and is pinned only by the unit tests.

<a id="d-036"></a>
### D-036: a line keeps no cells past the width it was narrowed to

- **Category:** Ghostty grid: a row holds as many cells as the pane is wide.
- **Test:** oracle `reflow_cells_used` scene `grow` (annotated, the last `capture-pane -e`).
- **tmux:** a line that fits the narrower width is moved as it is (`grid_reflow_move`, grid.c), so cells past the new width stay allocated (`cellsize`) and are drawn again when the pane widens. A line written `ab`, then erased to its end with a red background on 20 columns, narrowed to 10 and widened back to 20, is red to column 20.
- **vtmux:** Ghostty's reflow copies a row into rows of the new width, so the cells past it are gone: the same line is red to column 10, then default. Narrowing itself matches (patch 0013 measures the line by tmux's `cellused` and keeps the background cells up to the new width).
- **Evidence:**
  - Reference tmux 3.8, `printf 'ab\033[41m\033[K\033[0m\r\nnext'` on 20x8, `resize-window -x 10`, then `-x 20`. `capture-pane -pe`: tmux `ab\e[41m` / `\e[49mnext`; vtmux `ab\e[41m        \e[49m` / `next` (eight red cells, then the default background).
  - Widened past the first width the same way: a line erased in green and then written (`printf '\033[42m\033[K\033[0mafter-bg-erase\r\n'`, 20 -> 15 -> 25 columns, oracle `decawm_resize_alt` scene `bgerase`, annotated). tmux keeps the green cells to column 20 and its `capture-pane -pe` trims them (`after-bg-erase\e[42m`); vtmux has one green cell and default cells after it (`after-bg-erase\e[42m \e[49m`).
  - 80 -> 60 -> 100 columns (review_reflow_cells_used): tmux `after-bg-erase\e[42m` (its cells kept to column 80, trimmed); vtmux `after-bg-erase\e[42m` and 46 green blanks to column 60, then `\e[49m`.

<a id="d-037"></a>
### D-037: a client that starts as `kill-server` finishes, or as the last session goes, finds no server, not a dropped connection

- **Category:** Do not copy tmux bug: a command that follows `kill-server` is lost to a server that is already leaving.
- **Test:** cli `kill_server_closes_the_socket_before_it_answers` and `exit_empty_closes_the_socket_before_it_answers` (vtmux only); oracle `format_loops` and `config_errors` start a server right after `kill-server`.
- **tmux:** `kill-server` raises SIGTERM, and the listening socket stays open until the process exits (`server_loop`, server.c:268-306). A client that connects meanwhile is accepted and closed (`server_accept`, server.c:396-399): `server exited unexpectedly`, and a following `new`/`start-server` never starts a server. The window is a few ms.
- **vtmux:**
  - With no job the server waits for (`job_still_running`), the socket and the `tmux` shim are closed before `kill-server`'s client is answered. So the next client is refused and starts a fresh server.
  - With a job running, the socket stays open as in tmux (`kill_server_with_running_job`), and the shim with it. When the server then exits, the shim goes before the socket stops listening, so it never removes the shim of a server started after it (server.rs `a_fresh_server_keeps_its_shim_when_the_old_one_exits`).
  - The same for a server that exits with `exit-empty` (`server_loop`, server.c:268-306: no session left, a session-less `start-server`, the last `kill-session`). When the command clients answered in a turn leave nothing queued or running, and every other client is on its way out, the socket closes before they are told (server.rs `exits_once_told`).
- **Evidence:**
  - `kill-server` then `start-server ; set -g exit-empty off` at once, 8 busy loops, 150 rounds each: vtmux 11/150 failed (`server exited unexpectedly`, then `no server running`), reference tmux 0/450. Its window is shorter; vtmux's turn runs render and compression between the reply and the socket close.
  - With the change, the cli test passes 60/60 under load; without it, 37 of 81 runs fail.
  - The `exit-empty` path: `kill-session` of the last session, then a connect at once, 20 rounds. Before the change the socket was still listening in round 1; after it, refused in every round. cli `client_window_target_without_a_client` hit it (vtmux's `new` after a session-less `start-server`, `server exited unexpectedly`) on 4 loaded CPUs.

<a id="d-038"></a>
### D-038: the alternate screen keeps no cells past the width it was narrowed to

- **Category:** Ghostty grid: a row holds as many cells as the pane is wide.
- **Test:** oracle `decawm_resize_alt` scene `alt` (annotated, the `capture-pane` after widening back); oracle `alt_screen_exit` scene `pend47` (annotated, the `display` after `resize-window -x 40`).
- **tmux:** the alternate screen is resized without reflow (`window_pane_resize`: reflow only without `saved_grid`), and `screen_resize_cursor` then only changes the grid's `sx`. Every line keeps its cells, so narrowing hides the cells past the new width and widening shows them again, before the program redraws.
- **vtmux:** Ghostty's resize without reflow (`PageList.resizeWithoutReflow`) clears the cells past the new width, so widening shows blanks there until the program redraws.
  - Keeping them would need every full-line clear and scroll to clear past the width too (tmux's lines go whole, `grid_empty_line`), and Ghostty's page bookkeeping (style counts, clone, compression) to cover cells past the width.
  - A program that redraws on SIGWINCH (pi, `scripts/agent_tui.py`, a full-screen editor) hides it. It shows between the resize and that redraw, or in a program that does not redraw.
- **The cursor too:**
  - `screen_resize_cursor` keeps `cx` past a narrower alternate screen's width (only the end of the alternate screen clamps it, `screen_alternate_off`). So tmux reports `#{cursor_x}` 80 on a pane narrowed to 40 under the alternate screen.
  - Ghostty's cursor stays on the screen, in its last column (40 with a wrap pending, 39 without).
  - The end of the alternate screen matches again (the cursor goes to the end of its line).
- **Evidence:** reference tmux 3.8, a pane on 20 columns printing `\033[?1049h\033[?7labcdefghijklmnopqrstuvwxyz\r\n0123456789abcdefghij\r\nshort`, then `resize-window -x 14` and `-x 20`. `capture-pane -p`: tmux `abcdefghijklmnopqrsz` / `0123456789abcdefghij` / `short`; vtmux `abcdefghijklmn` / `0123456789abcd` / `short`.

<a id="d-039"></a>
### D-039: a blank cell left of a vertical border is written as a space to carry the bidi isolate

- **Category:** Ghostty grid: libghostty-vt's rule for a zero-width character after an empty cell. No arbiter run: tmux's grid and Ghostty's disagree, and vtmux's bytes give tmux's result on tmux's grid.
- **Test:** oracle `bidi_border_isolates` (annotated, client b's two screens: the rows whose cell left of the border is blank). Regress `screen-redraw-bidi.sh` passes.
- **tmux:**
  - For a UTF-8 client whose terminal has `Bidi`, `redraw_draw_border_span` (screen-redraw.c:1256-1264) moves the cursor to a vertical border span and writes U+2069 there. This comes after the pane line was drawn with its trailing blanks erased (`tty_draw_line`, EL or ECH for ten or more).
  - tmux's own grid joins the zero-width character to the erased cell left of the cursor (`screen_write_combine`), so tmux in tmux shows it.
  - libghostty-vt drops a zero-width character after a cell nothing was written to (Terminal.zig `print`), so an observer, or vtmux as the outer terminal, does not.
- **vtmux:** frames are cells, so the isolate goes on the cell left of the span, and a blank one is written as a space carrying it. vtmux in tmux and vtmux in vtmux show what tmux in tmux does (the regress scene, both ways). A libghostty-vt observer shows the U+2069 on those rows, where tmux's own isolate is dropped.
- **Evidence:** regress `screen-redraw-bidi.sh` passes with the outer server reference tmux 3.8 and the inner vtmux, and with vtmux in vtmux. The oracle trace of tmux 3.8 shows no U+2069 left of the border on rows with no text there (`bidi_border_isolates` step 17).

<a id="d-040"></a>
### D-040: closed (a tab wider than the pane gets tmux's empty rows on a reflow)

- **Was:** `grid_reflow_split` (grid.c) moves to the next line before every cell for which `width + gc.data.width > sx`, also with nothing on the line yet. So a TAB cell wider than the pane where a line splits leaves an empty line before it, and a line of only that tab two. vtmux started the next row with the tab: `printf 'ab\r\n\tcd\tx'` on 20x6, `resize-window -x 7` then `-x 5`, tmux `#{history_size}` 7, vtmux 5.
- **Now:**
  - Vendored patch `0029-tmux-grid-corners.patch` makes those rows. It keeps the empty lines a join stops after when the cell does not fit even an empty line (each later narrowing adds two more), and starts a line of its own after a row tmux fills exactly (a wide character's padding counted).
  - Oracle `reflow_tmux_quirks` scene `tabwide` (no longer annotated) and `reflow_wide_tab` (8 scenes, up to five resizes each); a random reflow probe of 780 seeds (tabs, wide characters, erases) matches tmux except for D-041 seeds.

<a id="d-041"></a>
### D-041: a reflow that joins a whole row keeps the line's soft wrap where the next row does not fit

- **Category:** Do not copy tmux bug: the soft wrap of a line is lost on a resize.
- **Test:** oracle `reflow_tmux_quirks` scene `lostwrap` (annotated, the `display` and `capture-pane -J` after widening).
- **tmux:** `grid_reflow_join` (grid.c) sets `wrapped` from the next line before it tests whether that line's first cell fits. When it does not, and the line before it was joined whole, `left` is 0 and the target line loses `GRID_LINE_WRAPPED` although the rest of the line follows. The logical line becomes two (`capture-pane -J`, copy mode and later reflows see two lines), and the cursor at the line's end goes to the end of the first.
- **vtmux:** the line stays one wrapped line; the next row is its continuation.
- **Evidence:** reference tmux 3.8, `printf 'abclklongwordlongword longwordlongword 字漢😀'` on 40x6, `resize-window -x 9` then `-x 23`. tmux `capture-pane -pJ` gives `abclklongwordlongword longwordlongword 字漢` / `😀`, and `#{history_size} #{cursor_x},#{cursor_y}` `1 20,0`; vtmux one line ending `字漢😀`, `1 2,1`.

<a id="d-042"></a>
### D-042: a character joined after a wide character whose padding cell it overwrote stays on screen

- **Category:** Do not copy tmux bug: a character on the line is hidden from the screen.
- **Test:** oracle `reflow_wide_padding_history` scene `redraw` (annotated, the screen after widening to 200).
- **tmux:**
  - A narrowing reflow can leave a wide character's padding cell to start the next row on its own (`grid_reflow_split` and `grid_reflow_join` test `width + gc.data.width > sx` for it too).
  - A character written over that cell replaces it (`screen_write_overwrite` at column 0 clears only the cell itself). When a widening reflow joins the rows again, the character becomes the cell after the wide character, where its padding was.
  - `capture-pane` prints it (`grid_string_cells` writes every cell that is not padding), but the screen does not: `tty_draw_line` steps over a cell's width (`i += gcp->data.width`). So the character is hidden under the wide character, and the rest of the line is drawn a column left of where `capture-pane` and the cursor put it.
- **vtmux:** Ghostty keeps every wide character with its spacer tail, so the character follows the wide character's second column. The screen shows it, and the rest of the line a column right of tmux's screen; `capture-pane` gives what tmux's does.
- **Evidence:**
  - Reference tmux 3.8: `$ echo` and 37 `字` printed on 80 columns, narrowed to 17, then the bytes bash 5 wrote for its redraw there (`\r ESC[K ESC[A`, `$ echo` and the 37 `字`, a space and `ESC[K` at each row end), widened to 200.
  - tmux's screen shows `…字字 echo 字…`, its `capture-pane -p` `…字字$ echo 字…`; vtmux both `…字字$ echo 字…`.
  - Found by review_lows_batch_k with bash readline. After bash's next redraw tmux shows 47 `字` in a row; vtmux 37, a blank, then 10.

<a id="d-043"></a>
### D-043: `capture-pane -e` writes the link of a line that starts inside the one the line above ends in

- **Category:** Do not copy tmux bug: a link is lost from the output.
- **Test:** oracle `reflow_wide_padding_history` scene `link` (annotated, `capture-pane -p -e -S -`).
- **tmux:** `grid_string_cells` closes a link still open at the end of each line (`has_link` is per call), but `cmd_capture_pane_history` hands it the last cell of the line before (`lastgc`). So a line whose first cell is in the same link as the last cell of the line before writes no OSC 8 for it, and its text comes out unlinked. With `-J` the two are one line and nothing is lost.
- **vtmux:** such a line opens the link again, as a line that starts in any other link does.
- **Evidence:** reference tmux 3.8, two lines of twelve `ESC]8;id=lIJ;http://e.x/I/J ESC\ LJ字 ESC]8;; ESC\` links and a space on 80 columns, narrowed to 33. tmux's second row starts `字 ESC]8;id=l06;…`, the `字` of link `l05` unlinked; vtmux's starts `ESC]8;id=l05;http://e.x/0/5 ESC\ 字 ESC]8;; ESC\ ESC]8;id=l06;…`. Found by review_lows_batch_k (links resized 80, 17, 200, 33x10).

<a id="d-044"></a>
### D-044: cells a terminal may draw wider than the grid counts them

- **Category:** Do not copy tmux bug: on such a terminal tmux's own output scrolls the screen, covers the border and moves text, and next to a border the joined character is overwritten.
- **Test:** oracle `vs16_last_column` (annotated: every screen, and `capture-pane -p` of the split pane), `tmux_char_widths` (the screen, annotated), `text_presentation_status` (a lone indicator in `status-left`, annotated D-045); compositor tests `a_cell_that_did_not_fit_wide_goes_out_narrow`, `a_lone_regional_indicator_moves_nothing_after_it`.
- **tmux:**
  - U+FE0F, a second regional indicator or a skin-tone modifier joining a one-column cell makes it two columns (`screen_write_combine`, with `variation-selector-always-wide` for U+FE0F), even in the pane's last column, its padding past the edge.
  - *At the window's right edge* that cell does not fit (`window_visible_ranges`), so it is not redrawn and the cursor stays past the edge. The terminal keeps what it was sent before the join (the narrow base, or a lone regional indicator), and the next character wraps.
  - *Next to a border* the cell is visible. tmux sends it whole (`tty_cmd_cell`), and `screen_write_set_cursor` clamps the cursor back onto the cell, so the next character overwrites it instead of wrapping (`capture-pane` loses the joined character).
  - Any redraw draws the cell as empty, as it is wider than the room left (`tty_draw_line_get_empty`, tty-draw.c).
  - A lone regional indicator is one column (`utf8_default_width_cache`) and is sent like any other cell, as is a one-column cell that a ZWJ joined an emoji to (`x` ZWJ `😀`).
  - A terminal that draws these two columns wide (Ghostty does) then wraps one in the last column onto the next line (scrolling the screen in the bottom-right cell), covers a border beside it, and draws the rest of the row a column right.
- **vtmux:**
  - The grid keeps a joined cell in the last column one column wide, with the wrap pending (Ghostty's grid has no cell past the edge), so the next character wraps wherever the pane is.
  - The compositor never sends what may be drawn two wide in a one-column cell (`render.rs` `outer_text`, `write_regional`):
    - a joined cell goes out as its base character without the joining characters (`❤` for `❤️`, `x` for `x` ZWJ `😀`), and a flag as a space, keeping zero-width marks;
    - a lone regional indicator goes out with the cursor moved after it and the cell after it written too: first when that cell is a blank of the same style (which the indicator then covers on such a terminal, as a program that counts it two wide, such as vim, means), else after it (which there wipes the indicator out);
    - in the last column, a lone indicator goes out as a space.
  - So the screen neither scrolls nor loses a border, and nothing after such a cell moves, on a terminal of either width. A redraw shows the base where tmux shows a blank.
  - A lone indicator followed by text is not shown on such a terminal: status-left `L🇯x ` shows `L x `, a pane's `cd🇯ef` shows `cd ef`. tmux (and vtmux before patch 0027) shows it with the text after it a column right.
    - tmux's grid and status layout give it one column (`utf8_default_width_cache`; `format_width` counts each indicator one, so a flag is two). A terminal that draws it two wide cannot show it and the next character where both belong, so vtmux keeps the next character in place rather than move the rest of the row (or, next to a border, cover it).
- **Evidence:** reference tmux 3.8 on 30x10 under the oracle's Ghostty observer (`vs16_last_column`):
  - `<29 letters>❤️z` shows the heart, then `z`. A redraw blanks the heart; vtmux keeps `❤`.
  - `<29 letters>🇯🇵z` shows `🇯z` on the next row (the indicator wrapped); vtmux: blank, `z`.
  - A lone `🇯` in the bottom-right cell scrolls the screen a line (`top` lost); vtmux: a space.
  - With the window split 19|10: `<18 letters>❤️` covers the border (`…qr❤️right`); `<18 letters>❤️z` leaves `…qrz` on one row in tmux's grid, where vtmux wraps the `z`; after `refresh-client` the right pane reads `rright` and ` iright` next to rows with lone indicators.
  - Before the compositor change vtmux sent `❤️` and the indicators as they are, and the observer scrolled a line, lost the border and moved text after a lone indicator, as under tmux.
  - vim 9 (`-u NONE`, `nowrap`) showing a file of such lines in the split pane: tmux's screen draws text after the indicators out of place (`b🇯🇵🇵cc👍`); vtmux's before the change lost the border on four rows; after it every row keeps its border and its text in place.

<a id="d-045"></a>
### D-045: text-presentation emoji tmux's table makes two columns

- **Category:** Do not copy tmux bug: on the terminals that draw these one column, tmux misplaces the rest of the row, the split's border and the status line. With the program's widths vtmux places them where the program and the terminal agree.
- **Test:**
  - Oracle `text_presentation_split` (annotated: both screens, `#{cursor_x}` after U+261D, `capture-pane -p`), `text_presentation_status` (annotated: every screen, which also show D-044's lone indicator in `status-left`, and `#{w:}`), `tmux_char_widths` (`#{cursor_x}` after U+261D, annotated).
  - Core tests `text_presentation_is_one_column_alone`, `char_width_matches_reference_at_every_boundary`; compositor tests `a_wide_cell_drawn_narrow_blanks_its_second_column`, `a_lone_skin_tone_is_written_before_the_emoji_before_it`.
- **tmux:**
  - `utf8_default_width_cache` (utf8.c) makes nine characters two columns, in panes, the status line and `#{w:}`: U+261D ☝, U+26F9 ⛹, U+270C ✌, U+270D ✍, U+1F3CB 🏋, U+1F3CC 🏌, U+1F574 🕴, U+1F575 🕵 and U+1F590 🖐.
  - They are text presentation (`Emoji_Presentation=No`, `East_Asian_Width=N`). glibc's `wcwidth`, and so a program in the pane, gives them one, and so do Ghostty and other terminals that follow Unicode's widths.
  - On such a terminal tmux draws the rest of the row a column left of where it means it, and its idea of the cursor drifts:
    - a pane row written in one pass looks right on attach (`tty_draw_line` ends it with spaces), but the program's own cursor moves land a column off in tmux's grid. `✌ab✍cd`, a move to the column after `ab`, then `XY`: tmux's grid holds `✌aXY cd`, the program meant `✌abXYd`;
    - `refresh-client` draws every such row with the split's border a column left per character;
    - the status line draws its end a column left per character over what it showed before (a status-right `✌🏿⛹‍♂☝️|` after `🇯R✌Q` is followed by `R✌Q`).
  - With U+FE0F, or after a ZWJ, these are two columns in tmux and on such terminals alike.
  - tmux joins a skin-tone modifier to U+1F575 and U+1F590 (`utf8_should_combine`) but not to the other seven, which Ghostty joins as it prints.
- **vtmux:**
  - The nine are one column (`vtmux_core::width::TEXT_PRESENTATION`): in `char_width`, so in the status line, prompts, menus and `#{w:}`, and in the pane grid, which keeps Ghostty's width for them.
  - They are two with U+FE0F, or after a ZWJ joins a character to them (`Cluster::push`, patch 0027 `tmuxCombine`), as tmux's cell and the terminal are.
  - So a program's cursor moves land where it means them, and no border or status cell moves. `#{cursor_x}` after U+261D is 1 (tmux 2), and `#{w:}` counts each one (tmux two).
  - The compositor writes a two-column cell whose first character a terminal draws one wide on its own (such a cell joined to a skin tone, or one `codepoint-widths` widened) after a blank in its second column, then moves the cursor (`render.rs` `may_draw_narrow`).
  - It writes a skin-tone modifier that has a cell of its own before the character on its left, which a terminal would join it to as it prints it (`write_after_base`). So a terminal of either width shows the grid.
- **Evidence:**
  - Reference tmux 3.8 under the oracle's Ghostty observer (`text_presentation_split`, 60x8 split 34|25): after `refresh-client` the border is at column 33 on `abc☝defghijklmn|`, 32 on `✌ hello ✍ world|` and 27 on `🕴🕵🖐🏋🏌⛹✍z|` (vtmux: 34 on every row, before and after). The program's `✌abXYd|` reads `✌aXY cd|` (capture too).
  - `text_presentation_status`: the window name `a☝b🇯🇵c` is seven columns to tmux, six drawn; status-right `✌🏿⛹‍♂☝️|` leaves `R✌Q` of the one before at the line's end.
  - A probe of every two-column entry of tmux's table against Ghostty's `codepoint_width` found exactly these nine one column (each two with U+FE0F).
  - Before this, vtmux's grid took tmux's two (patch 0027), and vim on such a file in that split drew the border at column 32 on its rows.

<a id="d-046"></a>
### D-046: the history counters tmux 3.8 tracks that vtmux does not yet, and `display-message -v`

- **Category:** Deferred.
  - The counters need history bookkeeping beside Ghostty's (D-001's model).
  - `-v` needs a trace through the format engine.
- **Test:** cli `display_message_all` leaves these names out of both sides; everything else `display-message -a` lists is compared.
- **tmux:**
  - `#{history_added}`, `#{history_collected}` and `#{history_generation}` count the lines scrolled into history, the lines the limit collected (a tenth of `history-limit` at a time, grid.c `grid_collect_history`), and the clears and reflows (`scroll_added`, `scroll_collected`, `scroll_generation`, grid.c:455-521, 1609).
  - `display-message -v` writes each step of the expansion before the result (`format_log`, `FORMAT_VERBOSE`).
- **vtmux:** these variables are empty (so `display-message -a` does not list them), and `-v` prints only the result.
- **Evidence:** `new -d -s a cat -; display -p '#{history_added}|#{history_collected}'`: tmux `0|0`, vtmux `|`. `display -pv '#{session_name}'`: tmux five `# ...` lines then `a`, vtmux `a`.

<a id="d-047"></a>
### D-047: text `format_draw` writes into a pane half under a floating pane keeps its visible end

- **Category:** Do not copy tmux bug: text on the pane is hidden from the screen.
- **Test:** oracle `copy_position_under_floating` (annotated, the screen after `copy-mode`).
- **tmux:**
  - `screen_write_fast_copy` (screen-write.c:667-717), which `format_draw` writes through, sets each cell in the pane's screen, then `break`s at the first one a floating pane hides (`window_position_is_visible`). So the cells after it are never written, the visible ones included.
  - Copy mode's position indicator (`copy-mode-position-format`, drawn with `format_draw` on line 0, window-copy.c `window_copy_write_line`), right-aligned in a 30-column pane under a floating pane whose border ends at column 27, loses its last two cells. tmux shows blanks where `0]` belongs, and a full redraw (`refresh-client`) still does.
  - Text the program printed there shows (`screen_write_cell` writes every cell).
- **vtmux:** the mode's screen holds the whole indicator, and the compositor draws what the floating pane leaves visible: `0]`.
- **Evidence:** reference tmux 3.8, panes 30x15 and 29x15 side by side, a floating pane 30x3 at -3,1 (border columns 0-27, rows 0-4), `copy-mode` in the left pane: row 0 ends `┐` then two blanks; with `abcdefghijklmnopqrstuvwxyz0123` printed instead it ends `┐23`. Found by audit_layout_resize (a random mouse run: `M-MouseDrag1` moved the pane off the left edge, then a drag entered copy mode).

<a id="d-048"></a>
### D-048: a control client's stdin closing while its output is queued does not kill the server

- **Category:** Do not copy tmux bug: the server exits and every session is lost.
- **Test:** `crates/vtmux/tests/control.rs` `stdin_eof_while_output_is_queued_keeps_the_server` (vtmux only: the reference dies in about 1 run in 100 under load). `pause_after_pauses_a_pane_the_client_is_behind_on` turns the pane off (`refresh-client -A '%0:off'`) before it closes stdin, so the comparison never reaches this bug.
- **tmux:**
  - stdin's EOF sets `CLIENT_EXIT` (control.c `control_error_callback`), but the pane's queued blocks are discarded only later in the loop (server-client.c `server_client_check_exit`, `control_discard`).
  - If the pane is read in the same loop turn, `control_write_output` takes the `CLIENT_EXIT` branch to `ignore` and moves the client's offset to the end of the pane's buffer, with the blocks still queued.
  - The write callback in that turn then asks for a block's bytes past the end, and `control_append_data` calls `fatalx("not enough data: 0 < N")`. The server exits 1, every client gets `%exit server exited unexpectedly` (status 1), and every session is lost.
- **vtmux:** the client exits 0 with `%exit`; the server, its sessions and the other clients stay.
- **Evidence:**
  - CI failure of `pause_after_pauses_a_pane_the_client_is_behind_on` (tmux's exit 1, vtmux 0).
  - The reference tmux 3.8 with `fatalx` also writing to a file, 12 runs at a time under 48 busy loops: the old test hit the crash in 2 of 360 runs (`not enough data: 0 < 2730`, `CLIENT_EXIT` set, client offset = queued = buffer length, one block of 4095 bytes left), and 0 of 360 with the pane turned off first.
  - `stdin_eof_while_output_is_queued_keeps_the_server` against it crashed 4 of 360 (blocks of 289, 866 and 2730 bytes).

<a id="d-049"></a>
### D-049: copying a line wrapped before a wide character that did not fit leaves out tmux's space

- **Category:** Deferred: matching it needs tmux's `cellsize` for every row, through every reflow.
- **Test:** none; found by the copy-mode differential audit (`select-line`, `copy-selection`, `show-buffer`).
- **tmux:**
  - A wide character that does not fit the rest of a row wraps to the next and leaves the last cell unwritten (`screen_write_cell`, screen-write.c:2706-2711).
  - `window_copy_copy_line` copies a wrapped line up to `cellsize` (window-copy.c:6236-6241), and `grid_expand_line` grows a line written past half its width to the whole width (grid.c). So the unwritten cell is copied as a space.
  - After a reflow the line is rebuilt with `cellsize` equal to `cellused` (`grid_reflow_join`, `grid_reflow_split`), and the space is gone, or comes back elsewhere, depending on the widths it went through.
- **vtmux:** the cell is Ghostty's spacer head, which no copy, search or capture counts as written, so the copy has no space, whatever the resizes. The search already reads it as tmux's blank.
- **Evidence:** reference tmux 3.8, 12x6 pane, `printf 'abcdefghijk日z\n'`, `copy-mode`, `history-top`, `select-line`, `copy-selection`: tmux `abcdefghijk 日z`, vtmux `abcdefghijk日z`. After `resize-window -x 30` and back to 12, tmux gives `abcdefghijk日z` too; after 12 to 13, `abcdefghijk日 z`. `capture-pane -pJ` gives `abcdefghijk日z` in both.

<a id="d-050"></a>
### D-050: a popup narrowed by a client resize or a drag reflows its lines

- **Category:** Deferred: needs a resize without reflow in vtmux-vt.
- **Test:** none. Oracle `popup_mouse` grows its borderless popup instead of narrowing it; `popup_resize` writes each line after the resize.
- **tmux:** `popup_resize_cb` and `popup_handle_drag` (popup.c) resize the popup's screen with `screen_resize(&pd->s, sx, sy, 0)`: no reflow, so a narrowed line loses the cells past the new width and a wrapped line stays split.
- **vtmux:** the popup's terminal is a `PaneTerm`, and libghostty-vt reflows its primary screen on every resize, so the cut cells move to the next row.
- **Evidence:** reference tmux 3.8, a `-B -w 20` popup whose job printed `E[<0;2;2ME[<32;3;3ME` and `[<0;3;3m` (one wrapped line), narrowed to 17 with a Meta right-button drag: tmux shows `E[<0;2;2ME[<32;3;` and `[<0;3;3m`, vtmux `E[<0;2;2ME[<32;3;` and `3ME[<0;3;3m`. Found by audit_modes_menus.

<a id="d-051"></a>
### D-051: `destroy-unattached` judges every session once the ones before it are gone

- **Category:** Do not copy tmux bug: iteration over a tree being removed from. Copying it means modelling tmux's red-black tree.
- **Test:** oracle `destroy_unattached`, the two steps annotated D-051 (`ls` and the `session-closed` log after `set -g destroy-unattached keep-last` with a group of three).
- **tmux:** `server_check_unattached` (server-fn.c:529-560) walks the session tree with `RB_FOREACH` while `session_destroy` (session.c:206) does `RB_REMOVE` on the session it is at, so the next session comes from the removed node's stale links. Which session that skips depends on the tree's shape. With sessions base, solo, x, y, z, and x, y, z in one group, keep-last destroys x, skips y and destroys z, keeping y and leaving `base solo y`.
- **vtmux:** the sessions are visited in name order, each judged once the ones before it are gone: x and y go, and z, the last of the group, stays (`base solo z`).
- **Evidence:** reference tmux 3.8, the scenario's trace (`base\nsolo\ny\n`, log `[a][x][z]`).

<a id="d-052"></a>
### D-052: the format expansion time limit counts CPU time, not wall time

- **Category:** Do not copy tmux bug: a finite format loses output because the host was busy.
- **Test:** unit test `system_clock_does_not_count_time_the_thread_is_off_the_cpu` (vtmux-core `format.rs`). Oracle `format_time` failed 1 run in 18 at a load of 130 on a Mac (`display -p '#{t/f/%%%%s|%%%%Y:@ts}'` printed a `ctime` date instead of `1781000000|2026`), and a loop of that command under 16 busy loops printed the date once in 9577 runs.
- **tmux:**
  - `format_check_time` (format.c:4750) stops an expansion that has run longer than `FORMAT_TIME_LIMIT` (100 ms, format.c:140) by wall time (`get_timer`): every further nested expansion is empty. It is there to end runaway re-expansion such as `@a='#{E:@a}#{E:@a}'`.
  - A process the host takes off the CPU for 100 ms in the middle of a finite template gets the rest of it empty. Here the flags argument of `t/f/...` came out empty, so `t` fell back to its `ctime` form.
- **vtmux:** `SystemClock` (the clock `expand` runs on) reads `CLOCK_THREAD_CPUTIME_ID`. A runaway expansion burns CPU and is cut off after the same 100 ms; a finite one is never cut by a busy host. Nothing else changes: the limit and what is cut are as in tmux, and the injected-clock tests drive the limit as before.
- **Evidence:** the loop above: 9576 x `1781000000|2026` and one `Tue Jun  9 11:13:20 2026` before the change, none after (see the task note of `flake_format_time_load`).

<a id="d-053"></a>
### D-053: spaces a popup's job writes with autowrap off show before the popup's next full draw

- **Category:** Deferred.
- **Test:** oracle `popup_nowrap_spaces`, the first `screen` (rows 1 and 2 of the popup).
- **tmux:**
  - Until a popup is drawn again in full, the terminal shows its first, empty draw with the job's output written over it as it arrives (`popup_job_update_cb`).
  - With autowrap off a character goes through `screen_write_cell`, not the collect path (`screen_write_collect_add` needs `MODE_WRAP`, screen-write.c:2633). A space in the default style at or past the line's allocated `cellsize` is skipped, written neither to the grid nor to the terminal (screen-write.c:2740-2742).
  - `cellsize` grows in steps (`grid_expand_line`: a quarter, a half, then the whole width). So after `ab` on a 28-column popup the spaces in columns 2-6 are written and the rest are not, and the terminal keeps the first draw there: one space in `popup-style` and an erase in the default foreground.
  - fzf draws its `--height` and `--tmux` popups this way (tmux-fzf, tmux-sessionx, extrakto).
- **vtmux:** the popup's terminal cannot tell which spaces tmux would have skipped, so every space the job wrote is drawn as written, in `popup-style`'s colours. Only blank cells differ, and only in their foreground colour, so nothing visible changes. The next full draw (`refresh-client`, a resize) matches tmux.
- **Evidence:** reference tmux 3.8 and vtmux on the scenario above: row 1 (28 spaces with DECAWM reset) is ` \e[39m` + 27 blanks in tmux, 28 blanks in `\e[37m` in vtmux. The same spaces with autowrap on (row 3) match. Found by audit_plugins (fzf `--height` in a `display-popup`).

<a id="d-054"></a>
### D-054: a title longer than 2047 bytes is not set

- **Category:** Deferred: needs the title read by vtmux's scanner, or a vendored patch to Ghostty's OSC buffer.
- **Test:** none; found by audit_passthrough_osc's differential probes `osc2-len*`, `apc-len*`.
- **tmux:** OSC 0, OSC 2 and APC collect up to `input-buffer-size` (1 MiB by default, `input_input`), and `screen_set_title` takes the whole string: a 5000-byte OSC 2 makes a 5000-byte `#{pane_title}`.
- **vtmux:** titles go through Ghostty's OSC parser, whose fixed buffer holds 2047 bytes. A longer OSC 0/2 (or APC, which vtmux hands Ghostty as an OSC 2) is dropped, and the title stays as it was.
- **Evidence:** reference tmux 3.8, `printf '\033]2;%s\007' "$(head -c 2048 /dev/zero | tr '\0' x)"`: `#{n:pane_title}` 2048; vtmux keeps the previous title. 1000 bytes match.

<a id="d-055"></a>
### D-055: passthrough goes after the frame of the same loop turn

- **Category:** Deferred: needs frames cut at each payload.
- **Test:** none; audit_passthrough_osc, probes `pt-on-*`, byte order of the client's output.
- **tmux:** a `DCS tmux;` payload is written to the client terminal when the parser reaches it (`screen_write_rawstring`, `tty_cmd_rawstring`). That is ahead of the text of the same read that `screen_write_collect` still holds, text before the payload on the same line included. tmux then forgets its idea of the terminal's cursor and attributes (`tty_invalidate`).
- **vtmux:** the payloads of a loop turn are written after that turn's frame (D14: frames are composed whole), which already shows all the text of the read, and the cursor is put back where the frame left it. What the terminal ends up showing differs only where a payload draws over cells that the same read also writes (a sixel or kitty image placed at the cursor).
- **Evidence:** reference tmux 3.8, `printf 'a\033Ptmux;\033\033]52;c;aGk=\007\033\\b'` with `allow-passthrough on`: tmux writes the OSC 52, then `ab`; vtmux the frame with `ab`, then the OSC 52.

<a id="d-056"></a>
### D-056: an underline colour on an attach redraw is each cell's own

- **Category:** Deferred. Copying it means drawing the redraw path with tmux's run grouping and its unrecorded palette colour: a wrong underline colour on the terminal.
- **Test:** oracle `attach_underline_styles` keeps its coloured underline runs apart from cells that look the same; reproduction below.
- **tmux:**
  - `tty_draw_line` (tty-draw.c) draws a whole line: an attach, a window change, any full redraw. It groups cells that `grid_cells_look_equal` (grid.c:305-318) by colour, attributes, flags and link, but not underline colour, and writes each group with the attributes of its last cell.
  - `tty_colours_us` (tty.c:3076-3082) also returns before recording a palette underline colour it wrote, so the next cell's default one is not written either.
  - On a redraw: `x\e[4;58;5;196mA\e[0;4mB` shows `A` and `B` underlined in the default colour; `\e[58;5;9mnone\e[0mX` gives `none` no underline colour; `\e[58;5;9mnone\e[0m ` gives the space after it colour 9.
  - Cells written as the pane prints them (`tty_cmd_cell`) get their own colour.
- **vtmux:** every frame is drawn from the composed cells, and each cell gets its own underline colour (the terminal's `Setulc`/`Setulc1` permitting).
- **Evidence:** reference tmux 3.8 with `terminal-features ',xterm*:usstyle:overline'` on xterm-256color, the bytes of the attach redraw (`\e[Hx\e[4mAB\e(B\e[m`, `\e[HnoneX`, `\e[58:5:9mnone \e[53mover`); task audit_client_tty_output.

<a id="d-057"></a>
### D-057: `suspend-client` leaves a control client alone

- **Category:** Do not copy tmux bug: a control client loses all pane output from then on, with nothing to bring it back.
- **Test:** oracle `suspend_client_control` (annotated: `list-clients`, `#{session_attached}` and the control stream after `suspend-client`).
- **tmux:**
  - `server_client_suspend` (server-client.c:598-610) stops any client with a session that is not dead, suspended or exiting (`CLIENT_UNATTACHEDFLAGS`), a control client included: it is marked `CLIENT_SUSPENDED` and sent `MSG_SUSPEND`.
  - A control client reads that message only once attached (client.c `client_dispatch_attached`), and it is never sent the `MSG_READY` that gets it there. So it never stops, and never answers with the `MSG_WAKEUP` that clears the flag.
  - It stays suspended for good: `list-clients` leaves it out, its session's `#{session_attached}` does not count it, and its stream gets no more `%output`, while it goes on answering commands.
  - `suspend-client` without `-t` takes the current client. That is a control client whenever it is the best one attached (a script run from outside, a control-mode front end such as iTerm2's `-CC`).
- **vtmux:** a control client is not suspended (it has no terminal to give back), as `lock-client` already leaves it (`server_lock_client` returns early for `CLIENT_CONTROL`). It stays listed, attached and fed with pane output.
- **Evidence:** reference tmux 3.8, `new -d -s a`, a control client `attach -t a`, then `suspend-client` from the shell: `lsc` prints nothing, `#{session_attached}` is `0`, and after `send-keys x Enter` the control client gets no `%output` but still answers `display -p still`. vtmux lists it, counts it and sends it the output (audit_nested_misc).

<a id="d-058"></a>
### D-058: a styled underline on a terminal without `Smulx` is a plain underline

- **Category:** Do not copy tmux bug: the program's text loses a distinction it asked for, where a plain underline keeps the cells marked.
- **Test:** oracle `attach_underline_styles` (annotated: the first attach, xterm-256color without `usstyle`); `render` `underline_styles_colours_and_overline_follow_the_terminal`.
- **tmux:** `tty_attributes` (tty.c:2735-2743) writes a double, curly, dotted or dashed underline as `Smulx` with its number, and nothing without it. A terminal that lacks `Smulx` (xterm-256color, unless `terminal-features` gives `usstyle`) shows the cell with no underline at all, so a program's spell-check squiggle or diagnostics underline disappears.
- **vtmux:** the cell gets a plain underline (`ESC[4m`, when the terminal has `smul`) in place of the style, with its underline colour as usual. With `Smulx`, tmux's form (`ESC[4:NM`).
- **Evidence:** reference tmux 3.8 on xterm-256color shows `curly dbl dotdash` with no underline and only the plain `u` underlined (trace `attach_underline_styles`, the first screen); task low_tty_underline_fallback_civis.

<a id="d-059"></a>
### D-059: two corners of a pane's palette

- **Category:** Deferred.
- **Test:** none fails. Oracle `pane_palette` covers the palette (`pane-colours`, OSC 4 and 104, copy mode, clock mode, a popup) without these two cases. Found with a probe scenario: `pane-colours` `colour196,#00ff00,blue,green,red` on a pane in copy mode with the default `mode-style` (`bg=themeyellow`); the selection's background drew green in tmux, blue in vtmux.
- **tmux:**
  - `tty_attributes` (tty.c:2652-2668) looks a cell's colours up in the pane's palette, then maps theme colours (`tty_map_theme_colour`), then `tty_check_fg`/`_bg` look them up again.
  - A theme colour is not a palette colour at the first lookup (`colour_palette_get` returns -1 for it). So a style's theme colour is looked up once, after it became a palette colour; a palette colour is looked up twice.
  - A popup takes `pane-colours` from the global window options once, when it opens (`popup_display`, popup.c:625-626).
- **vtmux:**
  - Theme colours are resolved when a style is parsed into cells, so a theme colour in a mode's style (copy-mode selection, marks, current line) is looked up twice like any palette colour. It differs only when the entry it maps to is a palette colour with an entry of its own (`yellow` to `green`, `green` to `blue` above).
  - A popup reads the global `pane-colours` at every draw, so a change shows in a popup that is already open.
- **Evidence:** the probe above against reference tmux 3.8 (selection row `\e[38;5;196m\e[42m` in tmux, `\e[38;5;196m\e[44m` in vtmux); popup.c:625-626.

<a id="d-060"></a>
### D-060: a client that is not UTF-8 is drawn in UTF-8

- **Category:** Deferred. It needs `enacs`, `smacs`, `rmacs` and `acsc` in the terminal table, which `tests/terminfo_table.py` must regenerate on a host with every terminal's terminfo, and the frame encoder to track the charset as `tty_attributes` and `tty_reset` do.
- **Test:** none; found by audit_owner_setup_linux (the owner's config attached with `LANG=C` and no `LC_ALL`/`LC_CTYPE`, as an SSH login that forwards no locale gets).
- **tmux:**
  - A client without `CLIENT_UTF8` (no `-u`, no `UTF-8` in `LC_ALL`, `LC_CTYPE` or `LANG`, tmux.c:479-521, and no `utf8` terminal feature) gets every cell through `tty_check_codeset` (tty.c:1482-1512).
  - A character that is not ASCII becomes its ACS form when `tty_acs_reverse_get` has one (box drawing, arrows, the heavy and double border lines), written between `smacs` and `rmacs` through the terminal's `acsc` (`tty_putc`, `tty_acs_get`). Otherwise it becomes one `_` per column.
  - `tty_start_tty` writes `enacs` for it (`tty_acs_needed`, tty-acs.c:205-228, which also forces ACS line drawing on a UTF-8 client whose terminal has `U8#0`).
  - So the owner's status line shows `_` for each Nerd Font glyph, and the heavy pane borders are ACS lines.
- **vtmux:** `#{client_utf8}` is 0 and `list-clients` has no `UTF-8` flag, as in tmux, and command output to such a client is sanitised (`display -p` prints `_`). But frames carry the cells' UTF-8 text and line drawing whatever the client.
- **Evidence:** reference tmux 3.8 and vtmux side by side with the owner's `.tmux.conf`, both attached 160x45 under `TERM=xterm-256color LANG=C`. tmux's status row is ` main     1 sub  ... _ 10%    _ 14%    _ devserver    _ 6d 15h` and its pane borders `x` and `+` in ACS; vtmux's shows the glyphs and `┃`, `→`.

<a id="d-061"></a>
### D-061: a restored anonymous link is a new link

- **Category:** Deferred: restoring the same link needs Ghostty's cursor to keep its hyperlink through DECSC and a screen switch (a Ghostty patch).
- **Test:** oracle `decrc_anonymous_link` (annotated, the `capture-pane -e` and `-J` captures).
- **tmux:**
  - DECRC and RCP restore the cell DECSC saved, its link number included (input.c `input_restore_state`); `?1049l` restores the one the alternate screen saved (screen.c `screen_alternate_off`); entering the alternate screen keeps the pen as it is (`screen_alternate_on` only copies it).
  - An anonymous link (OSC 8 with no `id=`) is a link of its own (hyperlinks.c `hyperlinks_put`). So text written under it before DECSC and after DECRC is one link, and `capture-pane -e` writes one OSC 8 over both (`grid_string_cells_code` compares the link numbers).
  - The comparison goes on across lines (`cmd_capture_pane_history` passes the last cell from line to line). A line whose first cell has the link the line before ended with opens no OSC 8, and, as none opened on it, closes none.
  - At the end of a line with a link open, the closing OSC 8 is appended to the last cell's sequences, which come out again (grid.c `grid_string_cells`, the `strlcat` in `grid_string_cells_add_hyperlink`).
- **vtmux:**
  - Ghostty's cursor saves no link and a screen switch ends it, so after such a sequence vtmux gives Ghostty tmux's link again by sending it the OSC 8 that set it (`PenLinks`, term.rs).
  - An anonymous link sent again is a new link to Ghostty (its implicit id counts up), so the capture writes the OSC 8 again where the restored link starts.
  - When that cell is the last one the capture writes on its line, tmux's repeat of the last cell's sequences repeats that OSC 8 too, right before the close.
  - On the alternate screen, the first line opens the new link where tmux, after a history line that ended with the same link, writes the text with no OSC 8 at all.
  - A link with an `id=` is the same link both ways. What the text links to, and every capture without `-e`, match.
- **Evidence:** reference tmux 3.8; found by `scripts/emu_fuzz.py --gen round2` (decsc; the alternate screen case is seed 910099).
  - 20x3, `printf '\033]8;;http://a/\033\\A\0337\033]8;;\033\\B\0338C'`: tmux `ESC]8;;http://a/ESC\AC ESC]8;;ESC\`, vtmux `ESC]8;;http://a/ESC\A ESC]8;;http://a/ESC\C ESC]8;;ESC\`.
  - 10x3 (the line's allocation, `cellsize`, ends at the last character, so the close follows it), `printf '\033]8;;http://h/\033\\L\0337\033]8;;\033\\X\0338M'`, `capture-pane -pe` and `-peJ`: tmux `ESC]8;;http://h/ESC\LM ESC]8;;ESC\`, vtmux `ESC]8;;http://h/ESC\L ESC]8;;http://h/ESC\M ESC]8;;http://h/ESC\ ESC]8;;ESC\`.
  - 5x2, `printf '\033]8;;http://h/\033\\abcdefghijk\033[?1049h\033[HXY'`, `capture-pane -pe -S -`: tmux `ESC]8;;http://h/ESC\abcde ESC]8;;ESC\` then `XY`; vtmux the same, then `ESC]8;;http://h/ESC\XY ESC]8;;ESC\`.

<a id="d-062"></a>
### D-062: DECSTBM under origin mode homes the cursor to the region

- **Category:** Deferred: the arbiter's verdict decides between Ghostty's home and tmux's.
- **Test:** oracle `decstbm_origin_home` (annotated, the capture and the cursor).
- **tmux:** DECSTBM puts the cursor at the top left of the screen whatever DECOM says (screen-write.c `screen_write_scrollregion`: `screen_write_set_cursor(ctx, 0, 0)`, not relative to the region). So under `?6h` the cursor is above the region it set, and text goes to row 0.
- **vtmux:** Ghostty's `setTopAndBottomMargin` homes the cursor through `setCursorPos(1, 1)`, which origin mode makes the region's top left: `ESC[?6h ESC[3;5r X` writes X on row 2, cursor `1,2` (tmux `1,0`). Without origin mode the two agree.
- **Evidence:** reference tmux 3.8, the scenario's trace. 13 of 144 `--gen round2` seeds from 300000 minimised to it (origin, margins). The D4 arbiter (esctest, vttest) has not been run on it (D22).

<a id="d-063"></a>
### D-063: a zero-width character after a cell nothing was written to is dropped

- **Category:** Deferred (Ghostty grid by D-039's reading; the arbiter has not been run on it).
- **Test:** oracle `zero_width_after_blank` (annotated, the capture).
- **tmux:** `screen_write_combine` (screen-write.c) joins a zero-width character (ZWJ, a combining mark, a variation selector) to the cell left of the cursor whatever it holds, a cleared cell's space included. So `a CHA 4 ZWJ b` captures as `a  ` ZWJ `b`, and a combining acute after `CHA 3` as two spaces and the mark.
- **vtmux:** libghostty-vt drops a zero-width character whose cell before has no text (Terminal.zig `print`), as D-039 found for the bidi isolate: the capture has no ZWJ or mark there. A zero-width character after text joins it as in tmux (patch 0027).
- **Evidence:** reference tmux 3.8, the scenario's trace; `--gen round2` (zwj): a lone ZWJ after a CHA is the most common minimised case of that generator (12 of 16 in the 144 seeds from 300000).

<a id="d-064"></a>
### D-064: with autowrap off, a mark after the last column joins the last character

- **Category:** Do not copy tmux bug: the mark changes a character the program did not put it on.
- **Test:** oracle `combining_nowrap_last_column` (annotated, the capture).
- **tmux:** under `?7l` a character printed in the last column leaves tmux's cursor on that column (`screen_write_cell`, `not_wrap`), and `screen_write_combine` joins a following combining mark to the cell left of the cursor (`cx - 1`). `0123456789` then U+0301 on 10 columns puts the accent on the 8, not on the 9 it follows.
- **vtmux:** Ghostty joins the mark to the character it follows (the 9), with or without autowrap, as xterm does.
- **Evidence:** reference tmux 3.8, the scenario's trace (`012345678́9`); `--gen round2` (widemargin, zwj, decsc).

<a id="d-065"></a>
### D-065: the kitty keyboard protocol in panes (D29, `vtmux-kitty-keys`)

- **Category:** Extension (D28): D29, option `vtmux-kitty-keys` (server, default off).
- **Test:**
  - Off: oracle `kitty_keys_off` (recorded from tmux 3.8, no annotation).
  - On: `crates/vtmux/tests/attach.rs` `kitty_keys_reach_a_granted_pane_typed_and_sent`, `kitty_keys_grant_follows_the_program` and `kitty_keys_grant_ends_when_the_pushing_program_is_killed`; vtmux-server `server::snapshot::tests::a_grant_*` and `the_alternate_screens_grant_ends_alone`; `crates/vtmux/tests/upgrade.rs` `a_granted_kitty_keys_pane_keeps_its_encoding_across_an_upgrade`; vtmux-vt `kitty_keyboard_*` and `kitty_keys::tests`; vtmux-server `keys::tests::kitty_flags_encode_key_names`.
- **tmux:** the CSI table's only `u` entry has no prefix (`INPUT_CSI_RCP`, input.c:347), so `CSI > f u`, `CSI < u`, `CSI = f;m u` and `CSI ? u` are ignored and get no reply. Keys reach the pane through `input_key` (input-keys.c:574) as legacy, modifyOtherKeys or `extended-keys-format` sequences.
- **vtmux:** with `set -s vtmux-kitty-keys on`:
  - A program's push or set of non-zero flags reaches its screen's kitty stack in the pane's Ghostty (stream_terminal.zig:719-723), which grants that screen. `CSI ? u` is answered with its flags of 1, 4 and 8 (`queryKittyKeyboard`, stream_terminal.zig:1963-1970).
  - Every key that reaches a granted screen, typed (after the key tables) or named by `send-keys`, is encoded by libghostty's key encoder with those flags (key_encode.zig:82-100): Escape `CSI 27u`, C-c `CSI 99;5u`, Shift+Enter `CSI 13;2u`, C-i apart from Tab; Enter and text unchanged.
  - `send-keys -l`, `-H`, text, pastes and `#{pane_key_mode}` stay tmux's.
  - Flags 2 (release events) and 16 (associated text) are kept but not applied: they need kitty input from the outer terminal.
- **The grant:**
  - It follows the program's push, pop and set, whatever the option says. Each screen has its own stack.
  - It ends at 0, at RIS, at a respawn, or when the process group that pushed it is gone. A SIGKILLed program never pops, so the pane notes the PTY's foreground group at the push and empties that screen's stack once the group has no process; the shell gets tmux's keys back without a `reset`.
  - A program in the pane's own process group (exec'd as the pane, or run by a shell without job control) shares the shell's group. One killed there while the shell lives on (`bash -c 'prog; prog2'`, `prog; exec bash`) leaves its grant until a `reset`.
- **Off** (the default): nothing differs from tmux.
- **Live upgrade:** Ghostty's stack is already in the snapshot and is the grant, so a granted pane keeps its keys across `vtmux-upgrade`. The pushing group per screen is an ignorable pane field, and an import drops a grant whose group is gone.
- **Evidence:** reference tmux 3.8, the scenario's trace (no reply; `cat -v` shows `^[^C^M` from `send-keys` and from the attached client); the vtmux tests above.

<a id="d-066"></a>
### D-066: desktop notifications reach Ghostty clients (D30, `vtmux-notifications`)

- **Category:** Extension (D28): D30, option `vtmux-notifications` (server, default off).
- **Test:**
  - Off: oracle `notifications_off` (screen, grid, title and progress bar as tmux's); attach `notifications_off_reach_no_terminal` and `popup_notifications_off_reach_no_terminal` (a Ghostty client's bytes carry no notification, against tmux 3.8).
  - On: attach `notifications_on_reach_a_ghostty_client`, `notifications_on_skip_other_terminals`, `popup_notifications_on_reach_its_ghostty_client`, `popup_notifications_on_skip_other_terminals`; server `notifications_go_to_ghostty_clients_of_the_window`, `a_dropped_notification_uses_no_limit_slot`, `a_notification_keeps_its_place_among_passthrough`.
- **tmux:** drops them. `input_osc_9` (input.c:3030-3037) handles only `9;4`, and `input_exit_osc` (input.c:2699-2789) has no 777. They reach the terminal only through `allow-passthrough` and a program that wraps them in `DCS tmux;`.
- **vtmux:** off (the default), the same. With `set -s vtmux-notifications on`:
  - Each notification libghostty finds in a pane goes, ended by ST, to every attached client whose terminal answered XTVERSION as Ghostty and whose session holds the pane's window, current or not. Notifications are OSC 9 that is not a ConEmu command, and `OSC 777;notify;title;body` (osc/parsers/osc9.zig, rxvt_extension.zig, the `desktop_notification` effect, stream_terminal.zig:780, 916).
  - A `display-popup -E` program's notifications go the same way to its own client alone (a popup has no window).
  - A title means OSC 777. An empty-title 777 goes as OSC 9, the same notification to Ghostty, unless Ghostty would read that as ConEmu.
  - Each client is limited as Ghostty limits itself (`showDesktopNotification`, Surface.zig:6612-6647): one a second, an identical one once in 5 s, counting only what is written (one dropped for a blocked client uses no slot).
  - Not sent to control clients or other terminals. `allow-passthrough` is unchanged, and a notification sent by passthrough is not sent again.
- **Live upgrade:** keeps nothing (the limit starts afresh); one in flight may be lost.
- **Evidence:** the tests above; the outer Ghostty shows both forms (termio/stream_handler.zig:350, `desktop-notifications`, config/Config.zig:3896).

<a id="d-067"></a>
### D-067: in-band resize (mode 2048) under `vtmux-inband-resize`

- **Category:** Extension (D28): D31, option `vtmux-inband-resize` (server, default off).
- **Test:**
  - None fails. Off: oracle `inband_resize_off` pins tmux's behaviour (DECRQM, `?2048h`, a resize, the next byte the program reads).
  - On: `crates/vtmux/tests/attach.rs` `inband_resize_reports_every_size_change` (a report at the set and one per path: layout, zoom, attach, client resize, swap, break, join, `resize-window`, respawn), `inband_resize_grant_follows_the_program`, `inband_resize_carries_across_an_upgrade`, `inband_resize_grant_refuses_a_downgrade_until_released`; vtmux-vt `inband_resize_grant`, `inband_resize_off_is_tmux`; vtmux-server `a_mode_2048_grant_and_a_queued_report_survive_a_rebuild`.
- **tmux:** no mode 2048. DECRQM `?2048` answers 0 (`input_csi_dispatch`, input.c:1651-1722), `?2048h` is ignored (`input_csi_dispatch_sm_private`, input.c:2022), and a program learns a new size only from `SIGWINCH`.
- **vtmux:** with `set -s vtmux-inband-resize on`:
  - A program that sets `?2048h` is granted. It gets `CSI 48 ; rows ; cols ; height ; width t` at once and after every size change of its pane, written right after the pty's `TIOCSWINSZ` (pixels from the window's cell size, D-014).
  - DECRQM answers 1 granted, 2 offered.
  - The grant outlives the option, and ends at `?2048l`, RIS or a respawn.
  - Off (the default), and for a program that set the mode while it was off, everything is tmux's.
- **Live upgrade:** the grant is carried (`TermState.inband_resize`) and required while a program holds it (`inband-resize-grant`: a downgrade is refused until the program turns the mode off or exits). A size queued at the snapshot is reported on resume.
- **Evidence:** Ghostty implements the mode in the pane (`in_band_size_reports = 2048`, modes.zig:406; `reportMode2048`, stream_terminal.zig:1881-1890; `Handler.resize`, :523-551; the report's form, size_report.zig:49-58) and as the outer terminal (termio/stream_handler.zig:761, termio/Termio.zig:556).

<a id="d-068"></a>
### D-068: a link's external id is numbered when it is first drawn

- **Category:** Deferred: numbering at parse time needs Ghostty's implicit id for each OSC 8 as it is parsed (a Ghostty patch).
- **Test:** oracle `osc8_ids_numbered_when_drawn` (annotated, the three screens); `osc8_to_clients` and attach.rs `hyperlinks_*` match the reference where the order is the same.
- **tmux:**
  - A link's external id (`tmuxN`, the `id=` a terminal with `Hls` is given, tty.c `tty_hyperlink`) comes from one counter for the server, taken when the OSC 8 is parsed (input.c `input_osc_8`, hyperlinks.c `hyperlinks_put`). A link that scrolls into history before any client draws it still takes the next number.
  - A `link=` style's URI takes one number in `style_hyperlinks` when the style is parsed, and one more in each screen it is drawn on (format-draw.c `format_draw`): each client's status line, each pane's border line.
  - At 5000 links in the server the oldest is forgotten, and its cells are drawn with no link.
- **vtmux:**
  - The grid is Ghostty's (D4), which keeps each cell's URI and OSC 8 id but no number. vtmux gives a link its number the first time a cell of it is read for drawing (`vtmux_vt::link`), from one counter for the server.
  - Links drawn in the order they were written (anything that fits the screen when drawn, copy mode over history nothing drew) get tmux's numbers. Links that scrolled away undrawn are numbered after those on the screen.
  - A `link=` URI takes two numbers the first time it is drawn anywhere, and keeps the second for every client and line.
  - Past 5000 links a pane forgets those no cell on screen holds, and a cell of one drawn again takes a new number.
  - Which cells are one link, their URIs, and where links start and end are tmux's.
- **Live upgrade:** keeps each pane's numbers (`drawn_links` in the snapshot) and the counter (`link_number`, so a link drawn after it never takes a number still on screen). A copy mode restored from one keeps only URIs, so its links take the pane's link of that URI when it has one, else a new one. `link=` URIs are numbered again.
- **Evidence:** reference tmux 3.8, the scenario's trace: two links without an `id=` and one with one scroll into history before the attach, and a fourth stays on the screen. tmux draws it as `tmux4`, and copy mode the others as `tmux1`-`tmux3`; vtmux draws it as `tmux1`, the others as `tmux2`-`tmux4`.

<a id="d-069"></a>
### D-069: a dead pane keeps its status when its pid is reused

- **Category:** Do not copy tmux bug: tmux can overwrite a kept dead pane's status, or destroy it.
- **Test:** `vtmux-server` unit test `server::tests::a_reused_pid_credits_the_live_pane_not_a_dead_one`.
- **tmux:** `server_child_exited` (server.c:491-515) gives a reaped child's status to the first pane with that pid in each window (the loop breaks there), a dead `remain-on-exit` pane included, whose pid the OS may have reused. That pane takes the new process's status, and with `remain-on-exit failed` and an exit of 0, `server_destroy_pane` (server-fn.c:382-399) removes it.
- **vtmux:** the status goes to every pane with that pid whose process has not exited yet; a dead pane keeps its own.
- **Evidence:** source reading: a pid can't be made to come back on demand, so no oracle scenario can show it. Most likely on macOS (pid_max 99999) with frequent `#()` jobs.

<a id="d-070"></a>
### D-070: a failed `-t` window lookup without a session part keeps the session

- **Category:** Deferred: vtmux's target is a winlink of a session, and tmux's half-filled state here has a winlink but no session.
- **Test:** oracle `target_canfail_ambiguous` (the cases tmux and vtmux agree on, every one with a client or a session part).
- **tmux:** `display -p -t w.%0` (CMD_FIND_CANFAIL) with a window name `w` that two windows match: `cmd_find_get_window` leaves the first window, then tries `w` as a session, which sets `fs->s` to NULL (cmd-find.c `cmd_find_get_session`). `format_defaults` takes the session from the target client, and with no client attached `#{session_name}` is empty.
- **vtmux:** the window and pane are tmux's; the session is the current one, as it is in tmux whenever a client is attached to it.
- **Evidence:** reference tmux 3.8, no client attached: `display -p -t w.%0 '#{window_index} [#{session_name}]'` prints `0 []`, vtmux `0 [foo]`.

<a id="d-071"></a>
### D-071: keys typed at a background attach stay with the foreground job

- **Category:** Spec decision: vtmux's client owns the terminal (D14), and the kernel lets a background job draw but not read.
- **Test:** attach.rs `a_background_attach_draws` (the client draws and keeps running while keys are typed).
- **tmux:** the client passes its terminal to the server, which reads it from outside the session (tty.c `tty_read_callback`), so keys typed at a background `tmux attach &` reach the pane while the shell competes for them.
- **vtmux:** the client reads the terminal itself, with SIGTTIN ignored as tmux ignores it (proc.c `proc_set_signals`). In the background the read fails (EIO) and the keys stay for the foreground job; once the client is put in the foreground (`fg`, SIGCONT) it reads what is left.
- **Evidence:** reference tmux 3.8: `sh -c 'set -m; tmux attach -t a & wait'` under a PTY draws the session, and typed keys reach the pane's `cat`. vtmux draws it, and the keys stay unread until the client is in the foreground.

<a id="d-072"></a>
### D-072: kitty graphics under `vtmux-graphics`

- **Category:** Extension (D28): D33, options `vtmux-graphics` (server, default off) and `vtmux-graphics-limit` (server, MB, default 256).
- **Test:**
  - None fails. Off: oracle `graphics_off` pins tmux's behaviour (a transmit, a query and a placement each set the title, nothing answers, the screen shows no image).
  - On: vtmux-vt `graphics::tests` (the grant, Ghostty's answers honouring `q`, rule 5, RIS, PNG, `only_direct_media_load_and_no_path_is_opened`, `a_pending_load_is_held_to_the_screens_limit`); vtmux-server `pane_graphics::tests` (the grant's process group, the shared limit evicting across panes, `a_chunked_load_past_the_limit_is_bounded`, `pending_loads_count_and_the_oldest_goes_first`), `server::tests::graphics` (a recording Ghostty client: the range freed at attach, the upload before the frame, the virtual placement, placeholder cells and their ids, clipped at a pane edge, zoomed, under a popup, at a window switch, in copy mode, a program's own placeholder cells, frames shared within one cell size only, nothing for another terminal), `a_graphics_grant_survives_a_rebuild_and_its_images_do_not`; `crates/vtmux/tests/attach.rs` `graphics_range_is_freed_and_uploaded_again_after_an_upgrade`.
- **tmux:** no graphics. Any APC is a title (`input_exit_apc`, input.c:2794-2809), so `ESC _ G ... ESC \` sets the pane title and is never answered; images reach the outer terminal only through `allow-passthrough`.
- **vtmux:** with `set -s vtmux-graphics on`:
  - A pane screen is granted at its first `APC G`. Its images and placements live in the pane's Ghostty (PNG decoded by vtmux), and Ghostty's answers reach the program.
  - Images come only as data in the escape (`t=d`). File, temporary-file and shared-memory transmissions are refused before any path is opened, with Ghostty's answer for a medium it does not allow (`EINVAL: unsupported medium`), whatever the path names, so a program, local or through ssh, never makes the server open a file and no answer tells which files exist. Popups get tmux's behaviour.
  - A client whose terminal is Ghostty (XTVERSION) gets each image its frame shows as its own upload (zlib, chunked, `q=2`) with a virtual placement, and the image's cells as unicode placeholders (U+10EEEE). A program's own placeholder cells carry vtmux's ids; a direct placement is drawn as placeholder cells over its cells, in whole cells (sub-cell offsets dropped; a placement under the text shows only in cells with no text). Pane edges, zoom, popups, window switches and copy mode clip and move them as any cell. An image no frame shows is freed; every attach first frees vtmux's id range.
  - A client of another terminal and a control client get tmux's cells: no uploads, a program's own placeholder cells as text. `capture-pane`, copy-mode copies and formats see the pane as tmux would.
  - The images of every pane together, with their loads in progress, are held to `vtmux-graphics-limit`. A load in progress never holds more than its screen's limit (the chunk past it fails the load, `ENOMEM` at its last chunk); over the cap, other screens' loads in progress go first, then the images of the screen that loaded least recently, in Ghostty's order.
  - The grant outlives the option and ends with the process group that started it, at RIS or at a respawn; the images outlive the grant.
  - `TMUX` and XTVERSION stay tmux's (D28 rule 3), so an app that wraps its commands for tmux keeps doing so through `allow-passthrough`, unchanged: yazi 26.9 sets `allow-passthrough all` on its pane and wraps every command (`docs/test-health/2026-11-10-graphics-real-apps.md`).
- **Live upgrade:** the grant is carried (`TermState.graphics_primary`, `graphics_alternate`, required as `graphics-grant` while a granted program runs; its process group ignorable), the images are not: a resumed screen draws nothing until its program sends them again.
- **Evidence:** Ghostty implements the protocol in the pane (`graphics_exec.zig` `execute`, `graphics_storage.zig`, placeholders `graphics_unicode.zig:16`) and as the outer terminal with placeholders (termio/Termio.zig:294-295, 511-512).

<a id="d-073"></a>
### D-073: `-D` (run the server in the foreground) is refused

- **Category:** Deferred: vtmux's client starts the server as its own process (D14), and no user has asked for a foreground server.
- **Test:** cli.rs `own_name` (`vtmux -D` prints `not implemented in vtmux yet: -D` and exits 1).
- **tmux:** `-D` sets `CLIENT_NOFORK` (tmux.c:434): the server runs in the foreground of the starting process instead of daemonising (server.c:189, 230), as a service manager wants.
- **vtmux:** refused with `not implemented in vtmux yet: -D`, status 1.
- **Evidence:** reference tmux 3.8 `tmux -D` runs a server in the foreground; vtmux exits 1.

<a id="d-074"></a>
### D-074: `#{c/f:theme...}` and `#{c/b:theme...}` give the slot's terminal colour, whatever the client's theme

- **Category:** Deferred: the format evaluator has no client theme colours yet; the renderer resolves theme colours per client, so a style still draws right.
- **Test:** oracle `format_colour_escape_client`: the annotated step (a client with RGB, `#{c/f:themered} #{c/b:themeblack}`); its other steps match the reference.
- **tmux:**
  - `format_defaults_client` (format.c) makes the attached client the format's client (`display -p`, `display -c`, a key binding's commands), and `colour_toescape` (colour.c:295-343) takes a theme slot from that client's `theme_colours`.
  - `server_client_update_theme_colours` (server-client.c) fills them from `dark-theme-*` or `light-theme-*`, so `themered` is `indianred` (`\e[38;2;205;92;92m`) on a client with 256 colours or more and `red` on one with fewer.
  - RGB and 256 colours are never downgraded: colour.c:318-323 tests `(~flags & TERM_RGBCOLOURS) & (colour & COLOUR_FLAG_RGB)`, disjoint bits (0x10 and 0x02000000), so `#{c/f:#ff0000}` is `\e[38;2;255;0;0m` even for a TERM=screen client, in tmux as in vtmux.
- **vtmux:** a theme slot always gives its terminal colour (`themered` is `\e[31m`, `themeblack` `\e[40m`), as tmux does with no client, and with the default theme options on a client with fewer than 256 colours.
- **Evidence:** the oracle trace; a probe of reference tmux 3.8 and vtmux with a key binding running `set -gF @x '#{c/f:themered}'` on an RGB client gave `\e[38;2;205;92;92m` and `\e[31m`.

## Adding an entry

Take the next unused number; never reuse one. Add a row to the summary table and the entry below, in number order. When an entry is fixed, keep it: retitle it `D-NNN: closed (...)`, say what it was and what fixed it, and move its row to the closed table.

```
<a id="d-NNN"></a>
### D-NNN: <short title>

- **Category:** Ghostty grid | Do not copy tmux bug | Deferred | Spec decision | Extension (D28). Say why in one line.
- **Test:** <regress test or oracle scenario, and the failing line>
- **Patch:** <compat/patches/NNNN-name.patch, if the regress assertion is patched>
- **tmux:** <what tmux 3.8 does, with source file and function>
- **vtmux:** <what vtmux does>
- **Evidence:** <arbiter result, trace, or reproduction>
```

- Keep bullets short: at most three sentences each; put lists in sub-bullets.
- A **Do not copy tmux bug** entry needs a regression test.
- An **Extension (D28)** entry names its decision (D29, D30, D31, ...), its option, the oracle scenario that pins tmux's behaviour with the option off, the test that pins vtmux's with it on, and what a live upgrade keeps.
- Oracle annotations use the same ID: `{ divergence = { id = "D-NNN", expect = {...} } }` (`oracle/README.md`).
- The process and the decisions (D4, D5, D28, ...) are in the design spec, `docs/specs/2026-10-29-vtmux.md` (*Divergence process*).
