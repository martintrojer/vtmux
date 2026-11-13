# vtmux

**Drop-in tmux replacement. Modern stack. Better rendering. Live upgrades.**

tmux 3.8, reimplemented in Rust on [libghostty-vt](https://github.com/ghostty-org/ghostty). Same CLI, config, formats, hooks and control mode, so your `.tmux.conf`, plugins and scripts work unchanged.

## Compliance

vtmux is tmux 3.8 to everything that talks to it: the same commands, options, formats, hooks, key tables, control mode and error messages, byte for byte where scripts can see them. Run as `tmux`, `tmux -V` prints `tmux 3.8`, so plugins' version checks pass.

- **tmux's own test suite:** 173 of its 174 regress tests pass (1 expected failure, recorded).
- **A behaviour oracle:** 449 scenarios recorded from real tmux 3.8 (screens, output, formats, control-mode streams) replay identically, all but six also with a live upgrade inserted between steps.
- **Your terminal:** Ghostty, WezTerm, foot, kitty, Alacritty, rio, xterm and some 2,600 other terminals from ncurses' database work out of the box, with the same features and colours tmux gives them, even on a host that has no terminfo entry for yours.
- **Your setup:** a real `.tmux.conf` with TPM plugins gives the same options, key bindings, hooks and screens as tmux 3.8 on macOS and Linux.
- **Restores across reboots:** tmux-resurrect and tmux-continuum work as with tmux.
- **Where it differs on purpose:** real tmux bugs (data loss, hangs) are fixed and listed in [compat/DIVERGENCES.md](https://github.com/martintrojer/vtmux/blob/main/compat/DIVERGENCES.md); everything else vtmux adds is opt-in.

## Compared with tmux 3.8

Measured, not assumed: release builds against tmux 3.8 on the same host ([numbers](https://github.com/martintrojer/vtmux/blob/main/docs/PERFORMANCE.md)).

| | vtmux vs tmux 3.8 |
|---|---|
| **Big scrollback** | 1M lines settle at 103 MB vs 622 MB. 20 panes streaming nonstop at a 100k limit: 217 MB vs 1.25 GB. Searching or capturing all of a 1M-line history takes 0.2-0.55x tmux's time; vim or less opens over it in 3 ms, as in tmux. |
| **CPU** | Pane output costs 0.43-0.54x tmux's server CPU, 0.48-0.58x with a client attached, 0.06-0.19x with 8 clients. An idle server with a real config (TPM, a status poller): 0.28x. Agent TUIs: 0.76-0.92x at 5-20 panes, 0.7x at 100. |
| **Rendering** | One frame per client, only what changed, inside synchronized output, the cursor placed once: no cursor jumping under heavy output. A redraw-heavy benchmark sends 0.07 MB where tmux sends 10.1 MB, at 0.3x the CPU. |
| **What it costs** | An idle server uses 8.5 MB vs 4.2 MB, each idle pane 20 KB vs 5.3 KB, the binary is 10.4 MB vs 1.3 MB. A key's echo takes 0.30 ms vs 0.21 ms. Many clients on one window cost more CPU than tmux (1.4x at 5). |

## What tmux can't do

Each one is off until you turn it on, and invisible to scripts while off. Put the lines inside `%if "#{vtmux_version}"` so the same `.tmux.conf` still loads in real tmux.

### Live upgrades

Swap the binary under a running server. Shells, editors and builds keep running, clients stay attached, scrollback comes along. 10 panes with 100k lines each move in under a second.

```sh
mise upgrade github:martintrojer/vtmux
ln -sf "$(mise which vtmux)" ~/.local/bin/tmux   # if you linked it as tmux
tmux vtmux-upgrade -b "$(mise which vtmux)"
```

Without `-b` it takes the path the server started from, or, when the client's protocol differs from the server's, the client's own binary. Exit status: 0 upgraded, 1 not (the server runs on as it was), 2 upgraded without a part that did not read (stderr says which). The new binary must prove it read the server's state before it takes over, so `-b` naming anything else (a typo'd `/bin/echo`, a script) is refused with 1. Downgrading to an earlier beta works too.

Saving sessions across a reboot is a plugin's job: tmux-resurrect and tmux-continuum work on vtmux as on tmux.

### Terminal features tmux drops

The Ghostty engine in each pane speaks protocols tmux 3.8 drops. vtmux passes them through to programs that ask, and keeps tmux's behaviour for everyone else.

Kitty keyboard protocol: Shift+Enter, C-i apart from Tab and Esc apart from Alt reach neovim, helix and agent TUIs that ask for them. Key tables and the prefix work as in tmux.

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

Kitty graphics: `kitten icat` images show in the pane on a Ghostty client, clipped to it like text. No `allow-passthrough` needed.

```tmux
%if "#{vtmux_version}"
set -s vtmux-graphics on
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

With [mise](https://mise.jdx.dev) (macOS 15 or later on Apple silicon, Linux x86_64 and aarch64):

```sh
mise use -g github:martintrojer/vtmux
ln -sf "$(mise which vtmux)" ~/.local/bin/tmux   # optional: be tmux (again after each upgrade)
```

On a Linux with glibc older than 2.35 (EL9, Debian 11), take the static build: `mise use -g "github:martintrojer/vtmux[matching=musl]"` (musl has no NSS: with sssd or LDAP users and no nscd, user names may not resolve). Tarballs are on the [releases page](https://github.com/martintrojer/vtmux/releases).

With cargo (needs Zig 0.17): `cargo install --locked vtmux@3.8.0-rc.1`. More in [RELEASING.md](https://github.com/martintrojer/vtmux/blob/main/docs/RELEASING.md).

Run as `vtmux`, it sits next to your tmux with its own sockets.

## Also

- **All the baggage.** The quirks plugins and scripts depend on are kept, byte for byte. Not a re-imagining: if you want a new multiplexer model, use Zellij. vtmux is for keeping your tmux setup and putting a better core under it.
- **Opt-in, never surprising.** Paper-cut fixes and new terminal features are opt-in (a `vtmux-*` option) or invisible to scripts. Real tmux bugs (data loss, hangs) are fixed by default and listed in [compat/DIVERGENCES.md](https://github.com/martintrojer/vtmux/blob/main/compat/DIVERGENCES.md).
- **Just the core.** No agent integration, no extra TUI, no session save and restore of its own: only what tmux and its plugins can't do.

**Status:** release candidate ([3.8-rc1](https://github.com/martintrojer/vtmux/blob/main/CHANGELOG.md)), used as the author's tmux every day. [How it works](https://github.com/martintrojer/vtmux/blob/main/docs/ARCHITECTURE.md). [Known issues](https://github.com/martintrojer/vtmux/blob/main/docs/KNOWN-ISSUES.md).

[How it was built](https://github.com/martintrojer/vtmux/blob/main/docs/HOW-IT-WAS-BUILT.md): agents, tmux 3.8 as the spec, and the checks.

[Security](https://github.com/martintrojer/vtmux/blob/main/SECURITY.md): reporting and threat model.

ISC license.
