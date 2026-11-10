# vtmux docs

For users: the [README](../README.md), [PAPERCUTS.md](PAPERCUTS.md) (every `vtmux-*` option), [PERFORMANCE.md](PERFORMANCE.md), the [changelog](../CHANGELOG.md) and [compat/DIVERGENCES.md](../compat/DIVERGENCES.md) (where vtmux differs from tmux 3.8, and why).

For contributors: [AGENTS.md](../AGENTS.md) (gates and rules), [ARCHITECTURE.md](ARCHITECTURE.md), [oracle/README.md](../oracle/README.md) (writing scenarios), [test-health/](test-health/) (benchmark and test-suite records, and how they are made), and the specs below.

## Specs

| Spec | What it decides |
|---|---|
| [2026-10-29-vtmux.md](specs/2026-10-29-vtmux.md) | The project: pitch, scope, decisions D1-D31 and D33, compliance triangulation, milestones |
| [2026-10-31-history-memory.md](specs/2026-10-31-history-memory.md) | Scrollback memory: retain close to `history-limit`, compress quiet history, keep compressing a pane that never stops |
| [2026-11-04-pane-blend.md](specs/2026-11-04-pane-blend.md) | Pane blend (D26): blend every cell of active or inactive panes toward a colour |
| [2026-11-06-live-upgrade.md](specs/2026-11-06-live-upgrade.md) | Live upgrade and persistence (D27), with its own numbered decisions 1-30 (cited as "decision 13") |
| [2026-11-09-osc-survey.md](specs/2026-11-09-osc-survey.md) | Survey of terminal protocols tmux 3.8 drops; led to D28-D31 |

## Decisions

From the main spec's *Key decisions*. The spec has the reasons; a change that contradicts one updates the spec first.

| | Decision |
|---|---|
| D1 | Named `vtmux`; own binary and socket dir, optional `tmux` symlink, so it runs beside real tmux |
| D2 | The compatibility surface is the CLI, config, formats, options, hooks, key tables, control mode and environment; the client/server protocol is vtmux's own |
| D3 | Pinned to tmux 3.8's regress suite and behaviour; bumps are deliberate |
| D4 | The grid follows Ghostty, everything else follows tmux exactly; an esctest/xterm arbiter settles grid disputes, ties go to tmux |
| D5 | Panes get a shim dir with `tmux` → vtmux first on `PATH`, and a tmux-shaped `$TMUX` |
| D6 | Config search order identical to tmux's |
| D7 | `default-terminal` honoured exactly, default `tmux-256color` |
| D8 | Output to the outer terminal follows `terminal-features`, `terminal-overrides` and terminfo, never assumes Ghostty |
| D9 | Single-threaded `mio` event loop; blocking work in child processes |
| D10 | `unsafe` only in `vtmux-vt` and the server's `sys/`; every other crate forbids it |
| D11 | Behaviour-faithful, not code-faithful: tmux's C is read as the spec, the core is idiomatic Rust |
| D12 | Hand-written recursive-descent command parser |
| D13 | `libghostty-vt` at an exact pin, vendored and patched when a bug can't wait |
| D14 | The server composes and encodes frames per client; the client is a pipe |
| D15 | Layout is a tiled tree plus a floating layer; layout strings in 3.8's format |
| D16 | Agents may write code; every merge is small, reviewed and green on regress and the oracle |
| D17 | ISC license |
| D18 | Rust 2024, stable pinned, clippy pedantic, Zig pinned through mise |
| D19 | Public repo from the design stage; no announcement before M2 |
| D20 | Socket dir follows the invoked name: `tmux-UID` as `tmux`, `vtmux-UID` as `vtmux` |
| D21 | `tmux -V` prints `tmux 3.8`; `#{vtmux_version}` and `VTMUX_VERSION` identify vtmux (amended by D25) |
| D22 | Grid divergences don't block M1; they are recorded as Deferred |
| D23 | The focus ring paper-cut fix; superseded by D26 and removed 2026-11-06 |
| D24 | The pitch: "Drop-in tmux replacement. Modern stack. Better rendering. Live upgrades."; speed and memory claimed only with published numbers |
| D25 | The version is the tmux release it is tested against: `3.8-beta1`, then `3.8` |
| D26 | Pane blend: `vtmux-pane-active-blend` and `vtmux-pane-inactive-blend` |
| D27 | Live upgrade by `execve` in place, with opt-in persistence (`vtmux-persist`) from the same snapshot |
| D28 | Protocol extensions: a protocol tmux drops, only where libghostty and Ghostty implement it, behind a `vtmux-*` option, tmux when off |
| D29 | Kitty keyboard protocol, pane side: `vtmux-kitty-keys` |
| D30 | Desktop notifications, OSC 9 and 777, to Ghostty clients: `vtmux-notifications` |
| D31 | In-band resize, mode 2048: `vtmux-inband-resize` |
| D33 | Kitty graphics: `vtmux-graphics` and `vtmux-graphics-limit`, drawn as unicode placeholders, dropped on live upgrade |

Divergence entries are numbered D-NNN (`D-001`, ...) in `compat/DIVERGENCES.md`: a different series from the decisions.
