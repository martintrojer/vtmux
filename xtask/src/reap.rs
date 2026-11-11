//! Teardown for test servers. A server daemonizes (setsid), so it leaves the
//! process group of whatever started it; a failing or timed-out test never
//! reaches its own `kill-server`. Harnesses call this after every run (pass,
//! fail, timeout or panic) so no server outlives it and holds its PTYs.
//!
//! `cargo xtask reap` is the other half: servers nobody's harness owns, left by
//! ad-hoc probe scripts (`-L rv*`, `att*`, ...) that never ran `kill-server`.
//! It kills test-labelled `tmux`/`vtmux` servers that are old and orphaned
//! (a server a live upgrade moved onto a binary of another name too: by its
//! `-S <test socket> --vtmux-resume`), with their pane processes (a flood loop in a pane outlives the server's
//! death otherwise). `check.sh` runs it first.
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
//! ones whose server is gone: its socket in those directories missing or
//! refusing, no server holding the directory's `lock` (the sweep holds it
//! while it removes), and no running process naming the label (a server
//! before it takes the lock, or one that does not hold it). The `lock`
//! file itself stays, for a server that may have it open.

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
const DEFAULT_MIN_AGE: Duration = Duration::from_hours(1);

/// One row of `ps -o pid=,ppid=,etime=,args=`, with its arguments as
/// [`Argv`].
#[derive(Debug, PartialEq, Eq)]
struct Proc {
    pid: u32,
    ppid: u32,
    age: Duration,
    args: String,
    argv: Argv,
}

/// A process's arguments. Exact on Linux: `/proc/<pid>/cmdline`, one word
/// per NUL-ended argument, so a value with a space in it (`-S "/a b/rv1"`,
/// `-f "a b.conf"`) stays one word. Elsewhere (macOS, whose `ps` prints
/// the arguments flattened, or a process gone before its `cmdline` was
/// read) `ps`'s line split at whitespace, which may cut a value in two:
/// such a line is `loose`, and a checkpoint any loose line could name is
/// kept ([`InUse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Argv {
    words: Vec<String>,
    loose: bool,
}

impl Argv {
    /// `ps`'s flattened line, split at whitespace.
    fn split(line: &str) -> Self {
        Self {
            words: line.split_whitespace().map(str::to_owned).collect(),
            loose: true,
        }
    }

    /// The exact arguments of `pid` (Linux), if they read. A process that
    /// rewrote its arguments into one string with spaces (a
    /// `setproctitle`) reads as that string split, loose.
    #[cfg(target_os = "linux")]
    fn of(pid: u32) -> Option<Self> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let raw = raw.strip_suffix(b"\0").unwrap_or(&raw);
        if raw.is_empty() {
            return None;
        }
        let words: Vec<String> = raw
            .split(|&b| b == 0)
            .map(|w| String::from_utf8_lossy(w).into_owned())
            .collect();
        if let [one] = words.as_slice()
            && one.contains(char::is_whitespace)
        {
            return Some(Self::split(one));
        }
        Some(Self {
            words,
            loose: false,
        })
    }

    /// No exact arguments off Linux.
    #[cfg(not(target_os = "linux"))]
    fn of(_pid: u32) -> Option<Self> {
        None
    }
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
            let args = rest.trim().to_string();
            Some(Proc {
                pid,
                ppid: parent,
                age,
                argv: Argv::split(&args),
                args,
            })
        })
        .collect()
}

/// Every process, with its exact arguments where they read
/// ([`Argv::of`]).
fn procs() -> Vec<Proc> {
    let mut procs = parse_procs(&ps_full());
    for p in &mut procs {
        if let Some(argv) = Argv::of(p.pid) {
            p.argv = argv;
        }
    }
    procs
}

/// The arguments of every process but `skip`'s: on Linux read from
/// `/proc` alone (fast, so a sweep holding checkpoint locks holds them a
/// short while), else from `ps`.
fn all_argv(skip: &[u32]) -> Vec<Argv> {
    #[cfg(target_os = "linux")]
    if let Ok(entries) = std::fs::read_dir("/proc") {
        return entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| !skip.contains(pid))
            .filter_map(Argv::of)
            .collect();
    }
    procs()
        .into_iter()
        .filter(|p| !skip.contains(&p.pid))
        .map(|p| p.argv)
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

/// Is this a `tmux`/`vtmux` command line whose socket is a test one: a
/// global `-L` label from [`is_test_label`], an oracle run's
/// `-S /tmp/vto-*/s`, or `-S` a test label's socket
/// (`.../tmux-<uid>/<label>` or `vtmux-<uid>`). The socket is the one
/// tmux takes ([`server_socket`]): `-uL rv1 -L default` is `default`'s
/// server, not a test one.
///
/// A server a live upgrade re-executed runs as `<binary> -S <its socket>
/// --vtmux-resume <fd>` with `argv[0]` the binary it was upgraded onto, so
/// whatever that is called (`vtmux-new`, a copy, `old`), a command line with
/// `--vtmux-resume` and a test `-S` is a test server. Any other `argv0` that
/// is not `tmux` or `vtmux` is not. An image of a binary from before the `-S`
/// form is started as `<binary> --vtmux-resume <fd>` alone: nothing on its
/// command line says whose it is, so it is not matched at any `argv0`.
fn is_test_server(argv: &Argv) -> bool {
    let Some((named, flags)) = socket_flags(&argv.words, false) else {
        return false;
    };
    match server_socket(&flags) {
        Socket::Label(l) => named && is_test_label(l),
        Socket::Path(p) => p.starts_with("/tmp/vto-") || is_label_socket(p),
    }
}

/// The socket a command line's global flags name.
#[derive(Debug, PartialEq, Eq)]
enum Socket<'a> {
    /// `-L`, or none (`default`).
    Label(&'a str),
    /// `-S`.
    Path(&'a str),
}

/// The socket tmux takes from global flags `flags`: the last `-S`, else
/// the last `-L`, else `default` (`main`, tmux.c: each `-L` and `-S`
/// replaces the one before, and `-S` wins over `-L`). `$TMUX` is not
/// looked at: a server's own is its starter's, which may be another
/// server's.
fn server_socket<'a>(flags: &[(char, &'a str)]) -> Socket<'a> {
    let last = |f| flags.iter().rev().find(|&&(g, _)| g == f).map(|&(_, v)| v);
    last('S').map_or_else(
        || Socket::Label(last('L').unwrap_or("default")),
        Socket::Path,
    )
}

/// The global `-L` and `-S` values of command line `words`, in order, with
/// whether `argv0` is `tmux` or `vtmux`. `None` for a command line of
/// another program, unless it has `--vtmux-resume` (a server a live upgrade
/// re-executed, whatever its `argv0`) or `any` is set. Global flags only,
/// read as tmux's `getopt` does (`main`, tmux.c:
/// `"2c:CDdf:hlL:NqS:T:uUvV"`): bundled (`-uL rv1`, `-dLrv1`), a value
/// attached or the next word, up to `--` or the command.
/// `--vtmux-resume <fd>` is skipped with its value.
fn socket_flags(words: &[String], any: bool) -> Option<(bool, Vec<(char, &str)>)> {
    let mut words = words.iter().map(String::as_str);
    let argv0 = words.next()?;
    let named = matches!(argv0.rsplit('/').next(), Some("tmux" | "vtmux"));
    if !named && !any && !words.clone().any(|w| w == "--vtmux-resume") {
        return None;
    }
    let mut found = Vec::new();
    while let Some(w) = words.next() {
        if w == "--" {
            break;
        }
        if let Some(long) = w.strip_prefix("--") {
            if long == "vtmux-resume" {
                words.next();
            }
            continue;
        }
        let Some(bundle) = w.strip_prefix('-').filter(|b| !b.is_empty()) else {
            break;
        };
        for (i, f) in bundle.char_indices() {
            if !matches!(f, 'c' | 'f' | 'L' | 'S' | 'T') {
                continue;
            }
            // The rest of the word, else the next one, is the value.
            let rest = &bundle[i + f.len_utf8()..];
            let value = if rest.is_empty() {
                words.next()
            } else {
                Some(rest)
            };
            if let Some(v) = value
                && matches!(f, 'L' | 'S')
            {
                found.push((f, v));
            }
            break;
        }
    }
    Some((named, found))
}

/// The test labels running processes may be using: a server of one may be
/// about to take its checkpoint directory's lock (at its restore, or at
/// its first checkpoint), or not hold it at all (`vtmux-persist` off, a
/// refused restore, the moment after a live upgrade).
///
/// Errs on keeping. Every `-L` and `-S` socket name of an exact command
/// line counts, not only the one tmux takes, whatever its `argv0` (a
/// binary run under another name, a copy). A loose one ([`Argv`]) may have
/// a value cut at a space, so it keeps every label it holds anywhere as
/// text.
///
/// A server a live upgrade re-executed from a binary before the `-S` form
/// runs as `<binary> --vtmux-resume <fd>` alone, which names no label
/// ([`is_test_server`]): its directory is kept by the other checks of
/// [`sweep_checkpoints`], its socket answering (in the default socket
/// directories) or its lock, which it takes again at its first checkpoint
/// after the exec.
#[derive(Debug, Default)]
pub struct InUse {
    labels: HashSet<String>,
    loose: Vec<String>,
}

impl InUse {
    fn of<'a>(argvs: impl IntoIterator<Item = &'a Argv>) -> Self {
        let mut in_use = Self::default();
        for argv in argvs {
            if argv.loose {
                in_use.loose.push(argv.words.join(" "));
            }
            let Some((_, flags)) = socket_flags(&argv.words, true) else {
                continue;
            };
            in_use.labels.extend(
                flags
                    .into_iter()
                    .filter_map(|(flag, value)| match flag {
                        'L' => Some(value),
                        _ => Path::new(value).file_name()?.to_str(),
                    })
                    .filter(|l| is_test_label(l))
                    .map(str::to_owned),
            );
        }
        in_use
    }

    /// Whether a server of `label` may be running.
    fn contains(&self, label: &str) -> bool {
        self.labels.contains(label) || self.loose.iter().any(|l| l.contains(label))
    }
}

/// `path` is `<dir>/tmux-<uid>/<label>` or `<dir>/vtmux-<uid>/<label>` with a
/// test label: where `-L <label>` puts the socket.
fn is_label_socket(path: &str) -> bool {
    let p = Path::new(path);
    let dir = p
        .parent()
        .and_then(Path::file_name)
        .and_then(|d| d.to_str());
    let label = p.file_name().and_then(|l| l.to_str());
    let socket_dir = dir.is_some_and(|d| {
        d.strip_prefix("tmux-")
            .or_else(|| d.strip_prefix("vtmux-"))
            .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()))
    });
    socket_dir && label.is_some_and(is_test_label)
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
                && is_test_server(&p.argv)
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

fn current_uid() -> Option<u32> {
    let out = Command::new("id")
        .arg("-u")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// [`socket_dirs`] for this user and environment.
pub fn default_socket_dirs() -> Vec<PathBuf> {
    let tmpdir = std::env::var_os("TMUX_TMPDIR").map(PathBuf::from);
    current_uid().map_or_else(Vec::new, |uid| socket_dirs(tmpdir.as_deref(), uid))
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

/// Empty the checkpoint directories in `root` (`<label>-<hash>`) whose
/// label is a test one ([`is_test_label`]) and whose server is gone, as
/// far as can be told; when in doubt, a directory stays.
///
/// A directory is a candidate when no socket `<dir>/<label>` of
/// `socket_dirs` that hashes to `<hash>` answers a `connect()`. Its lock is
/// then taken, exclusive and without waiting ([`take_checkpoint_lock`]):
/// one a server holds is kept. With every candidate's lock held, `in_use`
/// lists the processes running ([`InUse`]); a label any of them names is
/// kept (a server starting, before its restore takes the lock; one with
/// its socket elsewhere, or that does not hold its lock). Only then are
/// the rest emptied, all but the lock file ([`empty_checkpoint`]), and the
/// locks let go. A server that starts while they are held finds its lock
/// taken until the sweep is done, and no checkpoint after it: it can
/// never read one half removed.
///
/// `default`, other labels, live servers and anything else stay. Returns
/// the directories emptied (one already empty is not listed again), or
/// with `dry` the ones that would be.
pub fn sweep_checkpoints(
    root: &Path,
    socket_dirs: &[PathBuf],
    in_use: impl FnOnce() -> InUse,
    dry: bool,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let held: Vec<(PathBuf, String, Taken)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (label, hash) = name.rsplit_once('-')?;
            let hashed = hash.len() == 12 && hash.bytes().all(|b| b.is_ascii_hexdigit());
            (hashed && is_test_label(label)).then(|| (e.path(), label.to_owned(), hash.to_owned()))
        })
        .filter(|(_, label, hash)| {
            !socket_dirs
                .iter()
                .map(|d| d.join(label))
                .filter(|p| vtmux_server::persist::path_hash(p) == *hash)
                .any(|p| {
                    p.symlink_metadata()
                        .is_ok_and(|m| m.file_type().is_socket())
                        && !matches!(probe_connect(&p), Err(rustix::io::Errno::CONNREFUSED))
                })
        })
        .filter_map(|(dir, label, _)| {
            let lock = take_checkpoint_lock(&dir, dry)?;
            Some((dir, label, lock))
        })
        .collect();
    if held.is_empty() {
        return Vec::new();
    }
    let in_use = in_use();
    let mut gone: Vec<PathBuf> = held
        .iter()
        .filter(|(dir, label, _)| !in_use.contains(label) && empty_checkpoint(dir, dry))
        .map(|(dir, _, _)| dir.clone())
        .collect();
    drop(held);
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
    let procs = procs();
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
    // The processes running once the kill and the socket sweep are done
    // and the checkpoint locks are held (a server started meanwhile too),
    // but those just killed, which may linger a moment.
    let in_use = || InUse::of(&all_argv(&pids));
    let checkpoints = default_state_root()
        .map(|root| sweep_checkpoints(&root, &default_socket_dirs(), in_use, dry))
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

    /// `line` as exact arguments, one word each (`/proc/<pid>/cmdline`).
    fn exact(line: &str) -> Argv {
        Argv {
            loose: false,
            ..Argv::split(line)
        }
    }

    fn in_use_of(lines: &[&str]) -> InUse {
        InUse::of(&lines.iter().map(|l| exact(l)).collect::<Vec<_>>())
    }

    fn labels(in_use: &InUse) -> Vec<&str> {
        let mut found: Vec<&str> = in_use.labels.iter().map(String::as_str).collect();
        found.sort_unstable();
        found
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
            "vtmux -L ctl7-0 -f /dev/null -C new",
            "tmux -2 -L i42 new",
            "tmux -uL rv2 new",
            "vtmux -2dS/tmp/vto-a1b2c3/s new",
            "tmux -S /tmp/vto-a1b2c3/s -f /dev/null new",
            "tmux -S /tmp/vto-a1b2c3/s --vtmux-resume 9",
            "/w/vtmux -S /tmp/t1/vtmux-1000/rv1 --vtmux-resume 9 --vtmux-switched-back",
            "tmux -S /tmp/t1/tmux-1000/testA42 --vtmux-resume 9",
            // Upgraded onto a binary of another name (argv[0] is its path).
            "/x/vtmux-copy -S /tmp/t1/vtmux-1000/rv1 --vtmux-resume 9",
            "/h/old -S /tmp/vto-a1b2c3/s --vtmux-resume 9 --vtmux-switched-back",
            "vtmux-new -S /tmp/t1/vtmux-1000/testA42 --vtmux-resume 9",
            // The last `-L` is the one, and `-S` wins over `-L`.
            "tmux -L default -uL rv1 new",
            "vtmux -L work -L rv2",
            "vtmux -L work -S /tmp/vto-a1b2c3/s new",
            "vtmux -S /run/user/1/sock -S /tmp/vto-a1b2c3/s new",
        ] {
            assert!(is_test_server(&exact(yes)), "{yes}");
        }
        for no in [
            "tmux new-session -d -s dotfiles",
            "tmux -L work new",
            "tmux -L cliff new",
            "tmux -f /dev/null -L other new",
            "vtmux new -s rv1 -L rv1",
            "tmux -S /run/user/1/sock new",
            "tmux -S /tmp/t1/vtmux-1000/default --vtmux-resume 9",
            "tmux -S /tmp/t1/vtmux-1000/work --vtmux-resume 9",
            "tmux -S /tmp/t1/x/rv1 --vtmux-resume 9",
            "tmux --vtmux-resume 9",
            "/x/vtmux-copy --vtmux-resume 9",
            "/x/vtmux-copy -S /tmp/t1/vtmux-1000/work --vtmux-resume 9",
            "/x/vtmux-copy -S /tmp/t1/vtmux-1000/rv1 new",
            "/x/vtmux-copy -L rv1 --vtmux-resume 9",
            "/x/vtmux-copy -S /run/user/1/sock --vtmux-resume 9",
            "/usr/bin/python3 /tmp/rv_1/x.py -Lrv1",
            "tmuxer -Lrv1 x",
            "tmux -uL rvx -L default new",
            "vtmux -uL rvbundled -L work new",
            "vtmux -Lrv1 -Ldefault",
            "vtmux -L rv1 -S /run/user/1/sock new",
            "vtmux -S /tmp/vto-a1b2c3/s -L rv1 -S /run/x new",
        ] {
            assert!(!is_test_server(&exact(no)), "{no}");
        }
    }

    #[test]
    fn labels_in_use_are_the_test_sockets_named_on_command_lines() {
        let found = in_use_of(&[
            "/w/vtmux -L rv1 -f /dev/null new -d",
            "tmux -LtestA2 new",
            "/x/vtmux-copy -S /tmp/t1/vtmux-1000/cli3 --vtmux-resume 9",
            "vtmux -S /tmp/p/att4-0 new",
            "tmux -L work new",
            "vtmux new -s rv5 -L rv5",
            "/usr/bin/python3 x.py -L rv6",
            // Any `argv0`: a binary run under another name.
            "/x/vtmux-dev -L rv7 new",
            // Every label named, not only the one tmux takes.
            "tmux -L rv8 -L default new",
        ]);
        assert_eq!(
            labels(&found),
            ["att4-0", "cli3", "rv1", "rv7", "rv8", "testA2"]
        );
    }

    /// An exact command line keeps a value with a space in it whole; a
    /// loose one (macOS's `ps`) may have it cut in two, so every label it
    /// holds as text is in use.
    #[test]
    fn labels_in_use_survive_spaces_in_values() {
        let words = |w: &[&str]| w.iter().map(|&w| w.to_owned()).collect();
        let exact = Argv {
            words: words(&["vtmux", "-f", "/tmp/a b.conf", "-L", "rv1", "new"]),
            loose: false,
        };
        let path = Argv {
            words: words(&["vtmux", "-S", "/tmp/with space/rv2", "new"]),
            loose: false,
        };
        let found = InUse::of(&[exact, path]);
        assert_eq!(labels(&found), ["rv1", "rv2"]);
        let loose = InUse::of(&[Argv::split("vtmux -f /tmp/a b.conf -L rv3 new")]);
        assert_eq!(labels(&loose), [] as [&str; 0]);
        assert!(loose.contains("rv3"));
        assert!(!loose.contains("rv4"));
    }

    /// Global flags as tmux's `getopt` reads them: bundled, a value
    /// attached or the next word, up to `--` or the first non-option.
    #[test]
    fn labels_in_use_read_bundled_flags_as_getopt_does() {
        let found = in_use_of(&[
            "tmux -uL rv1 new",
            "/w/vtmux -2dLrv2",
            "vtmux -vS /tmp/t/tmux-1/cli3 ls",
            "vtmux -S /tmp/t/vtmux-1/ctl4 --vtmux-resume 3 --vtmux-switched-back",
            "vtmux -uf /dev/null -L rv5 new",
            "vtmux -f -L rv6 new",
            "vtmux -- -L rv7",
            "vtmux - -L rv8",
            "vtmux -Tfeatures -L rv9",
        ]);
        assert_eq!(labels(&found), ["cli3", "ctl4", "rv1", "rv2", "rv5", "rv9"]);
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
            // A later `-L` names the owner's servers (tmux.c `main`).
            "160 1 05:00:00 tmux -uL rvx -L default new",
            "161 1 05:00:00 vtmux -uL rvbundled -L work new",
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
    /// the subreaper adopts it) is reaped, a live child of ours is not, nor
    /// is an orphan whose later `-L` names the owner's `default` server.
    #[test]
    fn real_orphan_is_reaped_and_live_server_is_not() {
        let tag = format!("rvunit{}", std::process::id());
        let fake = |label: &str| format!("exec -a \"tmux -L{label} -f/dev/null new\" sleep 1000");
        let orphan = format!("{tag}o");
        let live = format!("{tag}l");
        let owners = format!("{tag}d");
        Command::new("sh")
            .args(["-c", &format!("(bash -c '{}' &)", fake(&orphan))])
            .status()
            .unwrap();
        let owners_fake = format!("exec -a \"tmux -uL{owners} -L default new\" sleep 1000");
        Command::new("sh")
            .args(["-c", &format!("(bash -c '{owners_fake}' &)")])
            .status()
            .unwrap();
        let mut child = Command::new("bash")
            .args(["-c", &fake(&live)])
            .spawn()
            .unwrap();
        let mine = |args: &str| args.contains(&tag);
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
            for p in procs
                .iter_mut()
                .filter(|p| p.args.contains(&orphan) || p.args.contains(&owners))
            {
                p.ppid = 1;
            }
            let ready = procs
                .iter()
                .any(|p| p.args.starts_with(&format!("tmux -uL{owners}")));
            let got = picked(&procs);
            if (ready && !got.is_empty()) || std::time::Instant::now() >= deadline {
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
            .filter(|p| p.args.contains(&orphan) || p.args.contains(&owners))
            .map(|p| p.pid)
            .collect();
        kill_pids(&pids);
        let after = parse_procs(&ps_full());
        assert!(!after.iter().any(|p| p.args.contains(&orphan)));
        assert!(after.iter().any(|p| p.args.contains(&live)));
        child.kill().unwrap();
        child.wait().unwrap();
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
    /// its lock or a running process names its label; a live socket,
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
        // A server starting (or with its socket elsewhere) that has not
        // taken the lock yet.
        dead_socket(&socks.join("rv6"));
        let starting = checkpoint("rv6");
        let in_use = || in_use_of(&["/w/vtmux -L rv6 -f /dev/null new -d"]);
        let want = ["cli1", "rv2"].map(|l| {
            let hash = vtmux_server::persist::path_hash(&socks.join(l));
            root.join(format!("{l}-{hash}"))
        });
        let dirs = [socks.clone()];
        assert_eq!(
            sweep_checkpoints(&root, &dirs, in_use, true),
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
        assert_eq!(sweep_checkpoints(&root, &dirs, in_use, false), want);
        for d in &want {
            assert_eq!(entries(d), ["lock"]);
        }
        assert_eq!(entries(&held), ["chunks", "lock", "manifest"]);
        assert_eq!(entries(&starting), ["chunks", "lock", "manifest"]);
        assert_eq!(entries(&root).len(), 8, "no directory goes");
        assert_eq!(
            sweep_checkpoints(&root, &dirs, in_use, false),
            [] as [PathBuf; 0]
        );
        drop(lock);
        // A parallel test's fork between a lock's open and its drop holds
        // the lock too until its child execs (`CLOEXEC` acts at exec, as
        // `dead_socket` says), this test's lock or an earlier sweep's: a
        // sweep keeps the directory until it is free.
        let sweep_until = |in_use: &dyn Fn() -> InUse| {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let gone = sweep_checkpoints(&root, &dirs, in_use, false);
                if !gone.is_empty() || std::time::Instant::now() >= deadline {
                    break gone;
                }
                thread::sleep(Duration::from_millis(5));
            }
        };
        assert_eq!(sweep_until(&in_use), [held], "unlocked");
        assert_eq!(sweep_until(&InUse::default), [starting], "its process gone");
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
            sweep_checkpoints(&root, &[], InUse::default, false),
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

    /// End to end: a live server that does not hold its checkpoint's lock
    /// (`vtmux-persist` off: a refused restore, or the moment after a live
    /// upgrade, look the same), with a space in a value on its command
    /// line, keeps its checkpoint, whether its socket is a custom `-S`
    /// path or in a `TMUX_TMPDIR` the sweep does not look in. Once the
    /// servers are gone, their checkpoints go too.
    #[test]
    fn a_live_server_with_spaces_in_its_arguments_keeps_its_checkpoint() {
        /// Runs its closure when dropped: the servers go however the test ends.
        struct Servers<F: Fn()>(F);
        impl<F: Fn()> Drop for Servers<F> {
            fn drop(&mut self) {
                (self.0)();
            }
        }
        let bin = crate::build_vtmux(&crate::oracle::workspace_root()).unwrap();
        let s = Scratch::new("ckpt-space");
        let state = s.0.join("st");
        let tmpdir = s.0.join("t");
        let spaced = s.0.join("with space");
        std::fs::create_dir_all(&tmpdir).unwrap();
        std::fs::create_dir_all(&spaced).unwrap();
        let conf = s.0.join("a b.conf");
        std::fs::write(&conf, "set -g status off\n").unwrap();
        let tag = std::process::id();
        let custom = spaced.join(format!("rvsp{tag}"));
        let label = format!("rvsf{tag}");
        let vtmux = |args: &[&std::ffi::OsStr]| {
            Command::new(&bin)
                .args(args)
                .env("TMUX_TMPDIR", &tmpdir)
                .env("XDG_STATE_HOME", &state)
                .env_remove("TMUX")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
        };
        let custom_args = |cmd: &[&str]| {
            let mut a: Vec<std::ffi::OsString> = vec!["-S".into(), custom.clone().into()];
            a.extend(cmd.iter().map(Into::into));
            a
        };
        let label_args = |cmd: &[&str]| {
            let mut a: Vec<std::ffi::OsString> = vec![
                "-f".into(),
                conf.clone().into(),
                "-L".into(),
                label.clone().into(),
            ];
            a.extend(cmd.iter().map(Into::into));
            a
        };
        let run = |a: Vec<std::ffi::OsString>| {
            vtmux(
                &a.iter()
                    .map(std::ffi::OsString::as_os_str)
                    .collect::<Vec<_>>(),
            )
        };
        let _servers = Servers(|| {
            run(custom_args(&["kill-server"]));
            run(label_args(&["kill-server"]));
        });
        let new = ["new", "-d", "sleep", "1000"];
        assert!(run(custom_args(&new)).success());
        assert!(run(label_args(&new)).success());
        let uid = rustix::process::getuid().as_raw();
        let socket = tmpdir
            .canonicalize()
            .unwrap()
            .join(format!("vtmux-{uid}"))
            .join(&label);
        let root = state.join("vtmux");
        let checkpoints = [custom.clone(), socket].map(|sock| {
            let dir =
                vtmux_server::persist::state_dir(&sock, Some(state.as_os_str()), None).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("manifest"), "m").unwrap();
            dir
        });
        let in_use = || InUse::of(&all_argv(&[]));
        assert_eq!(
            sweep_checkpoints(&root, &[], in_use, false),
            [] as [PathBuf; 0],
            "a live server's checkpoint went"
        );
        for d in &checkpoints {
            assert!(d.join("manifest").exists(), "{d:?}");
        }

        run(custom_args(&["kill-server"]));
        run(label_args(&["kill-server"]));
        let mut want = checkpoints.to_vec();
        want.sort();
        let mut gone = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while gone.len() < 2 {
            assert!(std::time::Instant::now() < deadline, "kept: {gone:?}");
            gone.extend(sweep_checkpoints(&root, &[], in_use, false));
            thread::sleep(Duration::from_millis(20));
        }
        gone.sort();
        assert_eq!(gone, want);
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
