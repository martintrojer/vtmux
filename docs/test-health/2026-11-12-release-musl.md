# Static musl release builds (2026-11-12)

The release gains `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` tarballs next to the gnu ones. The gnu build comes from ubuntu-22.04 and needs glibc 2.35, so it doesn't run on EL9 (2.34) or Debian 11 (2.31). The musl build is static and runs on any Linux with the right CPU.

## Host

A shared EL9 server (glibc 2.34, Linux 6.16, 96 cpus), Rust 1.99.1, Zig 0.17.0, commit `034e438a` plus this branch. The host could not download rustup's `rust-std` for musl (blocked by the proxy). As a stand-in, std came from Debian's `rust-src_1.99.1+dfsg1-1~exp1` (the same 1.99.1 library sources), built with `RUSTC_BOOTSTRAP=1 CARGO_UNSTABLE_BUILD_STD=std,panic_unwind,panic_abort` and `__CARGO_TESTS_ONLY_SRC_ROOT` pointing at it. CI uses rustup's prebuilt target. Ghostty came from a local `git archive` of the pinned commit (`GHOSTTY_SOURCE_TARBALL`).

## The build

- `vtmux-ghostty-vt-sys` already mapped both musl triples to Zig targets (`zig_target`); Zig builds Ghostty for musl as is.
- One Rust break: `libc::close_range` doesn't exist for musl in the libc crate. `close_from` now calls the raw `SYS_close_range` syscall on all of Linux. Before: `cargo check -p vtmux-server --target x86_64-unknown-linux-musl` fails with E0425. After: exit 0.
- Linking: `zig cc -target <arch>-linux-musl` is the linker, with `-C link-self-contained=no`, so Zig's musl (the one Ghostty is built against) is the only libc and crt. On aarch64 the wrapper drops `-Wl,--fix-cortex-a53-843419`, which `zig cc` refuses (cargo-zigbuild drops it too).
- `scripts/release-tarball.sh x86_64-unknown-linux-musl v3.8-beta3`: exit 0, a 4.58 MB tarball (gnu: 4.60 MB). The binary is `statically linked`, with no `INTERP` and no `NEEDED` (the script now checks this). It runs as `vtmux` (`vtmux 3.8-beta3`) and as `tmux` (`tmux 3.8`).
- `scripts/release-tarball.sh aarch64-unknown-linux-musl v3.8-beta3`, cross-built from x86_64: exit 0. Its `-V` checks ran under qemu-user.

## Running it on EL9

A scratch probe (Python, `pty.fork`; not kept) with a private `TMUX_TMPDIR`/`XDG_STATE_HOME` and its own `-L` label. Both orders pass:

| Check | musl first | gnu first |
|---|---|---|
| `vtmux -V`; a server with a pane runs a command | ok | ok |
| A server started without `PATH` runs `printf` (musl `execvp`: `/usr/local/bin:/bin:/usr/bin`) | ok | ok |
| A PTY client attaches and draws the pane | ok | ok |
| `vtmux-upgrade -b` onto the other libc's binary, client attached: exit 0, same pid, `/proc/<pid>/exe` is the target, 1,482 history lines kept, the pane takes input, the client stays | ok | ok |
| ...and back again | ok | ok |
| `C-b d` detaches | ok | ok |

A pane inherits only 0, 1 and 2 from either build (`ls /proc/self/fd` in the pane prints `0 1 2 3`, where 3 is `ls`'s own), so `close_from`'s raw syscall closes the server's descriptors.

aarch64 musl under qemu-user: server, pane, no-`PATH` pane and attach all ok, and `vtmux-upgrade` exits 0. The script's exe check can't follow qemu.

## Tests and the oracle against the musl build

- `cargo test --target x86_64-unknown-linux-musl --lib -p vtmux-server -p vtmux-vt -p vtmux-core`: 455 + 282 + 179 passed, including `no_path_searches_as_execvp_does` on its musl branch.
- `-p vtmux` integration tests: attach 164, upgrade 48, persist 37, handshake 11, upgrade_command 11 passed; cli 162 of 163 and control 32 of 33 (below). The non-workspace packages were built with `opt-level=3` here: std built from source in the dev profile is unoptimised, unlike rustup's, and that made three `upgrade.rs` copy-mode tests hit the format time limit. They pass with std optimised as rustup ships it. In release builds `lsp` of a 70k-line history takes 11-13 ms on both builds.
- `CARGO_BUILD_TARGET=x86_64-unknown-linux-musl cargo xtask oracle replay`: 442 of 443 scenarios pass.
- The three failures are all one thing: OS error text is musl's `strerror`. `buffer_files` step 15 and `buffers_stdin_and_file_errors` show `I/O error` where the glibc reference shows `Input/output error`. `control_control_needs_a_tty` shows `Not a tty` where it shows `Inappropriate ioctl for device`. tmux prints `strerror(errno)`, so a musl-built tmux says the same. The CHANGELOG notes it.

## Speed and memory, release builds

`seq 1 3000000` into a 200x50 pane (`history-limit 100000`), timed until `DONE` shows. Server RSS after that, then a live upgrade.

| | gnu | musl |
|---|---|---|
| Flood, 2-3 runs | 1.77, 1.86, 1.87 s | 1.91, 1.64, 2.45 s |
| Server RSS | 180 MB | 185 MB |
| Upgrade to the same binary (100k lines) | 0.071 s | 0.081-0.086 s |
| Upgrade to the other libc (100k lines) | 0.33 s | 0.32 s |

An upgrade across libcs is slower because the target triple is part of Ghostty's patch-stack hash. The two binaries don't share a page layout, so history moves in the row format.

## mise picks

The real mise 2026.11.6 ran against a fake GitHub releases API (`api_url=http://127.0.0.1:<port>`). It served the release job's five tarballs and `SHA256SUMS`, all built here or as stand-ins. Each run had a private `MISE_DATA_DIR`. mise's `AssetPicker` scores the libc in the triple: +25 for the host's libc and -10 for the other. A glibc host is `gnu`, a musl one (by `/etc/os-release` or `ld-musl-*`) is `musl`, and `MISE_LIBC` overrides it.

| Host | Downloaded | Installed binary |
|---|---|---|
| this glibc host | `x86_64-unknown-linux-gnu` | dynamic, `-V` ok |
| `MISE_LIBC=musl` (Alpine) | `x86_64-unknown-linux-musl` | static, `-V` ok |
| glibc host, `[matching=musl]` | `x86_64-unknown-linux-musl` | static, `-V` ok |
| `MISE_ARCH=arm64` | `aarch64-unknown-linux-gnu` | |
| `MISE_ARCH=arm64 MISE_LIBC=musl` | `aarch64-unknown-linux-musl` | static, `-V` ok (qemu) |
| `MISE_OS=macos MISE_ARCH=arm64` | `aarch64-apple-darwin` | |

The gnu build stays the default on glibc hosts. It reads the distribution's `CS_PATH` for a pane with no `PATH`, trims malloc after an upgrade, resolves users through NSS, and prints the host's own `strerror` text, as the host's tmux does. musl is only better where the gnu build can't run, and mise can't tell a glibc's version, so a host with a glibc older than 2.35 opts in with `[matching=musl]`.

## The release job

- `actionlint` 1.7.12 (with shellcheck) on `.github/workflows/*.yml`: exit 0. shellcheck on `scripts/release-tarball.sh`: exit 0.
- The job's *Build and package* step ran locally (`TARGET`, `TAG`, `GITHUB_OUTPUT`, `GITHUB_STEP_SUMMARY` set). For musl the summary shows `file`'s `statically linked`; for gnu it shows `needs glibc GLIBC_2.34` (this host's build).
- The *Release* step ran on the five tarballs with a stub `gh`: `sha256sum -c SHA256SUMS` OK for all five, and `gh release create` got all five plus `SHA256SUMS`.

## Gate

At load 15-20 the whole `scripts/check.sh` needed more than a 300 s command cap (killed at 293 s, mid-suites), so its steps ran in two batches, each one at once as check.sh runs them:

- fmt (both) and clippy `-D warnings`: clean. The darwin and musl type-checks were skipped (targets not installed here).
- `cargo test --workspace --exclude vtmux`: 1,292 passed, 4 ignored. The wrapper's `--features allocator_api` tests: 39 passed, 3 ignored. `oracle replay`: 443/443. `--upgrade-every 1 --only …`: 9/9 (230 upgrades). 86 s.
- `cargo test -p vtmux`: 472 passed, 4 ignored. `regress`: 171/174 (2 flaky passed, 1 xfail, 0 skipped). `--upgrade-every 3`: 443/443 (3,453 upgrades). 209 s.
- `socket-guard.sh verify`: clean.

## Review fixes

- **Errno text.** `source-file .` from a command client printed glibc's `Input/output error` from a hardcoded string. It now prints the libc's `strerror(EIO)`: `I/O error` from the musl build, `Input/output error` from the gnu one, run here for both.
- **Differential tests.** The cli and control comparisons and the oracle's `compare` turn the libc-specific errno texts (`EIO`, `ENOTTY`) into tokens on both sides. The traces are unchanged. Against the musl build: `buffers_stdin_and_file_errors` and `control_control_needs_a_tty` pass, and oracle replay is 443/443 (before: 442). On glibc: cli 164/164, control 33/33, replay 443/443, xtask 80, vtmux-server lib 456. clippy is clean.
- **User lookups.** musl has no NSS. tmux has no USER/LOGNAME fallback when `getpwuid` or `getpwnam` fails: `HOME` and `SHELL` come first, `#{user}` is empty, and `server-access` says `unknown user`. vtmux already does the same, so only the docs change: README Install, RELEASING and the CHANGELOG say the gnu build is the default on glibc hosts, and that on sssd/LDAP hosts without nscd the musl build may not resolve user names.
