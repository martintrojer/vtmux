# Kitty graphics with kitten icat and chafa (D33), 2026-11-11

What `kitten icat` and chafa send in a vtmux pane, and what the client
gets. Same harness as `2026-11-10-graphics-real-apps.md`: debug build of
main at 0a4bf52 plus `final_fix_misc`, private `-L`, `TMUX_TMPDIR` and
`XDG_STATE_HOME`, `-f /dev/null`, `vtmux-graphics on`; an attached client
in a pty answering XTVERSION as `ghostty 1.2.3` and every graphics query
`OK`; `pipe-pane -O` keeps what the program writes. Host: the shared Linux
devserver (CentOS 9). Programs from EPEL 9, unpacked with `rpm2cpio`:
`kitten` 0.29.2 (kitty-kitten), chafa 1.14.5. Image: a 16x16 red PNG.

| program | server's `argv[0]` | what the program writes | how it reaches the client |
|---|---|---|---|
| `kitten icat img.png` | `vtmux` | bare: three `a=q` queries, then `a=T,f=100` (a direct placement) | D33 draws it: an upload, a placement, placeholder cells, no `allow-passthrough` |
| `kitten icat img.png` | `tmux` (a link to vtmux) | wrapped (`DCS tmux;`), `U=1` with its own placeholder cells; it sets `allow-passthrough on` on its pane | passthrough, as under tmux 3.8 (same bytes) |
| `chafa -f kitty img.png` | either | wrapped (`DCS tmux;`), `U=1`, its own placeholder cells | passthrough, as under tmux 3.8 |
| `chafa img.png` | either | text (no kitty terminal detected from `TERM`) | text |

icat decides "tmux" from the server process's name, so a vtmux started as
`vtmux` gets its bare commands and D33's drawing; started as `tmux`, icat
wraps and D33 changes nothing for it. chafa wraps whenever `TMUX` is set
(D28 rule 3 keeps it). image.nvim and timg were not run (not installable
here): the README names neither.
