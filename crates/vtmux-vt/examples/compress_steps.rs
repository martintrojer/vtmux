//! Feed a file to one `PaneTerm`, then compress its history step by step
//! as the server does, and print `steps max_step_ms total_ms pages
//! rss_before_mb rss_after_mb identical`: the threading gate's step budget
//! (docs/specs/2026-10-31-history-memory.md, criterion 6) and capture
//! byte-identity across compression.
//!
//! Usage: `compress_steps COLS ROWS HISTORY_LIMIT FILE`

use std::time::{Duration, Instant};

use vtmux_vt::{CaptureOpts, Compression, LineRef, PaneTerm, capture};

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.trim().trim_end_matches(" kB").parse::<f64>().ok())
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [cols, rows, limit, file] = args.as_slice() else {
        eprintln!("usage: compress_steps COLS ROWS HISTORY_LIMIT FILE");
        std::process::exit(2);
    };
    let num = |s: &str| s.parse::<usize>().expect("number");
    let size = |s: &str| s.parse::<u16>().expect("size");
    let bytes = std::fs::read(file).expect("read file");
    let mut t = PaneTerm::new(size(cols), size(rows), num(limit));
    // The pane's tty turns LF into CRLF (onlcr).
    let mut fed = Vec::with_capacity(bytes.len() + bytes.len() / 50);
    for &b in &bytes {
        if b == b'\n' {
            fed.push(b'\r');
        }
        fed.push(b);
    }
    for chunk in fed.chunks(64 * 1024) {
        t.feed(chunk);
    }
    let all = CaptureOpts {
        escapes: true,
        start: LineRef::Start,
        end: LineRef::End,
        ..CaptureOpts::default()
    };
    let before = capture(&t, all);
    let rss_before = rss_mb();
    let (mut steps, mut max, mut total) = (0, Duration::ZERO, Duration::ZERO);
    loop {
        let start = Instant::now();
        let r = t.compress_step();
        let took = start.elapsed();
        steps += 1;
        max = max.max(took);
        total += took;
        if r != Compression::Pending {
            break;
        }
    }
    let pages = t.compressed_pages();
    let rss_after = rss_mb();
    let identical = capture(&t, all) == before;
    println!(
        "{steps} {:.3} {:.1} {pages} {rss_before:.1} {rss_after:.1} {identical}",
        max.as_secs_f64() * 1e3,
        total.as_secs_f64() * 1e3,
    );
}
