//! Behaviour oracle: `record`, `replay`, `live` (spec, *Behaviour oracle*).
//!
//! - `record`  runs every scenario against the reference tmux 3.8 and writes
//!   `oracle/traces/<name>.json`.
//! - `replay`  runs the same inputs against `target/debug/vtmux` and diffs the
//!   observations against the committed traces.
//! - `live`    reference vs vtmux in memory; `live --self` reference vs
//!   reference (a determinism check).
//!
//! All take `--only <glob>`; `*` and `?` match.

mod normalize;
mod runner;
mod scenario;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Mutex;
use std::thread;

use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::ref_tmux;
use runner::Item;
use scenario::{Observation, Scenario, Step};

const TMUX_VERSION: &str = "3.8";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn fail<T>(msg: impl Into<String>) -> Result<T> {
    Err(msg.into().into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Record,
    Replay,
    Live,
}

#[derive(Debug)]
struct Opts {
    mode: Mode,
    only: Option<String>,
    self_check: bool,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Opts> {
    let mut args = args;
    let mode = match args.next().as_deref() {
        Some("record") => Mode::Record,
        Some("replay") => Mode::Replay,
        Some("live") => Mode::Live,
        _ => {
            return fail("usage: cargo xtask oracle <record|replay|live [--self]> [--only <glob>]");
        }
    };
    let mut opts = Opts {
        mode,
        only: None,
        self_check: false,
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--only" => opts.only = Some(args.next().ok_or("--only needs a glob")?),
            "--self" if mode == Mode::Live => opts.self_check = true,
            _ => return fail(format!("unknown argument {a:?} for {mode:?}")),
        }
    }
    Ok(opts)
}

/// `*` and `?` glob match.
fn glob(pat: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((b'*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some((b'?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(pat.as_bytes(), text.as_bytes())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

pub fn run() -> ExitCode {
    match run_inner() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("oracle: {e}");
            ExitCode::from(2)
        }
    }
}

/// What a trace file holds: the inputs and what the reference answered.
#[derive(Debug, Serialize, Deserialize)]
struct Trace {
    tmux_version: String,
    /// Hostname of the machine that recorded the trace (see `host_view`).
    recorded_host: String,
    name: String,
    inputs: Scenario,
    observations: Vec<Item>,
}

fn load_scenarios(root: &Path, only: Option<&str>) -> Result<Vec<Scenario>> {
    let dir = root.join("oracle/scenarios");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let sc = scenario::load(&p)?;
        if only.is_none_or(|g| glob(g, &sc.name)) {
            out.push(sc);
        }
    }
    if out.is_empty() {
        return fail(match only {
            Some(g) => format!("no scenario matches {g:?}"),
            None => format!("no scenarios in {}", dir.display()),
        });
    }
    Ok(out)
}

fn build_vtmux(root: &Path) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "-q", "-p", "vtmux"])
        .current_dir(root)
        .status()?;
    if !st.success() {
        return fail("cargo build -p vtmux failed");
    }
    Ok(fs::canonicalize(root.join("target/debug/vtmux"))?)
}

/// Run `f` over `scenarios` on a small pool, keeping input order.
fn parallel<T: Send>(scenarios: &[Scenario], f: impl Fn(&Scenario) -> T + Sync) -> Vec<T> {
    let queue: Mutex<VecDeque<usize>> = Mutex::new((0..scenarios.len()).collect());
    let results: Mutex<Vec<Option<T>>> = Mutex::new(scenarios.iter().map(|_| None).collect());
    let workers = thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(1, 6);
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                while let Some(i) = queue.lock().unwrap().pop_front() {
                    let r = f(&scenarios[i]);
                    results.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("every scenario ran"))
        .collect()
}

fn run_inner() -> Result<ExitCode> {
    let opts = parse_args(std::env::args().skip(2))?;
    let root = workspace_root();
    let scenarios = load_scenarios(&root, opts.only.as_deref())?;
    match opts.mode {
        Mode::Record => record(&root, &scenarios),
        Mode::Replay => {
            let vtmux = build_vtmux(&root)?;
            let results = parallel(&scenarios, |sc| {
                let trace = read_trace(&root, sc)?;
                let got = runner::run(&vtmux, sc)?;
                let (want, got) = host_view(&trace, &current_host(), got);
                Ok::<_, String>(compare(&want, &got, false))
            });
            Ok(report(&scenarios, results, "replay"))
        }
        Mode::Live => {
            let reference = ref_tmux::reference_tmux()?;
            let other = if opts.self_check {
                reference.clone()
            } else {
                build_vtmux(&root)?
            };
            let results = parallel(&scenarios, |sc| {
                let want = runner::run(&reference, sc)?;
                let got = runner::run(&other, sc)?;
                Ok::<_, String>(compare(&want, &got, opts.self_check))
            });
            Ok(report(
                &scenarios,
                results,
                if opts.self_check {
                    "live --self"
                } else {
                    "live"
                },
            ))
        }
    }
}

fn current_host() -> String {
    std::process::Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The trace's and vtmux's observations with hostnames made comparable.
/// Scenarios never depend on the hostname (titles are pinned), so on the
/// recording host nothing is touched. On another host the recorded name is
/// replaced by `<HOST>` in the trace, as a whole token only
/// (`normalize::replace_hosts`), and the local name likewise in vtmux's output,
/// but only for steps where the trace holds a `<HOST>`: a host called `server`
/// must not rewrite `no server running` on one side only.
fn host_view(trace: &Trace, host: &str, got: Vec<Item>) -> (Vec<Item>, Vec<Item>) {
    if trace.recorded_host == host {
        return (trace.observations.clone(), got);
    }
    let on = |o: &Observation, name: &str| map_text(o, &|t| normalize::replace_hosts(t, &[name]));
    let want: Vec<Item> = trace
        .observations
        .iter()
        .map(|it| Item {
            observation: on(&it.observation, &trace.recorded_host),
            ..it.clone()
        })
        .collect();
    let host_steps: BTreeSet<usize> = want
        .iter()
        .filter(|it| mentions_host(&it.observation))
        .map(|it| it.step)
        .collect();
    let got = got
        .into_iter()
        .map(|it| {
            if host_steps.contains(&it.step) {
                Item {
                    observation: on(&it.observation, host),
                    ..it
                }
            } else {
                it
            }
        })
        .collect();
    (want, got)
}

fn mentions_host(o: &Observation) -> bool {
    let found = std::cell::Cell::new(false);
    map_text(o, &|t| {
        found.set(found.get() || t.contains("<HOST>"));
        t.to_string()
    });
    found.get()
}

fn map_text(o: &Observation, f: &dyn Fn(&str) -> String) -> Observation {
    let all = |v: &[String]| v.iter().map(|t| f(t)).collect();
    match o {
        Observation::Cli {
            stdout,
            stderr,
            exit,
        } => Observation::Cli {
            stdout: f(stdout),
            stderr: f(stderr),
            exit: *exit,
        },
        Observation::Control {
            lines,
            stderr,
            exit,
        } => Observation::Control {
            lines: all(lines),
            stderr: f(stderr),
            exit: *exit,
        },
        Observation::Screen { rows, cursor } => Observation::Screen {
            rows: all(rows),
            cursor: *cursor,
        },
        Observation::Timeout { what } => Observation::Timeout { what: what.clone() },
    }
}

fn record(root: &Path, scenarios: &[Scenario]) -> Result<ExitCode> {
    let reference = ref_tmux::reference_tmux()?;
    let dir = root.join("oracle/traces");
    fs::create_dir_all(&dir)?;
    let runs = parallel(scenarios, |sc| runner::run(&reference, sc));
    let mut bad = 0;
    for (sc, run) in scenarios.iter().zip(runs) {
        match run {
            Ok(observations) => {
                if let Some(it) = observations
                    .iter()
                    .find(|i| matches!(i.observation, Observation::Timeout { .. }))
                {
                    eprintln!(
                        "record {}: step {} timed out: {:?}",
                        sc.name, it.step, it.observation
                    );
                    bad += 1;
                    continue;
                }
                let trace = Trace {
                    tmux_version: TMUX_VERSION.into(),
                    recorded_host: current_host(),
                    name: sc.name.clone(),
                    inputs: sc.clone(),
                    observations,
                };
                let mut json = serde_json::to_string_pretty(&trace)?;
                json.push('\n');
                fs::write(dir.join(format!("{}.json", sc.name)), json)?;
                println!("recorded {}", sc.name);
            }
            Err(e) => {
                eprintln!("record {}: {e}", sc.name);
                bad += 1;
            }
        }
    }
    Ok(if bad == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn read_trace(root: &Path, sc: &Scenario) -> std::result::Result<Trace, String> {
    let path = root.join(format!("oracle/traces/{}.json", sc.name));
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e} (run `cargo xtask oracle record`)", path.display()))?;
    let trace: Trace =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if trace.tmux_version != TMUX_VERSION {
        return Err(format!(
            "{}: recorded against tmux {}, expected {TMUX_VERSION}",
            path.display(),
            trace.tmux_version
        ));
    }
    if &trace.inputs != sc {
        return Err(format!(
            "{}: scenario changed since the trace was recorded; re-record",
            path.display()
        ));
    }
    Ok(trace)
}

// ------------------------------------------------------------------ compare

/// One step whose observation differs.
struct Mismatch {
    step: usize,
    diff: String,
}

struct Outcome {
    mismatches: Vec<Mismatch>,
    /// Steps judged against a divergence annotation, with their ids.
    diverged: Vec<String>,
}

/// Diff `got` against `want`, matching observations by step (and by order
/// within a step), so a missing or timed-out observation never shifts later
/// diffs onto the wrong step. An item carrying a divergence annotation is
/// judged against its `expect`, never skipped, unless `ignore_divergence`
/// (reference vs reference, where the annotation does not apply).
fn compare(want: &[Item], got: &[Item], ignore_divergence: bool) -> Outcome {
    fn keyed(items: &[Item]) -> BTreeMap<(usize, usize), &Item> {
        let mut seen: BTreeMap<usize, usize> = BTreeMap::new();
        items
            .iter()
            .map(|it| {
                let n = seen.entry(it.step).or_insert(0);
                *n += 1;
                ((it.step, *n - 1), it)
            })
            .collect()
    }
    let (want, got) = (keyed(want), keyed(got));
    let keys: BTreeSet<_> = want.keys().chain(got.keys()).copied().collect();
    let mut out = Outcome {
        mismatches: Vec::new(),
        diverged: Vec::new(),
    };
    for key in keys {
        let (w, g) = (want.get(&key), got.get(&key));
        let div = g
            .and_then(|g| g.divergence.as_ref())
            .filter(|_| !ignore_divergence);
        let expected = match (div, w) {
            (Some(d), _) => {
                out.diverged.push(d.id.clone());
                Some(&d.expect)
            }
            (None, w) => w.map(|w| &w.observation),
        };
        let actual = g.map(|g| &g.observation);
        if expected != actual {
            let mut left = expected.map_or_else(|| "<no observation>\n".into(), render);
            let mut right = actual.map_or_else(|| "<no observation>\n".into(), render);
            if let Some(d) = div {
                left = format!("(divergence {})\n{left}", d.id);
            }
            if left == right {
                right.push('\n');
            }
            let diff = TextDiff::from_lines(&left, &right)
                .unified_diff()
                .context_radius(3)
                .header("expected", "vtmux")
                .to_string();
            out.mismatches.push(Mismatch { step: key.0, diff });
        }
    }
    out
}

/// A readable, line-oriented form of an observation.
fn render(o: &Observation) -> String {
    let mut s = String::new();
    match o {
        Observation::Cli {
            stdout,
            stderr,
            exit,
        } => {
            let _ = writeln!(s, "cli exit={exit}");
            for l in stdout.lines() {
                let _ = writeln!(s, "out| {l}");
            }
            for l in stderr.lines() {
                let _ = writeln!(s, "err| {l}");
            }
        }
        Observation::Control {
            lines,
            stderr,
            exit,
        } => {
            let _ = writeln!(s, "control exit={exit}");
            for l in lines {
                let _ = writeln!(s, "ctl| {l}");
            }
            for l in stderr.lines() {
                let _ = writeln!(s, "err| {l}");
            }
        }
        Observation::Screen { rows, cursor } => {
            let _ = writeln!(s, "screen cursor={},{}", cursor[0], cursor[1]);
            for (y, r) in rows.iter().enumerate() {
                let _ = writeln!(s, "{y:>2}| {r}");
            }
        }
        Observation::Timeout { what } => {
            let _ = writeln!(s, "TIMEOUT {what}");
        }
    }
    s
}

fn report(
    scenarios: &[Scenario],
    results: Vec<std::result::Result<Outcome, String>>,
    what: &str,
) -> ExitCode {
    let mut failed = 0;
    for (sc, res) in scenarios.iter().zip(results) {
        match res {
            Ok(o) if o.mismatches.is_empty() => {
                let note = if o.diverged.is_empty() {
                    String::new()
                } else {
                    format!(" (divergence {})", o.diverged.join(", "))
                };
                println!("ok    {}{note}", sc.name);
            }
            Ok(o) => {
                failed += 1;
                println!("FAIL  {}", sc.name);
                for m in o.mismatches {
                    println!("  step {} ({}):", m.step, describe(&sc.steps, m.step));
                    for l in m.diff.lines() {
                        println!("    {l}");
                    }
                }
            }
            Err(e) => {
                failed += 1;
                println!("ERROR {}: {e}", sc.name);
            }
        }
    }
    println!("{what}: {} scenarios, {failed} failed", scenarios.len());
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn describe(steps: &[Step], i: usize) -> String {
    steps
        .get(i)
        .map_or_else(|| "?".into(), |s| format!("{s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(step: usize, out: &str) -> Item {
        Item {
            step,
            observation: Observation::Cli {
                stdout: out.into(),
                stderr: String::new(),
                exit: 0,
            },
            divergence: None,
        }
    }

    #[test]
    fn globbing() {
        assert!(glob("control_*", "control_output"));
        assert!(glob("new_?as_kill", "new_has_kill"));
        assert!(!glob("control", "control_output"));
    }

    #[test]
    fn compare_reports_the_differing_step() {
        let want = [cli(0, "a\n"), cli(1, "b\n")];
        let got = [cli(0, "a\n"), cli(1, "c\n")];
        let o = compare(&want, &got, false);
        assert_eq!(o.mismatches.len(), 1);
        assert_eq!(o.mismatches[0].step, 1);
        assert!(o.mismatches[0].diff.contains("-out| b"));
        assert!(o.mismatches[0].diff.contains("+out| c"));
    }

    #[test]
    fn divergence_is_judged_against_expect_not_skipped() {
        let want = [cli(0, "tmux\n")];
        let mut got = cli(0, "vtmux\n");
        got.divergence = Some(scenario::Divergence {
            id: "D-001".into(),
            expect: cli(0, "vtmux\n").observation,
        });
        let o = compare(&want, std::slice::from_ref(&got), false);
        assert!(o.mismatches.is_empty());
        assert_eq!(o.diverged, ["D-001"]);
        got.observation = cli(0, "other\n").observation;
        assert_eq!(compare(&want, &[got], false).mismatches.len(), 1);
    }

    #[test]
    fn missing_observation_is_a_mismatch() {
        assert_eq!(compare(&[cli(0, "a\n")], &[], false).mismatches.len(), 1);
    }

    #[test]
    fn live_self_ignores_divergence() {
        let want = [cli(0, "tmux\n")];
        let mut got = cli(0, "tmux\n");
        got.divergence = Some(scenario::Divergence {
            id: "D-001".into(),
            expect: cli(0, "vtmux\n").observation,
        });
        let got = [got];
        assert_eq!(compare(&want, &got, false).mismatches.len(), 1);
        let o = compare(&want, &got, true);
        assert!(o.mismatches.is_empty() && o.diverged.is_empty());
    }

    #[test]
    fn observations_are_keyed_by_step() {
        // Step 1 produced no observation (e.g. a timeout elsewhere): step 2's
        // comparison must still be against step 2.
        let want = [cli(0, "a\n"), cli(1, "b\n"), cli(2, "c\n")];
        let got = [cli(0, "a\n"), cli(2, "x\n")];
        let o = compare(&want, &got, false);
        let steps: Vec<_> = o.mismatches.iter().map(|m| m.step).collect();
        assert_eq!(steps, [1, 2]);
        assert!(o.mismatches[0].diff.contains("+<no observation>"));
        assert!(o.mismatches[1].diff.contains("-out| c"));
        assert!(o.mismatches[1].diff.contains("+out| x"));
    }

    fn trace(host: &str, out: &str) -> Trace {
        Trace {
            tmux_version: TMUX_VERSION.into(),
            recorded_host: host.into(),
            name: "t".into(),
            inputs: serde_json::from_str(r#"{"name":"t","size":[80,24],"steps":[]}"#).unwrap(),
            observations: vec![cli(0, out)],
        }
    }

    #[test]
    fn colliding_hostnames_replay_clean_on_another_host() {
        // Recorded on `linuxpc`; replayed on hosts whose names occur in output.
        let out = "no server running on x\nduplicate session: a\n%output %0 abc hello\ncat\n";
        for host in ["server", "session", "abc", "cat", "hello"] {
            let (want, got) = host_view(&trace("linuxpc", out), host, vec![cli(0, out)]);
            assert!(compare(&want, &got, false).mismatches.is_empty(), "{host}");
        }
    }

    #[test]
    fn a_host_in_the_trace_is_normalised_on_both_sides() {
        let (want, got) = host_view(
            &trace("linuxpc", "\"linuxpc\" and linuxpcx\n"),
            "other",
            vec![cli(0, "\"other\" and linuxpcx\n")],
        );
        assert!(compare(&want, &got, false).mismatches.is_empty());
    }

    #[test]
    fn same_host_is_left_alone() {
        let (want, got) = host_view(
            &trace("server", "no server\n"),
            "server",
            vec![cli(0, "no sorver\n")],
        );
        assert_eq!(compare(&want, &got, false).mismatches.len(), 1);
    }
}
