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
use std::process::{Child, ChildStdin, Command, Stdio};
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
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
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
        let mut child = self
            .command(&[&["-C"], args].concat())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn control client");
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&out);
        let mut stdout = child.stdout.take().expect("stdout");
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let stdin = child.stdin.take();
        Control { child, stdin, out }
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
    out: Arc<Mutex<Vec<u8>>>,
}

impl Control {
    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
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
                "refresh-client -A %0:off",
                "refresh-client -A %0:on",
                "refresh-client -A %0:pause",
                "refresh-client -A %0:pause",
                "refresh-client -A %0:continue",
                "refresh-client -A %0:continue",
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
        let mut step = |c: &mut Control, w: &World, line: &str, text: &str| {
            c.send(line);
            c.send(&format!("send-keys -t c -l {text}"));
            c.send("display -p synced");
            c.wait("sync", |t| t.matches("synced").count() > seen.len());
            seen.push(String::from_utf8_lossy(&pane_output(&c.text())).into_owned());
            let _ = w;
        };
        step(&mut c, w, "display -p one", "AAA");
        step(&mut c, w, "refresh-client -A %0:off", "BBB");
        step(&mut c, w, "refresh-client -A %0:on", "CCC");
        step(&mut c, w, "refresh-client -A %0:pause", "DDD");
        step(&mut c, w, "refresh-client -A %0:continue", "EEE");
        step(&mut c, w, "refresh-client -f no-output", "FFF");
        step(&mut c, w, "refresh-client -f '!no-output'", "GGG");
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
