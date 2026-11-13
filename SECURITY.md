# Security

## Reporting a vulnerability

Report it privately through GitHub: [Report a vulnerability](https://github.com/martintrojer/vtmux/security/advisories/new) on martintrojer/vtmux (Security tab, private vulnerability reporting). Please don't open a public issue. The maintainer, Martin Trojer, answers there.

## Supported versions

| Version | Fixes |
|---|---|
| The latest release ([releases](https://github.com/martintrojer/vtmux/releases)) | Yes |
| Anything older | No: upgrade, live if you like (`tmux vtmux-upgrade`) |

## Threat model

The same as tmux 3.8's: the server trusts its own user, and everyone who can talk to it can run commands as that user.

| Surface | What vtmux does |
|---|---|
| **Socket directory** | `$TMUX_TMPDIR` (or `/tmp`) `/tmux-<uid>` or `/vtmux-<uid>`, created mode 0700. The client refuses one it doesn't own, one that isn't a real directory (a symlink isn't), and one with any bit for others (`directory … has unsafe permissions`), as tmux does. |
| **Socket** | Bound under a umask: mode 0660 in the `-<uid>` directory (still 0700, so only you get in), 0600 for an `-S` path. The server asks the kernel who connected (`SO_PEERCRED` on Linux, `getpeereid` on macOS). |
| **Other users** | Only the owner and root get in, as in tmux. `server-access -a` adds a user or group, `-r` makes them read-only; they also need file access to the socket, which vtmux never opens up for them. A read-only client can't ask for a live upgrade (`access denied`). |
| **What crosses the socket** | The client's argv, working directory, environment and terminal (name, `TERM`, size, features, control characters); then key and stdin bytes in, screen bytes out. No file descriptors: the client is a dumb pipe. A frame is at most 16 MiB; one that doesn't decode drops the connection. |
| **Live upgrade** | The snapshot (every pane's screen and scrollback) goes to the new binary in an anonymous memory file (`memfd`, Linux) or a file created 0600 in the socket directory and unlinked at once (macOS): only the user can read it, and nothing stays on disk. The binary must name an absolute path, and its dry run must read the snapshot and answer with its digest before the server execs it, so `/bin/echo`, `true` or a script is refused (exit 1). The one exception: a binary whose `-V` is exactly `vtmux 3.8-beta1` or `vtmux 3.8-beta2` passes without that line. |
| **Files on disk** | Nothing persistent: vtmux writes no state files. Besides the socket: a 0600 start lock and the 0700 `tmux` shim directory next to it, a 0600 temporary file while `e` in choose-buffer or customize mode edits a value, and what you ask for (`save-buffer`), as tmux. |

## The terminal side

| | |
|---|---|
| **Escape sequences** | Pane output is parsed by libghostty-vt (Ghostty's Zig parser), with vtmux's own scanner over it for titles and DCS. An OSC, DCS or APC string past `input-buffer-size` (1 MiB default) is discarded, as in tmux; a kitty graphics command is capped by the image limit below, never past 65 MiB. |
| **Kitty graphics** | Off unless you `set -s vtmux-graphics on`. Images come only inline in the escape: file, temporary-file and shared-memory media are refused with one answer, and no path is opened. All panes' images share `vtmux-graphics-limit` (256 MB default), and decoding (zlib, PNG) is held to it too. |
| **Passthrough, clipboard** | As tmux: `allow-passthrough` is off, `set-clipboard` is `external`, so a program can set your terminal's clipboard with OSC 52 unless you turn it `off`. |

## Unsafe code

| Where | `unsafe` blocks |
|---|---|
| `crates/vtmux-server/src/sys/` (ptys, signals, live upgrade, user lookup) | 80 |
| `crates/vtmux-vt` (the wrapper over libghostty-vt) | 3 |
| Every other crate | 0: `#![forbid(unsafe_code)]`; vtmux-server denies it outside `sys/` |
| **In all** | **83** |

Below that: the vendored Ghostty bindings (`third_party/libghostty-rs`, 417 blocks, mostly FFI calls) and Ghostty itself (Zig) with 43 local patches, each explained in its `VENDOR.md`.

## Not covered

- No external audit, of security or of the code. Reviews are by models, recorded in [docs/test-health/](https://github.com/martintrojer/vtmux/tree/main/docs/test-health).
- A malicious program in a pane can do what it could under tmux: draw anything in its pane, set titles (`allow-set-title`), set the clipboard with OSC 52 when `set-clipboard` allows, and send whatever it likes to your terminal when you turn passthrough on.
- Anyone running as your user owns the server, as in tmux.
