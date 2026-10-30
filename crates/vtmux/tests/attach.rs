//! Real binary under a PTY; all runs have private labels and pinned env.
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
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
            ("TERM", "xterm-256color".into()),
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
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(&self.bin);
        cmd.args(self.args());
        cmd.args(args);
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
        let _ = self.cli(&["kill-server"]);
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
