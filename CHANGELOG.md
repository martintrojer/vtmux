# Changelog

vtmux's version is the tmux release it is tested against: `3.8-beta1` is the first beta of vtmux for tmux 3.8. `tmux -V` still prints `tmux 3.8`, so plugins' version checks see tmux; `vtmux -V` prints `vtmux 3.8-beta1`.

## Unreleased

Next is `3.8-beta2`.

**Your terminal.** Most of ncurses 6.6's terminal database (2,663 names in vtmux's table: `wezterm`, `contour`, `foot`, `rio`, `ansi` and `Eterm` among them) attaches with the features, colours and capabilities tmux 3.8 gives it, on hosts that have no terminfo for it. Hardcopy terminals are refused with tmux's message. 163 8-bit terminals (`vt220-8bit`, `ansi.sys`) are refused where tmux takes them. The binary grows by 0.3 MB, to 10.0 MB.

**Pixel sizes, as tmux gives them.** A pane's terminal reports its size in pixels from your terminal's cell size (`ws_xpixel`, `CSI 14 t`, `#{window_cell_width}`), so image tools size their output; 16x32 per cell until a client gives one.

**Formats.** `#{pane_last_output_time}`, the second of the pane's last output, as tmux sets it.

**Fixes.** A split or respawned pane's window is named after its command, as a new window's is, not after the process caught before it execs (the server binary, `vtmux-<sha>`, on macOS). With `vtmux-notifications on`, a client re-attaching after a live upgrade while a pane sent a notification no longer crashes the server. A pane or popup whose process gets stopped (`kill -STOP`) is sent SIGCONT and goes on, as in tmux, instead of staying frozen. A live upgrade loses nothing in flight: a paste a busy pane has not read yet arrives whole, a client's half-read command finishes, a pane's directory with a non-UTF-8 name stays its own, a long `DCS tmux;` passthrough ends whole, and a half-parsed escape sequence no longer runs its newline or backspace twice. `vtmux-upgrade ; more commands` is refused, naming them, rather than dropping them. `capture-pane -a` in a pane without history says `no alternate screen` once it has left it, as tmux does. Commands after `switch-client`, `new-window`, `select-window`, `select-pane`, `break-pane`, `join-pane` or `rotate-window` in a list (`a \; b`, a key binding) start where those moved to, as in tmux: `new-window -t other: \; send-keys ...` types into the new window. `session:.%N`, `session:@N`, `=+1` and `[[:digit:]]` targets resolve as tmux's do. A checkpoint written while `manifest` is torn keeps the good `manifest.prev` until the new `manifest` is in place, so a crash then still restores. `set -s vtmux-persist layout ; kill-server` leaves no screen or history on disk, as setting it on a running server does. A line a resize reflows keeps tmux's extended-cell flag (`capture-pane -F`'s `X`) after its text is overwritten. A checkpoint written while `manifest` is torn keeps the good `manifest.prev` until the new `manifest` is in place, so a crash then still restores. `set -s vtmux-persist layout ; kill-server` leaves no screen or history on disk, as setting it on a running server does. A line a resize reflows keeps tmux's extended-cell flag (`capture-pane -F`'s `X`) after its text is overwritten, and a live upgrade or restore keeps those flags as the pane had them.

**vim and less over a long history.** The alternate screen comes up in 3 ms over 1M lines of scrollback, as in tmux (was 4.1 s; 0.43 s over 100k). Searching that history in copy mode while it is up takes 0.57 s (was 4.8 s, tmux 1.7 s). A live upgrade with it up costs what one without does, and moving back onto an older build keeps the history it hides.

**Faster frames.** A key's echo takes 0.30 ms (was 0.38; tmux 0.24). Agent TUIs cost 0.76-0.92x tmux's server CPU at 5-20 panes with one or two clients (was up to 1.25x), and 5 clients on one window 1.4x (was 1.85x). A one-cell change composes one row, a pane's screen is read once per write whatever the number of clients, and a client's frame acknowledgement wakes no render. Frames are byte for byte what they were ([numbers](docs/PERFORMANCE.md)).

- The cursor hides under a floating pane, in a shown overlay scrollbar, and in a pending wrap at the window's right edge, as tmux's does; it no longer draws over a floating pane.
- `#{V:...}` loops over the session, global (`V/g`) and client (`V/c`) environment; it was always empty.
- `TZ` names under `posix/` and `right/` give their zone (and leap seconds), and `TZ=:` is UTC.
- A combining mark in the prompt's completion list takes no column; keys `0x110000`-`0x13FFFF` list as their bytes; a `t/f` format ending in `#` drops it; a `display -t` that does not resolve describes what tmux's does.

**Signals and terminals, as tmux.** Typing while a session is killed no longer leaves your terminal on the alternate screen. An attached client survives `SIGUSR1`, `SIGUSR2` and an outside `SIGTSTP`, and `tmux attach &` draws instead of stopping; `SIGTERM` ends `wait-for` or a `-C` client with status 0 (and `%exit`), `SIGINT` and `SIGQUIT` don't. Panes start with the control characters of the terminal that made the session (`stty intr`, `werase`), `VERASE` from the `backspace` option. `-l` survives a live upgrade.

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

**crates.io ready.** All eight crates package and build from their `.crate` files alone, published as 3.8.0-beta.1: `cargo install --locked vtmux@3.8.0-beta.1` (a beta: cargo needs the version named; Zig 0.17 on `PATH`). The patched libghostty-rs ships as `vtmux-ghostty-vt-sys` and `vtmux-ghostty-vt`, whose build downloads the pinned Ghostty and checks its hash (`GHOSTTY_SOURCE_TARBALL` for offline builds), and stops with what to install when Zig is missing or the wrong version. How to release: [docs/RELEASING.md](docs/RELEASING.md).

**In-band resize (opt-in).** `set -s vtmux-inband-resize on`: programs that ask for mode 2048 (neovim) get every new size in-band, no `SIGWINCH` race. Live upgrades keep the grant, and a downgrade that would drop it is refused.

Every opt-in is tmux 3.8, byte for byte, while off.

**Built for speed.** Whole-program LTO: 7-18% less server CPU on render-bound work, key echo 12% faster, a 9.8 MB stripped binary (was 10.7).

**Lean where it counts** ([numbers](docs/PERFORMANCE.md)):

- 1M lines of scrollback settle at 103 MB vs tmux's 622 MB; 20 panes streaming at a 100k limit at 217 MB vs 1.25 GB.
- Pane output at 0.37-0.62x tmux's server CPU; 8 attached clients at 0.06-0.19x; an idle server with a real config at 0.28x.
- Searching or capturing all of a 1M-line history at 0.2-0.55x tmux's time.

**What it costs:** an idle server uses 9.0 MB vs 4.2 MB and each idle pane 18 KB vs 5.4 KB; a key's echo takes 0.38 ms vs 0.26 ms; a few agent TUIs with two clients cost up to 1.25x tmux's CPU; vim or less opening over a 1M-line history holds the server for 2-4 s.

Real tmux bugs fixed by default are in [compat/DIVERGENCES.md](compat/DIVERGENCES.md); opt-in fixes and options in [docs/PAPERCUTS.md](docs/PAPERCUTS.md).
