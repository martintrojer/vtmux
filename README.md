# vtmux

**tmux on a modern stack: all the baggage, fast and lean, fewer paper cuts, better rendering.**

A drop-in replacement for tmux 3.8, written in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode, so your `.tmux.conf`, plugins and scripts work unchanged.

- **All the baggage.** The quirks plugins and scripts depend on are kept, byte for byte. Not a re-imagining: if you want a new multiplexer model, use Zellij. vtmux is for keeping your tmux setup and putting a better core under it.
- **Fast and lean.** That's the goal, and it's measured, not assumed: today a server holding fresh scrollback, or many panes, uses more memory than tmux, and the CPU and memory comparison with tmux 3.8 is in [docs/PERFORMANCE.md](docs/PERFORMANCE.md).
- **Fewer paper cuts.** Long-standing tmux annoyances fixed without breaking compliance: each fix is opt-in or invisible to scripts ([docs/PAPERCUTS.md](docs/PAPERCUTS.md)). The first is an opt-in focus ring that makes the active pane obvious, instead of tmux's half-coloured border. Real tmux bugs (data loss, hangs) are fixed by default and documented in [compat/DIVERGENCES.md](compat/DIVERGENCES.md).
- **Better rendering.** The server composites one frame per client and sends only what changed, inside synchronized output, with the cursor placed once, so heavy output doesn't make the cursor jump around ([how it works](docs/ARCHITECTURE.md)).

Just the core: no agent integration, no extra TUI, no animations. Images pass through to your terminal.

Compliance is checked against tmux's own regress suite and a behaviour oracle recorded from real tmux 3.8. Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** early, not a daily tmux yet. M0 (the walking skeleton) is done; M1 (daily use) is well along. Working today: sessions, windows, panes and layouts, attach/detach, control mode (`-C`), options, formats, key bindings, hooks, the status line and messages, the command prompt, copy mode (search, selection, copy-pipe), `choose-tree` with its preview, clock mode, `display-panes`, paste buffers, popups, `run-shell`/`if-shell`, `#()` in formats, `%if` in configs, `set-titles`, `synchronize-panes`, the focus ring, and the mouse (clicks, the status line, the wheel, drag-select and border resize, forwarding to mouse-aware programs). Still missing: the other choose modes, menus. It passes all 169 behaviour-oracle scenarios recorded from real tmux 3.8 and 62 of tmux's 174 regress tests (the rest are mostly waiting on the missing pieces). See the [spec](docs/specs/2026-10-29-vtmux.md) and [how it works](docs/ARCHITECTURE.md).

**Server memory and CPU vs tmux 3.8** (release build, Linux; [numbers and method](docs/PERFORMANCE.md)): an idle server uses 7.3 MB against 4.3 MB, and an idle pane about 15 KB against 5.4 KB. With 100,000 lines of scrollback in a 120x40 pane it uses about 101 MB against 66 MB while the output arrives, and about 17 MB once the pane goes idle and its scrollback is compressed (`capture-pane` output unchanged). Consuming 100 MB of pane output with no client takes 1.17 s of server CPU against 2.53 s (plain text). With a client attached, the merged build still uses 6-10x tmux's server CPU on such floods; a fix in review measures 0.5-0.9x. With the owner's own config (TPM plugins and the mu-crew poller) the idle server uses 8.0 MB against 4.6 MB and 48 ms of CPU a minute against 187 ms; sourcing the config takes 62 ms for both.

Linux first, macOS second: CI runs the full gate, including the tmux comparison suites, on both. ISC license.
