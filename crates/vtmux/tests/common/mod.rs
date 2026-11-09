//! Test teardown shared by `cli`, `attach` and `control`: a test world must
//! not leave a server behind, whether the test passed, failed or panicked.
//!
//! `kill-server` is not enough on its own: if the server is wedged, or the
//! client call fails, the daemonized server (setsid) outlives the test and
//! keeps its PTYs, which exhausts the host's PTY pool for every other run.
//! So a world records the server pid first and `reap` verifies the death.

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
