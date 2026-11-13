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

/// The setup action installs the Rust of rust-toolchain.toml, the one
/// rustup runs in the repo, and gives it the lint stage's targets: a second
/// pin in the action could drift, and check.sh would find no target.
#[test]
fn ci_rust_is_the_toolchain_file() {
    let file = read("rust-toolchain.toml");
    // The line the action's `sed` reads.
    let channel: Vec<&str> = file
        .lines()
        .filter_map(|l| l.strip_prefix("channel = \"")?.strip_suffix('"'))
        .collect();
    assert_eq!(channel.len(), 1, "one `channel = \"…\"` line: {file}");
    let setup = read(".github/actions/setup/action.yml");
    assert!(
        setup.contains(r#"sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml"#),
        "the setup action reads rust-toolchain.toml's channel"
    );
    let toolchains: Vec<&str> = setup
        .lines()
        .filter_map(|l| l.trim().strip_prefix("toolchain:"))
        .map(str::trim)
        .collect();
    assert_eq!(toolchains, ["${{ steps.rust.outputs.channel }}"]);
}

/// Job `name` of a workflow: its lines, up to the next job's (two spaces'
/// indent and a name).
fn job(workflow: &str, name: &str) -> String {
    let mut lines = workflow
        .lines()
        .skip_while(|l| *l != format!("  {name}:"))
        .peekable();
    assert!(lines.peek().is_some(), "no job {name}");
    let head = lines.next().unwrap_or_default();
    let body = lines
        .take_while(|l| l.is_empty() || l.starts_with("   ") || l.trim_start().starts_with('#'));
    std::iter::once(head)
        .chain(body)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every musl target release.yml builds, ci.yml builds the same way
/// (`release-tarball.sh`, which links and runs it) on the same runner, and
/// the merge waits on it: a broken musl link fails before a tag.
#[test]
fn ci_builds_every_musl_release_target() {
    let targets = |text: &str| -> Vec<(String, String)> {
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        lines
            .windows(2)
            .filter_map(|w| {
                let target = w[0].strip_prefix("- target: ")?;
                let runner = w[1].strip_prefix("runner: ")?;
                target
                    .ends_with("-linux-musl")
                    .then(|| (target.to_owned(), runner.to_owned()))
            })
            .collect()
    };
    let release = targets(&read(".github/workflows/release.yml"));
    assert_eq!(release.len(), 2, "{release:?}");
    let ci = read(".github/workflows/ci.yml");
    let musl = job(&ci, "musl");
    assert_eq!(targets(&musl), release);
    assert!(
        musl.contains("run: bash scripts/release-tarball.sh ${{ matrix.target }}"),
        "{musl}"
    );
    let check = job(&ci, "check");
    assert!(check.contains("needs: [linux, musl]"), "{check}");
    assert!(
        check.contains(r#"test "$RESULT" = success && test "$MUSL" = success"#),
        "{check}"
    );
}

/// Both test jobs (Linux, macOS) give the live-upgrade tests the 3.8-beta3
/// release binary, so the upgrade from and back to beta3 runs rather than
/// skips.
#[test]
fn ci_test_jobs_have_the_beta3_binary() {
    let ci = read(".github/workflows/ci.yml");
    for name in ["linux", "macos"] {
        let job = job(&ci, name);
        let at = job
            .find("scripts/fetch-beta3.sh")
            .unwrap_or_else(|| panic!("{name}: no fetch-beta3.sh"));
        let step = &job[job[..at].rfind("- name:").expect("a step")..at];
        assert!(
            step.contains("if: matrix.stage == 'test'"),
            "{name}: {step}"
        );
        assert!(
            job.contains(r#"echo "VTMUX_BETA3_BINARY=$bin" >>"$GITHUB_ENV""#),
            "{name}"
        );
    }
}
