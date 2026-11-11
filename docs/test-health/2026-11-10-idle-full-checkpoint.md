# An idle `full` checkpoint

Base `be790b5` plus the fix. Host: devserver, 96 cores, shared with other workers, load 33-40.

**Release** (`cargo test --release -p vtmux-server --lib bench_checkpoint_ten -- --ignored --nocapture`): 10 panes x 100k lines, 200x50.

| step | before | after |
|---|---|---|
| full export (what every check paid) | 1.4-1.7 s | 1.67 s |
| full unchanged (the 60 s check, nothing changed) | the export above | 8.1 ms |
| full first | | 1.82 s, 429 chunks |

Before the fix `full unchanged` built the whole snapshot, exporting every terminal, and then compared fingerprints; it now compares a precheck (the checkpoint without terminals, plus each pane's output count, history size and terminal state outside its rows) and stops there.

**Debug probe** (`/tmp` script, not kept): 2 panes x 50k lines, `VTMUX_PERSIST_INTERVAL_MS=2000`, idle, manifest not rewritten, 40 `display -p x` 150 ms apart.

| build | median | max |
|---|---|---|
| before | 3.8 ms | 182.9 ms |
| after | 3.8 ms | 4.5 ms |

The test is `an_unchanged_full_checkpoint_exports_no_terminal` in `crates/vtmux-server/src/server/persist/tests.rs`.
