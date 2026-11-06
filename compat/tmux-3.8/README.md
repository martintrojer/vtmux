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
  120 s timeout. `TMUX_TMPDIR` is not set (`terminal-feature-utf8.sh` re-runs a
  nested client under its own `env -i`); isolation is the tests' own `-L` labels.

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
