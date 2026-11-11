//! Teardown for test servers. A server daemonizes (setsid), so it leaves the
//! process group of whatever started it; a failing or timed-out test never
//! reaches its own `kill-server`. Harnesses call this after every run (pass,
//! fail, timeout or panic) so no server outlives it and holds its PTYs.
//!
//! `cargo xtask reap` is the other half: servers nobody's harness owns, left by
//! ad-hoc probe scripts (`-L rv*`, `att*`, ...) that never ran `kill-server`.
//! It kills old, orphaned processes holding a test socket, with their pane
//! processes (a flood loop in a pane outlives the server's death otherwise).
//! `check.sh` runs it first. Which socket a server holds is asked of the
//! kernel ([`held_sockets`]: `/proc/net/unix` and `/proc/<pid>/fd` on Linux,
//! `lsof` on macOS), never read from its command line, which does not say:
//! `-L rvpath/../default` is `default`'s socket, a server with neither `-L`
//! nor `-S` takes `$TMUX`'s, and macOS's `ps` joins the words of a value
//! with a space in it. A test socket is a real path `.../tmux-<uid>/<label>`
//! or `vtmux-<uid>` with a test label, or an oracle run's
//! ([`is_test_socket`]).
//!
//! A server leaves its socket file behind when it exits (tmux does too), so
//! a harness that lets a server use the real socket directory strands one
//! dead file per run. [`sweep_sockets`] removes the dead test-labelled ones
//! from `$TMUX_TMPDIR/{tmux,vtmux}-<uid>/` and `/tmp/{tmux,vtmux}-<uid>/`;
//! `cargo xtask sockets` lists the test-labelled entries there, which is what
//! `check.sh` diffs around the suites to catch a harness that leaks.
//!
//! A server with `vtmux-persist` on keeps a checkpoint directory per socket,
//! `$XDG_STATE_HOME/vtmux/<label>-<hash>/` (`vtmux_server::persist::state_dir`),
//! which outlives it too. [`sweep_checkpoints`] empties the test-labelled
//! ones whose server is gone: no socket held open whose path names the
//! directory, none of those directories' socket files answering, no
//! server holding the directory's `lock` (the sweep holds it while it
//! empties that one directory), and nothing in it changed for `--min-age`.
//! When any of that cannot be told, the directory stays. The `lock` file
//! itself stays, for a server that may have it open.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, connect, socket};

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
/// A checkpoint directory changed more recently than this stays too.
const DEFAULT_MIN_AGE: Duration = Duration::from_hours(1);

/// One row of `ps -o pid=,ppid=,etime=,args=`. The arguments are only
/// shown (and tell init apart): nothing is decided from them, since a
/// command line does not say which socket a server took (`-L` with path
/// parts, `$TMUX`, macOS's `ps` joining the words of a value with a space).
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

/// A Unix socket bound to a path, as the kernel lists it. The kernel
/// keeps a socket only while some process holds it open, so each is a
/// running server's (or probe's), whatever its command line says; a socket
/// file nobody holds any more is not listed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Held {
    /// The process holding it, when that can be read (on Linux, a process
    /// of another user, or one exiting, does not show its descriptors).
    pid: Option<u32>,
    /// The path as it was bound: relative for a relative `-S`.
    path: PathBuf,
    /// The holder's working directory, which a relative `path` is from.
    cwd: Option<PathBuf>,
}

impl Held {
    /// The absolute path the server of this socket names its checkpoint
    /// directory by (`std::path::absolute` in its own directory, which it
    /// keeps: `Server::persist_dir`, `daemon(1, 0)`): `..` stays, as it
    /// does there. `None` for a relative path whose holder is unknown.
    fn absolute(&self) -> Option<PathBuf> {
        let path = if self.path.is_absolute() {
            self.path.clone()
        } else {
            self.cwd.as_ref()?.join(&self.path)
        };
        std::path::absolute(path).ok()
    }

    /// The socket's real path: its directory's `realpath` with its name,
    /// so `-L rvpath/../default` is `<dir>/default`.
    fn canonical(&self) -> Option<PathBuf> {
        let path = self.absolute()?;
        let name = path.file_name()?;
        Some(path.parent()?.canonicalize().ok()?.join(name))
    }

    /// Whether this may be the socket of checkpoint directory
    /// `<label>-<hash>` (`vtmux_server::persist::state_dir`): its name is
    /// `label`, and its absolute path hashes to `hash` or is unknown.
    fn may_own(&self, label: &str, hash: &str) -> bool {
        self.path.file_name() == Some(label.as_ref())
            && self
                .absolute()
                .is_none_or(|p| vtmux_server::persist::path_hash(&p) == hash)
    }
}

/// The sockets bound to a path and held open now, with their holders.
/// `None` when that cannot be told: then nothing is killed and every
/// checkpoint directory stays.
#[cfg(target_os = "linux")]
pub fn held_sockets() -> Option<Vec<Held>> {
    held_in_proc(Path::new("/proc"))
}

/// The sockets bound to a path and held open now (`lsof -U`), with their
/// holders and, for a relative path, their directories (`lsof -d cwd`).
#[cfg(target_os = "macos")]
pub fn held_sockets() -> Option<Vec<Held>> {
    let lsof = |args: &[&str]| {
        let out = Command::new("lsof")
            .args(["-n", "-w", "-F", "pftn"])
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        (out.status.success() || !out.stdout.is_empty()).then_some(out.stdout)
    };
    let mut held = parse_lsof_sockets(&lsof(&["-U"])?);
    let relative: Vec<String> = held
        .iter()
        .filter(|h| h.path.is_relative())
        .filter_map(|h| h.pid.map(|p| p.to_string()))
        .collect();
    if !relative.is_empty() {
        let list = relative.join(",");
        let cwds = parse_lsof_cwds(&lsof(&["-a", "-d", "cwd", "-p", list.as_str()])?);
        for h in &mut held {
            h.cwd = h.pid.and_then(|p| cwds.get(&p).cloned());
        }
    }
    Some(held)
}

/// No way to tell elsewhere.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn held_sockets() -> Option<Vec<Held>> {
    None
}

/// `net/unix` of a `/proc` tree: the inode and path of every socket bound
/// to a path (not an abstract one, `@...`). A line is `Num: RefCount
/// Protocol Flags Type St Inode`, then a space and the path as bound, if
/// any (`unix_seq_show`, `net/unix/af_unix.c`): the rest of the line, spaces
/// and all.
#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(
        dead_code,
        reason = "Linux reads `/proc`; the tests read a fake one everywhere"
    )
)]
fn parse_net_unix(table: &[u8]) -> Vec<(u64, PathBuf)> {
    use std::os::unix::ffi::OsStrExt as _;
    table
        .split(|&b| b == b'\n')
        .skip(1)
        .filter_map(|line| {
            let mut rest = line;
            let mut field = || {
                rest = &rest[rest.iter().position(|&b| b != b' ')?..];
                let end = rest.iter().position(|&b| b == b' ')?;
                let f = &rest[..end];
                rest = &rest[end + 1..];
                Some(f)
            };
            for _ in 0..6 {
                field()?;
            }
            let inode = std::str::from_utf8(field()?).ok()?.parse().ok()?;
            (!rest.is_empty() && rest[0] != b'@')
                .then(|| (inode, PathBuf::from(std::ffi::OsStr::from_bytes(rest))))
        })
        .collect()
}

/// The sockets of `proc`'s table (`net/unix`), each with the processes
/// holding it (`<pid>/fd/*` linking to `socket:[<inode>]`) and their
/// directories (`<pid>/cwd`); one no readable process holds is listed
/// with none. `proc` is `/proc`, or a test's fake of it. `None` when the
/// table does not read.
#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(
        dead_code,
        reason = "Linux reads `/proc`; the tests read a fake one everywhere"
    )
)]
fn held_in_proc(proc: &Path) -> Option<Vec<Held>> {
    let table = parse_net_unix(&std::fs::read(proc.join("net/unix")).ok()?);
    let by_inode: HashMap<u64, &PathBuf> = table.iter().map(|(i, p)| (*i, p)).collect();
    let mut held = Vec::new();
    let mut seen = HashSet::new();
    for entry in std::fs::read_dir(proc).ok()?.filter_map(Result::ok) {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let mut cwd = None;
        for fd in fds.filter_map(Result::ok) {
            let Some(inode) = std::fs::read_link(fd.path()).ok().and_then(|t| {
                t.to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse::<u64>()
                    .ok()
            }) else {
                continue;
            };
            let Some(&path) = by_inode.get(&inode) else {
                continue;
            };
            seen.insert(inode);
            let cwd = cwd
                .get_or_insert_with(|| std::fs::read_link(entry.path().join("cwd")).ok())
                .clone();
            held.push(Held {
                pid: Some(pid),
                path: path.clone(),
                cwd,
            });
        }
    }
    held.extend(
        table
            .iter()
            .filter(|(i, _)| !seen.contains(i))
            .map(|(_, path)| Held {
                pid: None,
                path: path.clone(),
                cwd: None,
            }),
    );
    held.sort();
    held.dedup();
    Some(held)
}

/// Whether a socket that may be checkpoint directory `<label>-<hash>`'s
/// ([`Held::may_own`]) is bound now, as far as a cheap look can tell: on
/// Linux the kernel's table (`/proc/net/unix`), whoever holds it (a
/// relative path counts by its name), true when the table does not read;
/// elsewhere false (the sweep asks the socket files then:
/// `sweep_checkpoints`).
fn bound_now(label: &str, hash: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read("/proc/net/unix").map_or(true, |t| {
            parse_net_unix(&t).into_iter().any(|(_, path)| {
                Held {
                    pid: None,
                    path,
                    cwd: None,
                }
                .may_own(label, hash)
            })
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (label, hash);
        false
    }
}

/// `lsof -F pftn` output: `p<pid>` starts a process, `f<fd>` one of its
/// files, `t<type>` and `n<name>` describe it.
#[cfg_attr(
    all(not(target_os = "macos"), not(test)),
    expect(dead_code, reason = "macOS reads `lsof`; the tests read it everywhere")
)]
fn parse_lsof(out: &[u8]) -> Vec<(u32, String, String, String)> {
    let out = String::from_utf8_lossy(out);
    let mut files: Vec<(u32, String, String, String)> = Vec::new();
    let mut pid = None;
    for line in out.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => pid = value.parse().ok(),
            'f' => {
                if let Some(pid) = pid {
                    files.push((pid, value.to_owned(), String::new(), String::new()));
                }
            }
            't' | 'n' => {
                if let Some(file) = files.last_mut().filter(|f| Some(f.0) == pid) {
                    value.clone_into(if tag == 't' { &mut file.2 } else { &mut file.3 });
                }
            }
            _ => {}
        }
    }
    files
}

/// The sockets bound to a path in `lsof -U -F pftn` output: a `unix` file
/// whose name is a path, not a peer (`->0x...`) or nothing.
#[cfg_attr(
    all(not(target_os = "macos"), not(test)),
    expect(dead_code, reason = "macOS reads `lsof`; the tests read it everywhere")
)]
fn parse_lsof_sockets(out: &[u8]) -> Vec<Held> {
    let mut held: Vec<Held> = parse_lsof(out)
        .into_iter()
        .filter(|(_, _, kind, name)| kind == "unix" && !name.is_empty() && !name.starts_with("->"))
        .map(|(pid, _, _, name)| Held {
            pid: Some(pid),
            path: PathBuf::from(name),
            cwd: None,
        })
        .collect();
    held.sort();
    held.dedup();
    held
}

/// Each process's directory in `lsof -d cwd -F pftn` output.
#[cfg_attr(
    all(not(target_os = "macos"), not(test)),
    expect(dead_code, reason = "macOS reads `lsof`; the tests read it everywhere")
)]
fn parse_lsof_cwds(out: &[u8]) -> HashMap<u32, PathBuf> {
    parse_lsof(out)
        .into_iter()
        .filter(|(_, fd, _, name)| fd == "cwd" && !name.is_empty())
        .map(|(pid, _, _, name)| (pid, PathBuf::from(name)))
        .collect()
}

/// `-L` labels the suites and the usual probe scripts use: `testA$$` and
/// friends (regress), `cli$$`, `att$$-n` and `ctl$$-n` (integration tests),
/// `i$$`, `covers-*`, and `rv*` (reviewers' probes). The owner's own server
/// uses the default label and never matches.
pub fn is_test_label(l: &str) -> bool {
    let numbered = |prefix: &str| {
        l.strip_prefix(prefix)
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
    };
    l.starts_with("test")
        || l.starts_with("rv")
        || l.starts_with("covers-")
        || numbered("cli")
        || numbered("att")
        || numbered("ctl")
        || numbered("i")
}

/// Whether real socket path `socket` is a test one: `<dir>/tmux-<uid>/<label>`
/// or `<dir>/vtmux-<uid>/<label>` with a test label ([`is_test_label`]),
/// where `-L <label>` puts it under any `TMUX_TMPDIR`, or an oracle run's
/// `<tmp>/vto-*/s` (`tmp` is `/tmp`'s real path). The path is real
/// ([`Held::canonical`]), so its name has no path parts and its directory
/// is the one it is in: `-L rvpath/../default` is `default`'s socket.
fn is_test_socket(socket: &Path, uid: u32, tmp: &Path) -> bool {
    let name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    let Some(dir) = socket.parent() else {
        return false;
    };
    let (Some(label), Some(dir_name)) = (name(socket), name(dir)) else {
        return false;
    };
    let socket_dir = dir_name == format!("tmux-{uid}") || dir_name == format!("vtmux-{uid}");
    let oracle = label == "s" && dir_name.starts_with("vto-") && dir.parent() == Some(tmp);
    oracle || (socket_dir && is_test_label(&label))
}

/// The processes holding a test socket ([`is_test_socket`]).
fn test_servers(held: &[Held], uid: u32, tmp: &Path) -> HashSet<u32> {
    held.iter()
        .filter_map(|h| {
            let pid = h.pid?;
            is_test_socket(&h.canonical()?, uid, tmp).then_some(pid)
        })
        .collect()
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

/// Pids to kill: every orphaned (parent is init-like), old enough test
/// server (one of `servers`, [`test_servers`]), then everything below it
/// (pane programs). Never `own`'s process tree or ancestors
/// ([`protected`]).
fn orphans(procs: &[Proc], servers: &HashSet<u32>, min_age: Duration, own: u32) -> Vec<u32> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let keep = protected(procs, own);
    let mut hit: HashSet<u32> = procs
        .iter()
        .filter(|p| {
            !keep.contains(&p.pid)
                && p.age >= min_age
                && servers.contains(&p.pid)
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

/// Capture selected processes and their descendants before killing a server:
/// its pane programs may ignore HUP and be reparented when it exits.
/// Never include this process or its ancestors, even if a selector picks one.
pub fn matching_tree(select: impl Fn(&str) -> Vec<u32>) -> Vec<u32> {
    let procs = parse_procs(&ps_full());
    let mut listing = String::new();
    for p in &procs {
        writeln!(listing, "{} {}", p.pid, p.args).unwrap();
    }
    let mut hit: HashSet<u32> = select(&listing).into_iter().collect();
    descendants(&procs, &mut hit);
    let mut at = std::process::id();
    loop {
        hit.remove(&at);
        let Some(parent) = procs.iter().find(|p| p.pid == at).map(|p| p.ppid) else {
            break;
        };
        if parent == 0 || parent == at {
            break;
        }
        at = parent;
    }
    let mut pids: Vec<_> = hit.into_iter().collect();
    pids.sort_unstable();
    pids
}

/// Kill `pids` (children first is not needed: all die within the grace).
pub fn kill_pids(pids: &[u32]) {
    kill_matching(|ps| {
        let alive: HashSet<u32> = pids_where(ps, 0, |_| true).into_iter().collect();
        pids.iter().copied().filter(|p| alive.contains(p)).collect()
    });
}

/// The directories a server of user `uid` keeps its sockets in when it is not
/// given `-S`: `<base>/tmux-<uid>` (tmux) or `<base>/vtmux-<uid>` (vtmux, D20)
/// for the base `$TMUX_TMPDIR` names and `/tmp` (tmux.h `TMUX_SOCK`,
/// tmux.c:159-198), deduplicated after `realpath` and only those that exist.
fn socket_dirs(tmux_tmpdir: Option<&Path>, uid: u32) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    for base in tmux_tmpdir
        .filter(|t| !t.as_os_str().is_empty())
        .into_iter()
        .chain([Path::new("/tmp")])
    {
        if let Ok(real) = base.canonicalize()
            && !bases.contains(&real)
        {
            bases.push(real);
        }
    }
    bases
        .iter()
        .flat_map(|b| ["tmux", "vtmux"].map(|n| b.join(format!("{n}-{uid}"))))
        .filter(|d| d.is_dir())
        .collect()
}

fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// [`socket_dirs`] for this user and environment.
pub fn default_socket_dirs() -> Vec<PathBuf> {
    let tmpdir = std::env::var_os("TMUX_TMPDIR").map(PathBuf::from);
    socket_dirs(tmpdir.as_deref(), current_uid())
}

/// Test-labelled entries of `dirs`, sorted: what a harness added is what a
/// later listing has over an earlier one.
pub fn test_sockets(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(is_test_label))
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

/// A non-blocking `connect()` to the socket at `p`. The flags are set after
/// `socket()`, not passed to it: macOS has no `SOCK_NONBLOCK`/`SOCK_CLOEXEC`.
fn probe_connect(p: &Path) -> rustix::io::Result<()> {
    let s = socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
    rustix::io::fcntl_setfd(&s, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::ioctl_fionbio(&s, true)?;
    connect(&s, &SocketAddrUnix::new(p)?)
}

/// Remove the sockets in `dirs` that are test-labelled ([`is_test_label`]),
/// satisfy `also`, and are dead: a `connect()` is refused, which is what a
/// socket file with no server behind it gives. Live ones (a connect succeeds
/// or would block), `default`, other labels and anything that is not a socket
/// stay. Returns the paths removed, or with `dry` the ones that would be.
pub fn sweep_sockets(dirs: &[PathBuf], also: impl Fn(&str) -> bool, dry: bool) -> Vec<PathBuf> {
    test_sockets(dirs)
        .into_iter()
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(&also)
                && p.symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_socket())
                && matches!(probe_connect(p), Err(rustix::io::Errno::CONNREFUSED))
        })
        .filter(|p| dry || std::fs::remove_file(p).is_ok())
        .collect()
}

/// `$XDG_STATE_HOME/vtmux` (else `~/.local/state/vtmux`), as a server of
/// this environment would name it (`vtmux_server::persist::state_dir`).
pub fn default_state_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    vtmux_server::persist::state_dir(
        Path::new("/s"),
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        home.as_deref(),
    )
    .and_then(|d| d.parent().map(Path::to_path_buf))
}

/// The lock of checkpoint directory `dir` (`vtmux_server::persist::Lock`),
/// taken exclusive without waiting: `None` while another holds it, or when
/// the file does not open. A server taking it meanwhile waits a moment
/// (`Lock::take`) or retries past it (a periodic checkpoint's
/// `Lock::try_take`, `Server::persist_lock`). The file is made if missing,
/// as a server would (in a dry run nothing is made: no lock file, no
/// holder).
fn take_checkpoint_lock(dir: &Path, dry: bool) -> Option<Taken> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = dir.join(vtmux_server::persist::LOCK);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(!dry)
        .create(!dry)
        .truncate(false)
        .mode(0o600)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if dry && e.kind() == std::io::ErrorKind::NotFound => return Some(Taken::NoFile),
        Err(_) => return None,
    };
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).ok()?;
    Some(Taken::Held(file))
}

/// A checkpoint lock [`take_checkpoint_lock`] got.
enum Taken {
    /// Held until dropped.
    Held(#[expect(dead_code, reason = "held for its lock")] std::fs::File),
    /// A dry run's: no lock file to take.
    NoFile,
}

/// Empty checkpoint directory `dir` but for its `lock`, which may be open
/// in a server about to take it: unlinked, the next server would take a
/// new file's lock while that one holds the old, and both would write the
/// directory. True if there was something to remove, and all of it went.
fn empty_checkpoint(dir: &Path, dry: bool) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut found = false;
    let mut all = true;
    for e in entries.filter_map(Result::ok) {
        if e.file_name() == vtmux_server::persist::LOCK {
            continue;
        }
        found = true;
        if dry {
            continue;
        }
        let gone = if e.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(e.path())
        } else {
            std::fs::remove_file(e.path())
        };
        all &= gone.is_ok();
    }
    found && all
}

/// How long since checkpoint directory `dir` changed: since the newest
/// change to it or an entry in it (a checkpoint renames its `manifest`
/// into place, which changes the directory). `None` if any of it does not
/// read.
fn unchanged_for(dir: &Path) -> Option<Duration> {
    let mut newest = dir.metadata().ok()?.modified().ok()?;
    for e in std::fs::read_dir(dir).ok()? {
        newest = newest.max(e.ok()?.metadata().ok()?.modified().ok()?);
    }
    Some(newest.elapsed().unwrap_or(Duration::ZERO))
}

/// Empty the checkpoint directories in `root` (`<label>-<hash>`) whose
/// label is a test one ([`is_test_label`]) and whose server is gone, as
/// far as can be told; when in doubt, a directory stays.
///
/// `held` lists the sockets held open now ([`held_sockets`]), with no lock
/// held; `None` (it cannot be told) keeps every directory. A directory is
/// kept when a held socket may be its own ([`Held::may_own`]: its name,
/// its absolute path hashing as the server's does), whatever command line
/// its server has (a `-L` with path parts, a socket from `$TMUX`), and its
/// lock is never touched; a server binds its socket before it restores,
/// so one starting is held already. Kept too: a directory with a socket
/// `<dir>/<label>` of `socket_dirs` hashing to `<hash>` that answers a
/// `connect()`, and one changed within `min_age`. Then one at a time: its
/// lock is taken, exclusive and without waiting ([`take_checkpoint_lock`]),
/// one a server holds kept; its socket is asked again under it (a socket of
/// its name bound now, [`bound_now`], or a file of `socket_dirs` that
/// answers: a server started since the list binds before its restore takes
/// the lock); and it is emptied, all but the lock file
/// ([`empty_checkpoint`]), and let go. So no lock is held longer than one
/// directory's emptying, far short of the 50 ms a restore's `Lock::take`
/// waits, and a server that starts then finds no checkpoint, never one
/// half removed.
///
/// An emptied directory keeps its `lock` for good (a server may have it
/// open), so it stays, holding that empty file; a later sweep does not
/// list it again.
///
/// `default`, other labels, live servers and anything else stay. Returns
/// the directories emptied, or with `dry` the ones that would be.
pub fn sweep_checkpoints(
    root: &Path,
    socket_dirs: &[PathBuf],
    held: impl FnOnce() -> Option<Vec<Held>>,
    min_age: Duration,
    dry: bool,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let candidates: Vec<(PathBuf, String, String)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (label, hash) = name.rsplit_once('-')?;
            let hashed = hash.len() == 12 && hash.bytes().all(|b| b.is_ascii_hexdigit());
            (hashed && is_test_label(label)).then(|| (e.path(), label.to_owned(), hash.to_owned()))
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let Some(held) = held() else {
        return Vec::new();
    };
    let live = |label: &str, hash: &str| {
        socket_dirs
            .iter()
            .map(|d| d.join(label))
            .filter(|p| vtmux_server::persist::path_hash(p) == hash)
            .any(|p| {
                p.symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_socket())
                    && !matches!(probe_connect(&p), Err(rustix::io::Errno::CONNREFUSED))
            })
    };
    let mut gone: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|(dir, label, hash)| {
            if held.iter().any(|h| h.may_own(label, hash))
                || live(label, hash)
                || unchanged_for(dir).is_none_or(|age| age < min_age)
            {
                return false;
            }
            let Some(lock) = take_checkpoint_lock(dir, dry) else {
                return false;
            };
            let emptied =
                !bound_now(label, hash) && !live(label, hash) && empty_checkpoint(dir, dry);
            drop(lock);
            emptied
        })
        .map(|(dir, _, _)| dir)
        .collect();
    gone.sort();
    gone
}

/// `cargo xtask sockets`: the test-labelled entries of the default socket
/// directories, one path per line, sorted.
pub fn list_sockets() -> ExitCode {
    for p in test_sockets(&default_socket_dirs()) {
        println!("{}", p.display());
    }
    ExitCode::SUCCESS
}

/// `/tmp`'s real path (`/private/tmp` on macOS), where an oracle run's
/// sockets are.
fn real_tmp() -> PathBuf {
    Path::new("/tmp")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
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
    let servers = held_sockets().map_or_else(HashSet::new, |held| {
        test_servers(&held, current_uid(), &real_tmp())
    });
    let pids = orphans(&procs, &servers, min_age, std::process::id());
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
    // After the kill: a killed server's socket file is dead too.
    let socks = sweep_sockets(&default_socket_dirs(), |_| true, dry);
    for p in &socks {
        eprintln!("socket {}", p.display());
    }
    eprintln!(
        "{} {} dead test socket(s)",
        if dry { "would remove" } else { "removed" },
        socks.len()
    );
    // The sockets held once the kill and the socket sweep are done (a
    // server started meanwhile too), but those of the processes just
    // killed, which may linger a moment.
    let held = || {
        let mut held = held_sockets()?;
        held.retain(|h| h.pid.is_none_or(|p| !pids.contains(&p)));
        Some(held)
    };
    let checkpoints = default_state_root()
        .map(|root| sweep_checkpoints(&root, &default_socket_dirs(), held, min_age, dry))
        .unwrap_or_default();
    for p in &checkpoints {
        eprintln!("checkpoint {}", p.display());
    }
    eprintln!(
        "{} {} dead test checkpoint dir(s)",
        if dry { "would empty" } else { "emptied" },
        checkpoints.len()
    );
    ExitCode::SUCCESS
}

/// Helpers shared by this crate's unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// A socket file nobody listens on: bound, then the listener dropped (std
    /// leaves the file behind, as a server that exits does).
    ///
    /// Returns only once a `connect()` is refused. `bind` also listens, and a
    /// parallel test that forks (`Command::spawn`) between `bind` and `drop`
    /// has a copy of the fd until its child execs (`CLOEXEC` acts at exec), so
    /// right after `drop` the socket can still answer for a few ms and a sweep
    /// would take it for live. Once refused it never revives: the fd is closed
    /// here, so no later fork can inherit it.
    pub(crate) fn dead_socket(path: &Path) {
        drop(UnixListener::bind(path).unwrap());
        let give_up = Instant::now() + Duration::from_secs(10);
        while !matches!(
            UnixStream::connect(path),
            Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionRefused
        ) {
            assert!(Instant::now() < give_up, "{path:?} still answers");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(rows: &[&str]) -> Vec<Proc> {
        parse_procs(&rows.join("\n"))
    }

    fn hold(pid: Option<u32>, path: impl Into<PathBuf>, cwd: Option<&Path>) -> Held {
        Held {
            pid,
            path: path.into(),
            cwd: cwd.map(Path::to_path_buf),
        }
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

    /// The kernel's table as Linux prints it: the inode padded, the path
    /// the rest of the line (spaces and all), none for an unbound socket,
    /// `@` for an abstract one.
    #[test]
    fn the_kernel_socket_table_reads_paths_whole() {
        let table = b"Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000000000000000: 00000002 00000000 00010000 0001 01   812 /tmp/vtmux-7/rv1\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 99814\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 99815 @/tmp/.X11-unix/X0\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 99816 /tmp/with space/rv 2\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 99817 rel/rv3\n";
        assert_eq!(
            parse_net_unix(table),
            [
                (812, PathBuf::from("/tmp/vtmux-7/rv1")),
                (99816, PathBuf::from("/tmp/with space/rv 2")),
                (99817, PathBuf::from("rel/rv3")),
            ]
        );
    }

    /// A fake `/proc`: each socket with the processes whose descriptors
    /// link to it and their directories; one no readable process holds
    /// (another user's, one exiting) with none.
    #[test]
    fn held_sockets_come_from_the_kernel_table_and_fd_links() {
        use std::os::unix::fs::symlink;
        let s = Scratch::new("fakeproc");
        let proc = s.0.join("proc");
        std::fs::create_dir_all(proc.join("net")).unwrap();
        std::fs::write(
            proc.join("net/unix"),
            "Num       RefCount Protocol Flags    Type St Inode Path\n\
             0000000000000000: 00000002 00000000 00010000 0001 01 11 /t/vtmux-7/rv a\n\
             0000000000000000: 00000002 00000000 00000000 0001 03 12 /t/vtmux-7/rv a\n\
             0000000000000000: 00000002 00000000 00010000 0001 01 13 rel/rv2\n\
             0000000000000000: 00000002 00000000 00010000 0001 01 14 /t/o/rv3\n\
             0000000000000000: 00000003 00000000 00000000 0001 03 15\n",
        )
        .unwrap();
        let pid = |n: u32, fds: &[&str], cwd: Option<&str>| {
            let dir = proc.join(n.to_string());
            std::fs::create_dir_all(dir.join("fd")).unwrap();
            for (i, t) in fds.iter().enumerate() {
                symlink(t, dir.join("fd").join(i.to_string())).unwrap();
            }
            if let Some(c) = cwd {
                symlink(c, dir.join("cwd")).unwrap();
            }
        };
        // A server: its listener and an accepted connection, which the
        // table lists under the listener's path.
        pid(
            100,
            &["/dev/null", "socket:[11]", "socket:[12]", "pipe:[3]"],
            Some("/w"),
        );
        pid(200, &["socket:[13]", "socket:[15]"], Some("/home/u"));
        // A process whose descriptors do not read (no `fd/`).
        std::fs::create_dir_all(proc.join("300")).unwrap();
        std::fs::create_dir_all(proc.join("self")).unwrap();
        assert_eq!(
            held_in_proc(&proc).unwrap(),
            [
                hold(None, "/t/o/rv3", None),
                hold(Some(100), "/t/vtmux-7/rv a", Some(Path::new("/w"))),
                hold(Some(200), "rel/rv2", Some(Path::new("/home/u"))),
            ]
        );
        assert_eq!(held_in_proc(&s.0.join("missing")), None);
    }

    /// `lsof -F pftn` as macOS prints it: a process's files after its
    /// `p` line, a socket's path whole (spaces too), a peer (`->0x...`) or
    /// an unbound one not a path.
    #[test]
    fn held_sockets_come_from_lsof_on_macos() {
        let sockets = b"p501\nfcwd\ntDIR\nn/Users/u\nf3u\ntunix\nn/private/tmp/vtmux-501/rv a\n\
            f4u\ntunix\nn->0x1f2e3d4c5b6a7980\nf5u\ntunix\nn\n\
            p77\nf6u\ntunix\nnrel/rv2\nf7u\ntunix\nn/private/tmp/vtmux-501/rv a\n";
        assert_eq!(
            parse_lsof_sockets(sockets),
            [
                hold(Some(77), "/private/tmp/vtmux-501/rv a", None),
                hold(Some(77), "rel/rv2", None),
                hold(Some(501), "/private/tmp/vtmux-501/rv a", None),
            ]
        );
        let cwds = b"p77\nfcwd\ntDIR\nn/Users/u/a b\np78\nfcwd\ntDIR\nn/\n";
        assert_eq!(
            parse_lsof_cwds(cwds),
            HashMap::from([
                (77, PathBuf::from("/Users/u/a b")),
                (78, PathBuf::from("/")),
            ])
        );
    }

    /// Which held sockets are test ones, from their real paths: a test
    /// label in a `tmux-<uid>`/`vtmux-<uid>` directory, or an oracle run's
    /// `<tmp>/vto-*/s`. `-L rvpath/../default` reaches `default`; a socket
    /// from `$TMUX` elsewhere, another user's directory, or a relative
    /// path whose holder's directory is unknown are not test ones.
    #[test]
    fn test_servers_are_told_by_the_real_socket_path() {
        use std::os::unix::fs::symlink;
        let s = Scratch::new("realpath");
        let base = s.0.canonicalize().unwrap();
        let socks = base.join("vtmux-7");
        let spaced = base.join("with space/tmux-7");
        for d in [
            socks.join("rvpath"),
            spaced.clone(),
            base.join("o"),
            base.join("vtmux-8"),
            base.join("vto-ab12"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        symlink(&socks, base.join("link")).unwrap();
        let cases = [
            (socks.join("rv1"), None, true),
            (socks.join("testA4"), None, true),
            (socks.join("att3-0"), None, true),
            (socks.join("rvpath/../rv2"), None, true),
            (socks.join("rvpath/../default"), None, false),
            (socks.join("rvpath/../work"), None, false),
            (socks.join("default"), None, false),
            (socks.join("cliff"), None, false),
            (spaced.join("rv s"), None, true),
            (base.join("link/rv5"), None, true),
            (PathBuf::from("vtmux-7/rv6"), Some(base.as_path()), true),
            (PathBuf::from("vtmux-7/rv7"), None, false),
            (base.join("o/rvT"), None, false),
            (base.join("vtmux-8/rv8"), None, false),
            (base.join("vto-ab12/s"), None, false),
            (base.join("rvloose"), None, false),
            (socks.join("rvpath/.."), None, false),
        ];
        let list: Vec<Held> = cases
            .iter()
            .zip(1..)
            .map(|((path, cwd, _), pid)| hold(Some(pid), path, *cwd))
            .chain([hold(None, socks.join("rv9"), None)])
            .collect();
        let mut want: Vec<u32> = cases
            .iter()
            .zip(1..)
            .filter(|((_, _, test), _)| *test)
            .map(|(_, pid)| pid)
            .collect();
        want.sort_unstable();
        let mut got: Vec<u32> = test_servers(&list, 7, Path::new("/nowhere"))
            .into_iter()
            .collect();
        got.sort_unstable();
        assert_eq!(got, want, "{cases:#?}");
        // The oracle's, in the real `/tmp` only.
        let oracle = [hold(Some(1), base.join("vto-ab12/s"), None)];
        assert_eq!(test_servers(&oracle, 7, &base), HashSet::from([1]));
    }

    /// A held socket keeps the checkpoint directory its server names
    /// (`state_dir`: the name, and the hash of the absolute path, `..`
    /// kept); one whose absolute path is unknown keeps every directory of
    /// its name.
    #[test]
    fn a_held_socket_keeps_the_directory_its_server_names() {
        let dir_of = |socket: &str| {
            let dir =
                vtmux_server::persist::state_dir(Path::new(socket), Some("/st".as_ref()), None)
                    .unwrap();
            let name = dir.file_name().unwrap().to_str().unwrap().to_owned();
            let (label, hash) = name.rsplit_once('-').unwrap();
            (label.to_owned(), hash.to_owned())
        };
        let owns = |h: &Held, socket: &str| {
            let (label, hash) = dir_of(socket);
            h.may_own(&label, &hash)
        };
        let dotted = hold(Some(1), "/t/vtmux-7/rvp/../rv1", None);
        assert!(owns(&dotted, "/t/vtmux-7/rvp/../rv1"));
        assert!(!owns(&dotted, "/t/vtmux-7/rv1"));
        let relative = hold(Some(2), "rel/rv2", Some(Path::new("/w")));
        assert!(owns(&relative, "/w/rel/rv2"));
        assert!(!owns(&relative, "/x/rel/rv2"));
        let unknown = hold(None, "rel/rv3", None);
        assert!(owns(&unknown, "/w/rel/rv3"));
        assert!(owns(&unknown, "/x/rv3"));
        assert!(!owns(&unknown, "/x/rv4"));
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
            // The owner's server, whatever its command line says.
            "140 1 05:00:00 tmux -L rvpath/../default new",
            // Our own pid is never picked.
            "150 1 05:00:00 xtask -L rv3",
        ]);
        let servers = HashSet::from([100, 110, 120, 130, 150]);
        let hour = Duration::from_secs(3600);
        assert_eq!(orphans(&procs, &servers, hour, 150), [100, 101, 102, 110]);
        assert_eq!(
            orphans(&procs, &servers, Duration::ZERO, 150),
            [100, 101, 102, 110, 120]
        );
    }

    /// The reaper must never pick its own ancestors or descendants, even when
    /// they hold a test socket (a harness run from init).
    #[test]
    fn own_tree_is_never_picked() {
        let procs = ps(&[
            "1 0 99:00:00 /sbin/init",
            // Ancestors: an agent under init that holds a test socket.
            "10 1 05:00:00 tmux -L rv1 -f /dev/null new",
            "11 10 05:00:00 bash -c pkill -f rv1",
            "12 11 05:00:00 xtask reap",
            // A descendant that is a server, and its child.
            "13 12 05:00:00 vtmux -L rv2 new",
            "14 13 05:00:00 sleep 5",
            // A real orphan elsewhere.
            "20 1 05:00:00 vtmux -L rv3 new",
            "21 20 05:00:00 sleep 5",
        ]);
        let servers = HashSet::from([10, 13, 20]);
        assert_eq!(orphans(&procs, &servers, Duration::ZERO, 12), [20, 21]);
    }

    /// A scratch directory under `/tmp` (socket paths must stay short),
    /// removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = PathBuf::from(format!("/tmp/vtx-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    use super::test_support::dead_socket;

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn socket_dirs_follow_tmux_tmpdir_then_tmp() {
        let s = Scratch::new("dirs");
        let real = s.0.canonicalize().unwrap();
        for d in ["tmux-7", "vtmux-7", "tmux-8"] {
            std::fs::create_dir(s.0.join(d)).unwrap();
        }
        let got = socket_dirs(Some(&s.0), 7);
        assert_eq!(got, [real.join("tmux-7"), real.join("vtmux-7")]);
        // Empty or missing TMUX_TMPDIR falls back to /tmp alone; /tmp named
        // twice is listed once (the dirs there may or may not exist).
        let tmp = socket_dirs(None, 7);
        assert_eq!(socket_dirs(Some(Path::new("")), 7), tmp);
        assert_eq!(socket_dirs(Some(&s.0.join("missing")), 7), tmp);
        assert_eq!(socket_dirs(Some(Path::new("/tmp")), 7), tmp);
        assert!(tmp.iter().all(|d| d.starts_with("/tmp")), "{tmp:?}");
    }

    /// The entries of checkpoint directory `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        found.sort();
        found
    }

    /// A test-labelled checkpoint directory is emptied (all but its lock)
    /// when its server's socket is dead or missing, unless a server holds
    /// its lock or a socket held open names it; a live socket file,
    /// `default` and other labels keep theirs. A dry run lists the same
    /// and removes nothing; an emptied one is not listed again.
    #[test]
    fn sweep_empties_test_checkpoints_of_gone_servers_only() {
        use std::os::unix::net::UnixListener;
        let s = Scratch::new("ckpt");
        let socks = s.0.join("vtmux-1");
        let root = s.0.join("state");
        std::fs::create_dir_all(&socks).unwrap();
        let checkpoint = |label: &str| {
            let hash = vtmux_server::persist::path_hash(&socks.join(label));
            let dir = root.join(format!("{label}-{hash}"));
            std::fs::create_dir_all(dir.join("chunks")).unwrap();
            std::fs::write(dir.join("chunks/c"), "c").unwrap();
            std::fs::write(dir.join("manifest"), "m").unwrap();
            dir
        };
        dead_socket(&socks.join("cli1"));
        checkpoint("cli1");
        checkpoint("rv2");
        let _live = UnixListener::bind(socks.join("att3-0")).unwrap();
        checkpoint("att3-0");
        let held = checkpoint("testA4");
        let lock = std::fs::File::create(held.join("lock")).unwrap();
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
        dead_socket(&socks.join("default"));
        checkpoint("default");
        checkpoint("mywork");
        std::fs::create_dir_all(root.join("rv5-nothex")).unwrap();
        // A socket held open (the kernel lists it) of a server that does
        // not hold its lock: its file replaced by a dead one.
        dead_socket(&socks.join("rv6"));
        let starting = checkpoint("rv6");
        let in_use = || Some(vec![hold(None, socks.join("rv6"), None)]);
        let want = ["cli1", "rv2"].map(|l| {
            let hash = vtmux_server::persist::path_hash(&socks.join(l));
            root.join(format!("{l}-{hash}"))
        });
        let dirs = [socks.clone()];
        assert_eq!(
            sweep_checkpoints(&root, &dirs, in_use, Duration::ZERO, true),
            want,
            "dry run"
        );
        for d in &want {
            assert_eq!(
                entries(d),
                ["chunks", "manifest"],
                "a dry run changes nothing"
            );
        }
        assert_eq!(
            sweep_checkpoints(&root, &dirs, in_use, Duration::ZERO, false),
            want
        );
        for d in &want {
            assert_eq!(entries(d), ["lock"]);
        }
        assert_eq!(entries(&held), ["chunks", "lock", "manifest"]);
        // Kept for its process: its lock never touched, nor made.
        assert_eq!(entries(&starting), ["chunks", "manifest"]);
        assert_eq!(entries(&root).len(), 8, "no directory goes");
        assert_eq!(
            sweep_checkpoints(&root, &dirs, in_use, Duration::ZERO, false),
            [] as [PathBuf; 0]
        );
        drop(lock);
        // A parallel test's fork between a lock's open and its drop holds
        // the lock too until its child execs (`CLOEXEC` acts at exec, as
        // `dead_socket` says), this test's lock or an earlier sweep's: a
        // sweep keeps the directory until it is free.
        let sweep_until = |in_use: &dyn Fn() -> Option<Vec<Held>>| {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let gone = sweep_checkpoints(&root, &dirs, in_use, Duration::ZERO, false);
                if !gone.is_empty() || std::time::Instant::now() >= deadline {
                    break gone;
                }
                thread::sleep(Duration::from_millis(5));
            }
        };
        assert_eq!(sweep_until(&in_use), [held], "unlocked");
        assert_eq!(
            sweep_until(&|| Some(Vec::new())),
            [starting],
            "its socket gone"
        );
    }

    /// When the held sockets cannot be listed, every directory stays; so
    /// does one changed within `min_age`, until it is older.
    #[test]
    fn a_sweep_keeps_what_it_cannot_tell_and_what_is_recent() {
        let s = Scratch::new("ckpt-doubt");
        let root = s.0.join("state");
        let hash = vtmux_server::persist::path_hash(&s.0.join("rv1"));
        let dir = root.join(format!("rv1-{hash}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest"), "m").unwrap();
        let none = || None;
        assert_eq!(
            sweep_checkpoints(&root, &[], none, Duration::ZERO, false),
            [] as [PathBuf; 0]
        );
        let hour = Duration::from_secs(3600);
        let nothing = || Some(Vec::new());
        assert_eq!(
            sweep_checkpoints(&root, &[], nothing, hour, false),
            [] as [PathBuf; 0]
        );
        assert_eq!(entries(&dir), ["manifest"]);
        let old = std::time::SystemTime::now() - 2 * hour;
        for p in [dir.join("manifest"), dir.clone()] {
            std::fs::File::open(&p).unwrap().set_modified(old).unwrap();
        }
        assert_eq!(
            sweep_checkpoints(&root, &[], nothing, hour, false),
            std::slice::from_ref(&dir)
        );
    }

    /// The sweep holds no lock while it lists the processes, and never
    /// the lock of a directory it keeps for a running label: a server of
    /// that label restoring meanwhile (`Lock::take`, which waits 50 ms)
    /// gets it, however many dead directories the sweep empties.
    #[test]
    fn a_sweep_never_holds_the_lock_of_a_directory_a_process_keeps() {
        use vtmux_server::persist::{LOCK, Lock};
        let s = Scratch::new("ckpt-hold");
        let root = s.0.join("state");
        let dir = |label: &str| {
            let hash = vtmux_server::persist::path_hash(&s.0.join(label));
            let dir = root.join(format!("{label}-{hash}"));
            std::fs::create_dir_all(dir.join("chunks")).unwrap();
            std::fs::write(dir.join("chunks/c"), "c").unwrap();
            std::fs::write(dir.join("manifest"), "m").unwrap();
            // Made, not taken (`a_lock_file_a_server_may_take_is_never_unlinked`).
            std::fs::File::create(dir.join(LOCK)).unwrap();
            dir
        };
        let kept = dir("rv1");
        let dead: Vec<PathBuf> = (2..42).map(|n| dir(&format!("rv{n}"))).collect();
        let in_use = || {
            let restoring = Lock::take(&kept);
            assert!(
                restoring.is_ok(),
                "the sweep holds rv1's lock: {restoring:?}"
            );
            Some(vec![hold(Some(1), s.0.join("rv1"), None)])
        };
        let gone = sweep_checkpoints(&root, &[], in_use, Duration::ZERO, false);
        assert!(!gone.contains(&kept), "{gone:?}");
        assert_eq!(entries(&kept), ["chunks", LOCK, "manifest"]);
        let mut want = dead.clone();
        want.sort();
        assert_eq!(gone, want);
    }

    /// A server between opening its directory's lock file and taking the
    /// lock (`Lock::take`) while the sweep runs: the file it has open stays
    /// the directory's, so the next server is refused while it holds it.
    /// Were the file unlinked, the next would take a new one's lock and
    /// both would write the directory.
    #[test]
    fn a_lock_file_a_server_may_take_is_never_unlinked() {
        use std::os::unix::fs::MetadataExt as _;
        use vtmux_server::persist::{LOCK, Lock};
        let s = Scratch::new("ckpt-lock");
        let root = s.0.join("state");
        let hash = vtmux_server::persist::path_hash(&s.0.join("rv1"));
        let dir = root.join(format!("rv1-{hash}"));
        std::fs::create_dir_all(dir.join("chunks")).unwrap();
        std::fs::write(dir.join("manifest"), "m").unwrap();
        // Made, not taken: a parallel test's fork would hold a taken lock
        // until its child execs (`sweep_empties_test_checkpoints_of_gone_servers_only`).
        let opened = std::fs::File::create(dir.join(LOCK)).unwrap();
        let inode = opened.metadata().unwrap().ino();

        assert_eq!(
            sweep_checkpoints(&root, &[], || Some(Vec::new()), Duration::ZERO, false),
            std::slice::from_ref(&dir)
        );
        assert_eq!(entries(&dir), [LOCK]);
        assert_eq!(std::fs::metadata(dir.join(LOCK)).unwrap().ino(), inode);
        // The sweep's own descriptor may live on a moment in a parallel
        // test's fork, until its child execs.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while rustix::fs::flock(
            &opened,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .is_err()
        {
            assert!(std::time::Instant::now() < deadline, "the lock stays held");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            Lock::take(&dir).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "two servers hold the directory"
        );
    }

    /// A real server, started with `args` (and `$TMUX`, when set) from
    /// directory `cwd` under `TMUX_TMPDIR` `tmpdir`, on `socket` (absolute,
    /// as the server takes it); `test` when that is a test socket.
    struct RealServer {
        args: Vec<std::ffi::OsString>,
        tmpdir: PathBuf,
        tmux: Option<std::ffi::OsString>,
        socket: PathBuf,
        test: bool,
    }

    /// Where [`RealServer`]s run: the binary, their directory (`cwd`) and
    /// `XDG_STATE_HOME`.
    struct Place {
        bin: PathBuf,
        cwd: PathBuf,
        state: PathBuf,
    }

    impl RealServer {
        fn new(
            args: &[&dyn AsRef<std::ffi::OsStr>],
            tmpdir: &Path,
            socket: PathBuf,
            test: bool,
        ) -> Self {
            Self {
                args: args.iter().map(|a| a.as_ref().to_owned()).collect(),
                tmpdir: tmpdir.to_path_buf(),
                tmux: None,
                socket,
                test,
            }
        }

        fn run(&self, at: &Place, cmd: &[&str]) -> std::process::Output {
            let mut command = Command::new(&at.bin);
            command
                .args(&self.args)
                .args(cmd)
                .current_dir(&at.cwd)
                .env("TMUX_TMPDIR", &self.tmpdir)
                .env("XDG_STATE_HOME", &at.state)
                .env_remove("TMUX")
                .stdin(Stdio::null())
                .stderr(Stdio::null());
            if let Some(t) = &self.tmux {
                command.env("TMUX", t);
            }
            command.output().unwrap()
        }

        /// Its checkpoint directory, as the server names it.
        fn dir(&self, at: &Place) -> PathBuf {
            vtmux_server::persist::state_dir(&self.socket, Some(at.state.as_os_str()), None)
                .unwrap()
        }

        fn pid(&self, at: &Place) -> u32 {
            let out = self.run(at, &["display", "-p", "#{pid}"]);
            String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
        }

        /// Start it with a session, write a real checkpoint (at its
        /// `kill-server`), start it again to restore it, turn
        /// `vtmux-persist` off so it lets the lock go, and give it a pane
        /// program.
        fn start_restored(&self, at: &Place) {
            let name = self.socket.display().to_string();
            let ok = |cmd: &[&str]| assert!(self.run(at, cmd).status.success(), "{name}: {cmd:?}");
            ok(&["new", "-d", "-s", "z", "sleep", "1000"]);
            ok(&["kill-server"]);
            until(&format!("{name} checkpoints"), || {
                self.dir(at).join("manifest").exists()
            });
            ok(&["start"]);
            until(&format!("{name} restores"), || {
                self.run(at, &["has", "-t", "z"]).status.success()
            });
            ok(&["set", "-g", "vtmux-persist", "off"]);
            until(&format!("{name} lets its lock go"), || {
                vtmux_server::persist::Lock::try_take(&self.dir(at)).is_ok()
            });
            ok(&["neww", "-d", "sleep", "1000"]);
        }
    }

    /// Poll `ok` for up to 30 s.
    fn until(what: &str, ok: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !ok() {
            assert!(std::time::Instant::now() < deadline, "never: {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Runs its closure when dropped: servers go however a test ends.
    struct Teardown<F: Fn()>(F);
    impl<F: Fn()> Drop for Teardown<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }

    /// Servers on the sockets tmux would take for their command lines
    /// (`main`, tmux.c: `-L` concatenated as given, `make_label`; `$TMUX`
    /// with neither `-L` nor `-S`), the cases a command line misled the
    /// reaper with: `-L rvpath/../default` (`default`'s socket),
    /// `-L rvpath/../<label>`, `-L rvX -f "a b.conf" -L default` (macOS's
    /// `ps` prints it flattened), bundled `-uLrvX`, a socket from `$TMUX`,
    /// a `-S` with spaces, a relative `-S` (from `base`).
    fn misleading_servers(base: &Path, uid: u32, tag: u32) -> Vec<RealServer> {
        use std::os::unix::fs::PermissionsExt as _;
        let (t1, t2) = (base.join("t1"), base.join("t2"));
        let socks = t1.join(format!("vtmux-{uid}"));
        let spaced = base.join(format!("with space/vtmux-{uid}"));
        for d in [
            socks.join(format!("rvp{tag}")),
            t2.clone(),
            spaced.clone(),
            base.join("o"),
            base.join("rel"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::set_permissions(&socks, std::fs::Permissions::from_mode(0o700)).unwrap();
        let conf = base.join("a b.conf");
        std::fs::write(&conf, "set -g vtmux-persist layout\nset -g status off\n").unwrap();
        let (dotted_default, dotted) = (
            format!("rvp{tag}/../default"),
            format!("rvp{tag}/../rvH{tag}"),
        );
        let space_sock = spaced.join(format!("rvF {tag}"));
        let rel_sock = format!("rel/rvG{tag}");
        let mut from_tmux = RealServer::new(
            &[&"-f", &conf],
            &t1,
            base.join(format!("o/rvE{tag}")),
            false,
        );
        let mut tmux = from_tmux.socket.clone().into_os_string();
        tmux.push(",1,0");
        from_tmux.tmux = Some(tmux);
        vec![
            RealServer::new(
                &[&"-f", &conf, &"-L", &format!("rvA{tag}")],
                &t1,
                socks.join(format!("rvA{tag}")),
                true,
            ),
            RealServer::new(
                &[&"-f", &conf, &"-L", &dotted_default],
                &t1,
                socks.join(&dotted_default),
                false,
            ),
            RealServer::new(
                &[&"-f", &conf, &"-L", &dotted],
                &t1,
                socks.join(&dotted),
                true,
            ),
            RealServer::new(
                &[&"-L", &format!("rvC{tag}"), &"-f", &conf, &"-L", &"default"],
                &t2,
                t2.join(format!("vtmux-{uid}/default")),
                false,
            ),
            RealServer::new(
                &[&format!("-uLrvD{tag}"), &"-f", &conf],
                &t1,
                socks.join(format!("rvD{tag}")),
                true,
            ),
            from_tmux,
            RealServer::new(
                &[&"-f", &conf, &"-S", &space_sock],
                &t1,
                space_sock.clone(),
                true,
            ),
            RealServer::new(
                &[&"-f", &conf, &"-S", &rel_sock],
                &t1,
                base.join(&rel_sock),
                false,
            ),
        ]
    }

    /// Sweep `root` until `want` are emptied, and only they.
    fn sweep_until(root: &Path, want: Vec<PathBuf>) {
        let mut want = want;
        want.sort();
        let mut gone = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while gone.len() < want.len() {
            assert!(std::time::Instant::now() < deadline, "kept: {gone:?}");
            gone.extend(sweep_checkpoints(
                root,
                &[],
                held_sockets,
                Duration::ZERO,
                false,
            ));
            thread::sleep(Duration::from_millis(20));
        }
        gone.sort();
        assert_eq!(gone, want);
    }

    /// Real servers on [`misleading_servers`]' sockets. Each writes a real
    /// checkpoint, restores it and turns `vtmux-persist` off, so it holds
    /// no lock: while they run, a sweep empties none of their checkpoints.
    /// The reaper picks exactly the test sockets' servers (and their
    /// panes); killed, those are gone and the others still answer. Then
    /// the gone servers' checkpoints are emptied, and once the rest are
    /// killed theirs too; `default`'s never.
    #[test]
    fn real_servers_are_told_by_the_socket_they_hold() {
        let s = Scratch::new("real");
        let base = s.0.canonicalize().unwrap();
        let at = Place {
            bin: crate::build_vtmux(&crate::oracle::workspace_root()).unwrap(),
            cwd: base.clone(),
            state: base.join("st"),
        };
        let uid = current_uid();
        let servers = misleading_servers(&base, uid, std::process::id());
        let _teardown = Teardown(|| {
            for srv in &servers {
                srv.run(&at, &["kill-server"]);
            }
        });
        for srv in &servers {
            srv.start_restored(&at);
        }
        let root = at.state.join("vtmux");
        let held = held_sockets().expect("the held sockets read");
        assert_eq!(
            sweep_checkpoints(&root, &[], || Some(held), Duration::ZERO, false),
            [] as [PathBuf; 0],
            "a live server's checkpoint went"
        );

        let pids: Vec<u32> = servers.iter().map(|srv| srv.pid(&at)).collect();
        let found = test_servers(&held_sockets().unwrap(), uid, &real_tmp());
        let picked: Vec<u32> = pids.iter().copied().filter(|p| found.contains(p)).collect();
        let want: Vec<u32> = servers
            .iter()
            .zip(&pids)
            .filter(|(srv, _)| srv.test)
            .map(|(_, &p)| p)
            .collect();
        assert_eq!(picked, want, "servers {pids:?}");
        // A subreaper above the test (muzzle, under the gate) may have
        // adopted them instead of init: show them as init's, as a dead
        // agent's leftovers are.
        let mut procs = parse_procs(&ps_full());
        for p in procs.iter_mut().filter(|p| pids.contains(&p.pid)) {
            p.ppid = 1;
        }
        let mut ours: HashSet<u32> = pids.iter().copied().collect();
        descendants(&procs, &mut ours);
        let reaped: Vec<u32> = orphans(&procs, &found, Duration::ZERO, std::process::id())
            .into_iter()
            .filter(|p| ours.contains(p))
            .collect();
        let mut panes: HashSet<u32> = want.iter().copied().collect();
        descendants(&procs, &mut panes);
        let mut panes: Vec<u32> = panes.into_iter().collect();
        panes.sort_unstable();
        assert_eq!(reaped, panes);
        assert!(reaped.len() > want.len(), "no pane picked: {reaped:?}");
        kill_pids(&reaped);
        for srv in &servers {
            let alive = srv.run(&at, &["has", "-t", "z"]).status.success();
            assert_eq!(alive, !srv.test, "{}", srv.socket.display());
        }

        // The killed servers' checkpoints go; the others' stay while they
        // run, and go once they are killed too. `default`'s never.
        let tested = |test: bool| {
            servers
                .iter()
                .filter(|srv| srv.test == test)
                .map(|srv| srv.dir(&at))
                .filter(|d| d.file_name().unwrap().to_str().unwrap().starts_with("rv"))
                .collect::<Vec<_>>()
        };
        sweep_until(&root, tested(true));
        for srv in servers.iter().filter(|srv| !srv.test) {
            assert!(srv.run(&at, &["kill-server"]).status.success());
        }
        sweep_until(&root, tested(false));
        for srv in servers.iter().filter(|srv| srv.socket.ends_with("default")) {
            assert!(srv.dir(&at).join("manifest").exists(), "{:?}", srv.dir(&at));
        }
    }

    #[test]
    fn sweep_removes_dead_test_sockets_only() {
        let s = Scratch::new("sweep");
        let dir = s.0.join("tmux-1");
        std::fs::create_dir(&dir).unwrap();
        for dead in [
            "att99-0",
            "att99-1",
            "cli7",
            "ctl7-0",
            "i7",
            "rv1",
            "rvchk9",
            "testA5",
            "covers-x-1",
            "default",
            "work",
            "cliff",
        ] {
            dead_socket(&dir.join(dead));
        }
        // A server that answers, one that is not a socket at all, a symlink.
        let live = std::os::unix::net::UnixListener::bind(dir.join("att99-live")).unwrap();
        std::fs::write(dir.join("att99-file"), "").unwrap();
        std::os::unix::fs::symlink("att99-live", dir.join("att99-link")).unwrap();

        let dirs = [dir.clone()];
        let would = sweep_sockets(&dirs, |_| true, true);
        assert_eq!(would.len(), 9, "{would:?}");
        assert!(would.iter().all(|p| p.exists()), "dry run removed");

        let mut gone = names(&sweep_sockets(&dirs, |_| true, false));
        gone.sort();
        assert_eq!(
            gone,
            [
                "att99-0",
                "att99-1",
                "cli7",
                "covers-x-1",
                "ctl7-0",
                "i7",
                "rv1",
                "rvchk9",
                "testA5"
            ]
        );
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "att99-file",
                "att99-link",
                "att99-live",
                "cliff",
                "default",
                "work"
            ]
        );
        drop(live);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sweep_leaves_a_full_listen_queue_alone_without_blocking() {
        let s = Scratch::new("full-queue");
        let socket = s.0.join("rvfull");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        rustix::net::listen(&listener, 1).unwrap();
        let mut clients = Vec::new();
        for _ in 0..2 {
            clients.push(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        }
        let dir = s.0.clone();
        let (send, recv) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            send.send(sweep_sockets(&[dir], |_| true, false)).unwrap();
        });
        let result = recv.recv_timeout(Duration::from_secs(2));
        // Unblock the old implementation before asserting, so even a red
        // test leaves no worker or socket behind.
        drop(listener);
        worker.join().unwrap();
        assert!(result.is_ok(), "socket sweep blocked on a full queue");
        assert_eq!(result.unwrap(), [] as [PathBuf; 0]);
        assert!(socket.exists(), "live socket was removed");
    }

    #[test]
    fn sweep_honours_the_extra_filter() {
        let s = Scratch::new("filter");
        let dir = s.0.join("vtmux-1");
        std::fs::create_dir(&dir).unwrap();
        for n in ["testA10", "testA10-2", "testA11"] {
            dead_socket(&dir.join(n));
        }
        let got = sweep_sockets(
            std::slice::from_ref(&dir),
            |n| n.starts_with("testA10"),
            false,
        );
        assert_eq!(names(&got), ["testA10", "testA10-2"]);
        assert!(dir.join("testA11").exists());
    }

    #[test]
    fn listing_shows_test_labels_only() {
        let s = Scratch::new("list");
        let dir = s.0.join("tmux-1");
        std::fs::create_dir(&dir).unwrap();
        for n in ["att1-0", "default", "rv2", "other"] {
            dead_socket(&dir.join(n));
        }
        assert_eq!(names(&test_sockets(&[dir])), ["att1-0", "rv2"]);
    }
}
