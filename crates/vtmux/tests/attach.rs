//! Real binary under a PTY; all runs have private labels and pinned env.
mod common;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::cell::Cell;
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
    /// `TMUX_TMPDIR` of every command and client: a server leaves its socket
    /// file behind when it exits, so the world owns the directory and removes
    /// it on drop. Never the user's real `/tmp/tmux-<uid>/`.
    sockets: tempfile::TempDir,
    /// `TERM` for the client.
    term: &'static str,
    /// The client terminal's size in pixels (`ws_xpixel`, `ws_ypixel`).
    pixels: (u16, u16),
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
            // Under /tmp, not $TMPDIR: the socket path (dir, `tmux-<uid>`,
            // label) must fit `sun_path`, 104 bytes on macOS, where
            // $TMPDIR is /var/folders/....
            sockets: tempfile::Builder::new()
                .prefix("vta")
                .tempdir_in("/tmp")
                .unwrap(),
            term: "xterm-256color",
            pixels: (0, 0),
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
            ("TMUX_TMPDIR", self.sockets.path().display().to_string()),
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
                pixel_width: self.pixels.0,
                pixel_height: self.pixels.1,
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
        self.resize_pixels(cols, rows, (0, 0));
    }
    /// `resize` with the terminal's size in pixels.
    fn resize_pixels(&mut self, cols: u16, rows: u16, (x, y): (u16, u16)) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: x,
                pixel_height: y,
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
        self.wait_labelled("", limit, predicate);
    }
    /// `wait_for`, naming the binary under test in the timeout message.
    fn wait_labelled(&self, what: &str, limit: Duration, predicate: impl Fn(&Self) -> bool) {
        let end = Instant::now() + limit;
        while !predicate(self) {
            assert!(
                Instant::now() < end,
                "timeout{}{what}: {:?}",
                if what.is_empty() { "" } else { " " },
                self.text()
            );
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
/// A world owns its socket directory: the server's socket and an attached
/// client's both live in it, it is short enough for `sun_path`, and it goes
/// away with the world. A server leaves its socket file behind on exit (tmux
/// does too), so without this every test strands one in the user's real
/// `/tmp/tmux-<uid>/`.
#[test]
fn world_sockets_live_in_a_private_dir() {
    for bin in bins() {
        let w = World::new(bin);
        let dir = w.sockets.path().canonicalize().unwrap();
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "cat", "-"]);
        a.wait(|_| w.cli(&["lsc"]).stdout.len() > 1);
        let out = w.cli(&["display", "-p", "#{socket_path}"]);
        let sock = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        assert!(
            sock.starts_with(&dir),
            "{} not in {}",
            sock.display(),
            dir.display()
        );
        assert!(sock.exists(), "{}", sock.display());
        // tmux fails at sizeof sun_path (client.c:114-116); macOS has 104.
        assert!(sock.as_os_str().len() < 100, "{}", sock.display());
        a.keys(b"\x02d");
        a.exited(0);
        drop(w);
        assert!(!dir.exists(), "{} outlived its world", dir.display());
    }
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
        let slave = || {
            open(
                slave_path.as_c_str(),
                OFlags::RDWR | OFlags::NOCTTY,
                Mode::empty(),
            )
            .unwrap()
        };
        // macOS refuses TIOCSWINSZ on a master whose slave was never
        // opened (ENOTTY): open one first, then set the size.
        let (stdin, stdout, stderr) = (slave(), slave(), slave());
        tcsetwinsize(
            &master,
            Winsize {
                ws_row: 10,
                ws_col: 40,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        let mut child = Command::new(&w.bin)
            .args(w.args())
            .args(["attach", "-t", "a"])
            .env_clear()
            .envs(w.env())
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
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

/// input.c `input_add_request` / `input_request_reply`: a palette entry
/// nobody set, and the clipboard under `get-clipboard request` or `both`,
/// are asked of the attached terminal (OSC 4 always with ST, OSC 52 through
/// `Ms`), and its reply goes to the pane, ended as the query was; `both`
/// also keeps it as a paste buffer, `request` does not.
#[test]
fn pane_requests_reach_the_terminal_and_back() {
    for bin in bins() {
        let w = World::new(bin.clone());
        let out = w.home.path().join("out");
        let a = &mut w.attach(&["new", "-s", "a", "-n", "w", "cat"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        let query = |a: &mut Attached,
                     w: &World,
                     setup: &str,
                     q: &str,
                     needle: &[u8],
                     reply: &[u8]| {
            let _ = std::fs::remove_file(&out);
            let seen = a.text().len();
            let script = format!(
                "stty raw -echo min 0 time 20; printf '{q}'; dd bs=1 count=64 2>/dev/null | cat -v > {}; exec cat",
                out.display()
            );
            for args in setup.split(';').filter(|c| !c.is_empty()) {
                let args: Vec<&str> = args.split(' ').collect();
                assert!(w.cli(&args).status.success());
            }
            assert!(
                w.cli(&["respawn-pane", "-k", "-t", "a", &script])
                    .status
                    .success()
            );
            let needle = String::from_utf8_lossy(needle).into_owned();
            a.wait_labelled(
                &format!("{} {needle:?}", bin.display()),
                Duration::from_secs(5),
                |a| a.text()[seen..].contains(&needle),
            );
            a.keys(reply);
            let end = Instant::now() + Duration::from_secs(5);
            while std::fs::metadata(&out).map_or(true, |m| m.len() == 0) && Instant::now() < end {
                thread::sleep(Duration::from_millis(20));
            }
            thread::sleep(Duration::from_millis(100));
            std::fs::read_to_string(&out).unwrap_or_default()
        };
        assert_eq!(
            query(
                a,
                &w,
                "",
                r"\033]4;99;?\007",
                b"\x1b]4;99;?\x1b\\",
                b"\x1b]4;99;rgb:0101/0202/0303\x1b\\"
            ),
            "^[]4;99;rgb:0101/0202/0303^G",
            "{}",
            bin.display()
        );
        assert_eq!(
            query(
                a,
                &w,
                "set -s set-clipboard on;set -s get-clipboard request",
                r"\033]52;c;?\033\\",
                b"\x1b]52;;?\x07",
                b"\x1b]52;c;UmVxdWVzdA==\x1b\\"
            ),
            r"^[]52;c;UmVxdWVzdA==^[\",
            "{}",
            bin.display()
        );
        let buffers = |w: &World| {
            String::from_utf8_lossy(&w.cli(&["list-buffers", "-F", "#{buffer_sample}"]).stdout)
                .into_owned()
        };
        assert_eq!(buffers(&w), "", "{}", bin.display());
        assert_eq!(
            query(
                a,
                &w,
                "set -s get-clipboard both",
                r"\033]52;c;?\007",
                b"\x1b]52;;?\x07",
                b"\x1b]52;c;Qm90aA==\x07"
            ),
            "^[]52;c;Qm90aA==^G",
            "{}",
            bin.display()
        );
        assert_eq!(buffers(&w), "Both\n", "{}", bin.display());
    }
}

/// tty-keys.c:990-1012: while terminal queries are outstanding a partial
/// sequence waits at least 500ms, so a reply split over a slow link is
/// still swallowed. DECRPM goes last: it ends vtmux's query window.
#[test]
fn split_terminal_replies_are_swallowed() {
    for bin in bins() {
        let w = World::new(bin);
        // The pane signals once `stty raw` has run: a `|` typed earlier sits
        // in the cooked line and is echoed and then read by `cat`, twice.
        let mut a = w.attach(&[
            "new",
            "-s",
            "a",
            "sh",
            "-c",
            "stty raw -echo; printf R; exec cat -v",
        ]);
        // The last startup query (OSC 11) is out: replies are now expected.
        a.wait(|a| a.text().contains("\x1b]11;?\x1b\\"));
        a.wait(|_| pane_line(&w) == "R");
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
        a.wait(|_| pane_line(&w) == "R|||");
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

/// D-020 (not copied from tmux): `send-keys -K` keys reach the pane. tmux
/// 3.8 leaves `KEYC_SENT` on them (cmd-send-keys.c:79) and `input_key` then
/// drops most (mode 1 drops even `a`); vtmux encodes them as for a typed key
/// (the `send_keys_in_each_key_mode_and_format` unit test pins that table
/// against tmux). Standard mode, modifyOtherKeys 1 and 2, default
/// `extended-keys-format`. vtmux only: the reference gives `||||||` there.
#[test]
fn send_keys_k_delivers_keys_to_the_pane() {
    const KEYS: [&str; 7] = ["C-F3", "BTab", "Escape", "F3", "C-Up", "a", "Tab"];
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_vtmux"));
    for (mode, want_mode, want) in [
        ("", "VT10x", "\x1b[1;5R|\x1b[Z|\x1b|\x1bOR|\x1b[1;5A|a|\t"),
        (
            r"\033[>4;1m",
            "Ext 1",
            "\x1b[1;5R|\x1b[Z|\x1b|\x1bOR|\x1b[1;5A|a|\t",
        ),
        (
            r"\033[>4;2m",
            "Ext 2",
            "\x1b[1;5R|\x1b[27;2;9~|\x1b|\x1bOR|\x1b[1;5A|a|\t",
        ),
    ] {
        let w = World::new(bin.clone());
        let out = w.home.path().join("out");
        let script = format!(
            "stty raw -echo; printf '{mode}'; printf R; exec cat > {}",
            out.display()
        );
        let a = w.attach(&[
            "new",
            "-s",
            "a",
            "sh",
            "-c",
            &script,
            ";",
            "set",
            "-g",
            "extended-keys",
            "on",
        ]);
        // The pane's own text: the status line may hold an `R` (host name).
        a.wait(|_| pane_line(&w) == "R");
        a.wait(|_| {
            let shown = w.cli(&["display", "-p", "-t", "a", "#{pane_key_mode}"]);
            String::from_utf8_lossy(&shown.stdout).trim() == want_mode
        });
        let client = String::from_utf8_lossy(&w.cli(&["lsc", "-F", "#{client_name}"]).stdout)
            .trim()
            .to_owned();
        for key in KEYS {
            assert!(
                w.cli(&["send-keys", "-K", "-c", &client, key])
                    .status
                    .success()
            );
            // The key is handled as a queue item after the command.
            thread::sleep(Duration::from_millis(100));
            assert!(
                w.cli(&["send-keys", "-t", "a", "-H", "7c"])
                    .status
                    .success()
            );
        }
        assert!(
            w.cli(&["send-keys", "-t", "a", "-H", "2e"])
                .status
                .success()
        );
        let start = Instant::now();
        let bytes = loop {
            let bytes = std::fs::read(&out).unwrap_or_default();
            if bytes.ends_with(b".") {
                break bytes;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{want_mode}: {bytes:?}"
            );
            thread::sleep(Duration::from_millis(20));
        };
        let keys: Vec<u8> = bytes[..bytes.len() - 1].to_vec();
        // Each key is followed by the `|` mark, so the last one is too.
        let got = String::from_utf8_lossy(&keys);
        assert_eq!(got, format!("{want}|"), "{want_mode}");
    }
}

/// server-client.c:1503-1504, 1699-1704: inside a bracketed paste every key
/// goes to the pane as the bytes the terminal sent (`window_pane_paste`),
/// not encoded again: a byte that starts no UTF-8 and an arrow in
/// application cursor mode arrive as typed. The markers go only to a pane
/// that asked for them, and a pane in a mode gets nothing
/// (window.c:2055-2056): the pasted `q` leaves copy mode alone. Against
/// tmux 3.8.
#[test]
fn pasted_bytes_reach_the_pane_as_sent() {
    for bin in bins() {
        let w = World::new(bin.clone());
        let out = w.home.path().join("out");
        let script = format!(
            "printf '\\033[?1h'; stty raw -echo; printf R; exec cat > {}",
            out.display()
        );
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "sh", "-c", &script]);
        a.wait(|_| pane_line(&w) == "R");
        let read_until = |end: &[u8]| {
            let start = Instant::now();
            loop {
                let bytes = std::fs::read(&out).unwrap_or_default();
                if bytes.ends_with(end) {
                    break bytes;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "{}: {bytes:?}",
                    bin.display()
                );
                thread::sleep(Duration::from_millis(20));
            }
        };
        a.keys(b"\x1b[200~a\xe6b\xf0\x9f\x98\x80c\x1b[A\x1b[201~.");
        let got = read_until(b".");
        assert_eq!(got, b"a\xe6b\xf0\x9f\x98\x80c\x1b[A.", "{}", bin.display());
        assert!(w.cli(&["copy-mode", "-t", "a"]).status.success());
        a.keys(b"\x1b[200~q\x1b[201~q,");
        let got = read_until(b",");
        assert_eq!(got, b"a\xe6b\xf0\x9f\x98\x80c\x1b[A.,", "{}", bin.display());
    }
}

/// A typed `M-BSpace` (CSI-u or xterm-modified, so `KEYC_IMPLIED_META`) with
/// a named-key `backspace` option: input-keys.c:612 keeps the typed key's
/// flags, so Meta is not an Escape before the option's key, and a keypad
/// key or `BTab` (no Meta rows, not looked up again without Meta) sends
/// nothing (input-keys.c:655-656, 681-685). An Escape before a plain DEL
/// is (no implied Meta). Raw bytes through an attached client, in every
/// key mode and format, against tmux 3.8.
#[test]
fn typed_meta_bspace_follows_the_backspace_option() {
    const INPUTS: [&[u8]; 12] = [
        b"\x7f",
        b"\x1b\x7f",
        b"\x1b[127;3u",
        b"\x1b[127;4u",
        b"\x1b[127;7u",
        b"\x1b[127;8u",
        b"\x1b[127;5u",
        b"\x1b[27;3;127~",
        b"\x1b[27;4;127~",
        b"\x1b[27;7;127~",
        b"\x1b[27;8;127~",
        b"\x1b[27;2;127~",
    ];
    let mut seen: Vec<Vec<String>> = Vec::new();
    for bin in bins() {
        let mut rows = Vec::new();
        for option in ["C-h", "Up", "F1", "Home", "KP1", "BTab"] {
            for mode in ["", r"\033[>4;1m", r"\033[>4;2m"] {
                for format in ["csi-u", "xterm"] {
                    let w = World::new(bin.clone());
                    let out = w.home.path().join("out");
                    let script = format!(
                        "stty raw -echo; printf '{mode}'; printf R; exec cat > {}",
                        out.display()
                    );
                    let mut a = w.attach(&[
                        "new",
                        "-s",
                        "a",
                        "sh",
                        "-c",
                        &script,
                        ";",
                        "set",
                        "-g",
                        "extended-keys",
                        "on",
                        ";",
                        "set",
                        "-g",
                        "extended-keys-format",
                        format,
                        ";",
                        "set",
                        "-g",
                        "backspace",
                        option,
                    ]);
                    // The pane's own text: the status line may hold an `R`.
                    a.wait(|_| pane_line(&w) == "R");
                    let want_mode = match mode {
                        "" => "VT10x",
                        r"\033[>4;1m" => "Ext 1",
                        _ => "Ext 2",
                    };
                    a.wait(|_| {
                        let shown = w.cli(&["display", "-p", "-t", "a", "#{pane_key_mode}"]);
                        String::from_utf8_lossy(&shown.stdout).trim() == want_mode
                    });
                    thread::sleep(Duration::from_millis(100));
                    for input in INPUTS {
                        a.keys(input);
                        // Apart, so an Escape is not read as a prefix.
                        thread::sleep(Duration::from_millis(60));
                        a.keys(b"|");
                        thread::sleep(Duration::from_millis(30));
                    }
                    a.keys(b".");
                    let start = Instant::now();
                    let bytes = loop {
                        let bytes = std::fs::read(&out).unwrap_or_default();
                        if bytes.ends_with(b".") {
                            break bytes;
                        }
                        assert!(
                            start.elapsed() < Duration::from_secs(5),
                            "{option} {want_mode} {format}: {bytes:?}"
                        );
                        thread::sleep(Duration::from_millis(20));
                    };
                    rows.push(format!(
                        "{option} {want_mode} {format}: {}",
                        String::from_utf8_lossy(&bytes).escape_debug()
                    ));
                }
            }
        }
        seen.push(rows);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) against tmux 3.8 (right)");
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

/// tty.c `tty_cmd_rawstring` / `tty_block_maybe`: passthrough goes into the
/// client's output like any other, so a client that stops reading blocks and
/// the rest is dropped (`tty_add`). A redraw deferred while the client has a
/// frame unwritten must not hold the payloads back in the server, where
/// they grew without bound (reviewer stall3.py: 9 -> 157 MB in 8 s). Three
/// full screens of text first (~36 KB of frames, more than the PTY buffer
/// takes, less than the 113 KB block threshold) leave one frame unwritten.
#[test]
fn stalled_terminal_bounds_passthrough() {
    let payload = format!("\\033Ptmux;\\033\\033]0;{}\\007\\033\\\\", "x".repeat(200));
    let script = format!(
        "printf READY; read x; for r in 1 2 3; do i=0; while [ $i -lt 59 ]; do printf '%0200d\\n' $((r*100+i)); i=$((i+1)); done; sleep 0.2; done; while :; do printf '{payload}'; done"
    );
    let w = World::new(PathBuf::from(env!("CARGO_BIN_EXE_vtmux")));
    let mut a = w.attach_sized(&["new", "-s", "a", "sh", "-c", &script], 240, 59);
    a.wait(|a| pane_text(a).contains("READY"));
    w.cli(&["set", "-g", "allow-passthrough", "all"]);
    a.paused.store(true, Ordering::Relaxed);
    a.keys(b"\r");
    for _ in 0..4 {
        thread::sleep(Duration::from_secs(3));
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
            // The reference takes under 0.1 s (tty_close frees what the
            // terminal has not taken); 3 s leaves room for a loaded CI
            // runner. The bug this pins took 5.9 s and more.
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
            (c.text.to_string(), c.style.fg)
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
        // The splits run `cat -v` too, not a login shell: its prompt comes
        // from the host (macOS /etc/profile sets PS1 to host and user,
        // which may hold a `j`).
        assert!(
            w.cli(&[
                "set",
                "-g",
                "default-command",
                "stty raw -echo; exec cat -v"
            ])
            .status
            .success()
        );
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

/// D-014: a client's cell size in pixels (tty.c `tty_resize`): from the
/// terminal's size when it gives pixels; else `CSI 18 t` and `CSI 14 t` go
/// to the terminal, and its `CSI 4 ; height ; width t` reply sets it
/// (tty-keys.c `tty_keys_winsz`). A resize without pixels clears it and
/// asks again; one with pixels asks nothing.
#[test]
fn cell_pixel_size_from_the_terminal_or_its_reply() {
    const QUERY: &str = "\x1b[18t\x1b[14t";
    let cell = |w: &World| {
        String::from_utf8_lossy(
            &w.cli(&["lsc", "-F", "#{client_cell_width}x#{client_cell_height}"])
                .stdout,
        )
        .trim()
        .to_owned()
    };
    let wait_cell = |w: &World, a: &Attached, want: &str| {
        a.wait(|_| cell(w) == want);
    };
    for bin in bins() {
        let mut w = World::new(bin.clone());
        w.pixels = (400, 200);
        let a = w.attach(&["new", "-s", "a", "-n", "w", "cat", "-"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        wait_cell(&w, &a, "10x20");
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "cat", "-"]);
        a.wait(|a| a.text().contains(QUERY));
        assert_eq!(cell(&w), "0x0", "{}", w.bin.display());
        a.keys(b"\x1b[8;10;40t\x1b[4;200;400t");
        wait_cell(&w, &a, "10x20");
        a.resize(50, 10);
        a.wait(|a| a.text().matches(QUERY).count() == 2);
        wait_cell(&w, &a, "0x0");
        a.keys(b"\x1b[4;300;500t");
        wait_cell(&w, &a, "10x30");
        a.resize_pixels(40, 10, (320, 160));
        wait_cell(&w, &a, "8x16");
        // Fewer pixels than columns: no cell width, asked again.
        a.resize_pixels(80, 10, (40, 160));
        wait_cell(&w, &a, "0x16");
        a.keys(b"\x1b[4;160;400t");
        wait_cell(&w, &a, "5x16");
        // popup_modify's -b is a tty_resize (popup.c:551): the terminal's
        // own pixels again, and so the queries.
        let name = String::from_utf8_lossy(&w.cli(&["lsc", "-F", "#{client_name}"]).stdout)
            .trim()
            .to_owned();
        // Opening one waits for it to close (cmd-display-menu.c:703).
        let mut opened = Command::new(&w.bin)
            .args(w.args())
            .args(["display-popup", "-c", &name, "-w", "20", "-h", "5"])
            .args(["sh", "-c", "printf POPUP; exec cat -"])
            .env_clear()
            .envs(w.env())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        a.wait(|a| a.text().contains("POPUP"));
        let out = w.cli(&["display-popup", "-c", &name, "-b", "rounded"]);
        assert!(out.status.success(), "{out:?}");
        wait_cell(&w, &a, "0x16");
        // Everything sent so far is in once the popup's echo is.
        a.keys(b"END");
        a.wait(|a| a.text().contains("END"));
        assert_eq!(a.text().matches(QUERY).count(), 4, "{}", w.bin.display());
        let out = w.cli(&["display-popup", "-C", "-c", &name]);
        assert!(out.status.success(), "{out:?}");
        opened.wait().unwrap();
    }
}

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

/// Whether the host has a terminfo entry for `term`: the reference tmux
/// refuses a terminal it can't find (macOS's base set has no
/// `rxvt-unicode-256color`), so a row that attaches under it can't be
/// compared there. Assumed present when `infocmp` is not.
fn host_has_terminfo(term: &str) -> bool {
    std::process::Command::new("infocmp")
        .arg(term)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(true, |s| s.success())
}

/// D-007: the terminals a tmux user's `TERM` names attach, with what their
/// terminfo entries say (tty-term.c `tty_term_create`): the console has 8
/// colours, no `smcup` (so `tty_start_tty` writes none) and no `kmous`; a VT
/// terminal has no colours at all. The startup and teardown bytes are
/// terminfo's: `rmkx`/`smcup` follow the entry. Terminals from ncurses'
/// base set, which every host has.
#[test]
fn terminals_tmux_knows_attach() {
    let fields = "#{client_termname}|#{client_termfeatures}|#{client_colours}|#{Ic:smcup}|#{Ic:kmous}|#{Ic:bce}|#{If:mouse}";
    for term in ["linux", "vt100", "vt220", "rxvt-unicode-256color", "tmux"] {
        if !host_has_terminfo(term) {
            continue;
        }
        let mut seen = Vec::new();
        for bin in bins() {
            let w = World::new(bin.clone());
            let pane = r"printf 'hello\n'; exec cat";
            assert!(
                w.cli(&[
                    "new", "-d", "-s", "a", "-x", "40", "-y", "10", "sh", "-c", pane
                ])
                .status
                .success()
            );
            assert!(w.cli(&["set", "-g", "status-right", ""]).status.success());
            let mut a = w.attach_env(&["attach", "-t", "a"], 40, 10, &[("TERM", term)]);
            a.wait(|a| screen_text(a).contains("hello"));
            let list = w.cli(&["lsc", "-F", fields]);
            let list = String::from_utf8_lossy(&list.stdout).trim().to_owned();
            let started = a.text().contains("\x1b[?1049h");
            a.keys(b"\x02d");
            a.exited(0);
            let left = a.text().contains("\x1b[?1049l");
            seen.push((list, started, left));
        }
        assert_eq!(seen[0], seen[1], "{term}: vtmux vs reference");
    }
}

/// D-007: what tmux refuses. A terminal without `clear` (`dumb`, or an empty
/// `TERM`) fails in `tty_term_create`; one terminfo does not have fails in
/// tmux's client (`missing or unsuitable terminal`, client.c:325-331). The
/// terminal is opened before `new-session` makes the session
/// (cmd-new-session.c:207-213), so none is left behind.
#[test]
fn terminals_tmux_refuses_leave_no_session() {
    for term in ["dumb", "", "vtmux-no-such-terminal"] {
        let mut seen = Vec::new();
        for bin in bins() {
            let w = World::new(bin.clone());
            assert!(
                w.cli(&["new", "-d", "-s", "keep", "cat", "-"])
                    .status
                    .success()
            );
            let mut a = w.attach_env(&["new", "-s", "x", "cat", "-"], 40, 10, &[("TERM", term)]);
            a.exited(1);
            let ls = w.cli(&["ls", "-F", "#{session_name}"]);
            seen.push((a.text(), String::from_utf8_lossy(&ls.stdout).into_owned()));
        }
        assert_eq!(seen[0], seen[1], "{term:?}: vtmux vs reference");
        assert_eq!(seen[0].1, "keep\n", "{term:?}");
    }
    // An override that removes `cup` refuses too.
    for bin in bins() {
        let w = World::new(bin.clone());
        assert!(
            w.cli(&["new", "-d", "-s", "keep", "cat", "-"])
                .status
                .success()
        );
        assert!(
            w.cli(&["set", "-s", "terminal-overrides[10]", "xterm*:cup@"])
                .status
                .success()
        );
        let mut a = w.attach(&["attach", "-t", "keep"]);
        a.exited(1);
        assert_eq!(
            a.text(),
            "open terminal failed: terminal does not support cup\r\n",
            "{}",
            bin.display()
        );
    }
}

/// D-007: the key tree has the terminal's terminfo keys
/// (`tty_default_code_keys`): the console's F1 is `ESC [[A`, rxvt's Home and
/// End are `ESC [7~` and `ESC [8~`.
#[test]
fn terminfo_keys_follow_the_terminal() {
    for (term, keys, want) in [
        ("linux", &b"\x1b[[A"[..], "F1"),
        ("linux", b"\x1b[[E", "F5"),
        ("rxvt-unicode-256color", b"\x1b[7~", "Home"),
        ("rxvt-unicode-256color", b"\x1b[8~", "End"),
    ] {
        if !host_has_terminfo(term) {
            continue;
        }
        for bin in bins() {
            let w = World::new(bin.clone());
            assert!(
                w.cli(&["new", "-d", "-s", "a", "cat", "-"])
                    .status
                    .success()
            );
            for k in ["F1", "F5", "Home", "End"] {
                assert!(
                    w.cli(&["bind", "-n", k, "set", "-g", "@key", k])
                        .status
                        .success()
                );
            }
            let mut a = w.attach_env(&["attach", "-t", "a"], 40, 10, &[("TERM", term)]);
            a.wait(|_| !w.cli(&["lsc", "-F", "x"]).stdout.is_empty());
            a.keys(keys);
            a.wait_labelled(
                &format!("{term} {want} {}", bin.display()),
                Duration::from_secs(5),
                |_| String::from_utf8_lossy(&w.cli(&["show", "-gv", "@key"]).stdout).trim() == want,
            );
            a.keys(b"\x02d");
            a.exited(0);
        }
    }
}

/// D-007: `terminal-overrides` reaches the strings vtmux writes. The common
/// `smcup@:rmcup@` keeps the terminal on its normal screen, so what was on
/// it stays in its scrollback (`tty_start_tty`, `tty_stop_tty`).
#[test]
fn overrides_remove_the_alternate_screen() {
    for bin in bins() {
        let w = World::new(bin.clone());
        assert!(
            w.cli(&["new", "-d", "-s", "a", "cat", "-"])
                .status
                .success()
        );
        assert!(
            w.cli(&[
                "set",
                "-s",
                "terminal-overrides[10]",
                "xterm*:smcup@:rmcup@"
            ])
            .status
            .success()
        );
        let mut a = w.attach(&["attach", "-t", "a"]);
        a.wait(|_| !w.cli(&["lsc", "-F", "x"]).stdout.is_empty());
        a.keys(b"\x02d");
        a.exited(0);
        let text = a.text();
        assert!(!text.contains("\x1b[?1049"), "{}: {text:?}", bin.display());
        // `clear` still starts and ends it (clear-on-attach is on).
        assert!(
            text.contains("\x1b[H\x1b[2J"),
            "{}: {text:?}",
            bin.display()
        );
    }
}

/// `tty_check_fg`/`tty_check_bg`: a client with fewer than 256 colours gets
/// `colour_256to16`, aixterm colours only from 16, bold for bright foregrounds
/// at 8 (reviewer-26 raw.py). The observer's screen is the same under both.
/// A palette colour 0-15 set as `38;5;N` (or `colourN` in a style) keeps
/// `COLOUR_FLAG_256`: plain red for `38;5;9` at 8 colours where `91` is bold;
/// at 256 colours `setaf` writes it as `91` (task `d_sgr_256_low16`).
#[test]
fn colours_below_256_are_downgraded_like_tmux() {
    // fg 38;5;200 on default, 38;5;16 on 48;5;200, rgb 1;2;3, aixterm 91 on 101,
    // 38;5;244 on 47 (grey on white), 38;5;9 on 48;5;12, 31.
    let pane = r"printf '\033[38;5;200mA\033[0m \033[38;5;16;48;5;200mB\033[0m \033[38;2;1;2;3mC\033[0m \033[91;101mD\033[0m \033[38;5;244;47mE\033[0m \033[38;5;9;48;5;12mF\033[0m \033[31mG\033[0m'; exec cat";
    for term in [
        "xterm",
        "screen",
        "xterm-16color",
        "xterm-88color",
        "xterm-256color",
        "linux",
        "vt100",
    ] {
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
            // The status line in a `colourN` style (colour.c `colour_fromstring`).
            for (opt, value) in [
                ("status-style", "fg=colour9,bg=colour12"),
                ("status-right", ""),
            ] {
                assert!(w.cli(&["set", "-g", opt, value]).status.success());
            }
            let mut a = w.attach_env(&["attach", "-t", "a"], 40, 10, &[("TERM", term)]);
            a.wait(|a| {
                vtmux_vt::snapshot(&a.screen())
                    .cell(12, 0)
                    .is_some_and(|c| c.text == "G")
            });
            let screen = vtmux_vt::snapshot(&a.screen());
            let row: Vec<_> = (0..13)
                .map(|x| (x, 0))
                .chain([(0, 9)])
                .map(|(x, y)| {
                    let c = screen.cell(x, y).unwrap();
                    (c.text.clone(), c.style.fg, c.style.bg, c.style.bold)
                })
                .collect();
            screens.push(row);
            a.keys(b"\x02d");
            a.exited(0);
        }
        assert_eq!(screens[0], screens[1], "{term}: vtmux vs reference");
        // 200 is bright magenta in 16 colours: aixterm or plain, never 38;5;n.
        if term != "xterm-256color" {
            assert_ne!(screens[0][0].1, vtmux_vt::Color::Palette(200), "{term}");
        }
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
/// a SIGWINCH came within one `sleep 0.03` of the switch. (A loop of six
/// `sleep 0.01` forks and execs each time: on macOS that took 100-185 ms,
/// past the 250 ms timer's expiry, so leaving never looked SLOW.) To test leaving,
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
         sleep 0.03; \
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

/// `history-file` (cfg.c:59, server.c:261, prompt-history.c): the server
/// writes `type:line` lines when it exits and reads them once the config
/// has run, on both servers.
#[test]
fn history_file_is_saved_at_exit_and_loaded_after_the_config() {
    for bin in bins() {
        let w = World::new(bin);
        let file = w.home.path().join("hist");
        let conf = w.home.path().join("c.conf");
        std::fs::write(&conf, format!("set -g history-file {}\n", file.display())).unwrap();
        let conf = conf.to_str().unwrap();
        let mut a = w.attach(&["-f", conf, "new", "-s", "a", "-n", "w", "cat"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        a.keys(b"\x02:");
        a.wait(|a| status_row(a).starts_with(':'));
        a.keys(b"set -g @h one\r");
        a.wait(|_| w.cli(&["show", "-gv", "@h"]).stdout == b"one\n");
        assert!(w.cli(&["kill-server"]).status.success());
        a.exited(1);
        // Written as the server exits: wait for the whole line.
        let end = Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(&file).is_ok_and(|t| t.ends_with('\n')) {
            assert!(Instant::now() < end, "history file not written");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "command:set -g @h one\n",
            "{}",
            w.bin.display()
        );
        let out = w.cli(&[
            "-f",
            conf,
            "new",
            "-d",
            "-s",
            "b",
            ";",
            "show-prompt-history",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "History for command:\n\n1: set -g @h one\n\nHistory for search:\n\n\n",
            "{}",
            w.bin.display()
        );
        assert!(w.cli(&["kill-server"]).status.success());
    }
}

/// `prompt_load_history` (prompt-history.c:79-106) reads lines as bytes:
/// a line that is not UTF-8 is listed as it is by `show-prompt-history`
/// and written back unchanged when the server exits, on both servers.
#[test]
fn history_file_keeps_lines_that_are_not_utf8() {
    for bin in bins() {
        let w = World::new(bin);
        let file = w.home.path().join("hist");
        let data: &[u8] = b"command:display \xff\xfe\nsearch:caf\xe9\n";
        std::fs::write(&file, data).unwrap();
        let conf = w.home.path().join("c.conf");
        std::fs::write(&conf, format!("set -g history-file {}\n", file.display())).unwrap();
        let conf = conf.to_str().unwrap();
        let out = w.cli(&[
            "-f",
            conf,
            "new",
            "-d",
            "-s",
            "b",
            "cat",
            ";",
            "show-prompt-history",
        ]);
        assert_eq!(
            out.stdout,
            b"History for command:\n\n1: display \xff\xfe\n\nHistory for search:\n\n1: caf\xe9\n\n",
            "{}",
            w.bin.display()
        );
        std::fs::remove_file(&file).unwrap();
        assert!(w.cli(&["kill-server"]).status.success());
        let end = Instant::now() + Duration::from_secs(10);
        while !std::fs::read(&file).is_ok_and(|t| t.ends_with(b"\n") && t.len() == data.len()) {
            assert!(Instant::now() < end, "history file not written");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&file).unwrap(), data, "{}", w.bin.display());
    }
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
        // Past the start timer (5 s) with the flood still running: wait
        // for the state, not a fixed time. A slow runner (or frame
        // coalescing) delivers the bytes later; bounded at 30 s.
        let deadline = Instant::now() + Duration::from_secs(30);
        while (start.elapsed() < Duration::from_millis(7500)
            || a.bytes.lock().unwrap().len() <= skip + 20_000)
            && Instant::now() < deadline
        {
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

/// `show-messages` with an attached client (cmd-queue.c:549-563,
/// status.c:354-362): a command run by a key is logged `<tty> key <key>:
/// <command>`, a status-line message `<tty> message: <text>`, a command run
/// by the CLI `client-PID command: ...`; `-T` lists the client's terminal
/// (the reference lists every terminfo capability under the header, which
/// vtmux has no model for: only the header line is compared, D-017).
#[test]
fn show_messages_names_keys_and_terminals() {
    let mut outputs = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "sh", "-c", "exec cat"]);
        a.wait(|a| a.text().contains("\x1b[?1049h"));
        assert!(
            w.cli(&["bind", "-n", "F5", "display-message", "hi"])
                .status
                .success()
        );
        assert!(
            w.cli(&["bind", "x", "set", "-g", "@x", "1"])
                .status
                .success()
        );
        assert!(
            w.cli(&["bind", "-n", "M-Up", "set", "-g", "@y", "1"])
                .status
                .success()
        );
        a.keys(b"\x1b[15~");
        a.wait(|_| {
            String::from_utf8_lossy(&w.cli(&["show-messages"]).stdout).contains("message: hi")
        });
        a.keys(b"\x02x");
        a.wait(|_| display(&w, "#{@x}") == "1");
        a.keys(b"\x1b\x1b[A");
        a.wait(|_| display(&w, "#{@y}") == "1");
        let tty = String::from_utf8_lossy(&w.cli(&["lsc", "-F", "#{client_tty}"]).stdout)
            .trim()
            .to_owned();
        let log = String::from_utf8_lossy(&w.cli(&["show-messages"]).stdout).into_owned();
        let mut text = String::new();
        for line in log.lines().skip(1) {
            let line = line.get(7..).unwrap_or(line).replace(&tty, "TTY");
            if line.starts_with("client-") {
                continue;
            }
            text.push_str(&line);
            text.push('\n');
        }
        let terminals =
            String::from_utf8_lossy(&w.cli(&["show-messages", "-T"]).stdout).into_owned();
        let header = terminals
            .lines()
            .next()
            .unwrap_or_default()
            .replace(&tty, "TTY");
        // flags differ by terminfo (0x20 VT100LIKE is set for both).
        outputs.push((text, header));
        a.keys(b"\x02d");
        a.exited(0);
    }
    assert_eq!(outputs[0], outputs[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// The screen's rows with trailing blanks trimmed.
fn rows_of(a: &Attached) -> Vec<String> {
    let snap = vtmux_vt::snapshot(&a.screen());
    (0..snap.rows)
        .map(|y| {
            (0..snap.cols)
                .map(|x| {
                    let t = &snap.cell(x, y).unwrap().text;
                    if t.is_empty() { " " } else { t }
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// Clock mode with the clock pinned (`VTMUX_FAKE_TIME`, debug builds only;
/// the reference ignores it, so this runs against vtmux alone): the exact
/// glyph rows window-clock.c draws for 2026-11-01 13:07:45 UTC in each
/// style, in `clock-mode-colour`, the compact text on a small pane, and
/// any key leaving the mode. The rows were laid out by an independent
/// script from `window_clock_table` and `window_clock_draw_screen`'s
/// arithmetic (`x = sx/2 - 3*len`, `y = sy/2 - 3`, 6 columns a glyph).
#[test]
fn clock_mode_glyphs_at_a_fixed_time() {
    let w = World::new(PathBuf::from(env!("CARGO_BIN_EXE_vtmux")));
    let mut a = w.attach_env(
        &["new", "-s", "a", "-n", "w", "cat"],
        80,
        24,
        &[("VTMUX_FAKE_TIME", "1793538465"), ("TZ", "UTC")],
    );
    a.wait(|a| a.text().contains("\x1b[?1049h"));
    for cmd in [
        &["set", "-g", "status", "off"][..],
        &["set", "-w", "clock-mode-colour", "red"],
        &["set", "-w", "clock-mode-style", "24-with-seconds"],
        &["clock-mode", "-t", "a"],
    ] {
        assert!(w.cli(cmd).status.success(), "{cmd:?}");
    }
    let glyphs = |top: [&str; 5]| -> Vec<String> {
        let mut rows = vec![String::new(); 24];
        for (i, r) in top.iter().enumerate() {
            rows[9 + i] = (*r).to_owned();
        }
        rows
    };
    let want = glyphs([
        "                    # #####       ##### #####       #   # #####",
        "                    #     #   #   #   #     #   #   #   # #",
        "                    # #####       #   #     #       ##### #####",
        "                    #     #   #   #   #     #   #       #     #",
        "                    # #####       #####     #           # #####",
    ]);
    a.wait_for(Duration::from_secs(5), |a| rows_of(a) == want);
    // Every `#` is the colour on the colour (red is palette 1).
    let snap = vtmux_vt::snapshot(&a.screen());
    let hashes: Vec<_> = snap.cells.iter().filter(|c| c.text == "#").collect();
    assert_eq!(hashes.len(), 79);
    assert!(
        hashes
            .iter()
            .all(|c| c.style.fg == vtmux_vt::Color::Palette(1)
                && c.style.bg == vtmux_vt::Color::Palette(1))
    );
    // 12-hour style: " 1:07 PM".
    assert!(
        w.cli(&["set", "-w", "clock-mode-style", "12"])
            .status
            .success()
    );
    let want = glyphs([
        "                          #       ##### #####       ##### #   #",
        "                          #   #   #   #     #       #   # ## ##",
        "                          #       #   #     #       ##### # # #",
        "                          #   #   #   #     #       #     #   #",
        "                          #       #####     #       #     #   #",
    ]);
    a.wait_for(Duration::from_secs(5), |a| rows_of(a) == want);
    // A pane too small for glyphs shows the text, in the colour.
    a.resize(20, 5);
    assert!(
        w.cli(&["set", "-w", "clock-mode-style", "24"])
            .status
            .success()
    );
    a.wait_for(Duration::from_secs(5), |a| {
        rows_of(a).get(2).map(String::as_str) == Some("        13:07")
    });
    let snap = vtmux_vt::snapshot(&a.screen());
    assert_eq!(
        snap.cell(8, 2).unwrap().style.fg,
        vtmux_vt::Color::Palette(1)
    );
    // Any key leaves the mode.
    a.keys(b"x");
    a.wait_for(Duration::from_secs(5), |_| {
        display(&w, "[#{pane_mode}]") == "[]"
    });
}

/// The titles the client wrote to its terminal so far (`tsl` `title` `fsl`
/// with xterm's `\e]0;` and BEL).
fn titles(a: &Attached) -> Vec<String> {
    let bytes = a.bytes.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&bytes);
    text.split("\x1b]0;")
        .skip(1)
        .filter_map(|rest| rest.split_once('\x07').map(|(t, _)| t.to_owned()))
        .collect()
}

/// server-client.c `server_client_set_title` (2588-2610): with `set-titles`
/// on, a redraw writes `set-titles-string` (expanded for the client) between
/// `tsl` and `fsl`, unless it is the title last written. Reference probe:
/// `\e]0;T:a:neww\a` after `rename-window neww`. Everything is compared
/// raw against tmux 3.8.
#[test]
fn set_titles_emits_the_expanded_string_once_per_change() {
    let steps: &[&[&str]] = &[
        &["set", "-g", "set-titles", "on"],
        &["set", "-g", "set-titles-string", "T:#S:#W"],
        // The same redraw again: nothing is written.
        &["refresh-client"],
        &["rename-window", "neww"],
        &["new-window", "-d", "-n", "other", "cat"],
        &["select-window", "-t", ":other"],
        &["select-window", "-t", ":neww"],
        // The owner's string (~/dotfiles/tmux/.tmux.conf:20) over a user option.
        &["set", "-g", "@win_label", "lbl-#{window_index}"],
        &["set", "-g", "set-titles-string", "#S · #{T:@win_label}"],
        &["set", "-g", "set-titles", "off"],
        &["rename-window", "hidden"],
        &["set", "-g", "set-titles", "on"],
        &["refresh-client"],
        // Same text as the last title written: still nothing.
        &["set", "-g", "set-titles-string", "#S · #{T:@win_label}"],
        &["refresh-client"],
    ];
    let mut all = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        assert!(
            w.cli(&[
                "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "10", "cat"
            ])
            .status
            .success()
        );
        let mut a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).starts_with("[a]"));
        // With set-titles off (the default) nothing is written.
        assert_eq!(titles(&a), Vec::<String>::new());
        let mut seen = Vec::new();
        for step in steps {
            assert!(w.cli(step).status.success(), "{step:?}");
            // Let the redraw land before the next step.
            thread::sleep(Duration::from_millis(300));
            seen.push(titles(&a));
        }
        a.keys(b"\x02d");
        a.exited(0);
        all.push(seen);
    }
    assert_eq!(all[0], all[1], "vtmux (left) vs tmux 3.8 (right)");
    // What the reference wrote, in order (the first is the default string, which has the host name).
    let default = all[0][0][0].clone();
    assert!(default.starts_with("a:0:w - \""), "{default:?}");
    let want = [
        default.as_str(),
        "T:a:w",
        "T:a:neww",
        "T:a:other",
        "T:a:neww",
        "a · lbl-0",
    ];
    // Per step: nothing new for the repeated redraw (step 2), the unchanged
    // string (last) and the title that goes with set-titles off (10).
    let counts: Vec<usize> = all[0].iter().map(Vec::len).collect();
    assert_eq!(
        counts,
        [1, 2, 2, 3, 3, 4, 5, 5, 6, 6, 6, 6, 6, 6, 6][..counts.len()]
    );
    assert_eq!(all[0].last().unwrap(), &want);
}

/// `tty_set_title` needs `tsl` and `fsl`: `terminal-features` without
/// `title` writes no title at all, and `title` on `TERM=xterm-256color`
/// is what the default `xterm*` entry gives.
#[test]
fn set_titles_needs_the_title_feature() {
    for bin in bins() {
        let w = World::new(bin);
        assert!(
            w.cli(&["new", "-d", "-s", "a", "-x", "40", "-y", "10", "cat"])
                .status
                .success()
        );
        for args in [
            &["set", "-s", "terminal-features[10]", "xterm*:title@"][..],
            &["set", "-g", "set-titles", "on"],
            &["set", "-g", "set-titles-string", "X#S"],
        ] {
            assert!(w.cli(args).status.success());
        }
        let mut a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| status_row(a).starts_with("[a]"));
        assert!(w.cli(&["rename-window", "n"]).status.success());
        thread::sleep(Duration::from_millis(500));
        assert!(!a.text().contains("\x1b]0;"), "{:?}", a.text());
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// The startup config's causes, once a session is attached (cfg.c
/// `cfg_show_causes`, from `cfg_done`, new-session and attach-session): view
/// mode of the session's active pane, each written unparsed
/// (`window_copy_add(wp, 0, ...)`), so an escape in an option name shows as
/// text: `ESC(0` would draw `red` as line-drawing characters. The name has
/// no `[`: for a name `options_parse` rejects, `cmd_set_option_exec` reads
/// `ambiguous` uninitialised (options.c `options_match`, valgrind: "depends
/// on uninitialised value"), so the reference says `invalid` or
/// `ambiguous` depending on the build (CI's Ubuntu build said `invalid`).
/// Then a detached start: the causes wait for the attach (D-009).
#[test]
fn config_causes_show_in_view_mode_once_attached() {
    let mut screens = Vec::new();
    // One short path for both servers: the messages show it, a host's long
    // temp dir (macOS's /var/folders/...) wraps on the screen where no
    // replacement of it can match, and the two servers' random dirs differ.
    let cfg_dir = PathBuf::from(format!("/tmp/vtmux-cc-{}", std::process::id()));
    std::fs::create_dir_all(&cfg_dir).unwrap();
    for bin in bins() {
        let w = World::new(bin);
        let conf = cfg_dir.join("c.conf");
        std::fs::write(
            &conf,
            "set -g status-right ''\nset -g status-left 'S '\nset -g nosuchopt 1\nset -g \"\\033(0red\" 1\nset -g status-interval x\n",
        )
        .unwrap();
        let conf = conf.to_str().unwrap().to_owned();
        let dir = w.home.path().to_str().unwrap().to_owned();
        let shown = |a: &Attached| screen_text(a).replace(&dir, "DIR");
        let mut seen = Vec::new();
        // new-session attached: the causes are in view mode when it ends.
        let mut a = w.attach(&["-f", &conf, "new", "-s", "a", "-n", "w", "cat", "-"]);
        a.wait(|a| screen_text(a).contains("(0red"));
        seen.push(shown(&a));
        // The mode is the pane's own: `q` leaves it, and nothing comes back.
        a.keys(b"q");
        a.wait(|a| !screen_text(a).contains("nosuchopt"));
        seen.push(shown(&a));
        a.keys(b"\x02d");
        a.exited(0);
        // Started detached: the causes wait for the attach.
        assert!(w.cli(&["kill-server"]).status.success());
        let out = w.cli(&["-f", &conf, "new", "-d", "-s", "b", "-n", "w", "cat", "-"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut b = w.attach(&["attach", "-t", "b"]);
        b.wait(|a| screen_text(a).contains("(0red"));
        seen.push(shown(&b));
        b.keys(b"\x02d");
        b.exited(0);
        // Shown once: a later attach finds a plain shell.
        let mut c = w.attach(&["attach", "-t", "b"]);
        c.wait(|a| status_row(a).contains("0:w"));
        seen.push(shown(&c));
        c.keys(b"\x02d");
        c.exited(0);
        screens.push(seen);
    }
    let _ = std::fs::remove_dir_all(&cfg_dir);
    assert!(screens[0][0].contains("nosuchopt"), "{}", screens[0][0]);
    assert_eq!(screens[0], screens[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// The screen as plain text, trailing spaces trimmed per line.
fn screen_text(a: &Attached) -> String {
    String::from_utf8(capture(&a.screen(), CaptureOpts::default()))
        .unwrap()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The owner's `bind S choose-tree -sZ -O name` (dotfiles .tmux.conf:132)
/// and the default `s` and `w` bindings (`choose-tree -Zs`, `-Zw`,
/// key-bindings.c:439,443): the tree opens zoomed on the current session
/// or window, sessions by name; Enter switches the client
/// (`switch-client -Zt`) and the window is unzoomed when the mode goes.
#[test]
fn choose_tree_bindings_s_w_and_owner_s() {
    let mut results = Vec::new();
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&[
            "new", "-d", "-s", "mmm", "-n", "a", "-x", "60", "-y", "12", "exec cat",
        ]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["splitw", "-d", "-t", "mmm", "exec cat"]);
        w.cli(&["new", "-d", "-s", "bbb", "-n", "b", "exec cat"]);
        w.cli(&["neww", "-d", "-t", "bbb:1", "-n", "c", "exec cat"]);
        w.cli(&["new", "-d", "-s", "zzz", "-n", "d", "exec cat"]);
        for p in ["%0", "%1", "%2", "%3", "%4"] {
            w.cli(&["select-pane", "-t", p, "-T", "t"]);
        }
        w.cli(&["bind", "S", "choose-tree", "-sZ", "-O", "name"]);
        let mut a = w.attach_sized(&["attach", "-t", "mmm"], 60, 12);
        a.wait(|a| status_row(a).contains("[mmm]"));
        let mut seen = Vec::new();
        let show = |w: &World, f: &str| {
            String::from_utf8_lossy(&w.cli(&["display", "-p", "-t", "mmm", f]).stdout)
                .trim()
                .to_owned()
        };
        let zoomed = |w: &World| show(w, "#{window_zoomed_flag}");
        for keys in [&b"\x02S"[..], b"\x02s", b"\x02w"] {
            a.keys(keys);
            a.wait(|a| screen_text(a).contains("(0)"));
            seen.push(screen_text(&a));
            seen.push(zoomed(&w));
            a.keys(b"q");
            a.wait(|a| !screen_text(a).contains("(0)"));
            seen.push(zoomed(&w));
        }
        // C-b S, to the last session (zzz), Enter: the client moves there
        // and the window it left is unzoomed.
        a.keys(b"\x02S");
        a.wait(|a| screen_text(a).contains("(2)"));
        a.keys(b"G\r");
        a.wait(|a| status_row(a).contains("[zzz]"));
        seen.push(screen_text(&a));
        seen.push(zoomed(&w));
        results.push(seen);
    }
    assert_eq!(results[0], results[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// input.c `input_dcs_dispatch` and screen-write.c `screen_write_rawstring`
/// (`screen_write_set_client_cb`, 139-161): the payload of `DCS tmux; ... ST`
/// (a doubled ESC is one) is written raw to the clients when the pane's
/// `allow-passthrough` is `on` and the pane's window is the client's current
/// one, or `all` and the client's session has it. `off` drops it, and the
/// pane's grid and title show nothing of it.
#[test]
fn allow_passthrough_delivers_dcs_tmux_payloads_raw() {
    const OSC52: &[u8] = b"\x1b]52;c;aGVsbG8=\x07";
    const APC: &[u8] = b"\x1b_Gi=7,a=q,s=1,v=1,f=24;AAAA\x1b\\";
    // (mode, the pane is in a background window, delivered)
    let cases = [
        ("off", false, false),
        ("on", false, true),
        ("all", false, true),
        ("off", true, false),
        ("on", true, false),
        ("all", true, true),
    ];
    for bin in bins() {
        for (mode, background, want) in cases {
            let w = World::new(bin.clone());
            let file = w.home.path().join("payload");
            let mut payload = b"X\x1bPtmux;\x1b".to_vec();
            payload
                .extend_from_slice(b"\x1b]52;c;aGVsbG8=\x07\x1b\x1b]2;INNER\x07\x1b\x1b[31m\x1b\\");
            payload.extend_from_slice(b"\x1bPtmux;\x1b");
            payload.extend_from_slice(b"\x1b_Gi=7,a=q,s=1,v=1,f=24;AAAA\x1b\x1b\\\x1b\\Y");
            std::fs::write(&file, payload).unwrap();
            let script = format!("read x; cat {}; exec cat", file.display());
            let a = w.attach(&["new", "-s", "a", "-n", "w", "sh", "-c", &script]);
            // The server is up once the status line draws; then blank
            // status-right: a long host name in it hides the window list
            // of a 40-column client.
            a.wait(|a| status_row(a).starts_with("[a]"));
            w.cli(&["set", "-g", "status-right", ""]);
            a.wait(|a| status_row(a).contains("0:w"));
            w.cli(&["set", "-g", "allow-passthrough", mode]);
            let target = "a:0";
            if background {
                w.cli(&["neww", "-d", "-n", "other", "cat"]);
                // Move the client off the window that prints.
                w.cli(&["select-window", "-t", "a:1"]);
                a.wait(|a| status_row(a).contains("1:other*"));
            }
            let start = a.bytes.lock().unwrap().len();
            w.cli(&["send-keys", "-t", target, "Enter"]);
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                let out = w.cli(&["capture-pane", "-p", "-t", target]);
                if String::from_utf8_lossy(&out.stdout).contains('Y') {
                    break;
                }
                assert!(Instant::now() < end, "{} {mode}: no marker", bin.display());
                thread::sleep(Duration::from_millis(20));
            }
            thread::sleep(Duration::from_millis(300));
            let got = a.bytes.lock().unwrap()[start..].to_vec();
            let has = |needle: &[u8]| got.windows(needle.len()).any(|x| x == needle);
            let what = format!("{} {mode} background={background}", bin.display());
            assert_eq!(has(OSC52), want, "{what}: OSC 52 in {got:?}");
            assert_eq!(has(APC), want, "{what}: APC in {got:?}");
            // Nothing of the payload is drawn or titles the pane, delivered
            // or not.
            let out = w.cli(&["display", "-p", "-t", target, "#{pane_title}"]);
            let title = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            assert!(!title.contains("INNER"), "{what}: title {title:?}");
            let out = w.cli(&["capture-pane", "-p", "-e", "-t", target]);
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            // (the Enter that released `read` echoed a newline first)
            let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            assert_eq!(lines, ["XY"], "{what}: grid {text:?}");
        }
    }
}

/// input.c:2970, 3099, 3115, 3142, 3158, 3455, 3478: an OSC that changes the
/// pane's palette (4, 104, 10, 11, 110, 111) makes tmux redraw the whole pane
/// (`screen_write_fullredraw`, `PANE_REDRAW`), and `allow-passthrough on`
/// drops a payload that follows in the same redraw cycle
/// (screen-write.c:150). One that changes nothing (a bad colour, a query,
/// OSC 12) does not. Each case is a list of steps, one per Enter; the last
/// ends with the payload.
#[test]
fn palette_oscs_drop_the_next_passthrough_payload_under_on() {
    const OSC52: &[u8] = b"\x1b]52;c;aGk=\x07";
    let cases: &[(&[&[u8]], bool)] = &[
        (&[b"\x1b]4;1;#ff0000\x07"], false),
        (&[b"\x1b]4;1;rgb:ff/00/00\x1b\\"], false),
        (&[b"\x1b]4;1;1,2,3\x07"], false),
        (&[b"\x1b]4;1;red\x07"], false),
        (&[b"\x1b]4;1;zzz\x07"], true),
        (&[b"\x1b]4;1;#ff00\x07"], true),
        (&[b"\x1b]4;999;red\x07"], true),
        (&[b"\x1b]10;#ffffff\x07"], false),
        (&[b"\x1b]11;#000000\x07"], false),
        (&[b"\x1b]12;#ff0000\x07"], true),
        (&[b"\x1b]104\x07"], false),
        (&[b"\x1b]110\x07"], false),
        (&[b"\x1b]111\x07"], false),
        (&[b"\x1b]110;x\x07"], true),
        (&[b"\x1b]112\x07"], true),
        // OSC 104;N redraws only once an entry has been set.
        (&[b"\x1b]104;1\x07"], true),
        (&[b"\x1b]4;1;red\x07", b"\x1b]104;1\x07"], false),
        (&[b"\x1b]4;1;red\x07", b"\x1b]104\x07\x1b]104;1\x07"], false),
        // The redraw of an earlier cycle is over by the next one.
        (&[b"\x1b]4;1;red\x07", b"-"], true),
    ];
    for bin in bins() {
        for (steps, want) in cases {
            let w = World::new(bin.clone());
            let mut script = String::new();
            for (i, step) in steps.iter().enumerate() {
                let mut data = (*step).to_vec();
                if i + 1 == steps.len() {
                    data.extend_from_slice(b"X\x1bPtmux;\x1b");
                    data.extend_from_slice(OSC52);
                    data.extend_from_slice(b"\x1b\\Y");
                }
                let file = w.home.path().join(format!("step{i}"));
                std::fs::write(&file, data).unwrap();
                write!(script, "read x; cat {}; ", file.display()).unwrap();
            }
            script.push_str("exec cat");
            let a = w.attach(&["new", "-s", "a", "-n", "w", "sh", "-c", &script]);
            // The server is up once the status line draws; then blank
            // status-right: a long host name in it hides the window list
            // of a 40-column client.
            a.wait(|a| status_row(a).starts_with("[a]"));
            w.cli(&["set", "-g", "status-right", ""]);
            a.wait(|a| status_row(a).contains("0:w"));
            w.cli(&["set", "-g", "allow-passthrough", "on"]);
            let mut start = 0;
            for _ in 0..steps.len() {
                start = a.bytes.lock().unwrap().len();
                w.cli(&["send-keys", "-t", "a:0", "Enter"]);
                // Let the redraw of this cycle happen before the next.
                thread::sleep(Duration::from_millis(500));
            }
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                let out = w.cli(&["capture-pane", "-p", "-t", "a:0"]);
                if String::from_utf8_lossy(&out.stdout).contains('Y') {
                    break;
                }
                assert!(Instant::now() < end, "{}: no marker", bin.display());
                thread::sleep(Duration::from_millis(20));
            }
            thread::sleep(Duration::from_millis(300));
            let got = a.bytes.lock().unwrap()[start..].to_vec();
            let n = got.windows(OSC52.len()).filter(|x| *x == OSC52).count();
            assert_eq!(
                n,
                usize::from(*want),
                "{} {steps:?}: OSC 52 in {got:?}",
                bin.display()
            );
        }
    }
}

/// The number after `CNT` in the status row, if any.
fn status_count(a: &Attached) -> Option<u32> {
    let row = status_row(a);
    let rest = row.split_once("CNT")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// status.c:241-243, format.c `format_job_get`/`format_job_complete`,
/// cmd-refresh-client.c:275-280: a `#()` in the status line shows what its
/// job printed once the job reports, runs again on every `status-interval`
/// tick (a later second) and on `refresh-client -S` (`FORMAT_FORCE`), and
/// with neither nothing runs. The job counts its runs in a file.
#[test]
fn status_jobs_refresh_on_interval_and_refresh_client() {
    for bin in bins() {
        let w = World::new(bin.clone());
        let a = w.attach(&["new", "-s", "a", "-n", "w", "cat"]);
        // The server is up once the status line draws; then blank
        // status-right: a long host name in it hides the window list
        // of a 40-column client.
        a.wait(|a| status_row(a).starts_with("[a]"));
        w.cli(&["set", "-g", "status-right", ""]);
        a.wait(|a| status_row(a).contains("0:w"));
        let counter = w.home.path().join("n");
        let script = w.home.path().join("count.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nn=$(cat {c} 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > {c}\necho CNT$n\n",
                c = counter.display()
            ),
        )
        .unwrap();
        w.cli(&["set", "-g", "status-interval", "0"]);
        w.cli(&[
            "set",
            "-g",
            "status-right",
            &format!("#(sh {})", script.display()),
        ]);
        let what = bin.display();
        // The first expansion starts the job; its completion redraws.
        a.wait(|a| status_count(a) == Some(1));
        // No interval, no refresh: the line stays.
        thread::sleep(Duration::from_millis(2200));
        assert_eq!(status_count(&a), Some(1), "{what}: ran without a reason");
        // refresh-client -S forces the job even in the same second.
        w.cli(&["refresh-client", "-S"]);
        a.wait(|a| status_count(a) == Some(2));
        // status-interval: each tick runs the job again.
        w.cli(&["set", "-g", "status-interval", "1"]);
        a.wait_labelled(&what.to_string(), Duration::from_secs(15), |a| {
            status_count(a) >= Some(4)
        });
        let seen = status_count(&a).unwrap();
        // Back to none: no more ticks. A redraw in a later second may still
        // run the job once (format_job_get runs it when fj->last != now,
        // format.c), so check the rate, not an exact count: over 3.5 s an
        // interval of 1 would add 3 or more.
        w.cli(&["set", "-g", "status-interval", "0"]);
        thread::sleep(Duration::from_millis(1500));
        let settled = status_count(&a).unwrap();
        assert!(settled >= seen, "{what}: went back");
        thread::sleep(Duration::from_millis(3500));
        let after = status_count(&a).unwrap();
        assert!(
            after <= settled + 1,
            "{what}: kept running ({settled} -> {after})"
        );
    }
}

/// tty.c `tty_update_features` writes `Eneks` (`\e[>4;2m`, the `extkeys`
/// feature) when `extended-keys` is not off, after a reply to the startup
/// queries or the 5 s start timer (tty.c:311-322, 552-575); `tty_stop_tty`
/// writes `Dseks` (`\e[>4m`) on the way out whatever `extended-keys` says
/// (tty.c:508). The owner's config sets `terminal-features[10]
/// "xterm*:RGB:usstyle:hyperlinks"` and `extended-keys on`: inside Ghostty
/// the XTVERSION reply brings `extkeys`. Each case: (terminal-features,
/// extended-keys, the terminal's reply, Eneks written, Dseks written).
#[test]
fn extended_keys_are_asked_of_a_terminal_with_extkeys() {
    const ENEKS: &[u8] = b"\x1b[>4;2m";
    const DSEKS: &[u8] = b"\x1b[>4m";
    let cases: [(&str, &str, &[u8], bool, bool); 6] = [
        (
            "xterm*:RGB:usstyle:hyperlinks",
            "on",
            b"\x1bP>|ghostty 1.2.3\x1b\\",
            true,
            true,
        ),
        (
            "xterm*:RGB:usstyle:hyperlinks",
            "on",
            b"\x1b[?62;22c",
            false,
            false,
        ),
        ("xterm*:extkeys", "on", b"\x1b[>0;10;1c", true, true),
        ("xterm*:extkeys", "off", b"\x1b[>0;10;1c", false, true),
        ("xterm*:extkeys", "always", b"", true, true),
        ("", "on", b"", false, false),
    ];
    for bin in bins() {
        for (features, ek, reply, eneks, dseks) in cases {
            let w = World::new(bin.clone());
            let mut v = vec!["new", "-d", "-s", "a"];
            v.extend(CAT);
            assert!(w.cli(&v).status.success());
            if !features.is_empty() {
                w.cli(&["set", "-s", "terminal-features[10]", features]);
            }
            w.cli(&["set", "-s", "extended-keys", ek]);
            let mut a = w.attach(&["attach", "-t", "a"]);
            a.wait(|a| a.text().contains("\x1b[?2026$p"));
            let what = format!("{} {features} {ek} {reply:?}", bin.display());
            let has = |a: &Attached, needle: &[u8]| {
                a.bytes
                    .lock()
                    .unwrap()
                    .windows(needle.len())
                    .any(|x| x == needle)
            };
            if reply.is_empty() {
                // No reply: the start timer.
                thread::sleep(Duration::from_secs(3));
                assert!(!has(&a, ENEKS), "{what}: Eneks before the start timer");
                a.wait_for(Duration::from_secs(5), |a| !eneks || has(a, ENEKS));
            } else {
                a.keys(reply);
                a.wait(|a| !eneks || has(a, ENEKS));
            }
            // The redraw that follows, and no Eneks where none is due.
            thread::sleep(Duration::from_millis(300));
            assert_eq!(has(&a, ENEKS), eneks, "{what}: Eneks");
            let before = a.bytes.lock().unwrap().len();
            a.keys(b"\x02d");
            a.exited(0);
            let tail = a.bytes.lock().unwrap()[before..].to_vec();
            let found = tail.windows(DSEKS.len()).any(|x| x == DSEKS);
            assert_eq!(found, dseks, "{what}: Dseks in {tail:?}");
        }
    }
}

/// `#{client_termtype}` is the XTVERSION text (tty-keys.c:1676-1747), and
/// the features its terminal is known for are added. (tmux hands a second
/// reply to the pane as keys once `TTY_HAVEXDA` is set; vtmux swallows
/// it, as it did every `ESC P > |` string before.)
#[test]
fn xtversion_reply_names_the_terminal() {
    const F: &str = "#{client_termtype}|#{client_termfeatures}";
    let mut outputs = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        let mut v = vec!["new", "-d", "-s", "a"];
        v.extend(CAT);
        assert!(w.cli(&v).status.success());
        let mut a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| a.text().contains("\x1b[?2026$p"));
        let lsc =
            |w: &World| String::from_utf8_lossy(&w.cli(&["lsc", "-F", F]).stdout).into_owned();
        let mut out = lsc(&w);
        a.keys(b"\x1bP>|foot(1.2)\x1b\\");
        let end = Instant::now() + Duration::from_secs(5);
        while !lsc(&w).starts_with("foot") {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(20));
        }
        out.push_str(&lsc(&w));
        a.keys(b"x");
        a.wait(|_| pane_line(&w) == "x");
        outputs.push(out);
    }
    assert_eq!(outputs[0], outputs[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// Keys typed on the attached terminal reach a pane in modifyOtherKeys 1
/// or 2 extended, in `extended-keys-format csi-u` as the owner's config
/// sets it: C-S-a, S-Enter, C-Tab, M-C-Left and an Escape-prefixed key
/// (input-keys.c `input_key`). The pane copies its input to a file.
#[test]
fn attached_keys_reach_the_pane_in_its_key_mode() {
    let mut outputs = Vec::new();
    for bin in bins() {
        let mut out = Vec::new();
        for mode in ["", r"\033[>4;1m", r"\033[>4;2m"] {
            let w = World::new(bin.clone());
            let file = w.home.path().join("keys");
            let script = format!(
                "stty raw -echo; printf '{mode}'; printf R; exec cat > {}",
                file.display()
            );
            let mut a = w.attach(&[
                "set",
                "-s",
                "extended-keys",
                "on",
                ";",
                "set",
                "-s",
                "extended-keys-format",
                "csi-u",
                ";",
                "new",
                "-s",
                "a",
                "sh",
                "-c",
                &script,
            ]);
            a.wait(|a| pane_text(a).contains('R'));
            // C-S-a and S-Enter as CSI u, C-Tab, M-C-Left as xterm, then
            // Escape + a and a plain `.` to end.
            a.keys(b"\x1b[97;6u\x1b[13;2u\x1b[9;5u\x1b[1;7D\x1b[1;5D\x1ba.");
            let end = Instant::now() + Duration::from_secs(5);
            let bytes = loop {
                let bytes = std::fs::read(&file).unwrap_or_default();
                if bytes.ends_with(b".") {
                    break bytes;
                }
                assert!(Instant::now() < end, "{mode}: {bytes:?}");
                thread::sleep(Duration::from_millis(20));
            };
            out.push(format!(
                "{mode}: {}",
                String::from_utf8_lossy(&bytes).escape_debug()
            ));
        }
        outputs.push(out);
    }
    assert_eq!(
        outputs[1][2],
        r"\033[>4;2m: \u{1b}[97;6u\u{1b}[13;2u\u{1b}[9;5u\u{1b}[1;7D\u{1b}[1;5D\u{1b}[97;3u.",
        "tmux 3.8"
    );
    assert_eq!(outputs[0], outputs[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// A 40x10 client on session `a`: window 0 split left/right (`cat` panes,
/// pane 1 active), window 1 behind it, then `cmds`.
fn mouse_world(bin: PathBuf, cmds: &[&[&str]]) -> (World, Attached) {
    let w = World::new(bin);
    for cmd in [
        &[
            "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "cat",
        ][..],
        &["splitw", "-h", "-t", "a", "cat"],
        &["neww", "-d", "-t", "a", "-n", "x", "cat"],
        // The host name in the default status-right would move nothing
        // here, but a long one hides the window list on 40 columns.
        &["set", "-g", "status-right", ""],
    ]
    .iter()
    .chain(cmds)
    {
        assert!(w.cli(cmd).status.success(), "{cmd:?}");
    }
    let a = w.attach(&["attach", "-t", "a"]);
    a.wait(|a| a.text().contains("\x1b[?1049h"));
    a.wait(|a| pane_text(a).contains("[a]"));
    (w, a)
}

/// SGR press and release of button 1 at 1-based `x`, `y`.
fn click(x: u16, y: u16) -> Vec<u8> {
    format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").into_bytes()
}

/// Wait (at most 5 s) for `f` to be `want`; what it was last.
/// The sessions the attached clients are on, and every session.
fn client_sessions(w: &World) -> String {
    let lsc = w.cli(&["lsc", "-F", "#{client_session}"]);
    let ls = w.cli(&["ls", "-F", "#{session_name}"]);
    format!(
        "clients {:?} sessions {:?}",
        String::from_utf8_lossy(&lsc.stdout)
            .split_whitespace()
            .collect::<Vec<_>>(),
        String::from_utf8_lossy(&ls.stdout)
            .split_whitespace()
            .collect::<Vec<_>>(),
    )
}

/// cmd-attach-session.c:118-139 and cmd-new-session.c:335-344 (finding
/// `low_attach_session_new_session_from_a`): `attach` and `new` run from a key binding of
/// an attached client switch that client (no new terminal), and the commands
/// after them run. Also `attach -d` from a second terminal, whose first
/// client goes, and `attach -t a:1` selecting the window.
#[test]
fn attach_and_new_session_from_a_key_binding_switch_the_client() {
    let mut seen = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        for cmd in [
            &["new", "-d", "-s", "s1", "-n", "w"][..],
            &["new", "-d", "-s", "s2", "-n", "w"],
            &["neww", "-d", "-t", "s2:", "-n", "two"],
            &["set", "-g", "status-right", ""],
            &["bind", "a", "attach -t s2:1 ; set -g @e after"],
            &["bind", "n", "new -s s3 ; set -g @n after"],
            &["bind", "A", "new -A -s s1 ; set -g @A after"],
        ] {
            assert!(w.cli(cmd).status.success(), "{cmd:?}");
        }
        let mut got = Vec::new();
        let mut a = w.attach(&["attach", "-t", "s1"]);
        a.wait(|a| pane_text(a).contains("[s1]"));
        got.push(client_sessions(&w));
        for (key, session) in [(b"\x02a", "s2"), (b"\x02n", "s3"), (b"\x02A", "s1")] {
            a.keys(key);
            a.wait(|a| pane_text(a).contains(&format!("[{session}]")));
            got.push(client_sessions(&w));
        }
        got.push(format!(
            "{} {} {} {}",
            user_option(&w, "@e"),
            user_option(&w, "@n"),
            user_option(&w, "@A"),
            display_t(&w, "s2", "#{window_index}")
        ));
        // A second terminal with -d takes the session and the first goes.
        let mut b = w.attach(&["attach", "-d", "-t", "s1"]);
        a.exited(0);
        b.wait(|b| pane_text(b).contains("[s1]"));
        got.push(client_sessions(&w));
        b.keys(b"\x02d");
        b.exited(0);
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(
        seen[1],
        [
            r#"clients ["s1"] sessions ["s1", "s2"]"#,
            r#"clients ["s2"] sessions ["s1", "s2"]"#,
            r#"clients ["s3"] sessions ["s1", "s2", "s3"]"#,
            r#"clients ["s1"] sessions ["s1", "s2", "s3"]"#,
            "after after after 1",
            r#"clients ["s1"] sessions ["s1", "s2", "s3"]"#,
        ]
    );
}

/// server-client.c:211-226 and cmd-attach-session.c:75-79, cmd-new-session.c:
/// 195-198: a client run in one of the server's panes (its `$TMUX` set, its
/// terminal a pane) that would attach is refused; `new -d` and a
/// `switch-client` from a popup (the owner's session picker) work.
#[test]
fn nested_attach_is_refused_and_popup_switch_client_works() {
    let mut seen = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        let t = format!("{} -L {}", w.bin.display(), w.label);
        let script = w.home.path().join("nest.sh");
        std::fs::write(
            &script,
            format!(
                "for c in 'attach -t b' 'new -s c' 'new -A -s b' 'attach -d -t b' 'new -d -s d'; do\n\
                 echo \"[$c]\"; {t} $c 2>&1; echo rc=$?; done\n\
                 echo END; exec cat\n"
            ),
        )
        .unwrap();
        for cmd in [
            &[
                "new",
                "-d",
                "-s",
                "a",
                "-n",
                "w",
                "sh",
                script.to_str().unwrap(),
            ][..],
            &["new", "-d", "-s", "b", "-n", "w"],
            &["set", "-g", "status-right", ""],
            &["select-pane", "-t", "a", "-T", "t"],
        ] {
            assert!(w.cli(cmd).status.success(), "{cmd:?}");
        }
        let end = Instant::now() + Duration::from_secs(5);
        let mut text;
        loop {
            text = String::from_utf8_lossy(&w.cli(&["capture-pane", "-p", "-J", "-t", "a"]).stdout)
                .into_owned();
            if text.contains("END") || Instant::now() > end {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let mut got = vec![text.trim().to_owned(), client_sessions(&w)];
        // The popup's command is a client of the popup's terminal.
        let mut a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| pane_text(a).contains("[a]"));
        assert!(
            w.cli(&[
                "bind",
                "s",
                "display-popup",
                "-E",
                &format!("{t} switch-client -t =b")
            ])
            .status
            .success()
        );
        a.keys(b"\x02s");
        a.wait(|a| pane_text(a).contains("[b]"));
        got.push(client_sessions(&w));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    let nested = "sessions should be nested with care, unset $TMUX to force";
    assert_eq!(seen[1][0].matches(nested).count(), 4, "{}", seen[1][0]);
    assert!(
        seen[1][1].ends_with(r#"sessions ["a", "b", "d"]"#),
        "{}",
        seen[1][1]
    );
    assert_eq!(seen[1][2], r#"clients ["b"] sessions ["a", "b", "d"]"#);
}

fn settles(w: &World, f: &str, want: &str) -> String {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let got = display(w, f);
        if got == want || Instant::now() >= end {
            return got;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A global user option's value (`show -gv`), empty if unset.
fn user_option(w: &World, name: &str) -> String {
    String::from_utf8_lossy(&w.cli(&["show", "-gv", name]).stdout)
        .trim()
        .to_owned()
}

/// The mouse-mode sequences in `bytes`.
fn mouse_modes(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    let mut rest = &*text;
    while let Some(at) = rest.find("\x1b[?100") {
        let seq = &rest[at + 2..];
        let end = seq.find(['h', 'l']).map_or(seq.len(), |e| e + 1);
        if end <= 6 {
            out.push(seq[..end].to_owned());
        }
        rest = &rest[at + 2..];
    }
    out
}

/// `tty_update_mode` via `server_client_reset_state` (tty.c:904-920,
/// server-client.c:2294-2308): with `mouse` on (the default) the terminal
/// gets button mode, and all-motion while a pane of the window has 1003;
/// with it off the active pane's own mode. Each change clears all and sets
/// again.
#[test]
fn outer_mouse_modes_follow_the_mouse_option_and_panes() {
    let mut seen = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        // The pane prints what it reads, raw: its input is its modes.
        let a = w.attach(&[
            "new",
            "-s",
            "a",
            "-n",
            "w",
            "sh",
            "-c",
            "stty raw -echo; exec cat",
        ]);
        a.wait(|a| mouse_modes(&a.bytes.lock().unwrap()).contains(&"?1002h".to_owned()));
        thread::sleep(Duration::from_millis(300));
        let mut marks = vec![a.bytes.lock().unwrap().len()];
        let steps: [&[&str]; 5] = [
            &["set", "-g", "mouse", "off"],
            &["send", "-t", "a", "-l", "\x1b[?1000h"],
            &["set", "-g", "mouse", "on"],
            &["send", "-t", "a", "-l", "\x1b[?1003h"],
            &["send", "-t", "a", "-l", "\x1b[?1000l"],
        ];
        for step in steps {
            assert!(w.cli(step).status.success());
            thread::sleep(Duration::from_millis(400));
            marks.push(a.bytes.lock().unwrap().len());
        }
        let bytes = a.bytes.lock().unwrap().clone();
        // Attaching, the modes set; how often depends on how many full
        // redraws the attach made (tmux usually two, vtmux one when the
        // first waits for the startup output to be written,
        // server_client_check_redraw), which leaves the terminal the same.
        let mut on = Vec::new();
        for m in mouse_modes(&bytes[..marks[0]]) {
            if m.ends_with('h') && !on.contains(&m) {
                on.push(m);
            }
        }
        let mut got = vec![on.join(" ")];
        for pair in marks.windows(2) {
            got.push(mouse_modes(&bytes[pair[0]..pair[1]]).join(" "));
        }
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1][1], "?1006l ?1000l ?1002l ?1003l");
    assert_eq!(seen[1][2], "?1006l ?1000l ?1002l ?1003l ?1006h ?1000h");
}

/// `server_client_reset_state` with a window menu (server-client.c:
/// 2201-2203, menu.c:633-634): the menu's screen gives the modes, all
/// motion when it takes the mouse (opened by a mouse event or with -M),
/// none when it does not, whatever `mouse` is; `mouse on` then adds
/// button mode (server-client.c:2294-2308) but not, under a menu, the
/// panes' all-motion.
#[test]
fn outer_mouse_modes_under_a_menu() {
    let mut seen = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        let mut a = w.attach(&["new", "-s", "a", "-n", "w", "cat"]);
        a.wait(|a| mouse_modes(&a.bytes.lock().unwrap()).contains(&"?1002h".to_owned()));
        thread::sleep(Duration::from_millis(300));
        let mut marks = vec![a.bytes.lock().unwrap().len()];
        let menu = |m: &str| -> Vec<String> {
            let mut v = vec!["display-menu".to_owned()];
            v.extend(m.split_whitespace().map(str::to_owned));
            v.extend(["-x", "0", "-y", "5", "A", "a", ""].map(str::to_owned));
            v
        };
        // A CLI step, or None: `q` typed to the client, closing the menu.
        let steps: [Option<Vec<String>>; 7] = [
            Some(menu("")),
            None,
            Some(menu("-M")),
            None,
            Some(vec![
                "set".into(),
                "-g".into(),
                "mouse".into(),
                "off".into(),
            ]),
            Some(menu("-M")),
            None,
        ];
        for step in steps {
            match step {
                Some(args) => {
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    assert!(w.cli(&args).status.success());
                }
                None => a.keys(b"q"),
            }
            thread::sleep(Duration::from_millis(400));
            marks.push(a.bytes.lock().unwrap().len());
        }
        let bytes = a.bytes.lock().unwrap().clone();
        let mut got = Vec::new();
        for pair in marks.windows(2) {
            got.push(mouse_modes(&bytes[pair[0]..pair[1]]).join(" "));
        }
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    let all = "?1006l ?1000l ?1002l ?1003l ?1006h ?1000h ?1002h ?1003h";
    let button = "?1006l ?1000l ?1002l ?1003l ?1006h ?1000h ?1002h";
    let off = "?1006l ?1000l ?1002l ?1003l";
    assert_eq!(seen[1], ["", "", all, button, off, all, off]);
}

/// `server_client_check_mouse` and the default bindings: a click selects the
/// pane under it (`MouseDown1Pane`), a click on a window in the status line
/// switches to it (`MouseDown1Status`), the wheel over the status line
/// moves between windows (`WheelUpStatus`, `WheelDownStatus`).
#[test]
fn clicks_select_panes_and_windows() {
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = mouse_world(bin, &[]);
        let mut got = vec![display(&w, "#{pane_index}")];
        a.keys(&click(3, 3));
        got.push(settles(&w, "#{pane_index}", "0"));
        a.keys(&click(30, 3));
        got.push(settles(&w, "#{pane_index}", "1"));
        // "[a] 0:w* 1:x": window 1 is at columns 10-12.
        a.keys(&click(11, 10));
        got.push(settles(&w, "#{window_index}", "1"));
        // The wheel over a window in the list.
        a.keys(b"\x1b[<64;6;10M");
        got.push(settles(&w, "#{window_index}", "0"));
        a.keys(b"\x1b[<65;6;10M");
        got.push(settles(&w, "#{window_index}", "1"));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1], ["1", "0", "1", "1", "0", "1"]);
}

/// The legacy X10 report (`ESC [ M` and three bytes from 32/33,
/// tty-keys.c:1246-1271) clicks too, positions past 0x7f included.
#[test]
fn x10_reports_click() {
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = mouse_world(bin, &[&["resize-window", "-t", "a", "-x", "140"]]);
        let mut got = vec![display(&w, "#{pane_index}")];
        // Press at 3,3 (bytes 0x20+0, 0x20+3, 0x20+3), release.
        a.keys(b"\x1b[M\x20\x23\x23\x1b[M\x23\x23\x23");
        got.push(settles(&w, "#{pane_index}", "0"));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1], ["1", "0"]);
}

/// A binding's commands run with the mouse event: `=` is the pane under
/// the pointer and the mouse formats describe it (format.c:2032-2125,
/// cmd-find.c:1027-1056); on the status line, the status formats.
#[test]
fn mouse_formats_and_target_in_a_binding() {
    let f =
        "#{mouse_x},#{mouse_y},#{mouse_pane},#{mouse_status_line},#{mouse_status_range}|#{pane_id}";
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = mouse_world(
            bin,
            &[
                &["bind", "-n", "MouseDown1Pane", "set", "-gF", "@m", f],
                &["bind", "-n", "MouseDown1Status", "set", "-gF", "@m", f],
                &["bind", "-n", "MouseDown1StatusLeft", "set", "-gF", "@m", f],
                &[
                    "bind",
                    "-n",
                    "MouseDown3Pane",
                    "set",
                    "-gF",
                    "@m",
                    "#{pane_id}",
                ],
            ],
        );
        let mut got = Vec::new();
        for (x, y) in [(5, 4), (25, 2), (11, 10), (2, 10)] {
            w.cli(&["set", "-gu", "@m"]);
            a.keys(&click(x, y));
            a.wait(|_| !user_option(&w, "@m").is_empty());
            got.push(user_option(&w, "@m"));
        }
        // The binding's current target is the pane under the pointer.
        w.cli(&["set", "-gu", "@m"]);
        a.keys(b"\x1b[<2;5;4M\x1b[<2;5;4m");
        a.wait(|_| !user_option(&w, "@m").is_empty());
        got.push(user_option(&w, "@m"));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(
        seen[1],
        [
            "4,3,%0,,|%0",
            "3,1,%1,,|%1",
            "10,0,%2,0,window|%2",
            "1,0,%1,0,left|%1",
            "%0"
        ]
    );
}

/// `server_client_click_timer`: a second click is `SecondClick` at once and
/// `DoubleClick` only once the 300 ms timeout passes without a third; a
/// third click is `TripleClick` at once and no `DoubleClick` follows.
#[test]
fn double_click_waits_for_the_timeout_triple_click_does_not() {
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = mouse_world(
            bin,
            &[
                &["bind", "-n", "SecondClick1Pane", "set", "-g", "@s", "1"],
                &["bind", "-n", "DoubleClick1Pane", "set", "-g", "@d", "1"],
                &["bind", "-n", "TripleClick1Pane", "set", "-g", "@t", "1"],
            ],
        );
        let mut c = click(5, 4);
        c.extend(click(5, 4));
        a.keys(&c);
        a.wait(|_| user_option(&w, "@s") == "1");
        let early = user_option(&w, "@d");
        a.wait(|_| user_option(&w, "@d") == "1");
        let mut got = vec![format!("double: early {early:?}")];
        w.cli(&["set", "-gu", "@d"]);
        w.cli(&["set", "-gu", "@s"]);
        thread::sleep(Duration::from_millis(400));
        let mut c = click(5, 4);
        c.extend(click(5, 4));
        c.extend(click(5, 4));
        a.keys(&c);
        a.wait(|_| user_option(&w, "@t") == "1");
        thread::sleep(Duration::from_millis(500));
        got.push(format!(
            "triple: s {:?} d {:?}",
            user_option(&w, "@s"),
            user_option(&w, "@d")
        ));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1], ["double: early \"\"", "triple: s \"1\" d \"\""]);
}

/// The owner's `bind -n MouseDown1StatusLeft choose-tree -s`: a click on
/// the session name opens the tree; in the tree a click selects a row and
/// a double click chooses it (mode-tree.c:1582-1617).
#[test]
fn status_left_click_opens_choose_tree_and_rows_click() {
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = mouse_world(
            bin,
            &[
                &["new", "-d", "-s", "b", "-n", "bw", "cat"],
                &["bind", "-n", "MouseDown1StatusLeft", "choose-tree", "-s"],
            ],
        );
        let mut got = Vec::new();
        a.keys(&click(2, 10));
        got.push(settles(&w, "#{pane_mode}", "tree-mode"));
        // The tree is in the active (right) pane, rows "(0) + a: ..." and
        // "(1) + b: ...". A click on the second selects it, a double click
        // chooses session b.
        a.keys(&click(26, 2));
        thread::sleep(Duration::from_millis(400));
        got.push(pane_text(&a).lines().nth(1).unwrap_or("").to_owned());
        let mut c = click(26, 2);
        c.extend(click(26, 2));
        a.keys(&c);
        let end = Instant::now() + Duration::from_secs(5);
        let session = || {
            String::from_utf8_lossy(&w.cli(&["display", "-p", "#{client_session}"]).stdout)
                .trim()
                .to_owned()
        };
        while session() != "b" && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        got.push(session());
        got.push(display(&w, "#{pane_mode}"));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1][2..], ["b", ""]);
}

/// With `status-position top` the status line is row 1 and the panes start
/// below it (server-client.c:1029-1035).
#[test]
fn status_position_top_moves_the_window_down() {
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = mouse_world(bin, &[&["set", "-g", "status-position", "top"]]);
        let mut got = vec![];
        a.keys(&click(3, 2));
        got.push(settles(&w, "#{pane_index}", "0"));
        a.keys(&click(11, 1));
        got.push(settles(&w, "#{window_index}", "1"));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1], ["0", "1"]);
}

/// A read-only client's mouse events do nothing (server-client.c:1477).
#[test]
fn read_only_client_drops_mouse_events() {
    let mut seen = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        for cmd in [
            &[
                "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "cat",
            ][..],
            &["splitw", "-h", "-t", "a", "cat"],
            &["set", "-g", "status-right", ""],
        ] {
            assert!(w.cli(cmd).status.success());
        }
        let mut a = w.attach(&["attach", "-r", "-t", "a"]);
        a.wait(|a| pane_text(a).contains("[a]"));
        a.keys(&click(3, 3));
        thread::sleep(Duration::from_millis(400));
        seen.push(display(&w, "#{pane_index}"));
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1], "1");
}

/// A pane program that turns on the mouse modes `modes` (printf escapes),
/// prints [`RECORDING`], then copies what it reads, raw, to `file`.
fn mouse_recorder(file: &std::path::Path, modes: &str) -> String {
    format!(
        "stty raw -echo; printf '{modes}{RECORDING}'; exec cat > {}",
        file.display()
    )
}

/// What [`mouse_recorder`] prints after its modes.
const RECORDING: &str = "recording";

/// Wait for a [`mouse_recorder`] on `a`'s screen: the server has its
/// modes, so the next event goes by them, not by the shell's. The
/// recording file existing is not enough: it can be there before the
/// server has read the modes (CI flake of
/// `wheel_up_pane_enters_copy_mode_or_forwards`).
fn recorder_ready(a: &Attached, file: &std::path::Path) {
    a.wait(|a| pane_text(a).contains(RECORDING));
    let end = Instant::now() + Duration::from_secs(5);
    while !file.exists() {
        assert!(Instant::now() < end, "no {}", file.display());
        thread::sleep(Duration::from_millis(20));
    }
}

/// Type `.` and wait (at most 5 s) for `file` to end with it: everything
/// before it has reached the pane. The bytes before the `.`; a timeout
/// names case `what` and shows the screen.
fn recorded(a: &mut Attached, file: &std::path::Path, what: &str) -> String {
    a.keys(b".");
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let bytes = std::fs::read(file).unwrap_or_default();
        if let Some(b) = bytes.strip_suffix(b".") {
            return String::from_utf8_lossy(b).escape_debug().to_string();
        }
        assert!(
            Instant::now() < end,
            "{what}: the `.` never reached the pane; recorded {bytes:?}, screen:\n{}",
            pane_text(a)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The events [`mouse_forwarding_matches_tmux`] sends, as SGR or X10 at
/// 1-based client cells: a click on the left pane (no mouse modes), then
/// on the recorder: press, drag, release (a drag end), motion with no
/// button, wheel up and down, buttons 2 and 3, a C- press.
fn forwarded_events(x10: bool) -> Vec<u8> {
    let events: [(u32, u32, u32, bool); 14] = [
        (0, 5, 4, false),
        (0, 5, 4, true),
        (0, 25, 4, false),
        (32, 26, 5, false),
        (0, 26, 5, true),
        (35, 27, 6, false),
        (64, 25, 4, false),
        (65, 25, 4, false),
        (1, 24, 3, false),
        (1, 24, 3, true),
        (2, 28, 7, false),
        (2, 28, 7, true),
        (16, 40, 10, false),
        (16, 40, 10, true),
    ];
    let mut out = Vec::new();
    for (b, x, y, release) in events {
        if x10 {
            let b = if release { 3 } else { b };
            out.extend_from_slice(b"\x1b[M");
            out.extend([b + 32, x + 32, y + 32].map(|v| u8::try_from(v).unwrap()));
        } else {
            let end = if release { 'm' } else { 'M' };
            out.extend(format!("\x1b[<{b};{x};{y}{end}").into_bytes());
        }
    }
    out
}

/// input-keys.c `input_key_get_mouse` through the default bindings
/// (`send -M`) and the unbound path (`input_key_pane`, only to the pane
/// under the pointer): the bytes a program gets for each of 1000, 1002 and
/// 1003 with no extension, 1005 or 1006, for SGR and X10 reports from the
/// terminal, at its own position (the right pane, below a top status line);
/// and with `mouse off`, where every event is forwarded unbound.
#[test]
fn mouse_forwarding_matches_tmux() {
    let mut seen = Vec::new();
    for bin in bins() {
        let mut got = Vec::new();
        let mut cases = Vec::new();
        for tracking in ["1000", "1002", "1003"] {
            for ext in ["", "1005", "1006"] {
                for x10 in [false, true] {
                    cases.push((tracking, ext, x10, "on"));
                }
            }
        }
        cases.push(("1002", "1006", false, "off"));
        cases.push(("1003", "", true, "off"));
        for (tracking, ext, x10, mouse) in cases {
            let w = World::new(bin.clone());
            let file = w.home.path().join("mouse");
            let mut modes = format!(r"\033[?{tracking}h");
            if !ext.is_empty() {
                write!(modes, r"\033[?{ext}h").unwrap();
            }
            let script = mouse_recorder(&file, &modes);
            for cmd in [
                &[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "cat",
                ][..],
                &["splitw", "-h", "-t", "a", "sh", "-c", &script],
                &["set", "-g", "status-right", ""],
                &["set", "-g", "status-position", "top"],
                &["set", "-g", "mouse", mouse],
            ] {
                assert!(w.cli(cmd).status.success(), "{cmd:?}");
            }
            let end = Instant::now() + Duration::from_secs(5);
            while display_t(&w, "a:0.1", "#{mouse_any_flag}") != "1" {
                assert!(Instant::now() < end);
                thread::sleep(Duration::from_millis(20));
            }
            let mut a = w.attach(&["attach", "-t", "a"]);
            a.wait(|a| pane_text(a).contains("[a]"));
            a.keys(&forwarded_events(x10));
            let what = format!(
                "{tracking} {ext:4} {} mouse {mouse}",
                if x10 { "x10" } else { "sgr" }
            );
            got.push(format!("{what}: {}", recorded(&mut a, &file, &what)));
        }
        seen.push(got);
    }
    assert_eq!(
        seen[1][16],
        concat!(
            r"1003 1006 sgr mouse on: \u{1b}[<0;4;3M\u{1b}[<32;5;4M\u{1b}[<0;5;4m",
            r"\u{1b}[<35;6;5M\u{1b}[<64;4;3M\u{1b}[<65;4;3M\u{1b}[<1;3;2M\u{1b}[<1;3;2m",
            r"\u{1b}[<2;7;6M\u{1b}[<2;7;6m\u{1b}[<16;19;9M\u{1b}[<16;19;9m"
        ),
        "tmux 3.8"
    );
    for (v, t) in seen[0].iter().zip(&seen[1]) {
        assert_eq!(v, t, "vtmux (left) vs tmux 3.8 (right)");
    }
}

/// input-keys.c:767-789: X10 clamps a position past 0xff - 0x21 to 0xff;
/// UTF-8 (1005) writes it as two bytes; SGR (1006) needs an SGR report
/// from the terminal and falls back to X10 for an X10 one.
#[test]
fn mouse_forwarding_far_columns() {
    let mut seen = Vec::new();
    for bin in bins() {
        let mut got = Vec::new();
        for (ext, report) in [
            ("", "sgr"),
            ("1005", "sgr"),
            ("1006", "sgr"),
            ("1006", "x10"),
        ] {
            let w = World::new(bin.clone());
            let file = w.home.path().join("mouse");
            let mut modes = r"\033[?1000h".to_owned();
            if !ext.is_empty() {
                write!(modes, r"\033[?{ext}h").unwrap();
            }
            let mut a = w.attach_sized(
                &[
                    "set",
                    "-g",
                    "status",
                    "off",
                    ";",
                    "new",
                    "-s",
                    "a",
                    "-n",
                    "w",
                    "sh",
                    "-c",
                    &mouse_recorder(&file, &modes),
                ],
                240,
                5,
            );
            let end = Instant::now() + Duration::from_secs(5);
            while display(&w, "#{mouse_any_flag}") != "1" {
                assert!(Instant::now() < end);
                thread::sleep(Duration::from_millis(20));
            }
            // A click at column 230, row 2.
            if report == "sgr" {
                a.keys(b"\x1b[<0;230;2M\x1b[<0;230;2m");
            } else {
                a.keys(b"\x1b[M\x20\xff\x22\x1b[M\x23\xff\x22");
            }
            let what = format!("{ext} {report}");
            got.push(format!("{what}: {}", recorded(&mut a, &file, &what)));
        }
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
}

/// The default `WheelUpPane` (`if -F '#{||:#{alternate_on},#{pane_in_mode},
/// #{mouse_any_flag}}' { send -M } { copy-mode -e }`): over a shell the
/// wheel enters copy mode; a pane with `?1000h` gets it; on the alternate
/// screen with no mouse mode nothing happens (tmux does not make arrow
/// keys of it). Motion in the prefix table goes to a `?1003h` pane and
/// keeps the prefix (server-client.c:1655-1661).
#[test]
fn wheel_up_pane_enters_copy_mode_or_forwards() {
    let mut seen = Vec::new();
    for bin in bins() {
        let mut got = Vec::new();
        for modes in [
            "",
            r"\033[?1000h",
            r"\033[?1049h",
            r"\033[?1003h\033[?1006h",
        ] {
            let w = World::new(bin.clone());
            let file = w.home.path().join("mouse");
            let mut a = w.attach(&[
                "set",
                "-g",
                "status-right",
                "",
                ";",
                "bind",
                "x",
                "set",
                "-g",
                "@p",
                "1",
                ";",
                "new",
                "-s",
                "a",
                "-n",
                "w",
                "sh",
                "-c",
                &mouse_recorder(&file, modes),
            ]);
            a.wait(|a| pane_text(a).contains("[a]"));
            recorder_ready(&a, &file);
            a.keys(b"\x1b[<64;5;3M");
            if modes.contains("1003") {
                a.keys(b"\x02\x1b[<35;6;4Mx");
                a.wait(|_| user_option(&w, "@p") == "1");
            }
            // Over the shell: copy mode, then out of it so `.` reaches the
            // pane. Otherwise the `.` after the wheel shows it was handled.
            let mode = if modes.is_empty() {
                let mode = settles(&w, "#{pane_in_mode}", "1");
                a.keys(b"q");
                settles(&w, "#{pane_in_mode}", "0");
                mode
            } else {
                String::new()
            };
            let bytes = recorded(&mut a, &file, &format!("modes {modes:?}"));
            let mode = if mode.is_empty() {
                display(&w, "#{pane_in_mode}")
            } else {
                mode
            };
            got.push(format!(
                "{modes}: in mode {mode}, alternate {}, got {}",
                display(&w, "#{alternate_on}"),
                bytes
            ));
        }
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(
        seen[1],
        [
            ": in mode 1, alternate 0, got ",
            r"\033[?1000h: in mode 0, alternate 0, got \u{1b}[M`%#",
            r"\033[?1049h: in mode 0, alternate 1, got ",
            r"\033[?1003h\033[?1006h: in mode 0, alternate 0, got \u{1b}[<64;5;3M\u{1b}[<35;6;4M",
        ]
    );
}

/// The layout and pane sizes of session `a`'s current window.
fn geometry(w: &World) -> String {
    String::from_utf8_lossy(
        &w.cli(&[
            "list-panes",
            "-t",
            "a",
            "-F",
            "#{pane_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}",
        ])
        .stdout,
    )
    .trim()
    .to_owned()
}

/// Wait (at most 5 s) for `read` to stop changing and differ from
/// `before`; what it was last.
fn changed(before: &str, read: impl Fn() -> String) -> String {
    let end = Instant::now() + Duration::from_secs(5);
    let mut last = read();
    while last == before && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
        last = read();
    }
    stable(Duration::from_millis(300), read)
}

/// `MouseDrag1Border { resize-pane -M }` (cmd-resize-pane.c:194-221,
/// 357-420): dragging the vertical border moves it with the mouse, and so
/// does the horizontal one in a nested split; the drag keeps resizing
/// (`KEYC_DRAGGING`) until the release.
#[test]
fn border_drag_resizes_panes() {
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = mouse_world(bin, &[&["splitw", "-v", "-t", "a:0.1", "cat"]]);
        let mut got = vec![geometry(&w)];
        // The vertical border is column 20 (1-based 21); drag it to 15,
        // then 12, then release.
        a.keys(b"\x1b[<0;21;3M\x1b[<32;18;3M\x1b[<32;15;3M\x1b[<32;12;3M\x1b[<0;12;3m");
        got.push(changed(&got[0], || geometry(&w)));
        // The horizontal border of the right column is row 4 (1-based 5):
        // up two rows.
        let before = got[1].clone();
        a.keys(b"\x1b[<0;30;5M\x1b[<32;30;4M\x1b[<32;30;3M\x1b[<0;30;3m");
        got.push(changed(&before, || geometry(&w)));
        // Back to the right, past where it started.
        let before = got[2].clone();
        a.keys(b"\x1b[<0;13;3M\x1b[<32;15;3M\x1b[<32;19;3M\x1b[<32;25;3M\x1b[<0;25;3m");
        got.push(changed(&before, || geometry(&w)));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(seen[1][1], "%0 0,0 11x9\n%1 12,0 28x4\n%3 12,5 28x4");
    assert_eq!(seen[1][2], "%0 0,0 11x9\n%1 12,0 28x2\n%3 12,3 28x6");
    assert_eq!(seen[1][3], "%0 0,0 23x9\n%1 24,0 16x2\n%3 24,3 16x6");
}

/// One write holding a whole border drag (a terminal under load batches
/// motion reports into one read): every motion still fires
/// `window-layout-changed` and a control `%layout-change` with the layout
/// at that step, the clamped ones too (cmd-resize-pane.c:357-420 runs per
/// `KEYC_DRAGGING` event). Hook count and control sequence match tmux 3.8.
#[test]
fn border_drag_in_one_read_fires_each_step() {
    // Press, motions, release; SGR 1-based.
    let drags: [(&str, &[&str], &str); 3] = [
        (
            "horizontal",
            &[],
            "\x1b[<0;21;3M\x1b[<32;18;3M\x1b[<32;15;3M\x1b[<32;12;3M\x1b[<32;9;3M\x1b[<0;9;3m",
        ),
        // 21>10>1>30: the 1-column step is clamped and still reported.
        (
            "clamped",
            &[],
            "\x1b[<0;21;3M\x1b[<32;10;3M\x1b[<32;1;3M\x1b[<32;30;3M\x1b[<0;30;3m",
        ),
        (
            "vertical",
            &["splitw", "-v", "-t", "a:0.1", "cat"],
            "\x1b[<0;30;5M\x1b[<32;30;7M\x1b[<32;30;9M\x1b[<32;30;3M\x1b[<0;30;3m",
        ),
    ];
    for (what, split, bytes) in drags {
        let mut seen = Vec::new();
        for bin in bins() {
            let mut cmds: Vec<&[&str]> = Vec::new();
            if !split.is_empty() {
                cmds.push(split);
            }
            cmds.push(&["set", "-g", "@n", ""]);
            cmds.push(&[
                "set-hook",
                "-g",
                "window-layout-changed",
                "set -gaF @n ' L'",
            ]);
            let (w, mut a) = mouse_world(bin, &cmds);
            let mut control = Command::new(&w.bin)
                .args(w.args())
                .args(["-C", "attach", "-t", "a"])
                .env_clear()
                .envs(w.env())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut out = control.stdout.take().unwrap();
            let reader = thread::spawn(move || {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut out, &mut s);
                s
            });
            // The control client is attached once it is listed.
            let start = Instant::now();
            while String::from_utf8_lossy(&w.cli(&["lsc"]).stdout)
                .lines()
                .count()
                < 2
            {
                assert!(start.elapsed() < Duration::from_secs(5), "{what}");
                thread::sleep(Duration::from_millis(20));
            }
            assert!(w.cli(&["set", "-g", "@n", ""]).status.success());
            a.keys(bytes.as_bytes());
            let hooks = stable(Duration::from_millis(500), || {
                String::from_utf8_lossy(&w.cli(&["show", "-gv", "@n"]).stdout).into_owned()
            });
            drop(control.stdin.take());
            let _ = control.wait();
            let layouts: Vec<String> = reader
                .join()
                .unwrap()
                .lines()
                .filter(|l| l.starts_with("%layout-change"))
                .map(|l| l.split(' ').nth(2).unwrap_or_default().to_owned())
                .collect();
            seen.push((hooks, layouts, geometry(&w)));
        }
        assert_eq!(seen[0], seen[1], "{what}: vtmux (left) vs tmux 3.8 (right)");
        let (hooks, layouts, _) = &seen[1];
        assert_eq!(
            hooks.trim().split(' ').count(),
            layouts.len(),
            "{what}: one hook per %layout-change: {hooks:?} {layouts:?}"
        );
        assert!(layouts.len() >= 3, "{what}: {layouts:?}");
    }
}

/// A 40x10 client on session `a`, one pane printing `lines` then reading
/// raw (so the screen is the lines), status-right and the pane title
/// pinned, then `cmds`.
fn text_world(bin: PathBuf, lines: &str, cmds: &[&[&str]]) -> (World, Attached) {
    let w = World::new(bin);
    let script = format!("stty raw -echo; printf '{lines}'; exec cat");
    for cmd in [
        &[
            "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "sh", "-c", &script,
        ][..],
        &["set", "-g", "status-right", ""],
        &["select-pane", "-t", "a", "-T", "t"],
    ]
    .iter()
    .chain(cmds)
    {
        assert!(w.cli(cmd).status.success(), "{cmd:?}");
    }
    let a = w.attach(&["attach", "-t", "a"]);
    a.wait(|a| pane_text(a).contains("[a]"));
    a.wait(|a| pane_text(a).contains("gamma"));
    (w, a)
}

/// The top paste buffer, or "" with none.
fn top_buffer(w: &World) -> String {
    String::from_utf8_lossy(&w.cli(&["show-buffer"]).stdout).into_owned()
}

/// Wait (at most 5 s) for a paste buffer; the top one.
fn buffer_settles(w: &World) -> String {
    let end = Instant::now() + Duration::from_secs(5);
    while top_buffer(w).is_empty() && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    top_buffer(w)
}

const TEXT: &str = "alpha beta-gamma delta\\r\\nsecond line here\\r\\ngamma\\r\\n";

/// The default `DoubleClick1Pane` and `TripleClick1Pane` from a shell:
/// `copy-mode -H; send -X select-word` (or `select-line`) at the pointer,
/// not at the cursor (`window_copy_move_mouse`, window-copy.c:3911), then
/// `copy-pipe-and-cancel` 0.3 s later.
#[test]
fn double_and_triple_click_copy_under_the_pointer() {
    let mut seen = Vec::new();
    for bin in bins() {
        let mut got = Vec::new();
        for clicks in [2, 3] {
            let (w, mut a) = text_world(bin.clone(), TEXT, &[]);
            // "beta" on the first row, column 8 (1-based 9).
            let mut c = Vec::new();
            for _ in 0..clicks {
                c.extend(click(9, 1));
            }
            a.keys(&c);
            got.push(buffer_settles(&w));
            got.push(settles(&w, "#{pane_in_mode}", "0"));
        }
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(seen[1], ["beta", "0", "alpha beta-gamma delta", "0"]);
}

/// `MouseDrag1Pane { ... copy-mode -M }` from a shell enters copy mode and
/// starts a selection at the press (`window_copy_start_drag`); the drag
/// extends it and `MouseDragEnd1Pane { send -X copy-pipe-and-cancel }`
/// copies it and leaves.
#[test]
fn drag_from_a_shell_selects_and_copies() {
    let mut seen = Vec::new();
    for bin in bins() {
        let mut got = Vec::new();
        for vi in ["emacs", "vi"] {
            let (w, mut a) = text_world(bin.clone(), TEXT, &[&["set", "-g", "mode-keys", vi]]);
            // From "beta" (column 6, row 0) to "line" (column 9, row 1).
            a.keys(b"\x1b[<0;7;1M\x1b[<32;8;1M\x1b[<32;4;2M\x1b[<32;10;2M\x1b[<0;10;2m");
            got.push(format!("{vi}: {:?}", buffer_settles(&w)));
            got.push(settles(&w, "#{pane_in_mode}", "0"));
        }
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(
        seen[1],
        [
            r#"emacs: "beta-gamma delta\nsecond li""#,
            "0",
            r#"vi: "beta-gamma delta\nsecond lin""#,
            "0"
        ]
    );
}

/// In copy mode, `MouseDrag1Pane { select-pane; send -X begin-selection }`
/// starts a drag at the press; double and triple click there select the
/// word and line under the pointer; the wheel scrolls five lines
/// (`send -N5 -X scroll-up`/`scroll-down`).
#[test]
fn copy_mode_mouse_bindings() {
    let mut lines = String::new();
    for i in 0..30 {
        let _ = write!(lines, "line {i:02}\\r\\n");
    }
    lines.push_str("gamma\\r\\n");
    let mut seen = Vec::new();
    for bin in bins() {
        let (w, mut a) = text_world(bin, &lines, &[]);
        let mut got = Vec::new();
        assert!(w.cli(&["copy-mode", "-t", "a"]).status.success());
        let pos = "#{scroll_position} #{copy_cursor_x},#{copy_cursor_y}";
        got.push(display(&w, pos));
        a.keys(b"\x1b[<64;5;4M");
        got.push(settles(&w, "#{scroll_position}", "5"));
        a.keys(b"\x1b[<64;5;4M\x1b[<65;5;4M\x1b[<64;5;4M");
        thread::sleep(Duration::from_millis(400));
        got.push(display(&w, pos));
        // A drag in copy mode: from row 1 column 2 to row 3 column 4.
        a.keys(b"\x1b[<0;3;2M\x1b[<32;3;3M\x1b[<32;5;4M\x1b[<0;5;4m");
        got.push(buffer_settles(&w));
        got.push(settles(&w, "#{pane_in_mode}", "0"));
        // Double click on a word in copy mode.
        assert!(w.cli(&["delete-buffer"]).status.success());
        assert!(w.cli(&["copy-mode", "-t", "a"]).status.success());
        a.keys(b"\x1b[<64;5;4M");
        thread::sleep(Duration::from_millis(400));
        let mut c = click(7, 3);
        c.extend(click(7, 3));
        a.keys(&c);
        got.push(buffer_settles(&w));
        // Triple click: the whole line.
        assert!(w.cli(&["delete-buffer"]).status.success());
        thread::sleep(Duration::from_millis(400));
        assert!(w.cli(&["copy-mode", "-t", "a"]).status.success());
        let mut c = click(2, 6);
        c.extend(click(2, 6));
        c.extend(click(2, 6));
        a.keys(&c);
        got.push(buffer_settles(&w));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    assert_eq!(
        seen[1],
        [
            "0 0,8",
            "5",
            "10 7,8",
            "ne 14\nline 15\nline",
            "0",
            "20",
            "line 28"
        ]
    );
}

/// The default pane bindings with drag callbacks and forwarding together:
/// a pane with no mouse mode beside one with `?1000h`. `MouseDrag1Pane`
/// (`if -F '#{||:#{pane_in_mode},#{mouse_any_flag}}' { send -M }
/// { copy-mode -M }`) drag-selects in the first and is forwarded to the
/// second; `DoubleClick1Pane` copies the word in the first and is
/// forwarded to the second; `WheelUpPane` enters copy mode in the first
/// and is forwarded to the second; `MouseDrag1Border` still resizes.
#[test]
fn default_bindings_drag_or_forward() {
    let mut seen = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        let file = w.home.path().join("mouse");
        let text = format!("stty raw -echo; printf '{TEXT}'; exec cat");
        let recorder = mouse_recorder(&file, r"\033[?1000h");
        // The text pane is made at its final width: printed at 40 columns
        // and then split, its first line would wrap on the resize and (in
        // tmux, keeping the cursor row) go to history, or not, by timing.
        for cmd in [
            &[
                "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "sh", "-c", &recorder,
            ][..],
            &["splitw", "-hb", "-t", "a", "sh", "-c", &text],
            &["set", "-g", "status-right", ""],
            &["select-pane", "-t", "a:0.0", "-T", "t"],
            &["select-pane", "-t", "a:0.1", "-T", "t"],
        ] {
            assert!(w.cli(cmd).status.success(), "{cmd:?}");
        }
        let end = Instant::now() + Duration::from_secs(5);
        while display_t(&w, "a:0.1", "#{mouse_any_flag}") != "1" {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(20));
        }
        let mut a = w.attach(&["attach", "-t", "a"]);
        a.wait(|a| pane_text(a).contains("[a]"));
        a.wait(|a| pane_text(a).contains("gamma"));
        let mut got = Vec::new();
        // Drag in the left pane: "beta" (column 6, row 0) to row 1.
        a.keys(b"\x1b[<0;7;1M\x1b[<32;8;1M\x1b[<32;4;2M\x1b[<0;4;2m");
        got.push(format!("drag {:?}", buffer_settles(&w)));
        got.push(settles(&w, "#{pane_in_mode}", "0"));
        thread::sleep(Duration::from_millis(400));
        // Double click on "beta" in the left pane.
        assert!(w.cli(&["delete-buffer"]).status.success());
        let mut c = click(9, 1);
        c.extend(click(9, 1));
        a.keys(&c);
        got.push(format!("double {:?}", buffer_settles(&w)));
        got.push(settles(&w, "#{pane_in_mode}", "0"));
        thread::sleep(Duration::from_millis(400));
        // The wheel over the left pane: copy mode.
        a.keys(b"\x1b[<64;5;3M");
        let end = Instant::now() + Duration::from_secs(5);
        while display_t(&w, "a:0.0", "#{pane_in_mode}") != "1" && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        got.push(format!(
            "wheel left {}",
            display_t(&w, "a:0.0", "#{pane_in_mode}")
        ));
        assert!(
            w.cli(&["send", "-t", "a:0.0", "-X", "cancel"])
                .status
                .success()
        );
        thread::sleep(Duration::from_millis(400));
        // The right pane: a press (which selects it), a drag, a release,
        // the wheel both ways, a double click; all to the program.
        a.keys(b"\x1b[<0;25;3M\x1b[<32;26;4M\x1b[<32;27;4M\x1b[<0;27;4m");
        a.keys(b"\x1b[<64;25;3M\x1b[<65;25;3M");
        thread::sleep(Duration::from_millis(400));
        let mut c = click(30, 5);
        c.extend(click(30, 5));
        a.keys(&c);
        got.push(format!("right {}", recorded(&mut a, &file, "right pane")));
        got.push(format!(
            "right mode {}, buffer {:?}",
            display_t(&w, "a:0.1", "#{pane_in_mode}"),
            top_buffer(&w)
        ));
        // The border (column 20) dragged to column 14.
        let before = geometry(&w);
        a.keys(b"\x1b[<0;21;3M\x1b[<32;18;3M\x1b[<32;15;3M\x1b[<0;15;3m");
        got.push(changed(&before, || geometry(&w)));
        seen.push(got);
    }
    assert_eq!(seen[0], seen[1], "vtmux (left) vs tmux 3.8 (right)");
    // The left pane is 20 wide: row 1 is the wrapped "ta". ?1000h gets
    // the press and release but not the motion between.
    assert_eq!(
        seen[1],
        [
            r#"drag "beta-gamma delta""#,
            "0",
            r#"double "beta""#,
            "0",
            "wheel left 1",
            concat!(
                r"right \u{1b}[M $#\u{1b}[M#&$\u{1b}[M`$#\u{1b}[Ma$#",
                r"\u{1b}[M )%\u{1b}[M#)%\u{1b}[M )%\u{1b}[M#)%"
            ),
            r#"right mode 0, buffer "beta""#,
            "%1 0,0 14x9\n%0 15,0 25x9",
        ]
    );
}

/// D5: every child's `PATH` starts with a private shim dir holding `tmux`
/// -> this server's executable, so scripts that call plain `tmux` reach
/// this server even when it was started as `vtmux` and the `PATH` it was
/// given has another `tmux` first (here one that prints FAKE and fails).
/// The shim, invoked as `tmux`, finds the server through `$TMUX`, for a
/// `-L` and a `-S` socket alike. vtmux only: tmux has no shim
/// (compat/DIVERGENCES.md D-022).
#[test]
fn tmux_shim_reaches_this_server_from_children() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_vtmux"));
    let w = World::new(bin.clone());
    let fake = w.home.path().join("fake");
    std::fs::create_dir(&fake).unwrap();
    let fake_tmux = fake.join("tmux");
    std::fs::write(&fake_tmux, "#!/bin/sh\necho FAKE\nexit 1\n").unwrap();
    std::fs::set_permissions(
        &fake_tmux,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", fake.display());
    let sock = w.home.path().join("s");
    let sock = sock.to_str().unwrap();
    for socket in [["-L", w.label.as_str()], ["-S", sock]] {
        let cli = |args: &[&str]| {
            let out = Command::new(&bin)
                .args(socket)
                .args(["-f", "/dev/null"])
                .args(args)
                .env_clear()
                .envs(w.env())
                .env("PATH", &path)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
        };
        let mut cmd = CommandBuilder::new(&bin);
        cmd.args(socket);
        // `sh` as the command, not a login shell: macOS's /etc/profile
        // (path_helper) would reorder PATH and push the shim back.
        cmd.args(["-f", "/dev/null", "new", "-s", "a", "sh"]);
        let a = w.spawn_attached(cmd, 60, 10, &[("PATH", &path)]);
        a.wait(|a| pane_text(a).contains('$'));
        let shim_dir = cli(&["display", "-p", "#{socket_path}"]);
        let (dir, name) = shim_dir.rsplit_once('/').unwrap();
        let shim_dir = format!("{dir}/.{name}.bin");
        let ctx = format!("{socket:?}");

        // run-shell (a job).
        cli(&["run-shell", "tmux set -g @x 1"]);
        assert_eq!(cli(&["show", "-gv", "@x"]), "1", "{ctx}: run-shell");

        // A pane: the shim is first and says tmux 3.8.
        cli(&["send-keys", "-t", "a", "command -v tmux; tmux -V", "Enter"]);
        let want = format!("{shim_dir}/tmux");
        a.wait(|a| {
            let t = pane_text(a).replace('\n', "");
            t.contains(&want.replace('\n', "")) && t.contains("tmux 3.8")
        });
        assert!(!pane_text(&a).contains("FAKE"), "{ctx}: pane");

        // A respawned pane gets the shim once.
        cli(&["respawn-pane", "-k", "-t", "a"]);
        a.wait(|a| !pane_text(a).contains("tmux 3.8"));
        cli(&["run-shell", "-t", "a", "true"]);
        cli(&["send-keys", "-t", "a", "echo \"P=$PATH\"", "Enter"]);
        let once = format!("P={shim_dir}:{path}");
        a.wait(|a| pane_text(a).replace('\n', "").contains(&once));

        // A hook's run-shell, with a command substitution.
        cli(&[
            "set-hook",
            "-g",
            "after-new-window",
            "run-shell 'tmux set -g @hook \"$(tmux display -p ok)\"'",
        ]);
        cli(&["neww", "-d"]);
        let end = Instant::now() + Duration::from_secs(5);
        while cli(&["show", "-gv", "@hook"]) != "ok" {
            assert!(Instant::now() < end, "{ctx}: hook");
            thread::sleep(Duration::from_millis(50));
        }

        // A #() job in the status line.
        cli(&["set", "-g", "status-interval", "1"]);
        cli(&["set", "-g", "status-right", "<#(tmux display -p fmtjob)>"]);
        a.wait(|a| status_row(a).contains("<fmtjob>"));

        cli(&["kill-server"]);
        let end = Instant::now() + Duration::from_secs(5);
        while std::path::Path::new(&shim_dir).exists() {
            assert!(Instant::now() < end, "{ctx}: shim dir left behind");
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// What the attached terminal received after byte `mark`.
fn since(a: &Attached, mark: usize) -> String {
    String::from_utf8_lossy(&a.bytes.lock().unwrap()[mark..]).into_owned()
}

/// Synchronized output (`?2026`) holds a pane's drawing (tmux's
/// `MODE_SYNC`, screen-write.c:211-240, 1017-1062): while it is on, no byte
/// of the pane's new content reaches the terminal, and the clients keep its
/// last rows; `?2026l` (input.c:1981) draws it whole. Another pane keeps
/// drawing meanwhile. A program that never ends it is drawn anyway one
/// second after its last `?2026h` (the sync timer). Dogfood bug 4: an
/// Ink TUI clears and redraws inside `?2026`, and flashed blank.
/// Modelled on the owner's synccheck.py.
///
/// The timer is wall-clock, so a check that something is *still* held is only
/// valid inside its second: on a starved host a `sleep` or a CLI call alone can
/// outlast it. A check that fails after the timer could have run out is
/// inconclusive, not a failure (`SyncWindow`): the phase starts over from a
/// reset screen. A check that fails inside the window, or a phase that never
/// gets a window, still fails. Waits for output have no wall-clock bound a
/// loaded host could miss (`LOAD_LIMIT` only ends a hang).
#[test]
#[allow(clippy::too_many_lines)]
fn synchronized_output_holds_the_pane_until_it_ends() {
    /// The sync timer (input.c `INPUT_SYNC_TIMER`, one second after the last `?2026h`).
    const SYNC_TIMER: Duration = Duration::from_secs(1);
    const LOAD_LIMIT: Duration = Duration::from_secs(60);
    const ATTEMPTS: usize = 50;

    struct Retry;
    /// When the last `?2026h` was written: the timer cannot have run out
    /// before `SYNC_TIMER` has passed since.
    struct SyncWindow(Cell<Instant>);
    impl SyncWindow {
        /// Call before writing the `?2026h`.
        fn arm(&self) {
            self.0.set(Instant::now());
        }
        fn open(&self) -> bool {
            self.0.get().elapsed() < SYNC_TIMER
        }
        fn check(&self, ok: bool, what: impl FnOnce() -> String) -> Result<(), Retry> {
            if ok {
                Ok(())
            } else if self.open() {
                panic!("{}", what())
            } else {
                Err(Retry)
            }
        }
    }

    /// Nothing of the held drawing reached the client since `mark`: its text
    /// is not in the bytes and the left pane still shows `word0`..`word4`.
    /// Not "no bytes at all": a redraw of the borders from an earlier event
    /// can land late on a starved host (the reference does that), and is not
    /// the pane's content.
    fn held(a: &Attached, mark: usize, drawing: &str, word: &str) -> bool {
        !since(a, mark).contains(drawing)
            && (0..5).all(|y| {
                pane_text(a)
                    .lines()
                    .nth(y)
                    .is_some_and(|r| r.starts_with(&format!("{word}{y}")))
            })
    }

    for bin in bins() {
        let name = bin.display().to_string();
        let w = World::new(bin);
        let ok = |args: &[&str]| assert!(w.cli(args).status.success(), "{name}: {args:?}");
        ok(&[
            "new", "-d", "-s", "a", "-x", "60", "-y", "10", "sleep", "1000",
        ]);
        ok(&["set", "-g", "status", "off"]);
        ok(&["splitw", "-h", "-d", "-t", "a:0.0", "sleep", "1000"]);
        ok(&["selectp", "-t", "a:0.0", "-T", "left"]);
        ok(&["selectp", "-t", "a:0.1", "-T", "right"]);
        let tty = |p: &str| {
            let path = display_t(&w, p, "#{pane_tty}");
            std::fs::OpenOptions::new().write(true).open(&path).unwrap()
        };
        let (mut left, mut right) = (tty("a:0.0"), tty("a:0.1"));
        let sync_flag = || display_t(&w, "a:0.0", "#{synchronized_output_flag}");
        let a = w.attach_sized(&["attach", "-t", "a"], 60, 10);
        let row = |a: &Attached, y: usize| pane_text(a).lines().nth(y).unwrap_or("").to_owned();
        let rows_start =
            |a: &Attached, word: &str| (0..5).all(|y| row(a, y).starts_with(&format!("{word}{y}")));
        let win = SyncWindow(Cell::new(Instant::now()));

        // Ends any sync and draws `word0`..`word4` in the left pane (and
        // clears the right one): the starting point of every attempt.
        let reset = |left: &mut std::fs::File, right: &mut std::fs::File, word: &str| {
            left.write_all(b"\x1b[?2026l\x1b[2J\x1b[H").unwrap();
            for y in 0..5 {
                write!(left, "{word}{y}\r\n").unwrap();
            }
            right.write_all(b"\x1b[2J\x1b[H").unwrap();
            a.wait_labelled(&name, LOAD_LIMIT, |a| {
                rows_start(a, word) && !row(a, 0).contains("OTHER")
            });
            // Output that trails the last row must not count as sent during the sync.
            thread::sleep(Duration::from_millis(200));
        };

        let hold = |left: &mut std::fs::File, right: &mut std::fs::File| {
            // Mid-redraw: the screen is cleared and half drawn.
            let mark = a.bytes.lock().unwrap().len();
            win.arm();
            left.write_all(b"\x1b[?2026h\x1b[2J\x1b[HMIDSYNC").unwrap();
            thread::sleep(Duration::from_millis(300));
            win.check(held(&a, mark, "MIDSYNC", "OLD"), || {
                format!(
                    "{name}: bytes sent while the pane syncs: {:?}",
                    since(&a, mark)
                )
            })?;
            win.check(sync_flag() == "1", || format!("{name}: sync flag off"))?;

            // Another pane is not held back. `?2026h` again re-arms the timer.
            win.arm();
            left.write_all(b"\x1b[?2026h").unwrap();
            right.write_all(b"OTHER").unwrap();
            a.wait_labelled(&name, LOAD_LIMIT, |a| row(a, 0).contains("OTHER"));
            thread::sleep(Duration::from_millis(100));
            win.check(rows_start(&a, "OLD"), || {
                format!("{name}: the pane's last rows stay: {:?}", pane_text(&a))
            })?;
            win.check(sync_flag() == "1", || format!("{name}: sync flag off"))
        };
        let mut attempts = 0;
        loop {
            reset(&mut left, &mut right, "OLD");
            if hold(&mut left, &mut right).is_ok() {
                break;
            }
            attempts += 1;
            assert!(
                attempts < ATTEMPTS,
                "{name}: the sync timer always ran out first"
            );
        }

        // ?2026l: the finished drawing, at once. No timer to race: ending
        // the sync after it ran out draws the same rows.
        left.write_all(b"\x1b[2J\x1b[H").unwrap();
        for y in 0..5 {
            write!(left, "NEW{y}\r\n").unwrap();
        }
        left.write_all(b"\x1b[?2026l").unwrap();
        a.wait_labelled(&name, LOAD_LIMIT, |a| rows_start(a, "NEW"));
        assert_eq!(sync_flag(), "0", "{name}");

        // A program that never ends it: drawn once the timer is up, one
        // second after its `?2026h`. The start is taken before the write, so
        // a drawing earlier than the timer shows up as too early whatever the
        // load; one later than three seconds only says the host was slow, and
        // another attempt follows.
        let mut attempts = 0;
        loop {
            thread::sleep(Duration::from_millis(200));
            let mark = a.bytes.lock().unwrap().len();
            let start = Instant::now();
            win.arm();
            left.write_all(b"\x1b[?2026h\x1b[2J\x1b[HSTUCK").unwrap();
            thread::sleep(Duration::from_millis(500));
            let quiet = win.check(held(&a, mark, "STUCK", "NEW"), || {
                format!(
                    "{name}: bytes sent while the pane syncs: {:?}",
                    since(&a, mark)
                )
            });
            a.wait_labelled(&name, LOAD_LIMIT, |a| row(a, 0).starts_with("STUCK"));
            let took = start.elapsed();
            assert!(
                took >= Duration::from_millis(800),
                "{name}: drawn after {took:?}"
            );
            if quiet.is_ok() && took < Duration::from_secs(3) {
                break;
            }
            attempts += 1;
            assert!(
                attempts < ATTEMPTS,
                "{name}: drawn after {took:?}, in {attempts} attempts"
            );
            reset(&mut left, &mut right, "NEW");
        }
        assert_eq!(sync_flag(), "0", "{name}: the timer ends the mode");
    }
}

/// A process's run state: `T` when stopped (`/proc/PID/stat` on Linux,
/// `ps` elsewhere).
fn proc_state(pid: u32) -> char {
    let state = if cfg!(target_os = "linux") {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // pid (comm) S ...: comm may hold spaces, the state follows its ')'.
        stat.rsplit_once(") ")
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_default()
    } else {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    state.chars().next().unwrap_or('?')
}

/// `suspend-client` (prefix C-z): the terminal is restored and the client
/// stops itself with SIGTSTP; the server carries on without it (pane
/// output, no listed client); SIGCONT (the shell's `fg`) has it take the
/// terminal back, and the server redraws it in full. The client runs
/// under a job-control shell: in the orphaned process group of a session
/// leader the kernel discards SIGTSTP.
///
/// The shell is dash where installed (macOS ships `/bin/dash`): macOS's
/// `/bin/sh` is bash 3.2, which ignores `-m` with `-c` ("fg: no job
/// control"), so the client stays in the orphaned group and its SIGTSTP
/// is discarded, for tmux as for vtmux.
#[test]
fn suspend_client_stops_and_sigcont_redraws() {
    for bin in bins() {
        let w = World::new(bin.clone());
        let cli = |args: &[&str]| String::from_utf8_lossy(&w.cli(args).stdout).into_owned();
        w.cli(&[
            "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "cat",
        ]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["select-pane", "-t", "a", "-T", "p"]);
        let go = w.home.path().join("go");
        let script = format!(
            "'{}' -L {} -f /dev/null attach -t a; echo STOPPED; \
             while [ ! -e '{}' ]; do sleep 0.05; done; fg; echo DONE",
            bin.display(),
            w.label,
            go.display()
        );
        let shell = ["/bin/dash", "/bin/sh"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .unwrap();
        let mut cmd = CommandBuilder::new(shell);
        cmd.args(["-m", "-c", &script]);
        let mut a = w.spawn_attached(cmd, 40, 10, &[]);
        let ctx = bin.display().to_string();
        let limit = Duration::from_secs(5);
        a.wait_labelled(&ctx, limit, |a| status_row(a).starts_with("[a] 0:w*"));
        let pid: u32 = cli(&["lsc", "-F", "#{client_pid}"]).trim().parse().unwrap();

        a.keys(b"\x02\x1a");
        a.wait_labelled(&ctx, limit, |a| a.text().contains("STOPPED"));
        let end = Instant::now() + limit;
        while proc_state(pid) != 'T' {
            assert!(Instant::now() < end, "{ctx}: client {pid} not stopped");
            thread::sleep(Duration::from_millis(10));
        }
        // The terminal was handed back before the stop.
        let text = a.text();
        let stopped = text.rfind("STOPPED").unwrap();
        assert!(
            text[..stopped].contains("\x1b[?1049l"),
            "{ctx}: no teardown"
        );
        // The server keeps going: commands run and the pane takes output.
        // A suspended client is not listed.
        assert_eq!(cli(&["lsc"]), "", "{ctx}");
        w.cli(&["send-keys", "-t", "a", "zebra", "Enter"]);
        let end = Instant::now() + limit;
        while !cli(&["capture-pane", "-p", "-t", "a"]).contains("zebra") {
            assert!(Instant::now() < end, "{ctx}: pane output");
            thread::sleep(Duration::from_millis(10));
        }
        let before = a.text().len();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(proc_state(pid), 'T', "{ctx}");
        assert!(
            !a.text()[before..].contains("zebra"),
            "{ctx}: drawn while suspended"
        );

        std::fs::write(&go, "").unwrap();
        a.wait_labelled(&ctx, limit, |a| {
            pane_text(a).contains("zebra") && status_row(a).starts_with("[a] 0:w*")
        });
        assert_ne!(proc_state(pid), 'T', "{ctx}");
        assert_eq!(cli(&["lsc", "-F", "#{client_pid}"]).trim(), pid.to_string());
        // Attached as before: keys reach the pane, prefix d detaches.
        a.keys(b"yak\r");
        a.wait_labelled(&ctx, limit, |a| pane_text(a).contains("yak\nyak"));
        a.keys(b"\x02d");
        a.wait_labelled(&ctx, limit, |a| a.text().contains("DONE"));
        a.exited(0);
    }
}

/// Lines in `path`, 0 while it does not exist.
fn line_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |s| s.lines().count())
}

/// Wait until `path` has `n` lines.
fn wait_lines(ctx: &str, path: &std::path::Path, n: usize) {
    let end = Instant::now() + Duration::from_secs(5);
    while line_count(path) < n {
        assert!(
            Instant::now() < end,
            "{ctx}: {} has {} lines, want {n}",
            path.display(),
            line_count(path)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// `server_lock_client` (server-fn.c:197-218) and client.c `MSG_LOCK`: the
/// terminal is handed back in its own modes, `lock-command` runs in the
/// foreground on it, and when it exits the client is raw again and
/// redrawn in full; nothing is drawn meanwhile and the locked client is
/// not listed.
#[test]
fn lock_client_runs_the_lock_command_and_redraws() {
    for bin in bins() {
        let w = World::new(bin.clone());
        let cli = |args: &[&str]| String::from_utf8_lossy(&w.cli(args).stdout).into_owned();
        w.cli(&[
            "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "cat",
        ]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["select-pane", "-t", "a", "-T", "p"]);
        let home = w.home.path();
        let (modes, go) = (home.join("modes"), home.join("go"));
        let script = home.join("lock.sh");
        std::fs::write(
            &script,
            format!(
                "stty -a >'{}'\necho LOCKED\nwhile [ ! -e '{}' ]; do sleep 0.05; done\n",
                modes.display(),
                go.display()
            ),
        )
        .unwrap();
        w.cli(&[
            "set",
            "-g",
            "lock-command",
            &format!("/bin/sh '{}'", script.display()),
        ]);
        let mut a = w.attach(&["attach", "-t", "a"]);
        let ctx = bin.display().to_string();
        let limit = Duration::from_secs(5);
        a.wait_labelled(&ctx, limit, |a| status_row(a).starts_with("[a] 0:w*"));
        a.keys(b"zebra\r");
        a.wait_labelled(&ctx, limit, |a| pane_text(a).contains("zebra\nzebra"));
        let name = cli(&["lsc", "-F", "#{client_name}"]).trim().to_owned();

        let out = w.cli(&["lock-client", "-t", &name]);
        assert!(out.status.success(), "{ctx}: {out:?}");
        a.wait_labelled(&ctx, limit, |a| a.text().contains("LOCKED"));
        wait_lines(&ctx, &modes, 1);
        // The command had the terminal in its own (cooked) modes.
        let stty = std::fs::read_to_string(&modes).unwrap();
        assert!(
            stty.split_whitespace().any(|f| f == "icanon"),
            "{ctx}: not cooked: {stty}"
        );
        // Teardown, then the alternate screen cleared for the command.
        let text = a.text();
        let locked = text.rfind("LOCKED").unwrap();
        let leave = text[..locked].rfind("\x1b[?1049l").expect("teardown");
        assert!(
            text[leave..locked].contains("\x1b[?1049h"),
            "{ctx}: no lock screen"
        );
        // Not listed, and nothing drawn while locked.
        assert_eq!(cli(&["lsc"]), "", "{ctx}");
        w.cli(&["send-keys", "-t", "a", "yak", "Enter"]);
        let end = Instant::now() + limit;
        while !cli(&["capture-pane", "-p", "-t", "a"]).contains("yak\nyak") {
            assert!(Instant::now() < end, "{ctx}: pane output");
            thread::sleep(Duration::from_millis(10));
        }
        let before = a.text().len();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(a.text().len(), before, "{ctx}: drawn while locked");

        // The command exits: the client is back, the terminal started
        // again (tty_start_tty), redrawn in full and raw.
        std::fs::write(&go, "").unwrap();
        a.wait_labelled(&ctx, limit, |a| {
            a.text()[before..].contains("\x1b[?1049h")
                && pane_text(a).contains("yak\nyak")
                && status_row(a).starts_with("[a] 0:w*")
        });
        assert_eq!(cli(&["lsc", "-F", "#{client_name}"]).trim(), name, "{ctx}");
        a.keys(b"gnu\r");
        a.wait_labelled(&ctx, limit, |a| pane_text(a).contains("gnu\ngnu"));
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// `server_lock_session` and `server_lock` (server-fn.c:174-194): every
/// client on the session, or every client with a session, runs its
/// session's `lock-command`; each comes back redrawn.
#[test]
fn lock_session_and_lock_server_lock_every_matching_client() {
    for bin in bins() {
        let w = World::new(bin.clone());
        for s in ["a", "b"] {
            w.cli(&[
                "new", "-d", "-s", s, "-n", "w", "-x", "40", "-y", "9", "cat",
            ]);
            w.cli(&["select-pane", "-t", s, "-T", "p"]);
        }
        w.cli(&["set", "-g", "status-right", "R"]);
        let home = w.home.path();
        let (la, lb) = (home.join("a"), home.join("b"));
        w.cli(&[
            "set",
            "-t",
            "a",
            "lock-command",
            &format!("echo x >>'{}'", la.display()),
        ]);
        w.cli(&[
            "set",
            "-t",
            "b",
            "lock-command",
            &format!("echo x >>'{}'", lb.display()),
        ]);
        let ctx = bin.display().to_string();
        let limit = Duration::from_secs(5);
        let clients = [
            w.attach(&["attach", "-t", "a"]),
            w.attach(&["attach", "-t", "a"]),
            w.attach(&["attach", "-t", "b"]),
        ];
        for (c, s) in clients.iter().zip(["[a]", "[a]", "[b]"]) {
            c.wait_labelled(&ctx, limit, |c| status_row(c).starts_with(s));
        }
        // Locked and back: the lock screen and tty_start_tty each enter
        // the alternate screen, then the full redraw shows the status.
        let quiet = |c: &Attached| {
            let n = c.text().len();
            move |c: &Attached| {
                c.text()[n..].matches("\x1b[?1049h").count() >= 2 && status_row(c).starts_with('[')
            }
        };

        let marks: Vec<_> = clients.iter().map(quiet).collect();
        assert!(w.cli(&["lock-session", "-t", "a"]).status.success());
        wait_lines(&ctx, &la, 2);
        // Both of a's clients come back redrawn.
        for (c, m) in clients[..2].iter().zip(&marks) {
            c.wait_labelled(&ctx, limit, m);
        }
        thread::sleep(Duration::from_millis(200));
        assert_eq!(line_count(&la), 2, "{ctx}");
        assert_eq!(line_count(&lb), 0, "{ctx}");

        let marks: Vec<_> = clients.iter().map(quiet).collect();
        assert!(w.cli(&["lock-server"]).status.success());
        wait_lines(&ctx, &la, 4);
        wait_lines(&ctx, &lb, 1);
        for (c, m) in clients.iter().zip(&marks) {
            c.wait_labelled(&ctx, limit, m);
        }
        thread::sleep(Duration::from_millis(200));
        assert_eq!(line_count(&la), 4, "{ctx}");
        assert_eq!(line_count(&lb), 1, "{ctx}");
        let listed = String::from_utf8_lossy(&w.cli(&["lsc"]).stdout)
            .lines()
            .count();
        assert_eq!(listed, 3, "{ctx}");
    }
}

/// session.c `session_update_activity` / `session_lock_timer`: an
/// attached session with `lock-after-time` set locks its clients that
/// many seconds after its last activity (attach, a key, an unlock), and
/// the timer goes again after the unlock.
#[test]
fn lock_after_time_locks_an_idle_session() {
    for bin in bins() {
        let w = World::new(bin.clone());
        w.cli(&[
            "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "9", "cat",
        ]);
        w.cli(&["set", "-g", "status-right", "R"]);
        w.cli(&["select-pane", "-t", "a", "-T", "p"]);
        let locked = w.home.path().join("locked");
        w.cli(&[
            "set",
            "-g",
            "lock-command",
            &format!("echo x >>'{}'", locked.display()),
        ]);
        w.cli(&["set", "-g", "lock-after-time", "2"]);
        let ctx = bin.display().to_string();
        let mut a = w.attach(&["attach", "-t", "a"]);
        a.wait_labelled(&ctx, Duration::from_secs(5), |a| {
            status_row(a).starts_with("[a] 0:w*")
        });
        // Keys keep it awake.
        for _ in 0..6 {
            a.keys(b"k");
            thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(line_count(&locked), 0, "{ctx}: locked while active");
        let idle = Instant::now();
        wait_lines(&ctx, &locked, 1);
        assert!(
            idle.elapsed() >= Duration::from_millis(1000),
            "{ctx}: too soon"
        );
        // Unlocking is activity: it locks again two seconds later.
        wait_lines(&ctx, &locked, 2);
        w.cli(&["set", "-g", "lock-after-time", "0"]);
        a.keys(b"\r");
        thread::sleep(Duration::from_millis(2500));
        let n = line_count(&locked);
        thread::sleep(Duration::from_millis(2500));
        assert_eq!(
            line_count(&locked),
            n,
            "{ctx}: locked with lock-after-time 0"
        );
        a.wait_labelled(&ctx, Duration::from_secs(5), |a| {
            pane_text(a).contains("kkkkkk") && status_row(a).starts_with("[a] 0:w*")
        });
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// A client that leaves while its lock command runs (detach-client,
/// its session killed) goes as tmux's does once the command exits, with
/// the reason and status of its exit, not as if the server had gone.
/// tmux keeps the connection until the client answers (`MSG_EXITING`
/// after `MSG_UNLOCK`). Detached, the unlock starts the terminal again
/// and the exit tears it down, off the lock screen's alternate screen;
/// with the session gone (`c->session` NULL) neither happens, and tmux
/// leaves the terminal on it.
#[test]
fn a_client_leaving_while_locked_exits_with_its_reason() {
    for bin in bins() {
        for (leave, reason, teardown) in [
            (&["detach-client"][..], "[detached (from session a)]", true),
            (&["kill-session", "-t", "a"][..], "[exited]", false),
        ] {
            let w = World::new(bin.clone());
            for s in ["a", "b"] {
                w.cli(&[
                    "new", "-d", "-s", s, "-n", "w", "-x", "40", "-y", "9", "cat",
                ]);
                w.cli(&["select-pane", "-t", s, "-T", "p"]);
            }
            w.cli(&["set", "-g", "status-right", "R"]);
            let go = w.home.path().join("go");
            w.cli(&[
                "set",
                "-g",
                "lock-command",
                &format!(
                    "echo LOCKED; while [ ! -e '{}' ]; do sleep 0.05; done",
                    go.display()
                ),
            ]);
            let mut a = w.attach(&["attach", "-t", "a"]);
            let ctx = format!("{} {leave:?}", bin.display());
            let limit = Duration::from_secs(5);
            a.wait_labelled(&ctx, limit, |a| status_row(a).starts_with("[a] 0:w*"));
            assert!(w.cli(&["lock-client"]).status.success(), "{ctx}");
            a.wait_labelled(&ctx, limit, |a| a.text().contains("LOCKED"));
            assert!(w.cli(leave).status.success(), "{ctx}");
            thread::sleep(Duration::from_millis(200));
            std::fs::write(&go, "").unwrap();
            a.wait_labelled(&ctx, limit, |a| a.text().contains(reason));
            a.exited(0);
            let text = a.text();
            let after = &text[text.rfind("LOCKED").unwrap()..];
            match after.rfind("\x1b[?1049l") {
                Some(left) => assert!(
                    teardown && !after[left..].contains("\x1b[?1049h"),
                    "{ctx}: {after:?}"
                ),
                None => assert!(!teardown, "{ctx}: no teardown after the lock: {after:?}"),
            }
            assert!(!text.contains("server exited unexpectedly"), "{ctx}");
        }
    }
}

/// choose-client's info view (`i`, window-client.c
/// `window_client_draw_info`): each line is the client's own expansion, so
/// the feature list and the mouse, clipboard, focus-events and
/// extended-keys lines are what `#{I/f:...}` and `#{I/c:...}` say about its
/// terminal (format.c:6304-6327). The lines above Features hold the pid,
/// times and byte counts, which vary per run; from Features down the view
/// is compared, styles included, for terminals with different features.
#[test]
fn choose_client_info_view_shows_the_terminal() {
    for term in ["xterm-256color", "screen", "tmux-256color"] {
        let mut views = Vec::new();
        for bin in bins() {
            let mut w = World::new(bin);
            w.term = term;
            assert!(
                w.cli(&[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "100", "-y", "59", "cat"
                ])
                .status
                .success()
            );
            w.cli(&["select-pane", "-t", "%0", "-T", "t0"]);
            w.cli(&["set", "-g", "status-right", "R"]);
            w.cli(&["set", "-g", "extended-keys", "on"]);
            w.cli(&["set", "-g", "mouse", "on"]);
            w.cli(&["set", "-g", "focus-events", "on"]);
            let mut a = w.attach_sized(&["attach", "-t", "a"], 100, 60);
            a.wait(|_| !w.cli(&["lsc", "-F", "x"]).stdout.is_empty());
            // The terminal answers none of the startup queries: wait them out.
            thread::sleep(Duration::from_millis(1500));
            assert!(
                w.cli(&["choose-client", "-t", "a:0.0", "-F", "x"])
                    .status
                    .success()
            );
            a.wait(|a| pane_text(a).contains("(sort: name)"));
            a.keys(b"i");
            a.wait(|a| pane_text(a).contains("escape-time"));
            thread::sleep(Duration::from_millis(200));
            let rows = String::from_utf8(capture(
                &a.screen(),
                CaptureOpts {
                    escapes: true,
                    ..CaptureOpts::default()
                },
            ))
            .unwrap();
            let from = rows.find("Features").expect("a Features line");
            let to = rows.find("escape-time").expect("an escape-time line");
            views.push(rows[from..to].to_owned());
        }
        assert_eq!(
            views[0], views[1],
            "{term}: vtmux (left) vs tmux 3.8 (right)"
        );
    }
}

/// `-T utf8` marks a client UTF-8 when its locale does not (`LC_ALL=C`):
/// `tty_apply_features` sets `CLIENT_UTF8` for the utf8 feature
/// (tty-features.c:562-563), so `#{client_utf8}` is 1; without it the
/// client is not UTF-8.
#[test]
fn utf8_terminal_feature_sets_client_utf8() {
    let mut outputs = Vec::new();
    for bin in bins() {
        let w = World::new(bin);
        w.cli(&["new", "-d", "-s", "s", "-n", "w", "cat"]);
        w.cli(&["set", "-g", "status", "off"]);
        let mut seen = Vec::new();
        for features in [&["-T", "utf8"][..], &[][..]] {
            let mut args = features.to_vec();
            args.extend(["attach", "-t", "s"]);
            let mut a = w.attach_env(&args, 40, 10, &[("LC_ALL", "C")]);
            a.wait(|a| a.text().contains("\x1b[?1049h"));
            let out = w.cli(&["lsc", "-F", "u8=#{client_utf8} f=#{I/f:utf8}"]);
            seen.push(String::from_utf8_lossy(&out.stdout).into_owned());
            w.cli(&["detach-client", "-s", "s"]);
            a.exited(0);
        }
        w.cli(&["kill-server"]);
        outputs.push(seen);
    }
    assert_eq!(outputs[0], ["u8=1 f=1\n", "u8=0 f=0\n"]);
    assert_eq!(outputs[0], outputs[1], "left vtmux, right tmux 3.8");
}

/// `clear-on-attach` (tty.c `tty_start_tty`, `tty_stop_tty`): on, the
/// client's terminal goes to the alternate screen and is cleared, and back
/// at the end; off, its contents are scrolled into its history (`csr`,
/// `cup`, `indn` of the height plus one) and the alternate screen is never
/// used, the screen cleared on the way out instead (clear-on-attach.sh).
#[test]
fn clear_on_attach_off_scrolls_instead_of_the_alternate_screen() {
    for bin in bins() {
        for (setting, start, end) in [
            (
                "on",
                "\x1b[?1049h\x1b[22;0;0t\x1b[H\x1b[2J",
                "\x1b[?1049l\x1b[23;0;0t\x1b[?2031l",
            ),
            (
                "off",
                "\x1b[1;10r\x1b[1;10H\x1b[11S",
                "\x1b[?1004l\x1b[H\x1b[2J\x1b[?2031l",
            ),
        ] {
            let w = World::new(bin.clone());
            let mut v = vec!["new", "-d", "-s", "a"];
            v.extend(CAT);
            assert!(w.cli(&v).status.success());
            w.cli(&["set", "-g", "clear-on-attach", setting]);
            let mut a = w.attach(&["attach", "-t", "a"]);
            a.wait(|a| a.text().contains("\x1b[?2026$p"));
            let what = format!("{} {setting}", bin.display());
            assert!(a.text().starts_with(start), "{what}: {:?}", a.text());
            assert!(w.cli(&["detach-client", "-s", "a"]).status.success());
            a.exited(0);
            let text = a.text();
            assert!(
                text.contains(&format!("{end}[detached")),
                "{what}: {text:?}"
            );
            assert_eq!(
                text.contains("\x1b[?1049"),
                setting == "on",
                "{what}: alternate screen"
            );
        }
    }
}

/// The inactive blend the D26 tests set: text half way to the terminal's
/// background, backgrounds half way to white.
const BLEND: &str = "fg=bg/50,bg=#ffffff/50";
/// [`BLEND`]'s background over a black terminal.
const GREY: vtmux_vt::Color = vtmux_vt::Color::Rgb(128, 128, 128);

/// What a terminal answers to OSC 10 and 11: white text on black.
const COLOUR_REPLY: &[u8] = b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\";

/// D26: a 40x6 client (status off) on two side-by-side `cat` panes, each
/// showing `RED` in `38;2;200;0;0`, the right one active; `cmds` run before
/// it attaches, `env` is extra client environment. Waits for both panes.
fn blend_world(cmds: &[&[&str]], env: &[(&str, &str)]) -> (World, Attached) {
    let w = World::new(PathBuf::from(env!("CARGO_BIN_EXE_vtmux")));
    let pane = r"printf '\033[38;2;200;0;0mRED\033[0m'; exec cat";
    let ok = |args: &[&str]| assert!(w.cli(args).status.success(), "{args:?}");
    ok(&[
        "new", "-d", "-s", "a", "-n", "w", "-x", "40", "-y", "6", "sh", "-c", pane,
    ]);
    ok(&["set", "-g", "status", "off"]);
    ok(&["splitw", "-h", "-t", "a", "sh", "-c", pane]);
    for cmd in cmds {
        ok(cmd);
    }
    // Both panes have drawn before the client attaches, so its frames do
    // not depend on when their output arrived.
    let end = Instant::now() + Duration::from_secs(5);
    for t in ["a:0.0", "a:0.1"] {
        while !String::from_utf8_lossy(&w.cli(&["capture-pane", "-p", "-t", t]).stdout)
            .starts_with("RED")
        {
            assert!(Instant::now() < end, "pane {t}");
            thread::sleep(Duration::from_millis(10));
        }
    }
    let a = w.attach_env(&["attach", "-t", "a"], 40, 6, env);
    a.wait(|a| blend_cell(a, 0).text == "R" && blend_cell(a, RIGHT).text == "R");
    (w, a)
}

/// The right pane's first column (the left pane is 20 wide, then the border).
const RIGHT: u16 = 21;

fn blend_cell(a: &Attached, x: u16) -> vtmux_vt::Cell {
    vtmux_vt::snapshot(&a.screen())
        .cell(x, 0)
        .cloned()
        .unwrap_or_default()
}

/// The foreground of `R` in the left and the right pane.
fn blend_fgs(a: &Attached) -> (vtmux_vt::Color, vtmux_vt::Color) {
    (blend_cell(a, 0).style.fg, blend_cell(a, RIGHT).style.fg)
}

fn settle_on<T: PartialEq + std::fmt::Debug>(a: &Attached, want: &T, got: impl Fn(&Attached) -> T) {
    let end = Instant::now() + Duration::from_secs(5);
    while got(a) != *want && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(got(a), *want);
}

/// D26 on a truecolour client that told us its colours (white on black):
/// the inactive pane's explicitly coloured text is blended toward black
/// and its default background toward white, the active pane's are not; `select-pane` swaps
/// them, the first frame after it showing the whole swap (no stale rows).
#[test]
fn pane_blend_dims_the_inactive_pane() {
    use vtmux_vt::Color;
    let (w, mut a) = blend_world(
        &[&["set", "-g", "vtmux-pane-inactive-blend", BLEND]],
        &[("COLORTERM", "truecolor")],
    );
    // No reply yet: no blend.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        blend_fgs(&a),
        (Color::Rgb(200, 0, 0), Color::Rgb(200, 0, 0))
    );
    a.keys(COLOUR_REPLY);
    settle_on(
        &a,
        &(Color::Rgb(100, 0, 0), Color::Rgb(200, 0, 0)),
        blend_fgs,
    );
    // Every row of the inactive pane, blank ones included, is grey;
    // the active pane and the border keep the terminal's default.
    let screen = vtmux_vt::snapshot(&a.screen());
    for y in 0..6 {
        assert_eq!(screen.cell(5, y).unwrap().style.bg, GREY, "row {y}");
        assert_eq!(
            screen.cell(RIGHT + 5, y).unwrap().style.bg,
            Color::Default,
            "row {y}"
        );
        assert_eq!(
            screen.cell(20, y).unwrap().style.bg,
            Color::Default,
            "border row {y}"
        );
    }
    let before = a.bytes.lock().unwrap().len();
    assert!(w.cli(&["select-pane", "-t", "a:0.0"]).status.success());
    settle_on(
        &a,
        &(Color::Rgb(200, 0, 0), Color::Rgb(100, 0, 0)),
        blend_fgs,
    );
    // The first frame after select-pane: up to the next frame's start.
    let bytes = a.bytes.lock().unwrap().clone();
    let start = before + find(&bytes[before..], b"\x1b[?25l").expect("a frame");
    let end = find(&bytes[start + 1..], b"\x1b[?25l").map_or(bytes.len(), |n| start + 1 + n);
    let mut t = PaneTerm::new(40, 6, 0);
    t.feed(&bytes[..end]);
    let first = vtmux_vt::snapshot(&t);
    for y in 0..6 {
        assert_eq!(
            first.cell(5, y).unwrap().style.bg,
            Color::Default,
            "row {y}"
        );
        assert_eq!(first.cell(RIGHT + 5, y).unwrap().style.bg, GREY, "row {y}");
    }
    assert_eq!(
        first.cell(RIGHT, 0).unwrap().style.fg,
        Color::Rgb(100, 0, 0)
    );
    // Zoomed, the one pane shown is the active one: not blended.
    assert!(
        w.cli(&["resize-pane", "-Z", "-t", "a:0.1"])
            .status
            .success()
    );
    settle_on(&a, &Color::Rgb(200, 0, 0), |a| blend_cell(a, 0).style.fg);
    // An active-pane blend reaches the active pane.
    assert!(
        w.cli(&["resize-pane", "-Z", "-t", "a:0.1"])
            .status
            .success()
    );
    assert!(
        w.cli(&["set", "-g", "vtmux-pane-active-blend", "fg=#000000/100"])
            .status
            .success()
    );
    settle_on(&a, &(Color::Rgb(100, 0, 0), Color::Rgb(0, 0, 0)), blend_fgs);
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// D26 off state and no reply: with the options unset, or set on a client
/// whose terminal never answered OSC 10/11, the client gets byte for byte
/// what it gets with neither (the startup queries included); a reply alone
/// changes nothing either.
#[test]
fn pane_blend_off_and_without_a_reply_is_byte_identical() {
    let set: &[&str] = &["set", "-g", "vtmux-pane-inactive-blend", BLEND];
    let mut outputs = Vec::new();
    for (cmds, reply) in [(&[][..], false), (&[set][..], false), (&[][..], true)] {
        let (_w, mut a) = blend_world(cmds, &[("COLORTERM", "truecolor")]);
        if reply {
            a.keys(COLOUR_REPLY);
        }
        thread::sleep(Duration::from_millis(500));
        a.keys(b"\x02d");
        a.exited(0);
        outputs.push(a.text());
    }
    assert!(outputs[0].contains("\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]4;0;?\x1b\\"));
    assert_eq!(outputs[1], outputs[0], "set, no reply");
    assert_eq!(outputs[2], outputs[0], "a reply, unset");
}

/// D26 colour depth: a 256-colour client gets the blended colour's nearest
/// palette entry ((200,0,0) is 160, (100,0,0) is 52, grey 128 is 244);
/// a 16-colour or smaller one gets SGR 2 on the inactive pane and no colour change.
#[test]
fn pane_blend_256_and_16_colour_fallbacks() {
    use vtmux_vt::Color;
    let set: &[&str] = &["set", "-g", "vtmux-pane-inactive-blend", BLEND];
    let (_w, mut a) = blend_world(&[set], &[]);
    a.keys(COLOUR_REPLY);
    settle_on(&a, &(Color::Palette(52), Color::Palette(160)), blend_fgs);
    assert_eq!(blend_cell(&a, 5).style.bg, Color::Palette(244), "grey");
    for term in ["xterm", "xterm-16color"] {
        let (_w, mut a) = blend_world(&[set], &[("TERM", term)]);
        let faint = |a: &Attached| {
            (
                blend_cell(a, 0).style.faint,
                blend_cell(a, RIGHT).style.faint,
            )
        };
        assert_eq!(faint(&a), (false, false));
        a.keys(COLOUR_REPLY);
        settle_on(&a, &(true, false), faint);
        assert_eq!(
            blend_cell(&a, 0).style.fg,
            blend_cell(&a, RIGHT).style.fg,
            "{term}"
        );
        assert_eq!(blend_cell(&a, 5).style.bg, Color::Default, "{term}");
    }
}

/// `tty_repeat_requests(force)` after a theme report (`CSI ? 997 ; 1 n`):
/// the colours are asked for again, so the blend follows a theme change.
#[test]
fn pane_blend_follows_a_theme_change() {
    use vtmux_vt::Color;
    let set: &[&str] = &["set", "-g", "vtmux-pane-inactive-blend", "fg=bg/100"];
    let (_w, mut a) = blend_world(&[set], &[("COLORTERM", "truecolor")]);
    a.keys(COLOUR_REPLY);
    settle_on(&a, &Color::Rgb(0, 0, 0), |a| blend_cell(a, 0).style.fg);
    let before = a.bytes.lock().unwrap().len();
    a.keys(b"\x1b[?997;2n");
    a.wait(|a| find(&a.bytes.lock().unwrap()[before..], b"\x1b]11;?\x1b\\").is_some());
    a.keys(b"\x1b]10;#000000\x1b\\\x1b]11;#ffffff\x1b\\");
    settle_on(&a, &Color::Rgb(255, 255, 255), |a| {
        blend_cell(a, 0).style.fg
    });
}

/// `server_client_update_theme_colours` (server-client.c:1274): under
/// `theme detect` the `theme*` colours come from the client's theme, its
/// last `CSI ? 997 ; 1|2 n` report (`server_client_report_theme`, 3305,
/// which redraws), else its OSC 11 background's brightness
/// (`colour_totheme(c->tty.bg)`, tty-keys.c:834; no redraw of its own, so
/// `refresh-client -S` shows it). Dark while neither is known.
#[test]
fn client_theme_reaches_the_theme_colours() {
    use vtmux_vt::Color;
    let status = |a: &Attached| {
        vtmux_vt::snapshot(&a.screen())
            .cell(0, 9)
            .cloned()
            .unwrap_or_default()
            .style
            .bg
    };
    let (dark, light) = (Color::Palette(100), Color::Palette(200));
    for bin in bins() {
        for report in [true, false] {
            let w = World::new(bin.clone());
            let mut v = vec!["new", "-d", "-s", "a", "-n", "w"];
            v.extend(CAT);
            assert!(w.cli(&v).status.success());
            for set in [
                ["set", "-g", "status-right", ""],
                ["set", "-g", "status-style", "bg=themegreen"],
                ["set", "-g", "dark-theme-green", "colour100"],
                ["set", "-g", "light-theme-green", "colour200"],
            ] {
                assert!(w.cli(&set).status.success());
            }
            let mut a = w.attach(&["attach", "-t", "a"]);
            settle_on(&a, &dark, status);
            if report {
                a.keys(b"\x1b[?997;2n");
                settle_on(&a, &light, status);
                a.keys(b"\x1b[?997;1n");
                settle_on(&a, &dark, status);
            } else {
                a.keys(b"\x1b]10;#000000\x1b\\\x1b]11;#ffffff\x1b\\");
                // The reply is in before the redraw.
                thread::sleep(Duration::from_millis(200));
                assert!(w.cli(&["refresh-client", "-S"]).status.success());
                settle_on(&a, &light, status);
            }
            a.keys(b"\x02d");
            a.exited(0);
        }
    }
}

/// tty.c `tty_send_requests` (tty.c:420): after the DECRQM 2026 query
/// both ask for the default colours (OSC 10, 11); a theme report
/// (`CSI ? 997 ; 1 n`) asks again (`server_client_report_theme` ->
/// `tty_repeat_requests(force)`, server-client.c:3315). vtmux follows
/// each with OSC 4 for palette entries 0-15 (the pane blend, D26).
#[test]
fn colour_queries_at_attach_and_on_a_theme_report() {
    const COLOURS: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
    for bin in bins() {
        let w = World::new(bin.clone());
        let mut v = vec!["new", "-d", "-s", "a"];
        v.extend(CAT);
        assert!(w.cli(&v).status.success());
        let mut a = w.attach(&["attach", "-t", "a"]);
        let mut wanted = b"\x1b[?2026$p".to_vec();
        wanted.extend_from_slice(COLOURS);
        a.wait(|a| find(&a.bytes.lock().unwrap(), &wanted).is_some());
        let what = bin.display().to_string();
        let vtmux = bin.as_path() == std::path::Path::new(env!("CARGO_BIN_EXE_vtmux"));
        let palette = |a: &Attached| find(&a.bytes.lock().unwrap(), b"\x1b]4;15;?\x1b\\").is_some();
        assert_eq!(palette(&a), vtmux, "{what}: OSC 4");
        let before = a.bytes.lock().unwrap().len();
        a.keys(b"\x1b[?997;1n");
        a.wait(|a| find(&a.bytes.lock().unwrap()[before..], COLOURS).is_some());
        a.keys(b"\x02d");
        a.exited(0);
    }
}

/// tty.c `tty_resize` (130-143): a terminal that reports 0 columns or 0
/// rows (a PTY nobody sized) is taken as 80 columns or 24 rows, so the
/// window it attaches to is 80x23, not shrunk to a column; the same
/// after a resize (`MSG_RESIZE`, server-client.c:2712-2730).
#[test]
fn zero_size_terminal_is_80x24() {
    for bin in bins() {
        let w = World::new(bin.clone());
        assert!(w.cli(&["new", "-d", "-s", "a", "cat -"]).status.success());
        let mut a = w.attach_sized(&["attach", "-t", "a"], 0, 0);
        let client = || {
            String::from_utf8_lossy(
                &w.cli(&["lsc", "-F", "#{client_width}x#{client_height}"])
                    .stdout,
            )
            .trim()
            .to_owned()
        };
        a.wait(|_| !client().is_empty());
        assert_eq!(client(), "80x24", "{}", bin.display());
        let window = || {
            display(
                &w,
                "#{window_width}x#{window_height} #{pane_width}x#{pane_height}",
            )
        };
        a.wait_labelled(&bin.display().to_string(), Duration::from_secs(5), |_| {
            window() == "80x23 80x23"
        });
        // A later resize to 0x0 (MSG_RESIZE) is taken the same way.
        a.resize(40, 10);
        a.wait_labelled(&bin.display().to_string(), Duration::from_secs(5), |_| {
            client() == "40x10"
        });
        a.resize(0, 0);
        a.wait_labelled(&bin.display().to_string(), Duration::from_secs(5), |_| {
            client() == "80x24"
        });
    }
}

/// The server's open descriptors: `/proc/<pid>/fd` where there is one,
/// `lsof` elsewhere (macOS), its numbered descriptors only.
fn open_fds(pid: &str) -> Vec<String> {
    if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        let mut fds: Vec<String> = entries
            .filter_map(|e| {
                let e = e.ok()?;
                let to = std::fs::read_link(e.path()).ok()?;
                Some(format!(
                    "{} {}",
                    e.file_name().to_string_lossy(),
                    to.display()
                ))
            })
            .collect();
        fds.sort();
        return fds;
    }
    let lsof = if std::path::Path::new("/usr/sbin/lsof").exists() {
        "/usr/sbin/lsof"
    } else {
        "lsof"
    };
    let out = Command::new(lsof)
        .args(["-n", "-P", "-p", pid, "-F", "fn"])
        .output()
        .expect("lsof");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fds = Vec::new();
    let mut fd = None;
    for line in text.lines() {
        if let Some(f) = line.strip_prefix('f') {
            fd = f.parse::<u32>().ok();
        } else if let (Some(n), Some(f)) = (line.strip_prefix('n'), fd.take()) {
            fds.push(format!("{f} {n}"));
        }
    }
    fds
}

/// `args` succeed; their output, trimmed.
fn cli_ok(w: &World, args: &[&str]) -> String {
    let out = w.cli(args);
    assert!(
        out.status.success(),
        "{}: {args:?}: {}",
        w.bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Every way a pane, popup or job comes and goes, once, in session `a`
/// (window `w`, pane `a:w.0` running `cat -`, client `a` attached).
fn lifecycle_round(w: &World, a: &Attached) {
    let ok = |args: &[&str]| cli_ok(w, args);
    let until = |what: &str, f: &dyn Fn() -> bool| {
        let what = format!("{} {what}", w.bin.display());
        a.wait_labelled(&what, Duration::from_secs(10), |_| f());
    };
    let panes = || {
        ok(&["lsp", "-s", "-t", "a", "-F", "#{pane_id}"])
            .lines()
            .count()
    };
    let split = |cmd: &[&str]| {
        let mut args = vec!["splitw", "-d", "-P", "-F", "#{pane_id}", "-t", "a:w"];
        args.extend(cmd);
        ok(&args)
    };
    ok(&["respawn-pane", "-k", "-t", "a:w.0", "cat", "-"]);
    ok(&["killp", "-t", &split(&["cat", "-"])]);
    split(&["sh", "-c", "exit 0"]);
    until("exited pane gone", &|| panes() == 1);
    ok(&["set", "-g", "remain-on-exit", "on"]);
    let p = split(&["sh", "-c", "exit 0"]);
    let dead = || ok(&["display", "-p", "-t", &p, "#{pane_dead}"]) == "1";
    until("pane dead", &dead);
    ok(&["respawn-pane", "-t", &p, "sh", "-c", "exit 0"]);
    until("respawned pane dead", &dead);
    ok(&["killp", "-t", &p]);
    ok(&["set", "-g", "remain-on-exit", "off"]);
    ok(&["neww", "-d", "-n", "x", "-t", "a:", "cat", "-"]);
    ok(&["killw", "-t", "a:x"]);
    ok(&["new", "-d", "-s", "b", "cat", "-"]);
    ok(&["kill-session", "-t", "b"]);
    let p = ok(&[
        "new-pane",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "a:w",
        "cat",
        "-",
    ]);
    ok(&["killp", "-t", &p]);
    ok(&["display-popup", "-t", "a", "-E", "true"]);
    // A CLI display-popup waits until the popup closes.
    let mut popup = Command::new(&w.bin)
        .args(w.args())
        .args(["display-popup", "-t", "a", "-T", "pp", "exec cat"])
        .env_clear()
        .envs(w.env())
        .spawn()
        .unwrap();
    a.wait(|a| pane_text(a).contains("pp"));
    ok(&["display-popup", "-C", "-t", "a"]);
    popup.wait().unwrap();
    ok(&["run-shell", "true"]);
    ok(&["run-shell", "-b", "true"]);
    ok(&["if-shell", "true", "set -g @x 1"]);
    ok(&["display", "-p", "#(echo hi)"]);
    ok(&["copy-mode", "-t", "a:w.0"]);
    ok(&["send", "-t", "a:w.0", "-X", "cancel"]);
    ok(&["pipe-pane", "-t", "a:w.0", "cat >/dev/null"]);
    ok(&["pipe-pane", "-t", "a:w.0"]);
}

/// A pane's master closes when the pane goes, by whatever way it goes, and
/// a popup's and a job's descriptors when they end (window.c
/// `window_pane_destroy`, popup.c `popup_free_cb`, job.c `job_free`), so
/// the server holds as many descriptors (so PTYs) after any number of
/// lifecycles as after the first: a leak of one per respawn exhausts
/// macOS's 511 PTYs (`kern.tty.ptmx_max`) over a test run.
#[test]
fn pane_lifecycles_leave_no_descriptor_behind() {
    for bin in bins() {
        let vtmux = bin.as_os_str() == env!("CARGO_BIN_EXE_vtmux");
        let w = World::new(bin.clone());
        let a = w.attach(&["new", "-s", "a", "-n", "w", "cat", "-"]);
        a.wait(|_| w.cli(&["lsc"]).stdout.len() > 1);
        cli_ok(&w, &["set", "-g", "status-right", "x"]);
        let pid = cli_ok(&w, &["display", "-p", "#{pid}"]);
        // The first round may open what lasts (a job's first use).
        lifecycle_round(&w, &a);
        let settled = |want: Option<usize>| {
            // Jobs, popups and exited panes finish on their own time.
            let end = Instant::now() + Duration::from_secs(10);
            let mut last = open_fds(&pid);
            loop {
                thread::sleep(Duration::from_millis(200));
                let now = open_fds(&pid);
                let done = want.map_or(now == last, |n| now.len() == n);
                if done || Instant::now() > end {
                    return now;
                }
                last = now;
            }
        };
        let before = settled(None);
        for _ in 0..5 {
            lifecycle_round(&w, &a);
        }
        let same = |what: &str| {
            let after = settled(Some(before.len()));
            assert_eq!(
                after.len(),
                before.len(),
                "{} {what}: descriptors before {before:#?} after {after:#?}",
                bin.display()
            );
        };
        same("after 5 rounds");
        // A live upgrade keeps the descriptors it chose to, no more.
        if vtmux {
            cli_ok(&w, &["vtmux-upgrade"]);
            a.wait(|_| w.cli(&["lsc"]).stdout.len() > 1);
            same("after an upgrade");
        }
    }
}
