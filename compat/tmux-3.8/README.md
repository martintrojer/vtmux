# tmux 3.8 regress suite (vendored)

`regress/` is tmux's own test suite at tag 3.8, commit
`7f2a35ad3321f9ba57a1062ca73b1f3ff26aca53` (174 `*.sh` tests plus fixtures),
taken with `git archive <commit> regress | tar -x -C compat/tmux-3.8/`.

- License: ISC (tmux's `COPYING`: "Copyright (c) Various Authors"), same as vtmux.
- **Never edit `regress/`.** Divergences go in `compat/patches/*.patch` (applied
  to a temp copy by the runner, `git apply -p1`, paths `a/regress/<test>.sh`)
  and are recorded in `compat/DIVERGENCES.md`; waits that fix a test's own
  race go there too, listed under *Test races* below.
- Run with `cargo xtask regress [--tmux vtmux|reference|<path>] [--only <glob>] [--update] [--json <file>]`.
  Expected outcomes live in `compat/expected.toml`.
- Environment per test is tmux's Makefile one: `env -i PATH=/usr/bin:/bin
  LC_CTYPE=C.UTF-8 TERM=screen SHELL=/bin/sh TEST_TMUX=<binary> sh -x <test>`,
  300 s timeout, plus a private short `TMUX_TMPDIR` per script (not tmux's
  Makefile; a test that re-runs a client under its own `env -i` must pass it on:
  `terminal-feature-utf8.sh`, see *Test races*); servers are told apart by the
  tests' own `-L` labels.

## Reference tmux 3.8 on this host

Runs of `cargo xtask regress --tmux reference` (16 cores, default parallelism),
Linux x86_64, 2026-10-29:

| run | result | failures |
|-----|--------|----------|
| 1 | 172/174 | prompt-words-history, screen-redraw-menus |
| 2 | 172/174 | prompt-words-history, screen-redraw-menus |
| 3 | 174/174 | none |
| 4 | 172/174 | prompt-words-history, screen-redraw-menus |

Isolated re-runs (`--only`, no parallel load): `prompt-words-history` failed 4 of
6 times, `screen-redraw-menus` 3 of 4; `terminal-feature-utf8` passed 3 of 3
(it failed under load in an earlier observation).

- **Unreliable on the reference:** `prompt-words-history.sh` (fails most runs:
  prompt result `show-r`, expected `history-command`), `screen-redraw-menus.sh`
  (scene `menu-over-split` differs; timing-dependent), `terminal-feature-utf8.sh`
  (seen failing once under load).
- No test failed in all four runs, so there is no stable failure. In
  `expected.toml`: `prompt-words-history.sh` is `skip` ("fails against reference
  tmux 3.8", it fails most runs); the other two are `flaky = true`.

## Test races

A few tests race their own pane program on the reference too. Where a wait
for the state the test then compares removes the race, it goes in
`compat/patches/` (applied for every `--tmux`, so the reference runs it too)
and is listed here; it is not a divergence, so it has no `D-NNN`.

- `cursor-test1.sh` … `cursor-test4.sh`
  (`0003-cursor-tests-wait-for-pane-output.patch`): each starts the pane with
  `new -d "cat …; printf '\e[…H'; cat"` and runs `display` at once, so the
  first `#{cursor_x} #{cursor_y}` can be from before the printf (`0 9` for
  `14 8 t` in `cursor-test1`). Under 8 parallel runs plus 4 busy loops on
  `taskset -c 0-3`, unpatched: `cursor-test1` failed 4/40 and 8/80 on the
  reference, 1/40 and 3/80 on vtmux; `cursor-test2` 2/40 on vtmux. The patch
  polls `display` (max 10 s) until it prints the result file's first line;
  patched, 40/40 each on both. Seen once in CI (GitHub run 42048753774).
- `copy-mode-test-vi.sh`, `copy-mode-test-emacs.sh`
  (`0004-copy-mode-tests-wait-for-pane-output.patch`): each runs `copy-mode`
  right after `new -d "cat …"`, and copy mode copies the screen when it starts
  (tmux `window_copy_init`), so a pane that has not printed yet leaves an
  empty copy. In `copy-mode-test-vi.sh`'s second part (`printf 'abc中\nxyz\n'`)
  three `cursor-right` on the empty copy give `0,3`, not `3,0`; the reference
  gives `0,3` on an empty pane too. Under 8 parallel runs plus 8 busy loops
  (macOS laptop, debug build), unpatched: 4/48 on vtmux, 0/48 on the
  reference. The patch polls `display` (max 10 s) for the printf's cursor
  position before `copy-mode`; patched, 96/96 vi and 48/48 emacs on vtmux,
  48/48 vi on the reference. Seen once in CI (GitHub run 42157716129, macOS).
- `screen-redraw-scrollbars.sh`
  (`0005-screen-redraw-scrollbars-wait-before-copy-mode.patch`): scene
  `scrollbar-copy-mode` runs `copy-mode -H` right after `neww -d "sh -c 'seq
  40; …'"`; the other scenes' `sleep 1` before capture covers late output, but
  copy mode copies the screen when it starts, so an early entry shows a short
  or empty copy. Failed once in a loaded local `cargo xtask regress` (8 busy
  loops); with `sleep 0.5` before `seq` it fails every run on both vtmux and
  the reference. The patch polls `capturep` (max 10 s) for seq's `40` before
  `copy-mode`; with the delay it then passes on both.
- `terminal-feature-utf8.sh`
  (`0006-terminal-feature-utf8-keeps-the-socket-dir.patch`): the script runs the
  nested client as `env -i PATH=… $TEST_TMUX -T utf8 … attach-session`, which
  drops the harness's private `TMUX_TMPDIR`, so the client looks for the inner
  server in `/tmp/{tmux,vtmux}-<uid>/` and never finds it (`utf8 terminal
  feature did not set client_utf8`; the reference fails the same way, and
  its socket would also outlive the test). The patch passes `TMUX_TMPDIR` on to
  the client (`${TMUX_TMPDIR:+TMUX_TMPDIR=$TMUX_TMPDIR}`, a no-op without
  one) and waits up to 60 s, not 5 s, for the client to attach, which a
  loaded machine can take longer than. Unpatched 0/1 on vtmux; patched it
  passes on both. It was `xfail` and `flaky` before; the `#{I/f:utf8}` lookup
  it also checks is implemented (`vtmux-core` `format/eval.rs`, `ClientInfo`).
- `screen-redraw-floating.sh`
  (`0007-screen-redraw-floating-wait-for-late-scenes.patch`): each scene is
  drawn by the inner server, its attached client, and the outer server, and
  `compare` captures the outer pane once, one second after the scene's last
  command. A host that stalls one of them for longer fails the scene, though
  the scene comes out right later. Failed once in CI (GitHub run 42460821207,
  macOS, under check.sh's parallel regress; Linux passed, and none of the 59
  ci runs before it failed this test). Locally 20/20 idle and 21/21 under 8 busy loops, on 8ec6e32 and
  12/12 on its parent 0f36b68 under the same load. With the servers stopped at
  random for 0.1 to 1 s (SIGSTOP), 7 of 80 scenes took 1.05 to 1.7 s to come
  right, and every one did. With `sleep 1.5` before a floating pane's
  `printf`, the unpatched script fails `floating-basic` on vtmux and the
  reference alike. The patch polls `capturep` (max 10 s more) until the
  capture matches the result file; with the delay it then passes on both.
  `GENERATE=1` still captures once.
