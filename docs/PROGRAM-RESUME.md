# Program resume (OSC 7577)

A program in a pane can tell vtmux how to start it again: its command line, its working directory and its pid. vtmux records that and shows it in formats and a hook. It never relaunches anything; a script or `tmux-resurrect` does.

The program speaks OSC 7577 (Program Resume Protocol) and proves it runs in this pane with a per-pane token.

## Enable

```
%if "#{vtmux_version}"
set -s vtmux-program-resume on
%endif
```

The guard keeps the file working under tmux, which has no such option. The default is `off`: vtmux then matches tmux, and no pane gets a token.

Only non-empty panes started (or respawned) while the option is `on` get a token. Turning it on does not grant panes already running. Turning it off keeps the grant until the pane is respawned or closed.

## Token hygiene

- An inherited `OSC7577_TOKEN` is always removed, with the option on or off: from pane and popup environments, the global and session environment, the client environment, `update-environment` and an imported snapshot. A pane never passes its token on.
- The always-on stripping is a security exception to the rule that the option off matches tmux.
- Each granted pane gets a fresh 32-character token (192 random bits from the OS CSPRNG, `[A-Za-z0-9_-]`) in its environment when it starts and again on respawn. Popups get none.
- A live upgrade to a compatible version keeps the token, the records and the last accepted time per pid. A downgrade to a version without this feature drops grants and records.

## Formats and hook

| Format | Value |
|---|---|
| `#{pane_resume_command}` | the reported `argv`, each word shell-quoted (POSIX), ready to paste into `sh` |
| `#{pane_resume_cwd}` | the reported `cwd`, else the program's working directory when it reported |
| `#{pane_resume_pid}` | the pid that reported |

All three are empty when the pane has no record. They are not listed by `display-message -a`. When a pane has several records, vtmux prefers the one for an ancestor of the others, else the most recent.

The pane hook `vtmux-pane-program-resume` runs when a pane's resume state changes. Never put `#{pane_resume_command}` into a shell command: it holds program-chosen text. Pass only the pane id (`%` and digits) and let a script read the formats as data:

```
set-hook -g vtmux-pane-program-resume 'run-shell "~/bin/log-resume.sh #{pane_id}"'
```

```sh
#!/bin/sh
# ~/bin/log-resume.sh: append "pane|cwd|command" for pane $1
vtmux display-message -p -t "$1" -F '#{pane_id}|#{pane_resume_cwd}|#{pane_resume_command}' >> "$HOME/.cache/resume.log"
```

### Shell script

Print what to run again in every pane. Each field is a separate query per pane, so `cwd` and `cmd` round-trip verbatim (spaces, quotes, `|`, tabs) and an empty `cwd` stays empty. The script quotes `cwd` and skips `cd` when it is empty:

```sh
vtmux list-panes -a -F '#{pane_id}' |
while IFS= read -r pane; do
  cmd=$(vtmux display-message -p -t "$pane" '#{pane_resume_command}')
  [ -n "$cmd" ] || continue
  cwd=$(vtmux display-message -p -t "$pane" '#{pane_resume_cwd}')
  if [ -n "$cwd" ]; then
    q=$(printf %s "$cwd" | sed "s/'/'\\\\''/g")
    printf '%s\n' "cd '$q' && $cmd"
  else
    printf '%s\n' "$cmd"
  fi
done
```

`cmd` is already shell-quoted, so the output is safe to run with `sh`. A `cwd` or `cmd` containing a newline is not supported.

### tmux-resurrect

tmux-resurrect saves `#{pane_current_command}`. To restore the reported command instead, a restore step can query each live pane and feed the line back with `send-keys -l`, which sends the text literally:

```sh
quote() { printf "'%s'" "$(printf %s "$1" | sed "s/'/'\\\\''/g")"; }
vtmux list-panes -a -F '#{pane_id}' |
while IFS= read -r pane; do
  cmd=$(vtmux display-message -p -t "$pane" '#{pane_resume_command}')
  [ -n "$cmd" ] || continue
  cwd=$(vtmux display-message -p -t "$pane" '#{pane_resume_cwd}')
  line=$cmd
  [ -n "$cwd" ] && line="cd $(quote "$cwd") && $cmd"
  vtmux send-keys -t "$pane" -l "$line" && vtmux send-keys -t "$pane" Enter
done
```

## Security and lifetime

- **MAC.** A report carries `mac=` (64 hex digits): SHA-256 over the raw body before `:mac=`, then `:`, then the pane's token. A report without a valid MAC, or whose `mac=` is not last, is ignored.
- **Time.** The report's `time` must be within ±60 s of the server clock. Each report's time is compared with the last accepted time for its pid: an older one is ignored.
- **Reject.** A report that passes MAC and time but is malformed (bad pair, duplicate key, missing key, both or neither of `argv` and `clear`, invalid base64 or UTF-8, control characters, empty `argv[0]`, a relative path or `-`-prefixed `argv[0]` (a bare command name not starting with `-`, such as `vim`, is legal), relative `cwd`, more than 128 arguments, a pid that is dead or not yours) removes that pid's records.
- **Size.** A body over 4096 bytes, or one cut short by `CAN`, `SUB` or an unfinished terminator, is dropped.
- **Clear.** `clear=1` removes the pid's record.
- **Not cleared by the terminal.** `RIS`, `DECSTR`, switching to or from the alternate screen and OSC 133 do not clear records.
- **Liveness.** A record lasts while its pid is alive. vtmux compares the process start time so a reused pid does not keep an old record. A pane holds at most 64 records.
- **Pane end.** Respawn or close of the pane drops the records and the grant.
