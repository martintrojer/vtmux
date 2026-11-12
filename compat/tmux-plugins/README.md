# tmux-resurrect and tmux-continuum (test fixtures)

Copies of [tmux-resurrect](https://github.com/tmux-plugins/tmux-resurrect) (master after v4.0.0) and [tmux-continuum](https://github.com/tmux-plugins/tmux-continuum) (master after v3.1.0), both MIT (`LICENSE.md` in each), Copyright Bruno Sutic. `crates/vtmux/tests/resurrect.rs` runs them against vtmux and the reference tmux 3.8 and requires the two to match.

They are vendored rather than fetched so the test needs no network, runs the same plugin code on every host and in CI (Linux and macOS), and a plugin release can't change what the test proves without a commit. Only what the plugins run is kept: their `docs/`, `tests/`, `video/` and the `lib/tmux-test` submodule are left out. Never edit these files; to bump, copy a newer release over them and rerun the test.
