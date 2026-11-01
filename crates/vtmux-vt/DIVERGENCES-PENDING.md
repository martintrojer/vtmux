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

## P2. Narrowing reflow does not push rows into history

- Golden: `cap_reflow_narrow`, `fmt_reflow_narrow_history_size`,
  `fmt_reflow_narrow_cursor_y`, `fmt_reflow_pending_wrap_wide_cursor_x`,
  `fmt_reflow_pending_wrap_wide_cursor_y`,
  `fmt_reflow_pending_wrap_wide_history_size`
- tmux: `screen_reflow` / `grid_reflow` on resize pushes the top row into history
  when narrowing 20->10 with `hello world` on row 0 (`hello worl` goes to history,
  `#{history_size}` 1, `#{cursor_y}` 0, CPR `1;2R`).
- vtmux (Ghostty): both rows stay on screen (`history_size` 0, cursor row 1,
  CPR `2;2R`).
- Pending-wrap cursor: 20x5, `0123456789abcdefghij0123`, `short`, then ten
  wide `漢字` cells filling the row (wrap pending), resized to 7x5: tmux puts
  the cursor at 4,3 with `#{history_size}` 6. Ghostty 3bd54e025 gives 2,4
  with `history_size` 4 (ghostty a887df42c gave 7,4, past the last column).
  The ASCII case (20 chars at 20x5, resize 7x5) now matches tmux's
  `cursor_x` 6 (`fmt_reflow_pending_wrap_cursor_x`; a887df42c gave 7).
- Visible: `#{history_size}`, `#{cursor_x}`, `#{cursor_y}`, CPR and
  `capture-pane` after any narrowing resize with content in the top rows.

## P3. ZWJ sequences do not cluster

- Golden: `fmt_zwj_cursor_x`
- tmux: `screen_write_combine` (screen-write.c:2850) joins ZWJ sequences into one
  cell: `x` + family emoji + `y` leaves `#{cursor_x}` at 4.
- vtmux (Ghostty without mode 2027): does not cluster, `cursor_x` 8, which is what
  xterm does.
- Visible: `#{cursor_x}`, CPR and capture of emoji text.

## P4. Tab cells (existing, from vt_wrap; follow-up `d_tab_capture`)

- Golden: `cap_tab`
- tmux keeps a tab as a TAB cell and captures `\t`; Ghostty stores spaces.

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

## P7. RIS (`ESC c`) drops history

- Golden: `cap_ris`, `fmt_ris`, `cap_ris_alt`, `fmt_ris_alt` (follow-up
  `history_parity`)
- tmux: `screen_reinit` (screen.c:109-130) turns the alternate screen off and
  clears only the visible lines (`grid_clear_lines(hsize, sy)`), so history
  stays: `ESC c` after `l0..l9` leaves `#{history_size}` 10; under the alternate
  screen it leaves the primary 6.
- vtmux (Ghostty): a full reset drops scrollback, `history_size` 0.
- Visible: `#{history_size}`, capture and copy mode after `reset` / `tput reset`.

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
