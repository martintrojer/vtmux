# vtmux

**Drop-in tmux. Modern core. Bugs fixed.**

A drop-in replacement for tmux 3.8, written in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode. Your `.tmux.conf` and plugins work unchanged.

Just the core: no agent integration, no extra TUI, no animations. Images pass through to your terminal. Where tmux has a bug (it loses data, hangs, or does something unsafe), vtmux fixes it and records the fix, with evidence, in [compat/DIVERGENCES.md](compat/DIVERGENCES.md). The one addition over tmux is an opt-in focus ring that makes the active pane obvious.

Compliance is checked against tmux's own regress suite and a behaviour oracle recorded from real tmux 3.8. Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** design stage. Nothing runs yet. See the [spec](docs/specs/2026-10-29-vtmux.md).

Linux first, macOS second. ISC license.
