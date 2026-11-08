# Divergences from tmux 3.8

vtmux follows tmux 3.8 exactly except where recorded here (spec: *Divergence
process*). Every entry needs evidence. There is no "intentional divergence for
product reasons" category.

## Categories

- **Ghostty grid**: the D4 arbiter ruled for Ghostty's terminal-emulation behaviour over tmux's.
- **Do not copy tmux bug**: tmux loses data, hangs or is unsafe. Needs evidence and a regression test.
- **Deferred**: not done yet. Must not be claimed as compatible.
- **Spec decision**: a numbered decision in the spec (`docs/specs/2026-10-29-vtmux.md`) chose this behaviour for drop-in use. Cite the decision.

## Entry template

```
### D-NNN: <short title>

- Test / assertion: <regress test or oracle scenario, and the failing line>
- tmux behaviour: <what tmux 3.8 does>
- vtmux behaviour: <what vtmux does>
- Evidence: <arbiter result, trace, or reproduction>
- Category: Ghostty grid | Do not copy tmux bug | Deferred
- Patch: <compat/patches/NNNN-name.patch, if the regress assertion is patched>
```

## Entries

### D-001: `#{history_bytes}` is empty

- Test / assertion: oracle `panes_split_resize`, the default-template `list-panes` step
  (`[history 0/2000, 120 bytes]` in tmux); every other list-panes comparison uses `-F` without it.
- tmux behaviour: `format_cb_history_bytes` (format.c:996-1020) sums tmux's own grid memory: each line's
  `cellsize * sizeof(struct grid_cell_entry)` plus extended cells plus `sizeof(struct grid_line)` per line.
- vtmux behaviour: the grid is Ghostty's (D4), which has no such per-line allocation; the variable is
  unset, so it expands to the empty string (`[history 0/2000,  bytes]`).
- Evidence: `panes_split_resize` trace; reference `new -d -x 80 -y 24 cat; splitw; lsp` prints
  `480 bytes` for an empty 80x12 pane (40 bytes per row of 80 cells, size of tmux structs).
- Category: Deferred (a byte count modelled on tmux's structs could be emitted; nothing known depends on it).

### D-002: ERE resource limits (`m/r`, `s///`)

- Test / assertion: `crates/vtmux-core/src/regex/tests.rs` `regex_resource_limits_are_errors_not_panics`; `crates/vtmux-core/src/format/matching.rs` `format_matching_limit_is_regexec_failure`.
- tmux behaviour: glibc `regcomp`/`regexec` (format.c:format_match, regsub.c:regsub) have no step cap. Backreference searches backtrack without bound: `(a*)\1b` against 3000 `a` takes 47 s. Compiling fails with `REG_ESPACE` only when memory runs out.
- vtmux behaviour: patterns without backreferences have no work cap; they run in O(text × compiled nodes) (`regex/nfa.rs`). These limits remain and return `Error::Limit`: 64 nested groups or chained quantifiers, 128 groups plus quantifiers, about one million compiled nodes after `{m,n}` expansion (`(a{1000}){1000}` is refused; glibc compiles it), and for backreference patterns a backtracking budget of one million capture-weighted steps per start position (no cross-start ceiling, so an ordinary backreference on long text still finds its match). Backreference search is quadratic in text length: `(\w+) \1` on 1.9k chars takes about 1 s in a release build where glibc takes about 10 ms. The format layer handles a limit the way tmux handles a failed `regexec`: `m/r` gives `0`, and `s///` copies the rest of the text unchanged, keeping earlier replacements. A large `{m,n}` makes matching slow but still correct: `a{32767}` against 32767 `a` takes about 4 s (glibc: 0.07 s).
- Evidence: reviewer-8's `high_regex_work_budget_silently_turns` repros (`[0-9]+%` on 710 digits, `[a-z]+:`, `\w+@`, `[^ ]+ x` on 1000 chars, `(a|b)*c`, `.*z|q`) now match glibc 2.43 and the reference tmux 3.8 (`regex_long_texts_match_like_glibc`). 100k-character texts take < 1 s per search (`regex_without_backreferences_is_linear`). `(a*)\1b` against 3000 `a` gives `Limit` after about 1 ms, where glibc returns no match after 47 s.
- Category: Deferred

### D-003: glibc's `^` inside a repeated group

- Test / assertion: none fails (the rows below are not in `src/regex/golden.in`); task `regex_corners` probes against glibc 2.34 and 2.43 (python ctypes `regcomp`/`regexec`, as `tests/regex_cases.sh`) and reference tmux 3.8. The other corners this entry used to list now match and are pinned: an escaped lowercase letter under `REG_ICASE` (`\a`) never matches and the last iteration of an empty-matching repeat is reported (`(a*){2}` on `aa`: `\1` = 2:2) in `golden.in`; fnmatch `[a-[:digit:]]` fails the bracket in `fnmatch_golden.in`; the `s///` empty-match step of one byte in `format/matching.rs` `empty_matches_step_one_byte_as_regsub_does`.
- tmux behaviour: glibc's DFA ignores a `^` that is not at the start in a later iteration of a repeated group, and its answer depends on how many captures the caller asks for (regexec.c `re_search_internal`: only with `nmatch > 1` does `prune_impossible_nodes` check the span, and when that fails no shorter match is tried). `(^a|b)+` on `ba`: 0..2 with `nmatch` 1 (copy-mode search), a match with `nmatch` 0 (`m/r`), no match with `nmatch` 10 (`s///`: `#{s/(^a|b)+/X/:abab}` is `abab`). `(^a)+` on `aa` and `(a|^b)+` on `ab` behave the same. A backreference to such a group differs too: `(a*){2}\1` on `aa` is no match.
- vtmux behaviour: POSIX: `^` matches only at the start of the text in every iteration. `(^a|b)+` on `ba` is 0..1 whatever the caller asks for, and `#{s/(^a|b)+/X/:abab}` is `XX`; `(a*){2}\1` on `aa` matches 0..2.
- Evidence: task regex_corners probes (glibc 2.34 on CentOS 9, the same answers as 2.43 in review_fmt_regex); reference tmux 3.8 `display -p '#{m/r:(^a|b)+,abab} #{s/(^a|b)+/X/:#{l:abab}}'` gives `1 abab`, vtmux `1 XX`.
- Category: Do not copy tmux bug (a glibc DFA bug whose result changes with the number of captures requested; copying it means modelling glibc's DFA and its pruning).

### D-004: time formatting: C locale, year range, long strftime output, unknown TZ names

- Test / assertion: `crates/vtmux-core/tests/time_cases.sh` (rows citing D-004); hand probes from review_fmt_time (reviewer-14).
- tmux behaviour: tmux calls `setlocale(LC_TIME, "")` and glibc `strftime`/`localtime_r` (format.c:4694-4700, 6728-6741). (1) A non-C `LANG` changes `%c %x %r %p %a %b`: `LANG=en_GB.UTF-8` gives `Fri 30 Oct 2026 14:32:28 GMT` for `%c`; `LANG=de_DE.UTF-8` gives `Do Okt` for `%a %b`. (2) Years past 9999 format normally (`@big=999999999999` gives `33658 Fri`). (3) `t/f` output longer than 511 bytes makes `strftime` fail and tmux prints its half-written stack buffer. (4) An unparseable `TZ` (`TZ=Invalid/Zone`) keeps the leading letters as the abbreviation: `%Z` is `Invalid`, with offset `+0000`.
- vtmux behaviour: (1) C locale always (`Fri Oct 30 14:32:28 2026`, `Thu Oct`); `#{t:}` (ctime) is C in both. (2) years past 9999 give empty output. (3) output over 511 bytes is empty. (4) `%Z` is `UTC` (jiff falls back to UTC); offset and wall time match.
- Evidence: reviewer-14's probes on b6b2721 against reference tmux 3.8 (notes on `medium_fmt_time_cites_d_004_but_compat` and `low_unknown_tz_name_renders_z_as_utc_ref`).
- Category: Deferred

### D-005: a loop nested in a loop of the same kind does not disturb the outer order

- Test / assertion: `crates/vtmux-core/tests/loop_cases.sh`, the row citing D-005 (`#{S:#{S/n:#{session_name}}-#{session_name} }`).
- tmux behaviour: `sort_get_sessions`, `sort_get_winlinks_session`, `sort_get_panes_window` and `sort_get_clients` (sort.c:461-600) return one static array per kind, which every call refills and sorts in place. A nested loop of the same kind therefore re-sorts the array the outer loop is walking, and the outer loop reads the inner order from then on. With sessions zeta, alpha, mike (IDs 0, 1, 2) the outer loop visits zeta, then reads index 1 and 2 of the name-sorted array: `alphamikezeta-zeta alphamikezeta-mike alphamikezeta-zeta`, so mike is visited once, zeta twice and alpha never.
- vtmux behaviour: each loop sorts its own list; the outer loop visits each session once in its own order: `alphamikezeta-zeta alphamikezeta-alpha alphamikezeta-mike`.
- Evidence: reference tmux 3.8, the `loop_cases.sh` topology (reference value kept in the row's comment).
- Category: Do not copy tmux bug (the outer loop skips and repeats items; copying it means sharing a mutable list across nested expansions).

### D-006: wrap-flag corners of tmux's grid bookkeeping that vtmux does not model

- Test / assertion: none fails. Oracle `wrap_flag_capture_join` (29 scenes: EL, ED, IL, DL, SD, SU, RI, regions, the history row, the alternate screen) and `wrap_flag_redraw_copy` (the redraw of a blank run and copy mode's `select-line`) pin what matches since patch 0011 (task wrap_model); `wrap_flag_resize` (15 scenes: DL, SU, SD, RI, IL, region scrolls, erased continuations, then a wider pane) pins the reflow since patches 0011 and 0012 (task medium_dl_wrap_flag_resize); `reflow_cells_used` pins the erased continuation and the ECH line clear since patch 0013.
- tmux behaviour: `grid_clear_lines` and `grid_move_lines` (grid.c) clear `GRID_LINE_WRAPPED` on the line above the lines they clear or move. Some callers reach them in ways the patch does not follow: `screen_write_clearline` returns early for a line never written (`cellsize` 0) with the default background, so it unwraps nothing; `grid_view_clear_history` (`scroll-on-clear`) clears the lines below the used ones after scrolling those into history, which unwraps the last scrolled line; IL and DL with the cursor outside the scroll region work on the whole screen (`screen_write_insertline`, `screen_write_deleteline`); a pane with `history-limit` 0 has no `GRID_HISTORY`, so its region scrolls go through `grid_move_lines`. `grid_reflow_join` joins every cell a wrapped line's next line has used (`cellused`), erased ones included; ECH over a whole line from its first column is `grid_clear_lines` too.
- vtmux behaviour: Ghostty's flag follows tmux's in the scenes above (patch 0011). The cases listed are not modelled: a clear of an unwritten line unwraps the line above (which cannot be wrapped onto an unwritten line unless that line was partly erased); the line above the cleared lines after a scroll-on-clear keeps its flag; IL/DL outside the region do nothing (Ghostty), wrap flags included; a pane without history scrolls its region as one with history. (A continuation erased to blanks joins with all its erased cells since patch 0013 keeps tmux's `cellused`: `printf 'x\r\naaaaaaaaaabbbbb\r\nccc\033[3;1H\033[5X'` on 10 columns, `resize-window -x 14` gives tmux's `x` / `aaaaaaaaaa` / blank / `ccc`.) Two related differences have their own entries: a line scrolled out of a region that does not start at the top (D-032) and the trailing erased cells `capture-pane -J` keeps on a wrapped line (D-027, the `cellsize` model).
- Evidence: task wrap_model probe, reference tmux 3.8 vs vtmux `capture-pane -pJ`: before patch 0011, 22 of 28 scenes differed (for example `printf 'aaaaaaaaaabbbbb\r\nccc\033[2;1H\033[2Kzz'` on 10 columns: tmux `aaaaaaaaaa` / `zz`, vtmux `aaaaaaaaaazz`); with it, 36 of 39 scenes match and the other three are two D-032 scenes and one D-027 scene. reviewer-21's redraw case (`\e[31m`, 40 `x`, 12 spaces, `y`, then EL 2 and red blanks) and reviewer-61's `?1049h` boundary row now match (`wrap_flag_redraw_copy`, `althist`). Task medium_dl_wrap_flag_resize: `wrap_flag_resize` fails 13 of its 15 scenes on the tree before patch 0012 and the continuation fixes in 0011 (DL: tmux `x` / blank / `ccc`, cursor 0,5; vtmux `x` / `ccc`, cursor 0,4) and passes with them; 19 probe scenes resized to 3 to 20 columns match tmux, the `5X` one above aside.
- Category: Deferred (corners: an unwritten line, a scroll-on-clear partial clear, IL/DL outside the region, a pane with no history).

### D-007: terminal-overrides is a subset, and terminals come from a built-in table

- Test / assertion: `crates/vtmux-core/src/term_features/golden.rs` (178 reference rows from `tests/term_features_cases.py`), `crates/vtmux/tests/attach.rs` `terminal_features_apply_at_attach`.
- tmux behaviour: `tty_term_create` (tty-term.c) loads the client's terminfo entry (`setupterm`), applies `terminal-features` (tty-features.c), `COLORTERM`, then `terminal-overrides` (`tty_term_apply`) to any of ~150 capability names. Later device-attribute replies add features too (`tty_default_features`: `XTVERSION`, DA2), and the terminfo entry may name any terminal.
- vtmux behaviour: vtmux has no terminfo model (D8). Terminals come from a built-in table (`xterm*`, `xterm-ghostty`, `tmux-256color`, `screen`, `screen-256color`; anything else, `foot`, `linux` and `dumb` included, is refused at attach with `open terminal failed: unsupported terminal (xterm*, xterm-ghostty, screen, screen-256color, tmux-256color)`; tmux attaches `foot` and `linux`). Feature parsing, `terminal-features` matching, `COLORTERM` and the `-T`/`-2` flags follow tmux exactly. `terminal-overrides` honours only `Tc`, `RGB`, `setrgbf`, `setrgbb`, `Sync`, `Smulx`, `Setulc`, `Ss`, `Se`, `Ms`, `Eneks` and `Dseks`. For all of them except `Ms`, `Eneks` and `Dseks` only the presence of the capability is kept; the value is ignored and vtmux writes its own fixed sequences: `Sync=x` or `Sync=QQ%p1%dQQ` still gives `ESC[?2026h` where tmux writes the template (nothing at all for a value that does not expand to a sequence), and `setrgbf` is always `38;2;r;g;b` where Ghostty's terminfo makes tmux write `38:2:r:g:b`. `Ms` is validated with a model of `tty_term_validate`'s check, fitted to 40 probed templates. Every other name of tmux's `tty_term_codes` is tracked by presence only (each built-in terminal's names are generated from the host's terminfo by `tests/terminfo_names.sh`; features add and `ignorefkeys` removes theirs, overrides set and remove them), so `#{I/c:name}` and `#{I/f:name}` answer as tmux does and `am@` stops the bottom-right cell being drawn; vtmux writes nothing else differently for them. The startup queries are tmux's (DA, DA2, XTVERSION, DECRQM 2026, the OSC 10/11 colours, and `?996n` with `?2031h`), and their replies add features as tmux's do (`tty_keys_sync`, `tty_keys_device_attributes`, `tty_keys_device_attributes2`, `tty_keys_extended_device_attributes`); XTVERSION sets `#{client_termtype}`. Then `Eneks` and `Enfcs` are written as `tty_update_features` writes them, but the client is redrawn only if a capability vtmux draws with changed (tmux always redraws; the screen is the same). vtmux does not repeat the colour queries on a resize (`tty_repeat_requests` after `TTY_REQUEST_LIMIT`), only after a theme report, and a reply that arrives after its query was answered or timed out is swallowed where tmux reads it as keys. `Smulx`/`Setulc`, `Ss`/`Se`, `Ms` and the `hyperlinks` feature are tracked and listed but the compositor does not yet write styled underlines, cursor styles, OSC 52 or OSC 8 for them.
- Evidence: reference tmux 3.8 `list-clients -F '#{client_termfeatures}|#{client_colours}|#{Ic:...}'` on a PTY client for every row of `golden/cases.tsv`; `Ms` probes in `term_features_cases.py` rows `xterm*:Ms=...`.
- Category: Deferred

### D-008: respawn-window -k on a zoomed window

- Test / assertion: `crates/vtmux/tests/cli.rs` `windows_respawn_zoomed` (vtmux only: the reference dies).
- tmux behaviour: `spawn_window` (spawn.c:106-128) with `SPAWN_RESPAWN` frees the layout (`layout_free`) and the other panes but leaves `WINDOW_ZOOMED` set with the saved layout pointing at freed cells. The next command that reads the window's layout (`display -p '#{window_layout}'`, `splitw`) kills the server (`server exited unexpectedly`).
- vtmux behaviour: the window is unzoomed first, then respawned: one pane, full size, `window_zoomed_flag` 0.
- Evidence: reference tmux 3.8, isolated: `new -d -s a cat; new -d -s k cat; splitw -d -t a cat; resizep -Z -t a:0.1; respawnw -k -t a; display -p -t a '#{window_layout}'` -> `server exited unexpectedly`, exit 1, and every later command says `no server running`.
- Category: Do not copy tmux bug

### D-009: config causes are not shown in view mode once a session is attached

- Test / assertion: none fails; the startup config's causes for an attached terminal client (`cfg_show_causes`).
- tmux behaviour: once a session is attached (`new-session`, `attach-session`, cfg.c:239-280), the causes go into the view mode of its active pane, written unparsed (`window_copy_add(wp, 0, ...)`, escapes shown as text).
- vtmux behaviour: view mode exists (`cmdq_print` from an attached client, `run-shell` output to a pane, parsed as tmux parses them; oracle `view_*`, regress `run-shell-output.sh`), but the config causes stay queued until the next `source-file` prints them; a control client gets them as `%config-error` as in tmux.
- Evidence: reference tmux 3.8 with a config holding a bad command: `new` shows the cause in view mode; vtmux shows the shell.
- Category: Deferred

### D-010: copy mode is partial (U1, U2 and U3 of the copy-mode split)

- Test / assertion: regress `copy-mode-commands.sh`, `copy-mode-no-client-scroll.sh` and `copy-mode-redraw.sh` stay xfail; oracle `copy_enter`, `copy_move`, `copy_resize`, `copy_clone`, `copy_select_draw`, `copy_select_mode` and `copy_search_draw`, `copy_search_kinds`, `copy_search_jumps`, `copy_search_prompts` pin what exists.
- tmux behaviour: window-copy.c has 99 commands, selections, search, jumps, marks, copy/pipe, line numbers, refresh, view mode and the mouse.
- vtmux behaviour: `copy-mode` (`-d -e -H -k -q -s -S -u`; `-M` is an error), the pane mode stack, keys through the `copy-mode`/`copy-mode-vi` tables, `send-keys -X` with `-N` and `-F` (search strings), cursor/line/page/half-page/history-top/bottom/scroll movement, `goto-line`, `recentre-top-bottom`, `cancel`, scroll-exit, the copy-mode formats and the position indicator. Selections (begin/stop/clear-selection, other-end, rectangles, `selection-mode`, select-word, select-line), word and space movement, `copy-selection*`, `append-selection*`, `copy-line*`, `copy-end-of-line*`, the `selection_*` formats and the selection drawn in `copy-mode-selection-style`. Search (regular expression, text, incremental, again/reverse, `wrap-search`), the `search_*` formats and `pane_search_string`, matches in `copy-mode-match-style`/`-current-match-style`, jumps, `set-mark`/`jump-to-mark` in `copy-mode-mark-style`, matching brackets, paragraphs, and `previous-prompt`/`next-prompt [-o]` over the lines OSC 133 marked (vtmux-vt pins each mark to its row; Ghostty's own semantic-prompt state is not tmux's line flags). The grid is copied on entry and resized and reflowed with the view as tmux does (`window_copy_clone_screen`, `window_copy_resize`). The pipe commands (`copy-pipe*`, `pipe*`, the `-line` and `-end-of-line` variants, `copy-command`) run their job with the selection on its stdin and send the copy to the clients' clipboards under `set-clipboard` (`pane-set-clipboard` fires); `refresh-on/-off/-toggle/-now` copy the pane's screen again every 50 ms while output arrives, with `#{pane_unseen_changes}` and `#{refresh_active}`. A line's time (`top_line_time`) is lost when the pane's width changes before copy mode is entered (Ghostty's reflow does not say which row went where), so the indicator shows no time for it; the same reflow keeps an OSC 133 mark on the row its pin follows. `copy-mode -S` without a mouse event scrolls to the top of history (the slider position for `my = 0`). Line numbers are not drawn: `line-numbers-on/-off/-toggle` only keep the setting (`#{copy_line_numbers}`, the position format and `goto-line` follow it). vtmux draws the whole mode screen after every command, where tmux redraws only the indicator and the cursor's lines after a move (`WINDOW_COPY_CMD_MOVE`, window-copy.c:3967-3976): when a word or line selection widens over lines the cursor did not cross, tmux's terminal keeps the old selection on those lines until the next full redraw, and vtmux shows the selection the formats report (with search marks present tmux redraws everything, as vtmux does).
- Evidence: `cargo xtask oracle replay --only 'copy_*'`; cli.rs `copy_mode_formats` and `copy_selection_formats` equal tmux 3.8; attach.rs `owner_copy_mode_vi_bindings`, `owner_prompt_jumps`; regress `copy-mode-selection-mode.sh` passes.
- Category: Deferred

### D-011: closed (U+0085 and TAB cells match tmux)

- Was: a C1 control written as UTF-8 (U+0085) took a cell of its own in Ghostty where tmux combines it into the cell before (`utf8_width`: wcwidth -1, C1, width 0), so next-word over `xa\302\205b` stopped one column later; and HT left no TAB cell.
- Now: vendored patch `0009-c1-is-zero-width.patch` makes a decoded C1 control a zero-width character that joins the previous cell (Ghostty dropped it), and `0002-tab-cell-mark.patch` gives tmux's TAB cell. Regress `copy-mode-unicode-whitespace.sh`, `utf8-test.sh` and `copy-mode-test-emacs.sh` pass; cli `program_output_reaches_the_grid_as_in_tmux` (case `c1`) pins it. A resize reflows a tab as tmux's `grid_reflow` does (the TAB cell's width plus one per padding cell): patch `0015-reflow-tab-as-tmux.patch`, goldens `cap_tab_resize_*`, oracle `rep_deccolm_tab_reflow`.

### D-012: a new window's first automatic-rename check waits one name interval

- Test / assertion: cli.rs `initial_window_name_is_the_command` (vtmux only; the reference's outcome is not repeatable, so it cannot be compared).
- tmux behaviour: `spawn_window` sets the name from `default_window_name` (the command's first word), and `check_window_name` (names.c) runs at the end of the same loop turn, with no `name_time` yet. `#{pane_current_command}` then reads the foreground process of the new PTY, which is usually still the forked server (`tmux`) and sometimes the shell before it execs or the command itself. So `new -d -s a cat; neww -d cat; neww -d cat` gives `tmux`, `cat` and `sh` in different runs (20 runs: 10 `tmux/tmux/tmux`, 6 `tmux/tmux/cat`, 2 `cat/cat/tmux`, 2 `tmux/cat/cat`), and `window-renamed` fires for the ones that lose. A window that prints nothing keeps the wrong name.
- vtmux behaviour: the window's name clock starts at creation, so the first check comes one `NAME_INTERVAL` (500 ms) later, when the command has exec'd. The window keeps the name from `default_window_name` (`cat`) or settles on the command's `argv[0]` (`sleep` for `sh -c 'exec sleep 5'`), and no spurious `window-renamed` or `%window-renamed` fires at start.
- Evidence: 30 runs of `new -d -s a cat; neww -d cat; neww -d; neww -d sleep 4; neww -d 'exec sleep 5'` with SHELL=/bin/sh give `cat/cat/sh/sleep/sleep` every time; before the change `sh/cat/...` mixes. Reference runs of the same script vary as above.
- Category: Do not copy tmux bug (the name depends on a scheduler race).

### D-013: byte sequences glibc reads as one character but Unicode does not (past U+10FFFF, surrogates)

- Test / assertion: none (the rows are left out of cli.rs `non_utf8_bytes_through_options_and_formats`).
- tmux behaviour: option values, formats and arguments are C strings. For `.`, `?` and `[...]` (`regexec`, `fnmatch`) and for `#{w:}`, `#{=N:}`, tmux 3.8 on glibc 2.43 sees `\364\220\200\200` (`\U00110000`), the five byte `\370\210\200\200\200` and the six byte `\375\277\277\277\277\277` (`\U7fffffff`) as one character each: `#{s/./x/:@o}` gives `x`, `#{m:?,#{@o}}` and `#{m/r:^.$,#{@o}}` give 1, `#{w:@o}` of the four byte form gives 1.
- vtmux behaviour: the core keeps text as `String` with each byte that is not UTF-8 as one escaped byte (`rawstr`), so these sequences are separate bytes: `.`, `?` and `[...]` do not match them, `w` is 0 and `=` skips them. `n`, `p`, `q`, `b`, `d`, `==`, `show`, `display -p` and `source-file -v` match tmux for them, as they do for every byte string.
- Evidence: `set -g @o "$(printf '\370\210\200\200\200')"; display -p '#{s/./x/:@o}'`: tmux `x`, vtmux `\370\210\200\200\200`; `/tmp/bytes_args/p7.sh` (bytes_args worker) lists every differing row.
- Also (review_bytes_args_2, reviewer-54, `/tmp/r54/n.sh`): names. tmux's `utf8_isvalid` decodes through `mbtowc`, so a window, session or buffer name containing `\364\220\200\200` (F4 90..F4 BF leads, U+110000..U+13FFFF) is accepted; vtmux rejects it as `invalid window/buffer/session name`. And surrogates: glibc decodes `\355\240\200` (U+D800, ED A0..ED BF leads) as one character for `regexec`/`fnmatch`, so `#{s/./Z/:@p}` on `a<ED A0 80>b` gives `ZZZ` in tmux and `Z<ED A0 80>Z` in vtmux; `m/r` and captures differ the same way.
- Category: Deferred (only byte sequences that are not valid UTF-8 but that glibc decodes anyway; every valid UTF-8 sequence, including U+10FF80..U+10FFFF and U+FFFD, matches the reference, and so does every byte glibc rejects).

### D-014: the partial-key delay doesn't wait on the terminal size query

- Test / assertion: none; reviewer-55's `/tmp/rev55/esc.py` (review_prompt_core_2).
- tmux behaviour: when the outer terminal reports no pixel size (`ws_xpixel == 0`), `tty_resize` (tty.c:144-150) sends `ESC[18t ESC[14t` and sets `TTY_WINSIZEQUERY`; until a `4;h;w` reply arrives, `tty_keys_next` (tty-keys.c:1002-1008) keeps the 500 ms partial-key delay, so a lone ESC waits 500 ms indefinitely on terminals that never answer.
- vtmux behaviour: vtmux doesn't send the size query, so after the first 5 s a lone ESC is taken after about 10 ms, as tmux does once the query is answered or when pixel sizes are known.
- Evidence: with `ws_xpixel=0`, tmux redraws 501 ms after a lone ESC at t=3/8/17/27/42 s; vtmux 503 ms at t=3 s, then 12 ms from t=7.6 s. With pixel sizes set, both are about 10 ms after 5 s.
- Category: Deferred (pixel-size queries come with image passthrough work; the visible effect is Escape responsiveness, in vtmux's favour).

### D-015: the cell after a pane prompt's input keeps message-style at some widths

- Test / assertion: none; reviewer-57's raw-byte probe (review_pane_prompt).
- tmux behaviour: the pane prompt row is drawn with `tty_draw_line` up to the line's `cellsize` (screen-redraw.c:1671, tty-draw.c:161-166), and `cellsize` comes from `grid_expand_line`'s rounding to sx/4 or sx/2 (grid.c). So whether the cell after the input is drawn in message-style depends on the pane width: at widths 48-51 tmux sends `(goto line)\e[39m\e[49m\e[K`; at 20, 30, 35, 46, 47, 52, 55, 99, 100 it sends a styled space.
- vtmux behaviour: always draws the styled space after the input.
- Evidence: reviewer-57's raw PTY bytes from the reference, `command-prompt -b -P -p '(goto line)'`, widths listed above.
- Category: Do not copy tmux bug (an allocation-rounding artefact; one cell's background at four widths).

### D-016: the pane prompt's cursor with status off and status-position top

- Test / assertion: none; reviewer-57's oracle live scenario (review_pane_prompt).
- tmux behaviour: the prompt row follows `status-position` (screen-redraw.c:1616-1617, 1641), but the cursor is placed using `status_at_line(c) == 0` (server-client.c:2141-2144), which is -1 with status off (status.c:90-91, 106). With `status off` and `status-position top`, the prompt is drawn on row 0 and the cursor is put on the last row.
- vtmux behaviour: the cursor is placed on the prompt's row.
- Evidence: 40x8, `set -g status off; set -g status-position top`, copy-mode, `g`: tmux cursor 13,7 with the prompt on row 0; vtmux 13,0.
- Category: Do not copy tmux bug (the cursor away from the text being edited).

### D-017: `show-messages -T` lists features, not terminfo capabilities

- Test / assertion: `crates/vtmux/tests/attach.rs` `show_messages_names_keys_and_terminals` (the `Terminal N: ...` header line only).
- tmux behaviour: `cmd_show_messages_terminals` (cmd-show-messages.c:47-70) prints `Terminal N: <TERM> for <client>, flags=0x..:` and then `tty_term_describe` for every capability the terminfo entry has or lacks (237 lines for `TERM=screen`).
- vtmux behaviour: vtmux has no terminfo model (D8, D-007). It prints the same header line (flags: 0x1 256 colours, 0x10 RGB, 0x20 VT100-like) and one `   features: <list>` line with what vtmux knows. A client with no open terminal (a control or command client) is skipped, as in tmux.
- Evidence: reference tmux 3.8 on a PTY client, `TERM=screen`, `xterm-256color` and `xterm-256color` with `COLORTERM=truecolor`: flags `0x20`, `0x20`, `0x31`; `show-messages -T | wc -l` is 237 for `screen`.
- Category: Deferred

### D-018: `clear-history -H` has no hyperlink table to reset

- Test / assertion: none.
- tmux behaviour: `-H` calls `screen_reset_hyperlinks` (cmd-capture-pane.c:427-428), emptying the pane's OSC 8 link table, which `capture-pane -H`, `#{mouse_hyperlink}` and `#{copy_cursor_hyperlink}` read: links written before the reset are gone from them.
- vtmux behaviour: `-H` is accepted and changes nothing. libghostty-vt keeps links with the pages that hold the rows and vtmux exposes no table, so `#{mouse_hyperlink}` and `#{copy_cursor_hyperlink}` still give a link written before `clear-history -H`, and so do `capture-pane -H`, `-e` and `-R`.
- Evidence: `clear-history -H` and `clear-history` give the same `history_size` and capture output on the reference and on vtmux (`cli.rs` `clear_history_keeps_the_screen`). After `printf '\033]8;;http://z/\033\\LNK...'`, `clear-history -H`, then `copy-mode` at the link, `#{copy_cursor_hyperlink}` is empty on the reference and `http://z/` on vtmux.
- Category: Deferred

### D-019: bytes that are not UTF-8 follow glibc on macOS too

- Test / assertion: cli.rs `non_utf8_bytes_in_command_strings`, `non_utf8_bytes_through_options_and_formats`, `plane16_characters_are_ordinary_text`: the rows gated on `GLIBC_REFERENCE` are compared on Linux only.
- tmux behaviour: tmux calls the host libc. On macOS (Darwin libc, tmux 3.8 built with utf8proc as configure requires there): (1) `regexec` returns `REG_ILLSEQ` for any text holding a byte that is not UTF-8, so `#{s/b/Z/:@o}` on `a\377b\303\251c` copies the text unchanged and `#{s/x*/-/:@p}`, whose empty-match step lands inside a character (regsub.c `regsub` steps one byte), stops there; (2) `wctomb` fails past U+10FFFF, so `\U00110000` in a config file is `invalid \U argument`; (3) `isalnum(0xe9)` is true in a UTF-8 locale, so `display -\351` says `unknown flag` instead of `invalid flag`.
- vtmux behaviour: the core implements glibc's answers on every system (D-013 lists where glibc itself differs): the rest of the text is matched around the byte, `\U00110000` gives the old four-byte form, and a non-ASCII flag byte is `invalid flag`.
- Evidence: C probes on macOS 15 (`setlocale(LC_CTYPE, "en_US.UTF-8")` as tmux.c does): `regexec("b", "a\377b")` = 17 (`REG_ILLSEQ`), `regcomp("a\377")` = 17; `wctomb(0x110000)` = -1; `isalnum(0xe9)` true. The reference tmux 3.8 on the same host gives the outputs above.
- Category: Deferred (a per-platform libc model in the core; only text that is not valid UTF-8, or code points past U+10FFFF, is affected).

### D-020: `send-keys -K` delivers keys to the pane

- Test / assertion: `crates/vtmux/tests/attach.rs` `send_keys_k_delivers_keys_to_the_pane` (vtmux only; the reference drops the keys); oracle `extended_keys_sendk` (last three captures annotated). The bytes themselves are pinned against tmux by `keys.rs` `send_keys_in_each_key_mode_and_format`.
- tmux behaviour: `cmd_send_keys_inject_key` (cmd-send-keys.c:79) hands the key to the client as `key|KEYC_SENT`. Nothing strips the flag when the key reaches a pane (`server_client_key_callback` -> `window_pane_key` -> `input_key`), so `input_key` misses the trivial case (input-keys.c:628-646) and the tree lookup (655-662); named keys then hit the `KEYC_IS_SPECIAL` ignore (input-keys.c:681-685), and in modes 1 and 2 `input_key_extended`'s modifier switch hits `default` and returns -1. Most keys sent with `-K` to a pane are lost: in mode 1 even `a` and `Tab`.
- vtmux behaviour: the key is handled like a typed one, and the pane gets the same bytes as from `send-keys` without `-K` (and as from the attached client's own keystroke). Bindings and prompts see the key as in tmux.
- Evidence: reference tmux 3.8, attached client under a PTY, `send-keys -K -c <client> KEY` for `C-F3 BTab Escape F3 C-Up a Tab`, each followed by `send-keys -H 7c`, the pane copying its input to a file: standard mode `||\e|||a|\t` (keys 1, 2, 4, 5 lost), mode 1 nothing at all, mode 2 only `\e[27;2;9~` (BTab) arrives. vtmux gives every key (review_extkeys, `skk2.py`).
- Category: Do not copy tmux bug (keys are lost).

### D-022: children's `PATH` starts with the `tmux` shim (spec D5)

- Test / assertion: `crates/vtmux/tests/attach.rs` `tmux_shim_reaches_this_server_from_children` (vtmux only); `vtmux-server` `shim::tests`; regress `new-session-environment.sh` (`PATH=1` became `<shim>:1`).
- tmux behaviour: panes get the session/global `PATH` (or the command client's, spawn.c:418-424); jobs (`run-shell`, `if-shell`, `#()`, hooks) and popups get `environ_for_session` unchanged. A script's `tmux` is whatever that `PATH` finds.
- vtmux behaviour: at start the server makes `.<socket name>.bin/` (0700) beside its socket, holding `tmux` -> its own executable (symlinks resolved), and puts that directory first on `PATH` for every pane, respawned pane, popup and job, the config's `run-shell`/`if-shell` included; once only, when `PATH` already starts with it. The directory is removed when the server exits and a stale one is reused. Invoked as `tmux`, the binary finds this server through `$TMUX`, so `tmux set -g @x 1` from a script reaches it even when the server was started as `vtmux -L foo` and the user's `PATH` has another `tmux` first. `show-environment PATH` still shows the stored value. A pane's shell may still reorder `PATH` (a login shell's /etc/profile with macOS path_helper, or an rc file that prepends its own directories), so in an interactive pane the shim can end up after another `tmux`; jobs and non-login commands keep it first.
- Evidence: the owner's `.tmux.conf` under `vtmux -L foo`: mu-crew's poller and `run-shell` scripts ran the `tmux` 3.7c on `PATH`, which set `@mu_crew_*` on the wrong server, so the status line showed only the host pill. With the shim, after 12 s `@mu_crew_cpu_text`, `@mu_crew_uptime_value` and `@mu_crew_host_color` are set and `#{T:status-right}` has the CPU, RAM, host and uptime pills.
- Category: Spec decision (D5, drop-in: plugins call `tmux` and must not depend on the user's `PATH`).
- Patch: `compat/patches/0001-new-session-environment-path-shim.patch` (the script prints a pane's `PATH`; it drops a leading shim entry, a no-op under tmux).

### D-023: a dead pane's line after a last cell vtmux cannot write back

- Test / assertion: none pins the cells it cannot write back (`remain_on_exit` and `remain_on_exit_screen` keep away from them); reproduction below. Oracle `remain_on_exit_reflow` pins the lines it marks.
- tmux behaviour: `server_destroy_pane` (server-fn.c:406-408) scrolls the dead pane with `screen_write_linefeed(ctx, 1, 8)`, which marks the old last line wrapped whatever it holds, so `capture-pane -J` joins it to the `remain-on-exit-format` line, and a wider pane reflows the two into one: 20x4, `printf 'a\nb\nc\nlast'; exit 2` with the format `DEAD #{pane_dead_status}`, `capture-pane -pJ` ends `lastDEAD 2`; after `resize-window -x 30` the last row is `lastDEAD 2` with the cursor at 4,3 (tmux's grid holds only the cells written).
- vtmux behaviour: Ghostty marks a row wrapped only when a print wraps, so vtmux prints at the line's last column to get the mark (`remain.rs` `mark_wrapped`): rewriting the cell that was there, clearing a blank one the line used with ICH, or, past the cells the line used, a wide character that does not fit, which leaves the cell unwritten and the line's used cells (patch 0013's `cellused`) as they were. `capture-pane -J` and the reflow (patch 0020 joins a wrapped row by its used cells) then match tmux, `lastDEAD 2` wider and narrower (oracle `remain_on_exit_reflow`), except when that cell is one it cannot write back as it was (a wide character's half, a hyperlink, a line-drawing character, a TAB, a blank with attributes beyond its background): that line is not marked, `-J` prints it on a line of its own and a reflow keeps the two lines apart.
- Evidence: the reproduction above against tmux 3.8 and vtmux.
- Category: Ghostty grid (a row carries no settable wrap flag, D4).

### D-024: a second stdin read from inside `source-file -` fails at once

- Test / assertion: `crates/vtmux/tests/cli.rs` `source_file_stdin` (the cases tmux finishes: `source - -`, a file then `-`, `-` alone).
- tmux behaviour: a `source -` or `load-buffer -` among the commands read from stdin by `source-file -` (`printf 'display -p x\nsource -\n' | tmux source -`) waits for ever when the client's stdout is a pipe: the inner `file_read` of `-` needs the client's stdin, which the outer read already has, and the client never answers. With stdout redirected to a file it ends with `Input/output error: -`; when the inner `source -` is the first command it fails at once with `Bad file descriptor: -`. `tmux source - </dev/null` (stdin empty) returns 0 at once.
- vtmux behaviour: the inner read fails at once with `Bad file descriptor: -` (exit 1), the output of the commands before it printed, however stdout is connected; `source - </dev/null` returns 0 at once, as tmux.
- Evidence: reference tmux 3.8 under `timeout -k 5 6`, six runs each: `printf 'display -p x\nsource -\n' | tmux source -` and the `load-buffer -` form time out every time with stdout a pipe; `>file` gives `x` and `Input/output error: -`, exit 1. vtmux: 0.01 s, `x` then `Bad file descriptor: -`, exit 1, every time.
- Category: Do not copy tmux bug (tmux hangs).

### D-025: closed (a program's RIS is tmux's `screen_write_reset`)

- Was: `ESC c` went to Ghostty, whose full reset erased the history and left the alternate screen.
- Now: the scanner keeps the `c` from Ghostty and runs the reset `send-keys -R` uses (`PaneTerm::reset_screen`): history kept, the used rows scrolled into it under `scroll-on-clear`, the alternate screen and the title kept (input.c:1407-1412, screen-write.c:423-437). Goldens `cap_ris`, `fmt_ris`, `cap_ris_alt`, `fmt_ris_alt`; cli `program_output_reaches_the_grid_as_in_tmux` (cases `ris`, `risalt`).

### D-026: `vtmux -V` is not `#{version}` (spec D21, D25)

- Test / assertion: regress `format-variables.sh`, `test_var version "$VER"` with `VER` from `$TMUX -V | sed 's/^tmux //'`.
- tmux behaviour: `-V` prints `tmux 3.8` and `#{version}` is `3.8`, so stripping `tmux ` from one gives the other.
- vtmux behaviour: invoked as `vtmux`, `-V` prints vtmux's own version (`vtmux 0.0.0 (tmux 3.8)` today, `vtmux 3.8-beta1` under D25), while `#{version}` stays the tmux version `3.8` so plugins' version gates see tmux. Invoked as `tmux`, `-V` prints `tmux 3.8` as tmux does. The regress harness runs the binary as `vtmux`.
- Evidence: `vtmux -V` gives `vtmux 0.0.0 (tmux 3.8)`; `vtmux display -p '#{version}'` gives `3.8`.
- Category: Spec decision (D21, amended by D25).
- Patch: `compat/patches/0002-format-variables-version.patch` (the script takes the tmux version out of a `vtmux` `-V` line too; a no-op under tmux).

### D-027: `capture-pane -R` and `-F` model tmux's grid from Ghostty's

- Test / assertion: oracle `capture_grid_dump` and `capture_hyperlinks` pin the scenes where the model is exact. Regress `input-unicode.sh` (wide/widepad `-F`) and the overwrite scenes of `input-raw-unicode.sh` stay xfail on it.
- tmux behaviour: `-R` (`cmd_capture_pane_grid`, cmd-capture-pane.c:132-178) dumps tmux's own grid: each line's flags, `cellused`/`cellsize` and OSC 133 data, each cell's flags, attributes and colours. Line flags such as `GRID_LINE_EXTENDED` stay set once a cell needed the extended form; `cellsize` is how far the line has been allocated, which also bounds `-N` and `-e`; a palette colour keeps the form it was set with (`ESC[31m` is `red[1]`, `ESC[38;5;1m` is `colour1[1000001]` with `FG256`).
- vtmux behaviour: Ghostty keeps none of that but the palette form (patch 0010), so it is derived from the cells as they are now (`capture.rs` `tmux_cellsize`, `capture/raw.rs`): `EXTENDED` and `HYPERLINK` when a cell on the line needs them now; `cellsize` from the runs of written cells with one style, charset and link. A line whose wide character was overwritten (tmux keeps `EXTENDED`, and the other half as an uncleared cell), a line erased and rewritten shorter, and a line whose run tmux flushed in two pieces (plain text with an escape sequence in it that changes nothing, `zz ESC[m out`: tmux allocates up to the second piece's end, 5/5 on a 20-column line, vtmux one run, 5/10) show differently (`ESC[38;5;1m` matches since patch 0010, oracle `capture_sgr_256_low16`); under `-N` the trailing blanks follow the same model.
- Evidence: reference tmux 3.8, 10x3 pane, `printf '\343\201\202B\rX'`: `capture-pane -pR` line 0 `flags=EXTENDED[2] 3/5`, cell 0,1 `flags=NONE[0]`; vtmux `flags=NONE[0] 3/5` and `flags=CLEARED[40]`. 20x3, `printf 'zz\033[mout'`: tmux `capture-pane -pN` gives `zzout`, vtmux `zzout` and five spaces.
- Category: Ghostty grid (the state does not exist in Ghostty's grid).

### D-028: closed (the editor runs in a modal floating pane)

- Was: customize-mode's and choose-buffer's `e` ran `editor file` in a popup over the client whose key pressed it, not in a modal floating pane as `spawn_editor` does (spawn.c:703-788), so `list-panes` did not show it and other clients saw only `WAITING FOR EDITOR`.
- Now: `crates/vtmux-server/src/editor.rs` runs it as tmux does: a floating pane nine tenths of the window, centred, after the whole layout, modal and above a zoomed pane, `remain-on-exit` off; the file is read back when the pane's program exits 0 (`spawn_editor_finish`), and a pane killed first changes nothing. Regress `modal-pane.sh` passes; oracle `editor_modal_pane`.

### D-029: a client whose socket takes nothing for 10 s after its exit is dropped then

- Test / assertion: regress `control-client-exit-stalled.sh`; control `stalled_client_does_not_keep_the_server`, `slow_reader_detached_in_a_flood_exits_cleanly`.
- tmux behaviour: `server_client_check_exit` and `server_client_exit_timer` (server-client.c:2361-2440): a control client told to exit first loses the pane output not yet written (`control_discard`); if what is left has not drained after 10 s, everything queued is dropped (`control_discard_all`) and `MSG_EXIT` sent alone, and a client that has still not closed 10 s later is dropped. A control client's output goes down its own stdout descriptor and `MSG_EXIT` down the server socket, so a stalled terminal never holds up the exit message. The tmux client itself can still hang there: when its full stdout has fewer than 6 bytes free, its `%exit` `fflush` fails with `EAGAIN` (stdout is still non-blocking). `client_main` then makes stdout blocking, and the flush at `exit()` blocks for good. So its server goes only at the second 10 s step, 20 s after `kill-server`.
- vtmux behaviour: the same steps, but the output and the exit message share the one socket, and a message the socket has taken part of must be finished first. So at the first 10 s step a client whose socket has taken nothing for 10 s is dropped at once instead of being sent the exit alone; one still reading gets its exit after the part-written message.
- Evidence: `control-client-exit-stalled.sh` passes on both, 12.1 s each (`cargo xtask regress --tmux reference --only control-client-exit-stalled.sh`). A reader 64 KB per 10 ms behind a `yes` flood and then detached: both exit 0 with `%exit` within 0.5 s (reviewer repro rv_det2.py).
- Category: Deferred (the exit on a channel of its own).

### D-030: closed (DECAWM as tmux)

- Was: Ghostty set its pending-wrap flag on a print reaching the last column under DECAWM reset too, and a print with the flag set and DECAWM reset replaced the last cell (xterm), so a wrap pending when `?7l` arrived reported column 9 of 10 (tmux 10) and took the next characters; one filled under `?7l` and then `?7h` reported 10 (tmux 9) and wrapped. Ghostty also did not reflow the main screen while DECAWM was reset.
- Now: vendored patch `0016-tmux-decawm.patch` sets the flag only under autowrap, drops a character printed with the flag set and autowrap reset (`screen_write_cell`, "Sanity check cursor position"), and reflows the main screen whatever DECAWM says (`window_pane_resize`); `term.rs` `tmux_cursor` reports the column count whenever the flag is set. Goldens `cap_decawm_*`, `fmt_decawm_*`; oracle `decawm_resize_alt`.

### D-031: tmux's own character widths that Ghostty's clustering does not give

- Test / assertion: none fails; reproductions below. (Grapheme clusters themselves match: vtmux runs Ghostty with mode 2027, `term.rs` `keep_clustering`; regress `combine-test.sh` passes; cli `program_output_reaches_the_grid_as_in_tmux` case `cluster`.)
- tmux behaviour: `utf8_default_width_cache` (utf8.c:53-) makes a lone regional indicator one column; `screen_write_combine` attaches U+FE0F to any character and makes it two columns wide (`variation-selector-always-wide`), not only an emoji.
- vtmux behaviour: Ghostty's widths: a lone regional indicator is two columns, and U+FE0F after a character that is not an emoji is dropped (the cell stays one column). Clusters (regional-indicator pairs, emoji modifiers, ZWJ sequences, VS16 after an emoji, Hangul) match.
- Evidence: 20x3 pane, reference tmux 3.8: `printf 'x\360\237\207\254y'` gives `#{cursor_x}` 3, vtmux 4; `printf 'x\357\270\217y'` 3 with U+FE0F kept on `x`, vtmux 2.
- Category: Deferred (tmux's table over Ghostty's, through the width override patch 0007 for the lone indicator; the VS16 rule needs a Ghostty change).

### D-032: lines scrolled out of a scroll region that does not start at the top stay out of history

- Test / assertion: regress `input-raw-scroll.sh` (scene `index`), xfail; goldens `cap_region_scroll_history`, `fmt_region_scroll_history_size` (ignored, `crates/vtmux-vt/DIVERGENCES-PENDING.md` P1).
- tmux behaviour: `grid_view_scroll_region_up` (grid-view.c:107-117) moves the line scrolled off the top of any region into history (`grid_scroll_history_region`), whatever row the region starts on.
- vtmux behaviour: Ghostty creates scrollback only for a region whose top margin is row 0 (Terminal.zig `index`), so the line is lost.
- Evidence: 6x4 pane, `printf 'A\nB\nC\033[2;3r\033[2;1HX\033D\033DY'`: tmux `#{history_size}` 1 and capture `-S -` starts `X`; vtmux 0.
- Category: Deferred (a Ghostty change: a region scroll that copies its top row into scrollback before rotating, about a day with its page bookkeeping).

### D-033: attach also asks the terminal for palette entries 0-15 (OSC 4)

- Test / assertion: attach `colour_queries_at_attach_and_on_a_theme_report` (the OSC 4 queries are the one byte-level difference it allows); oracle `terminal_colour_replies` (the replies change nothing on screen).
- tmux behaviour: `tty_send_requests` (tty.c:406-425) ends with `ESC]10;?ST ESC]11;?ST`; `tty_repeat_requests` (tty.c:428-451) sends the same two after a theme report. Palette replies are parsed (`tty_keys_palette`) only to answer a pane's own OSC 4 query.
- vtmux behaviour: the same two, then `ESC]4;N;?ST` for N 0-15, both at attach and after a theme report. Their replies are kept with the client's other reported colours (`term_queries::Reported`) and, while one of these queries is out, answer no pane's OSC 4 request (tmux sends none, so it must not answer or drop one); nothing changes on screen unless `vtmux-pane-active-blend` or `vtmux-pane-inactive-blend` is set.
- Evidence: the attach test above compares both binaries' startup bytes for OSC 10/11 and asserts the OSC 4 queries only for vtmux.
- Category: Spec decision (D26: the pane blend mixes palette cells from the terminal's own palette, `docs/specs/2026-11-04-pane-blend.md`).

### D-034: a search matching almost every line of a long history counts partially

- Test / assertion: none (timing); reproduction below.
- tmux behaviour: `window_copy_search_marks` (window-copy.c:4876-5003) counts the matches over all of history unless that takes over 200 ms (`WINDOW_COPY_SEARCH_ALL_TIMEOUT`), then shows `1000+`-style counts (`#{search_count_partial}`). tmux's grid reads fast enough that 100000 lines with a match on each count exactly (0.11 s).
- vtmux behaviour: the same rule; a line costs more to read (decoded from Ghostty's page, about 1.2 µs a line against tmux's grid) and to stringify, so a match on each of 100000 lines runs out the 200 ms and counts `1000+`. Searches matching a few lines in ten count exactly on the same history (`^5`, `^5.*0.some` below, 0.25 s with the search itself).
- Evidence: release build, 80x24, history-limit 200000, `seq 1 100000 | sed 's/$/ some filler text here/'`, `copy-mode`, `send -X search-backward RE`, `display '#{search_count}#{?search_count_partial,+,}'`: tmux `filler` 100000, `some f` 100000, `^5` 11111, `^5.*0.some` 1111; vtmux `1000+`, `1000+`, 11111, 1111.
- Category: Deferred (a faster row read: keep a text-only form of each line for searching, or decode a page's rows in one call).

### D-035: control output backpressure works on whole flushes and one queue

- Test / assertion: control `reply_buffer_cap_lets_a_stalled_client_go`, `pause_after_pauses_a_pane_the_client_is_behind_on`, `slow_reader_keeps_server_memory_bounded`, which compare only what both sides do the same.
- tmux behaviour: `control_check_reply_buffer` (control.c:429-451) runs for each `control_write` line, so the stream before `%exit too far behind` can stop in the middle of a command's guard. `control_write_callback` (control.c:925-958) takes the pending panes in turn, each up to its share. The buffer it measures is the client's own stdout descriptor.
- vtmux behaviour: the cap applies to each flush of a command's lines (its `%begin`, output and `%end` together), so the stream stops at a flush boundary. Pane output goes out in the order it was read, cut to the same per-pane share. The buffer measured is the server's queue for the socket, with the socket and the relaying client in front of the reader.
- Evidence: a stalled client sent 80 `show-buffer` of 1 MiB: both exit 0 with `%exit too far behind` after about 66 MB; tmux ends after a `%begin`, vtmux after an `%end`. Each pane's bytes arrive complete and in order on both, but how panes interleave under a backlog already depends on buffer timing in tmux.
- Category: Deferred (per-line cap and round-robin pane order; nothing known depends on either).

### D-036: a line keeps no cells past the width it was narrowed to

- Test / assertion: oracle `reflow_cells_used` scene `grow` (annotated, the last `capture-pane -e`).
- tmux behaviour: a line that fits the narrower width is moved as it is (`grid_reflow_move`, grid.c), so cells past the new width stay allocated (`cellsize`) and are drawn again when the pane widens: a line written `ab` then erased to its end with a red background on 20 columns, narrowed to 10 and widened back to 20, is red to column 20.
- vtmux behaviour: Ghostty's reflow copies a row into rows of the new width, so the cells past it are gone: the same line is red to column 10, then default. Narrowing itself matches (patch 0013 measures the line by tmux's `cellused` and keeps the background cells up to the new width).
- Evidence: reference tmux 3.8, `printf 'ab\033[41m\033[K\033[0m\r\nnext'` on 20x8, `resize-window -x 10`, then `-x 20`: `capture-pane -pe` `ab\e[41m` / `\e[49mnext`; vtmux `ab\e[41m        \e[49m` / `next` (eight red cells, then the default background). Widened past the first width the same way, a line erased in green and then written (`printf '\033[42m\033[K\033[0mafter-bg-erase\r\n'`, 20 -> 15 -> 25 columns, oracle `decawm_resize_alt` scene `bgerase`, annotated): tmux keeps the green cells to column 20 and its `capture-pane -pe` trims them (`after-bg-erase\e[42m`); vtmux has one green cell and default cells after it (`after-bg-erase\e[42m \e[49m`). 80 -> 60 -> 100 columns (review_reflow_cells_used): tmux `after-bg-erase\e[42m` (its cells kept to column 80, trimmed), vtmux `after-bg-erase\e[42m` and 46 green blanks to column 60, then `\e[49m`.
- Category: Ghostty grid (a row holds as many cells as the pane is wide).

### D-037: a client that starts as `kill-server` finishes finds no server, not a dropped connection

- Test / assertion: cli `kill_server_closes_the_socket_before_it_answers` (vtmux only); oracle `format_loops` and `config_errors` start a server right after `kill-server`.
- tmux behaviour: `kill-server` raises SIGTERM; the listening socket stays open until the process exits (`server_loop`, server.c:268-306), and a client that connects meanwhile is accepted and closed (`server_accept`, server.c:396-399): `server exited unexpectedly`, and a following `new`/`start-server` never starts a server. The window is a few ms.
- vtmux behaviour: with no job the server waits for (`job_still_running`), the socket and the `tmux` shim are closed before `kill-server`'s client is answered, so the next client is refused and starts a fresh server. With a job running the socket stays open as in tmux (`kill_server_with_running_job`), and the shim with it; when the server then exits the shim goes before the socket stops listening, so it never removes the shim of a server started after it (server.rs `a_fresh_server_keeps_its_shim_when_the_old_one_exits`).
- Evidence: `kill-server` then `start-server ; set -g exit-empty off` at once, 8 busy loops, 150 rounds each: vtmux 11/150 failed (`server exited unexpectedly`, then `no server running`), reference tmux 0/450 (its window is shorter; vtmux's turn runs render and compression between the reply and the socket close). With the change, the cli test passes 60/60 under load; without it, 37 of 81 runs fail.
- Category: Do not copy tmux bug (a command that follows `kill-server` is lost to a server that is already leaving).

### D-038: the alternate screen keeps no cells past the width it was narrowed to

- Test / assertion: oracle `decawm_resize_alt` scene `alt` (annotated, the `capture-pane` after widening back).
- tmux behaviour: the alternate screen is resized without reflow (`window_pane_resize`: reflow only without `saved_grid`), and `screen_resize_cursor` then only changes the grid's `sx`: every line keeps its cells, so narrowing hides the cells past the new width and widening shows them again, before the program redraws.
- vtmux behaviour: Ghostty's resize without reflow (`PageList.resizeWithoutReflow`) clears the cells past the new width, so widening shows blanks there until the program redraws. Keeping them would need every full-line clear and scroll to clear past the width too (tmux's lines go whole, `grid_empty_line`), and Ghostty's page bookkeeping (style counts, clone, compression) to cover cells past the width.
- Evidence: reference tmux 3.8, a pane on 20 columns printing `\033[?1049h\033[?7labcdefghijklmnopqrstuvwxyz\r\n0123456789abcdefghij\r\nshort`, then `resize-window -x 14` and `-x 20`: `capture-pane -p` `abcdefghijklmnopqrsz` / `0123456789abcdefghij` / `short`; vtmux `abcdefghijklmn` / `0123456789abcd` / `short`. A program that redraws on SIGWINCH (pi, `scripts/agent_tui.py`, a full-screen editor) hides it; it shows between the resize and that redraw, or in a program that does not redraw.
- Category: Ghostty grid (a row holds as many cells as the pane is wide).

### D-039: a blank cell left of a vertical border is written as a space to carry the bidi isolate

- Test / assertion: oracle `bidi_border_isolates` (annotated, client b's two screens: the rows whose cell left of the border is blank). Regress `screen-redraw-bidi.sh` passes.
- tmux behaviour: for a UTF-8 client whose terminal has `Bidi`, `redraw_draw_border_span` (screen-redraw.c:1256-1264) moves the cursor to a vertical border span and writes U+2069 there, after the pane line was drawn with its trailing blanks erased (`tty_draw_line`, EL or ECH for ten or more). tmux's own grid joins the zero-width character to the erased cell left of the cursor (`screen_write_combine`), so tmux in tmux shows it; libghostty-vt drops a zero-width character after a cell nothing was written to (Terminal.zig `print`), so an observer, or vtmux as the outer terminal, does not.
- vtmux behaviour: frames are cells, so the isolate goes on the cell left of the span; a blank one is written as a space carrying it. vtmux in tmux and vtmux in vtmux show what tmux in tmux does (the regress scene, both ways); a libghostty-vt observer shows the U+2069 on those rows, where tmux's own isolate is dropped.
- Evidence: regress `screen-redraw-bidi.sh` with the outer server reference tmux 3.8 and the inner vtmux passes, as does vtmux in vtmux; the oracle trace of tmux 3.8 shows no U+2069 left of the border on rows with no text there (`bidi_border_isolates` step 17).
- Category: Ghostty grid (libghostty-vt's rule for a zero-width character after an empty cell; no arbiter run: tmux's grid and Ghostty's disagree, and vtmux's bytes give tmux's result on tmux's).

### D-040: a tab wider than the pane gets no empty rows before it on a reflow

- Test / assertion: oracle `reflow_tmux_quirks` scene `tabwide` (annotated, the `display` and `capture-pane` after the second narrowing).
- tmux behaviour: `grid_reflow_split` (grid.c) moves to the next line before every cell for which `width + gc.data.width > sx`, also when nothing is on the line yet (`width` 0): a TAB cell wider than the pane at the point where a line splits leaves an empty line before it, and a line holding only such a tab (`at` stays 0) leaves two, the original line emptied and the one skipped. Each further narrowing below the tab's width adds more.
- vtmux behaviour: the tab starts the next row with nothing before it (patch 0015 measures it as tmux does, `whole_tabs` keeps it a TAB cell).
- Evidence: reference tmux 3.8, `printf 'ab\r\n\tcd\tx'` on 20x6, `resize-window -x 7` then `-x 5`: tmux `#{history_size}` 7, `capture-pane -p` `ab` / blank / blank / TAB / blank / `cd` / TAB / blank / `x`; vtmux 5 and `ab` / TAB / blank / `cd` / TAB / blank / `x`. Found by a random-content reflow probe (`lows_batch_k`, 100 seeds of wide characters, tabs, links and erases, five resizes each): with tabs replaced by spaces every seed matches tmux but the D-041 ones.
- Category: Deferred (blank rows tmux makes out of nothing; only tabs wider than the pane, 8 columns or fewer).

### D-041: a reflow that joins a whole row keeps the line's soft wrap where the next row does not fit

- Test / assertion: oracle `reflow_tmux_quirks` scene `lostwrap` (annotated, the `display` and `capture-pane -J` after widening).
- tmux behaviour: `grid_reflow_join` (grid.c) sets `wrapped` from the next line before it tests whether that line's first cell fits; when it does not, and the line before it was joined whole, `left` is 0 and the target line loses `GRID_LINE_WRAPPED` although the rest of the line follows: the logical line becomes two (`capture-pane -J`, copy mode and later reflows see two lines), and the cursor at the line's end goes to the end of the first.
- vtmux behaviour: the line stays one wrapped line; the next row is its continuation.
- Evidence: reference tmux 3.8, `printf 'abclklongwordlongword longwordlongword 字漢😀'` on 40x6, `resize-window -x 9` then `-x 23`: tmux `capture-pane -pJ` `abclklongwordlongword longwordlongword 字漢` / `😀`, `#{history_size} #{cursor_x},#{cursor_y}` `1 20,0`; vtmux one line ending `字漢😀`, `1 2,1`.
- Category: Do not copy tmux bug (the soft wrap of a line is lost on a resize).

