# Release binary size after kitty graphics (`final_lows`)

The 10.0 MB in the docs was measured before D33 added the `png` and `flate2` crates (`2026-11-10-terminal-table.md`). This record measures the binary as of the final review fixes.

Host: the shared Linux server (Linux 6.16, AMD EPYC Genoa, 96 cpus), load 52-131 (`uptime`, does not affect size), build through `muzzle run --class high`. Commit `635a1706` (branch `final_lows`, on main `069845bd`). `cargo build --release -p vtmux` (fat LTO, one codegen unit, `strip = true`), x86_64 Linux, one build.

| | bytes |
|---|---|
| `target/release/vtmux`, stripped | 10,445,032 |
| same, `gzip -9` | 4,529,762 |
| before the terminal table (`2026-11-10-terminal-table.md`, base) | 9,761,176 |
| with the table, before D33 (same record, shipped) | 10,035,416 |

Since the terminal table, the binary grew by 409,616 bytes (4.1%): kitty graphics (D33: the PNG decoder, zlib for uploads, the placeholder renderer) and the fixes since. Against the base before the table it is 683,856 bytes (7.0%) larger.

Raw:

```
$ stat -c %s target/release/vtmux
10445032
$ gzip -9 -c target/release/vtmux | wc -c
4529762
```
