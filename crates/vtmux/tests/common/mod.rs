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
