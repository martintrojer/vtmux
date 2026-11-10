# panel_g3_fork_safety

Base: main `0f384907`. Host: shared CentOS 9 devserver, 96 available CPUs.

## Findings

| Finding | Verdict | Evidence |
|---|---|---|
| Safe width setters/readers race with freed overrides | FIXED `ff7a9b6` | Two writers replacing/clearing ranges and two readers querying widths and printing caused SIGSEGV in an isolated subprocess. Patch 0039 locks every slice lookup and replacement/free; the empty fast path, narrow bound and clustering flags are atomic. Removing the patch reproduced SIGSEGV again. |
| Safe `GridRef::with_x` reaches native memory outside its page | FIXED `bb18b2b` | The new test aborted at `page.zig:getRowAndCell` before the fix. Removing patch 0038 in ReleaseFast returned `Ok` at column 5 of a 5-column page and failed the assertion. Readers now reject 5 and 65535; column 4 still reads `e`. `row_cells` still ignores x. |
| By-value allocator adapter retains a destroyed parameter | FIXED `c5acc6d` | The callback-context address test failed before invoking any dangling callback. An owned-allocator compile-fail test also failed because it compiled. `From<&A>` ties the original context to the adapter lifetime; allocation/free/grow/shrink and both lifetime doctests pass. |
| Growable key/mouse encoders reserve relative to capacity | FIXED `bc516d5` | Both new tests failed with `OutOfSpace` (3 bytes for ArrowUp, 9 for SGR mouse). Empty and prefixed vectors one byte short now preserve the prefix and append the full output. |
| OSC window title requests `&str` instead of `const char **` | FIXED `a4b7f3e` | `2;hello` returned an empty title before. The getter now reads the parser-owned C pointer and validates UTF-8 under the exclusive parser borrow. ASCII, empty and UTF-8 title tests pass. |

The fixes change the published wrapper's safety/output, not tmux's command or screen behavior. No new feature, option or snapshot field; README and PERFORMANCE need no new feature/how-to or performance claim. CHANGELOG, ARCHITECTURE and VENDOR record the changes.

## Call sites and safety argument

- `crates/vtmux-vt/src/lib.rs:36,48` exposes both setters safely. The fix is native, covering both width queries and terminal printing, not just the Rust setter or query. `crates/vtmux-server/src/commands/set_option.rs:227,231` uses them in the single-threaded loop. The matched semantics remain tmux `utf8.c: utf8_update_width_cache` and `screen-write.c: screen_write_combine`.
- `crates/vtmux-vt/src/capture.rs:1596,1748` derives x from `row_cells`, already within the page. Patch 0038 validates both coordinates in `CGridRef.toPin`, before readers call `getRowAndCell`; ReleaseFast assertions are not the defense.
- No vtmux call site enables `allocator_api`. The optional feature was executed here, not merely inspected. All context-dependent callbacks use the original `A`, not a temporary reference-to-reference. Allocator-backed objects are not Send/Sync.
- `include/ghostty/vt/osc.h` specifies the title output as `const char **`, NUL-terminated and valid until another parser operation. Rust's exclusive parser borrow prevents that operation while the returned title lives.
- A false atomic empty-table flag skips the table; a true flag acquires the native lock before reading it. A reader arriving after a clear finds an empty slice under the lock. Old memory is freed under that same lock. No bare reads of printer/clustering flags remain outside their atomic accessors/stores, including inside `unicode/main.zig`.
- Concurrent printing may observe option changes during a call; it is not a transaction over the whole printed string. The Latin-1 grapheme branch consumes valid `u21` codepoints regardless of the chosen clustering path. The enhanced test enables mode 2027 and prints Latin-1, ZWJ and VS16 while changing the narrow bound and clustering flags; both Debug and ReleaseFast pass.
- vtmux's daemon forks are before server threads (`sys/daemon.rs:53,85`). Job children exec without reading width state (`sys/job.rs:103,186`); compression workers only compress and join before returning (`vtmux-vt/src/term.rs:4573`). A multithreaded fork-without-exec user must still follow the POSIX restrictions on calls in the child; this patch does not promise post-fork lock recovery.

## Fresh-context check

A separate read-only pi invocation reviewed the five fixes; transcript was read from `/tmp/panel-g3-review.log`. It confirmed the slice lock, native bounds, borrowed allocator context, C title lifetime and encoder reserve correction. Its actionable test gaps were fixed: exercise the narrow/clustering paths, and run vendor tests in CI (the workspace excludes third_party). `scripts/check.sh` now formats and tests the wrapper with `allocator_api`, including its compile-fail doctests; build/freshness checks include that suite too.

The review's speculative same-file non-atomic reads were refuted by inspecting the whole patched `unicode/main.zig`; only the accessor loads and setter stores use those flags. Its zero-byte GLOBAL allocation concern does not reach a callback through `Bytes::new_with_alloc`: native `c/allocator.zig:16` rejects zero first. No unrelated allocator hardening or new dependency was added.

## Gate record

All cargo commands ran through `mise exec --` under muzzle and a `timeout -k 5` bound. Native source came from `git archive` of the supplied clone at pinned `3bd54e025cc3349ee76ab1af7ec1ff8f7353632b`, passed as `GHOSTTY_SOURCE_TARBALL=/tmp/panel-g3-ghostty.tar.gz`. The build script verified its Zig package hash, applied this tree's patches in order and wrote its stamp. No shared checkout was reset.

`V` below is `cargo test --manifest-path third_party/libghostty-rs/libghostty-vt/Cargo.toml`; vendor runs used `CARGO_TARGET_DIR=target`. `RF` means `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast`; otherwise the standalone vendor build uses Debug. Durations are rounded muzzle run times, excluding queue waits.

| GATE: commit / command / scope | RESULT: exit / duration / counts / skips | CONDITIONS |
|---|---|---|
| `0f384907` + encoder tests: `V growable_encoding -- --nocapture` | 101 / 40 s / 2 failures, required 3 and 9 / no skips | Cold standalone native/Rust build, Debug |
| `bc516d5`: `V` | 0 / 1 s / 14 unit + 18 doctests / 3 ignored | Warm Debug |
| `bc516d5` + grid test: `V grid_ref_readers -- --nocapture` | 101 / 2 s / native SIGABRT at missing column assertion / no skips | Debug |
| `bb18b2b` code: `RF V` | 0 / 79 s / 15 unit + 18 doctests / 3 ignored | New ReleaseFast build |
| `bb18b2b` + OSC test: `RF V window_title_is_read -- --nocapture` | 101 / 81 s / empty title, expected hello / no skips | Includes rebuilt grid patch preserving row_cells behavior |
| `a4b7f3e` code: `RF V` | 0 / 1 s / 16 unit + 18 doctests / 3 ignored | Warm ReleaseFast |
| `a4b7f3e` + allocator test: `RF V --features allocator_api adapter_borrows -- --nocapture` | 101 / 1 s / wrong context address / no skips | First allocator-api2 build |
| Same: `RF V --features allocator_api --doc alloc::Allocator` | 101 / 1 s / owned conversion unexpectedly compiled; short-lifetime case rejected / no skips | Warm |
| `c5acc6d` code: `RF V --features allocator_api` | 0 / 1 s / 17 unit + 20 doctests / 3 ignored | Warm ReleaseFast |
| `c5acc6d` + width test: `RF V width_overrides_can_change -- --nocapture` | 101 / 1 s / child SIGSEGV / no skips | An earlier attempt stopped at an invalid Rust escape; corrected before the real red run |
| `ff7a9b6` code: `RF V --features allocator_api` | 0 / 79 s / 18 unit + 20 doctests / 3 ignored | Rebuilt native synchronization patch |
| `ff7a9b6` enhanced test: `V --features allocator_api` | 0 / 26 s / 18 unit + 20 doctests / 3 ignored | Debug, including Latin-1, mode 2027, ZWJ and VS16 |
| `ff7a9b6` + gate coverage: `muzzle run --class high --rule vtmux-check --run-timeout 3000 -- timeout -k 5 3000 mise exec -- env GHOSTTY_SOURCE_TARBALL=/tmp/panel-g3-ghostty.tar.gz bash scripts/check.sh` | 0 / 424 s / workspace 1565 passed, vendor 18 unit + 20 doctests; regress 171 normal passes + 2 flaky-marked passes + 1 xfail; oracle 417/417, upgrade-3 417/417, upgrade-1 9/9 | 96 CPUs, suites at once; 3 muzzle slots; native caches warm but workspace build cold; Linux, Rust 1.99.1, Zig 0.17.0 |
| Native mutations: remove 0038, then remove 0039 separately; `RF V --features allocator_api grid_ref_readers` / `width_overrides_can_change`; restore both, `RF V --features allocator_api` | 0 / 242 s for the mutation script; each missing patch test exited 101 (unchecked read / SIGSEGV); restored suite 18 unit + 20 doctests / 3 ignored | Independent ReleaseFast rebuilds; traps restored the patches |
| First stress admission: 52-slot muzzle run, wait for flock 180 s | 1 / 180 s / no tests ran / not a pass | Shared stress lock held by another worker's benchmark; no other worker interrupted |
| Stress retry: 5x `RF V --features allocator_api` under `nproc/2` busy loops, then once with private POSIX `/bin/sh` | 0 / 6 s admitted / each run 18 unit + 20 doctests / 3 ignored per run | 48 busy loops, exclusive `/tmp/vtmux-stress.lock`, 52 muzzle slots, loop cleanup trap; sh substituted only in a private user/mount namespace with zsh 5.9's sh emulation |

Full gate exclusions: 6 workspace ignores (three prior-binary upgrade tests, two manual timing tests, one snapshot doctest), 3 vendor doctest ignores, expected `wait-for-client-lost.sh` failure. Upgrade insertion is intentionally omitted in six existing scenarios named by `oracle/upgrade-every-skip.toml`; their observations still ran and passed. Upgrade-3 inserted 3356 upgrades, put off 2583 a step; targeted upgrade-1 inserted 230, put off 43.

The macOS cross-target check was skipped: the target is not installed, and `rustup target add aarch64-apple-darwin` failed DNS resolution for static.rust-lang.org. Dash was unavailable and its source download also failed DNS; the additional shell run used actual `/bin/sh` in a private namespace with POSIX-like zsh, not dash and not a change to the shared host's shell. No performance benchmark or macOS execution was claimed.

The harness owned every server socket directory. No ad-hoc server was started. Socket guard passed; stress subprocesses exited and the shared flock was released. No mu database operation, stash or push.
