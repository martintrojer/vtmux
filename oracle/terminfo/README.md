# Terminfo fixtures

Compiled entries of ncurses 6.6's terminfo database (`wezterm`, `contour`, `foot`, `foot-direct`, `rio`, and the hardcopy `tty33`),
for the terminals older hosts' ncurses-term lacks. The oracle runner and `crates/vtmux/tests` put this directory
in `TERMINFO_DIRS` so the reference tmux finds them the same on every host. Each is in both layouts ncurses
uses (`w/wezterm`, and `77/wezterm` where the file system folds case). vtmux itself reads no terminfo
(D-007): `crates/vtmux-core/tests/terminfo_table.py` turns the full database into its table.

`xterm-ghostty` is Ghostty's own entry, the one vtmux's table carries: `src/terminfo/ghostty.zig` at the pinned
ghostty commit, written out as `Source.encode` does and compiled with `tic -x` (ncurses 6.2). Ghostty is MIT,
Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors: its licence is `LICENSE-ghostty` here, from the same commit.
