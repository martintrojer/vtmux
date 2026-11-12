//! CI runs the gate as `scripts/check.sh` runs it locally: one job per
//! stage, each `check.sh --only <stage>` (`.github/workflows/ci.yml`), so a
//! stage added to the script and not to CI, or the other way round, fails
//! here rather than going unrun.

use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace root")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `check.sh --list`: the words of its `all_stages="..."` line.
fn check_stages() -> Vec<String> {
    let script = read("scripts/check.sh");
    let line = script
        .lines()
        .find_map(|l| l.strip_prefix("all_stages=\""))
        .expect("check.sh sets all_stages");
    line.trim_end_matches('"')
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_ci_matrix_runs_every_check_stage_once() {
    let stages = check_stages();
    assert!(stages.contains(&"lint".to_owned()), "{stages:?}");
    let ci = read(".github/workflows/ci.yml");
    let matrices: Vec<Vec<String>> = ci
        .lines()
        .filter_map(|l| l.trim().strip_prefix("stage: ["))
        .map(|l| {
            l.trim_end_matches(']')
                // A job of two stages is one quoted entry, "a,b".
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_owned())
                .collect()
        })
        .collect();
    assert_eq!(
        matrices.len(),
        2,
        "a Linux and a macOS matrix: {matrices:?}"
    );
    let mut want = stages.clone();
    want.sort();
    for m in &matrices {
        let mut m = m.clone();
        m.sort();
        assert_eq!(
            m, want,
            "ci.yml's matrix against check.sh's stages, each once"
        );
    }
    // Each matrix job hands its stage to check.sh, and nothing else calls it.
    let calls: Vec<&str> = ci
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && l.contains("scripts/check.sh"))
        .collect();
    assert_eq!(
        calls, ["run: bash scripts/check.sh --only ${{ matrix.stage }}"; 2],
        "the jobs' check.sh calls"
    );
}

/// The Ghostty build cache is keyed on the setup action's Zig, which must
/// be the Zig mise installs locally.
#[test]
fn ci_zig_is_mise_zig() {
    let pinned = |text: &str, key: &str| {
        let mut lines = text.lines().map(str::trim);
        let line = lines
            .find(|l| l.starts_with(key))
            .unwrap_or_else(|| panic!("no {key}"));
        line[key.len()..].trim().trim_matches('"').to_owned()
    };
    let mise = pinned(&read("mise.toml"), "zig =");
    let setup = read(".github/actions/setup/action.yml");
    let section = &setup[setup.find("zig-version:").expect("a zig-version input")..];
    assert_eq!(pinned(section, "default:"), mise);
}
