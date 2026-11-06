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
| `vtmux-vt` | The only FFI crate: a safe wrapper over libghostty-vt. It turns Ghostty's terminal into tmux's shapes: `capture-pane` output, tmux's replies to terminal queries, cursor and history reporting. It also exports and imports a pane's whole terminal state exactly, whole (`PaneTerm::export_state` / `import_state`) or a page at a time (`export_state_head` + `export_pages`, `StateImport`), for live upgrade; an import validates every field and refuses a malformed state | `libghostty-vt` |
| `vtmux-server` | The single-threaded event loop and everything stateful: sessions, windows and panes, PTYs and processes, commands, control mode, the compositor and frame encoder. OS calls (`fork`, `openpty`, signals) are confined to `sys/`. `snapshot_term.rs` converts a pane's terminal to and from the snapshot format one Ghostty page at a time | core, vt, snapshot |
| `vtmux-client` | The attached client: raw tty mode, forwarding input, writing frames, resize and signals, restoring the terminal on exit | core |
| `vtmux-snapshot` | The live-upgrade and persistence format (D27): versioned, length-prefixed tagged records for the whole server state, rows in vtmux's own run-length encoded cell format, history pages LZ4-compressed and named by the BLAKE3 hash of their rows; each stream carries its body's length and hash. Its types are its own, not the server's, so a refactor can't change the format; `vtmux-server/src/server/snapshot.rs` converts the model to and from it (`Server::to_snapshot`, `Server::from_snapshot`); `vtmux-server/src/persist.rs` writes `vtmux-persist` checkpoints (a manifest plus content-addressed `chunks/`, one locked directory per socket path; `server/persist.rs` decides what goes in and when), and `server/restore.rs` restores one when a server starts: checked first in a child process by the upgrade's dry run, imported with every pane dead, the config run on top | nothing of ours (`lz4_flex`, `blake3`) |
| `vtmux` | The binary: global flags, connect-or-start, dispatching to a one-shot command, attach or control mode | all |
| `xtask` | Test tooling: the reference tmux build, the regress runner, the behaviour oracle | core, vt |

`unsafe` is allowed only in `vtmux-vt` and `vtmux-server/src/sys/`. Every other crate has `#![forbid(unsafe_code)]` (D10).

## Following the bytes

### Output: a program prints something

1. nvim writes bytes to its PTY (PTY #2).
2. The server's event loop reads them and feeds them into pane %0's libghostty-vt terminal, which updates the pane's grid. If the bytes contain a query (DA, cursor position), the pane terminal answers it in tmux's words, not Ghostty's, and the reply goes back down PTY #2.
3. Once per loop tick, the compositor builds the complete picture each attached client should see: every visible pane at its layout cell, the borders, a window menu, the status line and any popup. Each pane keeps one Ghostty render state that re-reads only the rows Ghostty marked dirty, and each client caches its window layout and composed rows, so a one-cell change recomposes one row; a tick where no row or the cursor changed builds no frame.
4. It diffs that picture against the last frame it sent the client (rows equal to the last frame are skipped whole), encodes only the changed cells for that client's terminal, and wraps the result in synchronized output (`?2026h`/`?2026l`) with the cursor placed once at the end.
5. The client writes the frame to PTY #1 and acknowledges it. The outer terminal draws it.

Steps 3 and 4 fix two of tmux's [paper cuts](PAPERCUTS.md) by construction: the cursor no longer flickers under heavy output, and popup borders don't flicker over scrolling panes. tmux forwards each pane update to the terminal as it happens; vtmux only ever sends finished pictures.

The acknowledgements are the backpressure. While a client still has a frame it has not written, the server composes no new one for it (tmux defers its redraw the same way in `server_client_check_redraw`); the acknowledgement brings the next frame, with everything that changed meanwhile. While a pane is flooding (it still has unread output after its read budget), frames to a client are also at most 10 ms apart, so a flood costs a frame per interval rather than one per read; a frame that also shows a change the flood did not make (a key's echo in another pane, a mode or status redraw) goes at once. Passthrough (`DCS tmux;`) is written as it comes, deferred frame or not, and counts towards the client's unwritten bytes like a frame. When a client's terminal is slow or stalled, the server stops composing frames for it, drops passthrough that comes meanwhile, and sends one fresh full frame once it catches up. tmux does the same thing in `tty_block_maybe` and `tty_add`.

A pane whose program has synchronized output on (`?2026h`) is composed from the screen it showed when the mode began (`PaneTerm::sync_held`), so frames carry none of its half-drawn content while other panes and the status line go on updating. The hold ends on `?2026l`, RIS, a resize, a redraw of that pane or the whole window, or one second after the last `?2026h`; the next frame shows the finished screen. tmux does this with `MODE_SYNC` (screen-write.c:211-240, 1017-1062).

### Input: you press a key

1. The outer terminal sends the key's bytes through PTY #1 to the client, which forwards them over the socket.
2. The server decodes them into keys, the way tmux's `tty-keys.c` does. It also swallows the terminal's replies to vtmux's own queries, so they never reach a pane as keystrokes. A mouse report is located (`mouse.rs`, after `server_client_check_mouse`: pane, border, status range) and becomes a key such as `MouseDown1Pane`; its binding's commands start from the pane under the pointer and carry the event, which `=` targets and the `mouse_*` formats read.
3. The key is looked up in the key tables. If the prefix is active, or the key has a root binding, the bound command runs (`C-b %` splits the pane). Otherwise the key is re-encoded for the active pane's current modes (normal or application cursor keys, bracketed paste) and written to that pane's PTY.

### Commands: `vtmux ls`, scripts, plugins, murmur and mu

1. A short-lived client connects. If nothing is listening and the command can start a server, it takes a lock, binds the socket, and starts the daemon.
2. It sends a `Hello` with its argv, cwd, environment and tty info.
3. The server parses the command line with `vtmux-core`, resolves targets against its model, runs the command, and replies with `Stdout`, `Stderr` and `Exit`.

Those replies match tmux 3.8 byte for byte, including error messages and exit codes. This is the compliance edge that scripts and plugins depend on.

Commands run on a command queue (`cmdq.rs`, after tmux's `cmd-queue.c`). Each client has its own queue, and a global queue runs work that has no client: the startup config, and `run-shell -b` commands. A command can wait, as tmux's `CMD_RETURN_WAIT` does. `wait-for`, file I/O, and plain `run-shell` and `if-shell` waiting on their job all hold their queue until they are continued. Other queues keep running meanwhile. Commands a running command adds (a sourced file, the branch `if-shell` picks) go in right after it. Jobs (`jobs.rs`) are `/bin/sh -c` children whose output comes back through the same `mio` loop. `#(cmd)` in a format is a job too (`format_jobs.rs`): the format context only asks a cache, which starts and kills the processes at the end of the loop turn. As in tmux, each client has its own cache, so a job's output shows only to a later expansion in the same client.

As in tmux, a command client does its own file I/O: `load-buffer` and `save-buffer` on a path, and `load-buffer -` from its stdin, become `ReadOpen` / `WriteOpen` requests. The command waits on its queue until the client answers `FileDone`, so a slow file such as a FIFO holds up only that client. Attached and control clients get the I/O done in the server, non-blocking.

Scripts and plugins inside vtmux call plain `tmux`. So that they reach this server whatever `tmux` the user's `PATH` holds, the server makes a private `.<label>.bin/` next to its socket with `tmux` → its own executable (`shim.rs`, spec D5). It puts that directory first on `PATH` for every pane, popup and job. Run as `tmux`, the binary finds the server through `$TMUX`. tmux has no shim: compat/DIVERGENCES.md D-022.

Control-mode clients (`vtmux -C`) use the same socket. They send command lines on stdin and get tmux's `%begin`/`%end`/`%error` guards, `%output`, and the other notifications instead of frames.

`vtmux-upgrade [-b binary]` moves the server onto another binary in place ([live-upgrade spec](specs/2026-11-06-live-upgrade.md), D27). `server/upgrade.rs` refuses a missing or non-executable binary at once, and a request from inside a `run-shell`/`if-shell` job (which would wait on the upgrade that waits on it; the refusal goes into the job's output too, which run-shell shows), waits up to 3 s for `run-shell`/`if-shell` jobs and for commands in flight, and writes every client's queued output; then the exec core below snapshots, dry-runs and execs. Any refusal leaves the server running as it was and reaches the requester. Without `-b` it takes the path the server was started as, resolved through symlinks when asked, so a relink of that path is followed. From the command line, a client the server turns away for its version asks again with the frozen `UpgradeRequest`.

Every frame but a few is free to change between builds, and a client of another protocol version gets `protocol version mismatch`, plus a hint to run `tmux vtmux-upgrade` when the client is the newer one. The few are frozen forever (`vtmux-core::protocol`, *Frozen frames*; live-upgrade spec): `Hello`, `Stdin`, `StdinEof`, `Stdout`, `Stderr`, `Exit`, `Exec`, `Flags`, and the upgrade handshake `UpgradeRequest` / `UpgradeResult` / `UpgradeNotice`. So are the 16 MiB frame cap, `Hello`'s version coming first (a `Hello` of another version that doesn't decode still gets the mismatch) and its trailing extension area, where later versions add fields. So a control client of any version from protocol 9 on is served (sent frozen frames only, and it skips frames it doesn't know), and an `UpgradeRequest` from a peer with write access is answered ahead of any `Hello` or version check.

A live upgrade (D27; `server/resume.rs`, `sys/upgrade.rs`) replaces the server's image in place. The server encodes a snapshot into an anonymous memory file (`memfd` on Linux, an unlinked file in the socket directory on macOS), runs the new binary's dry run on it (`--vtmux-check-snapshot <fd>`, at most 3 s, as the leader of a process group killed whole when it ends), which decodes it and rebuilds the whole server on placeholder sockets, clears close-on-exec on the listening socket, every client socket, pane and popup PTY and `pipe-pane` descriptor (all close-on-exec from creation, so nothing else the server starts inherits them), and `execve`s `<binary> --vtmux-resume <fd>` with the signals the server handles blocked, so one that arrives before the new image has handlers waits instead of killing it. The new image takes the descriptors back by the numbers the snapshot records, rebuilds the model, reaps panes that exited meanwhile, unblocks the signals and runs on. The pid stays the same, so pane processes remain its children and attached clients stay connected. A resume that fails (an error or a panic) switches back to the image that wrote the snapshot, on the same descriptors (`--vtmux-switched-back`): on Linux by `fexecve` of the executable that image kept open since it started, whatever its path holds by now; on macOS by path, after a dry run of it. If that fails too, the server serves the panes without terminal state, and failing that it serves with no session while holding every pane PTY open and reading it, discarding the output so no program blocks (`show-messages` says why; `kill-server` releases them), rather than exit. `show-messages` also records a switch back and a rebuild without terminal state. Whoever asked (a command client, a control or attached client's command, or the frozen handshake's connection, all named in the snapshot) hears how it went from the image that ends up serving: success only from the new image.

Pane output for control clients is applied backpressure the way tmux's `control.c` does it (`control_queue.rs`). The server keeps a pane's bytes once, for as long as some control client is still owed them. Each client keeps an ordered queue of lines and byte ranges, each range stamped with the time it was read. Output is escaped into `%output` lines only when the client's socket has room, at most 8 KB unwritten at a time. A pane is not read while every attached client is a control client that is behind, so its program blocks on its terminal. `pause-after` pauses a pane whose oldest unsent output is too old (`%pause`). Without it, a client 300 s behind, or one holding 64 MB of unread replies, exits `too far behind`.

## Where libghostty sits

libghostty-vt runs **inside the server, once per pane**, holding the screen state that tmux's own emulator would hold. This is the "grid follows Ghostty" half of D4. Anything a script can read is still shaped like tmux, even when the cells underneath came from Ghostty: `capture-pane` output, `#{cursor_x}`, history size, query replies. Where the two genuinely differ, the difference is recorded with evidence in [`compat/DIVERGENCES.md`](../compat/DIVERGENCES.md).

Idle scrollback is compressed in place (`compress.rs`): five seconds after a pane's history last changed, or after vtmux last read it (capture-pane, copy mode), the loop runs one bounded libghostty-vt compression step at a time, round-robin across such panes, only after 50 ms of whole-loop quiet, and rests between steps. A pane whose history has kept changing (or kept waiting for a pass) for 10 s is starved: its steps also run at the end of busy turns, after rendering, one per turn under the same rest (shared by the starved panes, so each gets what a lone one would), until its pass is done and its history is quiet for 5 s, so a pane that streams all day still shrinks. Large styled pages encode an immutable snapshot in 128 KiB blocks, without changing grid layout; publication validates the source again. A completed pass waits for a full page of new scrollback before rearming, unless history was explicitly read or resized. Reading compressed history restores it transparently, so no command's output changes. A pane with nothing to compress sets no timer.

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

Memory and CPU against tmux 3.8, and how to reproduce them: [PERFORMANCE.md](PERFORMANCE.md).
