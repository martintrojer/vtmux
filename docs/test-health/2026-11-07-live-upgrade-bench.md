# Live upgrade bench, 2026-11-07

`NO_BUILD=1 RUNS=3 OUT=/tmp/lut-bench-upgrade2 python3 scripts/bench.py upgrade` on the release build of
`5ae699c` (lu_upgrade_tests, rebased onto e61fbe3 and 55a4739), on the shared devserver (load 14.8 at the start,
up to about 45 earlier in the day from other workers' builds; numbers are noisy). An earlier run on `40e06e1`
(before 55a4739, load 49.8) gave the same picture within 20%: 10 x 100k idle 1670 ms, fresh refused 3 of 3.
The JSON is [`2026-11-07-live-upgrade-bench.json`](2026-11-07-live-upgrade-bench.json). What each column means:
[PERFORMANCE.md](../PERFORMANCE.md), *Live upgrade*.

2026-11-07 17:26 UTC, commit 5ae699c, Linux-6.16.1-0_fbk5_0_g0e127f210adc-x86_64-with-glibc2.34, AMD EPYC-Genoa Processor (96 cpus), load 14.8, median of 3; tmux: tmux 3.8; vtmux: vtmux 3.8-beta1

### upgrade (vtmux-upgrade onto the same binary: N panes x L lines, one window each, 1 attached 200x50 client with sync on, 2 control clients; target < 1000 ms, decision 13)

| case | binary | history lines | idle s | answered ms | redrawn ms | snapshot MB | RSS MB before | RSS MB after |
|---|---|---|---|---|---|---|---|---|
| 1 x 10k fresh | vtmux | 9952 | 0.0 | 121 | 123 | 0.9 | 29.4 | 32.6 |
| 1 x 10k idle | vtmux | 9952 | 9.0 | 40 | 42 | 1.3 | 15.6 | 19.7 |
| 1 x 100k fresh | vtmux | 99952 | 0.0 | 1015 | 1016 | 6.0 | 169.3 | 182.3 |
| 1 x 100k idle | vtmux | 99952 | 18.5 | 187 | 189 | 11.5 | 27.2 | 50.8 |
| 10 x 10k fresh | vtmux | 99520 | 0.0 | 1005 | 1007 | 6.1 | 170.0 | 183.7 |
| 10 x 10k idle | vtmux | 99520 | 12.5 | 205 | 207 | 11.3 | 30.5 | 54.2 |
| 10 x 100k fresh | vtmux | 999520 | 0.0 | - | - | - | 1566.6 | - |
| 10 x 100k idle | vtmux | 999520 | 21.8 | 1679 | 1680 | 113.1 | 145.7 | 364.3 |

10 x 100k fresh: 3 of 3 runs refused: <vtmux> --vtmux-check-snapshot took over 3 s
