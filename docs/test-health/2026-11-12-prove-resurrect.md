# tmux-resurrect and tmux-continuum against vtmux and tmux 3.8

Base `034e438a` plus the change (`crates/vtmux/tests/resurrect.rs`, the plugins vendored in `compat/tmux-plugins`, Ghostty patch 0045). Host: devserver, 96 cores, shared with other workers. Debug build, reference tmux 3.8 from `cargo xtask ref-tmux`.

## What the test does

Each side (vtmux, tmux 3.8) gets a private `/tmp` root: `HOME`, `TMUX_TMPDIR`, `XDG_STATE_HOME`, the plugin's save directory, and a `bin/` first on `PATH` with `tmux` (the binary under test), a `ps` that shows continuum only this side's processes (it stays off when it sees another tmux server of the user's) and a no-op `systemctl` (continuum disables a `tmux.service` otherwise). The panes' shell is `/bin/sh` started as a non-login shell, so the host's `/etc/profile` prints nothing into them.

- `resurrect_saves_and_restores_as_with_tmux`: sessions `work`, `misc` and the grouped `grp`; splits both ways, cwds (one with a space, one changed with `cd`), shell output, less, vim and `tail -f` running, a zoomed pane, pane titles, renamed windows. `save.sh quiet` by `run-shell` (what `prefix C-s` runs), `kill-server`, `new -d`, `restore.sh` (`prefix C-r`), pane contents on. Compared: resurrect's layout file and saved pane contents; sessions, windows (flags, layout, `automatic-rename`), panes (size, title, command, cwd) and every pane's screen after the restore.
- `continuum_saves_from_the_status_line_and_restores_at_start`: a client attached under a PTY; continuum's `#()` job in `status-right` saves; `kill-server`; the next server, with `@continuum-restore on`, restores by itself.

## Divergence found and fixed

resurrect saves pane contents with `capture-pane -epJ`. vtmux ended a `-J` (or `-T`) line at its last cell with text; tmux ends it at the line's `cellused` (grid.c `grid_string_cells`), which an erase inside the line does not lower (`grid_clear`). vim's first screen writes spaces on a row, then text and EL: tmux saved `line 3     `, vtmux `line 3`. Fixed with Ghostty patch 0045 (the row's `tmux_used` through the row API). Proof: `vtmux-vt` `join_ends_lines_at_the_cells_written` and oracle `capture_join_keeps_erased_cells` fail before the fix and pass after; the resurrect test's saved contents differed before.

## Races kept out of the comparison

resurrect's restore types each program's command right after laying the window out. A program can then draw for a size its pane had a moment before (tmux resizes a pane's tty on a timer, server-client.c `server_client_check_pane_resize`) and get one redraw more on one side than the other: seen under load as less showing `file.txt` on one side and `:` on the other, and once in the reference tmux as a less screen drawn 11 rows high in a 24-row pane. The test sends each restarted less and vim SIGWINCH and a redraw key before comparing. vim sits in the bottom pane of its window: in the top one it could read the pane's one-row moment, set a two-row scroll region and keep it after its pane grew back (seen on vtmux: `scroll_region_upper/lower` 0,1 in a 12-row pane).

Window names: tmux 3.8 names a new window after its pane's process before the shell has replaced it (`tmux` for up to ~500 ms) and renames it on a timer, and resurrect saves whatever name the window has. Review found the continuum test failing 4/14 at idle and 6/6 under build load (`misc:1` saved as `sh` on vtmux, `tmux` on tmux 3.8): it attached, and so saved, before that window was renamed. Both tests now wait for every pane's prompt and for `misc:1` to be named `sh` before the save and after the restore.

Shell names: on macOS `/bin/sh` runs as `bash`, so `#{pane_current_command}` is `bash` under both servers, and the first macOS run failed every time on both binaries waiting for `sh`. The test no longer names the shell: a shell pane is ready when its last row is a bare prompt (the count tells a restored pane's new prompt from its saved one), an automatically named window is waited on until its name is its pane's command, and the assertions take the shell's name from an idle pane. SIGWINCH goes to every process under the pane, not only its children, in case `/bin/sh` does not exec the shell. Simulated on Linux by making the panes' shell `bash --posix` (named `bash`): the previous version timed out on both binaries as on macOS, this one passes 3/3.

## Runs

| run | result |
|---|---|
| after the shell-name change: the test binary 10x at idle, 5x under 48 busy loops, once with a dash-like `/bin/sh` | 10/10, 5/5, 2/2 |
| after the window-name wait: the test binary run directly, 14x at idle | 14/14, 3.9-4.3 s |
| the same, 8x under 48 busy loops and 4x under 96 | 12/12, 4.1-4.2 s and 6.7-6.8 s |
| before it: `cargo test -p vtmux --test resurrect`, 10x under 48 busy loops (`flock /tmp/vtmux-stress.lock`) | 10/10, 4.3-4.6 s each |
| the same 12x, an earlier version (programs redrawn before the save too) | 12/12 |
| earlier still: vim in the top pane, redrawn | 1 failure in 8 (the stuck scroll region), 1 in 2 |
| earlier still: no redraw | 5 failures in 5 (less's last row, vim) |
| once with a dash-like `/bin/sh` (bind-mounted in a user and mount namespace) | 2/2 |
| the resurrect test with the capture fix reverted | fails on the saved contents (`line 3     ` vs `line 3`) |

Gate: `muzzle run --class high --rule vtmux-check --run-timeout 3000 -- mise exec -- bash scripts/check.sh` at `30c0558d` with `GHOSTTY_SOURCE_TARBALL` (`git archive` of the pinned commit; no GitHub on the host): exit 0 in 273 s. cargo test all pass; regress 171/174, 1 xfail, 2 flaky (passed on retry); oracle replay 444/444; `--upgrade-every 3` 444/444; `--upgrade-every 1` 9/9. The macOS type-check was skipped (no `aarch64-apple-darwin` target on the host). A first gate run at the same commit failed one test, `attach::a_signal_as_ready_is_read_ends_it_as_not_attached` (timed out waiting for `client-detached` under the gate's load); it passed 5/5 alone and in the second gate run, and nothing in this change touches the client's signal path.

## Seen on the way, not fixed

A debug-build vtmux server flooded with key input (an attached client reading `/dev/zero`) answered command clients in 4-6 s with `status-right` set to a short string, against 25 ms for tmux; with the default `status-right` it answered in 6 ms. Under that flood continuum's save job, whose `tmux` calls each waited seconds, was still running when the test ended. Not a plugin problem and not seen without the flood.
