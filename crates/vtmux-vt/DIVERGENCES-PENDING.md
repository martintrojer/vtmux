# vtmux-vt: pending divergences

Entries for `compat/DIVERGENCES.md`, which lands with the regress merge. Move
them there (and assign D-NNN numbers) when both are on main. Category for all of
them: **Deferred** (not done yet; must not be claimed as compatible). Each has an
ignored golden in `tests/golden.rs`; un-ignore it when the entry is closed.
Evidence is the reference tmux 3.8 run recorded in the golden's `command=` line
and the probes from `review_vt_wrap`.

## P1. Scroll regions that do not start at row 0 do not feed history: moved to D-032

- Golden: `cap_region_scroll_history`, `fmt_region_scroll_history_size`
  (ignored); regress `input-raw-scroll.sh`. See `compat/DIVERGENCES.md` D-032.

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

## P3. Grapheme clusters: closed (`gap_emulator_triage`)

- vtmux runs Ghostty with grapheme clustering (mode 2027, `term.rs`
  `keep_clustering`, set again after RIS and `CSI ? 2027 l`, which tmux does
  not have): ZWJ sequences, regional-indicator pairs, emoji modifiers and
  VS16 after an emoji make one cell as `screen_write_combine` does. Golden
  `fmt_zwj_cursor_x`; regress `combine-test.sh`; cli
  `program_output_reaches_the_grid_as_in_tmux`. Ghostty's clustering drops a
  zero-width character that starts a grapheme (U+200B, U+2028); patch
  `0008-cluster-keeps-zero-width.patch` attaches it as before. What is left
  (a lone regional indicator, VS16 after a non-emoji) is D-031.

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

## P6. Scroll-on-clear: closed (`gap_emulator_triage`, `history_parity`)

- ED 2, and ED 0 at the top left, scroll the used rows into history under the
  pane's `scroll-on-clear` (`PaneTerm::clear_into_history`: the scanner keeps
  the final byte from Ghostty, then `CSI 22 J`, the cursor put back, `CSI 2 J`).
  Goldens `cap_scroll_on_clear_ed2`, `fmt_scroll_on_clear_ed2`,
  `cap_scroll_on_clear_ctrl_l`, `fmt_scroll_on_clear_ctrl_l`; regress
  `input-raw-history.sh`; cli `program_output_reaches_the_grid_as_in_tmux`.
  Ghostty's own ED 22 and its prompt-aware ED 2 never run for a program
  (tmux ignores ED 22).

## P7. RIS: closed (`gap_emulator_triage`, D-025)

- A program's RIS is tmux's `screen_write_reset` (`PaneTerm::reset_screen`,
  shared with `send-keys -R`). Goldens `cap_ris`, `fmt_ris`, `cap_ris_alt`,
  `fmt_ris_alt`.

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

## P10. OSC 133: closed (`gap_emulator_triage`)

- The scanner keeps every OSC 133 from Ghostty (CAN instead of its end), so
  `133;A` only marks the line, as `input_osc_133` does, and Ghostty's prompt
  rows (cleared on resize, ED 2 at a prompt) never exist. Regress
  `input-modes.sh`; unit `oscs_tmux_reads_differently`.
