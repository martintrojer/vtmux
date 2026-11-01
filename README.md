# vtmux

**tmux on a modern stack: all the baggage, fast and lean, fewer paper cuts, better rendering.**

A drop-in replacement for tmux 3.8, written in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode, so your `.tmux.conf`, plugins and scripts work unchanged.

- **All the baggage.** The quirks plugins and scripts depend on are kept, byte for byte. Not a re-imagining: if you want a new multiplexer model, use Zellij. vtmux is for keeping your tmux setup and putting a better core under it.
- **Fast and lean.** That's the goal, and it's measured, not assumed: today scrollback-heavy servers still use more memory than tmux (see Memory below), and CPU numbers are coming in `docs/PERFORMANCE.md`.
- **Fewer paper cuts.** Long-standing tmux annoyances fixed without breaking compliance: each fix is opt-in or invisible to scripts ([docs/PAPERCUTS.md](docs/PAPERCUTS.md)). The first is an opt-in focus ring that makes the active pane obvious, instead of tmux's half-coloured border. Real tmux bugs (data loss, hangs) are fixed by default and documented in [compat/DIVERGENCES.md](compat/DIVERGENCES.md).
- **Better rendering.** The server composites one frame per client and sends only what changed, inside synchronized output, with the cursor placed once, so heavy output doesn't make the cursor jump around ([how it works](docs/ARCHITECTURE.md)).

Just the core: no agent integration, no extra TUI, no animations. Images pass through to your terminal.

Compliance is checked against tmux's own regress suite and a behaviour oracle recorded from real tmux 3.8. Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** early, not a daily tmux yet. M0 (the walking skeleton) is done; M1 (daily use) is well along. Working today: sessions, windows, panes and layouts, attach/detach, control mode (`-C`), options, formats, key bindings, hooks, the status line and messages, the command prompt, copy mode (search, selection, copy-pipe), clock mode, paste buffers, popups, `run-shell`/`if-shell`, `%if` in configs, and the focus ring. Still missing: `choose-tree` and the other tree modes, the mouse, menus. It passes all 126 behaviour-oracle scenarios recorded from real tmux 3.8 and 61 of tmux's 174 regress tests (the rest are mostly waiting on the missing pieces). See the [spec](docs/specs/2026-10-29-vtmux.md) and [how it works](docs/ARCHITECTURE.md).

**Memory:** with 100,000 lines of scrollback in a 120x40 pane, the server uses about 101 MB against tmux 3.8's 66 MB (it was 148 MB before retention was fixed); idle scrollback compression is next ([spec](docs/specs/2026-10-31-history-memory.md), [numbers](docs/test-health/2026-11-01-history-memory.md)). Full memory and CPU comparisons are coming in `docs/PERFORMANCE.md`.

Linux first, macOS second. ISC license.
