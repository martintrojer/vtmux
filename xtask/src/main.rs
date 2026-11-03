#![forbid(unsafe_code)]
//! Developer tasks: `ref-tmux`, `regress`, `oracle`, `reap`.

mod oracle;
mod reap;
mod ref_tmux;
mod regress;

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("ref-tmux") => ref_tmux::run(),
        Some("regress") => regress::run(),
        Some("oracle") => oracle::run(),
        Some("reap") => reap::run(),
        _ => {
            eprintln!("usage: cargo xtask <ref-tmux|regress|oracle|reap>");
            ExitCode::from(2)
        }
    }
}
