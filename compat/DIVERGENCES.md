# Divergences from tmux 3.8

vtmux follows tmux 3.8 exactly except where recorded here (spec: *Divergence
process*). Every entry needs evidence. There is no "intentional divergence for
product reasons" category.

## Categories

- **Ghostty grid**: the D4 arbiter ruled for Ghostty's terminal-emulation behaviour over tmux's.
- **Do not copy tmux bug**: tmux loses data, hangs or is unsafe. Needs evidence and a regression test.
- **Deferred**: not done yet. Must not be claimed as compatible.

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

### D-003: ERE and fnmatch corner cases

- Test / assertion: none yet (task `regex_corners`); hand probes against glibc 2.43 and tmux 3.8 from review_fmt_regex (reviewer-8).
- tmux behaviour: (1) `^` does not match inside later iterations of a repeated group: `(^a|b)+` on `abab` is no match in glibc. (2) A repeated group whose iterations can match empty reports the last iteration: `(a*){2}` on `aa` gives `\1` = 2:2. (3) With `REG_ICASE` an escaped ordinary letter (`\a`) never matches. (4) The `s///` empty-match progression steps one byte (regsub.c:regsub `start += m[0].rm_eo + 1`), splitting UTF-8 characters. (5) fnmatch `[a-[:digit:]]` does not match `5`.
- vtmux behaviour: (1) `(^a|b)+` on `abab` matches 0:2. (2) without backreferences, captures follow glibc's `set_regs` and `(a*){2}` gives `\1` = 2:2. Backreference patterns use a backtracker whose captures can differ: `(a*){2}\1` on `aa` matches 0:2, where glibc reports no match. Anchors inside repeated groups can also change captures. (3) `\a` with `REG_ICASE` matches `a`. (4) empty-match progression steps one character. (5) `[a-[:digit:]]` matches `5`.
- Evidence: review_fmt_regex notes; probe scripts in that review (`/tmp/rv8probe`, out of tree).
- Category: Deferred

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

### D-006: the wrap flag of the row above survives EL/ED/IL/DL

- Test / assertion: none yet; reviewer-21's scenarios `rv21_v1..v4`, `rv21_w1`, `rv21_w3` (review_draw_borders_3, out of tree in `/tmp/rv21/`).
- tmux behaviour: `grid_clear_lines` and `grid_move_lines` (grid.c) clear `GRID_LINE_WRAPPED` on the line above whenever a full-width clear or move touches a row, so EL 2, EL 0 or ED 0 from column 0, IL and DL all unwrap the row above. `tty_draw_line` then treats the rewritten row as not wrapped and erases a leading run of ≥ 10 blank cells with EL1 (the observer sees default fg on those blanks).
- vtmux behaviour: libghostty-vt keeps the soft-wrap flag on the row above through those edits, so the compositor still sees a wrapped continuation and writes the leading run as spaces in the row's last style (the observer sees the old fg on blank, attribute-free cells). Text, attributes, cursor and every non-blank cell are identical.
- Evidence: reviewer-21, oracle live, 40x10 pane, ref vs vtmux at 4c3bab8: `\e[31m` + 40 × `x` + 12 spaces + `y`, then `\e[2;1H\e[2K` + red 12 spaces + `y`: ref row 1 `\e[39m            \e[31my`, vtmux `            y` (red).
- Category: Deferred (needs a wrap model that follows tmux's grid bookkeeping rather than Ghostty's; the only visible effect is the foreground colour of blank cells).

### D-007: terminal-overrides is a subset, and terminals come from a built-in table

- Test / assertion: `crates/vtmux-core/src/term_features/golden.rs` (178 reference rows from `tests/term_features_cases.py`), `crates/vtmux/tests/attach.rs` `terminal_features_apply_at_attach`.
- tmux behaviour: `tty_term_create` (tty-term.c) loads the client's terminfo entry (`setupterm`), applies `terminal-features` (tty-features.c), `COLORTERM`, then `terminal-overrides` (`tty_term_apply`) to any of ~150 capability names. Later device-attribute replies add features too (`tty_default_features`: `XTVERSION`, DA2), and the terminfo entry may name any terminal.
- vtmux behaviour: vtmux has no terminfo model (D8). Terminals come from a built-in table (`xterm*`, `xterm-ghostty`, `tmux-256color`, `screen`, `screen-256color`; anything else, `foot`, `linux` and `dumb` included, is refused at attach with `open terminal failed: unsupported terminal (xterm*, xterm-ghostty, screen, screen-256color, tmux-256color)`; tmux attaches `foot` and `linux`). Feature parsing, `terminal-features` matching, `COLORTERM` and the `-T`/`-2` flags follow tmux exactly. `terminal-overrides` honours only `Tc`, `RGB`, `setrgbf`, `setrgbb`, `Sync`, `Smulx`, `Setulc`, `Ss`, `Se` and `Ms`. For all of them except `Ms` only the presence of the capability is kept; the value is ignored and vtmux writes its own fixed sequences: `Sync=x` or `Sync=QQ%p1%dQQ` still gives `ESC[?2026h` where tmux writes the template (nothing at all for a value that does not expand to a sequence), and `setrgbf` is always `38;2;r;g;b` where Ghostty's terminfo makes tmux write `38:2:r:g:b`. `Ms` is validated with a model of `tty_term_validate`'s check, fitted to 40 probed templates. Every other name is parsed and ignored, so `#{Ic:name}` is not answered for it. A `sync` reply to the DECRQM query is handled (`tty_keys_sync`); the XTVERSION and DA replies do not add features. `Smulx`/`Setulc`, `Ss`/`Se`, `Ms` and the `hyperlinks` feature are tracked and listed but the compositor does not yet write styled underlines, cursor styles, OSC 52 or OSC 8 for them.
- Evidence: reference tmux 3.8 `list-clients -F '#{client_termfeatures}|#{client_colours}|#{Ic:...}'` on a PTY client for every row of `golden/cases.tsv`; `Ms` probes in `term_features_cases.py` rows `xterm*:Ms=...`.
- Category: Deferred

### D-008: respawn-window -k on a zoomed window

- Test / assertion: `crates/vtmux/tests/cli.rs` `windows_respawn_zoomed` (vtmux only: the reference dies).
- tmux behaviour: `spawn_window` (spawn.c:106-128) with `SPAWN_RESPAWN` frees the layout (`layout_free`) and the other panes but leaves `WINDOW_ZOOMED` set with the saved layout pointing at freed cells. The next command that reads the window's layout (`display -p '#{window_layout}'`, `splitw`) kills the server (`server exited unexpectedly`).
- vtmux behaviour: the window is unzoomed first, then respawned: one pane, full size, `window_zoomed_flag` 0.
- Evidence: reference tmux 3.8, isolated: `new -d -s a cat; new -d -s k cat; splitw -d -t a cat; resizep -Z -t a:0.1; respawnw -k -t a; display -p -t a '#{window_layout}'` -> `server exited unexpectedly`, exit 1, and every later command says `no server running`.
- Category: Do not copy tmux bug

### D-009: run-shell output to a pane or an attached client is dropped (no view mode)

- Test / assertion: regress `run-shell-output.sh`, `[ "$($TMUX display -p '#{pane_mode}')" = "view-mode" ]` after `run -t: 'echo foo'`; the startup config's causes for an attached terminal client (`cfg_show_causes`).
- tmux behaviour: `cmd_run_shell_print` (cmd-run-shell.c:79-104) writes the job's output lines, and its `returned N` / `terminated by signal N` message, into the target pane's `window_view_mode` when `-t` names a pane, or when there is no waiting item (`-b`, or a config file): the active pane of the client's session or of `cmd_find_from_nothing`. `cmdq_print` from an attached terminal client goes to view mode too (`server_client_print`, server-client.c:3259-3275), and so do the config's causes once a session is attached (cfg.c:258-279).
- vtmux behaviour: vtmux has no view mode (`window-copy.c` modes are not implemented), so that output is dropped and `#{pane_mode}` stays empty. Output to a command client's stdout (no `-t`, without `-b`, from the CLI or a control client) is printed as tmux prints it. Config causes stay queued until the next `source-file` prints them.
- Evidence: reference tmux 3.8, isolated: `new -d 'sleep 100'; run -t: 'echo foo'; display -p '#{pane_mode}'` -> `view-mode`; vtmux prints an empty line.
- Category: Deferred

### D-010: copy mode is partial (U1, U2 and U3 of the copy-mode split)

- Test / assertion: regress `copy-mode-commands.sh`, `copy-mode-no-client-scroll.sh` and `copy-mode-redraw.sh` stay xfail; oracle `copy_enter`, `copy_move`, `copy_resize`, `copy_clone`, `copy_select_draw`, `copy_select_mode` and `copy_search_draw`, `copy_search_kinds`, `copy_search_jumps`, `copy_search_prompts` pin what exists.
- tmux behaviour: window-copy.c has 99 commands, selections, search, jumps, marks, copy/pipe, line numbers, refresh, view mode and the mouse.
- vtmux behaviour: `copy-mode` (`-d -e -H -k -q -s -S -u`; `-M` is an error), the pane mode stack, keys through the `copy-mode`/`copy-mode-vi` tables, `send-keys -X` with `-N` and `-F` (search strings), cursor/line/page/half-page/history-top/bottom/scroll movement, `goto-line`, `recentre-top-bottom`, `cancel`, scroll-exit, the copy-mode formats and the position indicator. Selections (begin/stop/clear-selection, other-end, rectangles, `selection-mode`, select-word, select-line), word and space movement, `copy-selection*`, `append-selection*`, `copy-line*`, `copy-end-of-line*`, the `selection_*` formats and the selection drawn in `copy-mode-selection-style`. Search (regular expression, text, incremental, again/reverse, `wrap-search`), the `search_*` formats and `pane_search_string`, matches in `copy-mode-match-style`/`-current-match-style`, jumps, `set-mark`/`jump-to-mark` in `copy-mode-mark-style`, matching brackets, paragraphs, and `previous-prompt`/`next-prompt [-o]` over the lines OSC 133 marked (vtmux-vt pins each mark to its row; Ghostty's own semantic-prompt state is not tmux's line flags). The grid is copied on entry and resized and reflowed with the view as tmux does (`window_copy_clone_screen`, `window_copy_resize`). Piping and refresh fail with `not implemented in vtmux yet: copy mode command NAME`. A line's time (`top_line_time`) is lost when the pane's width changes before copy mode is entered (Ghostty's reflow does not say which row went where), so the indicator shows no time for it; the same reflow keeps an OSC 133 mark on the row its pin follows. `copy-mode -S` without a mouse event scrolls to the top of history (the slider position for `my = 0`). Line numbers are not drawn, so `line-numbers-on`/`-toggle` are not implemented. vtmux draws the whole mode screen after every command, where tmux redraws only the indicator and the cursor's lines after a move (`WINDOW_COPY_CMD_MOVE`, window-copy.c:3967-3976): when a word or line selection widens over lines the cursor did not cross, tmux's terminal keeps the old selection on those lines until the next full redraw, and vtmux shows the selection the formats report (with search marks present tmux redraws everything, as vtmux does).
- Evidence: `cargo xtask oracle replay --only 'copy_*'`; cli.rs `copy_mode_formats` and `copy_selection_formats` equal tmux 3.8; attach.rs `owner_copy_mode_vi_bindings`, `owner_prompt_jumps`; regress `copy-mode-selection-mode.sh` passes.
- Category: Deferred

### D-011: copied text and word movement over cells Ghostty stores differently

- Test / assertion: regress `copy-mode-test-emacs.sh` (`printf "words\n\tIndented"`) and `copy-mode-unicode-whitespace.sh` (`check_cursor 2,17` after next-word over `xa\302\205b c`), both xfail; each passes with only that assertion changed.
- tmux behaviour: a TAB that moves over blank cells is stored as one TAB cell (`grid_set_tab`, input.c:1354-1360), copied as `\t` (`window_copy_copy_line`) and whitespace to word movement. U+0085 has no width (`utf8_width`: wcwidth -1, a C1 control, 0) and combines into the cell before it, so `xa\302\205b` is three cells.
- vtmux behaviour: Ghostty stores a tab as the blank cells it moved over (`d_tab_capture`, vt P4), so copy mode copies spaces; word movement still treats them as whitespace, so cursor positions agree. Ghostty prints U+0085 as a cell of its own (`#{cursor_x}` 6 vs tmux 5 after `xa\302\205b c`), so next-word over it stops one column later.
- Evidence: `printf 'aa\tbb'` then `capture-pane -p`: tmux `aa^Ibb`, vtmux `aa      bb`; `printf 'xa\302\205b c'` then `display -p '#{cursor_x}'`: tmux 5, vtmux 6.
- Category: Deferred (grid-level, D4 arbiter not run; the tab case follows `d_tab_capture`)

### D-012: a new window's first automatic-rename check waits one name interval

- Test / assertion: cli.rs `initial_window_name_is_the_command` (vtmux only; the reference's outcome is not repeatable, so it cannot be compared).
- tmux behaviour: `spawn_window` sets the name from `default_window_name` (the command's first word), and `check_window_name` (names.c) runs at the end of the same loop turn, with no `name_time` yet. `#{pane_current_command}` then reads the foreground process of the new PTY, which is usually still the forked server (`tmux`) and sometimes the shell before it execs or the command itself. So `new -d -s a cat; neww -d cat; neww -d cat` gives `tmux`, `cat` and `sh` in different runs (20 runs: 10 `tmux/tmux/tmux`, 6 `tmux/tmux/cat`, 2 `cat/cat/tmux`, 2 `tmux/cat/cat`), and `window-renamed` fires for the ones that lose. A window that prints nothing keeps the wrong name.
- vtmux behaviour: the window's name clock starts at creation, so the first check comes one `NAME_INTERVAL` (500 ms) later, when the command has exec'd. The window keeps the name from `default_window_name` (`cat`) or settles on the command's `argv[0]` (`sleep` for `sh -c 'exec sleep 5'`), and no spurious `window-renamed` or `%window-renamed` fires at start.
- Evidence: 30 runs of `new -d -s a cat; neww -d cat; neww -d; neww -d sleep 4; neww -d 'exec sleep 5'` with SHELL=/bin/sh give `cat/cat/sh/sleep/sleep` every time; before the change `sh/cat/...` mixes. Reference runs of the same script vary as above.
- Category: Do not copy tmux bug (the name depends on a scheduler race).
