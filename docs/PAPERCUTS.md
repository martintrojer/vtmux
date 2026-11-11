# Paper cuts

Long-standing tmux annoyances vtmux fixes without breaking compliance: each fix is opt-in, or invisible to scripts.

| Paper cut in tmux | tmux reference | vtmux | Turn it on |
|---|---|---|---|
| The active pane is easy to miss: a half-coloured border and one arrow cell, and `window-style` misses cells a program colours itself | `pane-border-indicators`; `tty_default_colours` (tty.c) | Pane blend: every cell of inactive panes blends toward a colour, like Ghostty's unfocused splits, at no cell cost | `set -g vtmux-pane-inactive-blend 'fg=bg/25,bg=bg/25'` |
| The cursor jumps around the screen under heavy output | each pane update written as it comes (tty.c `tty_cmd_*`) | One finished frame per client, inside synchronized output, the cursor placed once | always on |
| Big scrollback costs so much RAM that people keep `history-limit` low | tmux #3352, #3194, #4859 | Quiet history is compressed: 1M lines settle at 103 MB against tmux's 622 MB | always on |
| Upgrading kills every session | tmux FAQ, "protocol version mismatch" (client.c `client_dispatch_wait`) | Live upgrade: the server moves onto the new binary, panes and clients stay | `tmux vtmux-upgrade -b <binary>` |
| A crash or reboot loses every session; tmux-resurrect brings back the layout and a guess at the commands | the same FAQ; tmux-resurrect | Restore with screens and scrollback; Enter restarts a pane below its old screen, or every shell restarts at once | `set -s vtmux-persist full` |

Terminal protocols tmux 3.8 drops (kitty keys, notifications, in-band resize, kitty graphics) are listed with the options below.

## vtmux options

All are off until set (`vtmux-persist-respawn` acts only on restored panes) and invisible to scripts while unset. Guard them with `%if "#{vtmux_version}"` so the same `.tmux.conf` loads in real tmux:

```tmux
%if "#{vtmux_version}"
set -g vtmux-pane-inactive-blend 'fg=bg/25,bg=bg/25'
set -s vtmux-persist full
%endif
```

| Option | Scope | Values | What it does |
|---|---|---|---|
| `vtmux-pane-inactive-blend` | window | `fg=<colour>[/<pct>],bg=<colour>[/<pct>]` | Blends every cell of the window's inactive panes toward the colours (`/pct` 0-100, default 100). `fg` and `bg` as colours are the outer terminal's defaults. `fg=bg/25,bg=bg/25` dims like Ghostty's `unfocused-split-opacity = 0.75`. |
| `vtmux-pane-active-blend` | window | as above | The same, for the active pane. |
| `vtmux-persist` | server | `off`, `layout`, `full` | Checkpoints the server to `$XDG_STATE_HOME/vtmux/` (default `~/.local/state`, private to you) on a clean exit and every 60 s while something changed. A server starting on that socket restores it before the config runs, every pane dead. `layout`: sessions, windows, layout, each pane's command and directory, options and key bindings. `full` adds screens, scrollback, titles and paste buffers. A checkpoint that can't be read leaves a fresh server and says why in `show-messages`. |
| `vtmux-persist-respawn` | server | `off`, `key` (default), `auto` | What restarts a restored pane. `key`: Enter typed in it, or sent as a client key by `send-keys -K` (not plain `send-keys`); a hint over its last row says so. `auto`: also every restored shell at once after a restart, once the config has run; panes that ran another command wait for Enter. `off`: only `respawn-pane`. A restarted pane keeps its old screen above a dim `restored <time>` row. |
| `vtmux-persist-drop` | session | globs | User options not restored, e.g. `set -g vtmux-persist-drop '@murmur_*'` so a tool's state for panes that are now dead doesn't come back. |
| `vtmux-kitty-keys` | server | `off`, `on` | A program that asks for the kitty keyboard protocol gets kitty-encoded keys, typed or sent with `send-keys`: Shift+Enter, C-i apart from Tab, Escape apart from Alt. Bindings and the prefix are read as before. |
| `vtmux-notifications` | server | `off`, `on` | A pane's desktop notifications (OSC 9, OSC 777) reach attached Ghostty clients, from any window and from `display-popup -E` programs, at Ghostty's own rate limits. No `allow-passthrough` needed. |
| `vtmux-inband-resize` | server | `off`, `on` | A program that sets mode 2048 gets each new size of its pane in-band, at once and after every resize, with no `SIGWINCH` race. |
| `vtmux-graphics` | server | `off`, `on` | Kitty graphics without `allow-passthrough`: a program's images drawn by vtmux for Ghostty clients, clipped at pane edges, under popups, zoomed, in copy mode and across window switches. Apps that wrap for tmux (yazi) keep using passthrough. A live upgrade or restore drops the images. |
| `vtmux-graphics-limit` | server | MB, default 256 | Image memory of every pane together, half-sent images included; over it, half-sent images of other panes go first, then the images loaded least recently. |

A grant (kitty keys, in-band resize, graphics) outlives the option being turned off, and a live upgrade keeps it. Kitty keys and graphics end with the program that asked; in-band resize at `?2048l`, a reset or a respawn.

`vtmux-upgrade [-b binary]` is a command, not an option: bind it like any other. Without `-b` it takes the path the server started from, or, when the client's protocol differs from the server's, the client's own binary.

## How vtmux options stay invisible

They live in their own table, not tmux's, and match by exact name only, so tmux's prefix errors (`set -g vt on` → `invalid option: vt`) are unchanged. `show` listings and `#{name}` formats see one only once it is set. A test compares the off state with tmux 3.8 (`vtmux_options_invisible_while_unset` in `crates/vtmux/tests/cli.rs`).

## Adding one

A new paper cut needs its tmux reference (issue, source file and function) and a test that pins the fix. Add a row above when it ships.
