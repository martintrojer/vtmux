# First checkpoint of a new process, 2026-11-10

The first periodic `full` checkpoint of a server that did not write the checkpoint on disk (a live upgrade's image, or one that restored it), over 20 panes x 40k lines. Before: the writer read the whole previous checkpoint back (every chunk decompressed and hashed) in one loop turn, between `manifest.tmp`'s sync and its rename. After: it reads back only what it does not know, a chunk per piece of the 5 ms step, and a restore seeds what it read.

## Command and host

`cargo test --release -p vtmux-server --lib bench_first_checkpoint -- --ignored --nocapture` under `muzzle run --class high`, on the shared Linux server (Linux 6.16, AMD EPYC Genoa, 96 cpus), load 22-25. Before is `01d348d` with the same bench, less what it lacks (`Known`, the restored rows). Two runs after, one before.

- Real model: 20 windows of 200x50, 40k lines each, `history-limit 40000`; 172 chunks. The export at the start of every `full` checkpoint (1.3 s) is the same before and after and not counted in the turns.
- Synthetic: 20 panes of 40k rows in pages of 290 rows, 2,760 chunks; the history kept, or all new (every chunk of the old `manifest` then read through).
- `turns`: loop turns at the 5 ms step; `pieces`: at a zero step, so `longest` is the longest single piece (the manifest's write and sync, which main has too).

## Before (`01d348d`)

```text
real model, 20 x 40000: 172 chunks
  new process turns: export 1.308259516s, then 1 in 326.38718ms, longest 326.38664ms
  new process pieces: export 1.293468502s, then 1 in 335.054701ms, longest 335.054141ms
synthetic 2760 chunks, kept, new process, turns: begun 6.46644ms, 1 in 359.656189ms, longest 359.621657ms
synthetic 2760 chunks, all new, new process, turns: begun 10.899991ms, 164 in 1.283011386s, longest 335.224427ms
```

## After

```text
real model, 20 x 40000: 172 chunks
  new process turns: export 1.312705833s, then 2 in 11.932964ms, longest 6.544277ms, 172+ chunks read back
  restored turns: export 1.300072985s, then 1 in 5.925016ms, longest 5.924745ms, 0+ chunks read back
  new process pieces: export 1.288188904s, then 176 in 11.460014ms, longest 5.220446ms, 172+ chunks read back
  restored pieces: export 1.300199654s, then 3 in 5.92113ms, longest 5.547267ms, 0+ chunks read back
synthetic 2760 chunks, kept, new process, turns: begun 10.491928ms, 12 in 60.264109ms, longest 5.181166ms, 2760 chunks read back, 0 written
synthetic 2760 chunks, kept, new process, pieces: begun 10.058886ms, 2764 in 58.500866ms, longest 4.879032ms, 2760 chunks read back, 0 written
synthetic 2760 chunks, kept, restored, turns: begun 6.333761ms, 1 in 4.954915ms, longest 4.930888ms, 0 chunks read back, 0 written
synthetic 2760 chunks, all new, new process, turns: begun 9.769991ms, 268 in 1.38677868s, longest 8.164665ms, 2760 chunks read back, 2760 written
synthetic 2760 chunks, all new, new process, pieces: begun 10.480189ms, 5524 in 1.458660661s, longest 6.51289ms, 2760 chunks read back, 2760 written
synthetic 2760 chunks, all new, restored, turns: begun 10.139307ms, 182 in 947.198917ms, longest 9.421898ms, 0 chunks read back, 2760 written
```

The second run after gave the same within noise: longest turn 5.7-9.9 ms, longest piece 4.4-7.8 ms. One earlier run (before the bench took its final shape) had one 182 ms turn in the all-new case, not seen again: a chunk's `fsync` on the shared disk.

## Reading

- The read-back is spread: at most 10 ms a turn (5 ms step plus one piece), from 326-360 ms in one.
- A reused chunk whose bytes equal the page's is checked by comparison, so reading back 2,760 kept chunks costs 60 ms in all; a chunk that differs is decompressed and hashed (the all-new case reads the old `manifest` through that way).
- A restored server reads nothing back.
