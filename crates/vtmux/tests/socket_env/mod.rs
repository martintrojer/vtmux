/// Re-run a socket-using test with a temp base too long for `sun_path`.
/// A subprocess changes the environment without racing other test threads.
pub fn with_long_tmpdir(test: &str) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("rv-g7-{}", "x".repeat(85)))
        .tempdir_in("/tmp")
        .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("TMPDIR", dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "{test} with long TMPDIR failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
