# Behaviour oracle

Golden traces from tmux 3.8, replayed against vtmux (spec, *Behaviour oracle*).

```
cargo xtask oracle record  [--only <glob>]   reference tmux 3.8 -> oracle/traces/<name>.json
cargo xtask oracle replay  [--only <glob>]   vtmux vs the committed traces (what CI runs)
cargo xtask oracle live    [--only <glob>]   reference vs vtmux in memory, nothing written
cargo xtask oracle live --self               reference vs reference: determinism check
```

`replay` exits 1 on any mismatch not covered by a `divergence` annotation and prints a unified diff per
mismatching step. Traces are rewritten only by `record`, on a deliberate reference bump or a new/changed
scenario; never edit one by hand. `replay` refuses a trace whose stored scenario differs from the `.toml`.

## Isolation

Every run: environment cleared (no `TMUX`, `TMUX_PANE`; an inherited `$TMUX` would beat `TMUX_TMPDIR`,
tmux.c:557-571), `HOME` and `TMUX_TMPDIR` private temp dirs, `SHELL=/bin/sh`, `PS1='$ '`,
`TERM=xterm-256color`, and every invocation gets `-S /tmp/vto-<6 hex>/s -f /dev/null`. The root is always
under `/tmp`, never `$TMPDIR`, and the runner refuses a socket path of 100 bytes or more (tmux fails at
`sizeof sun_path`, client.c:114-116; 104 on macOS). A `tmux`
symlink to the binary under test is first on `PATH`, so pane programs can `tmux wait-for -S chan`.
Pin pane programs (`cat`, or `sh -c '...; exec cat'`); name windows (`-n w`) when a program's name would
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
| `{ attach = { session = "a" } }` | `attach -t` under a PTY of the scenario size |
| `{ keys = "echo hi\r" }` | raw bytes to the attached client |
| `{ feed = { pane = "%0", bytes } }` | `send-keys -l` |
| `{ resize = [c, r] }` | resize the attached client's PTY |
| `{ settle = {} }` | wait for the output a preceding input provoked, then 150 ms without output from any client (`quiet_ms` overrides) |
| `{ wait_for = "chan" }` | `wait-for chan`; observes like `run` |
| `{ screen = {} }` | settle, then observe the attached client's screen |
| `{ divergence = { id = "D-NNN", expect = {...} } }` | annotates the next observation: replay compares vtmux against `expect` (the documented behaviour from `compat/DIVERGENCES.md`), never skips it |

`expect` uses the stored observation shape, e.g. `{ kind = "cli", stdout = "...", stderr = "", exit = 0 }`.

Screens are observed by feeding the attached client's raw output into a `vtmux_vt::PaneTerm` of the scenario
size and capturing it with escapes plus the cursor; grids are compared, never raw escape bytes (`\e` stands
for ESC in stored rows). Sync uses quiescence and `wait-for`, no fixed sleeps.

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
