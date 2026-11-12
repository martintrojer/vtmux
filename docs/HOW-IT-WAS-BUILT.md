# How vtmux was built

AI agents write most of vtmux. One human runs them and owns the result. tmux 3.8 is the spec, and machines check that vtmux matches it, on every push. Nobody reads every line by hand.

## Who wrote it

- **Agents write the code.** pi agents work as a crew: tasks, workers in their own worktrees, reviews, merges. One human, Martin Trojer, runs the crew, makes the design decisions and decides what lands.
- **Design first.** Every design decision is written down and numbered (D1–D31, D33). A change that contradicts one updates the decision first.
- **The size of it.** 1,177 commits from 29 October to 12 November 2026; about 211k lines of Rust, tests included.

## tmux 3.8 is the spec

- vtmux doesn't guess at what tmux does. Agents read tmux 3.8's source as the reference and cite it where a behaviour isn't obvious: 2,917 comment lines cite 138 of tmux's 155 source files.
- The tests compare against tmux 3.8 built from its release tag, never whatever `tmux` happens to be installed.
- The core is ordinary Rust. tmux's behaviour is reproduced where scripts can see it (output, errors, IDs, ordering), and a test pins each case.

## How it's verified

Mechanically. The gate (`scripts/check.sh`) runs all of this:

| Check | What it does | Today |
|---|---|---|
| tmux's own test suite | the `regress/` tests from the 3.8 tag, unchanged, against vtmux | 173 of 174 pass; the one expected failure is recorded with its reason (it greps tmux's debug log) |
| Behaviour oracle | scenarios recorded once from real tmux 3.8 (CLI output, control-mode streams, screens compared as grids), replayed against vtmux | 443 scenarios; any mismatch fails the gate |
| Live upgrade | the same replay with vtmux upgrading itself in place after every third step, and after every step for a short list of scenarios (in `scripts/check.sh`) | 437 of 443 run with upgrades; 6 skipped, each with the reason written down |
| Divergences | every intended difference from tmux, with evidence, in [compat/DIVERGENCES.md](../compat/DIVERGENCES.md) | 74 entries: 22 tmux bugs not copied, 28 not done yet, 4 opt-in extensions, 9 other deliberate differences, 11 closed (vtmux matches tmux) |
| Lints and unit tests | `cargo fmt`, `clippy` pedantic with warnings as errors, the workspace's tests | 1,615 `#[test]`s |
| CI | the whole gate | Linux on every push; macOS on main, pull requests and weekly |

A behaviour fix lands with an oracle scenario or a regress test that proves it. A test that stops passing fails the gate, and so does one that starts passing while still marked as failing.

## Check it yourself

From a checkout, after `mise trust && mise install`:

```sh
mise exec -- cargo xtask ref-tmux        # build the reference tmux 3.8
mise exec -- cargo xtask regress         # tmux's own tests against vtmux
mise exec -- cargo xtask oracle replay   # the 443 scenarios
mise exec -- cargo xtask oracle replay --upgrade-every 3
mise exec -- bash scripts/check.sh       # the whole gate, as CI runs it
```

## How it's reviewed

- **Every change:** a second model reviews it in a fresh context before it merges.
- **Every milestone:** a review panel drawn from two model families. For 3.8-beta3: 63 findings, every critical and high one fixed and re-reviewed. Panel records with their evidence are in [docs/test-health/](test-health/).

## Unsafe code

- `unsafe` is allowed in two places only (D10): `crates/vtmux-vt`, the wrapper over libghostty-vt, and `crates/vtmux-server/src/sys/`, the OS calls. Every other crate is `#![forbid(unsafe_code)]`, and vtmux-server denies it outside `sys/`.
- 82 `unsafe` blocks in all: 3 in vtmux-vt and 79 in `sys/` (most of them for ptys, live upgrade and user lookup).
- Below that sit the vendored Ghostty bindings (`third_party/libghostty-rs`, 421 `unsafe` blocks, mostly FFI calls) and Ghostty itself (Zig, the libghostty-vt 1.0.1 release) with 42 local patches, each explained in its `VENDOR.md`.

## Not done yet

- No external human audit, of security or of the code.
- No distro packages: install with mise, the release tarballs or `cargo install`.
- No macOS performance numbers yet; the published ones are from Linux.
- It's a beta. The author runs it as his tmux every day.

## Who maintains it

Martin Trojer. Bugs and questions go to [GitHub issues](https://github.com/martintrojer/vtmux/issues).
