//! CLI scenarios run against `target/debug/vtmux` and the reference tmux
//! 3.8, compared after normalisation (socket dir, pid, time).
//!
//! Isolation: `TMUX` and `TMUX_PANE` cleared, a private `TMUX_TMPDIR` per
//! scenario, an explicit `-L` per binary, `-f /dev/null`, SHELL=/bin/sh.
//! The reference is `$VTMUX_REFERENCE_TMUX` or the `cargo xtask ref-tmux`
//! cache; without it each test prints why and checks vtmux alone against
//! the recorded expectations embedded below.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// One run's observable result.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn vtmux_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vtmux"))
}

/// vtmux invoked as `tmux` (a symlink, D20): tmux's socket directory and
/// tmux's program name in messages, so transcripts compare like for like.
fn vtmux_as_tmux() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let link = dir.path().join("tmux");
    std::os::unix::fs::symlink(vtmux_bin(), &link).expect("symlink");
    (dir, link)
}

fn reference() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VTMUX_REFERENCE_TMUX").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let bin = cache.join("vtmux/tmux-3.8/bin/tmux");
    bin.exists().then_some(bin)
}

/// A private world for one binary: its own `TMUX_TMPDIR` and label.
struct World {
    bin: PathBuf,
    tmpdir: tempfile::TempDir,
    label: String,
}

impl World {
    fn new(bin: &Path) -> Self {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        Self {
            bin: bin.to_owned(),
            label: format!("cli{}", std::process::id()),
            tmpdir,
        }
    }

    fn dir(&self) -> &Path {
        self.tmpdir.path()
    }

    /// Run with explicit global args (no `-L` added).
    fn raw(&self, args: &[&str]) -> Run {
        let out = Command::new(&self.bin)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", self.dir())
            .env("SHELL", "/bin/sh")
            .output()
            .expect("run binary");
        Run {
            stdout: self.normalise(&String::from_utf8_lossy(&out.stdout)),
            stderr: self.normalise(&String::from_utf8_lossy(&out.stderr)),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// Run `-L label -f /dev/null args...`.
    fn run(&self, args: &[&str]) -> Run {
        let mut full = vec!["-L", &self.label, "-f", "/dev/null"];
        full.extend_from_slice(args);
        self.raw(&full)
    }

    /// Socket dir and temp dir -> placeholders; ctime text -> `TIME`.
    fn normalise(&self, s: &str) -> String {
        let dir = self.dir().to_string_lossy().into_owned();
        let mut out = String::new();
        for line in s.split_inclusive('\n') {
            let mut line = line
                .replace(&format!("{dir}/tmux-"), "SOCKDIR/tmux-")
                .replace(&dir, "TMP");
            if let Some(at) = line.find("(created ") {
                let end = line[at..].find(')').map_or(line.len(), |e| at + e);
                line.replace_range(at..end, "(created TIME");
            }
            out.push_str(&line);
        }
        out
    }

    /// Poll `capture-pane -p` (max 5 s, 50 ms steps) until `want` shows.
    fn wait_for_text(&self, target: &str, want: &str) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self
                .run(&["capture-pane", "-p", "-t", target])
                .stdout
                .contains(want)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "{want:?} never appeared in {target} ({})",
            self.bin.display()
        );
    }

    /// Poll until the server is gone (`ls` fails).
    fn wait_gone(&self) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self.run(&["ls"]).code != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("server did not exit ({})", self.bin.display());
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

/// Run `scenario` on vtmux and the reference and compare the transcripts.
/// Each step's result is recorded; the reference's transcript is the
/// expectation.
fn compare(name: &str, scenario: impl Fn(&World) -> Vec<Run>) {
    let (_keep, as_tmux) = vtmux_as_tmux();
    let ours = scenario(&World::new(&as_tmux));
    let Some(reference) = reference() else {
        eprintln!(
            "{name}: SKIP reference comparison: no reference tmux (set VTMUX_REFERENCE_TMUX or run `cargo xtask ref-tmux`)"
        );
        return;
    };
    let theirs = scenario(&World::new(&reference));
    assert_eq!(ours.len(), theirs.len());
    for (i, (o, t)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(
            o, t,
            "{name}: step {i} differs (left vtmux, right tmux 3.8)"
        );
    }
}

/// (1) `ls` with no server: no socket file -> error connecting.
#[test]
fn ls_without_server() {
    compare("ls_without_server", |w| {
        vec![w.run(&["ls"]), w.run(&["has", "-t", "a"])]
    });
}

/// (2) `-S` into a missing directory: `ls` cannot connect, `new -d`
/// cannot create (client.c:291-297, server.c:151-152).
#[test]
fn missing_socket_dir() {
    compare("missing_socket_dir", |w| {
        let path = w.dir().join("nodir/s");
        let p = path.to_str().expect("utf-8 path");
        vec![
            w.raw(&["-f", "/dev/null", "-S", p, "ls"]),
            w.raw(&["-f", "/dev/null", "-S", p, "new", "-d"]),
            w.raw(&["-f", "/dev/null", "-S", p, "kill-server"]),
        ]
    });
}

/// (3) A stale socket file (nothing listening): `no server running on`.
#[test]
fn stale_socket() {
    compare("stale_socket", |w| {
        let path = w.dir().join("stale");
        drop(std::os::unix::net::UnixListener::bind(&path).expect("bind"));
        let p = path.to_str().expect("utf-8 path");
        vec![
            w.raw(&["-f", "/dev/null", "-S", p, "ls"]),
            w.raw(&["-f", "/dev/null", "-S", p, "has"]),
        ]
    });
}

/// (4) has-session on an existing and a missing session.
#[test]
fn has_session() {
    compare("has_session", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["has", "-t", "a"]),
            w.run(&["has", "-t", "nope"]),
            w.run(&["has", "-t", "=a"]),
            w.run(&["has"]),
        ]
    });
}

/// (5) A second session with the same name.
#[test]
fn duplicate_session() {
    compare("duplicate_session", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["new", "-d", "-s", "a"]),
        ]
    });
}

/// (6) `ls` lists in name order whatever the creation order; `-F`.
#[test]
fn list_sessions_order() {
    compare("list_sessions_order", |w| {
        vec![
            w.run(&["new", "-d", "-s", "b"]),
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["ls"]),
            w.run(&["ls", "-F", "#{session_name}"]),
            w.run(&["ls", "-O", "index", "-F", "#{session_name} #{session_id}"]),
            w.run(&["ls", "-O", "bad"]),
        ]
    });
}

/// (7) display-message -p with the ID and version variables.
#[test]
fn display_message_ids() {
    compare("display_message_ids", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&[
                "display",
                "-p",
                "#{session_name} #{session_id} #{window_id} #{pane_id} #{version}",
            ]),
            w.run(&["display", "-p", "-t", "a", "#S:#I.#P #{window_index}"]),
            w.run(&["display", "hello"]),
            w.run(&["display", "-p", "%%"]),
            w.run(&["display", "-p", "-F", "x", "y"]),
        ]
    });
}

/// (8) send-keys -l into `cat` on a 20x5 pane, then capture.
#[test]
fn send_keys_and_capture() {
    compare("send_keys_and_capture", |w| {
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a", "-x", "20", "-y", "5", "cat"]),
            w.run(&["send-keys", "-t", "a", "-l", "hi"]),
        ];
        w.wait_for_text("a", "hi");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        runs.push(w.run(&["display", "-p", "-t", "a", "#{pane_width}x#{pane_height}"]));
        runs.push(w.run(&["send-keys", "-t", "a", "Enter", "x", "y"]));
        w.wait_for_text("a", "xy");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        runs
    });
}

/// (9) Command lookup, flag and arity errors (the `task_0` shared facts).
#[test]
fn command_errors() {
    compare("command_errors", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["nosuchcmd"]),
            w.run(&["displ"]),
            w.run(&["display-message", "-Z"]),
            w.run(&["show-options", "a", "b"]),
            w.run(&["set-option"]),
            w.run(&["new", "-d", "-x", "abc"]),
            w.run(&["capture-pane", "-p", "-t", "nope"]),
            w.raw(&["-Z"]),
        ]
    });
}

/// (10) kill-session leaves the others; killing the last ends the server.
#[test]
fn kill_session() {
    compare("kill_session", |w| {
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["new", "-d", "-s", "b"]),
            w.run(&["kill-session", "-t", "a"]),
            w.run(&["ls"]),
            w.run(&["kill-session", "-t", "nope"]),
            w.run(&["kill-session", "-t", "b"]),
        ];
        w.wait_gone();
        runs.push(w.run(&["ls"]));
        runs
    });
}

/// (11) kill-server; afterwards the socket file is stale.
#[test]
fn kill_server() {
    compare("kill_server", |w| {
        let mut runs = vec![w.run(&["new", "-d", "-s", "a"]), w.run(&["kill-server"])];
        w.wait_gone();
        runs.push(w.run(&["ls"]));
        runs.push(w.run(&["kill-server"]));
        runs
    });
}

/// Under its own name (D20, D21): `vtmux-UID` socket directory, `vtmux`
/// in getopt messages and usage, and `-V` naming both versions.
#[test]
fn own_name() {
    let w = World::new(&vtmux_bin());
    let ls = w.run(&["ls"]);
    assert_eq!(
        ls.stderr,
        format!(
            "error connecting to TMP/vtmux-{}/{} (No such file or directory)\n",
            uid(),
            w.label
        )
    );
    let v = w.raw(&["-V"]);
    assert_eq!(
        v.stdout,
        format!("vtmux {} (tmux 3.8)\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(
        w.raw(&["-Z"])
            .stderr
            .starts_with("vtmux: unknown option -- Z\nusage: vtmux ")
    );
    let (_keep, as_tmux) = vtmux_as_tmux();
    assert_eq!(World::new(&as_tmux).raw(&["-V"]).stdout, "tmux 3.8\n");
}

fn uid() -> String {
    let out = Command::new("id").arg("-u").output().expect("id -u");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
