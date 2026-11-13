# Known issues and ideas

What is known to differ, be rough, or be worth doing next. Until 3.8 final only fixes land; ideas wait until after it. Intended differences from tmux, each with its evidence, are in [compat/DIVERGENCES.md](../compat/DIVERGENCES.md).

## Known issues

| Area | What happens | tmux 3.8 |
|---|---|---|
| Two clients | With two clients on a window of four agent panes, one client can show blank pane rows until `refresh-client`. Seen on macOS; not reproduced on demand. | draws every row |
| Delete characters | `CSI P` on a soft-wrapped line unwraps it, so `capture-pane -J`, resizes and copy mode see two lines. | keeps the wrap flag |
| Erase over half a wide character | `ab中c` then `ESC[4D ESC[2X` shows `a   c`. | `a  c` |
| `capture-pane -a -N` | A line erased by `ESC[1K` (erase to start) comes out empty. | the erased cells as spaces |
| Copy mode, wide characters | A reflowed line of wide characters splits 20/20. | 18/20/2 |
| `vtmux-upgrade` onto a hung binary | A `-V` script's own children outlive the kill (only the direct child is killed); while it hangs, another child's exit can be noticed up to 5 s late. | n/a |
| Upgrade signals, rare | During the client's re-exec onto an older build (3.8-beta3), a late `SIGTERM` or `SIGHUP` ends the client by its default action instead of `[terminated]`; the terminal is left cooked. If that exec fails, `SIGTERM` and `SIGHUP` stay at their default action until the client exits with its error. | n/a |
| `#{client_user}` | Looked up on every expansion; slow with LDAP or sssd users. | cached per client |

## Ideas

- **`#{pane_program_state}`:** a format from OSC 7501 (a pane program's own status: running, waiting, done) so any status line can show it. Opt-in, like the other `vtmux-*` extensions.
- **OSC 7501 to the outer terminal:** waits until a terminal vtmux runs inside supports it.
- **First redraw with the restart frame:** after a live upgrade, send the client's first redraw in the same write as the restart, as tmux's `MSG_WAKEUP` does (`tty_start_tty` and `server_redraw_client` in one buffer).
