//! Time copy-mode entry (`Grid::clone_of`) on a big history, in process:
//! `cargo run --release -p vtmux-server --example copy_entry -- [lines] [compress]`.
use std::time::Instant;

use vtmux_server::mode::grid::Grid;
use vtmux_vt::PaneTerm;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map_or(150_000, |s| s.parse().expect("line count"));
    let compress = std::env::args().nth(2).is_some();
    let mut t = PaneTerm::new(120, 40, 100_000);
    let mut buf = Vec::new();
    for i in 0..n {
        let id = i.wrapping_mul(2_654_435_761) % 4_294_967_296;
        let line = format!(
            "2026-10-31T12:00:00.000Z INFO  worker[{}] handled request id={id:08x} path=/api/v1/items/{i} status=200 bytes={}\r\n",
            i % 16,
            (i * 37) % 100_000
        );
        buf.extend_from_slice(line.as_bytes());
    }
    for c in buf.chunks(65536) {
        t.feed(c);
    }
    if compress {
        while t.compress_step() == vtmux_vt::Compression::Pending {}
    }
    let s = Instant::now();
    let copy = t.copy_screen();
    let screen = s.elapsed();
    drop(copy);
    let s = Instant::now();
    let (g, _, _) = Grid::clone_of(&t, 120, 40, false);
    let entry = s.elapsed();
    let s = Instant::now();
    let mut cells = 0;
    for y in 0..=g.last() {
        cells += g.row(y).cells.len();
    }
    let read = s.elapsed();
    let s = Instant::now();
    drop(g);
    let dropped = s.elapsed();
    println!(
        "hsize {} copy_screen {screen:?} clone_of {entry:?} read all ({cells} cells) {read:?} drop {dropped:?}",
        t.history_size()
    );
}
