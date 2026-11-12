# How vtmux works

vtmux has tmux's process model. A server owns every session, window, pane and shell. Each `vtmux` (or `tmux`) invocation is a short-lived client that talks to it over a Unix socket; an attached client also owns your real terminal.

Each pane's screen lives in its own libghostty-vt terminal inside the server, where tmux has `grid.c` and `input.c`. Everything a script can read is still shaped like tmux. The decisions behind this (D1-D31, D33) are indexed in [docs/README.md](README.md).

## The big picture

```
 ┌──────────────────────────────────────┐
 │ Terminal emulator (Ghostty, foot, …) │  draws pixels
 └───────────────┬──────────────────────┘
                 │ the terminal's pty
                 │   ↑ keys, mouse, terminal replies
                 │   ↓ escape sequences for the finished screen
 ┌───────────────┴──────────────────────┐
 │ client (crates/vtmux-client)         │  raw tty; a pipe:
 │  stdin → Input / Resize              │   forwards input bytes,
 │  Frame → stdout, then FrameAck       │   writes frames as they come
 └───────────────┬──────────────────────┘
                 │ Unix socket $TMUX_TMPDIR/vtmux-UID/<label>
                 │   (tmux-UID when run as tmux, D20)
                 │ vtmux's own framed protocol (vtmux-core::protocol)
 ┌───────────────┴────────────────────────────────────────────────────┐
 │ server (crates/vtmux-server)  one thread, mio event loop           │
 │                                                                    │
 │  input.rs: decode keys ─► key tables (prefix/root) ─► command      │
 │                        └► encode for the active pane ─► its pty    │
 │  model: sessions ─► windows (Layout) ─► panes                      │
 │  options · formats · targets · hooks (vtmux-core, pure)            │
 │                                                                    │
 │  render/ compositor, once per tick, per client:                    │
 │    panes + borders + status + popups + menus → one screen          │
 │    → diff against the client's last frame → encode for its         │
 │      terminal, inside ?2026 → Frame (held while the client lags)   │
 │                        ▲ reads the grids                           │
 │  ┌─ pane %0 ───────────┴──────┐   ┌─ pane %1 ──────────────────┐   │
 │  │ PaneTerm (vtmux-vt)        │   │ PaneTerm                   │ … │
 │  │  libghostty-vt Terminal    │   │                            │   │
 │  │  grid + scrollback         │   │                            │   │
 │  │  answers queries as tmux   │   │                            │   │
 │  └──────┬─────────────────────┘   └──────┬─────────────────────┘   │
 └─────────┼────────────────────────────────┼─────────────────────────┘
           │ pty                            │ pty
      ┌────┴─────────┐                 ┌────┴─────────┐
      │ shell / nvim │                 │ shell        │   TERM=tmux-256color
      └──────────────┘                 └──────────────┘   TMUX=<socket>,<pid>,<session>
```

## The crates

| Crate | Job |
|---|---|
| `vtmux-core` | Pure tmux semantics, no I/O: command parser and table, targets, formats (with a POSIX regex engine), options, styles, keys, layouts, socket paths, the client/server protocol, tmux's exact error texts. |
| `vtmux-vt` | The FFI crate: a safe wrapper over libghostty-vt that shapes Ghostty's terminal as tmux (`capture-pane`, query replies, cursor and history). Exports and imports a pane's whole terminal state for live upgrade, and refuses a malformed one. |
| `vtmux-server` | The event loop and everything stateful: model, ptys and processes, commands, control mode, compositor, live upgrade. OS calls live in `sys/`. |
| `vtmux-client` | The attached client: raw mode, input, frames, resize, signals, restoring the terminal on exit. |
| `vtmux-snapshot` | The live-upgrade format: versioned, tagged records, its own types so a server refactor can't change it. |
| `vtmux` | The binary: global flags, connect-or-start, then a command, attach or control mode. |
| `xtask` | Test tooling: the reference tmux build, the regress runner, the behaviour oracle, socket reaping. |

Under `vtmux-vt` sit `vtmux-ghostty-vt` and `vtmux-ghostty-vt-sys` (`third_party/libghostty-rs`, outside the workspace): libghostty-rs's `libghostty-vt` and `libghostty-vt-sys`, forked, published under vtmux's names. The sys crate's build script downloads the pinned libghostty-vt release and its Zig packages, checks their SHA-256, applies vtmux's patches and builds it with Zig, which fetches nothing (`VENDOR.md` there). Every crate but `xtask` is published to crates.io, in dependency order ([RELEASING.md](RELEASING.md)).

`unsafe` is allowed only in `vtmux-vt` and `vtmux-server/src/sys/`. Every other crate is `#![forbid(unsafe_code)]` (D10).

## Output: a program prints

1. The server reads the pane's pty and feeds the bytes to its libghostty-vt terminal. A query (DA, cursor position) is answered in tmux's words, not Ghostty's.
2. Once per loop tick, the compositor (`render/`) builds each attached client's screen: panes at their layout cells, borders, status line, menus and popups. Each pane keeps one Ghostty render state that re-reads only dirty rows, once per write however many clients and views look, and each client caches its composed rows, so a one-cell change recomposes one row. A tick where nothing changed builds no frame, and a tick that only takes clients' frame acknowledgements does not render at all.
3. The screen is diffed against the client's last frame, only between the first and last changed cell of each changed row, and only the changed cells are encoded for that client's terminal (`terminal-features`, D8, with its terminfo entry from a generated table: vtmux reads no terminfo), inside synchronized output, with the cursor placed once at the end.
4. The client writes the frame and acknowledges it.

Steps 2 and 3 are why the cursor doesn't jump under heavy output (D14): tmux forwards each pane update as it happens, vtmux only sends finished pictures. Pane blend (D26, `render/blend.rs`) works for the same reason: the compositor can change every cell's colour before encoding.

**Shared frames.** Clients that show the same window at the same size, view and terminal share the drawing and the frame bytes within a tick (`render/share.rs`). The time and each pane's foreground process are read once per tick (`live.rs`) so every client sees the same; a drawing that reads its own client (`#{client_prefix}`) is not shared. Each client still gets the bytes it would get alone, and debug builds redraw every shared frame to check.

**Backpressure.** A client with an unwritten frame gets no new one; its acknowledgement brings the next, with everything that changed meanwhile (tmux's `server_client_check_redraw`). A flooding pane's frames go at most every 10 ms, but a key's echo goes at once. A stalled client gets one full frame when it catches up (tmux's `tty_block_maybe`). A pane in synchronized output (`?2026h`) is drawn from the screen it showed when the mode began, for at most a second (tmux's `MODE_SYNC`).

## Input: you press a key

1. The client forwards the bytes. The server decodes them into keys as tmux's `tty-keys.c` does, and swallows the terminal's replies to vtmux's own queries.
2. A mouse report is located (`mouse.rs`: pane, border, status range) and becomes a key such as `MouseDown1Pane`.
3. A key with a binding runs its command; any other is encoded for the active pane's modes and written to its pty. A program granted the kitty keyboard protocol (D29) gets libghostty's encoding instead (`pane_kitty.rs` ends the grant when the program that asked exits).

## Commands and control mode

A command client connects (starting the server if needed and allowed), sends `Hello` with argv, cwd, environment and tty, and gets `Stdout`, `Stderr` and `Exit` back. Those match tmux 3.8 byte for byte, exit codes and errors included: the edge scripts depend on.

Commands run on per-client queues (`cmdq.rs`, after `cmd-queue.c`), plus a global one for the config and `run-shell -b`. `wait-for`, file I/O, `run-shell` and `if-shell` hold their queue without blocking the loop. Jobs and `#()` formats are `/bin/sh -c` children read through the same loop (`jobs.rs`, `format_jobs.rs`).

Inside panes, plugins call plain `tmux`. The server puts a private directory with `tmux` → its own binary first on every pane's `PATH` (`shim.rs`, D5), so they reach this server whatever `tmux` is installed.

Control clients (`-C`) get tmux's `%begin`/`%end`, `%output` and notifications instead of frames, with tmux's backpressure (`control_queue.rs`, after `control.c`): `pause-after` pauses a pane whose oldest unsent output is too old, a client 300 s behind is dropped, and a pane is not read while every attached client is a control client that is behind.

## Live upgrade

`vtmux-upgrade [-b binary]` (D27, [live-upgrade spec](specs/2026-11-06-live-upgrade.md)) replaces the server's image with `execve`, so the pid stays: pane processes remain its children and every descriptor it keeps stays open.

```
 old image (pid P)                                          new image (pid P)
 wait for jobs and commands in flight (≤ 3 s)
 ask B --vtmux-patch-stack; same Ghostty stack?
   yes: compress all history first (threads)
 snapshot ─► memfd ─► B --vtmux-check-snapshot  (dry run: decode + rebuild,
 serve on meanwhile                             at most 3 s + 1 s per 100k rows;
                                                passes only if B answers the
                                                snapshot's digest on stderr,
                                                or B -V is 3.8-beta1/beta2)
 passed: quiesce, flush clients, snapshot again
 UpgradeNotice + token ─► attached clients
 execve(B --vtmux-resume <fd>) ──────────────────────────► decode, adopt the fds,
   listener, client sockets, ptys, pipe-pane fds stay open    rebuild, reap, run
 attached client: restores its terminal, execs
   B --vtmux-reattach <socket> ───────────────────────────► rebound to its record
 control clients: stay connected (frozen frames)            answer the requester
```

- **Frozen frames.** A few protocol frames never change (`Hello`, `Stdin`, `StdinEof`, `Stdout`, `Stderr`, `Exit`, `Exec`, `Flags`, and `UpgradeRequest`/`UpgradeResult`/`UpgradeNotice`), so control clients of any version from protocol 9 on stay served and a newer client can ask an older server to upgrade. Every other frame may change with a protocol bump.
- **Failure never loses panes.** A failed dry run leaves the server as it was. A resume that fails switches back to the old image on the same descriptors (Linux: `fexecve` of the executable it kept open). Failing that, it serves without the parts that don't read, then without terminal state, then with no sessions while it holds every pty open. Exit status: 0 upgraded, 1 not, 2 upgraded short of a part.
- **History fast path.** With the same Ghostty patch stack, compressed history pages travel as Ghostty's own bytes and nothing is decoded; across stacks they go as vtmux rows. Across stacks an alternate screen's hidden history also goes as the copy a build without Ghostty patch 0038 reads, mostly the primary's own pages, which the dry run reads once. What an upgrade closes by design (tree modes, prompts, menus) is listed in `server/snapshot.rs`.

## Terminal protocols tmux drops (D28)

vtmux passes through a protocol tmux 3.8 drops only when libghostty-vt implements it in panes and Ghostty as the outer terminal. Each is a server option, off by default and tmux byte for byte when off, and only a program that asks sees it. A grant outlives the option being turned off and survives a live upgrade.

| | Option | Where |
|---|---|---|
| Kitty keyboard (D29) | `vtmux-kitty-keys` | Ghostty's key encoder via `vtmux_vt::kitty_keys`; grants in `pane_kitty.rs` |
| Desktop notifications, OSC 9 and 777 (D30) | `vtmux-notifications` | `notify.rs`: to Ghostty clients only, at Ghostty's rate limits |
| In-band resize, mode 2048 (D31) | `vtmux-inband-resize` | `resize.rs` |
| Kitty graphics (D33) | `vtmux-graphics`, `vtmux-graphics-limit` | Ghostty's image storage per granted screen, images sent inline only, `pane_graphics.rs` (grants, the shared limit over images and loads in progress); drawn as unicode placeholder cells for Ghostty clients, each with its own uploads, in `render/graphics.rs` |

## Ghostty inside

libghostty-vt runs inside the server, once per pane: the "grid follows Ghostty" half of D4. Where the two emulators differ, the difference is recorded with evidence in [compat/DIVERGENCES.md](../compat/DIVERGENCES.md).

It is vendored in `third_party/libghostty-rs`, pinned to the libghostty-vt 1.0.1 release (tag `vt-1.0.1`; releases are tracked by tag, never by commit), with 40 local patches applied at build time (`libghostty-vt-sys/patches`, each explained in `VENDOR.md`):

- **tmux's grid shape:** wrap and pending-wrap bookkeeping, cells used, reflow as tmux does it, the alternate screen, clusters and widths, hyperlink ids, line flags, DECAWM, DECALN.
- **Memory:** bounded, resumable page compression and resize without faulting blank pages; grid-reference bounds checked before native reads.
- **Library safety:** process-wide Unicode options are one atomic pointer to an immutable width table plus one atomic flag byte, read once per print call, so another thread may set them while a terminal prints, with no lock on the print path.
- **Live upgrade:** whole-state export and import, page by page.
- **Speed:** cloning a screen and whole pages as memory, stepping row to row instead of walking the page list, reading the primary history in place while the alternate screen is up.
- **Kitty graphics (D33):** image storage per granted screen, a pane's storage limit and its unfinished loads held to it, a decode capped at the screen's limit.

A build reports its patch stack (`--vtmux-patch-stack`); live upgrade uses it to decide whether compressed pages can travel as they are.

Quiet scrollback is compressed in place (`compress.rs`): five seconds after a pane's history last changed, one bounded step at a time across panes, only when the loop is idle. A pane that streams for 10 s is compressed in small steps at the end of busy turns too. Reading compressed history restores it transparently.

If your terminal is Ghostty, the finished picture is parsed again by Ghostty's own libghostty. That is the price of a drop-in tmux that works in any terminal and over SSH.

## Compatibility is at the edge

Inside, vtmux is ordinary Rust: IDs are newtypes, values are enums, windows hold a typed layout tree, state lives in ID-keyed arenas. Errors are typed until they reach the edge, where they become tmux's exact text. Where tmux's behaviour falls out of its data structures (sessions listed by name, IDs never reused, hook order), vtmux reproduces it explicitly and a test pins it.

## How it's tested

```
        tmux 3.8 regress/              behaviour oracle               esctest / specs
     (what tmux means to do)      (what tmux 3.8 actually does)     (what a terminal should do)
               │                              │                              │
   cargo xtask regress              cargo xtask oracle record → traces    D4 arbiter for
   tmux's own 174 tests             cargo xtask oracle replay → vtmux     grid differences
   against the vtmux binary         (CLI output, -C streams, screens via
                                     a libghostty observer terminal)
```

- **Regress:** tmux's test suite from the 3.8 tag, unchanged, scored by `compat/expected.toml`. The gate fails when a test regresses and when one passes unmarked.
- **Oracle:** 444 scenarios recorded once against real tmux 3.8 and replayed against vtmux. Screens are compared as grids in an observer terminal, never as raw bytes. The replay runs again with a live upgrade after every third step.
- **Side by side:** the CLI, attach and control tests run each case against both binaries.
- **Plugins:** tmux-resurrect and tmux-continuum (vendored in `compat/tmux-plugins`) save, kill the server and restore on both binaries; what was saved and what came back must match.

`scripts/check.sh` runs all of it, on Linux and macOS in CI, where each of its stages is a job of its own. Numbers against tmux: [PERFORMANCE.md](PERFORMANCE.md).
