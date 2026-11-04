# Pane blend cost, pane_blend, 2026-11-04

`scripts/bench.py` with `BLEND=1` against the same release build without it: the attached scenarios set `vtmux-pane-inactive-blend 'fg=bg/25,bg=bg/25'` and the reader answers OSC 10/11 (Catppuccin Mocha's `#cdd6f4` / `#1e1e2e`), so every inactive pane's cells are blended in every composed row. Linux 7.2.11 (Fedora 44), AMD Ryzen 7 5700X3D (16 cpus), load 1.5-4, under `flock /tmp/vtmux-stress.lock`, off and on interleaved. The spec's `agent_panes` bench does not exist yet (`perf_agent_panes` is adding it), so `render` and `throughput_attached` stand in.

## render (4 tiled panes, 3 inactive, 200x50 client), server CPU s, median of 7

`RENDER_FRAMES=6000 RUNS=7`, three off/on pairs:

| run | sync off: off | sync off: on | sync on: off | sync on: on |
|---|---|---|---|---|
| 1 | 0.70 | 0.71 | 0.70 | 0.70 |
| 2 | 0.70 | 0.72 | 0.70 | 0.70 |
| 3 | 0.71 | 0.71 | 0.72 | 0.70 |

On is within 3% of off (the noise between runs). Terminal bytes go from 1.02 to 1.32 MB: the blended colours are `38;2`/`48;2` SGRs where the panes had `3x` ones.

## throughput_attached (one pane, so the active one: blend unused), server CPU s, median of 5

`THROUGHPUT_MB=50 RUNS=5`, two pairs: plain 0.63-0.68 off, 0.61-0.67 on; sgr 1.54-1.57 off, 1.54-1.58 on. With the inactive blend set and no inactive pane the cost is resolving the option per frame: not measurable.

Commands:

```
OUT=target/bench/pb2-$mode-$r NO_BUILD=1 RUNS=7 RENDER_FRAMES=6000 BLEND=$b flock -w 1800 /tmp/vtmux-stress.lock mise exec -- python3 scripts/bench.py render
OUT=target/bench/pb3-$mode-$r NO_BUILD=1 RUNS=5 THROUGHPUT_MB=50 BLEND=$b flock -w 1800 /tmp/vtmux-stress.lock mise exec -- python3 scripts/bench.py throughput_attached
```
