# Changelog

vtmux's version is the tmux release it is tested against: `3.8-beta1` is the first beta of vtmux for tmux 3.8. `tmux -V` still prints `tmux 3.8`, so plugins' version checks see tmux; `vtmux -V` prints `vtmux 3.8-beta1`.

## 3.8-beta1

First release. Linux (x86_64, aarch64) and macOS 15+ (Apple silicon).

**Drop-in tmux 3.8.** Every tmux 3.8 command, option, format, hook and control-mode message. Your `.tmux.conf`, plugins and scripts work unchanged. All 415 behaviour-oracle scenarios recorded from real tmux 3.8 pass, and 173 of tmux's own 174 regress tests (1 expected failure, recorded).

**Ghostty's emulator.** Each pane is a [libghostty-vt](https://github.com/ghostty-org/ghostty) terminal, with tmux's quirks kept where scripts can see them.

**Better rendering.** One frame per client, only what changed, inside synchronized output, the cursor placed once: no cursor jumping under heavy output. A redraw-heavy benchmark sends 0.07 MB to your terminal where tmux sends 10.1 MB, at 0.3x the CPU.

**Live upgrade.** `tmux vtmux-upgrade -b <new binary>` swaps the binary under a running server. Shells, editors and builds keep running; clients stay attached; scrollback comes along. 10 panes x 100k lines upgrade in 0.6-0.9 s.

**Persistence (opt-in).** `set -s vtmux-persist layout` (or `full`, with screens and scrollback) checkpoints the server. After a crash or reboot it comes back with its sessions, windows and layout, panes dead until `respawn-pane`.

**Pane blend (opt-in).** `vtmux-pane-inactive-blend` dims inactive panes like Ghostty's unfocused splits.

**Kitty keyboard protocol (opt-in).** `set -s vtmux-kitty-keys on` and programs that ask for it (neovim, helix, agent TUIs) get kitty-encoded keys: Shift+Enter, C-i apart from Tab, Escape apart from Alt. Key tables and the prefix work as before; programs that don't ask see tmux. A grant ends with the program that asked, so a killed editor never leaves the shell sending `CSI 99;5u` for C-c, and a live upgrade keeps it. One exception: a program sharing a surviving shell's process group (`prog; exec bash`) keeps it until `reset`.

**Desktop notifications (opt-in).** `set -s vtmux-notifications on`: a pane's OSC 9 and OSC 777 notifications reach attached Ghostty clients from any window (a `display-popup -E` agent's too, on its own client), rate-limited as Ghostty limits itself.

**crates.io ready.** All eight crates package and build from their `.crate` files alone, so `cargo install --locked vtmux` works once they are published (Zig 0.17 on `PATH`). The patched libghostty-rs ships as `vtmux-ghostty-vt-sys` and `vtmux-ghostty-vt`, whose build downloads the pinned Ghostty and checks its hash (`GHOSTTY_SOURCE_TARBALL` for offline builds), and stops with what to install when Zig is missing or the wrong version. How to release: [docs/RELEASING.md](docs/RELEASING.md).

**In-band resize (opt-in).** `set -s vtmux-inband-resize on`: programs that ask for mode 2048 (neovim) get every new size in-band, no `SIGWINCH` race. Live upgrades keep the grant, and a downgrade that would drop it is refused.

Every opt-in is tmux 3.8, byte for byte, while off.

**Built for speed.** Whole-program LTO: 6-18% less server CPU for agent panes and many clients, key echo 12% faster, a 9.8 MB stripped binary (was 10.7).

**Lean where it counts** ([numbers](docs/PERFORMANCE.md)):

- 1M lines of scrollback settle at 103 MB vs tmux's 622 MB; 20 panes streaming at a 100k limit at 217 MB vs 1.25 GB.
- Pane output at 0.37-0.62x tmux's server CPU; 8 attached clients at 0.06-0.19x; an idle server with a real config at 0.28x.
- Searching or capturing all of a 1M-line history at 0.2-0.55x tmux's time.

**What it costs:** an idle server uses 9.0 MB vs 4.2 MB and each idle pane 18 KB vs 5.4 KB; a key's echo takes 0.38 ms vs 0.26 ms; a few agent TUIs with two clients cost up to 1.25x tmux's CPU; vim or less opening over a 1M-line history holds the server for 2-4 s.

Real tmux bugs fixed by default are in [compat/DIVERGENCES.md](compat/DIVERGENCES.md); opt-in fixes and options in [docs/PAPERCUTS.md](docs/PAPERCUTS.md).
