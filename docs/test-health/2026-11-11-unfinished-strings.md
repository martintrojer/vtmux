# Unfinished OSC and APC strings, memory

Base `e7aa58f` plus the fix. Host: devserver, 96 cores, load 9-12 (other workers). Release build, `VmRSS` of the server.

Command: 12 panes (`new-session` + 11 `split-window`), each running `printf '<lead>'; head -c 8388608 /dev/zero | tr '\0' a; sleep 30`: a string opened and never ended, 8 MiB of text in it. RSS read 8 s later. `/tmp` probe script, not kept: the test is `crates/vtmux-vt/tests/string_memory.rs`.

| lead | before | after | tmux 3.8 |
|---|---|---|---|
| OSC 52 (`ESC ] 52;c;`) | 146.7 MB | 48.7-49.3 MB (3 runs) | 165.3 MB |
| APC (`ESC _ x`) | 48.3 MB | 48.5 MB | |
| OSC 2 (`ESC ] 2;`) | not run | 48.5-48.7 MB | |
| 12 idle panes, nothing sent | 11.1 MB | | |

Reading: Ghostty's OSC parser kept up to 8 MiB of every OSC it can collect (OSC 52 is one), 9 MB a pane. tmux discards a string past `input-buffer-size` (1 MiB); the pane's scanner now counts the same bytes and, past the limit, gives Ghostty an OSC it rejects at its first byte, so nothing more is kept. The APC was already bounded (the pane keeps at most 2 MiB of pending input and 1 MiB of a title being read, 3.1 MB a pane); it is the same figure after.

`cargo test -p vtmux-vt --test string_memory` (4 panes, 9 MiB each): OSC 52 46.6 MB before, 14.8 MB after; the test allows 20 MB.
