# Live upgrade spike: Ghostty state export/import 2026-11-06 (lu_spike_ghostty_export)

Spec: `docs/specs/2026-11-06-live-upgrade.md`, *Snapshot > Ghostty access* and *Spike first*; decisions 5, 11, 23
and 24. Host: devserver, CentOS Stream 9, Linux 6.16, AMD EPYC Genoa, 96 cpus, shared with other agents.
Base main `ee4957d`.

## What was built

- Vendored patch `0017-vtmux-state-export.patch` (`third_party/libghostty-rs/VENDOR.md` has the details): new
  C functions only, in a new file (`src/terminal/c/vtmux_state.zig`, `include/ghostty/vt/vtmux_state.h`). No
  struct, page or row layout changed; the row bits it reads (`tmux_used`, tab marks, palette-256) are the ones
  patches 0002, 0010 and 0013 already added.
  - Page export: every page of either screen as rows of cells in the header's own records (text and graphemes,
    width, style table with every attribute and colour and the 256-colour form flags, hyperlinks with URI and
    explicit or implicit id, protected bit, tab mark, semantic content; per row wrap, continuation, cells used,
    semantic prompt). Compressed pages are decoded into scratch memory and stay compressed, or come out as a
    blob (header, native `Page` value, LZ4 bytes) for a build with the same patch stack (decision 23).
  - State export: modes (current, saved, default, by number), tab stops, scroll region, cursor (pending wrap,
    pen, open hyperlink), saved cursor, charsets, palette overrides, OSC 10/11/12 colours, Kitty keyboard stack,
    mouse flags, REP character.
  - Import: pages built with `PageList.Builder` and swapped into a fresh terminal's screens, cursor rebuilt as
    Ghostty's snapshot decoder does.
- `crates/vtmux-vt/src/term/state.rs`: `PaneTerm::export_state() -> TermState`,
  `PaneTerm::export_state_with(CompressedPages::{Rows, Blob})`, `PaneTerm::import_state(&TermState) ->
  Result<PaneTerm, StateError>`. `TermState` is vtmux's own type: per screen the pages (`PageState::Rows(PageRows)`
  of `RowState`/`CellState`, or `PageState::Blob(PageBlob)` tagged with `PATCH_STACK`), Ghostty's state
  (`GhosttyState`, `GhosttyCursor`, `SavedCursor`, `GhosttyCharset`, `ModeState`, `StyleState`, `LinkState`), and
  what `PaneTerm` keeps beside Ghostty (`PaneState`: tmux's history model and row times, OSC 133 marks, the
  history and screen frozen for an alternate screen, tmux charset, cursor style, mouse and key modes, palette,
  server title and title stack, sync hold, and the input since the parser left the ground state, replayed on
  import).

Ghostty's own snapshot codec (`src/terminal/snapshot`) was looked at and not used: its format is still breaking
(v1, "work-in-progress"), it drops vtmux's patch bits, and it decodes compressed pages.

## Round trips (byte-identical)

`crates/vtmux-vt/tests/state_roundtrip.rs`. Each round trip exports a pane, imports it into a fresh one, and
requires equal `capture-pane` output with `-e -S -`, `-J -N`, `-F -T` (row flags), `-H`, `-C -e -I` and `-a`,
equal cursor, alternate screen, history size, retained rows, title, path, every mode accessor, OSC 133 marks,
pending input and the visible grid, and an equal re-exported `TermState` (every row's flags and cells used,
pages and capacities included). Then both get the same further input and must stay equal, replies included.

| corpus | result |
|---|---|
| (a) escape-sequence fuzzer: seeded generator of text (wide, combining, ZWJ, regional indicators, C1-ish), C0, CSI moves/erases/inserts/scrolls, SGR (256/RGB/underline styles and colours), DEC/ANSI modes incl. 1049/47/1047, 2026, 2027, 69 with DECSLRM, DECSC/DECRC, charsets and SO/SI, OSC 0/2/4/7/8/10-12/133, title stack, DECSCUSR, modifyOtherKeys, DECRQSS, `ESC k`, DECSCA, ED3, REP, RIS, resizes; cut at a random byte, mid-sequence too | 4000 seeds pass (`VTMUX_STATE_FUZZ_SEEDS=4000`); the gate runs 60 |
| (b) the oracle scenarios' pane programs: `tests/state/oracle_programs.txt`, recorded by `tests/state/regen_corpus.py` (each scenario's programs under a pty of the pane's size, stub `tmux`, the scenario's `feed` text typed) | 214 programs, each cut at 1/4, 1/2, 3/4 and the end, and once with its history compressed and exported as blobs: all pass |
| targeted: history, styles/wide/graphemes/links/flags, modes/charsets/tabs/region/title stack/palette, pending wrap and saved cursor, alternate screen with frozen primary history, nine half-parsed sequences, reflow with limits and byte budget, compressed history as rows and as blobs (imports stay compressed), foreign blob refused | pass |

Mutation checks (reverted): dropping tab stops on import fails 10 of 11 tests, and still 6 with the state
comparison turned off (behaviour alone); not replaying the parser continuation fails 3 on behaviour alone.

Stress: the test binary 5x under 48 busy loops, `flock /tmp/vtmux-stress.lock`, load 18 to 56: 5/5 pass,
20.2-20.9 s each. No test spawns a shell, so the dash-like `/bin/sh` run does not apply.

Ghostty: `zig build test-lib-vt -Demit-lib-vt=true -Dapp-runtime=none` 3257 pass (one more than before: the
patch's own test), 38 skip, the known `tabClear single` failure.

## Time and memory

`cargo build --release -p vtmux-vt --example state_bench`, then
`flock /tmp/vtmux-stress.lock state_bench 1 100000` and `state_bench 10 100000` (120x40 panes, `history-limit`
100000, log lines with colours, RGB backgrounds and wide text; "compressed" is after a full compression pass;
load 13-21):

| panes x lines | history | pages as | export | import | total | state size | RSS added by import | identical |
|---|---|---|---|---|---|---|---|---|
| 1 x 100k | compressed | blobs | 1 ms | 2 ms | 3 ms | 5.0 MB | 6 MB | yes |
| 1 x 100k | compressed | rows | 73 ms | 50 ms | 123 ms | 71.9 MB | 94 MB | yes |
| 1 x 100k | resident | rows | 48 ms | 51 ms | 99 ms | 71.9 MB | 94 MB | yes |
| 10 x 100k | compressed | blobs | 18 ms | 20 ms | 37 ms | 49.7 MB | 61 MB | yes |
| 10 x 100k | compressed | rows | 753 ms | 482 ms | 1236 ms | 719 MB | 938 MB | yes |
| 10 x 100k | resident | rows | 450 ms | 475 ms | 925 ms | 719 MB | 937 MB | yes |

Panes imported from blobs keep every page compressed (2580 of 2580); from rows they are resident until the
compressor runs again (decision 23's background recompression).

## Verdict per decision 24

- Byte-identical `capture-pane -e -S -`, cursor, modes and row flags: yes, for both corpora.
- Export plus import of 10 x 100k lines: 37 ms on the same-build path, 1.24 s on the rows path with compressed
  history (0.93 s resident); both under 2 s.
- Ghostty page or row layout: unchanged; the patch only adds functions.

Go.

## Not carried, or carried approximately

- Pending events the server drains every loop turn (replies, bells, title events, passthrough, renames): drain
  before exporting. Kitty graphics images (vtmux keeps none).
- The 5 s ground timers of a half-read DCS or `ESC k` restart at import; a DCS payload longer than 1 MiB is
  replayed only up to `pending`'s cap.
- Ghostty page internals that hold no content (dead entries of a page's style and hyperlink sets, the
  allocator's free list): after more output an import may grow a page's capacity at another moment than its
  source. Contents and flags stay equal; the tests compare rows, not page splits, after further input.
- A page Ghostty compacted to an exact-size heap allocation comes back pool-allocated if it fits.
- The rows form is 12 bytes a cell in memory (72 MB for a 120-column, 100k-line pane): an encoder should
  export and write one page at a time rather than hold every pane's `TermState`.

## Found on the way (not fixed here)

`PaneTerm::resize` writes cursor moves straight to Ghostty (`write_screen_moves`, `eat_below_cursor`,
`reflow_blank_rows`), which breaks a sequence the program left half-written: Ghostty's parser ends up in the
ground state while tmux's scanner (and `pending`) still holds the sequence, so the rest of it prints. The fuzzer
avoids resizing mid-sequence for that reason; the bug is independent of the import.
