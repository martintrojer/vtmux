//! Teardown for test servers. A server daemonizes (setsid), so it leaves the
//! process group of whatever started it; a failing or timed-out test never
//! reaches its own `kill-server`. Harnesses call this after every run (pass,
//! fail, timeout or panic) so no server outlives it and holds its PTYs.
//!
//! `cargo xtask reap` is the other half: servers nobody's harness owns, left by
//! ad-hoc probe scripts (`-L rv*`, `att*`, ...) that never ran `kill-server`.
//! It kills test-labelled `tmux`/`vtmux` servers that are old and orphaned,
//! with their pane processes (a flood loop in a pane outlives the server's
//! death otherwise). `check.sh` runs it first.

use std::collections::{HashMap, HashSet};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

fn signal(sig: &str, pids: &[u32]) {
    let _ = Command::new("kill")
        .arg(sig)
        .args(pids.iter().map(u32::to_string))
        .stderr(Stdio::null())
        .status();
}

/// `pid args...` for every process (`ps -axww`: works on Linux and macOS).
pub fn ps() -> String {
    Command::new("ps")
        .args(["-axww", "-o", "pid=,args="])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Kill the processes `select` picks from the `ps` listing: SIGTERM, up to two
/// seconds of grace, then SIGKILL for whatever still lives.
pub fn kill_matching(select: impl Fn(&str) -> Vec<u32>) {
    let mut pids = select(&ps());
    if pids.is_empty() {
        return;
    }
    signal("-TERM", &pids);
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(100));
        pids = select(&ps());
        if pids.is_empty() {
            return;
        }
    }
    signal("-KILL", &pids);
}

/// Pids in the listing whose arguments satisfy `keep`, never `own`.
pub fn pids_where(ps: &str, own: u32, keep: impl Fn(&str) -> bool) -> Vec<u32> {
    ps.lines()
        .filter_map(|l| {
            let (pid, args) = l.trim_start().split_once(' ')?;
            let pid: u32 = pid.parse().ok()?;
            (pid != own && keep(args)).then_some(pid)
        })
        .collect()
}

/// Servers younger than this are left alone: a gate, a harness run or a
/// reviewer's script may still be using them (the slow gate budget is 10 min).
const DEFAULT_MIN_AGE: Duration = Duration::from_hours(1);

/// One row of `ps -o pid=,ppid=,etime=,args=`.
#[derive(Debug, PartialEq, Eq)]
struct Proc {
    pid: u32,
    ppid: u32,
    age: Duration,
    args: String,
}

fn ps_full() -> String {
    Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,etime=,args="])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// `[[dd-]hh:]mm:ss`, the `etime` format of both Linux and macOS `ps`.
fn parse_etime(s: &str) -> Option<Duration> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let mut secs = 0;
    let mut parts = 0;
    for p in rest.split(':') {
        secs = secs * 60 + p.parse::<u64>().ok()?;
        parts += 1;
    }
    (parts <= 3).then(|| Duration::from_secs(days * 86_400 + secs))
}

fn parse_procs(ps: &str) -> Vec<Proc> {
    ps.lines()
        .filter_map(|l| {
            let mut rest = l;
            let mut field = || {
                let (f, r) = rest.trim_start().split_once(char::is_whitespace)?;
                rest = r;
                Some(f)
            };
            let pid = field()?.parse().ok()?;
            let parent = field()?.parse().ok()?;
            let age = parse_etime(field()?)?;
            Some(Proc {
                pid,
                ppid: parent,
                age,
                args: rest.trim().to_string(),
            })
        })
        .collect()
}

/// `-L` labels the suites and the usual probe scripts use: `testA$$` and
/// friends (regress), `cli$$` and `att$$-n` (integration tests), `i$$`,
/// `covers-*`, and `rv*` (reviewers' probes). The owner's own server uses the
/// default label and never matches.
fn is_test_label(l: &str) -> bool {
    let numbered = |prefix: &str| {
        l.strip_prefix(prefix)
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
    };
    l.starts_with("test")
        || l.starts_with("rv")
        || l.starts_with("covers-")
        || numbered("cli")
        || numbered("att")
        || numbered("i")
}

/// Is this a `tmux`/`vtmux` command line whose socket is a test one: a global
/// `-L` label from [`is_test_label`], or an oracle run's `-S /tmp/vto-*/s`?
fn is_test_server(args: &str) -> bool {
    let mut words = args.split_whitespace();
    let Some(argv0) = words.next() else {
        return false;
    };
    if !matches!(argv0.rsplit('/').next(), Some("tmux" | "vtmux")) {
        return false;
    }
    // Global flags only: stop at the command. -L -S -f -c -T take a value.
    while let Some(w) = words.next() {
        let Some(flag) = w.strip_prefix('-') else {
            return false;
        };
        let value = match flag.chars().next() {
            Some(f @ ('L' | 'S' | 'f' | 'c' | 'T')) if flag.len() == 1 => {
                words.next().map(|v| (f, v))
            }
            Some(f @ ('L' | 'S')) => Some((f, &flag[1..])),
            _ => None,
        };
        match value {
            Some(('L', l)) if is_test_label(l) => return true,
            Some(('S', s)) if s.starts_with("/tmp/vto-") => return true,
            _ => {}
        }
    }
    false
}

/// A parent that adopts orphans: init, or the per-user/subreaper manager.
fn is_init_like(p: &Proc) -> bool {
    p.pid == 1
        || matches!(
            p.args
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit('/').next()),
            Some("systemd" | "init" | "launchd")
        )
}

/// `own`, its ancestors (the shell, the harness, the agent running us) and its
/// descendants: never reaped, whatever they look like.
fn protected(procs: &[Proc], own: u32) -> HashSet<u32> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut below = HashSet::from([own]);
    descendants(procs, &mut below);
    let mut keep = below;
    keep.insert(own);
    let mut at = own;
    while let Some(p) = by_pid.get(&at) {
        if p.ppid == 0 || !keep.insert(p.ppid) {
            break;
        }
        at = p.ppid;
    }
    keep
}

/// Grow `set` with everything below it.
fn descendants(procs: &[Proc], set: &mut HashSet<u32>) {
    loop {
        let more: Vec<u32> = procs
            .iter()
            .filter(|p| !set.contains(&p.pid) && set.contains(&p.ppid))
            .map(|p| p.pid)
            .collect();
        if more.is_empty() {
            return;
        }
        set.extend(more);
    }
}

/// Pids to kill: every orphaned (parent is init-like), old enough,
/// test-labelled server, then everything below it (pane programs). Never
/// `own`'s process tree or ancestors ([`protected`]).
fn orphans(procs: &[Proc], min_age: Duration, own: u32) -> Vec<u32> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let keep = protected(procs, own);
    let mut hit: HashSet<u32> = procs
        .iter()
        .filter(|p| {
            !keep.contains(&p.pid)
                && p.age >= min_age
                && is_test_server(&p.args)
                && by_pid
                    .get(&p.ppid)
                    .is_some_and(|parent| is_init_like(parent))
        })
        .map(|p| p.pid)
        .collect();
    descendants(procs, &mut hit);
    let mut v: Vec<u32> = hit.difference(&keep).copied().collect();
    v.sort_unstable();
    v
}

/// Kill `pids` (children first is not needed: all die within the grace).
fn kill_pids(pids: &[u32]) {
    kill_matching(|ps| {
        let alive: HashSet<u32> = pids_where(ps, 0, |_| true).into_iter().collect();
        pids.iter().copied().filter(|p| alive.contains(p)).collect()
    });
}

/// `cargo xtask reap [--min-age <minutes>] [--dry-run]`.
pub fn run() -> ExitCode {
    let mut min_age = DEFAULT_MIN_AGE;
    let mut dry = false;
    let mut args = std::env::args().skip(2);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dry-run" => dry = true,
            "--min-age" => {
                let Some(m) = args.next().and_then(|v| v.parse::<u64>().ok()) else {
                    eprintln!("--min-age needs a number of minutes");
                    return ExitCode::from(2);
                };
                min_age = Duration::from_mins(m);
            }
            _ => {
                eprintln!("usage: cargo xtask reap [--min-age <minutes>] [--dry-run]");
                return ExitCode::from(2);
            }
        }
    }
    let procs = parse_procs(&ps_full());
    let pids = orphans(&procs, min_age, std::process::id());
    for p in procs.iter().filter(|p| pids.contains(&p.pid)) {
        let head: String = p.args.chars().take(100).collect();
        eprintln!("reap {} ({}s old): {head}", p.pid, p.age.as_secs());
    }
    eprintln!(
        "{} {} orphaned test process(es)",
        if dry { "would reap" } else { "reaped" },
        pids.len()
    );
    if !dry {
        kill_pids(&pids);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(rows: &[&str]) -> Vec<Proc> {
        parse_procs(&rows.join("\n"))
    }

    #[test]
    fn etime_formats() {
        let s = Duration::from_secs;
        assert_eq!(parse_etime("00:07"), Some(s(7)));
        assert_eq!(parse_etime("12:07"), Some(s(727)));
        assert_eq!(parse_etime("1:02:03"), Some(s(3723)));
        assert_eq!(parse_etime("2-01:02:03"), Some(s(2 * 86_400 + 3723)));
        assert_eq!(parse_etime("x"), None);
    }

    #[test]
    fn test_servers_are_told_from_the_owners() {
        for yes in [
            "/w/target/debug/vtmux -L rv1 -f /dev/null new -d",
            "tmux -Lrv_probe -f/dev/null new",
            "/c/tmux -LtestA42 -f/dev/null new -d",
            "vtmux -L att99-3 -f /dev/null attach",
            "vtmux -L cli7 -f /dev/null start",
            "tmux -2 -L i42 new",
            "tmux -S /tmp/vto-a1b2c3/s -f /dev/null new",
        ] {
            assert!(is_test_server(yes), "{yes}");
        }
        for no in [
            "tmux new-session -d -s dotfiles",
            "tmux -L work new",
            "tmux -L cliff new",
            "tmux -f /dev/null -L other new",
            "vtmux new -s rv1 -L rv1",
            "tmux -S /run/user/1/sock new",
            "/usr/bin/python3 /tmp/rv_1/x.py -Lrv1",
            "tmuxer -Lrv1 x",
        ] {
            assert!(!is_test_server(no), "{no}");
        }
    }

    #[test]
    fn only_old_orphans_and_their_children_are_picked() {
        let procs = ps(&[
            "1 0 99:00:00 /sbin/init",
            "50 1 99:00:00 /usr/lib/systemd/systemd --user",
            // Old orphans (parent init, then a systemd --user) with a busy child.
            "100 1 02:00:00 vtmux -L rv1 -f /dev/null new -d",
            "101 100 02:00:00 sh -c while :; do :; done",
            "102 101 02:00:00 cat /proc/self/fd/0",
            "110 50 02:00:00 tmux -L att5-1 -f /dev/null new",
            // Too young.
            "120 1 00:30 vtmux -L rv2 new",
            // A live harness: the parent is the test, not init.
            "130 200 05:00:00 vtmux -L cli9 new",
            "200 300 05:00:00 test-binary",
            // The owner's server.
            "140 1 05:00:00 tmux new-session -d -s dotfiles",
            // Our own pid is never picked.
            "150 1 05:00:00 xtask -L rv3",
        ]);
        let hour = Duration::from_secs(3600);
        assert_eq!(orphans(&procs, hour, 150), [100, 101, 102, 110]);
        assert_eq!(
            orphans(&procs, Duration::ZERO, 150),
            [100, 101, 102, 110, 120]
        );
    }

    /// The reaper must never pick its own ancestors or descendants, even when
    /// their command lines look like old orphaned test servers (a shell whose
    /// command line mentions `-L rv1`, a harness run from init).
    #[test]
    fn own_tree_is_never_picked() {
        let procs = ps(&[
            "1 0 99:00:00 /sbin/init",
            // Ancestors: an agent under init whose argv embeds a probe command.
            "10 1 05:00:00 tmux -L rv1 -f /dev/null new",
            "11 10 05:00:00 bash -c pkill -f rv1",
            "12 11 05:00:00 xtask reap",
            // A descendant that looks like a server, and its child.
            "13 12 05:00:00 vtmux -L rv2 new",
            "14 13 05:00:00 sleep 5",
            // A real orphan elsewhere.
            "20 1 05:00:00 vtmux -L rv3 new",
            "21 20 05:00:00 sleep 5",
        ]);
        assert_eq!(orphans(&procs, Duration::ZERO, 12), [20, 21]);
    }

    /// Real processes: a daemonised fake server (its parent exits, so init or
    /// the subreaper adopts it) is reaped, a live child of ours is not.
    #[test]
    fn real_orphan_is_reaped_and_live_server_is_not() {
        let tag = format!("rvunit{}", std::process::id());
        let fake = |label: &str| format!("exec -a \"tmux -L{label} -f/dev/null new\" sleep 1000");
        let orphan = format!("{tag}o");
        let live = format!("{tag}l");
        Command::new("sh")
            .args(["-c", &format!("(bash -c '{}' &)", fake(&orphan))])
            .status()
            .unwrap();
        let mut child = Command::new("bash")
            .args(["-c", &fake(&live)])
            .spawn()
            .unwrap();
        let mine = |args: &str| args.contains(&format!("-L{tag}"));
        let picked = |procs: &[Proc]| -> Vec<String> {
            let own = std::process::id();
            orphans(procs, Duration::ZERO, own)
                .into_iter()
                .filter_map(|p| procs.iter().find(|q| q.pid == p))
                .filter(|p| mine(&p.args))
                .map(|p| p.args.clone())
                .collect()
        };
        // Poll: under load the orphan takes a while to be adopted and exec.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (procs, got) = loop {
            thread::sleep(Duration::from_millis(100));
            let mut procs = parse_procs(&ps_full());
            // A subreaper above the test (muzzle, under the gate) adopts the
            // orphan instead of init; show it to `orphans` as init's, which
            // is what a dead agent's leftovers look like.
            for p in procs.iter_mut().filter(|p| p.args.contains(&orphan)) {
                p.ppid = 1;
            }
            let got = picked(&procs);
            if got.len() == 1 || std::time::Instant::now() >= deadline {
                break (procs, got);
            }
        };
        let diag: Vec<_> = procs
            .iter()
            .filter(|p| p.args.contains(&tag))
            .map(|p| {
                (
                    p.pid,
                    p.ppid,
                    p.args.clone(),
                    procs
                        .iter()
                        .find(|q| q.pid == p.ppid)
                        .map(|q| q.args.clone()),
                )
            })
            .collect();
        assert_eq!(got.len(), 1, "{got:?} DIAG {diag:#?}");
        assert!(got[0].contains(&orphan), "{got:?}");
        // Reap only ours (other worktrees' servers may be running).
        let pids: Vec<u32> = procs
            .iter()
            .filter(|p| mine(&p.args) && p.args.contains(&orphan))
            .map(|p| p.pid)
            .collect();
        kill_pids(&pids);
        let after = parse_procs(&ps_full());
        assert!(!after.iter().any(|p| p.args.contains(&orphan)));
        assert!(after.iter().any(|p| p.args.contains(&live)));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
