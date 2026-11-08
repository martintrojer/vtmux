# vtmux

**Drop-in tmux replacement. Modern stack. Better rendering. Live upgrades.**

A drop-in replacement for tmux 3.8, written in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode, so your `.tmux.conf`, plugins and scripts work unchanged.

## Install

With [mise](https://mise.jdx.dev) (macOS on Apple silicon, Linux x86_64 and aarch64):

```sh
mise use -g github:martintrojer/vtmux
ln -s "$(mise which vtmux)" ~/.local/bin/tmux   # optional: be tmux
```

Prebuilt tarballs are on the [releases page](https://github.com/martintrojer/vtmux/releases). From a checkout (needs [mise](https://mise.jdx.dev) for the pinned Zig): `mise install && mise exec -- cargo install --path crates/vtmux`.

As `tmux` it is tmux to every script and plugin (`tmux -V` prints `tmux 3.8`). As `vtmux` it runs next to your tmux: own sockets, `vtmux -V` prints `vtmux 3.8-beta1`.

**Upgrade without losing a session:** `mise upgrade vtmux`, then `tmux vtmux-upgrade -b "$(mise which vtmux)"`. The running server moves onto the new binary; panes and clients stay. See the [changelog](CHANGELOG.md).

## Compared with tmux 3.8

Measured, not assumed: release builds against tmux 3.8 on the same host ([numbers and method](docs/PERFORMANCE.md)).

| | vtmux vs tmux 3.8 |
|---|---|
| **Big scrollback** | 1M lines settle at 103 MB vs 622 MB; 10 panes x 100k lines at 100 MB vs 629 MB; 20 panes streaming nonstop at their 100k limit at 217 MB vs 1.25 GB. Scrollback is compressed once it goes quiet, or in small steps while a pane keeps streaming. |
| **CPU** | Pane output costs 0.37-0.46x tmux's server CPU with no client and 0.42-0.62x with one attached; 8 clients 0.06-0.19x; an idle server with a real config (TPM, a status poller) 0.28x. Busy agent TUIs (5-20 panes): 0.85-1.3x. |
| **Rendering** | One composed frame per client, only what changed, inside synchronized output, the cursor placed once: no cursor jumping under heavy output. Under heavy output that means about 140x fewer bytes to your terminal: a redraw-heavy benchmark sends 0.07 MB where tmux sends 10.1 MB, at 0.3x the CPU. Typing in a shell sends about the same few bytes as tmux. |
| **Compatibility** | Same CLI, config, formats, hooks and control mode. All 341 behaviour-oracle scenarios recorded from real tmux 3.8 pass, and 168 of tmux's own 174 regress tests (3 expected failures, each recorded, 1 skipped). |
| **What it costs** | Speed over a few MB, as Ghostty does: an idle server uses 9.0 MB vs 4.2 MB and each idle pane 18 KB vs 5.4 KB; a key's echo takes 0.33 ms vs 0.21 ms (both far below what you can see); searching all of a 1M-line history in copy mode is 1.8x slower (about even at 100k lines). |

## What tmux can't do

**Pane blend.** Inactive panes dim, every cell, like Ghostty's unfocused splits. You always know where you're typing, and it costs no screen space ([spec](docs/specs/2026-11-04-pane-blend.md)).

```tmux
%if "#{vtmux_version}"
set -g vtmux-pane-inactive-blend 'fg=bg/25'
%endif
```

**Live upgrades.** `tmux vtmux-upgrade` swaps in a new binary under a running server. Your shells, editors and builds keep running; clients stay attached. Opt-in `vtmux-persist` uses the same snapshot to bring sessions back after a crash or reboot, with `full` their screens and scrollback too, panes dead until `respawn-pane` ([spec](docs/specs/2026-11-06-live-upgrade.md)).

## Also

- **All the baggage.** The quirks plugins and scripts depend on are kept, byte for byte. Not a re-imagining: if you want a new multiplexer model, use Zellij. vtmux is for keeping your tmux setup and putting a better core under it.
- **Paper cuts, not features.** Long-standing tmux annoyances are fixed only where scripts can't tell: each fix is opt-in (a `vtmux-*` option) or invisible ([docs/PAPERCUTS.md](docs/PAPERCUTS.md)). Real tmux bugs (data loss, hangs) are fixed by default and documented in [compat/DIVERGENCES.md](compat/DIVERGENCES.md).
- **Just the core.** No agent integration, no extra TUI, no animations. Images pass through to your terminal.

Compliance is checked against tmux's own regress suite and a behaviour oracle recorded from real tmux 3.8. Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** beta, [3.8-beta1](CHANGELOG.md). M1 (daily use) is done: every tmux 3.8 command is implemented and the author uses vtmux as their tmux every day. M2 (live upgrade and persistence, then hardening from daily use) is in progress. See the [spec](docs/specs/2026-10-29-vtmux.md) and [how it works](docs/ARCHITECTURE.md).

Linux first, macOS second: CI runs the full gate, including the tmux comparison suites, on both. ISC license.
