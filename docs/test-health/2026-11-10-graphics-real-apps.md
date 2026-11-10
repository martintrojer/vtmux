# Kitty graphics with real programs (D33), 2026-11-10

`vtmux-graphics` checked end to end: a vtmux server (debug build of the
D33 branch, `-L` private socket, `TMUX_TMPDIR` private, `-f /dev/null`),
an attached `vtmux attach` client in a pty whose terminal answers
XTVERSION as `ghostty 1.2.3`, TERM `xterm-256color`, COLORTERM
`truecolor`. Host: the shared Linux devserver (CentOS 9, 96 cpus). The
client's bytes are counted; the pane's own output comes from `pipe-pane
-O`. chafa, timg, `kitten icat` and image.nvim are not on this host; yazi
26.9.1 is.

## A program that sends bare kitty graphics

The emitter (below): a query (`a=q`), a direct placement (`a=T`, 4x4 RGB
over 4x2 cells), a virtual placement (`a=T,U=1`, 3x1 cells) and its own
three placeholder cells, then DSR 5, and prints the replies it read.

| | off (default) | on |
|---|---|---|
| pane title | the last APC's text (`Ga=T,U=1,...`), as tmux | unchanged |
| replies the program read | `ESC[0n` only, as tmux | `_Gi=31;OK`, `_Gi=1;OK`, then `ESC[0n` |
| `capture-pane` | the placeholder characters as written | the same: vtmux's cells exist only in frames |
| client: range free (`a=d,d=R`) | no | yes, once |
| client: uploads (`a=t`) / placements (`a=p,U=1`) | 0 / 0 | 2 / 2 |
| client: placeholder cells | 3 (the program's, as text, as tmux writes them) | 11 (8 for the direct placement, 3 the program's, with vtmux's ids) |
| bytes to the client | 783 | 1228 |

## yazi 26.9.1

yazi opened on a PNG in a pane, same harness. With the option off and on
alike it sets `allow-passthrough all` on its own pane (`tmux set -p`,
through the shim) and sends every graphics command wrapped for tmux
(`DCS tmux;`): 4 wrapped commands, no bare ones. So D33 changes nothing
for it: its images go by passthrough as under tmux, with passthrough's
problems (popups, pane edges). This is D28 rule 3 at work (`TMUX` and
XTVERSION stay tmux's); a program that sends bare commands is what D33
draws.

## Off costs nothing

`scripts/bench.py throughput throughput_attached render`, release builds:
`base` is main when measured (0f38490), `vtmux` the D33 branch on it
up to `graphics: the grant across a live upgrade, images dropped`
(b024071 since the branch was rebased onto a later main), with
`vtmux-graphics` at its default. `NO_BUILD=1
THROUGHPUT_MB=50`, under `flock /tmp/vtmux-stress.lock`, load 16-20 (the
host is shared), so a few percent is noise. Server CPU, median:

| | base | D33, off | runs |
|---|---|---|---|
| 50 MB plain, no client | 0.46 s | 0.46 s | 7 |
| 50 MB 19% SGR, no client | 0.82 s | 0.84 s | 7 |
| one client, plain, sync off / on | 0.63 / 0.63 s | 0.64 / 0.64 s | 3 |
| one client, SGR, sync off / on | 0.96 / 0.96 s | 1.05 / 1.01 s | 3 |
| `render`, sync off / on | 0.25 / 0.23 s | 0.24 / 0.26 s | 3 |

Peak RSS is 0.1-0.4 MB higher (the PNG and zlib code). Off, a pane read
does one option lookup more, and a frame nothing: no pane was ever
granted, so no turn reads placements and no client is a graphics client.

## The emitter

```python
#!/usr/bin/env python3
"""A kitty graphics program: a direct placement (a=T) and a virtual one
with its own placeholder cells (U=1), bare commands (no tmux wrapping),
then the replies it read."""
import base64, os, sys, termios, tty, select
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)
px = base64.b64encode(bytes([255, 0, 0] * 16)).decode()  # 4x4 red
w = sys.stdout.write
w(f"\x1b_Ga=q,i=31,s=1,v=1,f=24;AAAA\x1b\\")
w(f"\x1b_Ga=T,f=24,s=4,v=4,i=1,c=4,r=2;{px}\x1b\\\r\n\r\n")
w(f"\x1b_Ga=T,U=1,f=24,s=4,v=4,i=2,c=3,r=1,q=2;{px}\x1b\\")
w("\x1b[38;5;2m\U0010EEEE\u0305\u0305\U0010EEEE\U0010EEEE\x1b[m\r\n")
w("\x1b[5n")
sys.stdout.flush()
got = b""
while not got.endswith(b"\x1b[0n"):
    r, _, _ = select.select([fd], [], [], 5)
    if not r:
        break
    got += os.read(fd, 1024)
termios.tcsetattr(fd, termios.TCSADRAIN, old)
print("replies:", repr(got))
sys.stdout.flush()
os.system("sleep 30")
```
