//! Regress harness: run tmux 3.8's `regress/*.sh` against a binary and judge
//! the result against `compat/expected.toml`.
//!
//! The suite is vendored verbatim under `compat/tmux-3.8/regress/` and never
//! edited. Each run copies it to `target/regress/work/<pid>/regress`, applies
//! `compat/patches/*.patch` there (`git apply -p1` from the copy's parent, so
//! patch paths read `a/regress/<test>.sh`), and runs every test with tmux's
//! Makefile environment (`env -i`), in parallel, with a timeout. Pass means
//! exit 0. `TMUX_TMPDIR` is deliberately not set: isolation comes from the
//! tests' own `-L` labels, and `terminal-feature-utf8.sh` re-runs a nested
//! client under its own `env -i`.

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::ref_tmux;

const TIMEOUT: Duration = Duration::from_secs(120);
const REGRESS_REL: &str = "compat/tmux-3.8/regress";
const EXPECTED_REL: &str = "compat/expected.toml";
const PATCHES_REL: &str = "compat/patches";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn fail<T>(msg: impl Into<String>) -> Result<T> {
    Err(msg.into().into())
}

// ---------------------------------------------------------------- arguments

#[derive(Debug, PartialEq, Eq)]
enum Target {
    Vtmux,
    Reference,
    Path(PathBuf),
}

#[derive(Debug)]
struct Opts {
    tmux: Target,
    only: Option<String>,
    update: bool,
    json: Option<PathBuf>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Opts> {
    let mut opts = Opts {
        tmux: Target::Vtmux,
        only: None,
        update: false,
        json: None,
    };
    let mut args = args;
    while let Some(a) = args.next() {
        let mut value = |name: &str| -> Result<String> {
            args.next()
                .ok_or_else(|| format!("{name} needs a value").into())
        };
        match a.as_str() {
            "--tmux" => {
                let v = value("--tmux")?;
                opts.tmux = match v.as_str() {
                    "vtmux" => Target::Vtmux,
                    "reference" => Target::Reference,
                    p => Target::Path(PathBuf::from(p)),
                };
            }
            "--only" => opts.only = Some(value("--only")?),
            "--json" => opts.json = Some(PathBuf::from(value("--json")?)),
            "--update" => opts.update = true,
            other => return fail(format!("unknown argument {other}")),
        }
    }
    if opts.update && opts.tmux == Target::Reference {
        return fail("--update makes no sense with --tmux reference (it ignores expected.toml)");
    }
    Ok(opts)
}

/// `*` and `?` glob match.
fn glob(pat: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// `--only` matches the file name with or without `.sh`.
fn selected(only: Option<&str>, name: &str) -> bool {
    only.is_none_or(|g| glob(g, name) || glob(g, name.trim_end_matches(".sh")))
}

// ----------------------------------------------------------- expected.toml

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Xfail,
    Skip,
}

#[derive(Debug, Clone)]
struct Entry {
    status: Status,
    flaky: bool,
}

fn unquote(v: &str) -> Result<String> {
    let inner = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| format!("expected a quoted string, got {v}"))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(e @ ('"' | '\\')) => out.push(e),
                other => return fail(format!("unsupported escape \\{}", other.unwrap_or(' '))),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// The schema is tiny, so parse exactly it and reject everything else:
/// `[tests."<name>.sh"]` sections holding `status`, `reason`, `link`, `flaky`.
fn parse_expected(text: &str) -> Result<BTreeMap<String, Entry>> {
    type Raw = (Option<String>, Option<String>, Option<String>, bool);
    let mut raw: BTreeMap<String, Raw> = BTreeMap::new();
    let mut current: Option<String> = None;
    for (i, line) in text.lines().enumerate() {
        let at = |m: &str| format!("{EXPECTED_REL}:{}: {m}", i + 1);
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(h) = line.strip_prefix("[tests.") {
            let name = h
                .strip_suffix(']')
                .ok_or_else(|| at("bad section header"))
                .and_then(|n| unquote(n).map_err(|e| at(&e.to_string())))?;
            if raw
                .insert(name.clone(), (None, None, None, false))
                .is_some()
            {
                return fail(at(&format!("duplicate entry {name}")));
            }
            current = Some(name);
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return fail(at("expected key = value"));
        };
        let (k, v) = (k.trim(), v.trim());
        let Some(slot) = current.as_ref().and_then(|c| raw.get_mut(c)) else {
            return fail(at("key outside a [tests.\"...\"] section"));
        };
        let s = |v: &str| unquote(v).map_err(|e| at(&e.to_string()));
        match k {
            "status" => slot.0 = Some(s(v)?),
            "reason" => slot.1 = Some(s(v)?),
            "link" => slot.2 = Some(s(v)?),
            "flaky" => {
                slot.3 = match v {
                    "true" => true,
                    "false" => false,
                    _ => return fail(at("flaky must be true or false")),
                };
            }
            _ => return fail(at(&format!("unknown key {k}"))),
        }
    }
    let mut out = BTreeMap::new();
    for (name, (status, reason, _link, flaky)) in raw {
        let status = match status.as_deref() {
            Some("pass") => Status::Pass,
            Some("xfail") => Status::Xfail,
            Some("skip") => Status::Skip,
            other => return fail(format!("{name}: bad or missing status {other:?}")),
        };
        if status != Status::Pass && reason.as_deref().is_none_or(str::is_empty) {
            return fail(format!("{name}: xfail and skip need a reason"));
        }
        if flaky && status == Status::Skip {
            return fail(format!("{name}: a skipped test cannot be flaky"));
        }
        out.insert(name, Entry { status, flaky });
    }
    Ok(out)
}

/// Rewrite the given xfail sections to `status = "pass"`, dropping their
/// `reason` and `link` lines. Everything else is preserved byte for byte.
fn rewrite_flips(text: &str, flips: &[String]) -> String {
    let mut out = String::new();
    let mut in_flip = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with("[tests.") {
            in_flip = flips.iter().any(|n| t == format!("[tests.{}]", quote(n)));
        } else if in_flip {
            if t.starts_with("status") {
                out.push_str("status = \"pass\"\n");
                continue;
            }
            if t.starts_with("reason") || t.starts_with("link") {
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ------------------------------------------------------------------ running

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail(String),
}

impl Outcome {
    fn passed(&self) -> bool {
        *self == Outcome::Pass
    }
    fn label(&self) -> &str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail(why) => why,
        }
    }
}

struct Ran {
    name: String,
    outcome: Outcome,
    secs: f64,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn list_tests(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if Path::new(&name).extension().is_some_and(|x| x == "sh") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            fs::copy(e.path(), dest)?;
        }
    }
    Ok(())
}

/// Copy the suite and apply `compat/patches/*.patch` in name order.
fn prepare_work(root: &Path, work: &Path) -> Result<PathBuf> {
    let _ = fs::remove_dir_all(work);
    let regress = work.join("regress");
    copy_dir(&root.join(REGRESS_REL), &regress)?;
    let mut patches: Vec<PathBuf> = fs::read_dir(root.join(PATCHES_REL))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "patch"))
        .collect();
    patches.sort();
    for p in patches {
        let st = Command::new("git")
            .args(["apply", "-p1"])
            .arg(&p)
            .current_dir(work)
            .status()
            .map_err(|e| format!("git apply: {e}"))?;
        if !st.success() {
            return fail(format!("patch {} does not apply", p.display()));
        }
    }
    Ok(regress)
}

fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--"])
        .arg(format!("-{pid}"))
        .stderr(Stdio::null())
        .status();
}

fn run_one(tmux: &Path, dir: &Path, logs: &Path, name: &str) -> Ran {
    use std::os::unix::process::CommandExt;
    let start = Instant::now();
    let outcome = (|| -> Result<Outcome> {
        let log = File::create(logs.join(format!("{}.log", name.trim_end_matches(".sh"))))?;
        let mut child = Command::new("sh")
            .args(["-x", name])
            .current_dir(dir)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_CTYPE", "C.UTF-8")
            .env("TERM", "screen")
            .env("SHELL", "/bin/sh")
            .env("TEST_TMUX", tmux)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()?;
        loop {
            if let Some(st) = child.try_wait()? {
                return Ok(if st.success() {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!("fail ({st})"))
                });
            }
            if start.elapsed() > TIMEOUT {
                kill_group(child.id());
                child.kill().ok();
                child.wait()?;
                return Ok(Outcome::Fail("timeout".into()));
            }
            thread::sleep(Duration::from_millis(25));
        }
    })()
    .unwrap_or_else(|e| Outcome::Fail(format!("harness error: {e}")));
    Ran {
        name: name.to_owned(),
        outcome,
        secs: start.elapsed().as_secs_f64(),
    }
}

fn run_all(tmux: &Path, dir: &Path, logs: &Path, names: Vec<String>) -> Vec<Ran> {
    let jobs = thread::available_parallelism().map_or(1, usize::from);
    let queue = Mutex::new(names.into_iter().collect::<VecDeque<_>>());
    let done = Mutex::new(Vec::new());
    thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let next = queue.lock().expect("queue").pop_front();
                    let Some(name) = next else { break };
                    let r = run_one(tmux, dir, logs, &name);
                    eprintln!("{:<5} {} ({:.1}s)", r.outcome.label(), r.name, r.secs);
                    done.lock().expect("done").push(r);
                }
            });
        }
    });
    let mut done = done.into_inner().expect("done");
    done.sort_by(|a, b| a.name.cmp(&b.name));
    done
}

// ------------------------------------------------------------------ judging

#[derive(Default)]
struct Verdict {
    pass: usize,
    xfail: usize,
    skip: usize,
    flaky: Vec<(String, bool)>,
    unexpected_fail: Vec<String>,
    unexpected_pass: Vec<String>,
}

fn judge(all: &[String], ran: &[Ran], expected: &BTreeMap<String, Entry>) -> Verdict {
    let mut v = Verdict::default();
    for name in all {
        let e = &expected[name];
        if e.status == Status::Skip {
            v.skip += 1;
            continue;
        }
        let Some(r) = ran.iter().find(|r| &r.name == name) else {
            continue; // not selected by --only
        };
        let ok = r.outcome.passed();
        match (e.flaky, e.status == Status::Pass, ok) {
            (true, _, _) => v.flaky.push((name.clone(), ok)),
            (false, true, true) => v.pass += 1,
            (false, true, false) => v.unexpected_fail.push(name.clone()),
            (false, false, false) => v.xfail += 1,
            (false, false, true) => v.unexpected_pass.push(name.clone()),
        }
    }
    v
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn write_json(
    path: &Path,
    tmux: &Path,
    ran: &[Ran],
    expected: Option<&BTreeMap<String, Entry>>,
) -> Result<()> {
    let mut o = format!(
        "{{\n  \"tmux\": {},\n  \"tests\": [\n",
        json_str(&tmux.display().to_string())
    );
    for (i, r) in ran.iter().enumerate() {
        let exp = expected
            .and_then(|e| e.get(&r.name))
            .map_or("none", |e| match e.status {
                Status::Pass => "pass",
                Status::Xfail => "xfail",
                Status::Skip => "skip",
            });
        let _ = writeln!(
            o,
            "    {{\"name\": {}, \"expected\": {}, \"passed\": {}, \"outcome\": {}, \"secs\": {:.2}}}{}",
            json_str(&r.name),
            json_str(exp),
            r.outcome.passed(),
            json_str(r.outcome.label()),
            r.secs,
            if i + 1 < ran.len() { "," } else { "" }
        );
    }
    o.push_str("  ]\n}\n");
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, o)?;
    Ok(())
}

// --------------------------------------------------------------------- main

pub fn run() -> ExitCode {
    match run_inner() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("regress: {e}");
            ExitCode::from(2)
        }
    }
}

fn resolve_tmux(root: &Path, target: &Target) -> Result<PathBuf> {
    match target {
        Target::Reference => Ok(ref_tmux::reference_tmux()?),
        Target::Path(p) => Ok(fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))?),
        Target::Vtmux => {
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
    }
}

fn run_inner() -> Result<ExitCode> {
    let opts = parse_args(std::env::args().skip(2))?;
    let root = workspace_root();
    let all = list_tests(&root.join(REGRESS_REL))?;
    let reference = opts.tmux == Target::Reference;

    let expected_path = root.join(EXPECTED_REL);
    let expected_text = fs::read_to_string(&expected_path).unwrap_or_default();
    let expected = if reference {
        BTreeMap::new()
    } else {
        let e = parse_expected(&expected_text)?;
        let unlisted: Vec<_> = all.iter().filter(|n| !e.contains_key(*n)).collect();
        let stale: Vec<_> = e.keys().filter(|n| !all.contains(n)).collect();
        if !unlisted.is_empty() || !stale.is_empty() {
            return fail(format!(
                "{EXPECTED_REL} out of sync with the suite: unlisted {unlisted:?}, stale {stale:?}"
            ));
        }
        e
    };

    let tmux = resolve_tmux(&root, &opts.tmux)?;
    let to_run: Vec<String> = all
        .iter()
        .filter(|n| selected(opts.only.as_deref(), n))
        .filter(|n| reference || expected[*n].status != Status::Skip)
        .cloned()
        .collect();
    if to_run.is_empty() && opts.only.is_some() {
        return fail("--only matched no runnable test");
    }

    let base = root.join("target/regress");
    let logs = base.join("logs");
    fs::create_dir_all(&logs)?;
    let work = base.join(format!("work-{}", std::process::id()));
    let dir = prepare_work(&root, &work)?;
    eprintln!("regress: {} tests against {}", to_run.len(), tmux.display());
    let ran = run_all(&tmux, &dir, &logs, to_run);
    let _ = fs::remove_dir_all(&work);

    if let Some(j) = &opts.json {
        write_json(j, &tmux, &ran, (!reference).then_some(&expected))?;
    }

    if reference {
        let failed: Vec<_> = ran.iter().filter(|r| !r.outcome.passed()).collect();
        println!("pass {}/{}", ran.len() - failed.len(), ran.len());
        for r in &failed {
            println!(
                "  failed: {} ({}), log target/regress/logs/{}.log",
                r.name,
                r.outcome.label(),
                r.name.trim_end_matches(".sh")
            );
        }
        return Ok(if failed.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    let mut v = judge(&all, &ran, &expected);
    if opts.update && !v.unexpected_pass.is_empty() {
        let flips = std::mem::take(&mut v.unexpected_pass);
        fs::write(&expected_path, rewrite_flips(&expected_text, &flips))?;
        for n in &flips {
            println!("updated: {n} xfail -> pass");
        }
        v.pass += flips.len();
    }
    println!(
        "pass {}/{}, xfail {}, skip {}{}",
        v.pass,
        all.len(),
        v.xfail,
        v.skip,
        if v.flaky.is_empty() {
            String::new()
        } else {
            format!(", flaky {}", v.flaky.len())
        }
    );
    for (n, ok) in &v.flaky {
        println!("  flaky: {n} ({})", if *ok { "passed" } else { "failed" });
    }
    for n in &v.unexpected_fail {
        println!(
            "  unexpected failure: {n} (expected pass), log target/regress/logs/{}.log",
            n.trim_end_matches(".sh")
        );
    }
    for n in &v.unexpected_pass {
        println!("  unexpected pass: {n}");
    }
    if !v.unexpected_pass.is_empty() {
        println!("unexpected pass: update compat/expected.toml (cargo xtask regress --update)");
    }
    Ok(
        if v.unexpected_fail.is_empty() && v.unexpected_pass.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(glob("floating-*", "floating-pane-zoom.sh"));
        assert!(glob("conf-?yntax", "conf-syntax"));
        assert!(glob("*zoom*", "floating-pane-zoom.sh"));
        assert!(!glob("conf", "conf-syntax"));
        assert!(selected(Some("conf-syntax"), "conf-syntax.sh"));
        assert!(selected(None, "x.sh"));
    }

    const SAMPLE: &str = "# c\n[tests.\"a.sh\"]\nstatus = \"pass\"\n\n[tests.\"b.sh\"]\nstatus = \"xfail\"\nreason = \"not \\\"yet\\\"\"\nlink = \"x\"\n\n[tests.\"c.sh\"]\nstatus = \"xfail\"\nreason = \"r\"\nflaky = true\n";

    #[test]
    fn parses_schema() {
        let e = parse_expected(SAMPLE).unwrap();
        assert_eq!(e["a.sh"].status, Status::Pass);
        assert_eq!(e["b.sh"].status, Status::Xfail);
        assert!(e["c.sh"].flaky);
    }

    #[test]
    fn rejects_bad_entries() {
        assert!(parse_expected("[tests.\"a.sh\"]\nstatus = \"xfail\"\n").is_err());
        assert!(parse_expected("[tests.\"a.sh\"]\nstatus = \"maybe\"\n").is_err());
        assert!(parse_expected("[tests.\"a.sh\"]\nstatus = \"pass\"\nbogus = \"1\"\n").is_err());
        assert!(parse_expected("status = \"pass\"\n").is_err());
    }

    #[test]
    fn update_rewrites_only_flips() {
        let out = rewrite_flips(SAMPLE, &["b.sh".to_owned()]);
        assert!(out.contains("[tests.\"b.sh\"]\nstatus = \"pass\"\n\n"));
        assert!(!out.contains("link"));
        assert!(out.contains("[tests.\"c.sh\"]\nstatus = \"xfail\"\nreason = \"r\""));
        assert!(parse_expected(&out).is_ok());
    }
}
