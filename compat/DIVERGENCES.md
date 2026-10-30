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
