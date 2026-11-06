//! Export and import time and memory of whole panes (live upgrade spec,
//! decision 24: 10 panes x 100k lines in at most 2 s).
//!
//! Usage: `state_bench PANES LINES [COLS ROWS]` (release build). Each pane
//! gets `LINES` lines of log-like output (some styled, some wide), its
//! history compressed as the server would. Prints, per mode, export and
//! import time over all panes, the state's size, RSS growth, and whether
//! every import captures (`capture-pane -e -S -`) as its source does.
//! With `STATE_BENCH_STREAMED=1` each pane goes instead page by page from
//! `export_pages` into a `StateImport` (no whole `TermState`), and the
//! peak RSS is that path's alone.

use std::time::Instant;

use vtmux_vt::{
    CaptureOpts, CompressedPages, Compression, ImportCheck, LineRef, PageState, PaneTerm,
    StateImport, TermState, capture,
};

fn status_kb(key: &str) -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().trim_end_matches(" kB").parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}

fn rss_mb() -> f64 {
    status_kb("VmRSS:") / 1024.0
}

fn output(lines: usize, seed: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * 80);
    for i in 0..lines {
        let n = (i * 7 + seed) % 97;
        match n % 10 {
            0 => out.extend_from_slice(
                format!("\x1b[1;3{}m[{i:07}] ERROR\x1b[0m request failed: code={n} path=/api/v1/items/{i}\r\n", n % 8).as_bytes(),
            ),
            1 => out.extend_from_slice(format!("[{i:07}] \u{4e16}\u{754c} wide text {}\r\n", "\u{3042}".repeat(n % 20)).as_bytes()),
            2 => out.extend_from_slice(format!("\x1b[38;5;{n}m{i:07}\x1b[48;2;10;20;{n}m bg \x1b[m {}\r\n", "-".repeat(n)).as_bytes()),
            _ => out.extend_from_slice(format!("[{i:07}] INFO handled {} in {n}ms {}\r\n", i % 1000, "x".repeat(n % 60)).as_bytes()),
        }
    }
    out
}

fn state_bytes(s: &TermState) -> (usize, usize) {
    let (mut bytes, mut blobs) = (0, 0);
    for screen in std::iter::once(&s.primary).chain(s.alternate.as_ref()) {
        for p in &screen.pages {
            match p {
                PageState::Rows(r) => {
                    bytes += r.rows.len() * 12
                        + r.cells.len() * 12
                        + r.graphemes.len() * 4
                        + r.styles.len() * std::mem::size_of::<vtmux_vt::StyleState>()
                        + r.links
                            .iter()
                            .flatten()
                            .map(|l| l.uri.len() + 32)
                            .sum::<usize>();
                }
                PageState::Blob(b) => {
                    bytes += b.bytes.len();
                    blobs += 1;
                }
            }
        }
    }
    (bytes, blobs)
}

/// Every pane exported a page at a time straight into its import, as a
/// live upgrade's stream would carry it.
fn stream(terms: &[PaneTerm], mode: CompressedPages, captures: &[Vec<u8>], all: CaptureOpts) {
    let rss0 = rss_mb();
    let t0 = Instant::now();
    let imported: Vec<PaneTerm> = terms
        .iter()
        .map(|t| {
            let mut import =
                StateImport::new(&t.export_state_head(), ImportCheck::Lazy).expect("head");
            t.export_pages(mode, |screen, page| import.page(screen, &page))
                .expect("pages");
            import.finish().expect("finish")
        })
        .collect();
    let took = t0.elapsed();
    let rss1 = rss_mb();
    let same = imported
        .iter()
        .zip(captures)
        .all(|(t, c)| capture(t, all) == *c);
    println!(
        "  {mode:?} streamed: export+import {:.0} ms; rss +{:.0} MB; identical={same}",
        took.as_secs_f64() * 1e3,
        rss1 - rss0,
    );
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|a| a.parse().expect("number"))
        .collect();
    let (panes, lines) = (
        args.first().copied().unwrap_or(1),
        args.get(1).copied().unwrap_or(100_000),
    );
    let cols = u16::try_from(args.get(2).copied().unwrap_or(120)).unwrap();
    let rows = u16::try_from(args.get(3).copied().unwrap_or(40)).unwrap();
    let all = CaptureOpts {
        escapes: true,
        start: LineRef::Start,
        end: LineRef::End,
        ..CaptureOpts::default()
    };
    let streamed = std::env::var_os("STATE_BENCH_STREAMED").is_some_and(|v| v == "1");

    for compressed_history in [true, false] {
        let mut terms: Vec<PaneTerm> = (0..panes)
            .map(|i| {
                let mut t = PaneTerm::new(cols, rows, lines);
                for chunk in output(lines, i).chunks(64 * 1024) {
                    t.feed(chunk);
                }
                t
            })
            .collect();
        // Before compressing: a capture reads, so restores, every page.
        let captures: Vec<Vec<u8>> = terms.iter().map(|t| capture(t, all)).collect();
        if compressed_history {
            for t in &mut terms {
                while t.compress_step() == Compression::Pending {}
            }
        }
        let compressed: u64 = terms.iter().map(PaneTerm::compressed_pages).sum();
        println!(
            "panes={panes} lines={lines} {cols}x{rows} history={} compressed_pages={compressed} rss={:.0}MB",
            if compressed_history {
                "compressed"
            } else {
                "resident"
            },
            rss_mb()
        );
        for mode in [CompressedPages::Blob, CompressedPages::Rows] {
            if !compressed_history && mode == CompressedPages::Blob {
                continue;
            }
            if streamed {
                stream(&terms, mode, &captures, all);
                continue;
            }
            let rss0 = rss_mb();
            let t0 = Instant::now();
            let states: Vec<TermState> = terms.iter().map(|t| t.export_state_with(mode)).collect();
            let export = t0.elapsed();
            let rss1 = rss_mb();
            let t1 = Instant::now();
            let imported: Vec<PaneTerm> = states
                .iter()
                .map(|s| PaneTerm::import_state(s).expect("import"))
                .collect();
            let import = t1.elapsed();
            let rss2 = rss_mb();
            let (size, blobs) = states
                .iter()
                .map(state_bytes)
                .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            let still: u64 = imported.iter().map(PaneTerm::compressed_pages).sum();
            let same = imported
                .iter()
                .zip(&captures)
                .all(|(t, c)| capture(t, all) == *c);
            println!("  {mode:?}: imported panes hold {still} compressed pages");
            println!(
                "  {mode:?}: export {:.0} ms, import {:.0} ms, total {:.0} ms; state {:.1} MB ({blobs} blobs); rss +{:.0} MB export, +{:.0} MB import; identical={same}",
                export.as_secs_f64() * 1e3,
                import.as_secs_f64() * 1e3,
                (export + import).as_secs_f64() * 1e3,
                f64::from(u32::try_from(size / 1000).unwrap_or(u32::MAX)) / 1e3,
                rss1 - rss0,
                rss2 - rss1,
            );
            drop(imported);
            drop(states);
        }
        terms.clear();
    }
    println!("peak rss {:.0} MB", status_kb("VmHWM:") / 1024.0);
}
