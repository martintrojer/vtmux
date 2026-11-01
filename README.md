# vtmux

**Drop-in tmux replacement. Modern core. Bugs fixed.**

A drop-in replacement for tmux 3.8, written in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode. Your `.tmux.conf` and plugins work unchanged.

The reason it exists: tmux has paper cuts that have sat in its backlog for years. vtmux fixes them without breaking compliance. Each fix is opt-in or invisible to scripts, so your config, plugins and scripts still see tmux 3.8. The first is an opt-in focus ring that makes the active pane obvious, instead of tmux's half-coloured border. Real tmux bugs (data loss, hangs) are fixed by default and documented in [compat/DIVERGENCES.md](compat/DIVERGENCES.md).

Just the core: no agent integration, no extra TUI, no animations. Images pass through to your terminal.

Compliance is checked against tmux's own regress suite and a behaviour oracle recorded from real tmux 3.8. Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** early, not a daily tmux yet. M0 (the walking skeleton) is done; M1 (daily use) is well along. Working today: sessions, windows, panes and layouts, attach/detach, control mode (`-C`), options, formats, key bindings, hooks, the status line and messages, the command prompt, copy mode (search, selection, copy-pipe), paste buffers, popups, `run-shell`/`if-shell`, `%if` in configs, and the focus ring. Still missing: `choose-tree` and the other tree modes, the mouse, menus. It passes all 120 behaviour-oracle scenarios recorded from real tmux 3.8 and 54 of tmux's 174 regress tests (the rest are mostly waiting on the missing pieces). See the [spec](docs/specs/2026-10-29-vtmux.md) and [how it works](docs/ARCHITECTURE.md).

**Memory:** with 100,000 lines of scrollback in a 120x40 pane, the server uses about 101 MB against tmux 3.8's 66 MB (it was 148 MB before retention was fixed); idle scrollback compression is next ([spec](docs/specs/2026-10-31-history-memory.md), [numbers](docs/test-health/2026-11-01-history-memory.md)). Full memory and CPU comparisons are coming in `docs/PERFORMANCE.md`.

Linux first, macOS second. ISC license.
