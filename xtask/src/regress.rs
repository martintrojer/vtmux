//! Regress harness: run tmux 3.8's `regress/*.sh` against a binary and judge
//! the result against `compat/expected.toml`.
//!
//! The suite is vendored verbatim under `compat/tmux-3.8/regress/` and never
//! edited. Each run copies it to `target/regress/work/<pid>/regress`, applies
//! `compat/patches/*.patch` there (`git apply -p1` from the copy's parent with
//! repository discovery disabled, so patch paths read `a/regress/<test>.sh`), and runs every test with tmux's
//! Makefile environment (`env -i`), in parallel, with a timeout. Pass means
//! exit 0. Each script gets a private short `TMUX_TMPDIR`, removed afterwards;
//! its servers are told apart by the tests' own `-L` labels, which end in the
//! script's `$$`.
//!
//! Servers daemonize (setsid), so they leave the script's process group and a
//! failing or timed-out script never reaches its own `kill-server`. After every
//! script (pass, fail or timeout) `reap_servers` kills each leftover server of
//! the binary under test whose label carries that script's pid. Scripts that
//! set their own `TMUX_TMPDIR`, or run a nested client under `env -i`
//! (`new-session-environment.sh`), are covered by the label match, not the dir.
//!
//! Scripts hold PTYs (up to 48 each), and a host has a fixed pool: 511 on
//! macOS (`kern.tty.ptmx_max`), where an empty pool fails `openpty` with
//! ENXIO and the script with `fork failed`. The runner starts fewer scripts
//! at once when the pool is small, starts none beside another while fewer
//! than a script's worth are free, prints the pool's use at the start, peak
//! and end, and appends who holds terminals to the log of a script that
//! failed for a PTY.

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::{reap, ref_tmux};

const TIMEOUT: Duration = Duration::from_secs(300);
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
    prepare_work_from(&root.join(REGRESS_REL), &root.join(PATCHES_REL), work)
}

fn prepare_work_from(suite: &Path, patches_dir: &Path, work: &Path) -> Result<PathBuf> {
    let _ = fs::remove_dir_all(work);
    let regress = work.join("regress");
    copy_dir(suite, &regress)?;
    let mut patches: Vec<PathBuf> = fs::read_dir(patches_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "patch"))
        .collect();
    patches.sort();
    for p in patches {
        apply_patch(&p, work)?;
    }
    Ok(regress)
}

/// Apply one patch with `git apply -p1` from `work`, with repository discovery
/// disabled. Inside a checkout, `git apply` finds the enclosing repository and
/// silently skips every path outside the cwd prefix (exit 0, nothing changed),
/// for git-format patches. A `--git-dir` that does not exist makes git behave
/// as if outside any repository (a ceiling would not: it only stops discovery
/// above the listed directory). "Skipped patch" is still treated as a failure.
/// `git apply` is used because `patch(1)` is not installed everywhere.
fn apply_patch(patch: &Path, work: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(work.join(".vtmux-no-git-dir"))
        .args(["apply", "--verbose", "-p1"])
        .arg(fs::canonicalize(patch)?)
        .current_dir(work)
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| format!("git apply: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || stderr.contains("Skipped patch") {
        return fail(format!(
            "patch {} does not apply cleanly: {}",
            patch.display(),
            stderr.trim()
        ));
    }
    Ok(())
}

fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--"])
        .arg(format!("-{pid}"))
        .stderr(Stdio::null())
        .status();
}

/// Pids of `tmux` processes in `ps` output (`pid args...` per line) whose
/// global `-L` label has `script_pid` as its first number: the `-LtestA$$`,
/// `-Li$$`, `-Lcovers-inner-$$-n` convention of the suite. Never `own`.
fn labelled_pids(ps: &str, tmux: &Path, script_pid: u32, own: u32) -> Vec<u32> {
    let tmux = tmux.to_string_lossy();
    reap::pids_where(ps, own, |args| {
        let Some(rest) = args.strip_prefix(tmux.as_ref()) else {
            return false;
        };
        let words: Vec<&str> = rest.split_whitespace().collect();
        let label = words
            .iter()
            .enumerate()
            .find_map(|(i, w)| match w.strip_prefix("-L")? {
                "" => words.get(i + 1).copied(),
                l => Some(l),
            });
        rest.starts_with(' ') && label_is_of(label.unwrap_or(""), script_pid)
    })
}

/// Does the first number in `label` equal `script_pid`?
fn label_is_of(label: &str, script_pid: u32) -> bool {
    let digits: String = label
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits == script_pid.to_string()
}

/// Kill every server (and attached client) the script `script_pid` left
/// behind, then remove the dead socket files of its labels from the real
/// socket directories: a script that runs a client under `env -i`
/// (`new-session-environment.sh`) loses the
/// private `TMUX_TMPDIR`, so that server's socket lands in
/// `/tmp/{tmux,vtmux}-<uid>/` and outlives the server.
fn reap_servers(tmux: &Path, script_pid: u32) {
    let pids = reap::matching_tree(|ps| labelled_pids(ps, tmux, script_pid, std::process::id()));
    reap::kill_pids(&pids);
    sweep_script_sockets(&reap::default_socket_dirs(), script_pid);
}

/// The dead sockets in `dirs` whose label belongs to `script_pid`.
fn sweep_script_sockets(dirs: &[PathBuf], script_pid: u32) {
    reap::sweep_sockets(dirs, |label| label_is_of(label, script_pid), false);
}

/// A fresh private `TMUX_TMPDIR`. Short and under `/tmp`, never `$TMPDIR` or
/// `target/`: socket paths must fit `sun_path` (104 bytes on macOS).
fn private_tmpdir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(format!(
        "/tmp/vtr-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

fn run_one(tmux: &Path, dir: &Path, logs: &Path, name: &str) -> Ran {
    let start = Instant::now();
    let mut script_pid = None;
    let tmpdir = private_tmpdir();
    let outcome = tmpdir
        .as_ref()
        .map_err(ToString::to_string)
        .and_then(|t| {
            run_script(tmux, dir, logs, name, t, start, &mut script_pid).map_err(|e| e.to_string())
        })
        .unwrap_or_else(|e| Outcome::Fail(format!("harness error: {e}")));
    if let Some(pid) = script_pid {
        // Before the reap: the servers it kills hold the script's PTYs.
        if !matches!(outcome, Outcome::Pass) {
            log_pty_holders(logs, name);
        }
        reap_servers(tmux, pid);
    }
    if let Ok(t) = &tmpdir {
        let _ = fs::remove_dir_all(t);
    }
    Ran {
        name: name.to_owned(),
        outcome,
        secs: start.elapsed().as_secs_f64(),
    }
}

fn run_script(
    tmux: &Path,
    dir: &Path,
    logs: &Path,
    name: &str,
    tmpdir: &Path,
    start: Instant,
    script_pid: &mut Option<u32>,
) -> Result<Outcome> {
    use std::os::unix::process::CommandExt;
    {
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
            .env("TMUX_TMPDIR", tmpdir)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()?;
        *script_pid = Some(child.id());
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
    }
}

// --------------------------------------------------------------------- PTYs

/// Free PTYs a script may need: the most one holds at once is 48
/// (`screen-redraw-floating.sh`, two servers and the 4-pane scenes it keeps;
/// tmux 3.8 holds as many).
const PTY_RESERVE: usize = 64;

/// PTYs to budget per concurrent script, a mean and not a bound: the suite
/// held at most 216 at once on 96 jobs, but 77 on 3 when its two biggest
/// scripts overlap, which `may_start` is for.
const PTYS_PER_JOB: usize = 16;

/// Concurrent scripts: one per CPU, fewer when the PTYs left would not
/// hold `PTYS_PER_JOB` each.
fn job_count(cpus: usize, census: Option<(usize, usize)>) -> usize {
    let by_ptys = census.map_or(usize::MAX, |(used, max)| {
        max.saturating_sub(used) / PTYS_PER_JOB
    });
    cpus.min(by_ptys).max(1)
}

/// Is `name` (under `/dev`) a macOS PTY's slave? `ttys` and three digits or
/// more (`ttys%03d`), not the 16 legacy BSD nodes `ttys0`..`ttysf` that
/// every Mac has whether or not anything uses them.
fn is_macos_pty_slave(name: &str) -> bool {
    name.strip_prefix("ttys")
        .is_some_and(|d| d.len() >= 3 && d.bytes().all(|b| b.is_ascii_digit()))
}

/// PTYs in use on the host and the most it allows, when it says. macOS has a
/// `/dev/ttysNNN` node per PTY, made when its master opens and removed when
/// both sides have closed, up to `kern.tty.ptmx_max` (511 by default); Linux
/// one under `/dev/pts/` (beside `ptmx`) up to `kernel.pty.max`.
fn pty_census() -> Option<(usize, usize)> {
    let count = |dir: &str, is_pty: &dyn Fn(&str) -> bool| {
        fs::read_dir(dir).ok().map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .filter(|e| is_pty(&e.file_name().to_string_lossy()))
                .count()
        })
    };
    if cfg!(target_os = "macos") {
        let used = count("/dev", &is_macos_pty_slave)?;
        let out = Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.tty.ptmx_max"])
            .output()
            .ok()?;
        Some((
            used,
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()?,
        ))
    } else {
        // This devpts instance's own `max=` mount option (the last mount
        // on /dev/pts is the one in use), under the
        // host-wide `kernel.pty.max` counted by `kernel.pty.nr`.
        let read = |p: &str| -> Option<usize> { fs::read_to_string(p).ok()?.trim().parse().ok() };
        let used = count("/dev/pts", &|n| n.bytes().all(|b| b.is_ascii_digit()))?;
        let (host_max, host_used) = (
            read("/proc/sys/kernel/pty/max")?,
            read("/proc/sys/kernel/pty/nr")?,
        );
        let own_max = fs::read_to_string("/proc/self/mounts")
            .ok()
            .and_then(|m| {
                m.lines()
                    .rev()
                    .map(|l| l.split_whitespace().collect::<Vec<_>>())
                    .find(|f| f.get(1) == Some(&"/dev/pts") && f.get(2) == Some(&"devpts"))?
                    .get(3)?
                    .split(',')
                    .find_map(|o| o.strip_prefix("max=")?.parse::<usize>().ok())
            })
            .unwrap_or(host_max);
        let max = own_max.min(host_max);
        let free = max
            .saturating_sub(used)
            .min(host_max.saturating_sub(host_used));
        Some((max - free, max))
    }
}

/// May another script start, with `running` scripts going and `census`
/// PTYs in use of the most allowed? Only while `PTY_RESERVE` are left: a
/// suite run concurrently on a host with few PTYs (macOS, 511) otherwise
/// fails scripts with `fork failed` (ENXIO there, ENOSPC on Linux). A lone
/// script always starts, and an unknown count never holds one back.
fn may_start(running: usize, census: Option<(usize, usize)>) -> bool {
    running == 0 || census.is_none_or(|(used, max)| max.saturating_sub(used) >= PTY_RESERVE)
}

/// The PTYs in use over a run, sampled while it goes.
#[derive(Default)]
struct PtyUse {
    start: usize,
    peak: usize,
    max: usize,
    held_back: usize,
}

/// After a script fails for a PTY (`fork failed`, the pool nearly empty),
/// its log gets who holds the host's terminals, its own leftovers still
/// running, so the next such failure in CI names the holders.
fn log_pty_holders(logs: &Path, name: &str) {
    let path = logs.join(format!("{}.log", name.trim_end_matches(".sh")));
    let text = fs::read_to_string(&path).unwrap_or_default();
    let census = pty_census();
    let low = census.is_some_and(|(used, max)| max.saturating_sub(used) < PTY_RESERVE);
    if !text.contains("fork failed") && !low {
        return;
    }
    let mut note = format!("\n--- regress: PTYs in use (in use, most): {census:?}\n");
    let ps = Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,tty=,etime=,args="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    note.push_str("--- processes on a terminal (pid ppid tty etime args):\n");
    for line in ps.lines() {
        let tty = line.split_whitespace().nth(2).unwrap_or("?");
        if !tty.starts_with('?') {
            note.push_str(line);
            note.push('\n');
        }
    }
    // A server holds its panes' masters with no controlling terminal, so ps
    // shows it on `??`; lsof names every process holding masters.
    if let Ok(o) = Command::new("lsof")
        .args(["-nP", "-w", "-F", "pc", "/dev/ptmx"])
        .output()
    {
        let holders = ptmx_holders(&String::from_utf8_lossy(&o.stdout));
        if !holders.is_empty() {
            note.push_str("--- PTY masters held (count pid command), from lsof /dev/ptmx:\n");
            for (n, pid, cmd) in holders {
                let _ = writeln!(note, "{n:>4} {pid} {cmd}");
            }
        }
    }
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&path) {
        let _ = std::io::Write::write_all(&mut f, note.as_bytes());
    }
}

/// `lsof -F pc /dev/ptmx` output (`p<pid>`, `c<command>`, then one `f<fd>`
/// line per descriptor) as (masters, pid, command), most masters first.
fn ptmx_holders(lsof: &str) -> Vec<(usize, String, String)> {
    let mut out: Vec<(usize, String, String)> = Vec::new();
    for line in lsof.lines() {
        match line.split_at_checked(1) {
            Some(("p", pid)) => out.push((0, pid.to_owned(), String::new())),
            Some(("c", cmd)) => {
                if let Some(last) = out.last_mut() {
                    cmd.clone_into(&mut last.2);
                }
            }
            Some(("f", _)) => {
                if let Some(last) = out.last_mut() {
                    last.0 += 1;
                }
            }
            _ => {}
        }
    }
    out.retain(|h| h.0 > 0);
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out
}

fn run_all(tmux: &Path, dir: &Path, logs: &Path, names: Vec<String>) -> Vec<Ran> {
    let census = pty_census();
    let jobs = job_count(
        thread::available_parallelism().map_or(1, usize::from),
        census,
    );
    let queue = Mutex::new(names.into_iter().collect::<VecDeque<_>>());
    let done = Mutex::new(Vec::new());
    let running = AtomicUsize::new(0);
    let finished = std::sync::atomic::AtomicBool::new(false);
    let ptys = Mutex::new(PtyUse::default());
    if let Some((used, max)) = census {
        *ptys.lock().expect("ptys") = PtyUse {
            start: used,
            peak: used,
            max,
            held_back: 0,
        };
    }
    thread::scope(|s| {
        s.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                if let Some((used, _)) = pty_census() {
                    let mut p = ptys.lock().expect("ptys");
                    p.peak = p.peak.max(used);
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                s.spawn(|| {
                    loop {
                        let next = queue.lock().expect("queue").pop_front();
                        let Some(name) = next else { break };
                        let mut waited = false;
                        while !may_start(running.load(Ordering::SeqCst), pty_census()) {
                            waited = true;
                            thread::sleep(Duration::from_millis(100));
                        }
                        if waited {
                            ptys.lock().expect("ptys").held_back += 1;
                        }
                        running.fetch_add(1, Ordering::SeqCst);
                        let r = run_one(tmux, dir, logs, &name);
                        running.fetch_sub(1, Ordering::SeqCst);
                        eprintln!("{:<5} {} ({:.1}s)", r.outcome.label(), r.name, r.secs);
                        done.lock().expect("done").push(r);
                    }
                })
            })
            .collect();
        for w in workers {
            let _ = w.join();
        }
        finished.store(true, Ordering::Relaxed);
    });
    let p = ptys.into_inner().expect("ptys");
    if p.max > 0 {
        eprintln!(
            "regress: {jobs} jobs; PTYs in use: {} at the start, {} at the peak, {} at the end, of {}{}",
            p.start,
            p.peak,
            pty_census().map_or(0, |(used, _)| used),
            p.max,
            if p.held_back > 0 {
                format!("; {} scripts waited for free PTYs", p.held_back)
            } else {
                String::new()
            }
        );
    }
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
    use crate::reap::ps;
    use std::os::unix::net::UnixListener;

    #[test]
    fn labels_select_a_scripts_servers() {
        let tmux = Path::new("/w/vtmux");
        let ps = "  10 /w/vtmux -LtestA42 -f/dev/null new -d\n\
                  11 /w/vtmux -Lcovers-inner-42-3 -f/dev/null attach\n\
                  12 /w/vtmux -L i42 -f/dev/null new\n\
                  13 /w/vtmux -LtestA421 -f/dev/null new\n\
                  14 /w/vtmuxer -LtestA42 x\n\
                  15 /other/tmux -LtestA42 x\n\
                  16 /w/vtmux -LtestA42 -f/dev/null new\n\
                  17 sh -c /w/vtmux -LtestA42\n";
        assert_eq!(labelled_pids(ps, tmux, 42, 16), [10, 11, 12]);
    }

    /// A script that fails after starting a "server" (a process whose argv
    /// names the binary under test and a `-LtestA$$` label) must not leave it
    /// behind, and runs with a private `TMUX_TMPDIR`.
    #[test]
    fn failing_script_leaves_no_server() {
        let base = workspace_root()
            .join("target")
            .join(format!("regress-leak-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("leak.sh"),
            "echo $$ >pid\necho \"$TMUX_TMPDIR\" >tmpdir\n\
             (bash -c 'exec -a \"$0 -LtestA$1 -f/dev/null new\" sleep 1000' \"$TEST_TMUX\" $$ &)\n\
             sleep 0.3\nexit 1\n",
        )
        .unwrap();
        let ran = run_one(Path::new("/usr/bin/true"), &base, &base, "leak.sh");
        assert!(!ran.outcome.passed());
        let pid = fs::read_to_string(base.join("pid")).unwrap();
        let label = format!("-LtestA{}", pid.trim());
        assert!(!ps().contains(&label), "server survived: {label}");
        let tmpdir = fs::read_to_string(base.join("tmpdir")).unwrap();
        assert!(tmpdir.trim().starts_with("/tmp/vtr-"), "{tmpdir}");
        assert!(!Path::new(tmpdir.trim()).exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn failing_script_leaves_no_stubborn_pane() {
        let tmux = crate::ref_tmux::reference_tmux().unwrap();
        let base = workspace_root()
            .join("target")
            .join(format!("regress-pane-leak-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("leak.sh"),
            "\"$TEST_TMUX\" -LtestA$$ -f/dev/null new -d -s a -n w sh -c 'trap \"\" HUP TERM; echo $$ >pane; while :; do sleep 1000; done'\nwhile [ ! -s pane ]; do sleep 0.01; done\nexit 1\n"
        ).unwrap();
        let ran = run_one(&tmux, &base, &base, "leak.sh");
        let pid = fs::read_to_string(base.join("pane")).unwrap();
        let out = Command::new("ps")
            .args(["-p", pid.trim(), "-o", "stat="])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&out.stdout);
        let alive = !state.trim().is_empty() && !state.trim().starts_with('Z');
        // The test also cleans up after the broken implementation.
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", pid.trim())])
            .stderr(Stdio::null())
            .status();
        fs::remove_dir_all(base).unwrap();
        assert!(!ran.outcome.passed());
        assert!(!alive, "stubborn pane survived regress cleanup: {pid}");
    }

    /// A script's labelled servers can sit in the real socket directory
    /// (a client under `env -i` has no private `TMUX_TMPDIR`); once they are
    /// dead their socket files go, other scripts' and live ones stay.
    #[test]
    fn script_sockets_are_swept_by_label() {
        let base = PathBuf::from(format!("/tmp/vtx-{}-regress", std::process::id()));
        let dir = base.join("vtmux-1");
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "testA4242",
            "testA4242-2",
            "testI4242",
            "covers-inner-4242-1",
            "testA4243",
        ] {
            crate::reap::test_support::dead_socket(&dir.join(name));
        }
        let live = UnixListener::bind(dir.join("testA4242-live")).unwrap();
        sweep_script_sockets(std::slice::from_ref(&dir), 4242);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["testA4242-live", "testA4243"]);
        drop(live);
        let _ = fs::remove_dir_all(&base);
    }

    /// A host with few PTYs left (macOS: 511 in all) gets fewer concurrent
    /// scripts, and none starts beside another while fewer than a script's
    /// worth are free.
    #[test]
    fn scripts_wait_for_free_ptys() {
        assert_eq!(job_count(96, Some((5, 4096))), 96);
        assert_eq!(job_count(96, Some((0, 511))), 31);
        assert_eq!(job_count(3, Some((0, 511))), 3);
        assert_eq!(job_count(8, Some((500, 511))), 1);
        assert_eq!(job_count(8, None), 8);
        assert!(may_start(0, Some((511, 511))));
        assert!(may_start(2, Some((447, 511))));
        assert!(!may_start(2, Some((448, 511))));
        assert!(may_start(2, None));
    }

    /// macOS's 16 legacy `ttys0`..`ttysf` nodes are no PTYs in use.
    #[test]
    fn macos_census_counts_only_ttys_nnn() {
        assert!(is_macos_pty_slave("ttys000"));
        assert!(is_macos_pty_slave("ttys510"));
        assert!(!is_macos_pty_slave("ttys0"));
        assert!(!is_macos_pty_slave("ttys9"));
        assert!(!is_macos_pty_slave("ttysf"));
        assert!(!is_macos_pty_slave("ttys"));
        assert!(!is_macos_pty_slave("tty.usb"));
    }

    #[test]
    fn ptmx_holders_count_masters_per_process() {
        let lsof = "p7358\ncghostty\nf9\np13453\ncvtmux\nf8\nf10\nf11\np99\ncidle\n";
        assert_eq!(
            ptmx_holders(lsof),
            [
                (3, "13453".to_owned(), "vtmux".to_owned()),
                (1, "7358".to_owned(), "ghostty".to_owned()),
            ]
        );
    }

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

    #[test]
    fn patches_are_applied_inside_a_checkout() {
        // Under target/ so the temp copy sits inside this git repository, the
        // case where a plain `git apply` skips a git-format patch and exits 0.
        let base = workspace_root()
            .join("target")
            .join(format!("regress-test-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let suite = workspace_root().join(REGRESS_REL);
        let empty = base.join("none");
        fs::create_dir_all(&empty).unwrap();
        let logs = base.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let run = |patches: &Path, work: &str| {
            let dir = prepare_work_from(&suite, patches, &base.join(work)).unwrap();
            // /usr/bin/true exists on Linux and macOS; /bin/true is Linux-only.
            run_one(Path::new("/usr/bin/true"), &dir, &logs, "conf-syntax.sh").outcome
        };
        assert_eq!(run(&empty, "plain"), Outcome::Pass);
        assert!(matches!(run(&fixtures, "patched"), Outcome::Fail(_)));

        let bad = base.join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(
            bad.join("x.patch"),
            "--- a/regress/nope.sh\n+++ b/regress/nope.sh\n@@ -1 +1 @@\n-a\n+b\n",
        )
        .unwrap();
        let err = prepare_work_from(&suite, &bad, &base.join("badwork")).unwrap_err();
        assert!(err.to_string().contains("x.patch"), "{err}");

        let garbage = base.join("garbage");
        fs::create_dir_all(&garbage).unwrap();
        fs::write(garbage.join("y.patch"), "not a patch\n").unwrap();
        let err = prepare_work_from(&suite, &garbage, &base.join("gwork")).unwrap_err();
        assert!(err.to_string().contains("y.patch"), "{err}");
        let _ = fs::remove_dir_all(&base);
    }
}
