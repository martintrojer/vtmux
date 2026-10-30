//! Real binary under a PTY; all runs have private labels and pinned env.
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
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
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let paused = Arc::new(AtomicBool::new(false));
        let pause = Arc::clone(&paused);
        thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                // A stalled terminal: stop draining the master.
                while pause.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
                let Ok(n) = reader.read(&mut buf) else { break };
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().unwrap();
        Attached {
            master: pair.master,
            writer,
            child,
            bytes,
            paused,
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
                assert_eq!(s.exit_code(), code);
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
