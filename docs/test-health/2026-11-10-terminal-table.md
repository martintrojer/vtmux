# Terminal table: ncurses 6.6's whole database (`d007_terminals`)

Task `d007_terminals`: the built-in terminal table (`crates/vtmux-core/src/term_features/terminfo.rs`, D-007) went from 70 names to 2,663. This record measures what it costs.

Host: the shared Linux server (Linux 6.16, AMD EPYC Genoa, 96 cpus), builds through `muzzle run --class high`. `base` = main `a3f8b8d`; `table` = the same commit with the new table. `cargo build --release -p vtmux` (fat LTO, `strip = true`), one build each.

## Size

| | `target/release/vtmux` |
|---|---|
| base (70 terminals) | 9,761,176 bytes |
| table, first encoding (every entry's names as `&[&str]`, strings as `&[(&str, &str)]`) | 12,746,168 bytes |
| table, bit set of names and a shared pool of strings (shipped) | 10,035,416 bytes |

The shipped table costs 274,240 bytes (2.8%). The first encoding's 3 MB was 16 bytes per capability name per distinct entry; a bit set over the 236 names of `tty_term_codes` and indices into one pool of (name, value) pairs is 2,663 entries of about 100 bytes plus the pool.

The generated source is 642 KB (`terminfo.rs`) and the reference rows 134 KB (`golden/cases.tsv`, 2,908 rows).

## Lookup

`Term::open` finds the entry by binary search over the sorted table (about 12 comparisons). Building a client's terminal reads the entry's bit set and strings once per attach.
