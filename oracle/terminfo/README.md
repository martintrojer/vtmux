# Terminfo fixtures

Compiled entries of ncurses 6.6's terminfo database (`wezterm`, `contour`, `foot`, `foot-direct`, `rio`, and the hardcopy `tty33`),
for the terminals older hosts' ncurses-term lacks. The oracle runner and `crates/vtmux/tests` put this directory
in `TERMINFO_DIRS` so the reference tmux finds them the same on every host. Each is in both layouts ncurses
uses (`w/wezterm`, and `77/wezterm` where the file system folds case). vtmux itself reads no terminfo
(D-007): `crates/vtmux-core/tests/terminfo_table.py` turns the full database into its table.
