# How vtmux works

vtmux has tmux's process model: a **server** owns every session, window, pane and shell, and each `vtmux` invocation is a short-lived **client** that talks to it over a Unix socket. An attached client is a long-lived client that also owns your real terminal.

The server keeps each pane's screen in its own **libghostty-vt** terminal emulator. tmux does that job with its own `grid.c` and `input.c`.

The design decisions behind this (D1–D23) are in the [spec](specs/2026-10-29-vtmux.md). This page describes how the pieces fit.

## The big picture

```
 ┌──────────────────────────────────────┐
 │ Terminal emulator (Ghostty, foot, …) │  draws pixels
 │ (Ghostty has its own libghostty)     │
 └───────────────┬──────────────────────┘
                 │ PTY #1 (the terminal's own pty)
                 │   ↑ keys, mouse, terminal replies
                 │   ↓ escape sequences for the final screen
 ┌───────────────┴──────────────────────┐
 │ vtmux client (crates/vtmux-client)   │  raw tty mode; just a pipe:
 │  stdin → Input / Resize              │   forwards input bytes,
 │  Frame → stdout, then FrameAck       │   writes frames as they come
 └───────────────┬──────────────────────┘
                 │ Unix socket  $TMUX_TMPDIR/tmux-UID/<label>
                 │ vtmux's own framed protocol (vtmux-core::protocol)
                 │   Hello{argv,env,tty} · Input · Resize · FrameAck · ReadData · FileDone
                 │   Frame · Stdout · Stderr · Exit · ReadOpen · WriteOpen/Data/Close
 ┌───────────────┴────────────────────────────────────────────────────┐
 │ vtmux server (crates/vtmux-server)  one thread, mio event loop     │
 │                                                                    │
 │  input.rs: decode keys ─► key tables (prefix/root) ─► command      │
 │                        └► encode for the active pane ─► its PTY    │
 │                                                                    │
 │  model: sessions ─► windows (core Layout) ─► panes                 │
 │  options · formats · targets · hooks (vtmux-core, pure)            │
 │                                                                    │
 │  render.rs compositor, once per tick:                              │
 │    every pane's grid + borders + status line + popups              │
 │    → one screen per client → diff against the last frame           │
 │    → encode for the client's terminal (Caps), inside ?2026 sync    │
 │    → Frame (held back while the client's terminal is behind)       │
 │                        ▲ reads the grids                           │
 │  ┌─ pane %0 ───────────┴──────┐   ┌─ pane %1 ──────────────────┐   │
 │  │ PaneTerm (crates/vtmux-vt) │   │ PaneTerm                   │   │
 │  │  libghostty-vt Terminal    │   │  libghostty-vt Terminal    │ … │
 │  │  grid + scrollback         │   │  grid + scrollback         │   │
 │  │  answers queries (DA, CPR) │   │                            │   │
 │  │  the way tmux does         │   │                            │   │
 │  └──────┬─────────────────────┘   └──────┬─────────────────────┘   │
 └─────────┼────────────────────────────────┼─────────────────────────┘
           │ PTY #2                         │ PTY #3 …
      ┌────┴─────────┐                 ┌────┴─────────┐
      │ shell / nvim │                 │ shell        │   TERM=tmux-256color
      │ (pane %0)    │                 │ (pane %1)    │   TMUX=<socket>,<pid>,<session>
      └──────────────┘                 └──────────────┘
```

## The crates

| Crate | Job | Depends on |
|---|---|---|
| `vtmux-core` | Everything that's pure tmux semantics, with no I/O and no `unsafe`: the command parser and table, targets, the format engine (modifiers, loops, time, POSIX regex), options, styles and colours, keys and key tables, layouts, socket-path rules, the client/server protocol, and the exact error texts | nothing of ours |
| `vtmux-vt` | The only FFI crate: a safe wrapper over libghostty-vt. It turns Ghostty's terminal into tmux's shapes: `capture-pane` output, tmux's replies to terminal queries, cursor and history reporting | `libghostty-vt` |
| `vtmux-server` | The single-threaded event loop and everything stateful: sessions, windows and panes, PTYs and processes, commands, control mode, the compositor and frame encoder. OS calls (`fork`, `openpty`, signals) are confined to `sys/` | core, vt |
| `vtmux-client` | The attached client: raw tty mode, forwarding input, writing frames, resize and signals, restoring the terminal on exit | core |
| `vtmux` | The binary: global flags, connect-or-start, dispatching to a one-shot command, attach or control mode | all |
| `xtask` | Test tooling: the reference tmux build, the regress runner, the behaviour oracle | core, vt |

`unsafe` is allowed only in `vtmux-vt` and `vtmux-server/src/sys/`. Every other crate has `#![forbid(unsafe_code)]` (D10).

## Following the bytes

### Output: a program prints something

1. nvim writes bytes to its PTY (PTY #2).
2. The server's event loop reads them and feeds them into pane %0's libghostty-vt terminal, which updates the pane's grid. If the bytes contain a query (DA, cursor position), the pane terminal answers it in tmux's words, not Ghostty's, and the reply goes back down PTY #2.
3. Once per loop tick, the compositor builds the complete picture each attached client should see: every visible pane at its layout cell, the borders, the status line and any popup.
4. It diffs that picture against the last frame it sent the client, encodes only the changed cells for that client's terminal, and wraps the result in synchronized output (`?2026h`/`?2026l`) with the cursor placed once at the end.
5. The client writes the frame to PTY #1 and acknowledges it. The outer terminal draws it.

Steps 3 and 4 fix two of tmux's [paper cuts](PAPERCUTS.md) by construction: the cursor no longer flickers under heavy output, and popup borders don't flicker over scrolling panes. tmux forwards each pane update to the terminal as it happens; vtmux only ever sends finished pictures.

The acknowledgements are the backpressure. When a client's terminal is slow or stalled, the server stops composing frames for it, and sends one fresh full frame once it catches up. tmux does the same thing in `tty_block_maybe`.

### Input: you press a key

1. The outer terminal sends the key's bytes through PTY #1 to the client, which forwards them over the socket.
2. The server decodes them into keys, the way tmux's `tty-keys.c` does. It also swallows the terminal's replies to vtmux's own queries, so they never reach a pane as keystrokes.
3. The key is looked up in the key tables. If the prefix is active, or the key has a root binding, the bound command runs (`C-b %` splits the pane). Otherwise the key is re-encoded for the active pane's current modes (normal or application cursor keys, bracketed paste) and written to that pane's PTY.

### Commands: `vtmux ls`, scripts, plugins, murmur and mu

1. A short-lived client connects. If nothing is listening and the command can start a server, it takes a lock, binds the socket, and starts the daemon.
2. It sends a `Hello` with its argv, cwd, environment and tty info.
3. The server parses the command line with `vtmux-core`, resolves targets against its model, runs the command, and replies with `Stdout`, `Stderr` and `Exit`.

Those replies match tmux 3.8 byte for byte, including error messages and exit codes. This is the compliance edge that scripts and plugins depend on.

Commands run on a command queue (`cmdq.rs`, after tmux's `cmd-queue.c`). Each client has its own queue, and a global queue runs work that has no client: the startup config, and `run-shell -b` commands. A command can wait, as tmux's `CMD_RETURN_WAIT` does. `wait-for`, file I/O, and plain `run-shell` and `if-shell` waiting on their job all hold their queue until they are continued. Other queues keep running meanwhile. Commands a running command adds (a sourced file, the branch `if-shell` picks) go in right after it. Jobs (`jobs.rs`) are `/bin/sh -c` children whose output comes back through the same `mio` loop. `#(cmd)` in a format is a job too (`format_jobs.rs`): the format context only asks a cache, which starts and kills the processes at the end of the loop turn. As in tmux, each client has its own cache, so a job's output shows only to a later expansion in the same client.

As in tmux, a command client does its own file I/O: `load-buffer` and `save-buffer` on a path, and `load-buffer -` from its stdin, become `ReadOpen` / `WriteOpen` requests. The command waits on its queue until the client answers `FileDone`, so a slow file such as a FIFO holds up only that client. Attached and control clients get the I/O done in the server, non-blocking.

Control-mode clients (`vtmux -C`) use the same socket. They send command lines on stdin and get tmux's `%begin`/`%end`/`%error` guards, `%output`, and the other notifications instead of frames.

## Where libghostty sits

libghostty-vt runs **inside the server, once per pane**, holding the screen state that tmux's own emulator would hold. This is the "grid follows Ghostty" half of D4. Anything a script can read is still shaped like tmux, even when the cells underneath came from Ghostty: `capture-pane` output, `#{cursor_x}`, history size, query replies. Where the two genuinely differ, the difference is recorded with evidence in [`compat/DIVERGENCES.md`](../compat/DIVERGENCES.md).

If your outer terminal is Ghostty, the final picture is parsed a second time by Ghostty's own copy of libghostty. That double parse is the cost of being a drop-in tmux that works inside any terminal and over plain SSH. Superlogical's Rex avoids it by making its client the terminal, which gives up compatibility.

## Compatibility is at the edge

Inside, vtmux is ordinary Rust. IDs are newtypes, values are enums, windows hold a typed layout tree, and state lives in ID-keyed arenas rather than a pointer graph. Errors are typed values that only become tmux's exact text when they reach the edge.

Where tmux's observable behaviour falls out of its data structures (sessions listed in name order, IDs never reused, hooks firing in command-queue order), vtmux reproduces it explicitly and a test pins it. It doesn't copy the structure to get the behaviour for free. The spec section *Rust at the core, tmux at the edge* has the rules.

## How it's tested

Three sources of truth are checked against each other:

```
             tmux 3.8 regress/            behaviour oracle               esctest / specs
          (what tmux means to do)      (what tmux 3.8 actually does)    (what a terminal should do)
                    │                              │                              │
   cargo xtask regress                cargo xtask oracle record → traces    D4 arbiter for
   runs tmux's own 174 tests          cargo xtask oracle replay  → vtmux    grid differences
   against the vtmux binary           (CLI output, -C streams, and the
                                       attached screen as seen by a
                                       libghostty observer terminal)
```

- **Regress:** tmux's own test suite from the 3.8 tag, vendored unchanged and run against `vtmux`. `compat/expected.toml` is the scoreboard. The gate fails both when a test regresses and when one passes without being marked as passing.
- **Oracle:** scenarios are run once against the real tmux 3.8 and the results are stored as traces. Each run of the gate replays the same inputs against vtmux and compares. Screens are compared by feeding each client's output into a libghostty observer terminal and diffing the resulting grids, never the raw escape bytes, because the same screen can legitimately be drawn with different bytes. This is a third place libghostty appears.
- **Reference comparisons in tests:** the CLI, attach and control tests run each scenario against both binaries and compare the results directly.

`scripts/check.sh` runs all of it: fmt, clippy, unit tests, regress and oracle replay. CI runs it on Linux and macOS.
