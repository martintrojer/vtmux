# vtmux

**Drop-in tmux replacement. Modern core. Bugs fixed.**

A drop-in replacement for tmux 3.8, written in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode. Your `.tmux.conf` and plugins work unchanged.

The reason it exists: tmux has paper cuts that have sat in its backlog for years. vtmux fixes them without breaking compliance. Each fix is opt-in or invisible to scripts, so your config, plugins and scripts still see tmux 3.8. The first is an opt-in focus ring that makes the active pane obvious, instead of tmux's half-coloured border. Real tmux bugs (data loss, hangs) are fixed by default and documented in [compat/DIVERGENCES.md](compat/DIVERGENCES.md).

Just the core: no agent integration, no extra TUI, no animations. Images pass through to your terminal.

Compliance is checked against tmux's own regress suite and a behaviour oracle recorded from real tmux 3.8. Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** design stage. Nothing runs yet. See the [spec](docs/specs/2026-10-29-vtmux.md).

Linux first, macOS second. ISC license.
