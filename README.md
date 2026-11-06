# vtmux

**Drop-in tmux replacement. Modern stack. Better rendering. Live upgrades.**

A drop-in replacement for tmux 3.8, written in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode, so your `.tmux.conf`, plugins and scripts work unchanged.

## Compared with tmux 3.8

Measured, not assumed: release builds against tmux 3.8 on the same host ([numbers and method](docs/PERFORMANCE.md)).

| | vtmux vs tmux 3.8 |
|---|---|
| **Big scrollback** | 1M lines settle at 103 MB vs 622 MB; 10 panes x 100k lines at 100 MB vs 629 MB; 20 panes streaming nonstop at their 100k limit at 217 MB vs 1.25 GB. Scrollback is compressed once it goes quiet, or in small steps while a pane keeps streaming. |
| **CPU** | Pane output costs 0.37-0.46x tmux's server CPU with no client and 0.42-0.62x with one attached; 8 clients 0.06-0.19x; an idle server with a real config (TPM, a status poller) 0.28x. Busy agent TUIs (5-20 panes): 0.85-1.3x. |
| **Rendering** | One composed frame per client, only what changed, inside synchronized output, the cursor placed once: no cursor jumping under heavy output. Under heavy output that means about 140x fewer bytes to your terminal: a redraw-heavy benchmark sends 0.07 MB where tmux sends 10.1 MB, at 0.3x the CPU. Typing in a shell sends about the same few bytes as tmux. |
| **Compatibility** | Same CLI, config, formats, hooks and control mode. All 272 behaviour-oracle scenarios recorded from real tmux 3.8 pass, and 158 of tmux's own 174 regress tests (12 expected failures, each recorded, 1 skipped). |
| **What it costs** | Speed over a few MB, as Ghostty does: an idle server uses 9.0 MB vs 4.2 MB and each idle pane 18 KB vs 5.4 KB; a key's echo takes 0.33 ms vs 0.21 ms (both far below what you can see); searching all of a 1M-line history in copy mode is 3-5x slower. |

## What tmux can't do

**Pane blend.** Inactive panes dim, every cell, like Ghostty's unfocused splits. You always know where you're typing, and it costs no screen space ([spec](docs/specs/2026-11-04-pane-blend.md)).

```tmux
%if "#{vtmux_version}"
set -g vtmux-pane-inactive-blend 'fg=bg/25'
%endif
```

**Live upgrades.** `tmux vtmux-upgrade` swaps in a new binary under a running server. Your shells, editors and builds keep running; clients stay attached. Opt-in `vtmux-persist` uses the same snapshot to bring sessions and scrollback back after a reboot ([spec](docs/specs/2026-11-06-live-upgrade.md); in progress).

## Also

- **All the baggage.** The quirks plugins and scripts depend on are kept, byte for byte. Not a re-imagining: if you want a new multiplexer model, use Zellij. vtmux is for keeping your tmux setup and putting a better core under it.
- **Paper cuts, not features.** Long-standing tmux annoyances are fixed only where scripts can't tell: each fix is opt-in (a `vtmux-*` option) or invisible ([docs/PAPERCUTS.md](docs/PAPERCUTS.md)). Real tmux bugs (data loss, hangs) are fixed by default and documented in [compat/DIVERGENCES.md](compat/DIVERGENCES.md).
- **Just the core.** No agent integration, no extra TUI, no animations. Images pass through to your terminal.

Compliance is checked against tmux's own regress suite and a behaviour oracle recorded from real tmux 3.8. Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** unreleased. M1 (daily use) is done: every tmux 3.8 command is implemented and the author uses vtmux as their tmux every day. M2 (live upgrade and persistence, then hardening from daily use) is in progress. See the [spec](docs/specs/2026-10-29-vtmux.md) and [how it works](docs/ARCHITECTURE.md).

Linux first, macOS second: CI runs the full gate, including the tmux comparison suites, on both. ISC license.
