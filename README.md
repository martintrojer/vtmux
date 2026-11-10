# vtmux

**Drop-in tmux replacement. Modern stack. Better rendering. Live upgrades.**

tmux 3.8, reimplemented in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode, so your `.tmux.conf`, plugins and scripts work unchanged.

## Compliance

vtmux is tmux 3.8 to everything that talks to it: the same commands, options, formats, hooks, key tables, control mode and error messages, byte for byte where scripts can see them. Run as `tmux`, `tmux -V` prints `tmux 3.8`, so plugins' version checks pass.

- **tmux's own test suite:** 173 of its 174 regress tests pass (1 expected failure, recorded).
- **A behaviour oracle:** 419 scenarios recorded from real tmux 3.8 (screens, output, formats, control-mode streams) replay identically, also with a live upgrade inserted between steps.
- **Your terminal:** most `TERM`s of ncurses 6.6's database (2,663 names in vtmux's table: `wezterm`, `contour`, `foot`, `rio`, `ghostty`, `xterm-direct`, `ansi`...) attach with the features, colours and capabilities tmux 3.8 gives it (each checked against tmux); 163 8-bit ones (`vt220-8bit`) are refused where tmux takes them. `TERM=wezterm tmux new` works on a host with no wezterm terminfo.
- **Your setup:** a real `.tmux.conf` with TPM plugins gives the same options, key bindings, hooks and screens as tmux 3.8 on macOS and Linux.
- **Where it differs on purpose:** real tmux bugs (data loss, hangs) are fixed and listed in [compat/DIVERGENCES.md](https://github.com/martintrojer/vtmux/blob/main/compat/DIVERGENCES.md); everything else vtmux adds is opt-in.

## Compared with tmux 3.8

Measured, not assumed: release builds against tmux 3.8 on the same host ([numbers](https://github.com/martintrojer/vtmux/blob/main/docs/PERFORMANCE.md)).

| | vtmux vs tmux 3.8 |
|---|---|
| **Big scrollback** | 1M lines settle at 103 MB vs 622 MB. 20 panes streaming nonstop at a 100k limit: 217 MB vs 1.25 GB. Searching or capturing all of a 1M-line history takes 0.2-0.55x tmux's time; vim or less opens over it in 3 ms, as in tmux. |
| **CPU** | Pane output costs 0.37-0.54x tmux's server CPU, 0.42-0.62x with a client attached, 0.06-0.19x with 8 clients. An idle server with a real config (TPM, a status poller): 0.28x. 100 agent panes: 0.7x. |
| **Rendering** | One frame per client, only what changed, inside synchronized output, the cursor placed once: no cursor jumping under heavy output. A redraw-heavy benchmark sends 0.07 MB where tmux sends 10.1 MB, at 0.3x the CPU. |
| **What it costs** | An idle server uses 9.0 MB vs 4.2 MB, each idle pane 18 KB vs 5.4 KB, the binary is 10.0 MB vs 1.3 MB. A key's echo takes 0.38 ms vs 0.26 ms. A few agent TUIs with two clients attached cost up to 1.25x tmux's CPU. |

## What tmux can't do

Each one is off until you turn it on, and invisible to scripts while off. Put the lines inside `%if "#{vtmux_version}"` so the same `.tmux.conf` still loads in real tmux.

### Live upgrades

Swap the binary under a running server. Shells, editors and builds keep running, clients stay attached, scrollback comes along. 10 panes with 100k lines each move in under a second.

```sh
mise upgrade vtmux
tmux vtmux-upgrade -b "$(mise which vtmux)"
```

Without `-b` it takes the path the server started from, or, when the client's protocol differs from the server's, the client's own binary. Exit status: 0 upgraded, 1 not (the server runs on as it was), 2 upgraded without a part that did not read (stderr says which).

### Full restores

After a crash or a reboot, sessions come back with their windows, layout, screens and scrollback. Panes come back dead; `respawn-pane` restarts each one's command in its directory.

```tmux
%if "#{vtmux_version}"
set -s vtmux-persist full    # or layout: sessions and windows, no screens
%endif
```

Checkpoints go to `~/.local/state/vtmux/` on a clean exit and every 60 s while something changed.

### Terminal features tmux drops

The Ghostty engine in each pane speaks protocols tmux 3.8 drops. vtmux passes them through to programs that ask, and keeps tmux's behaviour for everyone else.

Kitty keyboard protocol: Shift+Enter, C-i apart from Tab and Esc apart from Alt reach neovim, helix and agent TUIs that ask for them. Key tables and the prefix work as before.

```tmux
%if "#{vtmux_version}"
set -s vtmux-kitty-keys on
%endif
```

Desktop notifications: a build finishing in a background window, or an agent in a popup, pops up in Ghostty (OSC 9 and 777). No `allow-passthrough` needed.

```tmux
%if "#{vtmux_version}"
set -s vtmux-notifications on
%endif
```

In-band resize: neovim gets each new size in-band (mode 2048), with no `SIGWINCH` race.

```tmux
%if "#{vtmux_version}"
set -s vtmux-inband-resize on
%endif
```

### Pane blend

Inactive panes dim, every cell, like Ghostty's unfocused splits. You always know where you're typing, and it costs no screen space.

```tmux
%if "#{vtmux_version}"
set -g vtmux-pane-inactive-blend 'fg=bg/25,bg=bg/25'
%endif
```

`vtmux-pane-active-blend` does the same for the active pane. All the options: [docs/PAPERCUTS.md](https://github.com/martintrojer/vtmux/blob/main/docs/PAPERCUTS.md).

## Install

With [mise](https://mise.jdx.dev) (macOS on Apple silicon, Linux x86_64 and aarch64):

```sh
mise use -g github:martintrojer/vtmux
ln -s "$(mise which vtmux)" ~/.local/bin/tmux   # optional: be tmux
```

Prebuilt tarballs are on the [releases page](https://github.com/martintrojer/vtmux/releases).

With cargo. The build compiles Ghostty, so it needs Zig 0.17 on `PATH`:

```sh
mise use -g zig@0.17
cargo install --locked vtmux@3.8.0-beta.1   # a beta: cargo needs the version named
```

That build lacks the tarballs' whole-program LTO, worth 7-18% of the server's CPU on render-bound work; [docs/RELEASING.md](https://github.com/martintrojer/vtmux/blob/main/docs/RELEASING.md#what-cargo-install-vtmux-gets) has the line that keeps it.

From a checkout (mise brings the pinned Zig): `mise install && mise exec -- cargo install --path crates/vtmux`.

Run as `tmux`, it is tmux to every script and plugin: `tmux -V` prints `tmux 3.8`. Run as `vtmux`, it sits next to your tmux with its own sockets: `vtmux -V` prints `vtmux 3.8-beta1`.

## Also

- **All the baggage.** The quirks plugins and scripts depend on are kept, byte for byte. Not a re-imagining: if you want a new multiplexer model, use Zellij. vtmux is for keeping your tmux setup and putting a better core under it.
- **Opt-in, never surprising.** Paper-cut fixes and new terminal features are opt-in (a `vtmux-*` option) or invisible to scripts. Real tmux bugs (data loss, hangs) are fixed by default and listed in [compat/DIVERGENCES.md](https://github.com/martintrojer/vtmux/blob/main/compat/DIVERGENCES.md).
- **Just the core.** No agent integration, no extra TUI, no animations. Images pass through to your terminal.

Inside, it's idiomatic Rust: `unsafe` only at the FFI and OS boundary.

**Status:** beta ([3.8-beta1](https://github.com/martintrojer/vtmux/blob/main/CHANGELOG.md)), used as the author's tmux every day. [How it works](https://github.com/martintrojer/vtmux/blob/main/docs/ARCHITECTURE.md).

Linux first, macOS second: CI runs the full gate, including the tmux comparison suites, on both. ISC license.
