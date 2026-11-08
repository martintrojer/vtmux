//! Control mode (`-C`) against `target/debug/vtmux` and the reference tmux
//! 3.8, compared after normalising guard timestamps, command numbers
//! (renumbered by first appearance: tmux counts internal queue items,
//! cmd-queue.c:706-746), the server pid and the socket dir.
//!
//! Isolation: environment cleared, a private `TMUX_TMPDIR` and `HOME`, an
//! explicit `-L`, `-f /dev/null`, pane command pinned to `cat`.

mod common;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

fn reference() -> PathBuf {
    let bin = std::env::var_os("VTMUX_REFERENCE_TMUX")
        .filter(|p| !p.is_empty())
        .map_or_else(
            || {
                let cache = std::env::var_os("XDG_CACHE_HOME").map_or_else(
                    || PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache"),
                    PathBuf::from,
                );
                cache.join("vtmux/tmux-3.8/bin/tmux")
            },
            PathBuf::from,
        );
    assert!(
        bin.exists(),
        "reference tmux missing: {} (run `cargo xtask ref-tmux`)",
        bin.display()
    );
    bin
}

fn bins() -> [PathBuf; 2] {
    [PathBuf::from(env!("CARGO_BIN_EXE_vtmux")), reference()]
}

struct World {
    bin: PathBuf,
    dir: tempfile::TempDir,
    label: String,
    /// Extra environment for every command (and so the server).
    env: Vec<(String, String)>,
}

impl World {
    fn new(bin: &Path) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Self {
            bin: bin.to_owned(),
            dir: tempfile::tempdir().expect("tempdir"),
            label: format!(
                "ctl{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            env: Vec::new(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["-L", &self.label, "-f", "/dev/null"])
            .args(args)
            .current_dir(self.dir.path())
            .env_clear()
            .env("HOME", self.dir.path())
            .env("TMUX_TMPDIR", self.dir.path())
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(self.env.iter().map(|(k, v)| (k, v)));
        cmd
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let out = self.command(args).output().expect("run");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn control(&self, args: &[&str]) -> Control {
        let mut c = self.control_stalled(args);
        c.start_reading();
        c
    }

    /// A control client whose output nobody reads until
    /// [`Control::start_reading`]: it fills its pipe and stays behind.
    fn control_stalled(&self, args: &[&str]) -> Control {
        let mut child = self
            .command(&[&["-C"], args].concat())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn control client");
        let stdout = child.stdout.take();
        let stdin = child.stdin.take();
        Control {
            child,
            stdin,
            stdout,
            out: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The server's pid.
    fn server_pid(&self) -> rustix::process::Pid {
        common::parse_pid(&self.run(&["display", "-p", "#{pid}"]).0).expect("server pid")
    }

    /// Wait until `n` clients are attached.
    fn wait_clients(&self, n: usize) {
        let end = Instant::now() + WAIT;
        while self.run(&["lsc", "-F", "x"]).0.lines().count() < n {
            assert!(Instant::now() < end, "never {n} clients");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Socket dir -> `SOCKDIR`.
    fn normalise(&self, s: &str) -> String {
        let dir = self.dir.path().to_string_lossy().into_owned();
        let canon = std::fs::canonicalize(self.dir.path())
            .map_or_else(|_| dir.clone(), |p| p.to_string_lossy().into_owned());
        s.replace(&canon, "SOCKDIR").replace(&dir, "SOCKDIR")
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let pid = self
            .command(&["display", "-p", "#{pid}"])
            .output()
            .ok()
            .and_then(|o| common::parse_pid(&String::from_utf8_lossy(&o.stdout)));
        let _ = self.command(&["kill-server"]).output();
        common::reap(pid);
    }
}

struct Control {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Not read yet ([`World::control_stalled`]).
    stdout: Option<ChildStdout>,
    out: Arc<Mutex<Vec<u8>>>,
}

impl Control {
    /// Read the client's output into `out` from now on.
    fn start_reading(&mut self) {
        let Some(mut stdout) = self.stdout.take() else {
            return;
        };
        let sink = Arc::clone(&self.out);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
    }

    /// `line` and its newline in one write.
    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    /// Poll the stream (max 5 s) until `pred` holds.
    fn wait(&self, what: &str, pred: impl Fn(&str) -> bool) {
        let end = Instant::now() + WAIT;
        while !pred(&self.text()) {
            assert!(
                Instant::now() < end,
                "timeout waiting for {what}: {:?}",
                self.text()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// EOF on stdin, then the whole stream, stderr and exit status.
    fn close(mut self) -> (String, String, i32) {
        drop(self.stdin.take());
        let end = Instant::now() + WAIT;
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() > end {
                let _ = self.child.kill();
                panic!("control client did not exit: {:?}", self.text());
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut err = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        // The reader thread sees EOF once the child is gone.
        let end = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&self.out) > 1 && Instant::now() < end {
            thread::sleep(Duration::from_millis(5));
        }
        (self.text(), err, status.code().unwrap_or(-1))
    }
}

/// Guard lines `%begin T N F` -> `%begin <T> #k F` with `k` the order in
/// which `N` first appeared.
fn normalise_guards(stream: &str) -> Vec<String> {
    let mut numbers: HashMap<String, usize> = HashMap::new();
    stream
        .lines()
        .map(|line| {
            let parts: Vec<&str> = line.split(' ').collect();
            match parts.as_slice() {
                [g @ ("%begin" | "%end" | "%error"), _time, number, flags] => {
                    let next = numbers.len() + 1;
                    let k = *numbers.entry((*number).to_owned()).or_insert(next);
                    format!("{g} <T> #{k} {flags}")
                }
                _ => line.to_owned(),
            }
        })
        .collect()
}

/// `\ooo` escapes back to bytes (control.c `control_append_data`).
fn decode_output(payload: &str) -> Vec<u8> {
    let b = payload.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            let s = std::str::from_utf8(&b[i + 1..i + 4]).unwrap();
            out.push(u8::from_str_radix(s, 8).unwrap());
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Every `%output %0` payload, decoded and concatenated.
fn pane_output(stream: &str) -> Vec<u8> {
    stream
        .lines()
        .filter_map(|l| l.strip_prefix("%output %0 "))
        .flat_map(decode_output)
        .collect()
}

/// Run a case on both binaries; the reference's result is the expectation.
fn compare<T: PartialEq + std::fmt::Debug>(name: &str, case: impl Fn(&World) -> T) {
    let [ours, theirs] = bins().map(|b| case(&World::new(&b)));
    assert_eq!(ours, theirs, "{name}: left vtmux, right tmux 3.8");
}

/// (1) An unattached control client exits at once without reading stdin
/// (server-client.c:2827-2831); the session stays.
#[test]
fn unattached_exits_at_once() {
    compare("unattached_exits_at_once", |w| {
        let mut c = w.control(&["new", "-d", "-s", "c", "cat"]);
        // The client may already be gone; the line is never run either way.
        if let Some(stdin) = c.stdin.as_mut() {
            let _ = stdin.write_all(b"display -p hello\n");
        }
        let (out, err, code) = c.close();
        let has = w.run(&["has", "-t", "c"]);
        (normalise_guards(&out), err, code, has)
    });
}

/// A control client's stdin is read only once its own command has run
/// (control.c `control_start` leaves the read event off; `control_ready`
/// from server-client.c `server_client_command_done` turns it on). Its
/// command waits for the startup config (cfg.c `start_cfg`), here held by a
/// job, so an EOF that comes first ends the client only after the command:
/// the session is made and the config's error reaches it as
/// `%config-error` (regress if-shell-error.sh).
#[test]
fn stdin_eof_waits_for_the_startup_config() {
    compare("stdin_eof_waits_for_the_startup_config", |w| {
        let conf = w.dir.path().join("job.conf");
        std::fs::write(&conf, "if 'sleep 1' 'nosuch'\n").expect("write");
        let conf = conf.to_str().expect("utf-8");
        // Named, so no automatic rename races the exit.
        let c = w.control(&["-f", conf, "new", "-s", "c", "-n", "w", "cat"]);
        let (out, err, code) = c.close();
        let has = w.run(&["has", "-t", "c"]);
        (normalise_guards(&w.normalise(&out)), err, code, has)
    });
}

/// (2) An attached control client runs stdin lines in flags-1 guards.
#[test]
fn attached_display_in_guard() {
    compare("attached_display_in_guard", |w| {
        let mut c = w.control(&["new", "-s", "c", "-n", "w", "cat"]);
        c.wait("%session-changed", |t| t.contains("%session-changed"));
        c.send("display -p hello");
        c.wait("%end", |t| {
            t.lines().filter(|l| l.starts_with("%end")).count() == 2
        });
        let (out, err, code) = c.close();
        (normalise_guards(&out), err, code)
    });
}

/// `display` without -p on a control client prints `%message <msg>`
/// inside the guard (cmd-display-message.c:163-169).
#[test]
fn display_message_without_p_is_percent_message() {
    compare("display_message_without_p_is_percent_message", |w| {
        let mut c = w.control(&["new", "-s", "c", "-n", "w", "cat"]);
        c.wait("%session-changed", |t| t.contains("%session-changed"));
        c.send("display hello");
        c.wait("%end", |t| {
            t.lines().filter(|l| l.starts_with("%end")).count() == 2
        });
        let (out, err, code) = c.close();
        (normalise_guards(&out), err, code)
    });
}

/// (3) An unknown command on stdin is a guarded `parse error: ` block
/// (control.c:672-680) and does not change the exit status.
#[test]
fn parse_error_block() {
    compare("parse_error_block", |w| {
        let mut c = w.control(&["new", "-s", "c", "-n", "w", "cat"]);
        c.wait("%session-changed", |t| t.contains("%session-changed"));
        c.send("nosuchcmd");
        c.wait("%error", |t| t.contains("%error"));
        let (out, err, code) = c.close();
        (normalise_guards(&out), err, code)
    });
}

/// (4) Pane output arrives as `%output`, never inside a guard; compared
/// as the decoded payload since chunking depends on buffering
/// (control.c:807-855).
#[test]
fn pane_output_outside_guards() {
    compare("pane_output_outside_guards", |w| {
        assert_eq!(w.run(&["new", "-d", "-s", "c", "-n", "w", "cat"]).2, 0);
        let mut c = w.control(&["attach", "-t", "c"]);
        c.wait("%session-changed", |t| t.contains("%session-changed"));
        c.send("send-keys -t c -l abc");
        c.wait("%output abc", |t| pane_output(t).ends_with(b"abc"));
        let (out, err, code) = c.close();
        let mut depth = 0;
        for line in out.lines() {
            if line.starts_with("%begin") {
                depth += 1;
            } else if line.starts_with("%end") || line.starts_with("%error") {
                depth -= 1;
            } else if line.starts_with("%output") {
                assert_eq!(depth, 0, "%output inside a guard: {out:?}");
            }
        }
        let events: Vec<String> = normalise_guards(&out)
            .into_iter()
            .filter(|l| !l.starts_with("%output"))
            .collect();
        (
            events,
            String::from_utf8_lossy(&pane_output(&out)).into_owned(),
            err,
            code,
        )
    });
}

/// (5) `-CC` needs a terminal on stdin (client.c:343-348).
#[test]
fn control_control_needs_a_tty() {
    compare("control_control_needs_a_tty", |w| {
        let out = w
            .command(&["-CC", "new", "-d", "-s", "c", "cat"])
            .stdin(Stdio::piped())
            .output()
            .expect("run -CC");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            w.normalise(&String::from_utf8_lossy(&out.stderr)),
            out.status.code(),
        )
    });
}

#[test]
fn decode_reverses_octal_escapes() {
    assert_eq!(decode_output("a\\134b\\015\\012"), b"a\\b\r\n");
    assert_eq!(
        normalise_guards("%begin 9 313 0\n%end 9 313 0\n%begin 9 320 1\nx"),
        ["%begin <T> #1 0", "%end <T> #1 0", "%begin <T> #2 1", "x"]
    );
}

/// Send each line of `lines` to control client `c` and wait for its reply
/// block; the whole stream after the session announcement, normalised.
fn script(w: &World, c: &mut Control, lines: &[&str]) -> Vec<String> {
    let mark = |t: &str| t.matches("%end").count() + t.matches("%error").count();
    for line in lines {
        let before = mark(&c.text());
        c.send(line);
        c.wait(line, |t| mark(t) > before);
    }
    normalise_guards(&w.normalise(&c.text()))
        .into_iter()
        .filter(|l| !l.starts_with("%output") && !l.starts_with("%session"))
        .collect()
}

/// cmd-refresh-client.c:196-284 and server-client.c:3120-3199 on a control
/// client: `-f` flags (a flag that does not apply to a control client, an
/// unknown one, `!` to clear, read-only that stays), `-F` as `-f`, the
/// error for `-A`/`-B`/`-C` on a client that is not a control client is
/// in the attach tests; here `-C` forms and their errors, `-A` pane
/// states, `-S`, `-t`, and `switch-client -r` toggling read-only.
#[test]
fn refresh_client_flags_and_sizes() {
    compare("refresh_client_flags_and_sizes", |w| {
        w.run(&["new", "-d", "-s", "c", "-x", "80", "-y", "24", "cat"]);
        let mut c = w.control(&["attach", "-t", "c"]);
        c.wait("%session-changed", |t| t.contains("%session-changed"));
        let flags = "display -p 'f=#{client_flags} ro=#{client_readonly}'";
        let size = "ls -F 's=#{window_width}x#{window_height}'";
        let out = script(
            w,
            &mut c,
            &[
                flags,
                "refresh-client -f new-layouts,wait-exit,pause-after=5",
                flags,
                "refresh-client -f '!new-layouts,!pause-after,bogus'",
                flags,
                "refresh-client -F no-output,ignore-size,no-detach-on-destroy",
                flags,
                "refresh-client -f '!no-output,!ignore-size,!no-detach-on-destroy,!wait-exit'",
                flags,
                "refresh-client -f read-only",
                flags,
                // A read-only client may not clear it with -f.
                "refresh-client -f '!read-only'",
                flags,
                "switch-client -r",
                flags,
                "switch-client -r",
                flags,
                "refresh-client -C 100,50",
                size,
                "refresh-client -C 120x30",
                size,
                "refresh-client -C 5x",
                "refresh-client -C 0x5",
                "refresh-client -C 10001x5",
                "refresh-client -C nonsense",
                "refresh-client -C @0:40x10",
                size,
                "refresh-client -C @0:",
                size,
                "refresh-client -C @0:0x10",
                "refresh-client -A '%0:off'",
                "refresh-client -A '%0:on'",
                "refresh-client -A '%0:pause'",
                "refresh-client -A '%0:pause'",
                "refresh-client -A '%0:continue'",
                "refresh-client -A '%0:continue'",
                "refresh-client -A '%9:pause' -A '%0:off'",
                "refresh-client -A nonsense",
                "refresh-client -S",
                "refresh-client",
                "refresh-client -t nosuch",
                "refresh-client -l",
                "switch-client -c nosuch -r",
                "switch-client -F",
            ],
        );
        let (_, _, code) = c.close();
        (out, code)
    });
}

/// `refresh-client -A %0:off` stops `%output` for the pane, `on` brings it
/// back, `pause` and `continue` announce themselves (control.c:349-402),
/// and `no-output` silences everything until `!no-output` (control.c:330).
#[test]
fn refresh_client_pane_output_states() {
    compare("refresh_client_pane_output_states", |w| {
        w.run(&["new", "-d", "-s", "c", "-n", "w", "cat"]);
        let mut c = w.control(&["attach", "-t", "c"]);
        c.wait("%session-changed", |t| t.contains("%session-changed"));
        let mut seen = Vec::new();
        // `read`: whether the server reads the pane after `line`. With the
        // control client the only one attached and the pane `off` for it,
        // neither does (control.c `control_pane_offset`, server-client.c
        // `server_client_check_pane_buffer`), so the echo waits for `on`.
        let mut step = |c: &mut Control, w: &World, line: &str, text: &str, read: bool| {
            c.send(line);
            c.send(&format!("send-keys -t c -l {text}"));
            // The echo comes back from the pane asynchronously: wait until
            // the server has read it (it is on the pane's screen), so any
            // `%output` for it is queued before the `synced` reply. Both
            // servers queue control output as they read the pane, before
            // the screen shows it (window.c `window_pane_read_callback`).
            let end = Instant::now() + WAIT;
            while read && !w.run(&["capture-pane", "-p", "-t", "c"]).0.contains(text) {
                assert!(
                    Instant::now() < end,
                    "{}: pane never showed {text}",
                    w.bin.display()
                );
                thread::sleep(Duration::from_millis(10));
            }
            c.send("display -p synced");
            c.wait("sync", |t| t.matches("synced").count() > seen.len());
            seen.push(String::from_utf8_lossy(&pane_output(&c.text())).into_owned());
        };
        step(&mut c, w, "display -p one", "AAA", true);
        step(&mut c, w, "refresh-client -A '%0:off'", "BBB", false);
        step(&mut c, w, "refresh-client -A '%0:on'", "CCC", true);
        step(&mut c, w, "refresh-client -A '%0:pause'", "DDD", true);
        step(&mut c, w, "refresh-client -A '%0:continue'", "EEE", true);
        step(&mut c, w, "refresh-client -f no-output", "FFF", true);
        step(&mut c, w, "refresh-client -f '!no-output'", "GGG", true);
        let (out, _, code) = c.close();
        let events: Vec<String> = normalise_guards(&out)
            .into_iter()
            .filter(|l| l.starts_with("%pause") || l.starts_with("%continue"))
            .collect();
        (seen, events, code)
    });
}

/// `refresh-client -f new-layouts` changes the layout form control clients
/// see (cmd-select-layout.c:93-96, format.c:859-896).
#[test]
fn refresh_client_new_layouts_format() {
    compare("refresh_client_new_layouts_format", |w| {
        w.run(&["new", "-d", "-s", "c", "-x", "80", "-y", "24", "cat"]);
        w.run(&["split-window", "-h", "-t", "c"]);
        let mut c = w.control(&["attach", "-t", "c"]);
        c.wait("%session-changed", |t| t.contains("%session-changed"));
        let layout = "display -p '#{window_layout} #{window_visible_layout}'";
        let out = script(
            w,
            &mut c,
            &[
                layout,
                "refresh-client -f new-layouts",
                layout,
                "refresh-client -f '!new-layouts'",
                layout,
            ],
        );
        let (_, _, code) = c.close();
        (out, code)
    });
}

/// `wait-exit` (client.c:423, control.c `control_wait_exit`): after
/// `%exit` the client keeps reading its input and exits at the first empty
/// line, even one read together with the lines before it; without the
/// flag (or with it cleared) it exits at `%exit` (control-client-wait-exit.sh).
#[test]
fn wait_exit_lingers_until_an_empty_line() {
    compare("wait_exit_lingers_until_an_empty_line", |w| {
        let mut out = Vec::new();
        for flags in ["wait-exit", "wait-exit,!wait-exit"] {
            w.run(&["new", "-d", "-s", "c", "-n", "w", "cat"]);
            let mut c = w.control(&["attach", "-t", "c"]);
            c.wait("%session-changed", |t| t.contains("%session-changed"));
            for f in flags.split(',') {
                c.send(&format!("refresh-client -f '{f}'"));
            }
            c.send("display -p ready");
            c.wait("ready", |t| t.contains("ready\n"));
            w.run(&["detach-client", "-s", "c"]);
            c.wait("%exit", |t| t.contains("%exit"));
            thread::sleep(Duration::from_millis(300));
            let waiting = c.child.try_wait().unwrap().is_none();
            if waiting {
                // A line, then the empty one, in one write.
                let stdin = c.stdin.as_mut().unwrap();
                stdin.write_all(b"a\n\n").unwrap();
                stdin.flush().unwrap();
                let end = Instant::now() + WAIT;
                while c.child.try_wait().unwrap().is_none() {
                    assert!(Instant::now() < end, "{flags}: still waiting");
                    thread::sleep(Duration::from_millis(10));
                }
            }
            let (text, _, code) = c.close();
            let tail: Vec<String> = normalise_guards(&text)
                .into_iter()
                .filter(|l| l.starts_with("%exit"))
                .collect();
            out.push((flags, waiting, tail, code));
            w.run(&["kill-session", "-t", "c"]);
        }
        out
    });
}

/// `server_client_exit_timer` (server-client.c:2361-2387): after
/// `kill-server` a control client whose output never drains (its reader
/// stopped) cannot keep the server alive past the exit timeout, 10 s
/// (control-client-exit-stalled.sh; compat/DIVERGENCES.md D-029).
///
/// tmux can take both 10 s steps here, so its server is gone 20 s after
/// `kill-server`, right on a 20 s limit. That happens when the full pipe has
/// fewer than 6 bytes free (about half the runs on macOS). At the first step
/// the client gets `MSG_SHUTDOWN` and prints `%exit` (client.c
/// `client_main`). Its stdout is still non-blocking (control.c
/// `control_start`), so that `fflush` fails with `EAGAIN` and the bytes stay
/// in the stdio buffer. Then `client_main` makes stdout blocking again, and
/// the flush at `exit()` blocks for good on the pipe nobody reads. The
/// server then waits for the second step, `took too long to exit`
/// (`server_client_exit_timer`). vtmux drops such a client at the first
/// step (D-029), so its limit stays 10 s + 10 s slack.
#[test]
fn stalled_client_does_not_keep_the_server() {
    for (bin, limit) in bins().into_iter().zip([20, 30]) {
        let w = World::new(&bin);
        w.run(&[
            "new",
            "-d",
            "-s",
            "c",
            "-x",
            "80",
            "-y",
            "24",
            "cat /dev/zero | tr '\\000' x",
        ]);
        let pid = common::parse_pid(&w.run(&["display", "-p", "#{pid}"]).0).expect("server pid");
        // stdout is a pipe nobody reads: it fills and stays full.
        let mut child = w
            .command(&["-C", "attach", "-t", "c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn control client");
        let end = Instant::now() + WAIT;
        while !w.run(&["lsc", "-F", "x"]).0.contains('x') {
            assert!(
                Instant::now() < end,
                "{}: client never attached",
                bin.display()
            );
            thread::sleep(Duration::from_millis(50));
        }
        thread::sleep(Duration::from_secs(2));
        w.run(&["kill-server"]);
        let end = Instant::now() + Duration::from_secs(limit);
        while rustix::process::test_kill_process(pid).is_ok() {
            assert!(
                Instant::now() < end,
                "{}: server still running {limit} s after kill-server",
                bin.display()
            );
            thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// A control client that reads steadily but is behind a flood when it is
/// detached gets its exit at once: the pane output not yet sent is dropped
/// (server-client.c `server_client_check_exit`, control.c
/// `control_discard`), so it ends with `%exit` and status 0 within a
/// moment, not after the exit timer with a lost server.
#[test]
fn slow_reader_detached_in_a_flood_exits_cleanly() {
    for bin in bins() {
        let w = World::new(&bin);
        let mut child = w
            .command(&[
                "-C",
                "new-session",
                "-s",
                "a",
                "-x",
                "80",
                "-y",
                "24",
                "yes rvflood",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn control client");
        let _stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("stdout");
        thread::sleep(Duration::from_secs(3));
        w.run(&["detach-client", "-s", "a"]);
        let start = Instant::now();
        let mut tail = Vec::new();
        let mut buf = vec![0u8; 65536];
        // 64 KB every 10 ms: slower than the flood, but always reading.
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            let keep = tail.len().saturating_sub(200);
            tail.drain(..keep);
            // tmux: well under a second; draining the flood would take
            // tens of seconds.
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "{}: still reading after detach",
                bin.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait().expect("wait");
        let tail = String::from_utf8_lossy(&tail);
        assert!(
            tail.ends_with("%exit\n"),
            "{}: tail {tail:?}",
            bin.display()
        );
        assert_eq!(status.code(), Some(0), "{}", bin.display());
    }
}

/// The server's resident set in KB (`ps -o rss=`: no /proc on macOS).
fn rss_kb(pid: rustix::process::Pid) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.as_raw_nonzero().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// A control client reading slower than its pane writes costs the server
/// a bounded amount, not a copy of everything the pane wrote: pane output
/// is held once and formatted as the client takes it, and the pane is not
/// read while its only consumers are behind (control.c
/// `control_write_callback`, server-client.c
/// `server_client_check_pane_buffer`). Before, vtmux grew past 200 MB in
/// 10 s here; tmux stays at a few MB.
#[test]
fn slow_reader_keeps_server_memory_bounded() {
    const BOUND_KB: u64 = 64 * 1024;
    for bin in bins() {
        let w = World::new(&bin);
        let mut child = w
            .command(&[
                "-C",
                "new-session",
                "-s",
                "a",
                "-x",
                "80",
                "-y",
                "24",
                "yes rvflood",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn control client");
        let _stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("stdout");
        w.wait_clients(1);
        let pid = w.server_pid();
        let mut buf = vec![0u8; 65536];
        let (mut peak, mut read) = (0, 0);
        let end = Instant::now() + Duration::from_secs(5);
        // 64 KB every 10 ms: slower than the flood, but always reading.
        while Instant::now() < end {
            read += stdout.read(&mut buf).expect("read");
            thread::sleep(Duration::from_millis(10));
            peak = peak.max(rss_kb(pid));
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "{}: client left",
            bin.display()
        );
        assert!(read > 1 << 20, "{}: read only {read} bytes", bin.display());
        assert!(
            peak < BOUND_KB,
            "{}: server RSS peaked at {peak} KB",
            bin.display()
        );
        drop(w);
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// `yes` in session `a`, a control client that reads everything (to
/// /dev/null) so the pane keeps being read, and a stalled control client
/// attached with `args`: its output ages while it does not read.
fn flood_with_a_stalled_client(w: &World, args: &[&str]) -> (Child, Control) {
    w.run(&[
        "new",
        "-d",
        "-s",
        "a",
        "-x",
        "80",
        "-y",
        "24",
        "yes rvflood",
    ]);
    let fast = w
        .command(&["-C", "attach", "-t", "a"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fast control client");
    w.wait_clients(1);
    let stalled = w.control_stalled(&[&["attach", "-t", "a"], args].concat());
    w.wait_clients(2);
    (fast, stalled)
}

/// `pause-after=1` (control.c `control_check_age`): a pane whose oldest
/// output not yet sent to the client is over a second old is paused,
/// `%pause %0`, its output dropped until `refresh-client -A %0:continue`
/// (`%continue %0`), and output is `%extended-output %0 <age> : ` meanwhile
/// (`control_append_data`).
#[test]
fn pause_after_pauses_a_pane_the_client_is_behind_on() {
    compare("pause_after_pauses_a_pane_the_client_is_behind_on", |w| {
        let (mut fast, mut c) = flood_with_a_stalled_client(w, &["-f", "pause-after=1"]);
        // Only time has to pass (the stalled client's pipe fills at once),
        // so a margin over the 1 s age is enough on a slow machine too.
        thread::sleep(Duration::from_secs(3));
        c.start_reading();
        c.wait("%pause", |t| t.contains("%pause %0\n"));
        c.send("refresh-client -A '%0:continue'");
        c.wait("output after %continue", |t| {
            t.split_once("%continue %0\n")
                .is_some_and(|(_, after)| after.contains("\n%extended-output %0 "))
        });
        let _ = fast.kill();
        let _ = fast.wait();
        let (out, _, code) = c.close();
        let lines: Vec<&str> = out.lines().collect();
        let at = |what: &str| lines.iter().position(|l| *l == what);
        let (pause, cont) = (at("%pause %0").unwrap(), at("%continue %0").unwrap());
        let extended = |l: &&str| {
            l.strip_prefix("%extended-output %0 ")
                .and_then(|r| r.split_once(" : "))
                .is_some_and(|(age, _)| age.parse::<u64>().is_ok())
        };
        (
            lines.iter().filter(|l| l.starts_with("%pause")).count(),
            lines.iter().filter(|l| l.starts_with("%continue")).count(),
            lines.iter().any(|l| l.starts_with("%output")),
            lines[..pause].iter().any(extended),
            lines[pause..cont].iter().any(extended),
            lines[cont..].iter().any(extended),
            code,
        )
    });
}

/// Without `pause-after`, a client whose oldest unsent output is
/// `CONTROL_MAXIMUM_AGE` old is let go: `%exit too far behind`, status 0
/// (control.c `control_check_age`). The age is 300 s in tmux; a debug
/// vtmux takes `VTMUX_CONTROL_MAXIMUM_AGE` (ms) so this takes seconds, so
/// only vtmux runs it.
#[test]
fn too_far_behind_lets_a_stalled_client_go() {
    let mut w = World::new(&bins()[0]);
    w.env
        .push(("VTMUX_CONTROL_MAXIMUM_AGE".into(), "1000".into()));
    let (mut fast, mut c) = flood_with_a_stalled_client(&w, &[]);
    // Let go, the stalled client leaves its session and the list; how
    // soon after the age passes depends on the machine, so no fixed stall.
    let end = Instant::now() + Duration::from_secs(20);
    while w.run(&["lsc", "-F", "x"]).0.lines().count() > 1 {
        assert!(Instant::now() < end, "stalled client never let go");
        thread::sleep(Duration::from_millis(50));
    }
    c.start_reading();
    c.wait("%exit", |t| t.contains("%exit"));
    let _ = fast.kill();
    let _ = fast.wait();
    let (out, _, code) = c.close();
    assert!(
        out.ends_with("\n%exit too far behind\n"),
        "{:?}",
        &out[out.len().saturating_sub(300)..]
    );
    assert_eq!(code, 0);
    // The session and the other clients are not touched.
    assert_eq!(w.run(&["has", "-t", "a"]).2, 0);
}

/// `control_check_reply_buffer` (control.c:429-451): a client that does
/// not read while its replies pass 64 MB is let go, `too far behind`. Let
/// go, it reads no more lines: tmux's `control_discard` (control.c:1040-1048)
/// stops the read event, so only lines already read run; vtmux runs the
/// lines it read in that loop turn and sends `Exit` at its end
/// (`control_tick`). A line sent after the let-go never runs in either.
///
/// Each line is checked on its own (`control_write_line`), so the stream
/// stops after the `%begin` of the `show-buffer` whose reply (one line:
/// `cmdq_print_data` writes the buffer whole) would pass the cap, not
/// after the `%end` of the one before it.
#[test]
fn reply_buffer_cap_lets_a_stalled_client_go() {
    compare("reply_buffer_cap_lets_a_stalled_client_go", |w| {
        // 4 MiB: 65536 lines of 64 bytes.
        let line = format!("{}\n", "x".repeat(63));
        std::fs::write(w.dir.path().join("big"), line.repeat(1 << 16)).unwrap();
        w.run(&["new", "-d", "-s", "a", "-x", "80", "-y", "24", "cat", "-"]);
        let mut c = w.control_stalled(&["attach", "-t", "a"]);
        w.wait_clients(1);
        // 80 MB of replies, none read, then a marker: the server has run
        // every `show-buffer` once it is set, however long that takes (a
        // debug vtmux without a UTF-8 locale needs over 2 s). The marker
        // runs only if it was read in the same go as the show-buffer that
        // passes the cap: the server runs what it read without reading
        // more, and drops what comes after the let-go. So the whole
        // script is one write of less than `PIPE_BUF` (512 on macOS),
        // which one read takes whole: tmux's from the pipe, vtmux's
        // client's into one `Stdin` frame. Sent a line at a time, the
        // server could read the lines up to past the cap but not the
        // marker, run them, and drop the marker (macOS CI). `big` is
        // relative to the session's directory (server_client_get_cwd), to
        // keep the script short.
        let mut script = String::from("load-buffer -b big big\n");
        script.push_str(&"show-buffer -b big\n".repeat(20));
        script.push_str("set -g @rvdone 1");
        assert!(script.len() < 512, "{} bytes and a newline", script.len());
        c.send(&script);
        let set = |w: &World, name: &str| w.run(&["show", "-gv", name]).0.trim() == "1";
        let end = Instant::now() + Duration::from_secs(60);
        while !set(w, "@rvdone") {
            let gone = !w.run(&["lsc", "-F", "x"]).0.contains('x');
            assert!(
                !gone || set(w, "@rvdone"),
                "{}: let go before the marker was read",
                w.bin.display()
            );
            assert!(
                Instant::now() < end,
                "{}: replies never queued",
                w.bin.display()
            );
            thread::sleep(Duration::from_millis(50));
        }
        // Read after the let-go: never run (checked once the client is
        // gone, so it had its chance).
        c.send("set -g @rvafter 1");
        c.start_reading();
        // stdin stays open: the server lets the client go, not EOF.
        let end = Instant::now() + Duration::from_secs(30);
        while c.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < end, "client still running: {}", {
                let t = c.text();
                t.get(t.len().saturating_sub(200)..)
                    .unwrap_or(&t)
                    .to_owned()
            });
            thread::sleep(Duration::from_millis(50));
        }
        let (out, _, code) = c.close();
        (
            out.ends_with("\n%exit too far behind\n"),
            out.len() < 70 << 20,
            // The line before `%exit`: the guard of the reply left out.
            out.rsplit('\n')
                .nth(2)
                .is_some_and(|l| l.starts_with("%begin ")),
            code,
            set(w, "@rvafter"),
        )
    });
}
