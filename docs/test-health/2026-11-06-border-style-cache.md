# Border style cache, 2026-11-06

perf_border_style_cache. `base` is main f9c94f2, `vtmux` the change (e41a9ba). Run: `RUNS=3 NO_BUILD=1 BASE=<f9c94f2 build> VTMUX=<e41a9ba build> python3 scripts/bench.py agent_panes`, under `flock /tmp/vtmux-stress.lock`, raw JSON in `2026-11-06-border-style-cache.json`. Server CPU % is CPU seconds per 100 s.

2026-11-06 08:20 UTC, commit e41a9ba, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 4.2, median of 3; tmux: tmux 3.8; base: vtmux 0.0.0 (tmux 3.8); vtmux: vtmux 0.0.0 (tmux 3.8)

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 263 | 6.1 |
| 5 panes / 1 clients | base | 1.6 | 0.2 | 14.3 | 68 | 233 | 17.7 |
| 5 panes / 1 clients | vtmux | 1.4 | 0.2 | 14.3 | 68 | 234 | 17.7 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 262 | 6.2 |
| 5 panes / 2 clients | base | 2.4 | 0.3 | 28.2 | 134 | 269 | 20.2 |
| 5 panes / 2 clients | vtmux | 1.9 | 0.3 | 28.3 | 135 | 296 | 20.2 |
| 10 panes / 1 clients | tmux | 2.2 | 0.0 | 15.2 | 68 | 477 | 6.6 |
| 10 panes / 1 clients | base | 2.2 | 0.2 | 14.3 | 68 | 398 | 21.6 |
| 10 panes / 1 clients | vtmux | 2.1 | 0.2 | 14.3 | 68 | 397 | 21.7 |
| 10 panes / 2 clients | tmux | 2.3 | 0.0 | 30.4 | 135 | 476 | 6.7 |
| 10 panes / 2 clients | base | 3.2 | 0.3 | 28.2 | 134 | 449 | 24.0 |
| 10 panes / 2 clients | vtmux | 2.7 | 0.3 | 28.2 | 135 | 457 | 24.0 |
| 20 panes / 1 clients | tmux | 4.1 | 0.0 | 15.2 | 68 | 857 | 8.0 |
| 20 panes / 1 clients | base | 3.8 | 0.2 | 14.3 | 68 | 699 | 31.2 |
| 20 panes / 1 clients | vtmux | 3.5 | 0.2 | 14.3 | 68 | 713 | 31.2 |
| 20 panes / 2 clients | tmux | 4.5 | 0.0 | 30.5 | 135 | 864 | 8.1 |
| 20 panes / 2 clients | base | 5.4 | 0.3 | 28.3 | 135 | 749 | 33.6 |
| 20 panes / 2 clients | vtmux | 4.4 | 0.3 | 28.3 | 134 | 765 | 33.6 |

vtmux / tmux, server CPU (unrounded): 0.98x, 1.28x, 0.96x, 1.14x, 0.85x, 0.97x (base: 1.16x, 1.55x, 1.02x, 1.38x, 0.91x, 1.19x).

## Steps at 5 panes / 2 clients (RUNS=5 unless noted, same host, load 4.6-6.9 at start; one run started at 26.7)

| build | tmux | base | vtmux | vtmux / tmux |
|---|---|---|---|---|
| 6b9812f (RUNS=3) | 1.47 | | 2.42 | 1.64x |
| border style cache (RUNS=3) | 1.61 | 2.37 | 2.14 | 1.33x |
| + pane span copies | 1.56 | 2.35 | 2.15 | 1.38x |
| + option misses looked up once | 1.47 | 2.36 | 2.11 | 1.43x |
| + unstyled cells skip Ghostty's style | 1.51 | 2.38 | 2.07 | 1.37x |
| + lazy client list (load 26.7) | 1.51 | 2.42 | 2.01 | 1.34x |

tmux itself moves 1.47-1.61 between runs, so a ratio moves about 0.1 with no change; `base` (2.35-2.42) is steadier.

## Profile (5 panes / 2 clients, `perf record -F 15000 -g -p <server>` for 30 s, frame pointers, ReleaseFast Ghostty; before the lazy client list)

Share of the server's samples (17,340); perf's own overhead inflates the CPU, not the split.

```
Server::run 93.0%
  render_clients 61.3%
    draw_window 26.5%
      compose_cached 11.7% (compose_row 7.8%: draw_line_keeping 4.3%, span copy 1.4%; row_signature 2.0%)
      Source::window 8.6% (number_option 1.3%, pane_screen 1.2%, style 1.1%, pane_defaults 0.5%)
      copy_rows 2.9%
      render_source 1.7% (the client list; built on first use since)
    PaneTerm::take_sync_flush 16.4% (ScreenView::update: reading Ghostty's dirty rows)
    Compositor::frame_tagged 8.9% (changed_rows 4.3%)
    send_as 2.6%, draw_status 1.6%
  pane input and parsing 17.0%
  epoll_wait 4.7%
```

Before the change (6b9812f, 4 kHz for 8 s) `Source::window` was 15.6%, half of it the two border style formats expanded per pane per frame; `border_style` is now 0.1%. What is left is spread: Ghostty's row reads, parsing (which tmux pays too), composing and diffing the changed rows, and the window's drawing inputs.
