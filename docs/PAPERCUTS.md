# Paper cuts

Long-standing tmux usability defects that vtmux fixes without breaking compliance. The rules are in the spec, *Paper-cut fixes*: fix a defect in existing behaviour, keep compliance (opt-in, or invisible to scripts), cite the tmux source, and pin it with a test.

| Paper cut | tmux reference | vtmux fix | Option | Status |
|---|---|---|---|---|
| Active pane is hard to see: shared borders are half-coloured, arrows are one cell | `pane-border-indicators`, `screen-redraw.c` (`style_wp`, `redraw_mark_border_arrows`) | Focus ring: outer frame, active pane enclosed by heavy lines in the active colour | `vtmux-focus-ring` (D23) | planned (M1) |

## Candidates

Add a row here with its tmux reference before work starts. Each candidate gets a short spec of its own when it's picked up.
