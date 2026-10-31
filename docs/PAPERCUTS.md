# Paper cuts

Long-standing tmux usability defects that vtmux fixes without breaking compliance. The rules are in the spec, *Paper-cut fixes*: fix a defect in existing behaviour, keep compliance (opt-in, or invisible to scripts), cite the tmux source, and pin it with a test.

| Paper cut | tmux reference | vtmux fix | Option | Status |
|---|---|---|---|---|
| Active pane is hard to see: shared borders are half-coloured, arrows are one cell | `pane-border-indicators`, `screen-redraw.c` (`style_wp`, `redraw_mark_border_arrows`) | Focus ring: outer frame, active pane enclosed by heavy lines in the active colour | `vtmux-focus-ring` (D23) | done (M1); `vtmux-focus-ring-lines` deferred: the first cut always draws the active rectangle heavy and every other border light |
| Cursor flickers under heavy pane output: it jumps around the screen as updates are written | tmux forwards each pane update to the terminal as it happens (`tty.c` `tty_cmd_*`, cursor moved per write) and only brackets them in synchronized output (`?2026`, `tty_sync_start/end`) in some paths (`tty_cmd_syncstart`, `screen-redraw.c:1772`) | The compositor (D14) builds one diffed frame per client tick and writes it in a single burst: cursor hidden at frame start, placed once at the end, the whole frame inside `?2026` when the terminal supports it | none (rendering-only, invisible to scripts) | planned (M1, client compositor) |
| Popup borders flicker when panes under them scroll | With an overlay (popup/menu) up, tmux can't use terminal scroll/insert/delete (`c->overlay_check != NULL` forces `tty_draw_pane` in `tty_cmd_insertcharacter`, `tty_cmd_deletecharacter`, scroll paths), so it redraws the pane lines, overwriting the popup, then redraws the overlay | Popups are composited into the same frame as the panes, so a cell under a popup is never written: the terminal only ever sees the final picture | none (rendering-only) | planned (M1, popups) |

## How vtmux options stay invisible

vtmux options live in their own table (`crates/vtmux-core/src/options/vtmux.rs`), not tmux's generated one. They match by exact name only, so tmux's prefix errors (`set -g vt on` → `invalid option: vt`) don't change. A tree holds one only once it is set there: listings (`show -g`, `-gw`, `-A`, …) and `#{name}` formats see it only then, and a named `show` prints the default. `crates/vtmux/tests/cli.rs` `vtmux_options_invisible_while_unset` compares the off state with tmux 3.8.

## Candidates

Add a row here with its tmux reference before work starts. Each candidate gets a short spec of its own when it's picked up.
