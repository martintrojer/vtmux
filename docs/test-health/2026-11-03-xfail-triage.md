# xfail triage, 2026-11-03

Every `xfail` in `compat/expected.toml` now carries the concrete blocker in `reason =`, replacing "not implemented (M0 baseline)" on 86 tests. No test changed status and no code changed.

## Method

`cargo xtask regress` ran against vtmux (`target/debug/vtmux`) once over the whole suite (4 cores, under muzzle), keeping each script's `sh -x` log. The first vtmux error line (`not implemented in vtmux yet: X`, `not supported in vtmux yet: X`, `unknown command`) or the first failing assertion gave the blocker. Where the script only does `cmp -s` or hides stderr (cursor-test*, combine-test, utf8-test, capture-pane-hyperlink, screen-redraw-*), the script was re-run from a scratch copy with `diff` in place of `cmp`, or the format/command was probed by hand against `target/debug/vtmux`. Spot checks against the reference tmux 3.8 (am-terminal, border-arrows, capture-pane-hyperlink, cursor-test1) pass. All 98 tests fail with exit status 1 except `control-client-wait-exit` (SIGPIPE, 141). Each run was one pass: a test that stays red after its first blocker lands will show a second one. `screen-redraw-menus` and `terminal-feature-utf8` are flaky.

## Buckets (by first blocker)

| bucket | count | tests |
|---|---:|---|
| missing command | 22 | clear-on-attach, confirm-before-lock, customize-mode, display-panes, format-render-contexts, join-pane-modal, layout-set, menu-mouse, modal-pane, mode-mutation, new-pane-mouse, pipe-pane, screen-redraw-fill-character, screen-redraw-floating, screen-redraw-indicators, screen-redraw-menus, server-access, swap-pane-zoom-error, switch-mode, sync-output-atomic, sync-output-copy-cursor, sync-output-cursor |
| missing flag | 33 | border-arrows, capture-pane-time, control-subscriptions, copy-mode-redraw, input-cursor, input-edit, input-keys, input-malformed, input-modes, input-osc, input-raw-controls, input-raw-cursor, input-raw-edit, input-raw-history, input-raw-reflow, input-raw-scroll, input-raw-sgr, input-raw-unicode, input-raw-wrap, input-reflow-stress, input-scroll, input-sgr, input-unicode, json, lifecycle-deferred, screen-redraw-outside, set-hook-B, show-options-output, split-window-wait-kill, style-trim, tty-clamp-area-status-top, tty-draw-line, wait-for-E |
| missing feature | 13 | copy-mode-no-client-scroll, decrqm-sync, format-variables, hooks-lifecycle, hooks-notify, input-replies, pane-ops, session-group-resize, session-ops, targets-panes, utf8-test, wait-for-client-lost, window-ops |
| missing format/option | 15 | am-terminal, codepoint-widths, cursor-test1, cursor-test2, cursor-test3, cursor-test4, format-animation, format-modifiers, format-mouse, format-no-current-window, format-time-range, screen-redraw-scrollbars-auto-hide, screen-redraw-scrollbars, screen-redraw-window-style, terminal-feature-utf8 |
| behaviour diff | 11 | capture-pane-hyperlink, check-names, combine-test, command-alias, control-client-exit-stalled, control-client-wait-exit, copy-mode-unicode-whitespace, layout-custom, prompt-keys, screen-redraw-bidi, screen-redraw-cache |
| terminal request/reply | 3 | input-requests, osc-11colours, theme-report |
| mouse | 1 | new-pane-mouse-status |
| **total** | **98** | |

Notes on the buckets:

- *missing feature* is a whole mode, behaviour or server facility rather than a command flag: remain-on-exit (dead panes), session groups, marked pane, the `-vv` server log.
- *missing format/option* includes options that vtmux accepts and ignores (`window-style`, `pane-scrollbars`, `codepoint-widths`, `terminal-overrides am@`) as well as missing format variables and modifiers.
- *test infrastructure*: none. The scripts that run an inner client inside an outer pane (the `screen-redraw-*` family, `tty-draw-line`, `format-animation`) work through attach and `send-keys`; they are blocked on the commands and options they exercise. `pipe-pane` is a missing command, not an infrastructure gap. Only `clear-on-attach`, `sync-output-*` and `pipe-pane` itself use it.
- *timing*: none is the primary blocker. `screen-redraw-menus` and `terminal-feature-utf8` are `flaky = true`; their reason is the real blocker.

## Top 10 blockers by tests unblocked

"needs" counts every test whose script uses the feature (found by scanning the scripts as well as the first error), so it is the upper bound on what the feature can unblock. "first" counts tests where it is the first failure. "only" counts tests where it is the only blocker seen, which will probably go green when it lands.

| # | blocker | needs | first | only | tests |
|---:|---|---:|---:|---:|---|
| 1 | `capture-pane -N` (keep trailing spaces) | 15 | 14 | 12 | input-cursor, input-edit, input-malformed, input-modes, input-osc, input-raw-controls, input-raw-cursor, input-raw-edit, input-raw-unicode, input-raw-wrap, input-reflow-stress, input-scroll, input-sgr, input-unicode, tty-draw-line |
| 2 | `new-pane` (floating / modal panes, M2) | 14 | 9 | 11 | display-panes, format-modifiers, join-pane-modal, layout-custom, layout-set, modal-pane, new-pane-mouse-status, new-pane-mouse, pane-ops, screen-redraw-fill-character, screen-redraw-floating, screen-redraw-indicators, screen-redraw-scrollbars, swap-pane-zoom-error |
| 3 | remain-on-exit (dead panes, `pane-died`) | 11 | 5 | 6 | copy-mode-no-client-scroll, decrqm-sync, hooks-lifecycle, hooks-notify, input-replies, modal-pane, osc-11colours, pane-ops, pipe-pane, theme-report, utf8-test |
| 4 | `capture-pane -R` (raw / history capture) | 5 | 4 | 4 | input-raw-history, input-raw-reflow, input-raw-scroll, input-raw-sgr, input-reflow-stress |
| 5 | `pipe-pane` | 5 | 5 | 4 | clear-on-attach, pipe-pane, sync-output-atomic, sync-output-copy-cursor, sync-output-cursor |
| 6 | session groups (`new-session -t`) | 5 | 5 | 4 | format-variables, hooks-notify, session-group-resize, session-ops, window-ops |
| 7 | `customize-mode` | 4 | 3 | 3 | customize-mode, format-render-contexts, modal-pane, mode-mutation |
| 8 | marked pane (`select-pane -m`, `~` target) | 4 | 2 | 1 | hooks-notify, pane-ops, screen-redraw-indicators, targets-panes |
| 9 | `set-hook -B` (monitor hooks) | 3 | 3 | 3 | set-hook-B, show-options-output, wait-for-E |
| 10 | `capture-pane -C` (escape output with colours) | 3 | 2 | 2 | border-arrows, style-trim, utf8-test |

Runners-up (each unblocks 1-2): capture-pane -L/-F/-I (2), display-menu (2), refresh-client -B (2), refresh-client -U (2), refresh-client -c (2), capture-pane -H/-M (2), display-message -j (1), lock-* (1), send-keys -R (1), server-access (1), split-window -W (1), switch-mode (1).

## Single-test blockers outside the ranking

Tests whose first blocker is neither a command/flag from the table above nor shared with another test:

| test | bucket | blocker |
|---|---|---|
| am-terminal | missing format/option | terminal-overrides `*:am@` (no auto-margin) not honoured by the client tty: last status line is `LLLWWW           RRR`, expected `LLLWWW           RR` (the inner client still draws the last column) |
| capture-pane-hyperlink | behaviour diff | capture-pane -e drops OSC 8 hyperlinks: captured `test1`, expected the same text wrapped in OSC 8 (`id=1;https://github.com`) open and close sequences |
| check-names | behaviour diff | the screen-style window-name escape (ESC k name, string terminator) does not rename the window: got `format:.#(ok)`, expected `escape#:.ok`; every name/title check before it passes |
| codepoint-widths | missing format/option | option codepoint-widths is accepted but not applied: `#{w:@text}` with U+03B1-U+03B3=2 is 4, expected 7 |
| combine-test | behaviour diff | terminal-emulator diff (libghostty): line 7 (two emoji + skin-tone modifiers at columns 3/12) has the second cell 2 columns narrower than tmux; every other row matches (`👍🏻     👍🏻4` vs `👍🏻       👍🏻4`) |
| command-alias | behaviour diff | command-alias set with `set -s command-alias[100] zoom=...` is not applied: `unknown command: zoom` |
| control-client-exit-stalled | behaviour diff | server does not exit after kill-server while a control client is stalled: `server did not exit after kill-server; new clients see: server exited unexpectedly` (needs the bounded exit handshake, 10 s) |
| control-client-wait-exit | behaviour diff | control client flag wait-exit is stored but not honoured: the client exits at %exit instead of waiting for an empty line (writer gets SIGPIPE, exit 141) |
| copy-mode-unicode-whitespace | behaviour diff | D-011: Ghostty prints U+0085 as a cell of its own where tmux combines it (wcwidth -1, C1, width 0) into the cell before, so next-word stops one column later (3,17 vs 2,17); every other assertion passes |
| cursor-test1 | missing format/option | missing format variable cursor_character (empty, e.g. `14 8 ` vs `14 8 t`); tests 1, 2 and 4 also show capture differences after `resizew` (reflow of a manual-size window) that cannot be judged until the variable exists |
| cursor-test2 | missing format/option | missing format variable cursor_character (empty, e.g. `14 8 ` vs `14 8 t`); tests 1, 2 and 4 also show capture differences after `resizew` (reflow of a manual-size window) that cannot be judged until the variable exists |
| cursor-test3 | missing format/option | missing format variable cursor_character (empty, e.g. `14 8 ` vs `14 8 t`); tests 1, 2 and 4 also show capture differences after `resizew` (reflow of a manual-size window) that cannot be judged until the variable exists |
| cursor-test4 | missing format/option | missing format variable cursor_character (empty, e.g. `14 8 ` vs `14 8 t`); tests 1, 2 and 4 also show capture differences after `resizew` (reflow of a manual-size window) that cannot be judged until the variable exists |
| format-animation | missing format/option | missing format modifier A (format_cycle): `#{A/7:AAAA,BBBB,CCCC}` expands to empty, the status line stays blank (`status line is '', not a frame`) |
| format-modifiers | missing format/option | format modifier N/s is wrong: `#{N/s:main}` gives 0, expected 1 (session-exists test); later the script uses new-pane |
| format-mouse | missing format/option | missing format variable mouse_word/mouse_line: `x=2 y=0 word= line= pane=%0` (mouse_x, mouse_y and mouse_pane work) |
| format-no-current-window | missing format/option | session-level formats read empty without a current client: `#{active_window_index}` is `` (expected 0); also last_window_index, session_stack, window_active_clients(_list) |
| format-time-range | missing format/option | format modifier t/f with an out-of-range time: `#{t/f/zz:@huge}` gives `[]`, tmux prints the format literally `[zz]`; the in-range cases pass |
| input-requests | terminal request/reply | terminal requests from a pane are not seen on the client output: `did not see terminal request` for the OSC 4 colour query (ESC ] 4;99;?); the script also respawns a remain-on-exit window |
| layout-custom | behaviour diff | a control client that has not asked for new-layouts is dumped the v2 JSON layout: got `{\"V\":2,\"L\":{...}}`, expected the v1 string `020a,80x24,0,0{40x24,0,0,1,39x24,41,0,2}` |
| new-pane-mouse-status | mouse | Ctrl-drag from the status line: the `C-MouseDragEnd1Pane` binding is not dispatched, `got @drag-end '', expected 'done'` (no new-pane error; the script's new-pane use comes after) |
| osc-11colours | terminal request/reply | OSC 11 (set background) is not tracked: `#{pane_bg}` is empty after a pane prints OSC 11 `rgb:ff/ff/ff`, expected `#ffffff` |
| prompt-keys | behaviour diff | a 24-row client of an 80x24 window (window-size manual) needs the pan offset to the mode prompt (tty_window_offset); with that, a status-prompt invalid-UTF-8 append |
| screen-redraw-bidi | behaviour diff | attach-session without -t switches to window 0 where tmux keeps the session's current window (the script's `new_scene` then `respawn-pane -k 'tmux attach'` shows BIDI-BASE not BIDI-LEFT); then needs the Bidi terminal override (iso... |
| screen-redraw-cache | behaviour diff | scene after `rotate-window` differs (`scene cache-rotate-after differs`: pane columns/stale cells differ from the golden); earlier scenes of the script pass |
| screen-redraw-scrollbars-auto-hide | missing format/option | pane-scrollbars auto-hide not implemented (no scrollbar overlay): `scrollbar did not hide after timeout` (captures never differ) |
| screen-redraw-scrollbars | missing format/option | pane-scrollbars is accepted and ignored: first scene has no scrollbar column (`scene scrollbar-right-w1 differs`) |
| screen-redraw-window-style | missing format/option | window-style / window-active-style do not show: the outer `capturep -pe` has no SGR where tmux has a blue background (`scene window-style-single differs`) |
| terminal-feature-utf8 | missing format/option | `-T utf8` (terminal feature) does not set client_utf8 for a client run with LC_ALL=C: `utf8 terminal feature did not set client_utf8` (flaky) |
| theme-report | terminal request/reply | DSR 996 / mode 2031 theme report: a pane's DSR 996 query gets no answer (`expected ^[[?997;2n (light, from terminal), got ''`) |
| wait-for-client-lost | missing feature | server log (`-vv` writes tmux-server-PID.log in cwd) is not written: `awk: cannot open .../tmux-server-*.log`; the test greps it for wait-channel debug lines |

## Full list

| test | bucket | first blocker |
|---|---|---|
| am-terminal | missing format/option | terminal-overrides am@ |
| border-arrows | missing flag | capture-pane -C |
| capture-pane-hyperlink | behaviour diff | capture -e hyperlinks |
| capture-pane-time | missing flag | capture-pane -L |
| check-names | behaviour diff | escape-k window name |
| clear-on-attach | missing command | pipe-pane |
| codepoint-widths | missing format/option | codepoint-widths |
| combine-test | behaviour diff | emulator width/grapheme |
| command-alias | behaviour diff | command-alias |
| confirm-before-lock | missing command | lock-client/session/server |
| control-client-exit-stalled | behaviour diff | exit handshake |
| control-client-wait-exit | behaviour diff | wait-exit |
| control-subscriptions | missing flag | refresh-client -B |
| copy-mode-no-client-scroll | missing feature | remain-on-exit |
| copy-mode-redraw | missing flag | refresh-client -c |
| copy-mode-unicode-whitespace | behaviour diff | D-011 U+0085 width |
| cursor-test1 | missing format/option | cursor_character |
| cursor-test2 | missing format/option | cursor_character |
| cursor-test3 | missing format/option | cursor_character |
| cursor-test4 | missing format/option | cursor_character |
| customize-mode | missing command | customize-mode |
| decrqm-sync | missing feature | remain-on-exit |
| display-panes | missing command | new-pane |
| format-animation | missing format/option | A modifier |
| format-modifiers | missing format/option | N/s |
| format-mouse | missing format/option | mouse_word |
| format-no-current-window | missing format/option | session formats |
| format-render-contexts | missing command | customize-mode |
| format-time-range | missing format/option | t/f out of range |
| format-variables | missing feature | session groups |
| hooks-lifecycle | missing feature | remain-on-exit |
| hooks-notify | missing feature | session groups |
| input-cursor | missing flag | capture-pane -N |
| input-edit | missing flag | capture-pane -N |
| input-keys | missing flag | send-keys -R |
| input-malformed | missing flag | capture-pane -N |
| input-modes | missing flag | capture-pane -N |
| input-osc | missing flag | capture-pane -N |
| input-raw-controls | missing flag | capture-pane -N |
| input-raw-cursor | missing flag | capture-pane -N |
| input-raw-edit | missing flag | capture-pane -N |
| input-raw-history | missing flag | capture-pane -R |
| input-raw-reflow | missing flag | capture-pane -R |
| input-raw-scroll | missing flag | capture-pane -R |
| input-raw-sgr | missing flag | capture-pane -R |
| input-raw-unicode | missing flag | capture-pane -N |
| input-raw-wrap | missing flag | capture-pane -N |
| input-reflow-stress | missing flag | capture-pane -N |
| input-replies | missing feature | remain-on-exit |
| input-requests | terminal request/reply | terminal requests |
| input-scroll | missing flag | capture-pane -N |
| input-sgr | missing flag | capture-pane -N |
| input-unicode | missing flag | capture-pane -N |
| join-pane-modal | missing command | new-pane |
| json | missing flag | display-message -j |
| layout-custom | behaviour diff | layout v1 dump |
| layout-set | missing command | new-pane |
| lifecycle-deferred | missing flag | refresh-client -B |
| menu-mouse | missing command | display-menu |
| modal-pane | missing command | new-pane |
| mode-mutation | missing command | customize-mode |
| new-pane-mouse-status | mouse | C-MouseDragEnd1Pane |
| new-pane-mouse | missing command | new-pane |
| osc-11colours | terminal request/reply | OSC 11 / pane_bg |
| pane-ops | missing feature | marked pane |
| pipe-pane | missing command | pipe-pane |
| prompt-keys | behaviour diff | manual-size client pan offset |
| screen-redraw-bidi | behaviour diff | attach resets window |
| screen-redraw-cache | behaviour diff | redraw after rotate-window |
| screen-redraw-fill-character | missing command | new-pane |
| screen-redraw-floating | missing command | new-pane |
| screen-redraw-indicators | missing command | new-pane |
| screen-redraw-menus | missing command | display-menu |
| screen-redraw-outside | missing flag | refresh-client -U |
| screen-redraw-scrollbars-auto-hide | missing format/option | pane-scrollbars |
| screen-redraw-scrollbars | missing format/option | pane-scrollbars |
| screen-redraw-window-style | missing format/option | window-style |
| server-access | missing command | server-access |
| session-group-resize | missing feature | session groups |
| session-ops | missing feature | session groups |
| set-hook-B | missing flag | set-hook -B |
| show-options-output | missing flag | set-hook -B |
| split-window-wait-kill | missing flag | split-window -W |
| style-trim | missing flag | capture-pane -C |
| swap-pane-zoom-error | missing command | new-pane |
| switch-mode | missing command | switch-mode |
| sync-output-atomic | missing command | pipe-pane |
| sync-output-copy-cursor | missing command | pipe-pane |
| sync-output-cursor | missing command | pipe-pane |
| targets-panes | missing feature | marked pane |
| terminal-feature-utf8 | missing format/option | -T utf8 |
| theme-report | terminal request/reply | DSR 996 theme |
| tty-clamp-area-status-top | missing flag | refresh-client -U |
| tty-draw-line | missing flag | refresh-client -c |
| utf8-test | missing feature | remain-on-exit |
| wait-for-E | missing flag | set-hook -B |
| wait-for-client-lost | missing feature | -vv log file |
| window-ops | missing feature | session groups |
