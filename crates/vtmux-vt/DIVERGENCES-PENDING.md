# vtmux-vt: pending divergences

Entries for `compat/DIVERGENCES.md`, which lands with the regress merge. Move
them there (and assign D-NNN numbers) when both are on main. Category for all of
them: **Deferred** (not done yet; must not be claimed as compatible). Each has an
ignored golden in `tests/golden.rs`; un-ignore it when the entry is closed.
Evidence is the reference tmux 3.8 run recorded in the golden's `command=` line
and the probes from `review_vt_wrap`.

## P1. Scroll regions that do not start at row 0 do not feed history

- Golden: `cap_region_scroll_history`, `fmt_region_scroll_history_size`
- tmux: `grid_view_scroll_region_up` (grid-view.c:107-117) moves the line scrolled
  out of *any* region into history when `GRID_HISTORY` is set
  (`grid_scroll_history_region`, grid.c:529). 20x5, `l0..l3`, `ESC[2;4r`,
  `ESC[4;1H`, three LF, `X` gives `#{history_size}` 3 and capture `-S - -E -`
  `l1 l2 l3 l0 "" "" X`.
- vtmux (Ghostty): only a region whose top margin is row 0 scrolls into history,
  so `history_size` and the capture differ.
- Visible: `#{history_size}`, `capture-pane -S`, copy mode, for programs that use
  top/bottom margins on the main screen (tmux itself, some TUIs).

## P2. Reflow of a cursor at the end of a row

- Golden: `fmt_reflow_pending_wrap_wide_cursor_x`,
  `fmt_reflow_pending_wrap_wide_cursor_y`,
  `fmt_reflow_pending_wrap_wide_history_size`
- Closed (`gap_render_styles`): narrowing now pushes the split rows into
  history and widening joins them back, the blank rows below the cursor kept,
  as `grid_reflow` does (`PaneTerm::resize`; goldens `cap_reflow_narrow`,
  `fmt_reflow_narrow_*`, regress `cursor-test4.sh`, oracle
  `reflow_footer_under_cursor`).
- Left: a cursor that ends a row after the reflow (waiting to wrap, or its
  column a multiple of the new width): tmux leaves `cx` past the end of that
  row (`grid_unwrap_position`), Ghostty starts the next row with it, so that
  step's `#{history_size}` is one more and the cursor is at column 0 of the
  row below (30 lines, prompt of 24 columns, `resize-window -x 12`: tmux
  `102 12,19`, vtmux `103 0,19`; a later resize agrees again).
- Pending-wrap cursor: 20x5, `0123456789abcdefghij0123`, `short`, then ten
  wide `漢字` cells filling the row (wrap pending), resized to 7x5: tmux puts
  the cursor at 4,3 with `#{history_size}` 6. Ghostty 3bd54e025 gives 2,4
  with `history_size` 4 (ghostty a887df42c gave 7,4, past the last column).
  The ASCII case (20 chars at 20x5, resize 7x5) now matches tmux's
  `cursor_x` 6 (`fmt_reflow_pending_wrap_cursor_x`; a887df42c gave 7).
- Visible: `#{history_size}`, `#{cursor_x}`, `#{cursor_y}`, CPR and
  `capture-pane` after a resize that leaves the cursor at the end of a row.

- Tab cells (`d_tab_capture`, golden `cap_tab_resize_narrow`): narrowing 20 -> 12 columns a line `ab<tab>c`
  (the tab covers 6 columns, so the line is 9 wide and fits): tmux wraps it (`ab<tab>` then `c`, one row into
  history; `grid_reflow` keeps a TAB cell whole), Ghostty keeps `ab<tab>c` on one row. A tab cut by a narrower
  row is two blanks in vtmux (`whole_tabs`), and tmux wraps the whole tab.

- Also (review_hist_limit_live, reviewer-63, `/tmp/rv/t7.sh`, `t6.sh`): after a reflow of a full history (limit set at creation or lowered live), vtmux's `history_size` is one higher than tmux's: limit 100, 300 styled 130-column lines at 50 columns, `resize-window -x 30` then `-x 50`: tmux 172 then 99, vtmux 173 then 100 (limit 200: 322/189 vs 323/190; 333-335 match). Same family: reflow and the history count differ around the limit.

## P3. ZWJ sequences do not cluster

- Golden: `fmt_zwj_cursor_x`
- tmux: `screen_write_combine` (screen-write.c:2850) joins ZWJ sequences into one
  cell: `x` + family emoji + `y` leaves `#{cursor_x}` at 4.
- vtmux (Ghostty without mode 2027): does not cluster, `cursor_x` 8, which is what
  xterm does.
- Visible: `#{cursor_x}`, CPR and capture of emoji text.
- Same for regional-indicator pairs and VS16 (regress `input-raw-unicode.sh`,
  scenes flag and variation, `capture-pane -R`): tmux makes `\U0001F1EC\U0001F1E7`
  one cell `data=(2,8,...)` and `*` + U+FE0F one width-2 cell; Ghostty keeps
  two width-2 cells, and `*` width 1 with the selector dropped.

## P4. Tab cells: closed (`d_tab_capture`)

- Ghostty has no tab cell (checked at 3bd54e025), so vendored patch
  `0002-tab-cell-mark.patch` marks the cells an HT crosses (`Cell.tab`, spare
  bits of the cell) and `vtmux-vt` rebuilds tmux's TAB cell from the marks
  (first cell as wide as the tab, the rest padding). Goldens `cap_tab*`,
  `fmt_tab_cursor`; oracle `copy_tab_cells`; regress
  `copy-mode-test-emacs.sh` passes. Remaining difference, a narrowing resize
  of a line holding a tab (`cap_tab_resize_narrow`, folded into P2): tmux
  wraps the line even when it fits, and a tab cut by a narrower row is
  blanks.

## P5. Ghostty retains history by whole pages, not lines

- No golden (memory only; nothing tmux shows differs).
- Ghostty's line limit (`OPT_SCROLLBACK_MAX_LINES`) prunes whole pages, so it
  can't keep exactly `history-limit` rows. `PaneTerm::retain` sets it to
  `max(history-limit, hsize)` plus one page of rows at the current width, with
  the byte limit off, and models tmux's `hsize` itself (`PaneTerm::feed`),
  hiding the excess: Ghostty keeps every row tmux has and at most one page
  more (578 rows at 80 columns, 93 at 500), whatever the styling.
- Resize lifts the limit, reflows and sets it again for the new width, pinning
  tmux's oldest row; under the alternate screen it stays lifted until the
  primary screen is back (`alt_entry`), as Ghostty reflows the primary at every
  resize and tmux only once, on `?1049l`.
- `history-limit 0` keeps a byte limit of 0 (no history at all), since a line
  limit can't go below one page.
- Visible: memory only (one page per pane).

## P6. Scroll-on-clear does not feed history

- Golden: `cap_scroll_on_clear_ed2`, `fmt_scroll_on_clear_ed2`,
  `cap_scroll_on_clear_ctrl_l`, `fmt_scroll_on_clear_ctrl_l` (follow-up
  `history_parity`)
- tmux: with the pane option `scroll-on-clear` (default on, options-table.c:1723),
  ED2 (`screen_write_clearscreen`, screen-write.c:2094) and ED0 at 0,0
  (`screen_write_clearendofscreen`, screen-write.c:1936-1942) call
  `grid_view_clear_history`, which moves the screen's used rows into history
  first. 20x5 after `l0..l9`: `ESC[2J` or `ESC[H ESC[J` (what bash/zsh Ctrl-L
  sends) gives `#{history_size}` 10 and capture `-S - -E -` `l0..l9` then blank
  rows (history was 6, the four used screen rows join it).
- vtmux (Ghostty): clears the screen in place, `history_size` stays 6 and the
  four screen rows are lost.
- Visible: `#{history_size}`, `capture-pane -S`, copy mode after every Ctrl-L.
  `ESC[3J` (ncurses `clear`) already matches.

## P7. RIS (`ESC c`) drops history and leaves the alternate screen

- Golden: `cap_ris`, `fmt_ris`, `cap_ris_alt`, `fmt_ris_alt` (follow-up
  `history_parity`)
- tmux: `INPUT_ESC_RIS` (input.c:1407-1412) calls `screen_write_reset`
  (screen-write.c:423-439), not `screen_reinit` (screen.c:109, which only
  `screen_init`/spawn.c:329 reach). It resets `s->mode`, tabs and the scroll
  region, then `screen_write_clearscreen` (screen-write.c:2073), which with
  `scroll-on-clear` (default on) runs `grid_view_clear_history`: this is P6, the
  used rows join history. It does not call `screen_alternate_off`, so the
  alternate screen stays on. `ESC c` after `l0..l9` on 20x5 leaves
  `#{history_size}` 10. After `ESC[?1049h ESC c` the pane is still on the
  alternate screen: `#{alternate_on}` 1, DECRQM `?1049` 1, the primary history
  6 (re-probed against the reference 3.8).
- vtmux (Ghostty): a full reset drops scrollback (`history_size` 0), and
  returns to the primary screen (`alternate_on` 0, DECRQM `?1049` 2).
- Why not fixed with the mouse modes: both need RIS emulated outside Ghostty's
  `fullReset` (keep screen and history, and P6's scroll-on-clear); the mouse
  bits already match.
- Visible: `#{history_size}`, `#{alternate_on}`, capture and copy mode after
  `reset` / `tput reset`.

## P8. Selective erase spares DEC-graphics cells

- Golden: none (no regress or oracle case uses DECSED/DECSEL).
- tmux: has no `CSI ? J` / `CSI ? K` entry (input.c `input_csi_table`), so
  selective erase does nothing; DECSCA (`CSI " q`), SPA and EPA are ignored.
- vtmux: Ghostty keeps no charset per cell, so `PaneTerm` carries tmux's
  `GRID_ATTR_CHARSET` in the cell's DECSCA protection bit (term.rs `Charset`,
  needed by `tty_draw_line_get_empty`). Ghostty runs DECSED/DECSEL, which
  erase the cells printed without `ESC ( 0` and keep the ones printed with it.
  ED/EL/ECH are unaffected: the most recent protection mode is always DEC.
- Visible: a program that sends `CSI ? J`/`CSI ? K` (rare; tmux drops them).

## P9. capture-pane -e and TABs: trailing styled blank, DECLRMM

- From review_d_tab_capture (rv-d_tab_capture). (1) `printf 'a\t\033[41m \033[0m'`, `capture-pane -p -e`: tmux emits `a\t` ESC[41m, a space, ESC[49m; vtmux emits `a\t` ESC[41m with no blank. With two red spaces tmux emits no blank and vtmux one (3-6 match). (2) After DECLRMM (`?69h`, `3;10s`), a TAB is kept as spaces: vtmux's tab marking bails when left/right margins are set (patch 0002); tmux has no DECLRMM, so its cursor stop differs anyway.
- Visible: `capture-pane -e` bytes after a TAB followed by one or two styled blanks; capture after DECLRMM. Rare.

## P10. OSC 133;A starts a fresh line

- Golden: none; regress `input-modes.sh` (scene osc133) stays xfail on it.
- tmux: `OSC 133;A` only flags the cursor's line `START_PROMPT` and records
  the column (input.c `input_osc_133`): `printf 'xx\033]133;A\007p>'` gives
  one line `xxp>`.
- vtmux (Ghostty): `OSC 133;A` is `fresh_line_new_prompt` (Terminal.zig
  `semanticPrompt`): with the cursor past column 0 it moves to a new line
  first, so the capture is `xx` then `p>`, and the line data `capture-pane -R`
  prints (`osc133=` and the flags) lands on the second line.
- Visible: any prompt that sends `OSC 133;A` without a newline before it.
