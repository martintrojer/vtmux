# Changelog

vtmux's version is the tmux release it is tested against: `3.8-rc1` is the first release candidate of vtmux for tmux 3.8. `tmux -V` still prints `tmux 3.8`, so plugins' version checks see tmux; `vtmux -V` prints vtmux's own version.

## 3.8-rc1

### New

- **Static Linux builds.** The release adds `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` tarballs: one binary, no shared libraries, so vtmux runs on a glibc older than the gnu build's 2.35 (EL9, Debian 11) and on Alpine. mise keeps the gnu build on glibc hosts; `github:martintrojer/vtmux[matching=musl]` picks the static one. A live upgrade between the two works both ways. OS error messages read as musl's (`I/O error`, not `Input/output error`), and musl has no NSS: with sssd or LDAP users and no nscd, user names may not resolve.

### Faster

- **Commands piped into `-C`.** 20,000 lines take 118 ms (was 415 ms; tmux 180 ms), 161 ms with formats in them (was 484; tmux 282).
  - A command client's answer while a script pipes commands: 6.5 ms (tmux 19 ms).

### Fixed

- Linked as `tmux`, a vtmux client that meets a running tmux 3.8 server on tmux's socket says so (`not a vtmux server: <socket> belongs to a tmux server`), with the ways out, and exits 1 at once, whatever the size of its environment. It printed `server exited unexpectedly`, which read as lost sessions, or hung (a small environment). The tmux server is untouched, as is a vtmux server that tmux's client reaches: that client exits 1 with `server exited unexpectedly`, and on macOS the vtmux server no longer keeps the terminal or output the client passes it, so a script reading that output no longer waits for ever.
- **A blinking cursor on Ghostty.** The cursor no longer starts blinking after a live upgrade, after a program resets the cursor style (`ESC[0 q`, which Ghostty draws blinking), or while a program's synchronized update sets a blinking cursor and steadies it again. Each frame shows the cursor as tmux shows it after a redraw (`cnorm`, then the style), the reset after an upgrade goes before `cnorm`, and a pane in synchronized output leaves the terminal's cursor as it is, as in tmux.
- **Mouse in copy mode with `mouse off`.** A pane in copy mode, a tree or the clock no longer keeps the terminal reporting the mouse its program asked for, so a drag selects in the terminal, as in tmux; with `mouse on`, such a pane no longer holds the terminal in all-motion mode.
- Live upgrade: typing (or pasting) into an attached client while the server upgrades no longer loses input. A client flooded with input read its terminal until it ran dry and so never saw the upgrade notice; the new server gave up on it after 10 s and dropped the rest of what was typed. The client now reads the server between reads of the terminal, and keeps the terminal raw across its re-exec, so keys typed in those milliseconds are no longer cooked by the terminal (Enter as a newline, Ctrl-C, Ctrl-U). Upgrading onto a build from before this fix still gives the terminal its modes back before the exec.
- Live upgrade: a SIGTERM or SIGHUP that reaches an attached client as it re-executes ends it (`[terminated]`, `[lost tty]`). One arriving in the microseconds between reading the signals caught so far and the exec was lost, and the client stayed attached.
- Live upgrade: a terminal resized while its client re-executes is started again at its new height. With `clear-on-attach off`, the scroll into history and the scroll region used the old height.
- Live upgrade: the server serves on while a downgrade waits for the old binary's `-V`. Every client froze for up to 5 s when that binary hung.
- `vtmux-upgrade` without `-b`, once the binary the server was started as is deleted (a version manager pruned it), says to name the new one with `-b`.
- A terminal whose reply adds capabilities its `TERM` lacks (Ghostty or foot under `TERM=vt100`) gets the mouse mode, cursor style and cursor colour at once, as in tmux, not only once a pane changes them.
- A soft-wrapped line erased in part with ECH (`printf '\033[X'`) stays one line, as in tmux: `capture-pane -J` joins it, `-F` flags it `W`, and a wider pane shows it whole. Only an ECH over the whole line from column 0 unwraps it.
- `capture-pane -J` (and `-T -N`) keeps a line's cells that were written then erased, as tmux does, also with `-a` (the screen under the alternate one) and `-M` (copy mode's copy): tmux-resurrect's saved pane contents (`capture-pane -epJ`) now match tmux's, vim's screen included. tmux-resurrect's save and restore and tmux-continuum's auto-save and auto-restore are tested against tmux 3.8, and a resurrect save made under either restores under the other.
- A client that goes away as it attaches (killed, or a SIGTERM as the attach completes) no longer crashes the server, and `client-detached` fires for it, as in tmux; before, on a loaded host, the hook was skipped, or the server died with every session.
- `#{client_uid}` and `#{client_user}` name the client's own user, not the server's, as in tmux: `list-clients` shows `[user …]` for a client another user attached through `server-access`.
- Live upgrade: a downgrade to 3.8-beta2 or 3.8-beta1 waits up to 5 s for the old binary's `-V`, not 1 s, so a loaded laptop or macOS checking the binary no longer gets it refused.
- Commands answer while an attached client floods keys (a huge paste, a terminal that sends input without end): 13 ms in a release build, where tmux takes 24 ms. Before, each waited 1.8 s (4-6 s in a debug build), and a plugin's `tmux` calls stalled with it. The server reads a client's input 4 KB a loop turn, as tmux does, and writes keys to a pane in batches.
- A line erased with a background colour and then narrowed keeps its coloured cells past the narrow width, as tmux does: widened again, the pane and `capture-pane -e` show them (`printf '\033[41m\033[K'`, 40 -> 30 -> 40 columns: red to column 40, not 30). They go into history with the line and through a live upgrade. DCH over a whole line (`\033[1;1H\033[<width>P`) empties it and unwraps the line above, as tmux does.

### Removed

- **Save and restore.** vtmux no longer checkpoints sessions to disk or restores them at start: restoring after a crash or reboot is tmux-resurrect's and tmux-continuum's job, and they work on vtmux as on tmux. vtmux keeps what no plugin can do: live upgrades, the terminal features tmux drops, pane blend.
  - `vtmux-persist`, `vtmux-persist-respawn` and `vtmux-persist-drop` are gone. A config that sets one now gets `invalid option`, inside `%if "#{vtmux_version}"` too (that guard is for real tmux); delete those lines.
  - The old checkpoints in `$XDG_STATE_HOME/vtmux` (default `~/.local/state/vtmux`) are never read again; delete the directory.
  - Live upgrades from and back to 3.8-beta3 keep working. A pane a beta3 server restored and still shows comes across as an ordinary dead pane with its screen; `respawn-pane` restarts it.

### Changed

- The engine is the libghostty-vt 1.0.1 release (tag `vt-1.0.1`), not a Ghostty commit: the build downloads its source and Zig packages from release.files.ghostty.org, checks their SHA-256 and builds with `zig build --system`, so Zig fetches nothing. Offline, set `GHOSTTY_SOURCE_TARBALL` and `GHOSTTY_DEPS_TARBALL` to local copies. 43 vtmux patches on top; two of the old ones are in 1.0.1 already. Behaviour and live upgrade both ways with 3.8-beta3 are unchanged; agent panes and idle panes cost the same, and SGR-heavy output ran faster (86 against 64 MB/s, one run on a shared host).

## 3.8-beta3

### New

- **Restores you can pick up.** After a restart a restored pane shows its last screen with a hint over its bottom row; press Enter and its command starts again below that screen (a dim `restored <time>` row between), editors' screens included. `respawn-pane` does the same.
  - `set -s vtmux-persist-respawn auto` restarts, after the config, every restored pane that ran your default shell; panes started with a command (`sh` and `bash -c …` included) wait for Enter. `off` keeps panes dead until `respawn-pane`.
  - `send-keys` (but `send-keys -K`, a client key), pastes and synchronized panes never restart one.
  - A restarted command that exits before anything is typed into it (a program gone since the checkpoint, a missing shell) no longer closes the pane and its saved screen, however long the server takes to see the exit: it stays restored, the hint saying the restart failed with its status, and Enter tries again. Once you type into it, its exit is tmux's.
  - An editor's screen kept by a restart is copied cell for cell: line drawing, coloured blank cells, links (across lines, with their ids) and wrapped lines.
  - On a narrow pane the hint drops the time, then the failure, before `Enter restarts`; a failed restart's status is not carried into the next restart.
  - With `vtmux-persist-respawn off`, an empty `remain-on-exit-format` draws nothing over a restored pane's last row, as tmux draws no dead line.
  - A checkpoint holds a restored pane still waiting as one that ran, so an older vtmux restoring it draws its dead line.
  - The restored dead line is no longer written into the pane's screen: `capture-pane` and the next checkpoint show the saved screen as it was.

### Fixed

- Live upgrade: `vtmux-upgrade -b` onto a program that is not a vtmux (`/bin/echo`, `true`, a script that exits 0) is refused with exit 1, the server and its panes untouched; before, it passed the dry run and replaced the server, losing every session. The new binary must answer the dry run with the digest of the snapshot it read; a downgrade to 3.8-beta1 or 3.8-beta2, which predate that, is still accepted when its `-V` says so.
- `set -s vtmux-persist off` at run time is remembered: the next start restores nothing unless its config turns persistence on again, and the checkpoint files stay.
- Live upgrade: a bracketed paste typed right after a key bound to `vtmux-upgrade` no longer refuses it; the paste lands in the new image as a paste.
- A client that vanishes while a live upgrade tells clients to re-attach no longer fails the upgrade with `Bad file descriptor`.
- `SIGTERM` or `SIGHUP` as a client attaches says `[terminated]` or `[lost tty]`, not `server exited unexpectedly`, and detaches as tmux does (the pane gets its focus-out). One that arrives as a command or control client starts no longer leaves it blocked. One that arrives as an attached client follows a live upgrade ends it the same way instead of being lost (the client stayed attached), and one that lands just before the client is attached ends it quietly with status 0, as tmux does.
- `tmux` run in a pane with `TMUX_PANE` unset (a cleaned environment) finds its pane by its tty, as tmux does, instead of acting on the newest session.
- `break-pane -a`/`-b` that can't shuffle the windows is an error: the rest of the command list doesn't run and `command-error` fires.
- `copy-mode -S` on another pane makes it active without firing `window-pane-changed`.
- `#{<:...}`, `#{>:...}`, `#{<=:...}` and `#{>=:...}` order bytes that are not UTF-8 as tmux does.
- A detach during a flood no longer leaves your terminal frozen in a synchronized update.
- Esc then a key that a busy server reads after `escape-time` is M-x, as in tmux, not Esc and the key.
- Live upgrade: a failed exec keeps an open editor's file; keys typed right after a key-bound `vtmux-upgrade` keep their order and stay the keys they were (Esc then `x` is no longer read as M-x), an Esc still waiting out `escape-time` keeps its wait, and a paste the upgrade lands in stays a paste; an upgrade refused for a program's kitty graphics says which pane and what to do.
- Kitty graphics: a placement under the text keeps wide characters whole, and the cursor after a full line sits on the pane's border as without images.
- On Linux, a job or popup command started with no `PATH` (a server started without one, and no `tmux` shim) searches what glibc's `execvp` does (`getconf PATH`: `/usr/bin` on EL9), not `/usr/bin:/bin`.
- On macOS and FreeBSD, an empty `PATH` entry runs `./name`, as their `execvp` does, so a script sees that as its `$0`.
- A server started with no `PATH` gives its panes, jobs and popups `/usr/bin:/bin` after the `tmux` shim, as tmux gives panes, not `/usr/bin:/bin:/usr/sbin:/sbin`.

## 3.8-beta2

### New

- **Kitty graphics (opt-in).** `set -s vtmux-graphics on`.
  - Programs that send kitty graphics unwrapped get them shown on Ghostty clients, with no `allow-passthrough`.
  - Images are drawn as unicode placeholder cells, so pane edges, popups, zoom, window switches and copy mode treat them as text.
  - Each client gets its own uploads, freed when nothing shows them.
  - All panes' images, half-sent ones included, share one cap (`vtmux-graphics-limit`, 256 MB). Decoding never costs more than the screen's share: a 300-byte PNG naming 9000x9000 pixels is refused before allocating.
  - Images only come inside the escape. File, temporary-file and shared-memory transmissions are refused, so a program, local or over ssh, never makes the server open a path.
  - Other terminals and control clients see tmux. Apps that wrap for tmux (yazi) still use passthrough.
  - Live upgrades keep the grant and drop the images.
- **More terminals.** Most of ncurses 6.6's terminal database attaches with the features tmux 3.8 gives it, on hosts without terminfo for it: 2,663 names, including `wezterm`, `contour`, `foot`, `rio`, `ansi` and `Eterm`.
  - Hardcopy terminals are refused with tmux's message.
  - 163 8-bit terminals (`vt220-8bit`, `ansi.sys`) are refused where tmux takes them.
  - The table adds 0.3 MB to the binary; with kitty graphics it is 10.4 MB, up from 9.8 MB.
- **Pixel sizes.** A pane reports its size in pixels from your terminal's cell size (`ws_xpixel`, `CSI 14 t`, `#{window_cell_width}`), so image tools size their output. 16x32 per cell until a client gives one.
- **`#{pane_last_output_time}`**, as tmux sets it.

### Faster

- **vim and less over a long history.** The alternate screen comes up in 3 ms over 1M lines, as in tmux (was 4.1 s).
  - Copy-mode search of that history while it is up: 0.57 s (was 4.8 s, tmux 1.7 s).
  - A live upgrade with it up costs what one without does.
- **Frames.**
  - Key echo 0.30 ms (was 0.38; tmux 0.24).
  - Agent TUIs at 0.76-0.92x tmux's server CPU at 5-20 panes with one or two clients (was up to 1.25x).
  - 5 clients on one window at 1.4x (was 1.85x).
  - Frames are byte for byte what they were ([numbers](docs/PERFORMANCE.md)).
- **Idle full persistence.** An idle server with `vtmux-persist full` no longer exports every pane each minute: 8 ms at 10 panes x 100k lines, where keys used to freeze for 1.7 s.

### Live upgrade and persistence

- Nothing in flight is lost: a paste a busy pane hasn't read yet, a client's half-read command, a long `DCS tmux;` passthrough, a half-parsed escape sequence, a non-UTF-8 directory name.
- With `vtmux-notifications on`, a client re-attaching while a pane sent a notification no longer crashes the server.
- `vtmux-upgrade ; more commands` is refused, naming them, instead of dropping them.
- A key bound to `vtmux-upgrade` is no longer refused by keys typed right after it.
- A checkpoint written while `manifest` is torn keeps the good `manifest.prev`, so a crash then still restores.
- `set -s vtmux-persist layout ; kill-server` leaves no screen or history on disk.
- The first checkpoint after a live upgrade reads the previous one back in steps of at most 10 ms (was one 330-360 ms turn). A damaged chunk is written again, not reused.

### Closer to tmux

- **Signals and terminals.**
  - Typing while a session is killed no longer leaves your terminal on the alternate screen.
  - An attached client survives `SIGUSR1`, `SIGUSR2` and an outside `SIGTSTP`; `tmux attach &` draws instead of stopping.
  - `SIGTERM` ends `wait-for` or a `-C` client with status 0 (and `%exit`); `SIGINT` and `SIGQUIT` don't.
  - A client whose terminal hangs up exits 1 quietly, as tmux does (was a panic, status 101).
  - A client detached while suspended no longer clears the shell's screen when it comes back.
  - Panes start with the control characters of the terminal that made the session (`stty intr`, `werase`), `VERASE` from `backspace`.
  - The client sets raw mode as tmux does. `-l` survives a live upgrade.
- **Commands and targets.**
  - Commands after `switch-client`, `new-window`, `select-*`, `break-pane`, `join-pane` or `rotate-window` in a list start where those moved to: `new-window -t other: \; send-keys ...` types into the new window.
  - `session:.%N`, `session:@N`, `=+1` and `[[:digit:]]` targets resolve as tmux's do.
  - `break-pane`'s `pane-moved` reports the old index; a bad `resize-pane` fires no layout hook; `wait-for -E` survives a waiter leaving mid-wake; a reaped pid credits every pane that has it.
  - `rename-window`, `rename-session`, `select-pane -T`, the `-P` and `-n`/`-c` of the pane-starting commands read the client formats tmux gives them.
  - `new-window -c`, `new-session -c`, `#{session_path}` and the command line's `-S`, `-f`, `-L`, `-c` take non-UTF-8 paths; a control client's `new-window` starts in its session's directory.
- **Panes and formats.**
  - A split or respawned pane's window is named after its command, not after the server binary caught before it execs.
  - A stopped pane or popup process (`kill -STOP`) gets SIGCONT, as in tmux.
  - An OSC a program never ends is dropped past `input-buffer-size` (1 MiB), as in tmux: 12 panes that got 8 MiB each held 147 MB, now 49 MB (tmux 165 MB).
  - An APC, PM or SOS holding a UTF-8 character with byte 0x9c ("本") no longer ends early.
  - `capture-pane -a` in a pane without history says `no alternate screen` once it has left it.
  - A line a resize reflows keeps tmux's extended-cell flag (`capture-pane -F`'s `X`), across live upgrade and restore too.
  - The cursor hides under a floating pane, in a shown overlay scrollbar and in a pending wrap at the right edge.
  - `#{V:...}` loops over the session, global (`V/g`) and client (`V/c`) environment; it was always empty.
  - `TZ` names under `posix/` and `right/` give their zone, and `TZ=:` is UTC.
  - Smaller: a combining mark in prompt completion takes no column; keys past U+10FFFF list as bytes; a `t/f` format ending in `#` drops it; long-lived servers no longer run out of job or client token numbers.
- **Library safety.** The published Ghostty wrapper lets another thread change Unicode options while terminals print (lock-free, at full speed), rejects out-of-range grid reads, and no longer keeps dangling allocator pointers.

## 3.8-beta1

First release. Linux (x86_64, aarch64) and macOS 15+ (Apple silicon).

### Highlights

- **Drop-in tmux 3.8.** Every tmux 3.8 command, option, format, hook and control-mode message. Your `.tmux.conf`, plugins and scripts work unchanged.
  - All 415 behaviour-oracle scenarios recorded from real tmux 3.8 pass.
  - 173 of tmux's own 174 regress tests pass (1 expected failure, recorded).
- **Ghostty's emulator.** Each pane is a [libghostty-vt](https://github.com/ghostty-org/ghostty) terminal, with tmux's quirks kept where scripts can see them.
- **Better rendering.** One frame per client, only what changed, inside synchronized output, the cursor placed once: no cursor jumping under heavy output. A redraw-heavy benchmark sends 0.07 MB to your terminal where tmux sends 10.1 MB, at 0.3x the CPU.
- **Live upgrade.** `tmux vtmux-upgrade -b <new binary>` swaps the binary under a running server. Shells, editors and builds keep running; clients stay attached; scrollback comes along. 10 panes x 100k lines upgrade in 0.6-0.9 s.

### Opt-in extras

Every opt-in is tmux 3.8, byte for byte, while off.

- **Persistence.** `set -s vtmux-persist layout` (or `full`, with screens and scrollback). After a crash or reboot the server comes back with its sessions, windows and layout, panes dead until `respawn-pane`.
- **Pane blend.** `vtmux-pane-inactive-blend` dims inactive panes like Ghostty's unfocused splits.
- **Kitty keyboard protocol.** `set -s vtmux-kitty-keys on`: programs that ask (neovim, helix, agent TUIs) get Shift+Enter, C-i apart from Tab, Escape apart from Alt.
  - Key tables and the prefix work as before; programs that don't ask see tmux.
  - A grant ends with the program that asked, so a killed editor never leaves the shell getting `CSI 99;5u` for C-c. A live upgrade keeps it.
  - Exception: a program sharing a surviving shell's process group (`prog; exec bash`) keeps it until `reset`.
- **Desktop notifications.** `set -s vtmux-notifications on`: a pane's OSC 9 and OSC 777 notifications reach attached Ghostty clients from any window, rate-limited as Ghostty limits itself.
- **In-band resize.** `set -s vtmux-inband-resize on`: programs that ask for mode 2048 (neovim) get every new size in-band, with no `SIGWINCH` race. A downgrade that would drop the grant is refused.

### Packaging

- **crates.io.** `cargo install --locked vtmux@3.8.0-beta.1` (a beta: cargo needs the version named; Zig 0.17 on `PATH`).
  - The patched libghostty-rs ships as `vtmux-ghostty-vt-sys` and `vtmux-ghostty-vt`. Its build downloads the pinned Ghostty and checks the hash (`GHOSTTY_SOURCE_TARBALL` for offline builds).
  - How to release: [docs/RELEASING.md](docs/RELEASING.md).
- **Built for speed.** Whole-program LTO: 7-18% less server CPU on render-bound work, key echo 12% faster, a 9.8 MB stripped binary (was 10.7).

### Numbers

[Details](docs/PERFORMANCE.md).

- 1M lines of scrollback settle at 103 MB vs tmux's 622 MB; 20 panes streaming at a 100k limit at 217 MB vs 1.25 GB.
- Pane output at 0.37-0.62x tmux's server CPU; 8 attached clients at 0.06-0.19x; an idle server with a real config at 0.28x.
- Searching or capturing all of a 1M-line history at 0.2-0.55x tmux's time.

What it costs:

- An idle server uses 9.0 MB vs 4.2 MB, and each idle pane 18 KB vs 5.4 KB.
- A key's echo takes 0.38 ms vs 0.26 ms.
- A few agent TUIs with two clients cost up to 1.25x tmux's CPU.
- vim or less opening over a 1M-line history holds the server for 2-4 s.

Real tmux bugs fixed by default are in [compat/DIVERGENCES.md](compat/DIVERGENCES.md); opt-in fixes and options in [docs/PAPERCUTS.md](docs/PAPERCUTS.md).
