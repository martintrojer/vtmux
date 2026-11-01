//! Real binary under a PTY; all runs have private labels and pinned env.
mod common;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use vtmux_vt::{CaptureOpts, PaneTerm, capture};

struct World {
    bin: PathBuf,
    label: String,
    home: tempfile::TempDir,
    /// `TERM` for the client.
    term: &'static str,
}
impl World {
    fn new(bin: PathBuf) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Self {
            bin,
            label: format!(
                "att{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            home: tempfile::tempdir().unwrap(),
            term: "xterm-256color",
        }
    }
    fn args(&self) -> Vec<String> {
        vec![
            "-L".into(),
            self.label.clone(),
            "-f".into(),
            "/dev/null".into(),
        ]
    }
    fn env(&self) -> Vec<(&str, String)> {
        vec![
            ("HOME", self.home.path().display().to_string()),
            ("SHELL", "/bin/sh".into()),
            ("TERM", self.term.into()),
            ("PS1", "$ ".into()),
            ("ENV", "/dev/null".into()),
            ("LC_ALL", "C.UTF-8".into()),
            ("PATH", "/usr/bin:/bin".into()),
        ]
    }
    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(&self.bin)
            .args(self.args())
            .args(args)
            .env_clear()
            .envs(self.env())
            .output()
            .unwrap()
    }
    fn attach(&self, args: &[&str]) -> Attached {
        self.attach_sized(args, 40, 10)
    }
    fn attach_sized(&self, args: &[&str], cols: u16, rows: u16) -> Attached {
        self.attach_env(args, cols, rows, &[])
    }
    /// `attach_sized` with extra variables in the client's environment.
    fn attach_env(&self, args: &[&str], cols: u16, rows: u16, extra: &[(&str, &str)]) -> Attached {
        let mut cmd = CommandBuilder::new(&self.bin);
        cmd.args(self.args());
        cmd.args(args);
        self.spawn_attached(cmd, cols, rows, extra)
    }
    /// `attach_env` with the whole command given (`cmd` already has the
    /// binary and its arguments, or a shell that runs them).
    fn spawn_attached(
        &self,
        mut cmd: CommandBuilder,
        cols: u16,
        rows: u16,
        extra: &[(&str, &str)],
    ) -> Attached {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        cmd.env_clear();
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let paused = Arc::new(AtomicBool::new(false));
        let pause = Arc::clone(&paused);
        let rate = Arc::new(AtomicUsize::new(0));
        let limit = Arc::clone(&rate);
        thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                // A stalled terminal: stop draining the master.
                while pause.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
                // A slow terminal: at most `rate` bytes a second, in 20 ms
                // slices (reviewer-29 slow.py).
                let rate = limit.load(Ordering::Relaxed);
                let want = if rate == 0 {
                    buf.len()
                } else {
                    (rate / 50).clamp(1, buf.len())
                };
                let Ok(n) = reader.read(&mut buf[..want]) else {
                    break;
                };
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
                if rate != 0 {
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });
        let writer = pair.master.take_writer().unwrap();
        Attached {
            master: pair.master,
            writer,
            child,
            bytes,
            paused,
            rate,
            size: (cols, rows),
        }
    }
}
impl Drop for World {
    fn drop(&mut self) {
        let out = self.cli(&["display", "-p", "#{pid}"]);
        let pid = common::parse_pid(&String::from_utf8_lossy(&out.stdout));
        let _ = self.cli(&["kill-server"]);
        common::reap(pid);
    }
}
struct Attached {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    bytes: Arc<Mutex<Vec<u8>>>,
    paused: Arc<AtomicBool>,
    /// Bytes per second the terminal drains; 0 is unlimited.
    rate: Arc<AtomicUsize>,
    size: (u16, u16),
}
impl Attached {
    /// The terminal changes size; the kernel sends the client SIGWINCH.
    fn resize(&mut self, cols: u16, rows: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.size = (cols, rows);
    }
    fn keys(&mut self, b: &[u8]) {
        self.writer.write_all(b).unwrap();
        self.writer.flush().unwrap();
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.lock().unwrap()).into_owned()
    }
    fn screen(&self) -> PaneTerm {
        let mut t = PaneTerm::new(self.size.0, self.size.1, 100);
        t.feed(&self.bytes.lock().unwrap());
        t
    }
    fn wait(&self, predicate: impl Fn(&Self) -> bool) {
        self.wait_for(Duration::from_secs(5), predicate);
    }
    fn wait_for(&self, limit: Duration, predicate: impl Fn(&Self) -> bool) {
        let end = Instant::now() + limit;
        while !predicate(self) {
            assert!(Instant::now() < end, "timeout: {:?}", self.text());
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn signal(&self, signal: rustix::process::Signal) {
        let pid = rustix::process::Pid::from_raw(
            i32::try_from(self.child.process_id().unwrap()).unwrap(),
        )
        .unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
    }
    /// The client's exit code once it has exited.
    fn status(&mut self) -> Option<u32> {
        self.child.try_wait().unwrap().map(|s| s.exit_code())
    }
    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
    fn exited(&mut self, code: u32) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                let text = self.text();
                let tail: String = text
                    .chars()
                    .rev()
                    .take(400)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                assert_eq!(s.exit_code(), code, "client output tail: {tail:?}");
                break;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn bins() -> Vec<PathBuf> {
    let mut bins = vec![PathBuf::from(env!("CARGO_BIN_EXE_vtmux"))];
    let reference = std::env::var_os("VTMUX_REFERENCE_TMUX").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/vtmux/tmux-3.8/bin/tmux"),
        PathBuf::from,
    );
    assert!(
        reference.exists(),
        "reference tmux missing: {}",
        reference.display()
    );
    bins.push(reference);
    bins
}
fn pane_text(a: &Attached) -> String {
    String::from_utf8(capture(&a.screen(), CaptureOpts::default())).unwrap()
}
#[test]
fn attach_detach_frames_and_exit() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            r"printf '\033]2;t\033\\'; printf '$ '; exec cat",
        ]);
        a.wait(|a| pane_text(a).contains('$'));
        a.wait(|a| {
            pane_text(a)
                .lines()
                .nth(9)
                .is_some_and(|r| r.starts_with("[a] 0:w*"))
        });
        let status = vtmux_vt::snapshot(&a.screen());
        assert_eq!(
            status.cell(0, 9).unwrap().style.fg,
            vtmux_vt::Color::Palette(0)
        );
        assert_eq!(
            status.cell(0, 9).unwrap().style.bg,
            vtmux_vt::Color::Palette(2)
        );
        a.keys(b"echo hi\r");
        a.wait(|a| pane_text(a).contains("echo hi\necho hi"));
        a.keys(b"\x02d");
        a.exited(0);
        a.wait(|a| a.text().contains("[detached (from session a)]"));
        assert!(w.cli(&["has", "-t", "a"]).status.success());
        let mut b = w.attach(&["attach", "-t", "a"]);
        b.wait(|a| pane_text(a).contains("echo hi"));
        b.master
            .resize(PtySize {
                rows: 12,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        b.wait(|_| {
            String::from_utf8_lossy(
                &w.cli(&["display", "-p", "-t", "a", "#{pane_width}x#{pane_height}"])
                    .stdout,
            )
            .trim()
                == "60x11"
        });
        assert!(w.cli(&["detach-client", "-s", "a"]).status.success());
        b.exited(0);
    }
}
#[test]
fn large_input_is_drained_without_another_key() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "stty raw -echo; printf READY; head -c 12000 | wc -c; sleep 10",
        ]);
        a.wait(|a| pane_text(a).contains("READY"));
        a.keys(&vec![b'x'; 12000]);
        a.wait(|a| pane_text(a).contains("12000"));
        a.keys(b"\x02d");
        a.exited(0);
    }
}

#[test]
fn signal_exit_restores_terminal_modes() {
    for bin in bins() {
        for signal in [rustix::process::Signal::TERM, rustix::process::Signal::HUP] {
            let w = World::new(bin.clone());
            let mut a = w.attach(&["new", "-s", "a", "cat"]);
            a.wait(|a| a.text().contains("\x1b[?1049h"));
            a.signal(signal);
            a.exited(1);
            a.wait(|a| a.text().contains("\x1b[?1049l"));
            assert!(w.cli(&["has", "-t", "a"]).status.success());
        }
    }
}

/// A terminal client whose tty goes away is dropped by the server itself
/// (tty.c:181-189 `tty_read_callback` -> `server_client_lost`): client-detached
/// fires, with no `window_update_focus` and so no pane-focus-out. Only
/// `MSG_EXITING` (C-b d, detach-client, SIGHUP) takes the focus away first.
#[test]
// Linux only: tcsetwinsize on a fresh posix_openpt master fails with ENOTTY on macOS until the slave is opened (task macos_attach_parity).
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "Linux-only timing/ioctl; macos_attach_parity"
)]
fn closing_the_tty_fires_no_focus_out() {
    use rustix::fs::{Mode, OFlags, open};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use rustix::termios::{Winsize, tcsetwinsize};
    use std::process::Stdio;
    for bin in bins() {
        let w = World::new(bin);
        let ok = |args: &[&str]| assert!(w.cli(args).status.success(), "{args:?}");
        ok(&["new", "-d", "-s", "a", "-x", "40", "-y", "10", "cat"]);
        ok(&["set", "-g", "focus-events", "on"]);
        ok(&["set", "-g", "@log", ""]);
        for hook in ["pane-focus-in", "pane-focus-out", "client-detached"] {
            ok(&[
                "set-hook",
                "-g",
                hook,
                "set -agF @log '|#{hook}:#{hook_pane}'",
            ]);
        }
        // Close-on-exec matters: the client must not inherit the master, or
        // dropping ours below would not hang up its terminal. posix_openpt
        // has no O_CLOEXEC on macOS, so set it with fcntl.
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let slave_path = ptsname(&master, Vec::new()).unwrap();
        let size = Winsize {
            ws_row: 10,
            ws_col: 40,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        tcsetwinsize(&master, size).unwrap();
        let slave = || {
            open(
                slave_path.as_c_str(),
                OFlags::RDWR | OFlags::NOCTTY,
                Mode::empty(),
            )
            .unwrap()
        };
        let mut child = Command::new(&w.bin)
            .args(w.args())
            .args(["attach", "-t", "a"])
            .env_clear()
            .envs(w.env())
            .stdin(Stdio::from(slave()))
            .stdout(Stdio::from(slave()))
            .stderr(Stdio::from(slave()))
            .spawn()
            .unwrap();
        let log = || {
            String::from_utf8_lossy(&w.cli(&["show", "-gv", "@log"]).stdout)
                .trim()
                .to_owned()
        };
        let end = Instant::now() + Duration::from_secs(5);
        while !log().contains("pane-focus-in") && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        // The terminal goes away with the client still running.
        drop(master);
        let end = Instant::now() + Duration::from_secs(5);
        while !log().contains("client-detached") && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(log(), "|pane-focus-in:%0|client-detached:%0");
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn session_and_server_exit_messages() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "cat"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        assert!(w.cli(&["kill-session", "-t", "a"]).status.success());
        a.exited(0);
        a.wait(|a| a.text().contains("[exited]"));
        let mut b = w.attach(&["new", "-s", "b", "cat"]);
        b.wait(|a| a.text().contains("\x1b[?1049h"));
        assert!(w.cli(&["kill-server"]).status.success());
        b.exited(1);
        b.wait(|a| a.text().contains("[server exited]"));
    }
}

/// tmux buffers frames for a stalled terminal (bufferevents); the vtmux
/// client shares `O_NONBLOCK` between stdin and stdout and must not die on
/// EAGAIN.
#[test]
fn stalled_terminal_under_heavy_output_keeps_the_client() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach_sized(
            &[
                "new",
                "-s",
                "a",
                "-n",
                "w",
                "sh",
                "-c",
                "printf READY; read x; i=0; while [ $i -lt 400 ]; do printf '\\033[3%dm%d %060d\\n' $((i%8)) $i $i; i=$((i+1)); done; printf '\\033[0mDONE'; exec cat",
            ],
            80,
            24,
        );
        a.wait(|a| pane_text(a).contains("READY"));
        a.paused.store(true, Ordering::Relaxed);
        a.keys(b"\r");
        thread::sleep(Duration::from_secs(3));
        assert!(a.alive(), "client died while the terminal was stalled");
        a.paused.store(false, Ordering::Relaxed);
        a.wait_for(Duration::from_secs(30), |a| pane_text(a).contains("DONE"));
        assert!(a.alive());
        a.keys(b"\x02d");
        a.exited(0);
        a.wait(|a| a.text().contains("\x1b[?1049l"));
    }
}

/// proc.c `proc_set_signals`: SIGQUIT is ignored and SIGINT is caught and
/// ignored by an attached client (client.c `client_signal`).
#[test]
fn sigint_and_sigquit_leave_the_client_attached() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "cat"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        a.signal(rustix::process::Signal::INT);
        a.signal(rustix::process::Signal::QUIT);
        thread::sleep(Duration::from_millis(300));
        assert!(a.alive());
        a.keys(b"\x02d");
        a.exited(0);
        a.wait(|a| a.text().contains("[detached (from session a)]"));
    }
}

fn pane_line(w: &World) -> String {
    String::from_utf8_lossy(&w.cli(&["capture-pane", "-p", "-t", "a"]).stdout)
        .trim()
        .to_owned()
}

/// tty-keys.c:990-1012: while terminal queries are outstanding a partial
/// sequence waits at least 500ms, so a reply split over a slow link is
/// still swallowed. DECRPM goes last: it ends vtmux's query window.
#[test]
fn split_terminal_replies_are_swallowed() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "sh", "-c", "stty raw -echo; exec cat -v"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        for reply in [
            &b"\x1b[?62;4c"[..],
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
            b"\x1b[?2026;2$y",
        ] {
            a.keys(&reply[..4]);
            thread::sleep(Duration::from_millis(50));
            a.keys(&reply[4..]);
            a.keys(b"|");
        }
        a.wait(|_| pane_line(&w) == "|||");
    }
}

/// tty-keys.c:118-131 and input-keys.c:658-671: Escape before an arrow is
/// Escape plus the arrow for the pane's cursor mode, not xterm M-Up.
#[test]
fn escape_escape_arrow_is_escape_then_arrow() {
    for bin in bins() {
        for (pre, want) in [("", "^[^[[A|"), ("printf '\\033[?1h'; ", "^[^[OA|")] {
            let w = World::new(bin.clone());
            let mut a = w.attach(&[
                "new",
                "-s",
                "a",
                "sh",
                "-c",
                &format!("{pre}stty raw -echo; exec cat -v"),
            ]);
            a.wait(|a| a.text().contains("\x1b[?1049h"));
            thread::sleep(Duration::from_millis(200));
            a.keys(b"\x1b\x1b[A|");
            a.wait(|_| pane_line(&w) == want);
        }
    }
}

/// An endless flood at 80x24, started after READY + Enter.
const FLOOD: &str = "printf READY; read x; i=0; while :; do printf '\\033[3%dm%d %060d\\n' $((i%8)) $i $i; i=$((i+1)); done";

fn flood(w: &World) -> Attached {
    let mut a = w.attach_sized(&["new", "-s", "a", "sh", "-c", FLOOD], 80, 24);
    a.wait(|a| pane_text(a).contains("READY"));
    a.paused.store(true, Ordering::Relaxed);
    a.keys(b"\r");
    a
}

fn rss_kb(w: &World) -> u64 {
    let pid = String::from_utf8(w.cli(&["display", "-p", "#{pid}"]).stdout).unwrap();
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", pid.trim()])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// tty.c `tty_block_maybe`: a client that stops draining while a pane
/// floods must not grow the server without bound (reviewer-28 mem.py: the
/// reference stays at 68 MB, unblocked vtmux passed 1 GB in 30 s).
#[test]
fn stalled_terminal_keeps_server_memory_bounded() {
    let w = World::new(PathBuf::from(env!("CARGO_BIN_EXE_vtmux")));
    let mut a = flood(&w);
    for _ in 0..6 {
        thread::sleep(Duration::from_secs(5));
        let rss = rss_kb(&w);
        assert!(rss < 100 * 1024, "server RSS {rss} kB while stalled");
    }
    assert!(a.alive());
    a.paused.store(false, Ordering::Relaxed);
    a.keys(b"\x02d");
    a.exited(0);
}

/// After a stall, detach is not queued behind stale frames: the client
/// exits promptly and drains about one terminal's worth, not the backlog
/// (reviewer-28 lag.py: reference 0.1 s / 16 KB; unblocked vtmux 15 s / 330 MB).
#[test]
fn detach_after_stall_is_not_behind_a_backlog() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = flood(&w);
        thread::sleep(Duration::from_secs(10));
        let before = a.bytes.lock().unwrap().len();
        a.paused.store(false, Ordering::Relaxed);
        let start = Instant::now();
        a.keys(b"\x02d");
        a.exited(0);
        let took = start.elapsed();
        let drained = a.bytes.lock().unwrap().len() - before;
        assert!(took < Duration::from_secs(1), "detach took {took:?}");
        assert!(drained < 8 << 20, "drained {drained} bytes after detach");
    }
}

/// A terminal that drains slowly but never stops (an SSH link) must not
/// queue key echoes and detach behind the buffers in front of it: the
/// client acknowledges frames once written, so the server blocks on what
/// the terminal has not taken (reviewer-29 slow.py: reference exits within
/// 0.7 s; vtmux counting only its own outbuf took 5.9 s / 12.4 s / 39.1 s).
#[test]
// Linux only: macOS CI runners miss its 2 s detach bound under a throttled terminal (task macos_attach_parity).
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "Linux-only timing/ioctl; macos_attach_parity"
)]
fn slow_terminal_detaches_promptly() {
    for bin in bins() {
        for rate in [500_000, 100_000, 30_000] {
            let w = World::new(bin.clone());
            let mut a = w.attach_sized(&["new", "-s", "a", "sh", "-c", FLOOD], 80, 24);
            a.wait(|a| pane_text(a).contains("READY"));
            a.rate.store(rate, Ordering::Relaxed);
            a.keys(b"\r");
            thread::sleep(Duration::from_secs(5));
            assert!(a.alive());
            let start = Instant::now();
            a.keys(b"\x02d");
            a.exited(0);
            let took = start.elapsed();
            // 3 s: CI's macOS runner takes up to 1.6 s for either binary;
            // the bug this pins took 5.9 s and more.
            assert!(
                took < Duration::from_secs(3),
                "{}: detach at {rate} B/s took {took:?}",
                bin.display()
            );
        }
    }
}

/// After a flood seen through a slow terminal stops, the blocked client
/// gets a full redraw: its screen shows what the pane shows, as with the
/// reference (tty.c `tty_block_maybe` redraws after the queue drains).
#[test]
fn slow_terminal_screen_is_correct_after_the_flood() {
    let flood = "printf READY; read x; i=0; while [ $i -lt 3000 ]; do printf '\\033[3%dm%d %060d\\n' $((i%8)) $i $i; i=$((i+1)); done; printf '\\033[0mDONE'; exec cat";
    let mut screens = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach_sized(&["new", "-s", "a", "sh", "-c", flood], 80, 24);
        a.wait(|a| pane_text(a).contains("READY"));
        a.rate.store(100_000, Ordering::Relaxed);
        a.keys(b"\r");
        a.wait_for(Duration::from_secs(60), |a| pane_text(a).contains("DONE"));
        let observer = pane_line(&w);
        // Rows above the status line; the client is not left behind.
        let rows = |a: &Attached| -> String {
            let text = pane_text(a);
            let rows: Vec<&str> = text.lines().take(23).collect();
            rows.join("\n").trim().to_owned()
        };
        a.wait(|a| rows(a) == observer);
        screens.push(rows(&a));
        a.keys(b"\x02d");
        a.exited(0);
    }
    assert_eq!(
        screens[0], screens[1],
        "vtmux screen differs from reference"
    );
}

/// cmd-select-layout.c:146: a layout string of another size ends in
/// `recalculate_sizes`, so an attached window is fitted back to the client
/// (80x24 less the status line) and the panes' PTYs follow (reviewer-15
/// attach.py).
#[test]
fn select_layout_string_is_fitted_to_the_client() {
    const V2: &str = r#"{"V":2,"L":{"t":"h","w":100,"h":30,"x":0,"y":0,"c":[{"t":"p","w":50,"h":30,"x":0,"y":0,"i":0},{"t":"p","w":49,"h":30,"x":51,"y":0,"i":1}]}}"#;
    for bin in bins() {
        let w = World::new(bin);
        let a = w.attach_sized(&["new", "-s", "a", "-n", "w", "sh"], 80, 24);
        a.wait(|a| pane_text(a).contains('$'));
        assert!(
            w.cli(&["splitw", "-d", "-h", "-t", "a", "sh"])
                .status
                .success()
        );
        let size = |f: &str| {
            String::from_utf8_lossy(&w.cli(&["display", "-p", "-t", "a", f]).stdout)
                .trim()
                .to_owned()
        };
        a.wait(|_| size("#{window_width}x#{window_height}") == "80x23");
        let out = w.cli(&["selectl", "-t", "a", V2]);
        assert!(out.status.success(), "{out:?}");
        a.wait(|_| size("#{window_width}x#{window_height}") == "80x23");
        let panes = String::from_utf8_lossy(
            &w.cli(&["lsp", "-t", "a", "-F", "#{pane_width}x#{pane_height}"])
                .stdout,
        )
        .into_owned();
        assert_eq!(panes, "40x23\n39x23\n");
        for (pane, want) in [("a:0.0", "23 40"), ("a:0.1", "23 39")] {
            assert!(
                w.cli(&["send-keys", "-t", pane, "stty size", "Enter"])
                    .status
                    .success()
            );
            a.wait(|_| {
                String::from_utf8_lossy(&w.cli(&["capture-pane", "-p", "-t", pane]).stdout)
                    .lines()
                    .any(|l| l.trim() == want)
            });
        }
    }
}

/// resize.c:410-421 and server-client.c:1953-1970: select-layout on a
/// background window only marks its new size (`WINDOW_RESIZE`); the window
/// keeps the layout string's size until select-window makes it current
/// (reviewer-17 attach3.py).
#[test]
fn select_layout_on_a_background_window_waits_until_selected() {
    const V2: &str = r#"{"V":2,"L":{"t":"h","w":100,"h":30,"x":0,"y":0,"c":[{"t":"p","w":50,"h":30,"x":0,"y":0,"i":0},{"t":"p","w":49,"h":30,"x":51,"y":0,"i":1}]}}"#;
    for bin in bins() {
        let w = World::new(bin);
        let a = w.attach_sized(&["new", "-s", "a", "-n", "w", "cat"], 80, 24);
        let size = |t: &str| {
            String::from_utf8_lossy(
                &w.cli(&["display", "-p", "-t", t, "#{window_width}x#{window_height}"])
                    .stdout,
            )
            .trim()
            .to_owned()
        };
        a.wait(|_| size("a:0") == "80x23");
        for args in [
            &["neww", "-d", "-t", "a:1", "cat"][..],
            &["splitw", "-d", "-h", "-t", "a:1", "cat"],
            &["selectl", "-t", "a:1", V2],
        ] {
            let out = w.cli(args);
            assert!(out.status.success(), "{args:?}: {out:?}");
        }
        thread::sleep(Duration::from_millis(300));
        assert_eq!(size("a:1"), "100x30");
        assert!(w.cli(&["selectw", "-t", "a:1"]).status.success());
        a.wait(|_| size("a:1") == "80x23");
        let panes = String::from_utf8_lossy(
            &w.cli(&["lsp", "-t", "a:1", "-F", "#{pane_width}x#{pane_height}"])
                .stdout,
        )
        .into_owned();
        assert_eq!(panes, "40x23\n39x23\n");
    }
}

/// `update-environment` from the option, applied when a client attaches
/// (cmd-attach-session.c:135,159): matches are copied (fnmatch), a name that
/// matches nothing is marked removed, `-E` leaves the session alone, and
/// list-clients sees the attached client.
#[test]
fn update_environment_on_attach() {
    for bin in bins() {
        let w = World::new(bin);
        let show = |args: &[&str]| {
            let mut a = vec!["showenv", "-t", "main"];
            a.extend_from_slice(args);
            let o = w.cli(&a);
            String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned()
        };
        assert!(w.cli(&["new", "-d", "-s", "main", "cat"]).status.success());
        assert!(
            w.cli(&["set", "-g", "update-environment", "MYVAR ABSENTVAR TEST_*"])
                .status
                .success()
        );
        w.cli(&["setenv", "-t", "main", "MYVAR", "oldvalue"]);
        w.cli(&["setenv", "-t", "main", "ABSENTVAR", "pre-existing"]);
        let mut a = w.attach_env(
            &["attach", "-t", "main"],
            40,
            10,
            &[
                ("MYVAR", "fromclient"),
                ("TEST_GLOB", "globval"),
                ("OTHER", "nope"),
            ],
        );
        a.wait(|_| !w.cli(&["lsc", "-F", "x"]).stdout.is_empty());
        assert_eq!(show(&["MYVAR"]), "MYVAR=fromclient\n");
        assert_eq!(show(&["TEST_GLOB"]), "TEST_GLOB=globval\n");
        assert_eq!(show(&["ABSENTVAR"]), "-ABSENTVAR\n");
        assert_eq!(show(&["OTHER"]), "unknown variable: OTHER\n");
        let lsc = w.cli(&["lsc", "-t", "main", "-F", "#{session_name}"]);
        assert_eq!(String::from_utf8_lossy(&lsc.stdout), "main\n");
        assert!(w.cli(&["detach-client", "-s", "main"]).status.success());
        a.exited(0);

        w.cli(&["setenv", "-t", "main", "MYVAR", "oldvalue2"]);
        w.cli(&["setenv", "-t", "main", "ABSENTVAR", "pre2"]);
        let mut b = w.attach_env(
            &["attach", "-E", "-t", "main"],
            40,
            10,
            &[("MYVAR", "fromclientE")],
        );
        b.wait(|_| !w.cli(&["lsc", "-F", "x"]).stdout.is_empty());
        assert_eq!(show(&["MYVAR"]), "MYVAR=oldvalue2\n");
        assert_eq!(show(&["ABSENTVAR"]), "ABSENTVAR=pre2\n");
        assert!(w.cli(&["detach-client", "-s", "main"]).status.success());
        b.exited(0);
    }
}

/// The divider column of a left-right split at 40x10: (glyph, fg) for each
/// window row.
fn divider(a: &Attached, x: u16) -> Vec<(String, vtmux_vt::Color)> {
    let snap = vtmux_vt::snapshot(&a.screen());
    (0..9)
        .map(|y| {
            let c = snap.cell(x, y).unwrap();
            (c.text.clone(), c.style.fg)
        })
        .collect()
}

/// screen-redraw.c `redraw_mark_two_pane_colours` and
/// `redraw_draw_border_arrow`: with two panes the divider's top half
/// (`wy <= sy/2`) is the left pane's, the bottom half the right pane's;
/// only the active pane's half is green, and the arrow on its second row
/// points at it. Selecting the other pane moves both.
#[test]
fn active_pane_change_moves_the_coloured_half_and_arrows() {
    use vtmux_vt::Color::Palette;
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "cat"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        for cmd in [
            &["set", "-g", "pane-border-indicators", "both"][..],
            &["splitw", "-h", "-t", "a", "cat"],
            &["select-pane", "-t", "%0", "-T", "t"],
            &["select-pane", "-t", "%1", "-T", "t"],
        ] {
            assert!(w.cli(cmd).status.success());
        }
        // 40 wide: panes of 20 and 19, divider at x 20; 9 window rows.
        let line = |fg| ("│".to_owned(), fg);
        let (grey, green) = (Palette(7), Palette(2));
        let mut want: Vec<_> = (0..9)
            .map(|y| line(if y <= 4 { grey } else { green }))
            .collect();
        want[1] = ("→".into(), grey);
        a.wait(|a| divider(a, 20) == want);
        assert!(w.cli(&["selectp", "-t", "%0"]).status.success());
        let mut want: Vec<_> = (0..9)
            .map(|y| line(if y <= 4 { green } else { grey }))
            .collect();
        want[1] = ("←".into(), green);
        a.wait(|a| divider(a, 20) == want);
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// With three panes there is no half-colouring: a border cell takes the
/// active pane's style whenever it touches it
/// (`redraw_get_pane_for_border_style`), and every arrow cell points at the
/// active pane.
#[test]
fn active_pane_change_recolours_shared_borders() {
    use vtmux_vt::Color::Palette;
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "cat"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        for cmd in [
            &["set", "-g", "pane-border-indicators", "arrows"][..],
            &["splitw", "-h", "-t", "a", "cat"],
            &["splitw", "-v", "-t", "a", "cat"],
            &["select-pane", "-t", "%0", "-T", "t"],
            &["select-pane", "-t", "%1", "-T", "t"],
            &["select-pane", "-t", "%2", "-T", "t"],
        ] {
            assert!(w.cli(cmd).status.success());
        }
        let (grey, green) = (Palette(7), Palette(2));
        // %2 (bottom right, rows 5-8) is active: the divider is green from
        // the junction at row 4 down, with an arrow at row 6.
        let row = |s: &str, fg| (s.to_owned(), fg);
        let mut want: Vec<_> = (0..9)
            .map(|y| row("│", if y < 4 { grey } else { green }))
            .collect();
        want[4] = row("├", green);
        want[6] = row("→", green);
        a.wait(|a| divider(a, 20) == want);
        // %0 (left) is active: the whole divider is its border.
        assert!(w.cli(&["selectp", "-t", "%0"]).status.success());
        let mut want: Vec<_> = (0..9).map(|_| row("│", green)).collect();
        want[4] = row("├", green);
        want[1] = row("←", green);
        want[6] = row("←", green);
        a.wait(|a| divider(a, 20) == want);
        a.keys(b"\x02d");
        a.exited(0);
    }
}

fn display(w: &World, f: &str) -> String {
    String::from_utf8_lossy(&w.cli(&["display", "-p", "-t", "a", f]).stdout)
        .trim()
        .to_owned()
}

/// server-client.c `server_client_key_callback`: the prefix and prefix2
/// switch to the prefix table, `C-b %` splits (the default binding), a
/// root binding runs, an unbound key after the prefix is swallowed.
#[test]
// Linux only: on macOS CI `ESC x` did not reach the M-x binding (escape-time vs a slow runner?) (task macos_attach_parity).
#[cfg_attr(not(target_os = "linux"), ignore = "macos_attach_parity")]
fn prefix_root_and_prefix2_bindings() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "stty raw -echo; exec cat -v",
        ]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        assert!(
            w.cli(&["bind", "-n", "M-x", "rename-window", "mx"])
                .status
                .success()
        );
        assert!(w.cli(&["set", "-g", "prefix2", "C-a"]).status.success());
        a.keys(b"\x02%");
        a.wait(|_| display(&w, "#{window_panes}") == "2");
        a.keys(b"\x1bx");
        a.wait(|_| display(&w, "#{window_name}") == "mx");
        // prefix2 then `"`: a second split.
        a.keys(b"\x01\"");
        a.wait(|_| display(&w, "#{window_panes}") == "3");
        // An unbound key after the prefix does not reach the pane; the
        // next key does.
        a.keys(b"\x02j");
        thread::sleep(Duration::from_millis(100));
        a.keys(b"|");
        a.wait(|_| pane_line(&w).ends_with('|'));
        assert!(!pane_line(&w).contains('j'), "{}", pane_line(&w));
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// `bind -r`: after the prefix, the key repeats without it within
/// repeat-time; once repeat-time passes the key goes to the pane
/// (server-client.c `server_client_repeat_timer`).
#[test]
fn repeat_keys_within_and_after_repeat_time() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "stty raw -echo; exec cat -v",
        ]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        assert!(w.cli(&["set", "-g", "@n", "0"]).status.success());
        assert!(
            w.cli(&["bind", "-r", "R", "set", "-gF", "@n", "#{e|+:#{@n},1}"])
                .status
                .success()
        );
        assert!(w.cli(&["set", "-g", "repeat-time", "400"]).status.success());
        a.keys(b"\x02R");
        a.wait(|_| display(&w, "#{@n}") == "1");
        assert_eq!(
            display(&w, "#{client_key_table} #{client_prefix}"),
            "prefix 1"
        );
        a.keys(b"R");
        a.wait(|_| display(&w, "#{@n}") == "2");
        a.wait_for(Duration::from_secs(2), |_| {
            display(&w, "#{client_key_table} #{client_prefix}") == "root 0"
        });
        a.keys(b"R");
        a.wait(|_| pane_line(&w) == "R");
        assert_eq!(display(&w, "#{@n}"), "2");
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// `switch-client -T` from a binding moves the client into a table; the
/// next key is looked up there, then the client goes back to root.
#[test]
fn switch_client_table_then_key() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "stty raw -echo; exec cat -v",
        ]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        assert!(
            w.cli(&["bind", "-T", "mine", "z", "rename-window", "zz"])
                .status
                .success()
        );
        assert!(
            w.cli(&["bind", "-n", "M-m", "switch-client", "-T", "mine"])
                .status
                .success()
        );
        a.keys(b"\x1bm");
        a.wait(|_| display(&w, "#{client_key_table}") == "mine");
        a.keys(b"z");
        a.wait(|_| display(&w, "#{window_name}") == "zz");
        assert_eq!(display(&w, "#{client_key_table}"), "root");
        // A key that is not in the table is swallowed.
        a.keys(b"\x1bmq|");
        a.wait(|_| pane_line(&w) == "|");
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// list-clients' default template (cmd-list-clients.c:31-36) and the
/// client_* variables (format.c `format_cb_client_*`) for a terminal client
/// (TERM=screen, 100x30) and a control client. The tty and the client
/// pid differ per run and are replaced; `client_termfeatures` is the
/// terminal-features work's and not compared.
#[test]
fn list_clients_default_template_and_client_variables() {
    const F: &str = "#{client_name}|w=#{client_width} h=#{client_height} tn=#{client_termname} \
         tt=#{client_termtype} f=#{client_flags} cm=#{client_control_mode} tty=#{client_tty} \
         ro=#{client_readonly} u8=#{client_utf8} kt=#{client_key_table} pfx=#{client_prefix} \
         ls=#{client_last_session} s=#{client_session} uid=#{==:#{client_uid},#{uid}} \
         user=#{==:#{client_user},#{user}} col=#{client_colours} cw=#{client_cell_width} \
         ch=#{client_cell_height} disc=#{client_discarded} pid=#{?client_pid,y,n} \
         act=#{?client_activity,y,n} cr=#{?client_created,y,n} wr=#{?client_written,y,n}";
    let mut outputs = Vec::new();
    for bin in bins() {
        let mut w = World::new(bin);
        w.term = "screen";
        let a = w.attach_sized(&["new", "-s", "s1", "-n", "w", "cat"], 100, 30);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        let mut control = Command::new(&w.bin)
            .args(w.args())
            .args(["-C", "new", "-s", "s2", "cat"])
            .env_clear()
            .envs(w.env())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let listed = |w: &World| String::from_utf8_lossy(&w.cli(&["lsc"]).stdout).into_owned();
        let start = Instant::now();
        while listed(&w).lines().count() < 2 {
            assert!(start.elapsed() < Duration::from_secs(5), "{}", listed(&w));
            thread::sleep(Duration::from_millis(20));
        }
        let pid = control.id();
        let tty = String::from_utf8_lossy(&w.cli(&["lsc", "-F", "#{client_tty}"]).stdout)
            .lines()
            .find(|l| !l.is_empty())
            .unwrap()
            .to_owned();
        let norm = |s: String| {
            s.replace(&tty, "TTY")
                .replace(&format!("client-{pid}"), "client-PID")
        };
        let mut out = norm(listed(&w));
        out.push_str(&norm(
            String::from_utf8_lossy(&w.cli(&["lsc", "-F", F]).stdout).into_owned(),
        ));
        outputs.push(out);
        drop(control.stdin.take());
        let _ = control.wait();
    }
    assert_eq!(outputs[0], outputs[1], "vtmux (left) vs tmux 3.8 (right)");
}

const CAT: [&str; 3] = ["sh", "-c", "stty raw -echo; exec cat -v"];

fn kt(w: &World) -> String {
    String::from_utf8_lossy(
        &w.cli(&[
            "lsc",
            "-F",
            "#{client_key_table}|#{client_prefix}|#{session_name}",
        ])
        .stdout,
    )
    .trim()
    .to_owned()
}

/// cmd-attach-session.c:162, cmd-new-session.c:344 and options.c:1399: a
/// client attaching, or the `key-table` option changing, puts every client
/// in the session's `key-table`; the first unbound key then goes to the pane
/// and `client_prefix` agrees with the table.
#[test]
fn key_table_option_resets_the_client_table() {
    for bin in bins() {
        let w = World::new(bin);
        let new = |name: &str| {
            let mut v = vec!["new", "-d", "-s", name, "-n", "w"];
            v.extend(CAT);
            assert!(w.cli(&v).status.success());
        };
        new("a");
        assert!(
            w.cli(&["set", "-g", "key-table", "custom"])
                .status
                .success()
        );
        assert!(
            w.cli(&["bind", "-T", "custom", "x", "rename-window", "xx"])
                .status
                .success()
        );
        // attach-session: the table comes from the option.
        let mut at = w.attach(&["attach", "-t", "a"]);
        at.wait(|a| a.text().contains("\x1b[?1049h"));
        assert_eq!(kt(&w), "custom|0|a");
        // An unbound key is passed to the pane, not swallowed.
        at.keys(b"q");
        at.wait(|_| pane_line(&w) == "q");
        // The option changing moves the attached client at once.
        assert!(w.cli(&["set", "-g", "key-table", "other"]).status.success());
        assert_eq!(kt(&w), "other|0|a");
        assert!(w.cli(&["set", "-gu", "key-table"]).status.success());
        assert_eq!(kt(&w), "root|0|a");
        assert!(
            w.cli(&["set", "-g", "key-table", "custom"])
                .status
                .success()
        );
        assert_eq!(kt(&w), "custom|0|a");
        drop(at);
        // new-session with a terminal: the client starts in `custom`.
        let mut v = vec!["new", "-s", "b", "-n", "w"];
        v.extend(CAT);
        let mut at = w.attach(&v);
        at.wait(|a| a.text().contains("\x1b[?1049h"));
        assert_eq!(kt(&w), "custom|0|b");
        at.keys(b"x");
        at.wait(|_| display_t(&w, "b", "#{window_name}") == "xx");
        assert_eq!(kt(&w), "custom|0|b");
    }
}

fn display_t(w: &World, t: &str, f: &str) -> String {
    String::from_utf8_lossy(&w.cli(&["display", "-p", "-t", t, f]).stdout)
        .trim()
        .to_owned()
}

/// server-client.c:1465-1469 and 1523: a focus report from the terminal
/// counts as client activity (and session activity) and is forwarded to the
/// active pane when the program there turned on focus reporting (?1004h)
/// (tty-keys.c:1030-1039, window.c `window_pane_update_focus`).
#[test]
fn focus_events_bump_activity_and_reach_the_pane() {
    for bin in bins() {
        let mut w = World::new(bin);
        w.term = "screen";
        let mut at = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "stty raw -echo; printf '\\033[?1004h'; exec cat -v",
        ]);
        at.wait(|a| a.text().contains("\x1b[?1049h"));
        assert!(w.cli(&["set", "-g", "focus-events", "on"]).status.success());
        let activity = |w: &World| -> (i64, i64, String) {
            let out = w.cli(&[
                "lsc",
                "-F",
                "#{client_activity} #{session_activity} #{client_flags}",
            ]);
            let out = String::from_utf8_lossy(&out.stdout).into_owned();
            let mut p = out.trim().splitn(3, ' ');
            (
                p.next().unwrap().parse().unwrap(),
                p.next().unwrap().parse().unwrap(),
                p.next().unwrap().to_owned(),
            )
        };
        let (c0, s0, flags) = activity(&w);
        assert_eq!(flags, "attached,focused,UTF-8");
        thread::sleep(Duration::from_millis(1100));
        at.keys(b"\x1b[O");
        at.wait(|_| pane_line(&w) == "^[[O");
        let (c1, s1, flags) = activity(&w);
        assert!(c1 > c0 && s1 > s0, "{c0} {c1} {s0} {s1}");
        assert_eq!(flags, "attached,UTF-8");
        thread::sleep(Duration::from_millis(1100));
        at.keys(b"\x1b[I");
        at.wait(|_| pane_line(&w) == "^[[O^[[I");
        let (c2, s2, flags) = activity(&w);
        assert!(c2 > c1 && s2 > s1, "{c1} {c2} {s1} {s2}");
        assert_eq!(flags, "attached,focused,UTF-8");
    }
}

/// `#{session_attached}`, `#{session_attached_list}`, list-sessions'
/// `(attached)` and `#{session_activity}` (format.c `format_cb_session_*`,
/// `session_update_activity`): they follow the client when it switches
/// session.
#[test]
fn session_attached_and_activity_variables() {
    const F: &str = "#{session_name}|#{session_attached}|#{session_attached_list}|\
         #{?#{e|>:#{session_activity},#{session_created}},later,same}";
    let mut outputs = Vec::new();
    for bin in bins() {
        let mut w = World::new(bin);
        w.term = "screen";
        let mut v = vec!["new", "-d", "-s", "s1", "-n", "w"];
        v.extend(CAT);
        assert!(w.cli(&v).status.success());
        v[3] = "s2";
        assert!(w.cli(&v).status.success());
        thread::sleep(Duration::from_millis(1100));
        assert!(
            w.cli(&["bind", "-n", "M-s", "switch-client", "-t", "s2"])
                .status
                .success()
        );
        let mut at = w.attach(&["attach", "-t", "s1"]);
        at.wait(|a| a.text().contains("\x1b[?1049h"));
        let tty = String::from_utf8_lossy(&w.cli(&["lsc", "-F", "#{client_tty}"]).stdout)
            .trim()
            .to_owned();
        let snapshot = |w: &World| {
            let list = String::from_utf8_lossy(&w.cli(&["ls"]).stdout).into_owned();
            // Drop the creation time: `(created ...)`.
            let list: String = list
                .lines()
                .flat_map(|l| {
                    let a = l.find("(created ").unwrap();
                    let b = a + l[a..].find(')').unwrap() + 1;
                    [&l[..a], &l[b..], "\n"]
                })
                .collect();
            let formatted = String::from_utf8_lossy(&w.cli(&["ls", "-F", F]).stdout).into_owned();
            format!("{list}{formatted}").replace(&tty, "TTY")
        };
        let mut out = snapshot(&w);
        at.keys(b"\x1bs");
        at.wait(|_| kt(&w).ends_with("|s2"));
        out.push_str(&snapshot(&w));
        outputs.push(out);
    }
    assert!(
        outputs[0].contains("s1: 1 windows  (attached)"),
        "{}",
        outputs[0]
    );
    assert!(outputs[0].contains("s1|1|TTY|later\n"), "{}", outputs[0]);
    assert_eq!(outputs[0], outputs[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// tty-term.c `tty_term_create` reads `terminal-features` once, when the client
/// attaches (tty.c `tty_open`); `COLORTERM` adds `RGB`/`256` after it. The pane
/// holds `38;2;1;2;3` red-ish text: a truecolour client gets it as is, a 256
/// colour client as `38;5;16` (colour.c `colour_find_rgb`).
#[test]
fn terminal_features_apply_at_attach() {
    // (features before attach, colorterm, features after attach, expect RGB)
    let cases: [(&[&str], &str, &[&str], bool); 7] = [
        (&["xterm*:RGB"], "", &[], true),
        (&[], "", &["xterm*:RGB"], false),
        (&["xterm*:bogus:RGB"], "", &[], false),
        (&["xterm*:RGB::hyperlinks"], "", &[], true),
        (&["xterm*:RGB", "xterm*:RGB@"], "", &[], false),
        (&[], "truecolor", &[], true),
        (&[], "24bit", &["xterm*:RGB@"], true),
    ];
    for bin in bins() {
        for (before, colorterm, after, rgb) in cases {
            let w = World::new(bin.clone());
            let pane = r"printf '\033[38;2;1;2;3mX\033[0m'; exec cat";
            assert!(
                w.cli(&[
                    "new", "-d", "-s", "a", "-x", "40", "-y", "10", "sh", "-c", pane
                ])
                .status
                .success()
            );
            for (i, item) in before.iter().enumerate() {
                let key = format!("terminal-features[{}]", 10 + i);
                assert!(w.cli(&["set", "-s", &key, item]).status.success());
            }
            let extra: Vec<(&str, &str)> = if colorterm.is_empty() {
                vec![]
            } else {
                vec![("COLORTERM", colorterm)]
            };
            let mut a = w.attach_env(&["attach", "-t", "a"], 40, 10, &extra);
            a.wait(|_| !w.cli(&["lsc", "-F", "x"]).stdout.is_empty());
            for (i, item) in after.iter().enumerate() {
                let key = format!("terminal-features[{}]", 20 + i);
                assert!(w.cli(&["set", "-s", &key, item]).status.success());
            }
            let wanted = if rgb { "38;2;1;2;3m" } else { "38;5;16m" };
            a.wait(|a| a.text().contains(wanted));
            let other = if rgb { "38;5;16m" } else { "38;2;1;2;3m" };
            assert!(!a.text().contains(other), "{before:?} {colorterm}: {other}");
            // #{client_termfeatures} and #{client_colours} agree with the reference.
            let list = w.cli(&["lsc", "-F", "#{client_termfeatures}|#{client_colours}"]);
            let list = String::from_utf8_lossy(&list.stdout).trim().to_owned();
            let (features, colours) = list.split_once('|').unwrap();
            assert_eq!(features.split(',').any(|f| f == "RGB"), rgb, "{list}");
            assert_eq!(colours, if rgb { "16777216" } else { "16" }, "{list}");
            a.keys(b"\x02d");
            a.exited(0);
        }
    }
}

/// `tty_check_fg`/`tty_check_bg`: a client with fewer than 256 colours gets
/// `colour_256to16`, aixterm colours only from 16, bold for bright foregrounds
/// at 8 (reviewer-26 raw.py). The observer's screen is the same under both.
#[test]
fn colours_below_256_are_downgraded_like_tmux() {
    // fg 38;5;200 on default, 38;5;16 on 48;5;200, rgb 1;2;3, aixterm 91 on 101,
    // 38;5;244 on 47 (grey on white).
    let pane = r"printf '\033[38;5;200mA\033[0m \033[38;5;16;48;5;200mB\033[0m \033[38;2;1;2;3mC\033[0m \033[91;101mD\033[0m \033[38;5;244;47mE\033[0m'; exec cat";
    for term in ["xterm", "screen", "xterm-16color", "xterm-88color"] {
        let mut screens = Vec::new();
        for bin in bins() {
            let w = World::new(bin);
            assert!(
                w.cli(&[
                    "new", "-d", "-s", "a", "-x", "40", "-y", "10", "sh", "-c", pane
                ])
                .status
                .success()
            );
            let mut a = w.attach_env(&["attach", "-t", "a"], 40, 10, &[("TERM", term)]);
            a.wait(|a| {
                vtmux_vt::snapshot(&a.screen())
                    .cell(8, 0)
                    .is_some_and(|c| c.text == "E")
            });
            let screen = vtmux_vt::snapshot(&a.screen());
            let row: Vec<_> = (0..9)
                .map(|x| {
                    let c = screen.cell(x, 0).unwrap();
                    (c.text.clone(), c.style.fg, c.style.bg, c.style.bold)
                })
                .collect();
            screens.push(row);
            a.keys(b"\x02d");
            a.exited(0);
        }
        assert_eq!(screens[0], screens[1], "{term}: vtmux vs reference");
        // 200 is bright magenta in 16 colours: aixterm or plain, never 38;5;n.
        assert_ne!(screens[0][0].1, vtmux_vt::Color::Palette(200), "{term}");
    }
}

/// BELs in everything the client wrote to its terminal so far.
fn bels(a: &Attached) -> usize {
    a.bytes
        .lock()
        .unwrap()
        .iter()
        .fold(0, |n, b| n + usize::from(*b == 7))
}

/// The status line (last row) of the client's screen.
fn status_row(a: &Attached) -> String {
    let t = a.screen();
    String::from_utf8(capture(&t, CaptureOpts::default()))
        .unwrap()
        .lines()
        .last()
        .unwrap_or_default()
        .trim_end()
        .to_owned()
}

/// alerts.c `alerts_set_message`, `alerts_check_bell`: a BEL from a
/// background pane reaches the attached client as a BEL unless
/// `bell-action` is none (or `current`: it is not the current window) or
/// `visual-bell` is on; the status line shows `!` and, for activity, `#`.
#[test]
fn alerts_bell_forwarding_and_status() {
    // (set-option args, a BEL is expected)
    let cases: &[(&[&str], bool)] = &[
        (&[], true),
        (&["bell-action", "none"], false),
        (&["bell-action", "current"], false),
        (&["bell-action", "other"], true),
        (&["visual-bell", "on"], false),
        (&["visual-bell", "both"], true),
    ];
    for bin in bins() {
        for (opt, want) in cases {
            let w = World::new(bin.clone());
            let a = w.attach(&[
                "new",
                "-s",
                "a",
                "-n",
                "w",
                "sh",
                "-c",
                "printf '$ '; exec cat",
            ]);
            a.wait(|a| pane_text(a).contains('$'));
            w.cli(&[
                "neww",
                "-d",
                "-n",
                "b",
                "sh",
                "-c",
                r"read x; printf '\007'; exec cat",
            ]);
            if !opt.is_empty() {
                w.cli(&["set", "-g", opt[0], opt[1]]);
            }
            // The default status-right holds the host name, which on CI is
            // long enough to push window 1 off the 40-column status line.
            w.cli(&["set", "-g", "status-right", ""]);
            // The activity alert would ring too (visual-activity is off).
            w.cli(&["set", "-g", "activity-action", "none"]);
            w.cli(&["set", "-g", "monitor-activity", "on"]);
            a.wait(|a| status_row(a).contains("1:b"));
            let before = bels(&a);
            w.cli(&["send-keys", "-t", "a:1", "Enter"]);
            a.wait(|a| status_row(a).contains("1:b#"));
            thread::sleep(Duration::from_millis(300));
            let seen = bels(&a) > before;
            assert_eq!(seen, *want, "{} {opt:?}: bell forwarded", bin.display());
            // The bell flag follows the winlink, whatever the action. With
            // visual-bell the "Bell in window 1" message may cover the status
            // line for display-time first, so wait for the flags.
            a.wait(|a| {
                let row = status_row(a);
                row.contains("1:b#!") || row.contains("1:b!#")
            });
            // Selecting the window clears both flags.
            w.cli(&["select-window", "-t", "a:1"]);
            a.wait(|a| status_row(a).contains("1:b*"));
        }
    }
}

/// cmd-display-message.c `tc = cmdq_get_target_client` (`CMD_CLIENT_CFLAG`,
/// cmd-find.c `cmd_find_client`): `-c` names the client that shows the
/// message, by name or tty with or without `/dev/`; the other client keeps
/// its status. An unknown name shows nothing and is not an error
/// (`CMD_CLIENT_CANFAIL`). Formats expand for the target session, with `tc`
/// only when it is on it (cmd-display-message.c:117-126).
#[test]
fn display_message_c_shows_on_the_named_client() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["new", "-d", "-s", "b", "-n", "bw", "cat"]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["set", "-g", "display-time", "0"]);
        let a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let b = w.attach(&["attach", "-t", "b"]);
        b.wait(|b| status_row(b).contains("[b]"));
        let name = |s: &str| {
            let out = w.cli(&["lsc", "-t", s, "-F", "#{client_name}"]);
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let (na, nb) = (name("a"), name("b"));
        let mut seen = Vec::new();
        let out = w.cli(&["display", "-c", &na, "to a #{session_name}"]);
        seen.push(format!("exit {:?}", out.status.code()));
        a.wait(|a| status_row(a).starts_with("to a"));
        thread::sleep(Duration::from_millis(200));
        seen.push(format!("{} / {}", status_row(&a), status_row(&b)));
        // A trailing colon and no /dev/ prefix still match.
        let short = format!("{}:", nb.trim_start_matches("/dev/"));
        w.cli(&["display", "-c", &short, "to b"]);
        b.wait(|b| status_row(b).starts_with("to b"));
        seen.push(format!("{} / {}", status_row(&a), status_row(&b)));
        let out = w.cli(&["display", "-c", "nope", "lost"]);
        seen.push(format!(
            "exit {:?} {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
        thread::sleep(Duration::from_millis(200));
        seen.push(format!("{} / {}", status_row(&a), status_row(&b)));
        results.push(seen);
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
    // The formats use the -t target (the best session, b), not -c's.
    assert!(
        results[0][1].starts_with("to a b / [b]"),
        "{:?}",
        results[0]
    );
}

/// The startup config is parsed for the attaching client (cfg.c:81-90,
/// `pi.c`): `%if` sees its tty, terminal name and name, on both servers.
#[test]
fn startup_config_conditions_see_the_attaching_client() {
    for bin in bins() {
        let w = World::new(bin);
        let conf = w.home.path().join("c.conf");
        std::fs::write(
            &conf,
            "%if \"#{client_tty}\"\nset -g @tty has\n%else\nset -g @tty none\n%endif\n\
             %if \"#{==:#{client_termname},xterm-256color}\"\nset -g @term yes\n%else\nset -g @term no\n%endif\n\
             %if \"#{client_name}\"\nset -g @cn has\n%else\nset -g @cn none\n%endif\n",
        )
        .unwrap();
        let mut a = w.attach(&["-f", conf.to_str().unwrap(), "new", "-s", "a"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        let got = display(&w, "#{@tty} #{@term} #{@cn}");
        assert_eq!(got, "has yes has", "{}", w.bin.display());
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// The focus ring screen (D23): each row of the client's screen, then a
/// style map of it: `A` a box-drawing cell in the active border colour
/// (green), `b` one in the other border colour (grey), `.` anything else.
fn ring_screen(a: &Attached) -> Vec<String> {
    use vtmux_vt::Color::Palette;
    let snap = vtmux_vt::snapshot(&a.screen());
    let mut text = Vec::new();
    let mut style = Vec::new();
    for y in 0..snap.rows {
        let (mut t, mut m) = (String::new(), String::new());
        for x in 0..snap.cols {
            let c = snap.cell(x, y).unwrap();
            t.push_str(if c.text.is_empty() { " " } else { &c.text });
            let boxed = c
                .text
                .chars()
                .next()
                .is_some_and(|ch| ('\u{2500}'..='\u{257f}').contains(&ch));
            m.push(match (boxed, c.style.fg) {
                (true, Palette(2)) => 'A',
                (true, Palette(7) | vtmux_vt::Color::Default) => 'b',
                (true, _) => '?',
                (false, _) => '.',
            });
        }
        text.push(t);
        style.push(m);
    }
    text.extend(style);
    text
}

/// Wait (at most 5 s) for the ring screen to be `want`, then compare it.
fn ring_settles(a: &Attached, want: &[&str]) {
    let end = Instant::now() + Duration::from_secs(5);
    while ring_screen(a) != want && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(ring_screen(a), want);
}

/// A 20x8 client with the status line off, `cat` panes and `cmds` run.
fn ring_world(cmds: &[&[&str]]) -> (World, Attached) {
    ring_world_sized(cmds, 20, 8)
}

/// [`ring_world`] on a `cols` x `rows` client.
fn ring_world_sized(cmds: &[&[&str]], cols: u16, rows: u16) -> (World, Attached) {
    let w = World::new(PathBuf::from(env!("CARGO_BIN_EXE_vtmux")));
    let a = w.attach_sized(&["new", "-s", "a", "-n", "w", "cat"], cols, rows);
    a.wait(|a| a.text().contains("\x1b[?1049h"));
    for cmd in [
        &["set", "-g", "status", "off"][..],
        &["set", "-g", "vtmux-focus-ring", "on"],
    ]
    .iter()
    .chain(cmds)
    {
        assert!(w.cli(cmd).status.success(), "{cmd:?}");
    }
    (w, a)
}

/// D23: one pane is framed whole, heavy and green; the window is 2 cells
/// smaller each way. Turning the ring off gives the window its full size
/// back, with no border at all.
#[test]
fn focus_ring_one_pane() {
    let (w, a) = ring_world(&[]);
    ring_settles(
        &a,
        &[
            "┏━━━━━━━━━━━━━━━━━━┓",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┗━━━━━━━━━━━━━━━━━━┛",
            "AAAAAAAAAAAAAAAAAAAA",
            "A..................A",
            "A..................A",
            "A..................A",
            "A..................A",
            "A..................A",
            "A..................A",
            "AAAAAAAAAAAAAAAAAAAA",
        ],
    );
    assert_eq!(display(&w, "#{window_width}x#{window_height}"), "18x6");
    // A new window is fitted inside the ring too.
    assert!(w.cli(&["neww", "-t", "a", "cat"]).status.success());
    assert_eq!(display(&w, "#{window_width}x#{window_height}"), "18x6");
    assert!(w.cli(&["kill-window", "-t", "a:1"]).status.success());
    assert!(
        w.cli(&["set", "-g", "vtmux-focus-ring", "off"])
            .status
            .success()
    );
    let blank = " ".repeat(20);
    let dots = ".".repeat(20);
    let want: Vec<&str> = [blank.as_str(); 8]
        .into_iter()
        .chain([dots.as_str(); 8])
        .collect();
    ring_settles(&a, &want);
    assert_eq!(display(&w, "#{window_width}x#{window_height}"), "20x8");
}

/// D23, two panes split left-right: the active (right) pane's rectangle is
/// heavy and green, including its side of the shared divider; the junctions
/// mix weights (`┲`, `┺`).
#[test]
fn focus_ring_two_panes_left_right() {
    let (_w, a) = ring_world(&[&["splitw", "-h", "-t", "a", "cat"]]);
    ring_settles(
        &a,
        &[
            "┌─────────┲━━━━━━━━┓",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "└─────────┺━━━━━━━━┛",
            "bbbbbbbbbbAAAAAAAAAA",
            "b.........A........A",
            "b.........A........A",
            "b.........A........A",
            "b.........A........A",
            "b.........A........A",
            "b.........A........A",
            "bbbbbbbbbbAAAAAAAAAA",
        ],
    );
}

/// D23, two panes split top-bottom: the bottom pane is enclosed.
#[test]
fn focus_ring_two_panes_top_bottom() {
    let (_w, a) = ring_world(&[&["splitw", "-v", "-t", "a", "cat"]]);
    ring_settles(
        &a,
        &[
            "┌──────────────────┐",
            "│                  │",
            "│                  │",
            "│                  │",
            "┢━━━━━━━━━━━━━━━━━━┪",
            "┃                  ┃",
            "┃                  ┃",
            "┗━━━━━━━━━━━━━━━━━━┛",
            "bbbbbbbbbbbbbbbbbbbb",
            "b..................b",
            "b..................b",
            "b..................b",
            "AAAAAAAAAAAAAAAAAAAA",
            "A..................A",
            "A..................A",
            "AAAAAAAAAAAAAAAAAAAA",
        ],
    );
}

/// D23, three tiled panes, then the active pane moving: the heavy green
/// rectangle follows it, and `pane-border-indicators both` (no arrows, no
/// half-colouring) is ignored.
#[test]
fn focus_ring_three_panes_and_the_active_pane_moving() {
    let (w, a) = ring_world(&[
        &["set", "-g", "pane-border-indicators", "both"],
        &["splitw", "-h", "-t", "a", "cat"],
        &["splitw", "-v", "-t", "a", "cat"],
    ]);
    ring_settles(
        &a,
        &[
            "┌─────────┬────────┐",
            "│         │        │",
            "│         │        │",
            "│         │        │",
            "│         ┢━━━━━━━━┪",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "└─────────┺━━━━━━━━┛",
            "bbbbbbbbbbbbbbbbbbbb",
            "b.........b........b",
            "b.........b........b",
            "b.........b........b",
            "b.........AAAAAAAAAA",
            "b.........A........A",
            "b.........A........A",
            "bbbbbbbbbbAAAAAAAAAA",
        ],
    );
    assert!(w.cli(&["selectp", "-t", "%0"]).status.success());
    ring_settles(
        &a,
        &[
            "┏━━━━━━━━━┱────────┐",
            "┃         ┃        │",
            "┃         ┃        │",
            "┃         ┃        │",
            "┃         ┠────────┤",
            "┃         ┃        │",
            "┃         ┃        │",
            "┗━━━━━━━━━┹────────┘",
            "AAAAAAAAAAAbbbbbbbbb",
            "A.........A........b",
            "A.........A........b",
            "A.........A........b",
            "A.........Abbbbbbbbb",
            "A.........A........b",
            "A.........A........b",
            "AAAAAAAAAAAbbbbbbbbb",
        ],
    );
}

/// D23, zoomed: the ring wraps the zoomed pane alone, at the inset size.
#[test]
fn focus_ring_zoomed() {
    let (w, a) = ring_world(&[
        &["splitw", "-h", "-t", "a", "cat"],
        &["splitw", "-v", "-t", "a", "cat"],
        &["resize-pane", "-Z", "-t", "%1"],
    ]);
    ring_settles(
        &a,
        &[
            "┏━━━━━━━━━━━━━━━━━━┓",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┃                  ┃",
            "┗━━━━━━━━━━━━━━━━━━┛",
            "AAAAAAAAAAAAAAAAAAAA",
            "A..................A",
            "A..................A",
            "A..................A",
            "A..................A",
            "A..................A",
            "A..................A",
            "AAAAAAAAAAAAAAAAAAAA",
        ],
    );
    assert_eq!(
        display(
            &w,
            "#{window_zoomed_flag} #{P:#{?pane_active,#{pane_id}=#{pane_width}x#{pane_height},}}"
        ),
        "1 %1=18x6"
    );
}

/// D23 with `pane-border-status top`: no row is reserved; each pane's
/// status line sits on its top edge, the ring's edge for the top panes,
/// and on the active pane's rectangle it keeps the heavy line and the
/// active colour. `bottom` puts them on the bottom edges.
#[test]
fn focus_ring_pane_border_status() {
    let (w, a) = ring_world(&[
        &["set", "-g", "pane-border-format", "#{pane_index}"],
        &["set", "-g", "pane-border-status", "top"],
        &["splitw", "-h", "-t", "a", "cat"],
        &["splitw", "-v", "-t", "a", "cat"],
    ]);
    ring_settles(
        &a,
        &[
            "┌──0──────┬──1─────┐",
            "│         │        │",
            "│         │        │",
            "│         │        │",
            "│         ┢━━2━━━━━┪",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "└─────────┺━━━━━━━━┛",
            "bbb.bbbbbbbbb.bbbbbb",
            "b.........b........b",
            "b.........b........b",
            "b.........b........b",
            "b.........AAA.AAAAAA",
            "b.........A........A",
            "b.........A........A",
            "bbbbbbbbbbAAAAAAAAAA",
        ],
    );
    assert_eq!(
        display(
            &w,
            "#{P:#{pane_id}=#{pane_width}x#{pane_height}@#{pane_left}/#{pane_top}|}"
        ),
        "%0=9x6@0/0|%1=8x3@10/0|%2=8x2@10/4|"
    );
    assert!(
        w.cli(&["set", "-g", "pane-border-status", "bottom"])
            .status
            .success()
    );
    ring_settles(
        &a,
        &[
            "┌─────────┬────────┐",
            "│         │        │",
            "│         │        │",
            "│         │        │",
            "│         ┢━━1━━━━━┪",
            "│         ┃        ┃",
            "│         ┃        ┃",
            "└──0──────┺━━2━━━━━┛",
            "bbbbbbbbbbbbbbbbbbbb",
            "b.........b........b",
            "b.........b........b",
            "b.........b........b",
            "b.........AAA.AAAAAA",
            "b.........A........A",
            "b.........A........A",
            "bbb.bbbbbbAAA.AAAAAA",
        ],
    );
}

/// Select each pane in turn and wait for its ring screen.
fn ring_follows(w: &World, a: &Attached, screens: &[(&str, &[&str])]) {
    for (pane, want) in screens {
        assert!(w.cli(&["selectp", "-t", pane]).status.success());
        ring_settles(a, want);
    }
}

/// D23, 2x2 tiled: the active pane in each quadrant of the 4-way cross
/// gets its own heavy corner there (`╃ ╄ ╅ ╆`), in the active colour. The
/// ring decides weights from the active pane's rectangle, not from which
/// pane owns the cell's sides: at a cross the panes up-right and down-left
/// own the down-right pane's slots (reviewer-37).
#[test]
fn focus_ring_two_by_two_cross() {
    let (w, a) = ring_world(&[
        &["splitw", "-h", "-t", "a", "cat"],
        &["splitw", "-h", "-t", "a", "cat"],
        &["splitw", "-h", "-t", "a", "cat"],
        &["select-layout", "-t", "a", "tiled"],
    ]);
    ring_follows(
        &w,
        &a,
        &[
            (
                "%0",
                &[
                    "┏━━━━━━━━┱─────────┐",
                    "┃        ┃         │",
                    "┃        ┃         │",
                    "┡━━━━━━━━╃─────────┤",
                    "│        │         │",
                    "│        │         │",
                    "│        │         │",
                    "└────────┴─────────┘",
                    "AAAAAAAAAAbbbbbbbbbb",
                    "A........A.........b",
                    "A........A.........b",
                    "AAAAAAAAAAbbbbbbbbbb",
                    "b........b.........b",
                    "b........b.........b",
                    "b........b.........b",
                    "bbbbbbbbbbbbbbbbbbbb",
                ],
            ),
            (
                "%1",
                &[
                    "┌────────┲━━━━━━━━━┓",
                    "│        ┃         ┃",
                    "│        ┃         ┃",
                    "├────────╄━━━━━━━━━┩",
                    "│        │         │",
                    "│        │         │",
                    "│        │         │",
                    "└────────┴─────────┘",
                    "bbbbbbbbbAAAAAAAAAAA",
                    "b........A.........A",
                    "b........A.........A",
                    "bbbbbbbbbAAAAAAAAAAA",
                    "b........b.........b",
                    "b........b.........b",
                    "b........b.........b",
                    "bbbbbbbbbbbbbbbbbbbb",
                ],
            ),
            (
                "%2",
                &[
                    "┌────────┬─────────┐",
                    "│        │         │",
                    "│        │         │",
                    "┢━━━━━━━━╅─────────┤",
                    "┃        ┃         │",
                    "┃        ┃         │",
                    "┃        ┃         │",
                    "┗━━━━━━━━┹─────────┘",
                    "bbbbbbbbbbbbbbbbbbbb",
                    "b........b.........b",
                    "b........b.........b",
                    "AAAAAAAAAAbbbbbbbbbb",
                    "A........A.........b",
                    "A........A.........b",
                    "A........A.........b",
                    "AAAAAAAAAAbbbbbbbbbb",
                ],
            ),
            (
                "%3",
                &[
                    "┌────────┬─────────┐",
                    "│        │         │",
                    "│        │         │",
                    "├────────╆━━━━━━━━━┪",
                    "│        ┃         ┃",
                    "│        ┃         ┃",
                    "│        ┃         ┃",
                    "└────────┺━━━━━━━━━┛",
                    "bbbbbbbbbbbbbbbbbbbb",
                    "b........b.........b",
                    "b........b.........b",
                    "bbbbbbbbbAAAAAAAAAAA",
                    "b........A.........A",
                    "b........A.........A",
                    "b........A.........A",
                    "bbbbbbbbbAAAAAAAAAAA",
                ],
            ),
        ],
    );
}

/// D23, 3x3 tiled: the middle pane sits in all four quadrants of a cross
/// at once (`╆ ╅ ╄ ╃`); the bottom-right pane closes on a `╆`.
#[test]
fn focus_ring_three_by_three_cross() {
    let mut cmds: Vec<&[&str]> = Vec::new();
    for _ in 0..8 {
        cmds.push(&["splitw", "-t", "a", "cat"]);
        cmds.push(&["select-layout", "-t", "a", "tiled"]);
    }
    let (w, a) = ring_world_sized(&cmds, 20, 11);
    ring_follows(
        &w,
        &a,
        &[
            (
                "%4",
                &[
                    "┌─────┬─────┬──────┐",
                    "│     │     │      │",
                    "│     │     │      │",
                    "├─────╆━━━━━╅──────┤",
                    "│     ┃     ┃      │",
                    "│     ┃     ┃      │",
                    "├─────╄━━━━━╃──────┤",
                    "│     │     │      │",
                    "│     │     │      │",
                    "│     │     │      │",
                    "└─────┴─────┴──────┘",
                    "bbbbbbbbbbbbbbbbbbbb",
                    "b.....b.....b......b",
                    "b.....b.....b......b",
                    "bbbbbbAAAAAAAbbbbbbb",
                    "b.....A.....A......b",
                    "b.....A.....A......b",
                    "bbbbbbAAAAAAAbbbbbbb",
                    "b.....b.....b......b",
                    "b.....b.....b......b",
                    "b.....b.....b......b",
                    "bbbbbbbbbbbbbbbbbbbb",
                ],
            ),
            (
                "%8",
                &[
                    "┌─────┬─────┬──────┐",
                    "│     │     │      │",
                    "│     │     │      │",
                    "├─────┼─────┼──────┤",
                    "│     │     │      │",
                    "│     │     │      │",
                    "├─────┼─────╆━━━━━━┪",
                    "│     │     ┃      ┃",
                    "│     │     ┃      ┃",
                    "│     │     ┃      ┃",
                    "└─────┴─────┺━━━━━━┛",
                    "bbbbbbbbbbbbbbbbbbbb",
                    "b.....b.....b......b",
                    "b.....b.....b......b",
                    "bbbbbbbbbbbbbbbbbbbb",
                    "b.....b.....b......b",
                    "b.....b.....b......b",
                    "bbbbbbbbbbbbAAAAAAAA",
                    "b.....b.....A......A",
                    "b.....b.....A......A",
                    "b.....b.....A......A",
                    "bbbbbbbbbbbbAAAAAAAA",
                ],
            ),
        ],
    );
}

/// D23, nested splits: a left-right split with each side split top-bottom
/// meets in a cross the layout tree never names; the active pane in each
/// quadrant gets its heavy corner there.
#[test]
fn focus_ring_nested_cross() {
    let (w, a) = ring_world(&[
        &["splitw", "-h", "-t", "a", "cat"],
        &["splitw", "-v", "-t", "%0", "cat"],
        &["splitw", "-v", "-t", "%1", "cat"],
    ]);
    ring_follows(
        &w,
        &a,
        &[
            (
                "%0",
                &[
                    "┏━━━━━━━━━┱────────┐",
                    "┃         ┃        │",
                    "┃         ┃        │",
                    "┃         ┃        │",
                    "┡━━━━━━━━━╃────────┤",
                    "│         │        │",
                    "│         │        │",
                    "└─────────┴────────┘",
                    "AAAAAAAAAAAbbbbbbbbb",
                    "A.........A........b",
                    "A.........A........b",
                    "A.........A........b",
                    "AAAAAAAAAAAbbbbbbbbb",
                    "b.........b........b",
                    "b.........b........b",
                    "bbbbbbbbbbbbbbbbbbbb",
                ],
            ),
            (
                "%1",
                &[
                    "┌─────────┲━━━━━━━━┓",
                    "│         ┃        ┃",
                    "│         ┃        ┃",
                    "│         ┃        ┃",
                    "├─────────╄━━━━━━━━┩",
                    "│         │        │",
                    "│         │        │",
                    "└─────────┴────────┘",
                    "bbbbbbbbbbAAAAAAAAAA",
                    "b.........A........A",
                    "b.........A........A",
                    "b.........A........A",
                    "bbbbbbbbbbAAAAAAAAAA",
                    "b.........b........b",
                    "b.........b........b",
                    "bbbbbbbbbbbbbbbbbbbb",
                ],
            ),
            (
                "%2",
                &[
                    "┌─────────┬────────┐",
                    "│         │        │",
                    "│         │        │",
                    "│         │        │",
                    "┢━━━━━━━━━╅────────┤",
                    "┃         ┃        │",
                    "┃         ┃        │",
                    "┗━━━━━━━━━┹────────┘",
                    "bbbbbbbbbbbbbbbbbbbb",
                    "b.........b........b",
                    "b.........b........b",
                    "b.........b........b",
                    "AAAAAAAAAAAbbbbbbbbb",
                    "A.........A........b",
                    "A.........A........b",
                    "AAAAAAAAAAAbbbbbbbbb",
                ],
            ),
            (
                "%3",
                &[
                    "┌─────────┬────────┐",
                    "│         │        │",
                    "│         │        │",
                    "│         │        │",
                    "├─────────╆━━━━━━━━┪",
                    "│         ┃        ┃",
                    "│         ┃        ┃",
                    "└─────────┺━━━━━━━━┛",
                    "bbbbbbbbbbbbbbbbbbbb",
                    "b.........b........b",
                    "b.........b........b",
                    "b.........b........b",
                    "bbbbbbbbbbAAAAAAAAAA",
                    "b.........A........A",
                    "b.........A........A",
                    "bbbbbbbbbbAAAAAAAAAA",
                ],
            ),
        ],
    );
}

/// D23, nested splits: a right side split top-bottom, its bottom split
/// again left-right, mixes weights at T junctions (`┢ ┱ ┲ ┡ ┯ ┩`).
#[test]
fn focus_ring_nested_tees() {
    let (w, a) = ring_world_sized(
        &[
            &["splitw", "-h", "-t", "a", "cat"],
            &["splitw", "-v", "-t", "%1", "cat"],
            &["splitw", "-h", "-t", "%2", "cat"],
        ],
        24,
        8,
    );
    ring_follows(
        &w,
        &a,
        &[
            (
                "%2",
                &[
                    "┌───────────┬──────────┐",
                    "│           │          │",
                    "│           │          │",
                    "│           │          │",
                    "│           ┢━━━━━┱────┤",
                    "│           ┃     ┃    │",
                    "│           ┃     ┃    │",
                    "└───────────┺━━━━━┹────┘",
                    "bbbbbbbbbbbbbbbbbbbbbbbb",
                    "b...........b..........b",
                    "b...........b..........b",
                    "b...........b..........b",
                    "b...........AAAAAAAbbbbb",
                    "b...........A.....A....b",
                    "b...........A.....A....b",
                    "bbbbbbbbbbbbAAAAAAAbbbbb",
                ],
            ),
            (
                "%3",
                &[
                    "┌───────────┬──────────┐",
                    "│           │          │",
                    "│           │          │",
                    "│           │          │",
                    "│           ├─────┲━━━━┪",
                    "│           │     ┃    ┃",
                    "│           │     ┃    ┃",
                    "└───────────┴─────┺━━━━┛",
                    "bbbbbbbbbbbbbbbbbbbbbbbb",
                    "b...........b..........b",
                    "b...........b..........b",
                    "b...........b..........b",
                    "b...........bbbbbbAAAAAA",
                    "b...........b.....A....A",
                    "b...........b.....A....A",
                    "bbbbbbbbbbbbbbbbbbAAAAAA",
                ],
            ),
            (
                "%1",
                &[
                    "┌───────────┲━━━━━━━━━━┓",
                    "│           ┃          ┃",
                    "│           ┃          ┃",
                    "│           ┃          ┃",
                    "│           ┡━━━━━┯━━━━┩",
                    "│           │     │    │",
                    "│           │     │    │",
                    "└───────────┴─────┴────┘",
                    "bbbbbbbbbbbbAAAAAAAAAAAA",
                    "b...........A..........A",
                    "b...........A..........A",
                    "b...........A..........A",
                    "b...........AAAAAAAAAAAA",
                    "b...........b.....b....b",
                    "b...........b.....b....b",
                    "bbbbbbbbbbbbbbbbbbbbbbbb",
                ],
            ),
        ],
    );
}

/// A pane program that prints `P ` and then `W<n>.` on each SIGWINCH.
const WINCH: [&str; 3] = [
    "sh",
    "-c",
    "n=0; trap 'n=$((n+1)); printf W$n.' WINCH; printf 'P '; sleep 100000 & while :; do wait; done",
];

/// One step of a sizing script ([`sizing`]).
enum Op {
    Run(&'static [&'static str]),
    /// [`Op::Run`] without the pause after it.
    RunNow(&'static [&'static str]),
    /// [`Op::Attach`] without the pause after it: the next step lands
    /// inside the panes' 250 ms resize timer.
    AttachNow(usize, &'static str, u16, u16),
    /// Attach client `n` to a session at this size.
    Attach(usize, &'static str, u16, u16),
    Resize(usize, u16, u16),
    Keys(usize, &'static [u8]),
    Close(usize),
    /// Window sizes, manual sizes and `window-size` of every window.
    Windows,
    /// The first line of a pane: its SIGWINCH count, read once it has not
    /// changed for longer than the 250 ms resize timer.
    Winch(&'static str),
    /// Poll until the first line of a pane contains this text (both
    /// binaries show it once the pane's program or SIGWINCH got there).
    WaitPane(&'static str, &'static str),
    /// Start the clock [`Op::Within`] reads.
    Mark,
    /// The steps since [`Op::Mark`] must have taken less than this many ms,
    /// or the whole run is void and starts again (see [`sizing`]).
    Within(u64),
}

/// Poll `read` until it has returned the same value for `quiet`.
fn stable(quiet: Duration, read: impl Fn() -> String) -> String {
    let end = Instant::now() + Duration::from_secs(20);
    let mut last = read();
    let mut since = Instant::now();
    while since.elapsed() < quiet {
        assert!(Instant::now() < end, "never settled: {last:?}");
        thread::sleep(Duration::from_millis(25));
        let now = read();
        if now != last {
            last = now;
            since = Instant::now();
        }
    }
    last
}

/// Run `ops` against vtmux and tmux 3.8 and compare what they observed.
/// Sizes and SIGWINCH counts settle without a wait condition both binaries
/// share (a pane's resize timer is 250 ms), so every observation waits
/// until what it reads has stopped changing ([`stable`]) instead of for a
/// fixed time (reviewer-25 h.py). A script that must land inside a timer
/// brackets it with [`Op::Mark`] and [`Op::Within`]: under load a run that
/// took too long proves nothing, so it is thrown away and repeated.
fn sizing(ops: &[Op]) {
    let outputs: Vec<String> = bins()
        .into_iter()
        .map(|bin| {
            (0..30)
                .find_map(|_| run_sizing(&bin, ops))
                .expect("every run took longer than its Op::Within budget")
        })
        .collect();
    assert_eq!(outputs[0], outputs[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// One run of `ops` against `bin`; `None` if an [`Op::Within`] budget ran out.
fn run_sizing(bin: &std::path::Path, ops: &[Op]) -> Option<String> {
    let w = World::new(bin.to_path_buf());
    let mut clients: Vec<Option<Attached>> = Vec::new();
    let mut out = String::new();
    let mut mark = Instant::now();
    let settle = || thread::sleep(Duration::from_millis(400));
    for op in ops {
        match op {
            Op::Run(args) | Op::RunNow(args) => {
                let o = w.cli(args);
                let _ = write!(
                    out,
                    "{args:?} {}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if matches!(op, Op::Run(_)) {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            Op::Attach(n, session, cols, rows) | Op::AttachNow(n, session, cols, rows) => {
                let a = w.attach_sized(&["attach", "-t", session], *cols, *rows);
                a.wait(|a| a.text().contains("\x1b[?1049h"));
                if clients.len() <= *n {
                    clients.resize_with(*n + 1, || None);
                }
                clients[*n] = Some(a);
                if matches!(op, Op::Attach(..)) {
                    settle();
                }
            }
            Op::Resize(n, cols, rows) => {
                clients[*n].as_mut().unwrap().resize(*cols, *rows);
                settle();
            }
            Op::Keys(n, keys) => {
                clients[*n].as_mut().unwrap().keys(keys);
                settle();
            }
            Op::Close(n) => {
                drop(clients[*n].take());
                settle();
            }
            Op::Windows => {
                settle();
                let read = || {
                    let o = w.cli(&[
                        "lsw",
                        "-a",
                        "-F",
                        "#{session_name}:#{window_index} #{window_width}x#{window_height} \
                         m=#{window_manual_width},#{window_manual_height} #{window-size}",
                    ]);
                    String::from_utf8_lossy(&o.stdout).into_owned()
                };
                out.push_str(&stable(Duration::from_millis(300), read));
            }
            Op::Winch(t) => {
                let read = || {
                    let o = w.cli(&["capture-pane", "-p", "-t", t, "-S", "0", "-E", "0"]);
                    String::from_utf8_lossy(&o.stdout).into_owned()
                };
                let seen = stable(Duration::from_millis(600), read);
                let _ = write!(out, "{t}: {seen}");
            }
            Op::WaitPane(t, text) => {
                let end = Instant::now() + Duration::from_secs(20);
                loop {
                    let o = w.cli(&["capture-pane", "-p", "-t", t, "-S", "0", "-E", "0"]);
                    if String::from_utf8_lossy(&o.stdout).contains(text) {
                        break;
                    }
                    assert!(Instant::now() < end, "{t} never showed {text:?}");
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Op::Mark => mark = Instant::now(),
            Op::Within(ms) => {
                if mark.elapsed() > Duration::from_millis(*ms) {
                    return None;
                }
            }
        }
    }
    Some(out)
}

fn wv(v: &[&'static str]) -> &'static [&'static str] {
    Vec::leak(v.to_vec())
}

/// Session `a` with windows 0 and 1 (80x24, both counting SIGWINCH).
fn two_windows() -> Vec<Op> {
    let mut new = vec!["new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24"];
    new.extend(WINCH);
    let mut neww = vec!["neww", "-d", "-t", "a:1", "-n", "x"];
    neww.extend(WINCH);
    vec![Op::Run(wv(&new)), Op::Run(wv(&neww))]
}

/// Two clients of different sizes on one session under each `window-size`,
/// with and without `aggressive-resize` (resize.c:131-276, 345-424):
/// attaching, a resize, a key (`server_client_update_latest`), select-window
/// from the CLI and a client leaving.
fn two_clients_under(policy: &'static str) {
    for aggressive in ["off", "on"] {
        let mut ops = two_windows();
        ops.extend([
            Op::Run(wv(&["set", "-g", "window-size", policy])),
            Op::Run(wv(&["setw", "-g", "aggressive-resize", aggressive])),
            Op::Attach(0, "a", 50, 15),
            Op::Windows,
            Op::Attach(1, "a", 70, 20),
            Op::Windows,
            Op::Resize(0, 60, 25),
            Op::Windows,
            Op::Keys(0, b"x"),
            Op::Windows,
            Op::Run(&["selectw", "-t", "a:1"]),
            Op::Windows,
            Op::Winch("a:1"),
            Op::Close(1),
            Op::Windows,
            Op::Winch("a:0"),
        ]);
        sizing(&ops);
    }
}
#[test]
fn window_size_latest_with_two_clients() {
    two_clients_under("latest");
}
#[test]
fn window_size_largest_with_two_clients() {
    two_clients_under("largest");
}
#[test]
fn window_size_smallest_with_two_clients() {
    two_clients_under("smallest");
}
#[test]
fn window_size_manual_with_two_clients() {
    two_clients_under("manual");
}

/// A window linked into two sessions, each with a client of its own size:
/// `aggressive-resize` only counts clients whose current window it is
/// (resize.c:349-361).
#[test]
fn aggressive_resize_counts_only_clients_looking_at_the_window() {
    for aggressive in ["off", "on"] {
        let mut ops = two_windows();
        let mut new_b = vec!["new", "-d", "-s", "b", "-n", "y"];
        new_b.extend(WINCH);
        ops.extend([
            Op::Run(wv(&new_b)),
            Op::Run(&["link-window", "-d", "-s", "a:1", "-t", "b:5"]),
            Op::Run(wv(&["setw", "-g", "aggressive-resize", aggressive])),
            Op::Run(&["set", "-g", "window-size", "smallest"]),
            Op::Attach(0, "a", 50, 15),
            Op::Attach(1, "b", 70, 20),
            Op::Windows,
            Op::Run(&["selectw", "-t", "b:5"]),
            Op::Windows,
            Op::Run(&["selectw", "-t", "a:1"]),
            Op::Windows,
            Op::Run(&["selectw", "-t", "b:0"]),
            Op::Windows,
            Op::Winch("a:1"),
        ]);
        sizing(&ops);
    }
}

/// reviewer-25 `two_clients` (parked `minor_window_size_latest_ignores_w`):
/// with two clients of different sizes on a session, select-window from
/// the CLI sizes the new window by its latest client, which is the first
/// to attach while the window was not current (w->latest is set by
/// `server_client_set_session` only for the current window).
#[test]
fn latest_client_is_per_window() {
    let mut ops = two_windows();
    ops.extend([
        Op::Attach(0, "a", 50, 15),
        Op::Attach(1, "a", 70, 20),
        Op::Windows,
        Op::Run(&["selectw", "-t", "a:1"]),
        Op::Windows,
        Op::Winch("a:1"),
        Op::Keys(0, b"x"),
        Op::Windows,
        Op::Close(1),
        Op::Windows,
        Op::Run(&["selectw", "-t", "a:0"]),
        Op::Windows,
        Op::Winch("a:0"),
    ]);
    sizing(&ops);
}

/// resize-window (cmd-resize-window.c:47-117) with a client attached:
/// -x/-y, each of -L -R -U -D with and without a count, -A and -a, the
/// manual size surviving a client resize, `window-size latest` again, and
/// the errors.
#[test]
fn resize_window_flags_with_an_attached_client() {
    let mut ops = two_windows();
    ops.extend([Op::Attach(0, "a", 50, 15), Op::Windows]);
    for args in [
        wv(&["resizew", "-t", "a:0", "-x", "30", "-y", "10"]),
        wv(&["resizew", "-t", "a:0", "-L", "5"]),
        wv(&["resizew", "-t", "a:0", "-R"]),
        wv(&["resizew", "-t", "a:0", "-U", "3"]),
        wv(&["resizew", "-t", "a:0", "-D", "20"]),
        wv(&["resizew", "-t", "a:0", "-L", "100"]),
        wv(&["resizew", "-t", "a:0", "-A"]),
        wv(&["resizew", "-t", "a:1", "-a"]),
    ] {
        ops.extend([Op::Run(args), Op::Windows]);
    }
    ops.extend([
        Op::Winch("a:0"),
        Op::Resize(0, 60, 20),
        Op::Windows,
        Op::Run(&["setw", "-t", "a:0", "window-size", "latest"]),
        Op::Windows,
        Op::Run(&["selectw", "-t", "a:1"]),
        Op::Run(&["selectw", "-t", "a:0"]),
        Op::Windows,
        Op::Winch("a:0"),
        Op::Run(&["resizew", "-t", "a:0", "-x", "0"]),
        Op::Run(&["resizew", "-t", "a:0", "-y", "10001"]),
        Op::Run(&["resizew", "-t", "a:0", "-x", "abc"]),
        Op::Run(&["resizew", "-t", "a:0", "0"]),
        Op::Run(&["resizew", "-t", "a:0", "-L", "x"]),
        Op::Run(&["resizew", "-t", "nosuch"]),
        Op::Windows,
    ]);
    sizing(&ops);
}

/// reviewer-23 `rv_zoom` (parked `minor_extra_sigwinch_to_split_panes_on`):
/// a split pane is created at the window's size and its cell's size is
/// queued (window.c:1156, 1702-1740), so sending it starts the 250 ms
/// resize timer. The size an attach brings waits for that timer
/// (server-client.c:1984-2041), and a zoom inside it replaces it: the
/// zoomed pane hears of both changes once.
///
/// The attach and the zoom must land inside the 250 ms timer that the
/// split's first size to %0 starts, which under load they sometimes cannot,
/// for tmux 3.8 as much as for us. The script therefore waits until both
/// shells have their traps (a SIGWINCH before that is lost) and %0 has heard
/// the split (the timer is running), marks the time before the split, and
/// requires the zoom within 200 ms of it: a slower run is void and repeated
/// ([`Op::Within`]), never compared (task `flake_resize_timer_window`).
#[test]
fn first_attach_after_split_waits_for_the_resize_timer() {
    let mut split = vec!["splitw", "-h", "-t", "a"];
    split.extend(WINCH);
    let mut new = vec!["new", "-d", "-s", "a", "-n", "w"];
    new.extend(WINCH);
    sizing(&[
        Op::RunNow(wv(&new)),
        Op::WaitPane("%0", "P"),
        Op::Mark,
        Op::RunNow(wv(&split)),
        Op::WaitPane("%0", "W1."),
        Op::WaitPane("%1", "P"),
        Op::AttachNow(0, "a", 40, 10),
        Op::RunNow(&["resize-pane", "-Z", "-t", "a"]),
        Op::Within(200),
        Op::Winch("%0"),
        Op::Winch("%1"),
        Op::Run(&["resize-pane", "-Z", "-t", "a"]),
        Op::Winch("%0"),
        Op::Winch("%1"),
    ]);
}

/// Entering the alternate screen flushes a pane's queued resize at once
/// (`screen_write_alternateon`, screen-write.c:3157-3166); leaving it does not.
/// The pane resizes twice 50 ms apart, so the second size waits on the
/// 250 ms resize timer, then switches screens. The script records whether
/// the last SIGWINCH came within 60 ms of the switch. To test leaving,
/// the pane enters the alternate screen first, before the resizes.
///
/// Both answers hang on wall-clock windows that a loaded host can miss, for
/// tmux 3.8 as much as for us: the second resize reaching the server late
/// (nothing queued to flush), the timer firing before the pane looks, or
/// the 60 ms loop outlasting the timer. A miss always lands on the other
/// answer, so a run that shows the wrong one is repeated: the behaviour
/// holds if some run in `TRIES` shows `want`, and a binary that does the
/// opposite never will (task `flake_resize_timer_window`).
fn alternate_flush(bin: &std::path::Path, before: &'static str, seq: &'static str) -> String {
    let script = format!(
        "w=0; trap 'w=$((w+1))' WINCH; printf '{before}P '; \
         while [ ! -e go ]; do sleep 0.01; done; printf '{seq}'; n=$w; \
         i=0; while [ $i -lt 6 ]; do sleep 0.01; i=$((i+1)); done; \
         if [ $w -gt $n ]; then printf 'FAST.'; else printf 'SLOW.'; fi; \
         sleep 100000 & while :; do wait; done"
    );
    let w = World::new(bin.to_path_buf());
    let dir = tempfile::tempdir().unwrap();
    let go = dir.path().join("go");
    let cmd = format!("cd {} && {script}", dir.path().display());
    assert!(
        w.cli(&[
            "new", "-d", "-s", "a", "-x", "60", "-y", "20", "sh", "-c", &cmd
        ])
        .status
        .success()
    );
    let mut a = w.attach_sized(&["attach", "-t", "a"], 60, 20);
    a.wait(|a| a.text().contains("\x1b[?1049h"));
    thread::sleep(Duration::from_millis(600));
    a.resize(61, 20);
    thread::sleep(Duration::from_millis(50));
    a.resize(62, 20);
    // The server has the second size once the window has it.
    let end = Instant::now() + Duration::from_secs(5);
    while String::from_utf8_lossy(
        &w.cli(&["display", "-p", "-t", "a", "#{window_width}"])
            .stdout,
    )
    .trim()
        != "62"
    {
        assert!(Instant::now() < end, "window never got the second size");
        thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(&go, b"").unwrap();
    thread::sleep(Duration::from_millis(600));
    let o = w.cli(&["capture-pane", "-p", "-t", "a"]);
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Every binary must show `want` in some run of [`alternate_flush`].
fn alternate_flush_shows(before: &'static str, seq: &'static str, want: &str) {
    const TRIES: usize = 10;
    for bin in bins() {
        let shown = (0..TRIES).any(|_| alternate_flush(&bin, before, seq).contains(want));
        assert!(
            shown,
            "{want} never shown in {TRIES} runs of {}",
            bin.display()
        );
    }
}
#[test]
fn entering_the_alternate_screen_flushes_resizes() {
    alternate_flush_shows("", "\\033[?1049h", "FAST.");
}
#[test]
// Linux only: the 60 ms SIGWINCH window is not met by macOS CI runners (task macos_attach_parity).
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "Linux-only timing/ioctl; macos_attach_parity"
)]
fn leaving_the_alternate_screen_waits_for_the_timer() {
    alternate_flush_shows("\\033[?1049h", "\\033[?1049l", "SLOW.");
}

/// The OSC 52 sequences in `text`, in order.
fn osc52(text: &str) -> Vec<String> {
    text.match_indices("\x1b]52;")
        .map(|(at, _)| {
            let rest = &text[at..];
            let end = rest.find('\x07').map_or(rest.len(), |e| e + 1);
            rest[..end].escape_debug().to_string()
        })
        .collect()
}

/// `set-buffer -w` and `load-buffer -w` send the data to the target
/// client's clipboard with OSC 52 (`tty_set_selection`, tty.c:2149-2166),
/// whatever `set-clipboard` says (cmd-set-buffer.c:127-128,
/// cmd-load-buffer.c:80-83); `load-buffer -` from stdin too; plain
/// `set-buffer` sends nothing; a terminal without `Ms` gets nothing.
#[test]
fn set_and_load_buffer_w_send_osc52() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["set", "-g", "set-clipboard", "off"]);
        let a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let file = w.home.path().join("f");
        std::fs::write(&file, b"from file\xff").unwrap();
        let mut seen = Vec::new();
        for args in [
            &["setb", "plain"][..],
            &["setb", "-w", "hello-off"],
            &["setb", "-a", "-w", "-b", "buffer0", "!"],
            &["loadb", "-w", "-b", "f", file.to_str().unwrap()],
        ] {
            let out = w.cli(args);
            seen.push(format!(
                "{} exit {:?}",
                args[..2].join(" "),
                out.status.code()
            ));
        }
        let mut child = Command::new(&w.bin)
            .args(w.args())
            .args(["loadb", "-w", "-"])
            .env_clear()
            .envs(w.env())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"stdin").unwrap();
        seen.push(format!("loadb - exit {:?}", child.wait().unwrap().code()));
        a.wait(|a| osc52(&a.text()).len() >= 4);
        thread::sleep(Duration::from_millis(200));
        seen.extend(osc52(&a.text()));
        results.push(seen);

        // Ms removed: nothing is sent, the buffer is still set.
        let w = World::new(bin);
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["set", "-g", "terminal-overrides", "xterm*:Ms@"]);
        let a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let out = w.cli(&["setb", "-w", "x"]);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(out.status.code(), Some(0));
        assert!(osc52(&a.text()).is_empty(), "{:?}", a.text());
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(
        results[0][5..],
        [
            r"\u{1b}]52;;aGVsbG8tb2Zm\u{7}",
            // buffer0 is `plain`: -a -w sends the whole buffer.
            r"\u{1b}]52;;cGxhaW4h\u{7}",
            r"\u{1b}]52;;ZnJvbSBmaWxl/w==\u{7}",
            r"\u{1b}]52;;c3RkaW4=\u{7}",
        ]
    );
}

/// The copy commands send what they copy to the clipboard of the clients
/// showing the pane (`window_copy_copy_buffer`, window-copy.c:6096-6123):
/// the pipe ones too, with the selection as the data, when `set-clipboard`
/// is not off and no `-C` is given; `append-selection` sends only what it
/// copied, and the pipe's job still runs with `set-clipboard off`.
#[test]
fn copy_pipe_sends_osc52() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        w.cli(&[
            "new",
            "-d",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "printf 'abcdef\\nghijkl\\n'; exec cat",
        ]);
        let a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let out = w.home.path().join("piped");
        let sink = format!("cat > {}", out.display());
        let mut seen = Vec::new();
        let mut step = |clip: &str, cmd: &[&str]| {
            w.cli(&["set", "-s", "set-clipboard", clip]);
            w.cli(&["copy-mode", "-t", "a"]);
            for k in ["history-top", "start-of-line", "begin-selection"] {
                w.cli(&["send-keys", "-t", "a", "-X", k]);
            }
            w.cli(&["send-keys", "-t", "a", "-X", "-N", "3", "cursor-right"]);
            let mut full = vec!["send-keys", "-t", "a", "-X"];
            full.extend_from_slice(cmd);
            let label = format!("{clip} {}", cmd[0]);
            seen.push(format!("{label} exit {:?}", w.cli(&full).status.code()));
            w.cli(&["send-keys", "-t", "a", "-X", "cancel"]);
        };
        step("on", &["copy-pipe-and-cancel", &sink]);
        step("off", &["copy-pipe-and-cancel", &sink]);
        step("external", &["copy-pipe-and-cancel", "-C", &sink]);
        step("on", &["copy-pipe-no-clear", "cat >/dev/null", "pre"]);
        step("on", &["pipe", "cat >/dev/null"]);
        step("on", &["copy-selection-and-cancel"]);
        thread::sleep(Duration::from_millis(400));
        seen.extend(osc52(&a.text()));
        seen.push(format!("piped {:?}", std::fs::read_to_string(&out)));
        results.push(seen);
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
    // abc: on (pipe), on (pipe-no-clear), on (copy-selection).
    assert_eq!(
        results[0][6..],
        [
            r"\u{1b}]52;;YWJj\u{7}",
            r"\u{1b}]52;;YWJj\u{7}",
            r"\u{1b}]52;;YWJj\u{7}",
            "piped Ok(\"abc\")".to_owned().as_str(),
        ]
    );
}

/// The owner's copy-mode-vi bindings (`~/dotfiles/tmux/.tmux.conf`
/// 137-143) typed on an attached client: `v` begins a selection, `C-v`
/// makes it a rectangle, `y` copies it and leaves copy mode; the rectangle
/// takes the same columns of each line. `M-Up`/`M-Down` (previous-prompt,
/// next-prompt) are bound; [`owner_prompt_jumps`] types them.
#[test]
fn owner_copy_mode_vi_bindings() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "printf 'abcdef\\nghijkl\\nmnopqr\\n'; exec cat",
        ]);
        a.wait(|_| display(&w, "#{cursor_y}") == "3");
        for line in [
            "setw -g mode-keys vi",
            "bind -T copy-mode-vi v send -X begin-selection",
            "bind -T copy-mode-vi y send -X copy-selection-and-cancel",
            "bind -T copy-mode-vi C-v send -X rectangle-toggle",
            "bind -T copy-mode-vi M-Up send -X previous-prompt",
            "bind -T copy-mode-vi M-Down send -X next-prompt",
        ] {
            let args: Vec<&str> = line.split(' ').collect();
            assert!(w.cli(&args).status.success(), "{line}");
        }
        for (k, cmd) in [("M-Up", "previous-prompt"), ("M-Down", "next-prompt")] {
            let out = w.cli(&["list-keys", "-T", "copy-mode-vi", k]);
            let line = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(
                line.ends_with(&format!("send-keys -X {cmd}\n")),
                "{k}: {line}"
            );
        }
        // Into copy mode, to the top left, then `l` once.
        a.keys(b"\x02[");
        a.wait(|_| display(&w, "#{pane_in_mode}") == "1");
        a.keys(b"ggl");
        a.wait(|_| display(&w, "#{copy_cursor_x},#{copy_cursor_y}") == "1,0");
        a.keys(b"v");
        a.wait(|_| display(&w, "#{selection_active}") == "1");
        a.keys(b"\x16");
        a.wait(|_| display(&w, "#{rectangle_toggle}") == "1");
        a.keys(b"jll");
        a.wait(|_| display(&w, "#{copy_cursor_x},#{copy_cursor_y}") == "3,1");
        // The selected cells are drawn in mode-style (yellow background).
        a.wait(|a| {
            let s = vtmux_vt::snapshot(&a.screen());
            let yellow = |x| s.cell(x, 1).map(|c| c.style.bg) == Some(vtmux_vt::Color::Palette(3));
            (1..4).all(yellow) && !yellow(0) && !yellow(4)
        });
        a.keys(b"y");
        a.wait(|_| display(&w, "#{pane_in_mode}") == "0");
        let buffer = w.cli(&["show-buffer"]);
        assert_eq!(String::from_utf8_lossy(&buffer.stdout), "bcd\nhij");
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// The owner's `M-Up`/`M-Down` (previous-prompt, next-prompt) typed on an
/// attached client over a shell that marks its prompts the way the
/// owner's zsh does (`~/dotfiles/zsh/.zsh/tools.zsh`: OSC 133;A in
/// PROMPT, 133;C from preexec), against tmux 3.8 and vtmux.
#[test]
fn owner_prompt_jumps() {
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "a() { printf '\\033]133;A\\007~ > '; }; \
             a; printf 'echo hi\\n\\033]133;C\\007hi\\n'; \
             a; printf 'true\\n\\033]133;C\\007'; \
             a; printf 'ls\\n\\033]133;C\\007x\\ny\\n'; a; exec cat",
        ]);
        a.wait(|_| display(&w, "#{cursor_y}") == "6");
        for line in [
            "setw -g mode-keys vi",
            "bind -T copy-mode-vi M-Up send -X previous-prompt",
            "bind -T copy-mode-vi M-Down send -X next-prompt",
        ] {
            let args: Vec<&str> = line.split(' ').collect();
            assert!(w.cli(&args).status.success(), "{line}");
        }
        a.keys(b"\x02[");
        a.wait(|_| display(&w, "#{pane_in_mode}") == "1");
        let at = |w: &World| display(w, "#{copy_cursor_y} #{copy_cursor_line}");
        // M-Up (CSI 1;3A) from the last prompt: the prompt above, then
        // the one above that; M-Down back down.
        a.keys(b"\x1b[1;3A");
        a.wait(|_| at(&w) == "3 ~ > ls");
        a.keys(b"\x1b[1;3A");
        a.wait(|_| at(&w) == "2 ~ > true");
        a.keys(b"\x1b[1;3A\x1b[1;3A");
        a.wait(|_| at(&w) == "0 ~ > echo hi");
        a.keys(b"\x1b[1;3B");
        a.wait(|_| at(&w) == "2 ~ > true");
        a.keys(b"q");
        a.wait(|_| display(&w, "#{pane_in_mode}") == "0");
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// Two clients on session `a`, one on `b`, and what each does after
/// `act` ran: `alive` or `exit N`, so both servers can be compared.
struct Pair {
    w: World,
    clients: Vec<Attached>,
    names: Vec<String>,
}

impl Pair {
    /// `extra` clients are attached to `a`, then one to `b`; every client
    /// is named by its tty.
    fn new(bin: PathBuf, third: bool) -> Self {
        let w = World::new(bin);
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["new", "-d", "-s", "b", "-n", "bw", "cat"]);
        w.cli(&["set", "-g", "status-right", "R"]);
        let mut clients = Vec::new();
        for target in ["a", "a", "b"].into_iter().take(if third { 3 } else { 2 }) {
            clients.push(w.attach(&["attach", "-t", target]));
            let want = clients.len();
            clients[want - 1].wait(|_| {
                w.cli(&["lsc", "-F", "x"])
                    .stdout
                    .split(|&b| b == b'\n')
                    .count()
                    > want
            });
        }
        let out = w.cli(&["lsc", "-F", "#{client_name}"]);
        let names = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        Self { w, clients, names }
    }

    /// Wait for the clients to settle, then report each.
    fn report(&mut self) -> Vec<String> {
        thread::sleep(Duration::from_millis(600));
        self.clients
            .iter_mut()
            .map(|c| match c.status() {
                None => "alive".to_owned(),
                Some(code) => {
                    let text = c.text();
                    let tail = text.rsplit("\x1b[?1049l").next().unwrap_or("");
                    format!("exit {code} {:?}", strip_escapes(tail).trim())
                }
            })
            .collect()
    }
}

/// `text` without CSI sequences (the teardown's mode resets).
fn strip_escapes(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn pair_results(third: bool, act: impl Fn(&mut Pair) -> Vec<String>) -> Vec<Vec<String>> {
    let results: Vec<_> = bins()
        .into_iter()
        .map(|bin| {
            let mut p = Pair::new(bin, third);
            let mut seen = act(&mut p);
            seen.extend(p.report());
            seen
        })
        .collect();
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
    results
}

fn run_line(p: &Pair, args: &[&str]) -> String {
    let out = p.w.cli(args);
    format!(
        "rc={:?} {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// cmd-detach-client.c: `-a` detaches every client but the caller, here
/// the one that pressed the key bound to it.
#[test]
fn detach_client_a_keeps_the_caller() {
    let r = pair_results(true, |p| {
        p.w.cli(&["bind", "-n", "F5", "detach-client", "-a"]);
        p.clients[0].keys(b"\x1b[15~");
        Vec::new()
    });
    assert!(r[0][0].starts_with("alive"), "{:?}", r[0]);
    assert!(r[0][1].starts_with("exit 0"), "{:?}", r[0]);
    assert!(r[0][2].starts_with("exit 0"), "{:?}", r[0]);
}

/// `-a -t`: every client but the named one, whatever its session.
#[test]
fn detach_client_a_with_target() {
    let r = pair_results(true, |p| {
        let name = p.names[1].clone();
        vec![run_line(p, &["detach-client", "-a", "-t", &name])]
    });
    assert!(r[0][1].starts_with("exit 0"), "{:?}", r[0]);
    assert_eq!(r[0][2], "alive");
    assert!(r[0][3].starts_with("exit 0"), "{:?}", r[0]);
}

#[test]
fn detach_client_t_names_one_client() {
    let r = pair_results(true, |p| {
        let name = p.names[1].clone();
        let short = format!("{}:", name.trim_start_matches("/dev/"));
        vec![
            run_line(p, &["detach-client", "-t", "nope"]),
            run_line(p, &["detach-client", "-t", &short]),
        ]
    });
    assert_eq!(r[0][0], "rc=Some(1) can't find client: nope");
    assert_eq!(
        &r[0][2..],
        ["alive", "exit 0 \"[detached (from session a)]\"", "alive"]
    );
}

/// `-s`: every client on the session, the others stay.
#[test]
fn detach_client_s_detaches_a_session() {
    let r = pair_results(true, |p| {
        vec![
            run_line(p, &["detach-client", "-s", "nosuch"]),
            run_line(p, &["detach-client", "-s", "a"]),
        ]
    });
    assert!(r[0][2].starts_with("exit 0"), "{:?}", r[0]);
    assert!(r[0][3].starts_with("exit 0"), "{:?}", r[0]);
    assert_eq!(r[0][4], "alive");
}

/// `-P`: the client says so and sends SIGHUP to its parent. The client
/// runs under a shell that records the hangup.
#[test]
fn detach_client_p_hangs_up_the_parent() {
    let r: Vec<_> = bins()
        .into_iter()
        .map(|bin| {
            let w = World::new(bin.clone());
            w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
            let mark = w.home.path().join("hup");
            let script = format!(
                "trap 'echo HUP > {}' HUP; {} -L {} -f /dev/null attach -t a; echo back",
                mark.display(),
                bin.display(),
                w.label
            );
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.args(["-c", &script]);
            let mut a = w.spawn_attached(cmd, 40, 10, &[]);
            a.wait(|_| w.cli(&["lsc"]).stdout.len() > 1);
            let before = run_line_w(&w, &["detach-client", "-t", "nope"]);
            let out = run_line_w(&w, &["detach-client", "-P"]);
            a.exited(0);
            a.wait(|a| a.text().contains("[detached and SIGHUP (from session a)]"));
            thread::sleep(Duration::from_millis(200));
            (before, out, std::fs::read_to_string(&mark).ok())
        })
        .collect();
    assert_eq!(r[0], r[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(r[0].2.as_deref(), Some("HUP\n"));
}

fn run_line_w(w: &World, args: &[&str]) -> String {
    let out = w.cli(args);
    format!(
        "rc={:?} {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// `-E`: the client is replaced by `$SHELL -c command` instead of
/// detaching: no message, the command's own exit status, `SHELL` set.
#[test]
fn detach_client_e_runs_a_command_in_place_of_the_client() {
    let r = pair_results(true, |p| {
        let name = p.names[1].clone();
        vec![run_line(
            p,
            &[
                "detach-client",
                "-E",
                "echo ran:$SHELL; exit 7",
                "-t",
                &name,
            ],
        )]
    });
    assert_eq!(r[0][1], "alive");
    assert_eq!(r[0][2], "exit 7 \"ran:/bin/sh\"");
    assert_eq!(r[0][3], "alive");
    // `-a` and `-s` take `-E` too.
    let r = pair_results(true, |p| {
        let name = p.names[0].clone();
        vec![run_line(
            p,
            &["detach-client", "-a", "-E", "echo ran; exit 3", "-t", &name],
        )]
    });
    assert_eq!(&r[0][1..], ["alive", "exit 3 \"ran\"", "exit 3 \"ran\""]);
    let r = pair_results(true, |p| {
        vec![run_line(
            p,
            &["detach-client", "-s", "a", "-E", "echo ran; exit 4"],
        )]
    });
    assert_eq!(&r[0][1..], ["exit 4 \"ran\"", "exit 4 \"ran\"", "alive"]);
}

/// A command client running `confirm-before` waits for the answer on the
/// attached client and exits 0 when it confirmed, 1 when not
/// (cmd-confirm-before.c:143-149). `command-prompt` waits too, and the
/// commands it makes run in the command client's batch, so their output
/// goes to its stdout (cmd-command-prompt.c:259-262).
#[test]
fn waiting_prompts_answer_the_command_client() {
    for bin in bins() {
        let w = World::new(bin.clone());
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "cat"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let spawn = |args: &[&str]| {
            Command::new(&bin)
                .args(w.args())
                .args(args)
                .env_clear()
                .envs(w.env())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let mut answers = Vec::new();
        for key in [&b"y"[..], b"n", b"\x1b"] {
            let child = spawn(&["confirm-before", "-p", "ok?", "display -p yes"]);
            a.wait(|a| status_row(a).contains("ok?"));
            a.keys(key);
            let out = child.wait_with_output().unwrap();
            answers.push((
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            ));
            a.wait(|a| !status_row(a).contains("ok?"));
        }
        assert_eq!(
            answers,
            [
                (Some(0), "yes\n".to_owned()),
                (Some(1), String::new()),
                (Some(1), String::new())
            ],
            "{}",
            bin.display()
        );
        let child = spawn(&["command-prompt", "-p", "say", "display -p 'got %%'"]);
        a.wait(|a| status_row(a).contains("say"));
        a.keys(b"hi\r");
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned()
            ),
            (Some(0), "got hi\n".to_owned()),
            "{}",
            bin.display()
        );
    }
}

/// Cells a stream of terminal output writes into the rectangle `(x, y, w, h)`
/// after the first `skip` bytes: printed characters and erases (ECH, EL),
/// counted by where the cursor is when they happen, even when they rewrite
/// what is already there (a diff of the screen would miss those).
fn writes_into(bytes: &[u8], skip: usize, size: (u16, u16), rect: (u16, u16, u16, u16)) -> usize {
    let mut term = PaneTerm::new(size.0, size.1, 0);
    term.feed(&bytes[..skip]);
    let inside = |col: u16, row: u16| {
        (rect.0..rect.0 + rect.2).contains(&col) && (rect.1..rect.1 + rect.3).contains(&row)
    };
    let text = String::from_utf8_lossy(&bytes[skip..]).into_owned();
    let chars: Vec<char> = text.chars().collect();
    let mut writes = 0;
    let mut at = 0;
    while at < chars.len() {
        let mut buf = [0; 4];
        let ch = chars[at];
        let (col, row) = term.cursor();
        if ch == '\x1b' && chars.get(at + 1) == Some(&'[') {
            // CSI: parameters, then a final byte in 0x40..=0x7e.
            let mut end_at = at + 2;
            while end_at < chars.len() && !('\x40'..='\x7e').contains(&chars[end_at]) {
                end_at += 1;
            }
            let params: String = chars[at + 2..end_at].iter().collect();
            let n: u16 = params
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .split(';')
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or(1);
            match chars.get(end_at) {
                Some('X') => writes += (col..col + n).filter(|&c| inside(c, row)).count(),
                Some('K') => {
                    let range = match params.as_str() {
                        "1" => 0..col + 1,
                        "2" => 0..size.0,
                        _ => col..size.0,
                    };
                    writes += range.filter(|&c| inside(c, row)).count();
                }
                _ => {}
            }
            let end = (end_at + 1).min(chars.len());
            let seq: String = chars[at..end].iter().collect();
            term.feed(seq.as_bytes());
            at = end;
        } else if ch == '\x1b' && chars.get(at + 1) == Some(&']') {
            // OSC: up to BEL or ST.
            let mut end_at = at + 2;
            while end_at < chars.len() && chars[end_at] != '\x07' && chars[end_at] != '\x1b' {
                end_at += 1;
            }
            let end = (if chars.get(end_at) == Some(&'\x1b') {
                end_at + 2
            } else {
                end_at + 1
            })
            .min(chars.len());
            let seq: String = chars[at..end].iter().collect();
            term.feed(seq.as_bytes());
            at = end;
        } else if ch.is_control() {
            term.feed(ch.encode_utf8(&mut buf).as_bytes());
            at += 1;
        } else {
            writes += usize::from(inside(col, row));
            term.feed(ch.encode_utf8(&mut buf).as_bytes());
            at += 1;
        }
    }
    writes
}

/// tty.c:1425-1455, 2268-2270: tmux trims pane draws to the cells outside
/// a popup, so pane output never lands in the popup. What it writes there
/// are its own repaints of the popup, on every full redraw of the client
/// (here the start timer, tty.c `tty_start_timer_callback`, 5 s after the
/// attach, which lands inside the flood; 72 border writes in a 3000-line
/// flood when it did). vtmux composes the popup into one frame and writes
/// nothing there unless the popup itself changes.
#[test]
fn pane_flood_under_a_popup_writes_nothing_into_it() {
    const SIZE: (u16, u16) = (80, 24);
    // x, y, w, h of the popup on the client's screen.
    const RECT: (u16, u16, u16, u16) = (30, 5, 30, 10);
    // Output every 10 ms for as long as the pane lives.
    let flood = "i=0; while :; do seq $i $((i+40)); i=$((i+1)); sleep 0.01; done";
    let mut counts = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        let start = Instant::now();
        let a = w.attach_sized(
            &["new", "-s", "a", "-n", "w", "sh", "-c", flood],
            SIZE.0,
            SIZE.1,
        );
        a.wait(|a| pane_text(a).contains("[a] 0:w"));
        // A CLI display-popup waits until the popup closes (CMD_RETURN_WAIT).
        let mut popup = Command::new(&w.bin)
            .args(w.args())
            .args([
                "display-popup",
                "-t",
                "a",
                "-w",
                "30",
                "-h",
                "10",
                "-x",
                "30",
                "-y",
                "15",
            ])
            .args(["-T", "popup", "exec cat"])
            .env_clear()
            .envs(w.env())
            .spawn()
            .unwrap();
        a.wait(|a| pane_text(a).contains("popup"));
        thread::sleep(Duration::from_millis(500));
        let skip = a.bytes.lock().unwrap().len();
        // Past the start timer (5 s) with the flood still running.
        while start.elapsed() < Duration::from_millis(7500) {
            thread::sleep(Duration::from_millis(50));
        }
        let bytes = a.bytes.lock().unwrap().clone();
        assert!(
            bytes.len() > skip + 20_000,
            "the flood is running: {} {skip}",
            bytes.len()
        );
        // The popup is intact at the end.
        let screen = pane_text(&a);
        let rows: Vec<&str> = screen.lines().collect();
        let row = |y: u16| -> String {
            rows[usize::from(y)]
                .chars()
                .skip(usize::from(RECT.0))
                .take(usize::from(RECT.2))
                .collect()
        };
        assert!(row(RECT.1).starts_with('┌'), "{screen}");
        assert!(row(RECT.1 + RECT.3 - 1).starts_with('└'), "{screen}");
        counts.push(writes_into(&bytes, skip, SIZE, RECT));
        let _ = w.cli(&["display-popup", "-C", "-t", "a"]);
        let _ = popup.wait();
    }
    eprintln!(
        "writes into the popup: vtmux {}, reference {}",
        counts[0], counts[1]
    );
    assert_eq!(counts[0], 0, "vtmux wrote into the popup during the flood");
    assert!(
        counts[1] > 0,
        "the reference repaints its popup during a flood"
    );
}

/// The client names `list-clients` gives, session by session.
fn client_name(w: &World, session: &str) -> String {
    let out = w.cli(&["lsc", "-t", session, "-F", "#{client_name}"]);
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// cmd-switch-client.c:48-163 with `CMD_CLIENT_CFLAG`: `-c` picks the client
/// that moves (by name or tty, `/dev/` optional, a trailing colon dropped)
/// and the other stays; an unknown name is an error before anything else;
/// `-F` is a flag that does nothing (3.8 reads nothing from it).
#[test]
fn switch_client_c_moves_the_named_client() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["new", "-d", "-s", "b", "-n", "bw", "cat"]);
        w.cli(&["new", "-d", "-s", "c", "-n", "cw", "cat"]);
        w.cli(&["set", "-g", "status-right", "R"]);
        let a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let b = w.attach(&["attach", "-t", "b"]);
        b.wait(|b| status_row(b).contains("[b]"));
        let (na, nb) = (client_name(&w, "a"), client_name(&w, "b"));
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let run = |args: &[&str]| {
            let out = w.cli(args);
            // Client names are ttys, which differ run to run.
            let args = format!("{args:?}")
                .replace(&na, "<A>")
                .replace(&nb, "<B>")
                .replace(nb.trim_start_matches("/dev/"), "<B>");
            seen.borrow_mut().push(format!(
                "{args} exit {:?} out {:?} err {:?}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        };
        run(&["switch-client", "-c", &na, "-t", "c"]);
        a.wait(|a| status_row(a).contains("[c]"));
        thread::sleep(Duration::from_millis(200));
        seen.borrow_mut()
            .push(format!("{} / {}", status_row(&a), status_row(&b)));
        let short = format!("{}:", nb.trim_start_matches("/dev/"));
        run(&["switch-client", "-c", &short, "-t", "c"]);
        b.wait(|b| status_row(b).contains("[c]"));
        run(&["switch-client", "-c", "nosuch", "-t", "a"]);
        run(&["switch-client", "-c", "nosuch", "-r"]);
        // -F takes no argument: `-F x` is a stray argument.
        run(&["switch-client", "-c", &na, "-F", "x", "-t", "a"]);
        run(&["switch-client", "-c", &na, "-F", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        run(&["switch-client", "-c", &nb, "-n"]);
        run(&["switch-client", "-c", &nb, "-T", "nope"]);
        run(&["switch-client", "-c", &nb, "-T", "prefix"]);
        run(&["lsc", "-F", "#{client_session} #{client_key_table}"]);
        results.push(seen.into_inner());
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// `switch-client -r` toggles `CLIENT_READONLY|CLIENT_IGNORESIZE`
/// (cmd-switch-client.c:82-95): `#{client_readonly}` and the flag list show
/// it, keys reach neither the pane nor the key tables' commands (a binding
/// of other than read-only commands says `client is read-only`; `send-keys`
/// and `switch-client` bindings run), typing does nothing, and a second `-r`
/// restores it. A read-only client does not size windows while another
/// client without the flag is attached (resize.c:95-107).
#[test]
fn switch_client_r_makes_a_client_read_only() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["set", "-g", "display-time", "0"]);
        w.cli(&["bind", "-n", "M-d", "display-message", "ran-display"]);
        w.cli(&["bind", "-n", "M-s", "send-keys", "-l", "viabinding"]);
        w.cli(&[
            "bind",
            "-n",
            "M-m",
            "display-message",
            "one",
            ";",
            "send-keys",
            "x",
        ]);
        let mut a = w.attach_sized(&["attach", "-t", "a"], 60, 10);
        a.wait(|a| status_row(a).contains("[a]"));
        let name = client_name(&w, "a");
        let flags = |w: &World| {
            let out = w.cli(&["lsc", "-F", "#{client_readonly}|#{client_flags}"]);
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let mut seen = vec![flags(&w)];
        let out = w.cli(&["switch-client", "-c", &name, "-r"]);
        seen.push(format!("exit {:?}", out.status.code()));
        seen.push(flags(&w));
        // Typed text and bindings.
        a.keys(b"typed\r");
        thread::sleep(Duration::from_millis(300));
        a.keys(b"\x1bd");
        thread::sleep(Duration::from_millis(300));
        seen.push(status_row(&a));
        a.keys(b"\x1bs");
        a.keys(b"\x1bm");
        thread::sleep(Duration::from_millis(300));
        seen.push(status_row(&a));
        let cap = w.cli(&["capture-pane", "-p", "-t", "a"]);
        seen.push(String::from_utf8_lossy(&cap.stdout).trim().to_owned());
        // Commands from the command line: refused unless read-only ones.
        let out = w.cli(&["display-message", "-p", "no"]);
        seen.push(format!(
            "exit {:?} {:?}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
        // The same client sends commands: a client that is not read-only
        // (this CLI client) is unaffected; the attached one stays read-only.
        let out = w.cli(&["send-keys", "-c", &name, "-t", "a", "refused", "Enter"]);
        seen.push(format!(
            "exit {:?} {:?}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
        let out = w.cli(&["send-keys", "-c", &name, "-t", "a", "-X", "cancel"]);
        seen.push(format!(
            "exit {:?} {:?}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
        let out = w.cli(&["send-keys", "-c", "nosuch", "-t", "a", "ok"]);
        seen.push(format!("exit {:?}", out.status.code()));
        let out = w.cli(&["switch-client", "-c", &name, "-r"]);
        seen.push(format!("exit {:?}", out.status.code()));
        seen.push(flags(&w));
        a.keys(b"back\r");
        thread::sleep(Duration::from_millis(300));
        let cap = w.cli(&["capture-pane", "-p", "-t", "a"]);
        seen.push(String::from_utf8_lossy(&cap.stdout).trim().to_owned());
        results.push(seen);
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// `attach -r` starts a client read-only and `-f` sets flags before it takes
/// the session (cmd-attach-session.c:108-121), `new -f` too
/// (cmd-new-session.c:335-336).
#[test]
fn attach_r_and_f_set_client_flags() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["set", "-g", "status-right", "R"]);
        let a = w.attach(&["attach", "-r", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let out = w.cli(&["lsc", "-F", "#{client_readonly}|#{client_flags}"]);
        let mut seen = vec![String::from_utf8_lossy(&out.stdout).trim().to_owned()];
        drop(a);
        let w2 = World::new(bin.clone());
        w2.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w2.cli(&["set", "-g", "status-right", "R"]);
        let b = w2.attach(&[
            "attach",
            "-f",
            "ignore-size,no-detach-on-destroy",
            "-t",
            "a",
        ]);
        b.wait(|b| status_row(b).contains("[a]"));
        let out = w2.cli(&["lsc", "-F", "#{client_readonly}|#{client_flags}"]);
        seen.push(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        drop(b);
        let w3 = World::new(bin.clone());
        let c = w3.attach(&["new", "-f", "read-only", "-s", "n", "-n", "w", "cat"]);
        c.wait(|c| status_row(c).contains("[n]"));
        let out = w3.cli(&["lsc", "-F", "#{client_readonly}|#{client_flags}"]);
        seen.push(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        results.push(seen);
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// cmd-refresh-client.c:196-284 on an attached terminal client: `-A`, `-B`
/// and `-C` are for control clients; plain `refresh-client` and `-S` redraw
/// (here observed as the status line showing a changed option), `-f` and
/// `-F` set flags, the adjustment of the pan flags is checked first, `-t`
/// names the client, and `-r` is accepted.
#[test]
fn refresh_client_on_a_terminal_client() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&["new", "-d", "-s", "a", "-n", "w", "cat"]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["set", "-g", "status-interval", "0"]);
        let a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).contains("[a]"));
        let name = client_name(&w, "a");
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let run = |args: &[&str]| {
            let out = w.cli(args);
            // The client's name is its tty, which differs run to run.
            let args = format!("{args:?}").replace(&name, "<CLIENT>");
            seen.borrow_mut().push(format!(
                "{args} exit {:?} out {:?} err {:?}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        };
        run(&["refresh-client", "-A", "%0:on"]);
        run(&["refresh-client", "-B", "x"]);
        run(&["refresh-client", "-C", "80x24"]);
        run(&["refresh-client", "-C", "@0:80x24"]);
        run(&["refresh-client", "-t", "nosuch"]);
        run(&["refresh-client", "-t", &name, "-f", "ignore-size"]);
        run(&["lsc", "-F", "#{client_flags}"]);
        run(&["refresh-client", "-t", &name, "-F", "!ignore-size"]);
        run(&["lsc", "-F", "#{client_flags}"]);
        run(&["refresh-client", "-L", "x"]);
        run(&["refresh-client", "-U", "0"]);
        run(&["refresh-client", "-r", "%0:rgb:ffff/0000/0000"]);
        // The status line after option changes, with and without a redraw
        // being asked for (set-option already asks for one).
        w.cli(&["set", "-g", "status-left", "[L1] "]);
        thread::sleep(Duration::from_millis(300));
        seen.borrow_mut()
            .push(format!("before {}", status_row(&a).contains("L1")));
        run(&["refresh-client", "-S"]);
        a.wait(|a| status_row(a).contains("L1"));
        seen.borrow_mut()
            .push(format!("after -S {}", status_row(&a).contains("L1")));
        w.cli(&["set", "-g", "status-left", "[L2] "]);
        run(&["refresh-client"]);
        a.wait(|a| status_row(a).contains("L2"));
        seen.borrow_mut()
            .push(format!("after plain {}", status_row(&a).contains("L2")));
        results.push(seen.into_inner());
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// window-copy.c:3921-3933: a read-only client's copy mode commands that
/// are not `WINDOW_COPY_CMD_FLAG_READONLY` are refused with the status
/// message `client is read-only` and change nothing: no selection from
/// `C-Space`, no buffer or clipboard write from `M-w`
/// (`copy-pipe-and-cancel`), and `send-keys -X` with the client named by
/// `-c` refuses `begin-selection`, `clear-selection`, `cancel` and a pipe
/// while the movement commands still run. Both binaries, same script.
#[test]
fn read_only_client_cannot_run_mutating_copy_commands() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&[
            "new",
            "-d",
            "-s",
            "a",
            "-n",
            "w",
            "-x",
            "40",
            "-y",
            "8",
            "sh",
            "-c",
            "printf 'one\\ntwo\\nthree\\n'; exec cat",
        ]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["set", "-g", "display-time", "0"]);
        w.cli(&["set", "-g", "set-clipboard", "on"]);
        let mut a = w.attach_sized(&["attach", "-t", "a"], 40, 9);
        a.wait(|a| status_row(a).contains("[a]"));
        let name = client_name(&w, "a");
        thread::sleep(Duration::from_millis(300));
        let mut seen = Vec::new();
        let state = |w: &World| {
            let out = w.cli(&[
                "display-message", "-p", "-t", "a",
                "mode=#{pane_in_mode} sel=#{selection_present} y=#{copy_cursor_y} buffers=#{?buffer_size,yes,no}",
            ]);
            let buffers = w.cli(&["list-buffers", "-F", "#{buffer_name}"]);
            format!(
                "{} / [{}]",
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&buffers.stdout).trim()
            )
        };
        let run = |args: &[&str]| {
            let out = w.cli(args);
            format!(
                "exit {:?} {:?}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )
        };
        w.cli(&["switch-client", "-c", &name, "-r"]);
        w.cli(&["copy-mode", "-t", "a"]);
        seen.push(state(&w));
        // 1) C-Space (begin-selection), then Down.
        a.keys(b"\x00");
        thread::sleep(Duration::from_millis(300));
        seen.push(status_row(&a));
        a.keys(b"\x1b[B");
        thread::sleep(Duration::from_millis(300));
        seen.push(state(&w));
        // 2) M-w (copy-pipe-and-cancel).
        a.keys(b"\x1bw");
        thread::sleep(Duration::from_millis(300));
        seen.push(state(&w));
        seen.push(format!("osc52 {}", a.text().contains("\x1b]52;")));
        // 3) send-keys -X with the read-only client.
        for cmd in [
            &["begin-selection"][..],
            &["clear-selection"],
            &["copy-selection-and-cancel"],
            &["copy-pipe", "cat"],
            &["search-forward", "two"],
            &["cancel"],
        ] {
            let mut args = vec!["send-keys", "-c", name.as_str(), "-t", "a", "-X"];
            args.extend_from_slice(cmd);
            seen.push(format!("{cmd:?} {}", run(&args)));
            seen.push(state(&w));
        }
        // Movement commands are allowed.
        for cmd in [&["cursor-up"][..], &["history-top"], &["cursor-down"]] {
            let mut args = vec!["send-keys", "-c", name.as_str(), "-t", "a", "-X"];
            args.extend_from_slice(cmd);
            seen.push(format!("{cmd:?} {}", run(&args)));
            seen.push(state(&w));
        }
        // Another client (the CLI, not read-only) is not refused.
        seen.push(run(&["send-keys", "-t", "a", "-X", "begin-selection"]));
        seen.push(state(&w));
        seen.push(run(&["send-keys", "-t", "a", "-X", "cancel"]));
        seen.push(state(&w));
        results.push(seen);
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
    // The refusals did something visible on the reference too.
    assert!(
        results[0].iter().any(|l| l.contains("client is read-only")),
        "{:?}",
        results[0]
    );
}

/// cmd-switch-client.c:66-74: with a session (or no) target, the lookup is
/// `CMD_FIND_PREFER_UNATTACHED` (cmd-find.c:134-148): a bare
/// `switch-client -c <tty>` goes to the best session no client is attached
/// to, not the most recently active one, and when every session has a
/// client it is the most recently active. `-t` names one; `-t @N` and
/// `-t %N` pick among the sessions linking it the same way.
#[test]
fn switch_client_prefers_an_unattached_session() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        for s in ["a", "b", "c", "d"] {
            w.cli(&["new", "-d", "-s", s, "-n", "w", "cat"]);
            thread::sleep(Duration::from_millis(20));
        }
        w.cli(&["set", "-g", "status-right", "R"]);
        // A second session links window @1 (the one in `b`).
        w.cli(&["link-window", "-s", "b:0", "-t", "c:5"]);
        let a = w.attach(&["attach", "-t", "b"]);
        a.wait(|a| status_row(a).contains("[b]"));
        let name = client_name(&w, "b");
        let sessions = |w: &World| {
            let out = w.cli(&["lsc", "-F", "#{client_session}"]);
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut seen = Vec::new();
        for args in [
            vec!["switch-client", "-c", &name],
            vec!["switch-client", "-c", &name],
            vec!["switch-client", "-c", &name, "-r"],
            vec!["switch-client", "-c", &name, "-t", "a"],
            vec!["switch-client", "-c", &name],
            vec!["switch-client", "-c", &name, "-t", "@1"],
            vec!["switch-client", "-c", &name, "-t", "%0"],
            vec!["switch-client", "-c", &name, "-t", "d:0"],
            vec!["switch-client", "-c", &name],
        ] {
            let out = w.cli(&args);
            thread::sleep(Duration::from_millis(100));
            seen.push(format!(
                "{} exit {:?} {:?} -> {}",
                args[3..].join(" "),
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim(),
                sessions(&w)
            ));
        }
        // With a client on every session the most recently active wins.
        let others: Vec<Attached> = ["a", "c", "d"]
            .iter()
            .map(|s| {
                let c = w.attach(&["attach", "-t", s]);
                c.wait(|c| status_row(c).contains(&format!("[{s}]")));
                c
            })
            .collect();
        let out = w.cli(&["switch-client", "-c", &name]);
        thread::sleep(Duration::from_millis(100));
        seen.push(format!(
            "all attached exit {:?} -> {}",
            out.status.code(),
            sessions(&w)
        ));
        drop(others);
        results.push(seen);
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
}
