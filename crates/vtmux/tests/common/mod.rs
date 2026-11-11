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
/// a server that is stopped or wedged.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn output_within(cmd: &mut Command, limit: Duration) -> Option<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let end = Instant::now() + limit;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().ok()
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
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn teardown(
    tmux_tmpdir: &Path,
    label: &str,
    known: Option<Pid>,
    mut command: impl FnMut(&[&str]) -> Command,
) {
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
}

/// Poll `done` every 20 ms until it holds; after 20 s, panic naming
/// `what`. The limit only ends a hang: no wait here is for a time.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
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
    let end = Instant::now() + Duration::from_secs(20);
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
