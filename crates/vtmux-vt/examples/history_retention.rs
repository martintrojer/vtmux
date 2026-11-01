//! Feed a file to one `PaneTerm` and print `history_size retained_rows`:
//! the rows tmux shows against the rows Ghostty keeps in memory.
//! `scripts/bench-history-memory.sh` runs it on each scenario's output.
//!
//! Usage: `history_retention COLS ROWS HISTORY_LIMIT FILE`

use vtmux_vt::PaneTerm;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [cols, rows, limit, file] = args.as_slice() else {
        eprintln!("usage: history_retention COLS ROWS HISTORY_LIMIT FILE");
        std::process::exit(2);
    };
    let num = |s: &str| s.parse::<usize>().expect("number");
    let size = |s: &str| s.parse::<u16>().expect("size");
    // The benchmark's pane runs `cat FILE; echo DONE`.
    let mut bytes = std::fs::read(file).expect("read file");
    bytes.extend_from_slice(b"DONE\n");
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
    println!("{} {}", t.history_size(), t.retained_rows());
}
