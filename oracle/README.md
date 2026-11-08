# Behaviour oracle

Golden traces from tmux 3.8, replayed against vtmux (spec, *Behaviour oracle*).

```
cargo xtask oracle record  [--only <glob>]   reference tmux 3.8 -> oracle/traces/<name>.json
cargo xtask oracle replay  [--only <glob>]   vtmux vs the committed traces (what CI runs)
cargo xtask oracle live    [--only <glob>]   reference vs vtmux in memory, nothing written
cargo xtask oracle live --self               reference vs reference: determinism check
cargo xtask oracle replay --upgrade-every <n>  replay, vtmux upgraded onto itself after every nth step
```

`--only` takes globs separated by commas (`a_*,b`). `replay` exits 1 on any mismatch not covered by a `divergence` annotation and prints a unified diff per
mismatching step. Traces are rewritten only by `record`, on a deliberate reference bump or a new/changed
scenario; never edit one by hand. `replay` refuses a trace whose stored scenario differs from the `.toml`.

## Isolation

Every run: environment cleared (no `TMUX`, `TMUX_PANE`; an inherited `$TMUX` would beat `TMUX_TMPDIR`,
tmux.c:557-571), `HOME` and `TMUX_TMPDIR` private temp dirs, `SHELL=/bin/sh`, `PS1='$ '`,
`TERM=xterm-256color`, and every invocation gets `-S /tmp/vto-<6 hex>/s -f /dev/null`. The root is always
under `/tmp`, never `$TMPDIR`, and the runner refuses a socket path of 100 bytes or more (tmux fails at
`sizeof sun_path`, client.c:114-116; 104 on macOS). A `tmux`
symlink to the binary under test is first on `PATH`, so pane programs can `tmux wait-for -S chan`.
Pin pane programs as separate arguments, two words at least (`cat`, `-`; or `sh`, `-c`, `...; exec cat`): a one-word command runs as `$SHELL -c word`, and under dash (Ubuntu's `/bin/sh`) the shell stays in front, so `#{pane_current_command}` is `sh`. `cargo test -p xtask` fails on a scenario that starts a pane (`new`, `neww`, `splitw`, `respawn-pane`, `new-pane`, ...) with one word, in `run`, `control` and `control_send`; name windows (`-n w`) when a program's name would
otherwise race a `%window-renamed`.

## Adding a scenario

Create `oracle/scenarios/<name>.toml` (the file stem must equal `name`), then `cargo xtask oracle record
--only <name>`, read the trace, run `live --self` a few times, commit both files.

```toml
name = "example"
size = [80, 24]            # attached client size, default 80x24
steps = [ ... ]
```

| step | effect / observation |
|---|---|
| `{ run = ["new","-d","-s","a"] }` | CLI call; observes stdout, stderr, exit |
| `{ setup = "stale_socket" }` | bound-then-closed socket file at the scenario's socket path |
| `{ control = { argv = [...], id = "c1" } }` | start an attached `-C` client (an unattached `-C new -d` exits at once, server-client.c:2827-2833) |
| `{ control_send = { id, line } }` | one line to its stdin |
| `{ control_wait = { id, until = "%end" } }` | wait (max 5 s) for a line starting with that after the last send |
| `{ control_close = { id } }` | EOF, read to exit; observes the whole stream |
| `{ attach = { session = "a" } }` | `attach -t` under a PTY of the scenario size; returns after the client's first draw, so clients connect (and `lsc` lists them) in step order |
| `{ attach = { session = "a", id = "b" } }` | a further attached client, named `b`; `keys`, `resize`, `screen` and `title` without an id use the first |
| `{ attach = { session = "a", term = "linux" } }` | the client's `TERM` instead of `xterm-256color`; only terminals in every host's terminfo (ncurses' base set: `linux`, `vt100`, `screen`, `tmux`) |
| `{ keys = "echo hi\r" }` | raw bytes to the attached client |
| `{ keys_to = { id = "b", keys = "q" } }` | raw bytes to attached client `b` |
| `{ feed = { pane = "%0", bytes } }` | `send-keys -l` |
| `{ resize = [c, r] }` | resize the attached client's PTY |
| `{ settle = {} }` | wait for the output a preceding input provoked, then 150 ms without output from any client (`quiet_ms` overrides); `react = true` first waits for any output since the previous settle, for steps that provoke a reaction without keys (a CLI `resize-window` that sends a pane SIGWINCH after its 250 ms timer) |
| `{ wait_for = "chan" }` | `wait-for chan`; observes like `run` |
| `{ screen = {} }` | settle, then observe the attached client's screen (`{ id = "b" }`: client `b`'s) |
| `{ title = {} }` | settle, then observe the attached client's terminal title (OSC 0/2 from `set-titles`) as a `cli` observation: the title and a newline |
| `{ divergence = { id = "D-NNN", expect = {...} } }` | annotates the next observation: replay compares vtmux against `expect` (the documented behaviour from `compat/DIVERGENCES.md`), never skips it |
| `{ upgrade = {} }` | vtmux only: the server moves onto the binary under test in place (the frozen `UpgradeRequest`, as `vtmux-upgrade -b` asks, which leaves no line in `show-messages`), then every attached client re-executes, re-attaches and its redraw settles; the reference tmux skips the step. Observes nothing when it goes through, so what follows must match what tmux shows with no upgrade |

`expect` uses the stored observation shape, e.g. `{ kind = "cli", stdout = "...", stderr = "", exit = 0 }`.

Screens are observed by feeding the attached client's raw output into a `vtmux_vt::PaneTerm` of the scenario
size and capturing it with escapes plus the cursor; grids are compared, never raw escape bytes (`\e` stands
for ESC in stored rows). Sync uses quiescence and `wait-for`, no fixed sleeps. A scenario that prints pane SIGWINCH counts
(`W1.W2.`) puts `{ settle = { react = true, quiet_ms = 500 } }` before each `screen`, so the 250 ms resize
timer has run out and the count is final; one CLI step then provokes at most one SIGWINCH per pane
between two observations (chain commands with `;` in one `run`, otherwise the timer coalesces them
depending on load).

## Upgrades between steps

`replay --upgrade-every <n>` (in `scripts/check.sh`, so in CI, with `n` = 3) inserts an `upgrade` after every
nth step of every scenario, and every scenario must still match its trace (live-upgrade spec, *Tests*). An
inserted upgrade never comes between an input and the `settle` or `control_wait` that waits for its reaction,
nor after the last step; it first settles. The runs set `VTMUX_TEST_UPGRADE_STRICT=1` for vtmux (a server
hook, never in the global environment): where an upgrade would wait for work in flight (a pane's `tmux
wait-for`, a job) or change what a client sees (a mode, prompt or menu it closes by design, a status message,
a key held for `escape-time`, a mouse drag, a locked client), the server refuses at once and the runner tries after the next
step. The summary counts upgrades inserted and put off. Scenarios whose observations an upgrade changes by
design with no state to check, or that hit a known bug, are listed with why in `oracle/upgrade-every-skip.toml`
and run without inserted upgrades; the replay prints the reason. `check.sh` also replays the scenarios that once
failed only with an upgrade after every step (`--upgrade-every 1 --only <list>`, the list in the script).

## Normalisation

Applied before storing or diffing:

- socket (`-S`) -> `<SOCKET>`, temp root -> `<TMP>`
- `$TMUX` pid (`<SOCKET>,1234,0`) -> `<PID>`
- ctime-style dates (`Thu Oct 29 17:17:32 2026`, from `#{t:session_created}`) -> `<DATE>`
- status-line clocks (`%H:%M %d-%b-%y`) -> `<CLOCK>`
- tty names (`/dev/pts/3`) -> `<TTY>`
- hostnames: scenarios never depend on the hostname (pin pane titles with `select-pane -T`). Traces store
  `recorded_host`; replaying on another host replaces the recorded name on the trace side, and the current
  name on the vtmux side (only in steps whose trace has one), with `<HOST>`, whole token only (not adjacent to
  `[A-Za-z0-9_.-]`). On the recording host nothing is replaced.
- control streams: `%begin/%end/%error` timestamps -> `<TIME>`; guard command numbers renumbered by first
  appearance (`#1`, `#2`; absolute numbers count internal queue items, cmd-queue.c:713,745-746); consecutive
  `%output %N` lines per pane coalesced into one decoded payload between other events (chunking depends on
  buffering, control.c:807-855, 936-948)

## Comparison

Observations are matched by step index (and order within a step), so a timeout or a step that observes
nothing never shifts later diffs onto the wrong step; unmatched items are reported per side. `live --self`
ignores `divergence` annotations (both sides are the reference); `replay` and `live` judge annotated steps
against their `expect`.

**Host names on screen.** A screen showing the recording machine's host name (the default `status-right`, or a pane title in `status-format[1]`) has padding that depends on the name's length, which no normalisation can undo elsewhere. `oracle record` refuses such a screen: pin `status-right` and the pane titles (`select-pane -T`). Pin the title of every pane a scenario creates, including ones split mid-scenario: a new pane's title is the host name, and screens that hide it only when it equals `#{host_short}` (the tree's default format) change on a host whose name has dots, which `record` can't see from a dotless one.

**Races under load.** CI runners are slower than a laptop. A `screen` right after a `run` that changes the screen (zoom, resize, `display-message`, option changes) should be preceded by `{ settle = { react = true } }`, which waits for output since the last settle. A pane program that changes tty modes (`stty raw`) must signal with `tmux wait-for -S` and the scenario must `wait_for` it before sending keys. Check a new scenario with `oracle replay` five times under a CPU hog (`nproc` busy loops) before committing.
