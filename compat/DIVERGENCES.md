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

None yet.
