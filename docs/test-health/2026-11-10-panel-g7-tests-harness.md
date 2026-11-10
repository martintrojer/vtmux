# Panel G7: tests and harness

Task: `panel_g7_tests_harness`, workstream `vtmux-m3`. Base: `0f384907`.
All seven findings confirmed; seven fixes committed. No vtmux runtime behaviour,
snapshot fields, oracle traces or user-facing features changed.

## Findings and evidence

| Finding | Fix | Before / after |
|---|---|---|
| Blocking socket sweep | `6e7a63c` | A private Linux listener with backlog 1 and two queued connections made the actual sweep exceed 2s. The test unblocks and joins the worker before asserting. NONBLOCK connect now returns immediately, keeps the live socket, and still removes refused sockets. |
| Pane descendants survive teardown | `2801724` | Real tmux 3.8 panes ignoring HUP/TERM, including a child, survived `Runner::drop`; a failing regress script also left its pane alive. Both tests failed before and pass after capturing descendants before server shutdown and retaining their pids through TERM/KILL. Each test explicitly cleans up even in its red run. |
| Control-close truncates stderr | `27f4111` | A fake client closes stdout, exits after stdin EOF, and leaves a child writing stderr 200ms later. Observation was successful with empty stderr; now contains `late error\n`. Both reader completion flags are retained. |
| Chunk-dependent normalization | `61a3d49` | Restoring the base `stream` implementation makes a literal socket split fail. Exhaustive splits of literal and octal payloads, interleaved across panes, now pass. A second red/green test pins dates/clocks after decoded newlines and preservation of invalid bytes. |
| Host TMPDIR length | `3edd2b9` | Persistence failed with `File name too long` under an 85-character temp base. Subprocess guards in control, handshake, persist and upgrade_command all failed before pinning socket roots under `/tmp`, then passed. The helper also checks that one test ran, rather than accepting an empty selection. Explicit and fake socket roots were fixed too. |
| Unchecked no-reference CLI branch | `cb85dbd` | With XDG_CACHE_HOME empty and VTMUX_REFERENCE_TMUX removed, `ls_without_server` passed without comparison. A new subprocess test failed on that false success. Now the child fails with a reference setup hint. The stale embedded-expectations comment is gone. |
| Stale pending-resize fuzz rationale | `52a093d` | Removed the parser-desynchronization claim and added bounded pending CSI, OSC, DCS and charset resize/import coverage: 4,000 VT cases and 8,000 snapshot cases pass across 1,000 seeds. Blanket removal of the combined-fuzzer guard was tested and refuted by separate import bugs below. This fix changes tests/comments only. |

Reference: `~/.cache/vtmux/tmux-3.8/bin/tmux`, source
`~/.cache/vtmux/tmux-3.8-src`. Non-obvious source contracts:

- `server.c:server_send_exit` destroys sessions/windows; closing their PTYs does
  not guarantee that a HUP-ignoring pane program exits.
- `control.c:control_write_pending` limits output by available buffering, so a
  volatile token may cross `%output` lines.
- `window.c:window_pane_resize` resizes the screen without resetting the input
  context. `PaneTerm::write_screen_moves` already preserves Ghostty's partial
  sequence in this base.

## Verification record

Unless noted, commands below ran through `muzzle run --class high --rule
vtmux-check --run-timeout <limit> -- timeout -k 5 <limit-10> ...`. Shell commands
and probes had timeouts. Final code was unchanged between the slow gate and
these seven commits; the record itself was added afterwards.

GATE: base 0f384907 + finding-specific tests; `mise exec -- cargo test -p xtask <test> -- --nocapture`; queue, teardown, stderr, normalization
RESULT: semantic red observed for each: queue blocked (2s); oracle pane and child survived; regress pane survived; stderr empty; split socket not replaced. All pass after their fixes. Final xtask suite: 63 passed, 0 failed, 0 ignored. Initial queue fixture bring-up had two invalid attempts (mio without os-poll; an assumed backlog cap), not counted as reproductions.
CONDITIONS: Linux; real reference for teardown; private sockets; first xtask build 109s using the cached Ghostty source archive.

GATE: base 0f384907 + TMPDIR fix; `mise exec -- cargo test -p vtmux --test <persist|control|handshake|upgrade_command> socket_paths_ignore_long_tmpdir -- --nocapture`
RESULT: four red subprocess guards before constructor changes; four pass after (1s combined warm run). Separate original `persist_off_writes_nothing` reproduction: exit 101 in 31s, including build.
CONDITIONS: subprocess TMPDIR longer than sun_path; no mutation of the parent test environment.

GATE: base 0f384907 + CLI fix; `mise exec -- cargo test -p vtmux --test cli missing_reference_is_a_failure_not_an_unchecked_pass -- --nocapture`
RESULT: red before, green after; 1 test each, about 4s including compilation, 0 skips.
CONDITIONS: child uses a fresh empty cache and removes the reference override.

GATE: base 0f384907 + bounded resize tests; `VTMUX_STATE_FUZZ_SEEDS=1000 mise exec -- cargo test -p vtmux-vt -p vtmux-server pending_sequences_resize_and_round_trip -- --nocapture`
RESULT: exit 0, 60s; 2 selected tests, 12,000 resize/import cases, 0 selected skips. Full workspace tests later pass with the default 60 seeds.
CONDITIONS: deterministic seeds; source state, both snapshot forms, replies and continuation compared.

GATE: final code batch; `mise exec -- cargo fmt --all && mise exec -- cargo clippy --workspace --all-targets -- -D warnings`
RESULT: first clippy run exit 101 (three new lint errors, 17s); corrected run exit 0 (8s). Slow gate repeats both successfully.
CONDITIONS: warm build; no suppressed lints.

GATE: final code batch (6e7a63c, 2801724, 27f4111, 61a3d49, 3edd2b9, cb85dbd, 52a093d); `muzzle run --class high --rule vtmux-check --run-timeout 3000 -- timeout -k 5 2990 bash /tmp/rv-g7-gate.sh`; wrapper runs `mise exec -- bash scripts/check.sh`
RESULT: exit 0, 274s; fmt/clippy clean; workspace 1,578 passed, 0 failed, 6 ignored; regress 171/174 ordinary passes, 2 flaky rows passed, 1 expected failure, 0 skipped; replay 417/417; replay --upgrade-every 3 417/417 (3,356 upgrades inserted, 2,582 deferred); replay --upgrade-every 1 targeted 9/9 (230 upgrades, 43 deferred). Socket guard passes.
CONDITIONS: shared CentOS 9 devserver, nproc=96; suites parallel; Rust 1.99.1, Zig 0.17.0; warm cache; muzzle 3 slots. Ghostty built offline from cached commit 3bd54e025 with this tree's patches applied by build.rs. macOS cross-target check skipped: `rustup target add aarch64-apple-darwin` failed DNS for static.rust-lang.org. The six ignored tests are three old-binary upgrade tests, two timing tests and one documentation example; no skip counted as a pass.

GATE: final code batch; `muzzle ... -- timeout -k 5 1790 bash /tmp/rv-g7-stress.sh`; five repetitions under `flock /tmp/vtmux-stress.lock` with nproc/2=48 busy loops
RESULT: exit 0, 438s; each repetition passes 63 xtask tests, four long-TMPDIR guards, the missing-reference guard and both bounded resize tests (70 selected tests per repetition). No selected skips. An earlier pre-final five-run stress also passed in its logs (49s), but its wrapper did not correctly capture the exit variable; the final rerun is the exit-status evidence.
CONDITIONS: busy loops cleaned by EXIT trap; ran alongside the slow gate and shell check, with cargo build-lock waits. No command exceeded 30 minutes; the full task did because of red/green checks, follow-up review and gates.

GATE: final code batch; private `unshare --user --map-root-user --mount` run of the same targeted commands with a dash-like `/bin/sh`
RESULT: exit 0, 55s; 70 selected tests pass, 0 selected skips. Probe shows `/bin/sh -c 'sleep 0.3'` retaining its shell instead of tail-execing sleep.
CONDITIONS: namespace-only bind mount; a copied bash runs in POSIX mode with a trailing `exit $?` to disable tail exec. This is dash-like execution, not a claim of testing an actual dash binary. Host /bin/sh was not modified.

## Fresh-context checks and limits

A separate ephemeral pi review challenged the whole diff. It identified that
escaping before replacement hides date/clock boundaries after newlines. A
red/green test confirmed it, and the final code normalizes UTF-8 chunks first,
passing invalid bytes through unchanged. A second fresh-context review of that
fix found no blocking defects. Both review transcripts were read; no mu database
or task verbs were used.

Two out-of-scope production import bugs were exposed, not fixed or hidden by
weaker assertions:

- Removing the combined-fuzzer resize guard fails at seed 57 (60x15, history
  limit 50, cut 1592, resize 61x6, SamePin): three reflowed rows gain EXTENDED
  flags on import. The stale parser explanation was replaced with this reason.
- A pending `CSI 3 CR LF` continuation is replayed by
  `StateImport::finish` through `pane.feed`, moving the restored cursor again
  (source y=2, import y=3). The bounded new cases do not claim C0-continuation
  support; the guard comment names this remaining limitation too.

Teardown captures descendants still parented to the server at shutdown. It does
not claim to recover processes that already double-forked away before capture,
or eliminate the existing pid-reuse race of ps-based cleanup. Backlog saturation
is tested on Linux only; macOS execution remains a CI obligation.
