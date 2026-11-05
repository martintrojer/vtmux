# agent_panes, 2026-11-05

perf_agent_panes. `base` is the vtmux build the change sits on, `vtmux` the change. Both runs: `RUNS=3 NO_BUILD=1 BASE=<base> VTMUX=<change> python3 scripts/bench.py agent_panes`, under `flock /tmp/vtmux-stress.lock`. Server CPU % is CPU seconds per 100 s (divide by 10 for seconds per 10 s).

## On main 3682ca7 (the table in PERFORMANCE.md)

2026-11-05 11:36 UTC, commit 8902002, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 2.8, median of 3; tmux: tmux 3.8; base: vtmux 0.0.0 (tmux 3.8); vtmux: vtmux 0.0.0 (tmux 3.8)

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 264 | 6.1 |
| 5 panes / 1 clients | base | 8.1 | 0.2 | 14.2 | 67 | 206 | 22.2 |
| 5 panes / 1 clients | vtmux | 1.6 | 0.2 | 14.3 | 68 | 233 | 21.4 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 265 | 6.2 |
| 5 panes / 2 clients | base | 14.3 | 0.3 | 27.7 | 133 | 204 | 25.7 |
| 5 panes / 2 clients | vtmux | 2.4 | 0.3 | 28.1 | 135 | 292 | 24.9 |
| 10 panes / 1 clients | tmux | 2.3 | 0.0 | 15.2 | 68 | 477 | 6.6 |
| 10 panes / 1 clients | base | 12.4 | 0.1 | 14.2 | 67 | 358 | 28.5 |
| 10 panes / 1 clients | vtmux | 2.3 | 0.2 | 14.3 | 68 | 396 | 27.8 |
| 10 panes / 2 clients | tmux | 2.3 | 0.0 | 30.4 | 135 | 474 | 6.8 |
| 10 panes / 2 clients | base | 20.3 | 0.3 | 27.7 | 134 | 331 | 32.0 |
| 10 panes / 2 clients | vtmux | 3.3 | 0.3 | 28.2 | 134 | 444 | 31.2 |
| 20 panes / 1 clients | tmux | 4.3 | 0.0 | 15.2 | 68 | 855 | 8.0 |
| 20 panes / 1 clients | base | 22.2 | 0.2 | 14.2 | 67 | 578 | 43.5 |
| 20 panes / 1 clients | vtmux | 3.9 | 0.2 | 14.3 | 68 | 713 | 42.3 |
| 20 panes / 2 clients | tmux | 4.5 | 0.0 | 30.5 | 135 | 852 | 8.1 |
| 20 panes / 2 clients | base | 33.6 | 0.3 | 27.8 | 134 | 513 | 46.4 |
| 20 panes / 2 clients | vtmux | 5.3 | 0.3 | 28.3 | 135 | 761 | 45.7 |


## The same change on e3778ca, before the rebase (load 3.3)

2026-11-05 11:04 UTC, commit a3b71dd+dirty, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 3.3, median of 3; tmux: tmux 3.8; base: vtmux 0.0.0 (tmux 3.8); vtmux: vtmux 0.0.0 (tmux 3.8)

### agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; 1/2 200x50 clients with sync on, the second on window 1; 10 s)

| case | binary | server CPU % | clients CPU % (sum) | terminal KB/s (sum) | sync frames/s (sum) | server wakeups/s | RSS MB |
|---|---|---|---|---|---|---|---|
| 5 panes / 1 clients | tmux | 1.4 | 0.0 | 15.2 | 68 | 261 | 6.1 |
| 5 panes / 1 clients | base | 7.9 | 0.2 | 9.4 | 67 | 218 | 21.6 |
| 5 panes / 1 clients | vtmux | 1.6 | 0.2 | 9.4 | 67 | 232 | 20.9 |
| 5 panes / 2 clients | tmux | 1.5 | 0.0 | 30.4 | 135 | 262 | 6.2 |
| 5 panes / 2 clients | base | 13.9 | 0.3 | 17.8 | 133 | 205 | 25.1 |
| 5 panes / 2 clients | vtmux | 2.3 | 0.3 | 17.8 | 134 | 279 | 24.4 |
| 10 panes / 1 clients | tmux | 2.2 | 0.0 | 15.2 | 68 | 482 | 6.6 |
| 10 panes / 1 clients | base | 11.3 | 0.1 | 9.4 | 67 | 350 | 26.6 |
| 10 panes / 1 clients | vtmux | 2.2 | 0.2 | 9.4 | 68 | 395 | 26.5 |
| 10 panes / 2 clients | tmux | 2.3 | 0.0 | 30.4 | 135 | 471 | 6.7 |
| 10 panes / 2 clients | base | 19.4 | 0.3 | 17.8 | 133 | 315 | 30.2 |
| 10 panes / 2 clients | vtmux | 3.1 | 0.3 | 17.8 | 135 | 450 | 29.9 |
| 20 panes / 1 clients | tmux | 4.5 | 0.0 | 15.2 | 68 | 865 | 8.0 |
| 20 panes / 1 clients | base | 20.1 | 0.2 | 9.4 | 68 | 585 | 40.3 |
| 20 panes / 1 clients | vtmux | 3.6 | 0.1 | 9.4 | 68 | 713 | 40.0 |
| 20 panes / 2 clients | tmux | 4.4 | 0.0 | 30.5 | 135 | 855 | 8.1 |
| 20 panes / 2 clients | base | 31.5 | 0.3 | 17.8 | 133 | 487 | 43.7 |
| 20 panes / 2 clients | vtmux | 5.1 | 0.3 | 17.8 | 135 | 762 | 43.4 |


## Profile after the change (5 panes / 2 clients, `perf record -F 4000 -g`, frame pointers, 10 s)

```
Server>::run: 1526/1712 = 89.1%
 63.84%  render_and_exit (inlined)
 15.54%  <vtmux_server::server::Server>::dispatch
  3.56%  epoll_wait
  0.82%  <vtmux_server::server::Server>::pane_readable
  0.82%  clock_gettime@@GLIBC_2.17
  0.76%  next_timeout (inlined)
Server>::render_clients: 1080/1712 = 63.1%
 37.97%  draw_window (inlined)
 11.16%  <vtmux_vt::term::PaneTerm>::take_sync_flush
  5.14%  <vtmux_server::render::Compositor>::frame_tagged
  2.28%  draw_status (inlined)
  2.22%  <vtmux_server::server::Server>::send_as
  0.82%  is_empty<u8, alloc::alloc::Global> (inlined)
draw_window: 650/1712 = 38.0%
 16.12%  map<&vtmux_server::render::WindowCache, bool, vtmux_server::server::{impl#5}::draw_window::{closure_env#4}> (inlined)
 15.60%  <vtmux_server::render::window::Source>::window
  4.15%  drop_glue<core::cell::Ref<vtmux_server::render::compose::ComposeCache>> (inlined)
  1.40%  render_source (inlined)
  0.23%  borrow_mut<alloc::collections::btree::map::BTreeMap<vtmux_core::ids::PaneId, alloc::vec::Vec<vtmux_core::styled::StyleRange, alloc::alloc::Global>, alloc::alloc::Global>> (inlined)
  0.06%  drop_glue<alloc::rc::Rc<vtmux_server::render::scene::Scene, alloc::alloc::Global>> (inlined)
Source>::window: 267/1712 = 15.6%
  8.06%  new<vtmux_core::styled::Cell> (inlined)
  1.29%  number_option (inlined)
  1.29%  <vtmux_server::render::window::Source>::style
  1.17%  pane_screen (inlined)
  0.70%  <vtmux_server::model::Model>::pane_index
  0.47%  vtmux_server::render::window::two_pane
```
