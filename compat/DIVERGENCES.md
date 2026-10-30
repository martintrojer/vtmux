# Divergences from tmux 3.8

vtmux follows tmux 3.8 exactly except where recorded here (spec: *Divergence
process*). Every entry needs evidence. There is no "intentional divergence for
product reasons" category.

## Categories

- **Ghostty grid**: the D4 arbiter ruled for Ghostty's terminal-emulation behaviour over tmux's.
- **Do not copy tmux bug**: tmux loses data, hangs or is unsafe. Needs evidence and a regression test.
- **Deferred**: not done yet. Must not be claimed as compatible.

## Entry template

```
### D-NNN: <short title>

- Test / assertion: <regress test or oracle scenario, and the failing line>
- tmux behaviour: <what tmux 3.8 does>
- vtmux behaviour: <what vtmux does>
- Evidence: <arbiter result, trace, or reproduction>
- Category: Ghostty grid | Do not copy tmux bug | Deferred
- Patch: <compat/patches/NNNN-name.patch, if the regress assertion is patched>
```

## Entries

### D-001: `#{history_bytes}` is empty

- Test / assertion: oracle `panes_split_resize`, the default-template `list-panes` step
  (`[history 0/2000, 120 bytes]` in tmux); every other list-panes comparison uses `-F` without it.
- tmux behaviour: `format_cb_history_bytes` (format.c:996-1020) sums tmux's own grid memory: each line's
  `cellsize * sizeof(struct grid_cell_entry)` plus extended cells plus `sizeof(struct grid_line)` per line.
- vtmux behaviour: the grid is Ghostty's (D4), which has no such per-line allocation; the variable is
  unset, so it expands to the empty string (`[history 0/2000,  bytes]`).
- Evidence: `panes_split_resize` trace; reference `new -d -x 80 -y 24 cat; splitw; lsp` prints
  `480 bytes` for an empty 80x12 pane (40 bytes per row of 80 cells, size of tmux structs).
- Category: Deferred (a byte count modelled on tmux's structs could be emitted; nothing known depends on it).

### D-002: ERE resource limits (`m/r`, `s///`)

- Test / assertion: `crates/vtmux-core/src/regex/tests.rs` `regex_resource_limits_are_errors_not_panics`; `crates/vtmux-core/src/format/matching.rs` `format_matching_limit_is_regexec_failure`.
- tmux behaviour: glibc `regcomp`/`regexec` (format.c:format_match, regsub.c:regsub) have no step cap. Backreference searches backtrack without bound: `(a*)\1b` against 3000 `a` takes 47 s. Compiling fails with `REG_ESPACE` only when memory runs out.
- vtmux behaviour: patterns without backreferences have no work cap; they run in O(text × compiled nodes) (`regex/nfa.rs`). These limits remain and return `Error::Limit`: 64 nested groups or chained quantifiers, 128 groups plus quantifiers, about one million compiled nodes after `{m,n}` expansion (`(a{1000}){1000}` is refused; glibc compiles it), and for backreference patterns a backtracking budget of one million capture-weighted steps per start position and sixteen million per search. The format layer handles a limit the way tmux handles a failed `regexec`: `m/r` gives `0`, and `s///` copies the rest of the text unchanged, keeping earlier replacements. A large `{m,n}` makes matching slow but still correct: `a{32767}` against 32767 `a` takes about 4 s (glibc: 0.07 s).
- Evidence: reviewer-8's `high_regex_work_budget_silently_turns` repros (`[0-9]+%` on 710 digits, `[a-z]+:`, `\w+@`, `[^ ]+ x` on 1000 chars, `(a|b)*c`, `.*z|q`) now match glibc 2.43 and the reference tmux 3.8 (`regex_long_texts_match_like_glibc`). 100k-character texts take < 1 s per search (`regex_without_backreferences_is_linear`). `(a*)\1b` against 3000 `a` gives `Limit` after about 1 ms, where glibc returns no match after 47 s.
- Category: Deferred

### D-003: ERE and fnmatch corner cases

- Test / assertion: none yet (task `regex_corners`); hand probes against glibc 2.43 and tmux 3.8 from review_fmt_regex (reviewer-8).
- tmux behaviour: (1) `^` does not match inside later iterations of a repeated group: `(^a|b)+` on `abab` is no match in glibc. (2) A repeated group whose iterations can match empty reports the last iteration: `(a*){2}` on `aa` gives `\1` = 2:2. (3) With `REG_ICASE` an escaped ordinary letter (`\a`) never matches. (4) The `s///` empty-match progression steps one byte (regsub.c:regsub `start += m[0].rm_eo + 1`), splitting UTF-8 characters. (5) fnmatch `[a-[:digit:]]` does not match `5`.
- vtmux behaviour: (1) `(^a|b)+` on `abab` matches 0:2. (2) without backreferences, captures follow glibc's `set_regs` and `(a*){2}` gives `\1` = 2:2. Backreference patterns use a backtracker whose captures can differ: `(a*){2}\1` on `aa` matches 0:2, where glibc reports no match. Anchors inside repeated groups can also change captures. (3) `\a` with `REG_ICASE` matches `a`. (4) empty-match progression steps one character. (5) `[a-[:digit:]]` matches `5`.
- Evidence: review_fmt_regex notes; probe scripts in that review (`/tmp/rv8probe`, out of tree).
- Category: Deferred
