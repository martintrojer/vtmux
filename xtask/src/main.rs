#![forbid(unsafe_code)]
//! Developer tasks: `ref-tmux`, `regress`, `oracle`, `reap`, `sockets`.

mod oracle;
#[cfg(test)]
mod publish;
mod reap;
mod ref_tmux;
mod regress;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("ref-tmux") => ref_tmux::run(),
        Some("regress") => regress::run(),
        Some("oracle") => oracle::run(),
        Some("reap") => reap::run(),
        Some("sockets") => reap::list_sockets(),
        _ => {
            eprintln!("usage: cargo xtask <ref-tmux|regress|oracle|reap|sockets>");
            ExitCode::from(2)
        }
    }
}

/// Builds the `vtmux` binary and returns the path cargo reports for it:
/// `CARGO_TARGET_DIR` and `build.target-dir` move it out of `<root>/target`.
fn build_vtmux(root: &Path) -> Result<PathBuf, String> {
    build_vtmux_via(
        Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())),
        root,
    )
}

/// `cargo` is the command to run; the build arguments are appended to it.
fn build_vtmux_via(mut cargo: Command, root: &Path) -> Result<PathBuf, String> {
    let out = cargo
        .args(["build", "-q", "-p", "vtmux", "--message-format=json"])
        .current_dir(root)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cargo build -p vtmux: {e}"))?;
    if !out.status.success() {
        return Err("cargo build -p vtmux failed".into());
    }
    let exe = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "vtmux")
        .filter(|m| {
            m["target"]["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k == "bin"))
        })
        .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
        .next_back()
        .ok_or("cargo build -p vtmux reported no vtmux executable")?;
    std::fs::canonicalize(&exe).map_err(|e| format!("{}: {e}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The binary under test is the one cargo built, wherever its target
    /// directory is, not `<root>/target/debug/vtmux`.
    #[test]
    fn build_vtmux_returns_the_executable_cargo_reports() {
        let dir = std::env::temp_dir().join(format!("xt-bv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("elsewhere-vtmux");
        std::fs::write(&exe, "").unwrap();
        let script = format!(
            "echo '{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\"vtmux\",\"kind\":[\"bin\"]}},\"executable\":\"{}\"}}'",
            exe.display()
        );
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script, "sh"]);
        let got = build_vtmux_via(cmd, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.unwrap().file_name().unwrap(), "elsewhere-vtmux");
    }

    #[test]
    fn build_vtmux_fails_without_a_reported_executable() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo '{\"reason\":\"build-finished\"}'", "sh"]);
        assert!(build_vtmux_via(cmd, Path::new("/")).is_err());
    }
}
