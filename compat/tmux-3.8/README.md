# tmux 3.8 regress suite (vendored)

`regress/` is tmux's own test suite at tag 3.8, commit
`7f2a35ad3321f9ba57a1062ca73b1f3ff26aca53` (174 `*.sh` tests plus fixtures),
taken with `git archive <commit> regress | tar -x -C compat/tmux-3.8/`.

- License: ISC (tmux's `COPYING`: "Copyright (c) Various Authors"), same as vtmux.
- **Never edit `regress/`.** Divergences go in `compat/patches/*.patch` (applied
  to a temp copy by the runner, `git apply -p1`, paths `a/regress/<test>.sh`)
  and are recorded in `compat/DIVERGENCES.md`.
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
