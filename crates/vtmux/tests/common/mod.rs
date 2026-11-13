//! Test teardown shared by `cli`, `attach` and `control`: a test world must
//! not leave a server behind, whether the test passed, failed or panicked.
//!
//! `kill-server` is not enough on its own: if the server is wedged, or the
//! client call fails, the daemonized server (setsid) outlives the test and
//! keeps its PTYs, which exhausts the host's PTY pool for every other run.
//! So a world records the server pid first and `reap` verifies the death.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process, test_kill_process};

/// The server pid out of `display -p '#{pid}'` output.
pub fn parse_pid(stdout: &str) -> Option<Pid> {
    Pid::from_raw(stdout.trim().parse().ok()?)
}

/// After `kill-server`: wait briefly for `pid` to exit, then SIGKILL it.
pub fn reap(pid: Option<Pid>) {
    let Some(pid) = pid else { return };
    let end = Instant::now() + Duration::from_secs(2);
    while test_kill_process(pid).is_ok() {
        if Instant::now() >= end {
            let _ = kill_process(pid, Signal::KILL);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What `cmd` prints, or `None` when it has not finished within `limit` (it
/// is killed): for a server that may not answer. Teardown must not wait on
/// a server that is stopped or wedged. Finished means its output closed
/// too: a process that keeps the command's stdout or stderr open after it
/// exits (a daemon it started, a server it passed them to) makes `None`
/// at `limit`, not a read that waits for ever.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn output_within(cmd: &mut Command, limit: Duration) -> Option<Output> {
    use std::io::Read;
    use std::thread::JoinHandle;

    fn drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut all = Vec::new();
            let _ = pipe.read_to_end(&mut all);
            all
        })
    }

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = drain(child.stdout.take()?);
    let stderr = drain(child.stderr.take()?);
    let end = Instant::now() + limit;
    let mut status = None;
    // A reader left blocked on a held pipe is abandoned: it ends with the
    // holder, or with the test binary.
    while status.is_none() || !stdout.is_finished() || !stderr.is_finished() {
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        if status.is_none() {
            status = child.try_wait().ok()?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(Output {
        status: status?,
        stdout: stdout.join().ok()?,
        stderr: stderr.join().ok()?,
    })
}

/// How long teardown waits for each command to the server.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub const TEARDOWN_LIMIT: Duration = Duration::from_secs(3);

/// The pids of the processes of the world `-L label` with `TMUX_TMPDIR`
/// `tmux_tmpdir`: its server and clients, found by their command line and
/// environment (`ps axeww`, which prints both on Linux and macOS). A
/// stopped server answers nothing, and its socket does not name its pid.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn world_pids(tmux_tmpdir: &Path, label: &str) -> Vec<Pid> {
    let Ok(ps) = Command::new("ps")
        .args(["axeww", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    let label = format!(" -L {label} ");
    let tmpdir = format!("TMUX_TMPDIR={}", tmux_tmpdir.display());
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| l.contains(&label) && l.contains(&tmpdir))
        .filter_map(|l| parse_pid(l.split_whitespace().next()?))
        .collect()
}

/// Tear a world's server down without ever waiting on it. `command` builds
/// `<binary> -L .. args` for the world; `known` is the server pid the world
/// recorded. The pid comes from `display`, else `known`; a server that
/// answers `display` is told `kill-server` and reaped, one that does not
/// (stopped, wedged) is killed with every other process of the world.
///
/// Then every process still running with the world's `TMUX_TMPDIR` in its
/// environment (a server on another socket, `-S` in the world's dirs, a
/// client, a pane program) gets a second to go and is killed: a world
/// leaves nothing running, pass or fail. What had to be killed is
/// returned, each process as its pid and command line.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn teardown(
    tmux_tmpdir: &Path,
    label: &str,
    known: Option<Pid>,
    mut command: impl FnMut(&[&str]) -> Command,
) -> Vec<(Pid, String)> {
    let shown = output_within(&mut command(&["display", "-p", "#{pid}"]), TEARDOWN_LIMIT);
    let answered = shown.is_some();
    let pid = shown
        .and_then(|o| parse_pid(&String::from_utf8_lossy(&o.stdout)))
        .or(known);
    if answered {
        let _ = output_within(&mut command(&["kill-server"]), TEARDOWN_LIMIT);
        reap(pid);
    } else {
        for pid in known.into_iter().chain(world_pids(tmux_tmpdir, label)) {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
    sweep(tmux_tmpdir)
}

/// The processes running with `TMUX_TMPDIR` `tmux_tmpdir` in their
/// environment (`ps axeww`, as [`world_pids`]): every process a world
/// starts has it, and the servers pass it on to their panes and jobs.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn tmpdir_processes(tmux_tmpdir: &Path) -> Vec<(Pid, String)> {
    let Ok(ps) = Command::new("ps")
        .args(["axeww", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    let var = format!("TMUX_TMPDIR={}", tmux_tmpdir.display());
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| l.split_whitespace().any(|w| w == var))
        .filter_map(|l| {
            let l = l.trim_start();
            let (pid, command) = l.split_once(' ')?;
            Some((parse_pid(pid)?, command.trim_start().to_owned()))
        })
        .filter(|(pid, _)| pid.as_raw_nonzero().get() != std::process::id().cast_signed())
        .collect()
}

/// Wait at most a second for the processes of [`tmpdir_processes`] to go,
/// then kill those left: what was killed.
fn sweep(tmux_tmpdir: &Path) -> Vec<(Pid, String)> {
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        let left = tmpdir_processes(tmux_tmpdir);
        if left.is_empty() {
            return left;
        }
        if Instant::now() >= end {
            for (pid, _) in &left {
                let _ = kill_process(*pid, Signal::KILL);
            }
            return left;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Kill what a test binary killed before its worlds' teardown left (a
/// timeout, `kill -9`): every process of a world of `prefix` (`-L
/// <prefix><pid>-...` with a `TMUX_TMPDIR` under `/tmp`) whose test
/// process `<pid>` is gone. Once per test binary.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn reap_dead_owners(prefix: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(ps) = Command::new("ps")
            .args(["axeww", "-o", "pid=,command="])
            .output()
        else {
            return;
        };
        let flag = format!(" -L {prefix}");
        for line in String::from_utf8_lossy(&ps.stdout).lines() {
            let Some(at) = line.find(&flag) else { continue };
            if !line.contains(" TMUX_TMPDIR=/tmp/") {
                continue;
            }
            let owner: String = line[at + flag.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let rest = &line[at + flag.len() + owner.len()..];
            let (Some(owner), true) = (parse_pid(&owner), rest.starts_with('-')) else {
                continue;
            };
            if test_kill_process(owner).is_ok() {
                continue;
            }
            if let Some(pid) = line.split_whitespace().next().and_then(parse_pid) {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
    });
}

/// Skip the calling test, loudly, when what it needs is missing (a skip is
/// not a pass, AGENTS.md *Gates*): `SKIP: <test>: <why>` goes straight to
/// stderr, past libtest's capture of `eprintln!`, so a plain `cargo test`
/// shows it, and is appended to the file `VTMUX_TEST_SKIPS` names, the
/// list `scripts/check.sh` prints and counts at its end. The caller
/// returns.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn skip(why: &str) {
    use std::io::Write as _;
    let test = std::thread::current().name().unwrap_or("?").to_owned();
    let line = format!("SKIP: {test}: {why}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Some(list) = std::env::var_os("VTMUX_TEST_SKIPS") {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&list)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .unwrap_or_else(|e| panic!("VTMUX_TEST_SKIPS {}: {e}", Path::new(&list).display()));
    }
}

/// How long a wait for a state lasts: it only ends a hang.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub const HANG: Duration = Duration::from_secs(20);

/// Poll `done` every 20 ms until it holds; after [`HANG`], panic naming
/// `what`. The limit only ends a hang: no wait here is for a time.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + HANG;
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// [`wait`] for what `read` gives to be `done`: that value. A timeout
/// shows the last one.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn wait_for<T: std::fmt::Debug>(
    what: &str,
    mut read: impl FnMut() -> T,
    done: impl Fn(&T) -> bool,
) -> T {
    let end = Instant::now() + HANG;
    loop {
        let value = read();
        if done(&value) {
            return value;
        }
        assert!(
            Instant::now() < end,
            "timed out waiting for {what}: {value:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until the server is gone: `ls` (run by `ls`, which gives whether it
/// succeeded and its stderr) fails to connect. A server that is exiting
/// still accepts and at once closes a connection (server.c
/// `server_accept` while `server_exit`), which the client reports as
/// `server exited unexpectedly`; a command run then would meet the same
/// fate, so that is not gone yet.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn wait_gone(mut ls: impl FnMut() -> (bool, String)) {
    wait("the server to exit", || {
        let (ok, stderr) = ls();
        !ok && !stderr.contains("server exited unexpectedly")
    });
}

/// The `strerror` texts that differ between libcs (glibc and Darwin say
/// one thing, musl another), each errno's as one token. tmux prints the
/// libc's text, so a differential test compares the errno, not which libc
/// named it: a static musl vtmux against a glibc tmux 3.8.
const ERRNO_TEXTS: &[(&str, &[&str])] = &[
    ("<EIO>", &["Input/output error", "I/O error"]),
    ("<ENOTTY>", &["Inappropriate ioctl for device", "Not a tty"]),
];

/// `s` with each libc's text for the errnos in [`ERRNO_TEXTS`] as its token.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn errno_tokens(s: &str) -> String {
    let mut out = s.to_owned();
    for (token, texts) in ERRNO_TEXTS {
        for text in *texts {
            out = out.replace(text, token);
        }
    }
    out
}

/// Whether Ghostty with no cursor settings shows a blinking cursor after
/// `bytes`, a client's output: libghostty's terminal as the Ghostty app
/// sets it up with `cursor-style-blink` unset (termio/Termio.zig: DEC mode
/// 12 on at the start), where DECSCUSR 0, xterm-ghostty's `Se`, blinks
/// (Terminal.zig `setCursorStyle`: `default_blink orelse true`).
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn ghostty_blinks(bytes: &[u8]) -> bool {
    use libghostty_vt::terminal::{Mode, Options, Terminal};
    let mut t = Terminal::new(Options {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("libghostty terminal");
    t.set_default_cursor_blink(Some(true))
        .expect("default cursor blink");
    t.vt_write(b"\x1b[?12h");
    t.vt_write(bytes);
    t.mode(Mode::CURSOR_BLINKING).expect("mode 12")
}
