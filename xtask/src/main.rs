#![forbid(unsafe_code)]
//! Developer tasks: `ref-tmux`, `regress`, `oracle`.

mod oracle;
mod ref_tmux;
mod regress;

use std::process::ExitCode;

fn main() -> ExitCode {
    let known = matches!(
        std::env::args().nth(1).as_deref(),
        Some("ref-tmux" | "regress" | "oracle")
    );
    if !known {
        eprintln!("usage: cargo xtask <ref-tmux|regress|oracle>");
    }
    eprintln!("not implemented");
    ExitCode::from(2)
}
