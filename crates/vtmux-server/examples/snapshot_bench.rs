//! Pane terminals through the snapshot format: time, stream size and peak
//! memory of 10 panes x 100k lines (live upgrade spec, decisions 13, 23).
//!
//! Usage: `snapshot_bench PANES LINES FORM [COLS ROWS]`, release build,
//! one form per process so each peak is its own. FORM: `samepin` (upgrade
//! stream, compressed pages in Ghostty's form), `rows` (every page as
//! rows, history compressed first), `resident` (rows, history never
//! compressed). Linux: peaks come from `VmHWM`, reset between phases
//! through `/proc/self/clear_refs`.
//!
//! Phase 1 (the old image): `term_to_snapshot` for every pane, then
//! `encode` into one stream. Phase 2 (the new image, source panes gone):
//! `decode_for`, then `term_from_snapshot` for every pane. Every import's
//! `capture-pane -e -S -` must hash as its source's did.

use std::hash::{Hash as _, Hasher as _};
use std::time::Instant;

use vtmux_server::snapshot_term::{
    Form, ghostty_pages_requirement, has_ghostty_pages, reader_features, term_from_snapshot,
    term_to_snapshot,
};
use vtmux_snapshot as snap;
use vtmux_vt::{CaptureOpts, Compression, LineRef, PATCH_STACK, PaneTerm, capture};

fn status_mb(key: &str) -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().trim_end_matches(" kB").parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        / 1024.0
}

/// Peak RSS restarts from the current RSS.
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

fn output(lines: usize, seed: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * 80);
    for i in 0..lines {
        let n = (i * 7 + seed) % 97;
        let line = match n % 10 {
            0 => format!(
                "\x1b[1;3{}m[{i:07}] ERROR\x1b[0m request failed: code={n} path=/api/v1/items/{i}\r\n",
                n % 8
            ),
            1 => format!(
                "[{i:07}] \u{4e16}\u{754c} wide text {}\r\n",
                "\u{3042}".repeat(n % 20)
            ),
            2 => format!(
                "\x1b[38;5;{n}m{i:07}\x1b[48;2;10;20;{n}m bg \x1b[m {}\r\n",
                "-".repeat(n)
            ),
            _ => format!(
                "[{i:07}] INFO handled {} in {n}ms {}\r\n",
                i % 1000,
                "x".repeat(n % 60)
            ),
        };
        out.extend_from_slice(line.as_bytes());
    }
    out
}

fn digest(t: &PaneTerm) -> u64 {
    let all = CaptureOpts {
        escapes: true,
        start: LineRef::Start,
        end: LineRef::End,
        ..CaptureOpts::default()
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    capture(t, all).hash(&mut h);
    h.finish()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize, d: usize| args.get(i).map_or(d, |a| a.parse().expect("number"));
    let (panes, lines) = (num(0, 10), num(1, 100_000));
    let mode = args.get(2).map_or("samepin", String::as_str);
    let cols = u16::try_from(num(3, 120)).unwrap();
    let rows = u16::try_from(num(4, 40)).unwrap();
    let (form, compress) = match mode {
        "samepin" => (Form::SamePin, true),
        "rows" => (Form::Rows, true),
        "resident" => (Form::Rows, false),
        other => panic!("unknown form {other}"),
    };

    let mut terms: Vec<PaneTerm> = (0..panes)
        .map(|i| {
            let mut t = PaneTerm::new(cols, rows, lines);
            for chunk in output(lines, i).chunks(64 * 1024) {
                t.feed(chunk);
            }
            t
        })
        .collect();
    let digests: Vec<u64> = terms.iter().map(digest).collect();
    if compress {
        for t in &mut terms {
            while t.compress_step() == Compression::Pending {}
        }
    }
    let compressed: u64 = terms.iter().map(PaneTerm::compressed_pages).sum();
    let base = status_mb("VmRSS:");
    println!(
        "panes={panes} lines={lines} {cols}x{rows} form={mode} compressed_pages={compressed} panes_rss={base:.0}MB"
    );

    // Phase 1: the old image writes the stream.
    reset_peak();
    let t0 = Instant::now();
    let mut s = snap::Snapshot::default();
    PATCH_STACK.clone_into(&mut s.meta.ghostty_patches);
    for (i, t) in terms.iter().enumerate() {
        s.panes.push(snap::Pane {
            id: u32::try_from(i).unwrap(),
            term: Some(term_to_snapshot(t, form)),
            ..snap::Pane::default()
        });
    }
    let convert = t0.elapsed();
    if s.panes
        .iter()
        .any(|p| p.term.as_ref().is_some_and(has_ghostty_pages))
    {
        s.meta.requires.push(ghostty_pages_requirement());
    }
    let t1 = Instant::now();
    let stream = snap::encode(&s);
    let encode = t1.elapsed();
    let peak1 = status_mb("VmHWM:");
    drop(s);
    drop(terms);

    // Phase 2: the new image reads it; the source panes are gone.
    let base2 = status_mb("VmRSS:");
    reset_peak();
    let t2 = Instant::now();
    let decoded = snap::decode_for(&stream, &reader_features(), |_| None).expect("decode");
    let decode = t2.elapsed();
    let t3 = Instant::now();
    let imported: Vec<PaneTerm> = decoded
        .panes
        .iter()
        .map(|p| term_from_snapshot(p.term.as_ref().unwrap(), PATCH_STACK).expect("import"))
        .collect();
    let import = t3.elapsed();
    let peak2 = status_mb("VmHWM:");
    let still: u64 = imported.iter().map(PaneTerm::compressed_pages).sum();
    let same = imported.iter().map(digest).eq(digests.iter().copied());
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    println!(
        "  write: convert {:.0} ms + encode {:.0} ms = {:.0} ms; stream {:.1} MB; peak +{:.0} MB over the panes",
        ms(convert),
        ms(encode),
        ms(convert + encode),
        f64::from(u32::try_from(stream.len() / 1000).unwrap_or(u32::MAX)) / 1e3,
        peak1 - base,
    );
    println!(
        "  read: decode {:.0} ms + import {:.0} ms = {:.0} ms; peak {:.0} MB (+{:.0} MB over the stream); imported panes hold {still} compressed pages",
        ms(decode),
        ms(import),
        ms(decode + import),
        peak2,
        peak2 - base2,
    );
    println!(
        "  total {:.0} ms; identical={same}",
        ms(convert + encode + decode + import)
    );
    assert!(same, "an import differs from its source");
}
